use std::collections::HashMap;
use crate::execution::execution::execution_state::{DependencyGraphMutation, ExecutionState, ExecutionStateEvaluation};
use std::fmt;
use std::fmt::Debug;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll};
use std::sync::{Mutex};
// use no_deadlocks::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use futures_util::FutureExt;

use crate::execution::primitives::identifiers::{DependencyReference, OperationId};

use petgraph::data::Build;
use petgraph::dot::Dot;

use crate::execution::primitives::serialized_value::RkyvSerializedValue;
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::{IntoEdgesDirected, VisitMap};
use petgraph::Direction;
use petgraph::prelude::Dfs;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::ser::SerializeMap;
use tokio::sync::oneshot::Receiver;
use crate::cells::CellTypes;

// TODO: update all of these identifies to include a "space" they're within

type EdgeIdentity = (OperationId, OperationId, DependencyReference);

/// ExecutionId is a unique identifier for a point in the execution graph.
/// It is a tuple of (branch, counter) where branch is an instance of a divergence from a previous
/// execution state. Counter is the iteration of steps taken in that branch.
// TODO: add a globally incrementing counter to this so that there can never be identity collisions

// TODO: (branch, depth, counter)
pub type ExecutionNodeId = (usize, usize);


#[derive(Debug, Clone)]
pub struct MergedStateHistory(HashMap<usize, (ExecutionNodeId, Arc<RkyvSerializedValue>)>);


impl Serialize for MergedStateHistory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0.iter() {
            map.serialize_entry(&k, &(v.0, &v.1.deref()))?;
        }
        map.end()
    }
}


impl<'de> Deserialize<'de> for MergedStateHistory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
    {
        unreachable!("ExecutionState cannot be deserialized.")
    }
}


// TODO: every point in the execution graph should be a top level element in an execution stack
//       but what about async functions?

// TODO: we need to introduce a concept of depth in the execution graph.
//       depth could be more edges from an element with a particular labelling.
//       we would need execution ids to have both branch, counter (steps), and depth they occur at.

// TODO: we need to effectively step inside of a function, so effectively these are fractional
//       steps we take within a given counter. but then those can nest even further.
//


/// This models the network of reactive relationships between different components.
///
/// This was initially inspired by works such as Salsa, Verde, Incremental, Adapton, and Differential Dataflow.
pub struct ExecutionGraph {
    /// Global revision number for modifications to the graph itself
    revision: usize,

    /// This is the graph of dependent execution state
    ///
    /// (branch, counter) -> steam_outputs_at_head
    /// The dependency graph is stored within the execution graph, allowing us to model changes
    /// to the dependency graph during the process of execution.
    /// This is a graph of the mutations to the dependency graph.
    /// As we make changes to the dependency graph itself, we track those transitions here.
    /// This is roughly equivalent to a git history of the dependency graph.
    ///
    /// We store immutable representations of the history of the dependency graph. These
    /// can be used to reconstruct a traversable dependency graph at any point in time.
    ///
    /// Identifiers on this graph refer to points in the execution graph. In execution terms, changes
    /// along those edges are always considered to have occurred _after_ the target step.
    execution_graph: Arc<Mutex<DiGraphMap<ExecutionNodeId, ExecutionStateEvaluation>>>,

    execution_node_id_to_state: Arc<Mutex<HashMap<ExecutionNodeId, ExecutionStateEvaluation>>>,

    /// Sender channel for sending messages to the execution graph
    sender: mpsc::Sender<(ExecutionNodeId, OperationId, RkyvSerializedValue)>,

    pub handle: JoinHandle<()>,
}

impl std::fmt::Debug for ExecutionGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionGraph")
            .field("revision", &self.revision)
            .finish()
    }
}


/// Execution Ids identify transitions in state during execution, as values are
/// emitted, we update the state of the graph and create a new execution id.
///
/// They are structured in the format (branch, counter), counter represents
/// iterative execution of the same branch. Branch in contrast increments when
/// we re-evaluate a node that head already previously been evaluated - creating a
/// new line of execution.
fn get_execution_id(
    execution_graph: &mut DiGraphMap<ExecutionNodeId, ExecutionStateEvaluation>,
    prev_execution_id: ExecutionNodeId,
) -> (usize, usize) {
    let edges = execution_graph
        .edges_directed(prev_execution_id, Direction::Outgoing);

    // Get the greatest id value from the edges leaving the previous execution state
    if let Some((_, max_to, _)) =
        edges.max_by(|(_, a_to, _), (_, b_to, _)| (a_to.0).cmp(&(b_to.0)))
    {
        // Create an edge in the execution graph from the previous state to this new one
        let id = (max_to.0 + 1, prev_execution_id.1 + 1);
        id
    } else {
        // Create an edge in the execution graph from the previous state to this new one
        let id = (prev_execution_id.0, prev_execution_id.1 + 1);
        id
    }
}

impl ExecutionGraph {
    /// Initialize a new reactivity database. This will create a default input and output node,
    /// graphs default to being the unit function x -> x.
    #[tracing::instrument]
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<(ExecutionNodeId, OperationId, RkyvSerializedValue)>();

        let mut state_id_to_state = HashMap::new();
        state_id_to_state.insert((0, 0), ExecutionStateEvaluation::Complete(ExecutionState::new()));
        let mut state_id_to_state = Arc::new(Mutex::new(state_id_to_state));

        let mut execution_graph = Arc::new(Mutex::new(DiGraphMap::new()));

        let execution_graph_clone = execution_graph.clone();
        let state_id_to_state_clone = state_id_to_state.clone();

        // Kick off a background thread that listens for events from async operations
        // These events inject additional state into the execution graph on new branches
        // Those branches will continue to evaluate independently.
        let handle = std::thread::spawn(move || {
            loop {
                match receiver.try_recv() {
                    Ok((prev_execution_id, operation_id, result)) => {
                        let mut execution_graph = execution_graph_clone.lock().unwrap();
                        let mut state_id_to_state = state_id_to_state_clone.lock().unwrap();

                        // TODO: await the result of the operation
                        let mut new_state = state_id_to_state.get(&prev_execution_id).unwrap().clone();
                        if let ExecutionStateEvaluation::Complete(ref mut state) = &mut new_state {
                            state.state_insert(operation_id, result);
                        }

                        // TODO: log this event
                        // outputs.push((operation_id, result.clone()));

                        let resulting_state_id = get_execution_id(execution_graph.deref_mut(), prev_execution_id);
                        state_id_to_state.deref_mut().insert(resulting_state_id.clone(), new_state.clone());

                        execution_graph
                            .add_edge(prev_execution_id, resulting_state_id.clone(), new_state);
                    },
                    Err(mpsc::TryRecvError::Empty) => {
                        // No messages available, take this time to sleep a bit
                        std::thread::sleep(Duration::from_millis(10)); // Sleep for 10 milliseconds
                    },
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // Handle the case where the sender has disconnected and no more messages will be received
                        break; // or handle it according to your application logic
                    },
                }
            }
            // TODO: execute the remaining graph from this state initialized as a new entity
        });
        ExecutionGraph {
            handle,
            execution_node_id_to_state: state_id_to_state,
            execution_graph,
            sender,
            revision: 0,
        }
    }

    pub fn get_execution_graph_elements(&self) -> Vec<(ExecutionNodeId, ExecutionNodeId)> {
        let execution_graph = self.execution_graph.lock().unwrap();
        execution_graph.deref().all_edges().map(|x| (x.0, x.1)).collect()
    }

    pub fn render_execution_graph_to_graphviz(&self) {
        println!("================ Execution graph ================");
        let execution_graph = self.execution_graph.lock().unwrap();
        println!("{:?}", Dot::with_config(&execution_graph.deref(), &[]));
    }

    pub fn get_state_at_id(&self, id: ExecutionNodeId) -> Option<ExecutionStateEvaluation> {
        let state_id_to_state = self.execution_node_id_to_state.lock().unwrap();
        state_id_to_state.get(&id).cloned()
    }

    pub fn get_merged_state_history(&self, endpoint: &ExecutionNodeId) -> MergedStateHistory {
        println!("Getting merged state history for id {:?}", &endpoint);
        let execution_graph = self.execution_graph.lock();
        let graph = execution_graph.as_ref().unwrap();
        let mut dfs = Dfs::new(graph.deref(), endpoint.clone());
        let root = (0, 0);
        let mut queue = vec![endpoint.clone()];
        while let Some(node) = dfs.next(graph.deref()) {
            if node == root {
                break;
            } else {
                for predecessor in graph.neighbors_directed(node, Direction::Incoming) {
                    if !dfs.discovered.is_visited(&predecessor) {
                        queue.push(predecessor);
                        dfs.stack.push(predecessor);
                    }
                }
            }
        }

        let mut merged_state = HashMap::new();
        for predecessor in queue {
            let state = self.get_state_at_id(predecessor).unwrap();
            if let ExecutionStateEvaluation::Complete(state) = state {
                for (k, v) in state.state.iter() {
                    merged_state.insert(*k, (predecessor, v.clone()));
                }
            }
        }
        MergedStateHistory(merged_state)
    }

    fn progress_graph(&mut self, prev_execution_id: ExecutionNodeId, new_state: ExecutionStateEvaluation) -> ExecutionNodeId {
        // The edge from this node is the greatest branching id + 1
        // if we re-evaluate execution at a given node, we get a new execution branch.
        let mut execution_graph = self.execution_graph.lock().unwrap();
        let mut state_id_to_state = self.execution_node_id_to_state.lock().unwrap();
        let resulting_state_id = get_execution_id(execution_graph.deref_mut(), prev_execution_id);
        state_id_to_state.deref_mut().insert(resulting_state_id.clone(), new_state.clone());
        execution_graph.deref_mut()
            .add_edge(prev_execution_id, resulting_state_id.clone(), new_state.clone());
        resulting_state_id
    }

    // TODO: step execution should be able to progress even when the previous state is current held up
    //       by an executing resolution. We instead want to return the NESTED state of the execution.

    // TODO: the mechanism of our execution engine should be hidden from outside of this class
    //       all that the parent observes is that there is a function that when they call it, it gets new states.
    //
    // TODO: right now, when this function is called, the parent is responsible for what execution id is being evaluated
    //       and for providing the right state. Instead we want to hide state access within this class.
    #[tracing::instrument]
    pub async fn step_execution(
        &mut self,
        prev_execution_id: ExecutionNodeId,
        previous_state: &ExecutionStateEvaluation,
    ) -> (
        ((usize, usize), ExecutionStateEvaluation), // the resulting total state of this step
        Vec<(usize, RkyvSerializedValue)>, // values emitted by operations during this step
    ) {
        let previous_state = match previous_state {
            ExecutionStateEvaluation::Complete(state) => state,
            ExecutionStateEvaluation::Executing(_) => panic!("Cannot step an execution state that is currently executing"),
        };
        // TODO: Execution can only be stepped when the previous state is complete
        let (new_state, outputs) = previous_state.step_execution(&self.sender).await;
        let resulting_state_id = self.progress_graph(prev_execution_id, new_state.clone());
        ((resulting_state_id, new_state), outputs)
    }


    #[tracing::instrument]
    pub fn mutate_graph(
        &mut self,
        prev_execution_id: ExecutionNodeId,
        cell: CellTypes,
        op_id: Option<usize>,
    ) -> (
        ((usize, usize), ExecutionState), // the resulting total state of this step
        usize, // id of the new operation
    ) {
        let state = self.get_state_at_id(prev_execution_id);
        if let Some(state) = state {
            let (final_state, op_id2) = match &state {
                ExecutionStateEvaluation::Complete(state1) => state1.update_op(cell, op_id),
                ExecutionStateEvaluation::Executing(_) => panic!("Cannot mutate a graph that is currently executing"),
            };
            let resulting_state_id = self.progress_graph(prev_execution_id, ExecutionStateEvaluation::Complete(final_state.clone()));
            ((resulting_state_id, final_state), op_id2)
        } else {
            panic!("No state found for id {:?}", prev_execution_id);
        }
    }

    #[tracing::instrument]
    pub async fn external_step_execution(
        &mut self,
        prev_execution_id: ExecutionNodeId,
    ) -> (
        ((usize, usize), ExecutionStateEvaluation), // the resulting total state of this step
        Vec<(usize, RkyvSerializedValue)>, // values emitted by operations during this step
    ) {
        let state = self.get_state_at_id(prev_execution_id);
        if let Some(state) = state {
            self.step_execution(prev_execution_id, &state).await
        } else {
            panic!("No state found for id {:?}", prev_execution_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::primitives::operation::{InputSignature, OperationNode, OutputSignature};
    use crate::execution::primitives::serialized_value::{
        RkyvSerializedValue as RSV, RkyvSerializedValue,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::runtime::Runtime;

    /*
    Testing the execution of individual nodes. Validating that operations as defined can be executed.
     */

    #[tokio::test]
    async fn test_evaluation_single_node() {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1) }.boxed()),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(2) }.boxed()),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["a", "b"]),
                    OutputSignature::new(),
                    Box::new(|_, args, _| {
                        async move {
                            if let RSV::Object(m) = args {
                                if let RSV::Object(args) = m.get("args").unwrap() {
                                    if let (Some(RSV::Number(a)), Some(RSV::Number(b))) =
                                        (args.get(&"0".to_string()), args.get(&"1".to_string()))
                                    {
                                        return RSV::Number(a + b);
                                    }
                                }
                            }

                            panic!("Invalid arguments")
                        }.boxed()
                    }),
                ),
                                                    None);

        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![
                    (0, DependencyReference::Positional(0)),
                    (1, DependencyReference::Positional(1)),
                ],
            }]);

        let arg0 = RSV::Number(1);
        let arg1 = RSV::Number(2);

        // Manually manipulating the state to insert the arguments for this test
        state.state_insert(0, arg0);
        state.state_insert(1, arg1);

        let ((_, new_state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state.clone())).await;

        assert!(new_state.state_get(&2).is_some());
        let result = new_state.state_get(&2).unwrap();
        assert_eq!(result, &RSV::Number(3));
    }

    /*
    Testing the traverse of the dependency graph. Validating that execution of the graph moves through
    the graph as expected.
     */

    #[tokio::test]
    async fn test_traverse_single_node() {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(0) }.boxed()),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1) }.boxed()),
                ),
                                                    None);
        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            }]);

        let ((_, new_state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(
            new_state.state_get(&1).unwrap(),
            &RkyvSerializedValue::Number(1)
        );
    }

    #[tokio::test]
    async fn test_traverse_linear_chain() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //    |
        //    2

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(0)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(2)}.boxed()),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
        ]);

        let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
    }

    #[tokio::test]
    async fn test_traverse_branching() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(0)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(2)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(3)}.boxed()),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
        ]);

        let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&3), Some(&RSV::Number(3)));
    }

    #[tokio::test]
    async fn test_traverse_branching_and_convergence() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3
        //   \ /
        //    4

        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(0)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(2)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(3)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1", "2"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(4)}.boxed()),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 4,
                depends_on: vec![
                    (2, DependencyReference::Positional(0)),
                    (3, DependencyReference::Positional(1)),
                ],
            },
        ]);

        let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&3), Some(&RSV::Number(3)));
        assert_eq!(state.state_get(&4), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(4)));
    }

    #[tokio::test]
    async fn test_traverse_cycle() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure _with the following cycle_
        //    0
        //    |
        //    1 * depends 1 -> 3
        //   / \
        //  2   3
        //   \ / * depends 3 -> 4
        //    4
        //    |
        //    5

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        // We start with the number 1 at node 0
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1)}.boxed()),
                ),
                                                    None);

        // Each node adds 1 to the inbound item (all nodes only have one dependency per index)
        let f1 = |_: &ExecutionState, args: RkyvSerializedValue, _| {
            async move {
                if let RSV::Object(m) = args {
                    if let RSV::Object(args) = m.get("args").unwrap() {
                        if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                            return RSV::Number(a + 1);
                        }
                    }
                }

                panic!("Invalid arguments")
            }.boxed()
        };

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![
                    (0, DependencyReference::Positional(0)),
                    (3, DependencyReference::Positional(0)),
                ],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(4, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 4,
                depends_on: vec![(2, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 5,
                depends_on: vec![(4, DependencyReference::Positional(0))],
            },
        ]);

        // We expect to see the value at each node increment repeatedly.
        let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;

        assert_eq!(state.state_get(&1), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(3)));
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(4)));
        assert_eq!(state.state_get(&5), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), Some(&RSV::Number(5)));
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), Some(&RSV::Number(6)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(7)));
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(8)));
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), Some(&RSV::Number(9)));
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(9)));
    }

    #[tokio::test]
    async fn test_branching_multiple_state_paths() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //    |
        //    2

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        // We start with the number 1 at node 0
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move {RSV::Number(1) }.boxed()),
                ),
                                                    None);

        // Globally mutates this value, making each call to this function side-effecting
        static atomic_usize: AtomicUsize = AtomicUsize::new(0);
        let f_side_effect = |_: &ExecutionState, args: RkyvSerializedValue, _| {
            async move {
                if let RSV::Object(m) = args {
                    if let RSV::Object(args) = m.get("args").unwrap() {
                        if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                            let plus = atomic_usize.fetch_add(1, Ordering::SeqCst);
                            return RSV::Number(a + plus as i32);
                        }
                    }
                }

                panic!("Invalid arguments")
            }.boxed()
        };

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_side_effect),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_side_effect),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
        ]);

        let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((x_state_id, x_state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(x_state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(x_state.state_get(&2), None);

        let ((state_id, state), _) = db.step_execution(x_state_id.clone(), &x_state.clone()).await;
        assert_eq!(state_id.0, 0);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));

        // When we re-evaluate from a previous point, we should get a new branch
        let ((state_id, state), _) = db.step_execution(x_state_id.clone(), &x_state).await;
        // The state_id.0 being incremented indicates that we're on a new branch
        assert_eq!(state_id.0, 1);
        assert_eq!(state.state_get(&1), None);
        // Op 2 should re-evaluate to 3, since it's on a new branch but continuing to mutate the stateful counter
        assert_eq!(state.state_get(&2), Some(&RSV::Number(3)));
    }

    #[tokio::test]
    async fn test_mutation_of_the_dependency_graph_on_branches() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1 * we're going to be changing the definiton of the function of this node on one branch
        //    |
        //    2

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        // We start with the number 0 at node 0
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(0)}.boxed()),
                ),
                                                    None);

        let f_v1 = |_: &ExecutionState, args: RkyvSerializedValue, _| {
            async move {
                if let RSV::Object(m) = args {
                    if let RSV::Object(args) = m.get("args").unwrap() {
                        if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                            return RSV::Number(a + 1);
                        }
                    }
                }

                panic!("Invalid arguments")
            }.boxed()
        };

        let f_v2 = |_: &ExecutionState, args: RkyvSerializedValue, _| {
            async move {
            if let RSV::Object(m) = args {
                if let RSV::Object(args) = m.get("args").unwrap() {
                    if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                        return RSV::Number(a + 200);
                    }
                }
            }

            panic!("Invalid arguments")
            }.boxed()
        };

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_v1),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_v1),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
        ]);

        let ((x_state_id, mut x_state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(x_state.state_get(&1), None);
        assert_eq!(x_state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(x_state_id, &x_state.clone()).await;
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));

        // Change the definition of the operation "1" to add 200 instead of 1, then re-evaluate
        if let ExecutionStateEvaluation::Complete(x_state) = x_state {
            let (_, mut state) = x_state.upsert_operation(OperationNode::new(
                None,
                InputSignature::from_args_list(vec!["0"]),
                OutputSignature::new(),
                Box::new(f_v2),
            ), Some(1));
            let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state.clone())).await;
            assert_eq!(state.state_get(&1), Some(&RSV::Number(200)));
            assert_eq!(state.state_get(&2), None);
            let ((state_id, state), _) = db.step_execution(state_id, &state).await;
            assert_eq!(state.state_get(&1), None);
            assert_eq!(state.state_get(&2), Some(&RSV::Number(201)));
        }

    }

    #[tokio::test]
    async fn test_composition_across_nodes() {
        // TODO: take two operators, and compose them into a single operator
    }

    #[tokio::test]
    async fn test_merging_traversed_state() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3
        //   \ /
        //    4

        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(0)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(1)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(2)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(3)}.boxed()),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1", "2"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _| async move { RSV::Number(4)}.boxed()),
                ),
                                                    None);

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(1, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 4,
                depends_on: vec![
                    (2, DependencyReference::Positional(0)),
                    (3, DependencyReference::Positional(1)),
                ],
            },
        ]);

        let ((state_id, state), _) = db.step_execution(state_id, &ExecutionStateEvaluation::Complete(state)).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&3), Some(&RSV::Number(3)));
        assert_eq!(state.state_get(&4), None);

        // This is the final state we're arriving at in execution
        let ((state_id, state), _) = db.step_execution(state_id, &state).await;
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(4)));

        // TODO: convert this to an actual test
        // dbg!(db.get_merged_state_history(state_id));
    }

}