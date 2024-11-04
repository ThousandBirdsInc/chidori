use std::collections::{HashMap, HashSet, VecDeque};
use crate::execution::execution::execution_state::{CloseReason, DependencyGraphMutation, EnclosedState, ExecutionState};
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
use tracing::debug;
// TODO: update all of these identifies to include a "space" they're within

type EdgeIdentity = (OperationId, OperationId, DependencyReference);

/// ExecutionId is a unique identifier for a point in the execution graph.
pub type ExecutionNodeId = Uuid;

pub type ChronologyId = Uuid;


#[derive(Debug, Clone)]
pub struct MergedStateHistory(pub HashMap<OperationId, (ExecutionNodeId, Arc<OperationFnOutput>)>);

pub type ExecutionGraphSendPayload = (ExecutionState, Option<tokio::sync::oneshot::Sender<()>>);

pub type ExecutionGraphDiGraphSet = DiGraphMap<ExecutionNodeId, ExecutionState>;


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

    execution_state_sender: Sender<ExecutionState>,
    execution_state_receiver: Option<tokio::sync::mpsc::Receiver<ExecutionState>>,

    // TODO: move to just using the digraph for this
    pub(crate) execution_node_id_to_state: Arc<DashMap<ExecutionNodeId, ExecutionState>>,

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


impl ExecutionGraph {
    #[tracing::instrument]
    pub fn new() -> Self {
        debug!("Initializing ExecutionGraph");
        let (sender_new_execution_states, mut receiver_new_execution_states) = tokio::sync::mpsc::channel::<ExecutionGraphSendPayload>(1028);

        let mut state_id_to_state = DashMap::new();

        // Initialization of the execution graph at Uuid::nil - this is always the root of the execution graph
        let init_id = Uuid::nil();
        state_id_to_state.insert(init_id, ExecutionState::new_with_graph_sender(
            init_id,
            Arc::new(sender_new_execution_states)
        ));

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
        let (execution_event_tx, execution_event_rx) = channel::<ExecutionState>(100);
        let execution_event_tx_clone = execution_event_tx.clone();

        // Kick off a background thread that listens for events from async operations
        // These events inject additional state into the execution graph on new branches
        // Those branches will continue to evaluate independently.
        debug!("Initializing background thread for handling async updates to our execution graph");
        let state_id_to_state_clone  = state_id_to_state.clone();
        let handle = tokio::spawn(async move {
            // Signal that the task has started, we can continue initialization
            initialization_notify_clone.notify_one();
            // Pushing this state into the graph
            loop {
                tokio::select! {
                    biased;  // Prioritize message handling over cancellation

                    Some((resulting_execution_state, oneshot)) = receiver_new_execution_states.recv() => {
                        let chronology_id = resulting_execution_state.chronology_id;
                        let parent_id = resulting_execution_state.parent_state_chronology_id;
                        println!("==== Execution Graph received dispatch event {:?}", chronology_id);

                        // Forward this new execution state to the runtime instance
                        if let Err(e) = execution_event_tx_clone.send(resulting_execution_state.clone()).await {
                            debug!("Failed to send execution event: {}", e);
                        }

                        // Pushing this state into the graph
                        state_id_to_state_clone.insert(chronology_id, resulting_execution_state.clone());
                        execution_graph_clone.lock().unwrap().deref_mut().add_edge(
                            parent_id,
                            chronology_id,
                            resulting_execution_state.clone());

                        // Resume execution
                        if let Some(oneshot) = oneshot {
                            oneshot.send(()).expect("Failed to send oneshot completion signal")
                        }
                    }
                    _ = cancellation_notify_clone.notified() => {
                        debug!("Task is notified to stop");
                        return;
                    }
                }
            }
        });
        ExecutionGraph {
            cancellation_notify,
            execution_depth_orchestration_initialized_notify: initialization_notify,
            execution_depth_orchestration_handle: handle,
            execution_node_id_to_state: state_id_to_state,
            execution_graph,
            chat_message_queue: vec![],
            execution_state_sender: execution_event_tx,
            execution_state_receiver: Some(execution_event_rx)
        }
    }

    pub fn take_execution_event_receiver(&mut self) -> tokio::sync::mpsc::Receiver<ExecutionState> {
        self.execution_state_receiver.take().expect("Execution event receiver may only be taken once by a new owner")
    }

    pub async fn shutdown(&mut self) {
        self.cancellation_notify.notify_one();
    }

    #[tracing::instrument]
    pub fn get_execution_graph_elements(&self) -> Vec<(ChronologyId, ChronologyId)>  {
        let execution_graph = self.execution_graph.lock().unwrap();
        execution_graph.deref().all_edges().map(|x| (x.0, x.1)).collect()
    }

    pub fn get_state_at_id(&self, id: ExecutionNodeId) -> Option<ExecutionState> {
        let state_id_to_state = self.execution_node_id_to_state.clone();
        state_id_to_state.get(&id).map(|x| x.clone())
    }

    /// Performs a depth first traversal of the execution graph to resolve the combined
    /// state at a given node.
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
            for (k, v) in state.state.iter() {
                merged_state.insert(*k, (predecessor, v.clone()));
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

    #[deprecated(note="please use `step_execution` directly")]
    pub async fn immutable_external_step_execution(
        state: ExecutionState,
    ) -> anyhow::Result<(
        ExecutionNodeId,
        ExecutionState, // the resulting total state of this step
        Vec<(OperationId, OperationFnOutput)>, // values emitted by operations during this step
    )> {
        println!("step_execution_with_previous_state {:?}", &state);
        let (new_state, outputs) = state.step_execution().await?;
        Ok((state.chronology_id, new_state, outputs))
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
        let (_, new_state, _) = ExecutionGraph::immutable_external_step_execution(state.clone()).await?;
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
        let (state_id, state, _) = ExecutionGraph::immutable_external_step_execution(state)?;
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), None);
        println!("step_execution_with_previous_state {:?}", &state);
        let (new_state, outputs) = state.step_execution().await?;
        let (state_id, state, _) = (state.chronology_id, new_state, outputs);
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), None);
        assert_eq!(state.state_get_value(&id_e), None);

        println!("step_execution_with_previous_state {:?}", &state);
        let (new_state, outputs) = state.step_execution().await?;
        let (state_id, state, _) = (state.chronology_id, new_state, outputs);
        assert_eq!(state.state_get_value(&id_b), Some(&Ok(RSV::Number(1))));
        assert_eq!(state.state_get_value(&id_c), Some(&Ok(RSV::Number(2))));
        assert_eq!(state.state_get_value(&id_d), Some(&Ok(RSV::Number(3))));
        assert_eq!(state.state_get_value(&id_e), None);

        // This is the final state we're arriving at in execution
        println!("step_execution_with_previous_state {:?}", &state);
        let (new_state, outputs) = state.step_execution().await?;
        let (state_id, state, _) = (state.chronology_id, new_state, outputs);
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

        let state1 = ExecutionStateEvaluation::Complete(state);
        println!("step_execution_with_previous_state {:?}", &state1);
        let (new_state, outputs) = state1.step_execution().await?;
        let (state_id, state, _) = (state1.chronology_id, new_state, outputs);

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

        let state1 = ExecutionStateEvaluation::Complete(state);
        println!("step_execution_with_previous_state {:?}", &state1);
        let (new_state, outputs) = state1.step_execution().await?;
        let (state_id1, state, _) = (state1.chronology_id, new_state, outputs);
        println!("step_execution_with_previous_state {:?}", &state);
        let (new_state, outputs) = state.step_execution().await?;
        let (state_id2, state, _) = (state.chronology_id, new_state, outputs);
        println!("step_execution_with_previous_state {:?}", &state);
        let (new_state, outputs) = state.step_execution().await?;
        let (state_id3, state, _) = (state.chronology_id, new_state, outputs);

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

        let state1 = ExecutionStateEvaluation::Complete(state);
        println!("step_execution_with_previous_state {:?}", &state1);
        let (new_state, outputs) = state1.step_execution().await?;
        let (state_id, state, _) = (state1.chronology_id, new_state, outputs);

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