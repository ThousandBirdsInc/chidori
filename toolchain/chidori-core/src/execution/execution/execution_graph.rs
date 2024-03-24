use std::collections::HashMap;
use crate::execution::execution::execution_state::{DependencyGraphMutation, ExecutionState};
use std::fmt;
use std::fmt::Formatter;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll};
// use std::sync::{Mutex};
use no_deadlocks::{Mutex};
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
use crate::cells::CellTypes;

// TODO: update all of these identifies to include a "space" they're within

type EdgeIdentity = (OperationId, OperationId, DependencyReference);

/// ExecutionId is a unique identifier for a point in the execution graph.
/// It is a tuple of (branch, counter) where branch is an instance of a divergence from a previous
/// execution state. Counter is the iteration of steps taken in that branch.
// TODO: add a globally incrementing counter to this so that there can never be identity collisions
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
/// This is heavily inspired by works such as Salsa, Verde, Incremental, Adapton, and Differential Dataflow.
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
    execution_graph: Arc<Mutex<DiGraphMap<ExecutionNodeId, ExecutionState>>>,

    state_id_to_state: Arc<Mutex<HashMap<ExecutionNodeId, ExecutionState>>>,

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
    execution_graph: &mut DiGraphMap<ExecutionNodeId, ExecutionState>,
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
        state_id_to_state.insert((0, 0), ExecutionState::new());
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
                        let mut new_state = state_id_to_state.get(&prev_execution_id).unwrap().clone();
                        new_state.state_insert(operation_id, result);

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
            state_id_to_state,
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

    pub fn get_state_at_id(&self, id: ExecutionNodeId) -> Option<ExecutionState> {
        let state_id_to_state = self.state_id_to_state.lock().unwrap();
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
        // TODO: for some reason this is resulting in exploring the other branches as well
        dbg!(&queue);
        let mut merged_state = HashMap::new();
        for predecessor in queue {
            let state = self.get_state_at_id(predecessor).unwrap();
            for (k, v) in state.state.iter() {
                merged_state.insert(*k, (predecessor, v.clone()));
            }
        }
        MergedStateHistory(merged_state)
    }

    #[tracing::instrument]
    pub fn mutate_graph(
        &mut self,
        prev_execution_id: ExecutionNodeId,
        previous_state: &ExecutionState,
        cell: CellTypes,
        op_id: Option<usize>,
    ) -> (
        ((usize, usize), ExecutionState), // the resulting total state of this step
        usize, // id of the new operation
    ) {
        let mut op = match &cell {
            CellTypes::Code(c) => crate::cells::code_cell::code_cell(c),
            CellTypes::Prompt(c) => crate::cells::llm_prompt_cell::llm_prompt_cell(c),
            CellTypes::Web(c) => crate::cells::web_cell::web_cell(c),
            CellTypes::Template(c) => crate::cells::template_cell::template_cell(c),
        };
        op.attach_cell(cell);
        let (op_id, new_state) = previous_state.upsert_operation(op, op_id);

        // TODO: when there is a dependency on a function invocation we need to
        //       instantiate a new instance of the function operation node.
        //       It itself is not part of the call graph until it has such a dependency.

        let mut available_values = HashMap::new();
        let mut available_functions = HashMap::new();

        // For all reported cells, add their exposed values to the available values
        for (id, op) in new_state.operation_by_id.iter() {
            let output_signature = &op.lock().unwrap().signature.output_signature;

            // Store values that are available as globals
            for (key, value) in output_signature.globals.iter() {
                // TODO: throw an error if there is a naming collision
                available_values.insert(key.clone(), id);
            }

            for (key, value) in output_signature.functions.iter() {
                // TODO: throw an error if there is a naming collision
                available_functions.insert(key.clone(), id);
            }

            // TODO: Store triggerable functions that may be passed as values as well
        }

        // TODO: we need to report on INVOKED functions - these functions are calls to
        //       functions with the locals assigned in a particular way. But then how do we handle compositions of these?
        //       Well we just need to invoke them in the correct pattern as determined by operations in that context.

        // Anywhere there is a matched value, we create a dependency graph edge
        let mut mutations = vec![];

        // let mut unsatisfied_dependencies = vec![];
        // For each destination cell, we inspect their input signatures and accumulate the
        // mutation operations that we need to apply to the dependency graph.
        for (destination_cell_id, op) in new_state.operation_by_id.iter() {
            let operation = op.lock().unwrap();
            let input_signature = &operation.signature.input_signature;
            let mut accum = vec![];
            for (value_name, value) in input_signature.globals.iter() {

                // TODO: we need to handle collisions between the two of these
                if let Some(source_cell_id) = available_functions.get(value_name) {
                    if source_cell_id != &destination_cell_id {
                        accum.push((
                            *source_cell_id.clone(),
                            DependencyReference::FunctionInvocation(value_name.to_string()),
                        ));
                    }
                }

                if let Some(source_cell_id) = available_values.get(value_name) {
                    if source_cell_id != &destination_cell_id {
                        accum.push((
                            *source_cell_id.clone(),
                            DependencyReference::Global(value_name.to_string()),
                        ));
                    }
                }
                // unsatisfied_dependencies.push(value_name.clone())
            }
            if accum.len() > 0 {
                mutations.push(DependencyGraphMutation::Create {
                    operation_id: destination_cell_id.clone(),
                    depends_on: accum,
                });
            }
        }

        let final_state = new_state.apply_dependency_graph_mutations(mutations);
        let resulting_state_id = self.progress_graph(prev_execution_id, final_state.clone());
        ((resulting_state_id, final_state), op_id)
    }

    fn progress_graph(&mut self, prev_execution_id: ExecutionNodeId, new_state: ExecutionState) -> ExecutionNodeId {
        // The edge from this node is the greatest branching id + 1
        // if we re-evaluate execution at a given node, we get a new execution branch.
        let mut execution_graph = self.execution_graph.lock().unwrap();
        let mut state_id_to_state = self.state_id_to_state.lock().unwrap();
        let resulting_state_id = get_execution_id(execution_graph.deref_mut(), prev_execution_id);
        state_id_to_state.deref_mut().insert(resulting_state_id.clone(), new_state.clone());
        execution_graph.deref_mut()
            .add_edge(prev_execution_id, resulting_state_id.clone(), new_state.clone());
        resulting_state_id
    }

    #[tracing::instrument]
    pub fn step_execution(
        &mut self,
        prev_execution_id: ExecutionNodeId,
        previous_state: &ExecutionState,
    ) -> (
        ((usize, usize), ExecutionState), // the resulting total state of this step
        Vec<(usize, RkyvSerializedValue)>, // values emitted by operations during this step
    ) {
        let (new_state, outputs) = previous_state.step_execution(&self.sender);
        let resulting_state_id = self.progress_graph(prev_execution_id, new_state.clone());
        ((resulting_state_id, new_state), outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::primitives::operation::{InputSignature, OperationNode, OutputSignature};
    use crate::execution::primitives::serialized_value::{
        deserialize_from_buf, serialize_to_vec, ArchivedRkyvSerializedValue,
    };
    use crate::execution::primitives::serialized_value::{
        RkyvSerializedValue as RSV, RkyvSerializedValue,
    };
    use log::warn;
    use rkyv::ser::serializers::AllocSerializer;
    use rkyv::ser::Serializer;
    use rkyv::{archived_root, Deserialize, Serialize};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::runtime::Runtime;

    /*
    Testing the execution of individual nodes. Validating that operations as defined can be executed.
     */

    #[test]
    fn test_evaluation_single_node() {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(2)),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["a", "b"]),
                    OutputSignature::new(),
                    Box::new(|args, _| {
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

        let ((_, new_state), _) = db.step_execution(state_id, &state.clone());

        assert!(new_state.state_get(&2).is_some());
        let result = new_state.state_get(&2).unwrap();
        assert_eq!(result, &RSV::Number(3));
    }

    /*
    Testing the traverse of the dependency graph. Validating that execution of the graph moves through
    the graph as expected.
     */

    #[test]
    fn test_traverse_single_node() {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(0)),
                ),
                                                    None);
        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);
        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, DependencyReference::Positional(0))],
            }]);

        let ((_, new_state), _) = db.step_execution(state_id, &state);
        assert_eq!(
            new_state.state_get(&1).unwrap(),
            &RkyvSerializedValue::Number(1)
        );
    }

    #[test]
    fn test_traverse_linear_chain() {
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
                    Box::new(|_args, _| RSV::Number(0)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(2)),
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

        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
    }

    #[test]
    fn test_traverse_branching() {
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
                    Box::new(|_args, _| RSV::Number(0)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(2)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(3)),
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

        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&3), Some(&RSV::Number(3)));
    }

    #[test]
    fn test_traverse_branching_and_convergence() {
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
                    Box::new(|_args, _| RSV::Number(0)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(2)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(3)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1", "2"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(4)),
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

        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&3), Some(&RSV::Number(3)));
        assert_eq!(state.state_get(&4), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(4)));
    }

    #[test]
    fn test_traverse_cycle() {
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
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);

        // Each node adds 1 to the inbound item (all nodes only have one dependency per index)
        let f1 = |args: RkyvSerializedValue, _| {
            if let RSV::Object(m) = args {
                if let RSV::Object(args) = m.get("args").unwrap() {
                    if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                        return RSV::Number(a + 1);
                    }
                }
            }

            panic!("Invalid arguments")
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
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);

        assert_eq!(state.state_get(&1), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(3)));
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(4)));
        assert_eq!(state.state_get(&5), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), Some(&RSV::Number(5)));
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), Some(&RSV::Number(6)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(7)));
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(8)));
        assert_eq!(state.state_get(&5), Some(&RSV::Number(5)));
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), Some(&RSV::Number(9)));
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), Some(&RSV::Number(9)));
    }

    #[test]
    fn test_branching_multiple_state_paths() {
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
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);

        // Globally mutates this value, making each call to this function side-effecting
        static atomic_usize: AtomicUsize = AtomicUsize::new(0);
        let f_side_effect = |args: RkyvSerializedValue, _| {
            if let RSV::Object(m) = args {
                if let RSV::Object(args) = m.get("args").unwrap() {
                    if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                        let plus = atomic_usize.fetch_add(1, Ordering::SeqCst);
                        return RSV::Number(a + plus as i32);
                    }
                }
            }

            panic!("Invalid arguments")
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

        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((x_state_id, x_state), _) = db.step_execution(state_id, &state);
        assert_eq!(x_state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(x_state.state_get(&2), None);

        let ((state_id, state), _) = db.step_execution(x_state_id.clone(), &x_state.clone());
        assert_eq!(state_id.0, 0);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));

        // When we re-evaluate from a previous point, we should get a new branch
        let ((state_id, state), _) = db.step_execution(x_state_id.clone(), &x_state);
        // The state_id.0 being incremented indicates that we're on a new branch
        assert_eq!(state_id.0, 1);
        assert_eq!(state.state_get(&1), None);
        // Op 2 should re-evaluate to 3, since it's on a new branch but continuing to mutate the stateful counter
        assert_eq!(state.state_get(&2), Some(&RSV::Number(3)));
    }

    #[test]
    fn test_mutation_of_the_dependency_graph_on_branches() {
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
                    Box::new(|_args, _| RSV::Number(0)),
                ),
                                                    None);

        let f_v1 = |args: RkyvSerializedValue, _| {
            if let RSV::Object(m) = args {
                if let RSV::Object(args) = m.get("args").unwrap() {
                    if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                        return RSV::Number(a + 1);
                    }
                }
            }

            panic!("Invalid arguments")
        };

        let f_v2 = |args: RkyvSerializedValue, _| {
            if let RSV::Object(m) = args {
                if let RSV::Object(args) = m.get("args").unwrap() {
                    if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                        return RSV::Number(a + 200);
                    }
                }
            }

            panic!("Invalid arguments")
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

        let ((x_state_id, mut x_state), _) = db.step_execution(state_id, &state);
        assert_eq!(x_state.state_get(&1), None);
        assert_eq!(x_state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(x_state_id, &x_state.clone());
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));

        // Change the definition of the operation "1" to add 200 instead of 1, then re-evaluate
        // TODO: we can no longer overwrite operations
        let (_, mut state) = x_state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_v2),
                ),
                                                      None);

        let ((state_id, state), _) = db.step_execution(state_id, &state.clone());
        assert_eq!(state.state_get(&1), Some(&RSV::Number(200)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(201)));
    }

    #[test]
    fn test_composition_across_nodes() {
        // TODO: take two operators, and compose them into a single operator
    }

    #[test]
    fn test_merging_traversed_state() {
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
                    Box::new(|_args, _| RSV::Number(0)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(1)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(2)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(3)),
                ),
                                                    None);

        let (_, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    InputSignature::from_args_list(vec!["1", "2"]),
                    OutputSignature::new(),
                    Box::new(|_args, _| RSV::Number(4)),
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

        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), Some(&RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), Some(&RSV::Number(2)));
        assert_eq!(state.state_get(&3), Some(&RSV::Number(3)));
        assert_eq!(state.state_get(&4), None);

        // This is the final state we're arriving at in execution
        let ((state_id, state), _) = db.step_execution(state_id, &state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), Some(&RSV::Number(4)));

        // TODO: convert this to an actual test
        // dbg!(db.get_merged_state_history(state_id));
    }

    #[test]
    fn test_future_behavior() {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use futures::Future;
        use tokio::sync::oneshot;
        use tokio::sync::oneshot::{Receiver, Sender};

        // TODO: this needs to be wrapped in something to hold its already calculated state, caching the result
        // when it actually has been resolved

        // MyFuture struct as defined earlier
        struct MyFuture {
            receiver: Option<Receiver<i32>>,
            cached_result: Option<i32>,
        }

        impl Future for MyFuture {
            type Output = Option<i32>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                if let Some(rx) = self.receiver.as_mut() {
                    match rx.poll_unpin(cx) {
                        Poll::Ready(Ok(value)) => Poll::Ready(Some(value)),
                        Poll::Ready(Err(_)) => Poll::Ready(None), // Channel was closed
                        Poll::Pending => Poll::Pending,
                    }
                } else {
                    Poll::Ready(None) // Receiver was already taken and we received nothing
                }
            }
        }

        Runtime::new().unwrap().block_on(async move {
            // Simulating some asynchronous work that sends a value through the oneshot channel
            let (sender, rx) = oneshot::channel();
            let my_future = MyFuture { receiver: Some(rx), cached_result: None };
            tokio::spawn(async move {
                // Simulate some work with a delay
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                sender.send(42).expect("Failed to send value");
            });

            // Await the MyFuture, which internally awaits the oneshot receiver
            match my_future.await {
                Some(value) => println!("Received: {}", value),
                None => println!("Did not receive any value"),
            }
        });
    }
}