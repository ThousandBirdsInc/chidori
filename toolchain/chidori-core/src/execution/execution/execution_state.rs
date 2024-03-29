use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::execution::primitives::operation::OperationNode;
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use im::{HashMap as ImHashMap, HashSet as ImHashSet};

use indoc::indoc;
use petgraph::dot::Dot;
use petgraph::graphmap::DiGraphMap;
use petgraph::Direction;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::{Arc, mpsc};
// use std::sync::{Mutex};
use no_deadlocks::Mutex;
use std::sync::mpsc::Sender;
use std::time::Duration;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use futures_util::FutureExt;
use tokio::runtime::Runtime;
use crate::cells::{CellTypes, CodeCell};
use crate::execution::execution::execution_graph::ExecutionNodeId;

pub enum OperationExecutionStatusOption {
    Running,
    LongRunning,
    Completed,
    Error,
}

pub enum OperationExecutionStatus {
    ExecutionEvent(ExecutionNodeId, OperationId, OperationExecutionStatusOption),
}

#[derive(Debug)]
pub enum DependencyGraphMutation {
    Create {
        operation_id: OperationId,
        depends_on: Vec<(OperationId, DependencyReference)>,
    },
    Delete {
        operation_id: OperationId,
    },
}

pub struct FutureExecutionState {
    receiver: Option<oneshot::Receiver<ExecutionState>>,
}

impl Future for FutureExecutionState {
    type Output = Option<ExecutionState>;

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

#[derive(Clone)]
pub enum ExecutionStateEvaluation {
    Complete(ExecutionState),
    Executing(Arc<FutureExecutionState>)
}

impl ExecutionStateEvaluation {
    pub fn state_get(&self, operation_id: &OperationId) -> Option<&RkyvSerializedValue> {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => state.state_get(operation_id),
            ExecutionStateEvaluation::Executing(ref future_state) => unreachable!("Cannot get state from a future state"),
        }
    }
}

impl Debug for ExecutionStateEvaluation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => f.debug_tuple("Complete").field(state).finish(),
            ExecutionStateEvaluation::Executing(ref future_state) => f.debug_tuple("Executing").field(&format!("Future state evaluating")).finish(),
        }
    }
}

// TODO: make this thread-safe
#[derive(Clone)]
pub struct ExecutionState {
    // TODO: update all operations to use this id instead of a separate representation
    id: (usize, usize),
    pub(crate) op_counter: usize,

    pub state: ImHashMap<usize, Arc<RkyvSerializedValue>>,

    pub operation_name_to_id: ImHashMap<String, OperationId>,

    pub operation_by_id: ImHashMap<OperationId, Arc<Mutex<OperationNode>>>,

    /// This is a mapping of function names to operation ids. Function calls are dispatched to the associated
    /// OperationId that they are initialized by. When a function is invoked, it is dispatched to the operation
    /// node that initialized it where we re-use that OperationNode's runtime in order to invoke the function.
    pub function_name_to_operation_id: ImHashMap<String, OperationId>,

    /// Note what keys have _ever_ been set, which is an optimization to avoid needing to do
    /// a complete historical traversal to verify that a value has been set.
    has_been_set: ImHashSet<usize>,

    /// Dependency graph of the computable elements in the graph
    ///
    /// The dependency graph is a directed graph where the nodes are the ids of the operations and the
    /// weights are the index of the input of the next operation.
    ///
    /// The usize::MAX index is a no-op that indicates that the operation is ready to run, an execution
    /// order dependency rather than a value dependency.
    dependency_map: ImHashMap<OperationId, HashSet<(OperationId, DependencyReference)>>,

    execution_event_sender: Option<mpsc::Sender<OperationExecutionStatus>>,
}

impl std::fmt::Debug for ExecutionState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&render_map_as_table(self))
    }
}

fn render_map_as_table(exec_state: &ExecutionState) -> String {
    let mut table = String::from("\n --- state ----");
    table.push_str(indoc!(
        r"
            | Key | Value |
            |---|---|"
    ));
    for key in exec_state.state.keys() {
        if let Some(val) = exec_state.state_get(key) {
            table.push_str(&format!(
                indoc!(
                    r"
                | {} | {:?} |"
                ),
                key, val,
            ));
        }
    }
    table.push_str("\n");
    // table.push_str("\n ---- operations ---- ");
    // table.push_str(indoc!(
    //     r"
    //         | Key | Value |
    //         |---|---|"
    // ));
    // for key in exec_state.operation_by_id.keys() {
    //     if let Some(val) = exec_state.operation_by_id.get(key) {
    //         table.push_str(&format!(
    //             indoc!(
    //                 r"
    //             | {} | {:?} |"
    //             ),
    //             key, val.lock().as_deref(),
    //         ));
    //     }
    // }
    // table.push_str("\n");

    table
}

impl ExecutionState {
    pub fn new() -> Self {
        ExecutionState {
            id: (0, 0),
            op_counter: 0,
            state: Default::default(),
            operation_name_to_id: Default::default(),
            operation_by_id: Default::default(),
            function_name_to_operation_id: Default::default(),
            has_been_set: Default::default(),
            dependency_map: Default::default(),
            execution_event_sender: None,
        }
    }

    pub fn state_get(&self, operation_id: &OperationId) -> Option<&RkyvSerializedValue> {
        self.state.get(operation_id).map(|x| x.as_ref())
    }

    pub fn check_if_previously_set(&self, operation_id: &OperationId) -> bool {
        self.has_been_set.contains(operation_id)
    }

    #[tracing::instrument]
    pub fn state_consume_marked(&mut self, marked_for_consumption: HashSet<usize>) {
        for key in marked_for_consumption.clone().into_iter() {
            self.state.remove(&key);
        }
    }

    #[tracing::instrument]
    pub fn state_insert(&mut self, operation_id: OperationId, value: RkyvSerializedValue) {
        self.state.insert(operation_id, Arc::new(value));
        self.has_been_set.insert(operation_id);
    }

    pub fn render_dependency_graph(&self) {
        println!("================ Dependency graph ================");
        println!(
            "{:?}",
            Dot::with_attr_getters(
                &self.get_dependency_graph(),
                &[],
                &|_, _| String::new(),
                &|_, _| String::new()
            )
        );
    }

    pub fn get_dependency_graph_flattened(&self) -> Vec<(OperationId, OperationId, Vec<DependencyReference>)> {
        let edges = self.get_dependency_graph();
        edges.all_edges().map(|x| (x.0, x.1, x.2.clone())).collect()
    }

    pub fn get_dependency_graph(&self) -> DiGraphMap<OperationId, Vec<DependencyReference>> {
        let mut graph = DiGraphMap::new();
        for (node, value) in self.dependency_map.clone().into_iter() {
            graph.add_node(node);
            for (depends_on, index) in value.into_iter() {
                let r = graph.add_edge(depends_on, node, vec![index]);
                if r.is_some() {
                    graph
                        .edge_weight_mut(depends_on, node)
                        .unwrap()
                        .append(&mut r.unwrap());
                }
            }
        }
        graph
    }

    /// Inserts a new operation into the execution state, returning the operation id and the new state.
    /// That operation can then be referred to by its id.
    #[tracing::instrument]
    pub fn upsert_operation(&self, operation_node: OperationNode, op_id: Option<usize>) -> (usize, Self) {
        let mut s = self.clone();
        let op_id = if let Some(op_id) = op_id {
            op_id
        } else {
            operation_node.name.as_ref()
                .and_then(|name| s.operation_name_to_id.get(name).copied())
                .unwrap_or_else(|| {
                    let new_id = s.op_counter;
                    s.op_counter += 1;
                    if let Some(name) = &operation_node.name {
                        s.operation_name_to_id.insert(name.clone(), new_id);
                    }
                    new_id
                })
        };

        s.operation_by_id.insert(op_id, Arc::new(Mutex::new(operation_node)));
        (op_id, s)
    }

    /// Applies a series of mutations to the dependency graph of cells. This returns a new ExecutionState
    /// with the mutations applied.
    #[tracing::instrument]
    pub fn apply_dependency_graph_mutations(
        &self,
        mutations: Vec<DependencyGraphMutation>,
    ) -> Self {
        let mut s = self.clone();
        for mutation in mutations {
            match mutation {
                DependencyGraphMutation::Create {
                    operation_id,
                    depends_on,
                } => {
                    if let Some(e) = s.dependency_map.get_mut(&operation_id) {
                        e.clear();
                        e.extend(depends_on.into_iter());
                    } else {
                        s.dependency_map
                            .entry(operation_id)
                            .or_insert(HashSet::from_iter(depends_on.into_iter()));
                    }
                }
                DependencyGraphMutation::Delete { operation_id } => {
                    s.dependency_map.remove(&operation_id);
                }
            }
        }
        s
    }

    /// Invoke a function made available by the execution state, this accepts arguments derived in the context
    /// of a parent function's scope. This targets a specific function by name that we've identified a dependence on.
    pub fn dispatch(&self, function_name: &str, payload: RkyvSerializedValue) {
        // Store the invocation payload into an execution state and record this before executing
        let mut state = self.clone();
        state.state_insert(usize::MAX, payload);

        self.function_name_to_operation_id.get(function_name).map(|op_id| {
            let op = state.operation_by_id.get(op_id).unwrap().lock().unwrap();
            op.execute(&state, RkyvSerializedValue::Object(HashMap::new()), None);
        });

        // TODO: return the result, which we will use in the context of the parent function
    }


    // TODO: extend this with an "event", steps can occur as events are flushed based on a previous state we were in
    #[tracing::instrument]
    pub async fn step_execution(
        &self,
        sender: &Sender<(ExecutionNodeId, OperationId, RkyvSerializedValue)>
    ) -> (ExecutionStateEvaluation, Vec<(usize, RkyvSerializedValue)>) {
        let previous_state = self;
        let mut new_state = previous_state.clone();
        let mut operation_by_id = previous_state.operation_by_id.clone();
        let dependency_graph = previous_state.get_dependency_graph();
        let mut marked_for_consumption = HashSet::new();

        let mut outputs = vec![];
        let operation_ids: Vec<OperationId> = operation_by_id.keys().copied().collect();

        // Every step, each operation consumes from its incoming edges.
        'traverse_nodes: for operation_id in operation_ids {

            // We skip nodes that are currently locked due to long running execution
            // TODO: we can regenerate async nodes if necessary by creating them from their original cells
            let mut op_node = operation_by_id
                .get_mut(&operation_id)
                .unwrap()
                .lock()
                .unwrap();

            println!("============================================================");
            println!("Evaluating operation {}: {:?}", operation_id, op_node.name);

            let signature = &op_node.signature.input_signature;

            let mut args = HashMap::new();
            let mut kwargs = HashMap::new();
            let mut globals = HashMap::new();
            let mut functions = HashMap::new();
            signature.prepopulate_defaults(&mut args, &mut kwargs, &mut globals);

            // TODO: state should contain an event queue as well as the stateful globals

            // Ops with 0 deps should only execute once, by do execute by default
            if signature.is_empty() {
                if previous_state.check_if_previously_set(&operation_id) {
                    continue 'traverse_nodes;
                }
            }

            // Fetch the values from the previous execution cycle for each edge on this node
            for (from, _to, argument_indices) in
            dependency_graph.edges_directed(operation_id, Direction::Incoming)
            {
                println!("Argument indices: {:?}", argument_indices);
                // TODO: we don't need a value from previous state for function invocation dependencies
                if let Some(output) = previous_state.state_get(&from) {
                    marked_for_consumption.insert(from.clone());

                    // TODO: we can implement prioritization between different values here
                    for argument_index in argument_indices {
                        match argument_index {
                            DependencyReference::Positional(pos) => {
                                args.insert(format!("{}", pos), output.clone());
                            }
                            DependencyReference::Keyword(kw) => {
                                kwargs.insert(kw.clone(), output.clone());
                            }
                            DependencyReference::Global(name) => {
                                if let RkyvSerializedValue::Object(value) = output {
                                    dbg!(&name);
                                    globals.insert(name.clone(), value.get(name).unwrap().clone());
                                }
                            }
                            DependencyReference::FunctionInvocation(name) => {
                                let op = self
                                    .operation_by_id
                                    .get(&from)
                                    .expect("Operation must exist")
                                    .lock()
                                    .unwrap();
                                functions.insert(
                                    name.clone(),
                                    RkyvSerializedValue::Cell(op.cell.clone()),
                                );
                            }
                            // if the dependency is of Ordering type, then this is an execution order dependency
                            DependencyReference::Ordering => {
                                // TODO: enforce that dependency executes if it has only an ordering dependence
                            }
                        }
                    }
                }
            }

            // Some of the required arguments are not yet available, continue to the next node
            if !signature.check_input_against_signature(&args, &kwargs, &globals, &functions) {
                continue 'traverse_nodes;
            }

            // TODO: all functions that are referred to that we know are not yet defined are populated with a shim,
            //       that shim goes to our lookup based on our function invocation dependencies.

            // Construct the arguments for the given operation
            let argument_payload: RkyvSerializedValue = RkyvSerializedValue::Object(HashMap::from_iter(vec![
                ("args".to_string(), RkyvSerializedValue::Object(args)),
                ("kwargs".to_string(), RkyvSerializedValue::Object(kwargs)),
                ("globals".to_string(), RkyvSerializedValue::Object(globals)),
                (
                    "functions".to_string(),
                    RkyvSerializedValue::Object(functions),
                ),
            ]));

            // Execute the operation
            // TODO: support async/parallel execution
            println!("Executing node {} ({:?}) with payload {:?}", operation_id, op_node.name, argument_payload);
            let op_node_execute = op_node.execute(&self, argument_payload, None);
            if op_node.is_async {
                let sender_clone = sender.clone();
                let state_clone = self.clone();
                let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();

                // Run the target long running function in a background thread
                tokio::spawn(async move {
                    dbg!("Spawning async operation");
                    dbg!("Starting background thread");
                    // This is another thread that handles annotating these events with additional metadata (operationId)
                    let (internal_sender, internal_receiver) = mpsc::channel();
                    std::thread::spawn(move || {
                        loop {
                            match internal_receiver.try_recv() {
                                Ok((prev_execution_id, value)) => {
                                    sender_clone.send((prev_execution_id, operation_id, value)).unwrap();
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
                    });

                    // Long-running execution
                    dbg!("Long-running execution");
                    // TODO: this is deadlocking
                    let _ = op_node_execute.await;
                    dbg!("Completed");
                    let _ = oneshot_sender.send(());
                });
                oneshot_receiver.await.expect("Failed to receive oneshot signal");
                // outputs.push((operation_id, result.clone()));
                // new_state.state_insert(operation_id, result);
            } else {
                let result = op_node_execute.await;
                println!("Executed node {} with result {:?}", operation_id, &result);
                outputs.push((operation_id, result.clone()));
                new_state.state_insert(operation_id, result);

                // Effectively during one state's execution, new intermediate states are also generated, there is a tree of states being created
                // how do we capture this and push it up into the execution graph itself.
                // We could have channels listen to events from all of the states but that doesn't feel right.
                // This is why what I wanted to do was to have the top level state then become a Future, it will eventually be resolved and when it
                // is resolved, execution can progress. But what we want to do is to still mutate the graph from there, so we need to return
                // while the future has been provided (which happens immediately with an async function) then we want call it again for evaluation
                // before we progress its child events.
            }
        }
        new_state.state_consume_marked(marked_for_consumption);

        (ExecutionStateEvaluation::Complete(new_state), outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_insert_and_get_value() {
        let mut exec_state = ExecutionState::new();
        let operation_id = 1;
        let value = RkyvSerializedValue::Number(1);
        exec_state.state_insert(operation_id, value.clone());

        assert_eq!(exec_state.state_get(&operation_id).unwrap(), &value);
        assert!(exec_state.check_if_previously_set(&operation_id));
    }

    #[test]
    fn test_dependency_graph_mutation() {
        let mut exec_state = ExecutionState::new();
        let operation_id = 1;
        let depends_on = vec![(2, DependencyReference::Positional(0))];
        let mutation = DependencyGraphMutation::Create {
            operation_id,
            depends_on: depends_on.clone(),
        };

        exec_state = exec_state.apply_dependency_graph_mutations(vec![mutation]);
        assert_eq!(
            exec_state.dependency_map.get(&operation_id),
            Some(&HashSet::from_iter(depends_on.into_iter()))
        );
    }

    #[test]
    fn test_dependency_graph_deletion() {
        let mut exec_state = ExecutionState::new();
        let operation_id = 1;
        let depends_on = vec![(2, DependencyReference::Positional(0))];
        let create_mutation = DependencyGraphMutation::Create {
            operation_id,
            depends_on,
        };
        exec_state = exec_state.apply_dependency_graph_mutations(vec![create_mutation]);

        let delete_mutation = DependencyGraphMutation::Delete { operation_id };
        exec_state = exec_state.apply_dependency_graph_mutations(vec![delete_mutation]);

        assert!(exec_state.dependency_map.get(&operation_id).is_none());
    }

    // TODO: add a test that demonstrates multiple edges from the same node, filling multiple values

    #[test]
    fn test_async_execution_at_a_state() {
        let mut exec_state = ExecutionState::new();
        let operation_id = 1;
        let depends_on = vec![(2, DependencyReference::Positional(0))];
        let create_mutation = DependencyGraphMutation::Create {
            operation_id,
            depends_on,
        };
        exec_state = exec_state.apply_dependency_graph_mutations(vec![create_mutation]);
    }
}

