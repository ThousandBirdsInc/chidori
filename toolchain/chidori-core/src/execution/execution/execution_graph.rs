use std::collections::{HashMap, HashSet, VecDeque};
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
use std::thread::{JoinHandle};
use std::time::Duration;
use anyhow::anyhow;
use dashmap::DashMap;
use futures_util::FutureExt;

use crate::execution::primitives::identifiers::{DependencyReference, OperationId};

use petgraph::data::Build;
use petgraph::dot::Dot;

use crate::execution::primitives::serialized_value::RkyvSerializedValue;
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::{IntoEdgesDirected, VisitMap};
use petgraph::Direction;
use petgraph::prelude::Dfs;
pub use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::ser::SerializeMap;
use tokio::signal;
use tokio::sync::Notify;
use tokio::sync::oneshot::Receiver;
use uuid::Uuid;
use crate::cells::CellTypes;
use crate::execution::primitives::operation::OperationFnOutput;
use tokio::sync::mpsc::{Sender, channel};

// TODO: update all of these identifies to include a "space" they're within

type EdgeIdentity = (OperationId, OperationId, DependencyReference);

/// ExecutionId is a unique identifier for a point in the execution graph.
/// It is a tuple of (branch, counter) where branch is an instance of a divergence from a previous
/// execution state. Counter is the iteration of steps taken in that branch.
// TODO: add a globally incrementing counter to this so that there can never be identity collisions

// TODO: (branch, depth, counter)
pub type ExecutionNodeId = Uuid;


#[derive(Debug, Clone)]
pub struct MergedStateHistory(pub HashMap<OperationId, (ExecutionNodeId, Arc<OperationFnOutput>)>);

// TODO: every point in the execution graph should be a top level element in an execution stack
//       but what about async functions?

// TODO: we need to introduce a concept of depth in the execution graph.
//       depth could be more edges from an element with a particular labelling.
//       we would need execution ids to have both branch, counter (steps), and depth they occur at.

// TODO: we need to effectively step inside of a function, so effectively these are fractional
//       steps we take within a given counter. but then those can nest even further.
//

pub type ExecutionGraphSendPayload = (ExecutionStateEvaluation, Option<tokio::sync::oneshot::Sender<()>>);

pub type ExecutionGraphDiGraphSet = DiGraphMap<ExecutionNodeId, ExecutionStateEvaluation>;


/// This models the network of reactive relationships between different components.
///
/// This was initially inspired by works such as Salsa, Verde, Incremental, Adapton, and Differential Dataflow.
pub struct ExecutionGraph {
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
    execution_graph: Arc<Mutex<ExecutionGraphDiGraphSet>>,


    /// Sender channel for sending messages to the execution graph
    graph_mutation_sender: tokio::sync::mpsc::Sender<ExecutionGraphSendPayload>,


    execution_event_sender: Sender<ExecutionEvent>,
    execution_event_receiver: Option<tokio::sync::mpsc::Receiver<ExecutionEvent>>,

    // TODO: move to just using the digraph for this
    pub(crate) execution_node_id_to_state: Arc<DashMap<ExecutionNodeId, ExecutionStateEvaluation>>,

    pub execution_depth_orchestration_handle: tokio::task::JoinHandle<()>,
    pub execution_depth_orchestration_initialized_notify: Arc<Notify>,
    pub cancellation_notify: Arc<Notify>,

    /// This is a queue of chat messages applied to our execution system
    /// execution states maintain a value that indicates the head location within this queue
    /// that they've processed thus far.
    pub chat_message_queue: Vec<String>,
}

impl std::fmt::Debug for ExecutionGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionGraph")
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
pub fn get_execution_id() -> Uuid {
    Uuid::now_v7()
}

pub fn get_operation_id() -> Uuid {
    Uuid::now_v7()
}

#[derive(Debug, Clone)]
pub struct ExecutionEvent {
    pub id: ExecutionNodeId,
    pub evaluation: ExecutionStateEvaluation
}

impl ExecutionGraph {
    #[tracing::instrument]
    pub fn new() -> Self {
        println!("Initializing ExecutionGraph");
        let (sender, mut execution_graph_event_receiver) = tokio::sync::mpsc::channel::<ExecutionGraphSendPayload>(1028);

        let mut state_id_to_state = DashMap::new();

        // Initialization of the execution graph at Uuid::nil - this is always the root of the execution graph
        let init_id = Uuid::nil();
        state_id_to_state.insert(init_id, ExecutionStateEvaluation::Complete(ExecutionState::new_with_graph_sender(
            init_id,
            Arc::new(sender.clone())
        )));

        // Graph of execution states
        let mut execution_graph = Arc::new(Mutex::new(DiGraphMap::new()));
        let execution_graph_clone = execution_graph.clone();

        // Mapping of state_ids to state
        let mut state_id_to_state = Arc::new(state_id_to_state);
        let state_id_to_state_clone = state_id_to_state.clone();

        // Notification of successful startup
        let initialization_notify = Arc::new(Notify::new());
        let initialization_notify_clone = initialization_notify.clone();

        // Notification for shutdown
        let cancellation_notify = Arc::new(Notify::new());
        let cancellation_notify_clone = cancellation_notify.clone();


        // Channel for execution_events
        let (execution_event_tx, execution_event_rx) = channel::<ExecutionEvent>(100);
        let execution_event_tx_clone = execution_event_tx.clone();

        // Kick off a background thread that listens for events from async operations
        // These events inject additional state into the execution graph on new branches
        // Those branches will continue to evaluate independently.
        println!("Initializing background thread for handling async updates to our execution graph");
        let handle = tokio::spawn(async move {
            // Signal that the task has started, we can continue initialization
            initialization_notify_clone.notify_one();
            loop {
                tokio::select! {
                    biased;  // Prioritize message handling over cancellation

                    Some((resulting_execution_state, oneshot)) = execution_graph_event_receiver.recv() => {
                        println!("==== Execution Graph received dispatch event {:?}", resulting_execution_state.id());

                        let s = resulting_execution_state.clone();
                        match &resulting_execution_state {
                            ExecutionStateEvaluation::Error(state) => {
                                let resulting_state_id = state.id;
                                if let Err(e) = execution_event_tx_clone.send(ExecutionEvent{
                                    id: resulting_state_id.clone(),
                                    evaluation: s.clone()
                                }).await {
                                    println!("Failed to send execution event: {}", e);
                                }
                            }
                            ExecutionStateEvaluation::EvalFailure(id) => {
                                if let Err(e) = execution_event_tx_clone.send(ExecutionEvent{
                                    id: id.clone(),
                                    evaluation: s.clone()
                                }).await {
                                    println!("Failed to send execution event: {}", e);
                                }
                            }
                            ExecutionStateEvaluation::Complete(state) => {
                                let resulting_state_id = state.id;
                                if let Err(e) = execution_event_tx_clone.send(ExecutionEvent{
                                    id: resulting_state_id.clone(),
                                    evaluation: s.clone()
                                }).await {
                                    println!("Failed to send execution event: {}", e);
                                }
                            }
                            ExecutionStateEvaluation::Executing(state) => {
                                let resulting_state_id = state.id;
                                if let Err(e) = execution_event_tx_clone.send(ExecutionEvent{
                                    id: resulting_state_id.clone(),
                                    evaluation: s.clone()
                                }).await {
                                    println!("Failed to send execution event: {}", e);
                                }
                            }
                        }

                        // Pushing this state into the graph
                        let mut execution_graph = execution_graph_clone.lock().unwrap();
                        let mut state_id_to_state = state_id_to_state_clone.clone();

                        match resulting_execution_state {
                            ExecutionStateEvaluation::Error(state) |
                            ExecutionStateEvaluation::Complete(state) => {
                                let resulting_state_id = state.id;
                                state_id_to_state.insert(resulting_state_id.clone(), s.clone());
                                execution_graph.deref_mut()
                                    .add_edge(state.parent_state_id, resulting_state_id.clone(), s);
                            }
                            ExecutionStateEvaluation::EvalFailure(id) => {
                                state_id_to_state.insert(id.clone(), s.clone());
                            }
                            ExecutionStateEvaluation::Executing(state) => {
                                let resulting_state_id = state.id;
                                if let Some(ExecutionStateEvaluation::Complete(_)) = state_id_to_state.get(&resulting_state_id).map(|x| x.clone()) {
                                    // State is already complete, skip updating
                                } else {
                                    state_id_to_state.insert(resulting_state_id.clone(), s.clone());
                                    execution_graph.deref_mut()
                                        .add_edge(state.parent_state_id, resulting_state_id.clone(), s);
                                }
                            }
                        }

                        // Resume execution
                        if let Some(oneshot) = oneshot {
                            oneshot.send(()).expect("Failed to send oneshot completion signal")
                        }
                    }
                    _ = cancellation_notify_clone.notified() => {
                        println!("Task is notified to stop");
                        return;
                    }
                }
            }
        });
        let graph = ExecutionGraph {
            cancellation_notify,
            execution_depth_orchestration_initialized_notify: initialization_notify,
            execution_depth_orchestration_handle: handle,
            execution_node_id_to_state: state_id_to_state,
            execution_graph,
            graph_mutation_sender: sender,
            chat_message_queue: vec![],
            execution_event_sender: execution_event_tx,
            execution_event_receiver: Some(execution_event_rx)
        };

        graph
    }

    pub fn take_execution_event_receiver(&mut self) -> tokio::sync::mpsc::Receiver<ExecutionEvent> {
        self.execution_event_receiver.take().expect("Execution event receiver may only be taken once by a new owner")
    }

    pub async fn shutdown(&mut self) {
        self.cancellation_notify.notify_one();
    }

    #[tracing::instrument]
    pub fn get_execution_graph_elements(&self) -> (Vec<(ExecutionNodeId, ExecutionNodeId)>, HashSet<ExecutionNodeId>)  {
        let execution_graph = self.execution_graph.lock().unwrap();
        let mut grouped_nodes = HashSet::new();
        for (source, target, ev) in execution_graph.deref().all_edges() {
            match ev {
                ExecutionStateEvaluation::EvalFailure(_) => {},
                ExecutionStateEvaluation::Error(_) => {}
                ExecutionStateEvaluation::Executing(s) => {
                }
                ExecutionStateEvaluation::Complete(s) => {
                    if !s.stack.is_empty() {
                        for item in s.stack.iter().cloned() {
                            grouped_nodes.insert(item);
                        }
                    }
                }
            }

        }

        (execution_graph.deref().all_edges().map(|x| (x.0, x.1)).collect(), grouped_nodes)
    }

    pub fn get_state_at_id(&self, id: ExecutionNodeId) -> Option<ExecutionStateEvaluation> {
        let state_id_to_state = self.execution_node_id_to_state.clone();
        state_id_to_state.get(&id).map(|x| x.clone())
    }

    #[tracing::instrument]
    pub fn get_merged_state_history(&self, endpoint: &ExecutionNodeId) -> MergedStateHistory {
        println!("Getting merged state history for id {:?}", &endpoint);
        let execution_graph = self.execution_graph.lock();
        let graph = execution_graph.as_ref().unwrap();
        let mut dfs = Dfs::new(graph.deref(), endpoint.clone());
        let root = Uuid::nil();
        let mut queue = vec![endpoint.clone()];
        while let Some(node) = dfs.next(graph.deref()) {
            if node == root {
                break;
            }
            for predecessor in graph.neighbors_directed(node, Direction::Incoming) {
                if !dfs.discovered.is_visited(&predecessor) {
                    queue.push(predecessor);
                    dfs.stack.push(predecessor);
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

    #[tracing::instrument]
    pub async fn push_message(
        &mut self,
        message: String,
    ) -> anyhow::Result<()> {
        self.chat_message_queue.push(message);
        Ok(())
    }

    #[tracing::instrument]
    pub fn progress_graph(&mut self, new_state: ExecutionStateEvaluation) -> ExecutionNodeId {
        let mut execution_graph = self.execution_graph.lock().unwrap();
        let mut state_id_to_state = self.execution_node_id_to_state.clone();
        let (parent_id, resulting_state_id ) = match &new_state {
            ExecutionStateEvaluation::Complete(state) => {
                (state.parent_state_id, state.id)
            },
            ExecutionStateEvaluation::Executing(state) =>
                (state.parent_state_id, state.id)
,
            ExecutionStateEvaluation::Error(_) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),
        };
        println!("Resulting state received from progress_graph {:?}", &resulting_state_id);
        // TODO: if state already exists how to handle
        state_id_to_state.insert(resulting_state_id.clone(), new_state.clone());
        execution_graph.deref_mut()
            .add_edge(parent_id, resulting_state_id.clone(), new_state.clone());
        resulting_state_id
    }

    pub async fn one_off_execute_code_against_state(
        &self,
        state_id: usize,
        code_cell: &CellTypes,
        cell: &CellTypes,
        args: RkyvSerializedValue
    ) -> anyhow::Result<(
        (ExecutionNodeId, ExecutionStateEvaluation), // the resulting total state of this step
        Vec<(OperationId, OperationFnOutput)>, // values emitted by operations during this step
    )> {
        // ExecutionGraph::immutable_external_step_execution(state_id, state)
        // TODO: we want a code cell that depends on the target cell

        // TODO: treat the repl as a two cell instances, one being the original cell, two being the evaluator
        // TODO: need to fetch functions that we depend on from other cells
        let mut new_state = ExecutionState::default();
        let mut op_node = new_state.get_operation_from_cell_type(cell)?;
        op_node.attach_cell(cell.clone());
        new_state.evaluating_cell = Some(op_node.cell.clone());
        let result = op_node.execute(&mut new_state, args, None, None).await?;

        let mut outputs = vec![];
        let next_operation_id = new_state.evaluating_id.clone();
        outputs.push((next_operation_id, result.clone()));
        // new_state.update_state(&mut new_state, next_operation_id, result);

        println!("step_execution about to complete, after update_state");
        todo!("complete implementations");
    }

    #[tracing::instrument]
    pub fn update_operation(
        &mut self,
        prev_execution_id: ExecutionNodeId,
        cell: CellTypes,
        op_id: OperationId,
    ) -> anyhow::Result<(
        (ExecutionNodeId, ExecutionStateEvaluation), // the resulting total state of this step
        OperationId, // id of the new operation
    )> {
        let state = self.get_state_at_id(prev_execution_id)
            .ok_or_else(|| anyhow!("No state found for id {:?}", prev_execution_id))?;
        let (final_state, op_id) = match state {
            ExecutionStateEvaluation::Complete(state) => {
                println!( "Capturing state of the mutate graph operation parent {:?}, id {:?}", state.parent_state_id, state.id);
                state.update_operation(cell, op_id)?
            },
            ExecutionStateEvaluation::Executing(..) => {
                return Err(anyhow!("Cannot mutate a graph that is currently executing"))
            },

            ExecutionStateEvaluation::Error(_) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),

        };

        let eval = ExecutionStateEvaluation::Complete(final_state.clone());
        println!("Capturing final_state of the mutate graph operation parent {:?}, id {:?}", final_state.parent_state_id, final_state.id);

        let resulting_state_id = self.progress_graph(eval.clone());
        Ok(((resulting_state_id, eval), op_id))
    }

    #[tracing::instrument]
    pub async fn immutable_external_step_execution(
        state: ExecutionStateEvaluation,
    ) -> anyhow::Result<(
        ExecutionNodeId,
        ExecutionStateEvaluation, // the resulting total state of this step
        Vec<(OperationId, OperationFnOutput)>, // values emitted by operations during this step
    )> {
        println!("step_execution_with_previous_state {:?}", &state);
        let previous_state = match &state {
            ExecutionStateEvaluation::Complete(state1) => state1,
            _ => { panic!("Stepping execution should only occur against completed states") }
        };
        let resolved_state_id = previous_state.id;
        let (new_state, outputs) = previous_state.step_execution().await?;
        Ok((resolved_state_id, new_state, outputs))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};
    use super::*;
    use crate::execution::primitives::operation::{InputSignature, OperationFnOutput, OperationNode, OutputSignature};
    use crate::execution::primitives::serialized_value::{
        RkyvSerializedValue as RSV, RkyvSerializedValue,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::runtime::Runtime;

    /*
    Testing the execution of individual nodes. Validating that operations as defined can be executed.
     */

    #[tokio::test]
    async fn test_evaluation_single_node() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();
        let (id_a, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(1))) }.boxed()),
                ),
                                                    Uuid::now_v7());
        let (id_b, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(2))) }.boxed()),
                ),
                                                    Uuid::now_v7());
        let (id_c, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["a", "b"]),
                    OutputSignature::new(),
                    Box::new(|_, args, _, _| {
                        async move {
                            if let RSV::Object(m) = args {
                                if let RSV::Object(args) = m.get("args").unwrap() {
                                    if let (Some(RSV::Number(a)), Some(RSV::Number(b))) =
                                        (args.get(&"0".to_string()), args.get(&"1".to_string()))
                                    {
                                        return Ok(OperationFnOutput::with_value(RSV::Number(a + b)));
                                    }
                                }
                            }

                            panic!("Invalid arguments")
                        }.boxed()
                    }),
                ),
                                                    Uuid::now_v7());

        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![
                    (id_a, DependencyReference::Positional(0)),
                    (id_b, DependencyReference::Positional(1)),
                ],
            }]);

        let arg0 = RSV::Number(1);
        let arg1 = RSV::Number(2);

        // Manually manipulating the state to insert the arguments for this test
        state.state_insert(id_a, OperationFnOutput {
            has_error: false,
            execution_state: None,
            output: Ok(arg0),
            stdout: vec![],
            stderr: vec![],
        });
        state.state_insert(id_b, OperationFnOutput {
            has_error: false,
            execution_state: None,
            output: Ok(arg1),
            stdout: vec![],
            stderr: vec![],
        });
        let (_, new_state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state.clone())).await?;
        assert!(new_state.state_get_value(&id_c).is_some());
        let result = new_state.state_get_value(&id_c).unwrap();
        assert_eq!(result, &Ok(RSV::Number(3)));
        Ok(())
    }

    /*
    Testing the traverse of the dependency graph. Validating that execution of the graph moves through
    the graph as expected.
     */

    #[tokio::test]
    async fn test_traverse_single_node() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();
        let (id_a, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(0))) }.boxed()),
                ),
                                                    Uuid::now_v7());
        let (id_b, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(1))) }.boxed()),
                ),
                                                    Uuid::now_v7());
        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            }]);

        let (_, new_state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state.clone())).await?;
        assert_eq!(
            new_state.state_get_value(&id_a).unwrap(),
            &Ok(RkyvSerializedValue::Number(0))
        );

        let (_, new_state, _) = ExecutionGraph::immutable_external_step_execution(new_state).await?;
        assert_eq!(
            new_state.state_get_value(&id_b).unwrap(),
            &Ok(RkyvSerializedValue::Number(1))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_traverse_linear_chain() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //    |
        //    2

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();

        let (id_a, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(0))) }.boxed()),
                ),
                                                    Uuid::now_v7());

        let (id_b, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(1))) }.boxed()),
                ),
                                                    Uuid::now_v7());

        let (id_c, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(2))) }.boxed()),
                ),
                                                    Uuid::now_v7());

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
        ]);

        let (state_id, new_state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state.clone())).await?;
        assert_eq!(state.state_get_value(&id_b), None);
        assert_eq!(state.state_get_value(&id_c), None);
        // if let ExecutionStateEvaluation::Complete(s) = &state {
        //     assert_eq!(s.exec_queue, VecDeque::from(vec![1,2]));
        // }
        let (state_id, new_state, _) = ExecutionGraph::immutable_external_step_execution(new_state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), None);
        // if let ExecutionStateEvaluation::Complete(s) = &state {
        //     assert_eq!(s.exec_queue, VecDeque::from(vec![2]));
        // }
        let (state_id, new_state, _) = ExecutionGraph::immutable_external_step_execution(new_state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        // if let ExecutionStateEvaluation::Complete(s) = &state {
        //     assert_eq!(s.exec_queue, VecDeque::from(vec![]));
        // }
        db.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_traverse_branching() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();

        let mut ids = vec![];
        for x in 0..4 {
            let (id, mut nstate) = state.upsert_operation(OperationNode::new(
                        None,
                        Uuid::nil(),
                        if x == 0 { InputSignature::new() } else { InputSignature::from_args_list(vec!["0"]) },
                        OutputSignature::new(),
                        Box::new(move |_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(x))) }.boxed()),
                    ),
                                                        Uuid::now_v7());
            ids.push(id);
            state = nstate
        }
        let id_a = ids[0];
        let id_b = ids[1];
        let id_c = ids[2];
        let id_d = ids[3];

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_d,
                depends_on: vec![(id_c, DependencyReference::Positional(0))],
            },
        ]);

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        assert_eq!(state.state_get_value(&id_b), None);
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), None);
        assert_eq!(state.state_get_value(&id_d), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(3))));
        db.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_traverse_branching_and_convergence() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3
        //   \ /
        //    4

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();
        let mut ids = vec![];
        for x in 0..5 {
            let (id, mut nstate) = state.upsert_operation(OperationNode::new(
                        None,
                        Uuid::nil(),
                        if x == 0 { InputSignature::new() } else if x == 4 {InputSignature::from_args_list(vec!["1", "2"]) } else { InputSignature::from_args_list(vec!["1"]) },
                        OutputSignature::new(),
                        Box::new(move |_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(x))) }.boxed()),
                    ),
                                                        Uuid::now_v7());
            ids.push(id);
            state = nstate
        }
        let id_a = ids[0];
        let id_b = ids[1];
        let id_c = ids[2];
        let id_d = ids[3];
        let id_e = ids[4];

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_d,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_e,
                depends_on: vec![
                    (id_c, DependencyReference::Positional(0)),
                    (id_d, DependencyReference::Positional(1)),
                ],
            },
        ]);

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        assert_eq!(state.state_get_value(&id_b), None);
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_e), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));
        Ok(())
    }

    #[tokio::test]
    async fn test_traverse_cycle() -> anyhow::Result<()> {
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

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();

        // We start with the number 1 at node 0
        let (id_a, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(1))) }.boxed()),
                ),
                                                    Uuid::now_v7());

        // Each node adds 1 to the inbound item (all nodes only have one dependency per index)
        let f1 = |_: &ExecutionState, args: RkyvSerializedValue, _, _| {
            async move {
                if let RSV::Object(m) = args {
                    if let RSV::Object(args) = m.get("args").unwrap() {
                        if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                            return Ok(OperationFnOutput::with_value(RSV::Number(a + 1)));
                        }
                    }
                }

                panic!("Invalid arguments")
            }.boxed()
        };

        let (id_b, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    Uuid::now_v7());

        let (id_c, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    Uuid::now_v7());

        let (id_d, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    Uuid::now_v7());

        let (id_e, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    Uuid::now_v7());

        let (id_f, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["1"]),
                    OutputSignature::new(),
                    Box::new(f1),
                ),
                                                    Uuid::now_v7());

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![
                    (id_a, DependencyReference::Positional(0)),
                    (id_d, DependencyReference::Positional(0)),
                ],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_d,
                depends_on: vec![(id_e, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_e,
                depends_on: vec![(id_c, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_f,
                depends_on: vec![(id_e, DependencyReference::Positional(0))],
            },
        ]);

        // We expect to see the value at each node increment repeatedly.
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        assert_eq!(state.state_get_value(&id_b), None);
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;

        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_c), None);
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), None);
        assert_eq!(state.state_get_value(&id_f), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), None);
        assert_eq!(state.state_get_value(&id_f), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));
        assert_eq!(state.state_get_value(&id_f), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(5))));
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(5))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(5))));
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(6))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(5))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(5))));
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(6))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(7))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(5))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(5))));
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        if let ExecutionStateEvaluation::Complete(s) = &state {
            s.render_dependency_graph();
        }
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(6))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(7))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(5))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(8))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(5))));
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(6))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(7))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(5))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(8))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(9))));
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(6))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(7))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(9))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(8))));
        assert_eq!(state.state_get_value(&id_f), Some(&Ok(RSV::Number(9))));
        Ok(())
    }

    #[tokio::test]
    async fn test_branching_multiple_state_paths() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //    |
        //    2

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();

        // We start with the number 1 at node 0
        let (id_a, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(1))) }.boxed()),
                ),
                                                    Uuid::now_v7());

        // Globally mutates this value, making each call to this function side-effecting
        static atomic_usize: AtomicUsize = AtomicUsize::new(0);
        let f_side_effect = |_: &ExecutionState, args: RkyvSerializedValue, _, _| {
            async move {
                if let RSV::Object(m) = args {
                    if let RSV::Object(args) = m.get("args").unwrap() {
                        if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                            let plus = atomic_usize.fetch_add(1, Ordering::SeqCst);
                            return Ok(OperationFnOutput::with_value(RSV::Number(a + plus as i32)));
                        }
                    }
                }

                panic!("Invalid arguments")
            }.boxed()
        };

        let (id_b, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_side_effect),
                ),
                                                    Uuid::now_v7());
        let (id_c, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_side_effect),
                ),
                                                    Uuid::now_v7());

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
        ]);

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        assert_eq!(state.state_get_value(&id_b), None);
        assert_eq!(state.state_get_value(&id_c), None);
        let (x_state_id, x_state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(x_state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(x_state.state_get_value(&id_c), None);

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(x_state.clone()).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));

        // When we re-evaluate from a previous point, we should get a new branch
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(x_state.clone()).await?;
        // The state_id.0 being incremented indicates that we're on a new branch
        // TODO: test some structural indiciation of what branch we're on
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        // Op 2 should re-evaluate to 3, since it's on a new branch but continuing to mutate the stateful counter
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(3))));
        Ok(())
    }

    #[tokio::test]
    async fn test_mutation_of_the_dependency_graph_on_branches() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1 * we're going to be changing the definiton of the function of this node on one branch
        //    |
        //    2

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();

        // We start with the number 0 at node 0
        let (id_a, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::new(),
                    OutputSignature::new(),
                    Box::new(|_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(0))) }.boxed()),
                ),
                                                    Uuid::now_v7());

        let f_v1 = |_: &ExecutionState, args: RkyvSerializedValue, _, _| {
            async move {
                if let RSV::Object(m) = args {
                    if let RSV::Object(args) = m.get("args").unwrap() {
                        if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                            return Ok(OperationFnOutput::with_value(RSV::Number(a + 1)));
                        }
                    }
                }

                panic!("Invalid arguments")
            }.boxed()
        };

        let f_v2 = |_: &ExecutionState, args: RkyvSerializedValue, _, _| {
            async move {
            if let RSV::Object(m) = args {
                if let RSV::Object(args) = m.get("args").unwrap() {
                    if let Some(RSV::Number(a)) = args.get(&"0".to_string()) {
                        return Ok(OperationFnOutput::with_value(RSV::Number(a + 200)));
                    }
                }
            }

            panic!("Invalid arguments")
            }.boxed()
        };

        let (id_b, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_v1),
                ),
                                                    Uuid::now_v7());
        let (id_c, mut state) = state.upsert_operation(OperationNode::new(
                    None,
                    Uuid::nil(),
                    InputSignature::from_args_list(vec!["0"]),
                    OutputSignature::new(),
                    Box::new(f_v1),
                ),
                                                    Uuid::now_v7());

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
        ]);

        let (x_state_id, mut x_state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        assert_eq!(x_state.state_get_value(&id_b), None);
        assert_eq!(x_state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(x_state.clone()).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));

        // Change the definition of the operation "1" to add 200 instead of 1, then re-evaluate
        if let ExecutionStateEvaluation::Complete(x_state) = x_state {
            let (_, mut state) = x_state.upsert_operation(OperationNode::new(
                None,
                Uuid::nil(),
                InputSignature::from_args_list(vec!["0"]),
                OutputSignature::new(),
                Box::new(f_v2),
            ), id_b);
            let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state.clone())).await?;
            assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(200))));
            assert_eq!(state.state_get_value(&id_c), None);
            let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
            assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(200))));
            assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(201))));
        }
        Ok(())

    }

    #[tokio::test]
    async fn test_composition_across_nodes() {
        // TODO: take two operators, and compose them into a single operator
    }

    #[tokio::test]
    async fn test_merging_traversed_state() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3
        //   \ /
        //    4

        let mut state = ExecutionState::new_with_random_id();
        let state_id = Uuid::nil();

        let mut ids = vec![];
        for x in 0..5 {
            let (id, mut nstate) = state.upsert_operation(OperationNode::new(
                None,
                Uuid::nil(),
                if x == 0 { InputSignature::new() } else if x == 4 {InputSignature::from_args_list(vec!["1", "2"]) } else { InputSignature::from_args_list(vec!["1"]) },
                OutputSignature::new(),
                Box::new(move |_, _args, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(x))) }.boxed()),
            ),
                                                         Uuid::now_v7());
            ids.push(id);
            state = nstate
        }
        let id_a = ids[0];
        let id_b = ids[1];
        let id_c = ids[2];
        let id_d = ids[3];
        let id_e = ids[4];

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_d,
                depends_on: vec![(id_c, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_e,
                depends_on: vec![
                    (id_c, DependencyReference::Positional(0)),
                    (id_d, DependencyReference::Positional(1)),
                ],
            },
        ]);

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        assert_eq!(state.state_get_value(&id_b), None);
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), None);
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), None);

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_e), None);

        // This is the final state we're arriving at in execution
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_e), Some(&Ok(RSV::Number(4))));

        // TODO: convert this to an actual test
        // dbg!(db.get_merged_state_history(state_id));
        Ok(())
    }

    #[tokio::test]
    async fn test_get_execution_graph_elements_empty() {
        let db = ExecutionGraph::new();
        let (edges, stack_hierarchy) = db.get_execution_graph_elements();
        assert!(edges.is_empty());
        assert!(stack_hierarchy.is_empty());
    }

    #[tokio::test]
    async fn test_get_execution_graph_elements_single_node() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new_with_random_id();

        let (_, mut state) = state.upsert_operation(OperationNode::new(
            None,
            Uuid::nil(),
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, _, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(1))) }.boxed()),
        ), Uuid::now_v7());
        let init_state_id = state.id.clone();

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;

        let (edges, stack_hierarchy) = db.get_execution_graph_elements();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0], (init_state_id, state_id));
        assert!(stack_hierarchy.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_execution_graph_elements_linear_chain() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new_with_random_id();

        let mut last_state_id = Uuid::nil();
        let mut ids = vec![];
        for i in 0..3 {
            let (id, mut new_state) = state.upsert_operation(OperationNode::new(
                None,
                Uuid::nil(),
                InputSignature::new(),
                OutputSignature::new(),
                Box::new(move |_, _, _, _| async move { Ok(OperationFnOutput::with_value(RSV::Number(i))) }.boxed()),
            ), Uuid::now_v7());
            ids.push(id);
            state = new_state;
        }
        let id_a = ids[0];
        let id_b = ids[1];
        let id_c = ids[2];

        state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: id_b,
                depends_on: vec![(id_a, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: id_c,
                depends_on: vec![(id_b, DependencyReference::Positional(0))],
            },
        ]);
        let init_state_id = state.id.clone();

        let (state_id1, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;
        let (state_id2, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;
        let (state_id3, state, _) = ExecutionGraph::immutable_external_step_execution(state).await?;

        let (edges, stack_hierarchy) = db.get_execution_graph_elements();

        let expected_edges: HashSet<_> = vec![
            (init_state_id, state_id1),
            (state_id1, state_id2),
            (state_id2, state_id3),
        ].into_iter().collect();

        assert_eq!(edges.len(), 3);
        assert_eq!(HashSet::from_iter(edges.into_iter()), expected_edges);
        assert!(stack_hierarchy.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_execution_graph_elements_with_stack() -> anyhow::Result<()> {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new_with_random_id();

        let (_, mut state) = state.upsert_operation(OperationNode::new(
            None,
            Uuid::nil(),
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, _, _, _| async move {
                Ok(OperationFnOutput::with_value(RSV::Number(1)))
            }.boxed()),
        ), Uuid::now_v7());
        let init_state_id = state.id.clone();

        // Simulate a stack by manually setting it in the state
        let stack = VecDeque::from(vec![Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()]);
        state.stack = stack.clone();

        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(ExecutionStateEvaluation::Complete(state)).await?;

        let (edges, stack_hierarchy) = db.get_execution_graph_elements();

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0], (init_state_id, state_id));

        let expected_stack_hierarchy: HashSet<_> = vec![
            stack[0],
            stack[1],
            stack[2],
        ].into_iter().collect();

        assert_eq!(stack_hierarchy.len(), 3);
        assert_eq!(HashSet::from_iter(stack_hierarchy.into_iter()), expected_stack_hierarchy);

        Ok(())
    }
}