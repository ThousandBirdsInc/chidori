use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::execution::primitives::operation::{InputSignature, OperationFnOutput, OperationNode, OutputItemConfiguration};
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use im::{HashMap as ImHashMap, HashSet as ImHashSet};

use indoc::indoc;
use petgraph::dot::Dot;
use petgraph::graphmap::DiGraphMap;
use petgraph::Direction;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::ops::{Deref};
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
use tokio::sync::oneshot::error::TryRecvError;
use uuid::Uuid;
use crate::cells::{CellTypes, CodeCell, get_cell_name, LLMPromptCell};
use crate::execution::execution::execution_graph::{ExecutionGraphSendPayload, ExecutionNodeId, get_execution_id};

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
    Error,
    Complete(ExecutionState),
    Executing(Arc<FutureExecutionState>)
}

impl ExecutionStateEvaluation {
    pub fn state_get(&self, operation_id: &OperationId) -> Option<&OperationFnOutput> {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => state.state_get(operation_id),
            ExecutionStateEvaluation::Executing(ref future_state) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::Error => unreachable!("Cannot get state from a future state"),
        }
    }

    pub fn state_get_value(&self, operation_id: &OperationId) -> Option<&RkyvSerializedValue> {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => state.state_get(operation_id).map(|o| &o.output),
            ExecutionStateEvaluation::Executing(ref future_state) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::Error => unreachable!("Cannot get state from a future state"),
        }
    }
}

impl Debug for ExecutionStateEvaluation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => f.debug_tuple("Complete").field(state).finish(),
            ExecutionStateEvaluation::Executing(ref future_state) => f.debug_tuple("Executing").field(&format!("Future state evaluating")).finish(),
            ExecutionStateEvaluation::Error => unreachable!("Cannot get state from a future state"),
        }
    }
}




#[derive(Debug, Clone)]
pub struct FunctionMetadata {
    operation_id: usize,
    pub(crate) input_signature: InputSignature,
}


// TODO: make this thread-safe
#[derive(Clone)]
pub struct ExecutionState {
    pub(crate) op_counter: usize,
    pub id: ExecutionNodeId,
    pub parent_state_id: ExecutionNodeId,

    pub chat_message_queue_head: usize,

    pub evaluating_id: usize,
    pub evaluating_name: Option<String>,
    pub evaluating_fn: Option<String>,

    /// CellType applied, by a state that is mutating cell definitions
    pub operation_mutation: Option<CellTypes>,

    /// Channel sender used to update the execution graph and resume execution
    pub graph_sender: Option<Arc<tokio::sync::mpsc::Sender<ExecutionGraphSendPayload>>>,

    /// Queue of operations to evaluate
    pub exec_queue: VecDeque<usize>,

    /// State values that we've marked to resolve as consumed, clearing the state
    pub marked_for_consumption: HashSet<usize>,

    // TODO: call_stack is only ever a single coroutine at a time and instead its the stack of execution states being resolved?
    // pub call_stack: Arc<Mutex<Pin<Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>>>>,

    /// Map of operation_id -> output value of that operation
    pub state: ImHashMap<usize, Arc<OperationFnOutput>>,

    /// Values that were introduced specifically by this state being evaluated, used to identity most recent changes
    pub fresh_values: Vec<usize>,

    /// Map of name of operation -> operation_id
    pub operation_name_to_id: ImHashMap<String, OperationId>,

    /// Map of operation_id -> OperationNode definition
    pub operation_by_id: ImHashMap<OperationId, Arc<Mutex<OperationNode>>>,

    /// This is a mapping of function names to operation ids. Function calls are dispatched to the associated
    /// OperationId that they are initialized by. When a function is invoked, it is dispatched to the operation
    /// node that initialized it where we re-use that OperationNode's runtime in order to invoke the function.
    pub function_name_to_metadata: ImHashMap<String, FunctionMetadata>,

    /// Note what keys have _ever_ been set, which is an optimization to avoid needing to do
    /// a complete historical traversal to verify that a value has been set.
    pub has_been_set: ImHashSet<usize>,

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
    table.push_str(indoc!(r"
        | Key | Value |
        |---|---|"));
    for key in exec_state.state.keys() {
        if let Some(val) = exec_state.state_get(key) {
            table.push_str(&format!(indoc!(r"| {} | {:?} |" ), key, val));
        }
    }
    table.push_str("\n");

    table
}

/// This causes the current async loop to pause until we send a signal over the oneshot sender returned
async fn pause_future_with_oneshot(execution_state_evaluation: ExecutionStateEvaluation, sender: &tokio::sync::mpsc::Sender<ExecutionGraphSendPayload>) -> Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>> {
    println!("============= should pause =============");
    let (oneshot_sender, mut oneshot_receiver) = tokio::sync::oneshot::channel();
    let future = async move {
        println!("Should be pending oneshot signal");
        loop {
            match oneshot_receiver.try_recv() {
                Ok(_) => {
                    break;
                }
                Err(TryRecvError::Empty) => {
                }
                Err(TryRecvError::Closed) => {
                    // TODO: error instead of just continuing
                    println!("Error during oneshot pause.");
                    break;
                }
            }
            // let recv = oneshot_receiver.await.expect("Failed to receive oneshot signal");
        }
        println!("Continuing from oneshot signal");
        RkyvSerializedValue::Null
    };
    sender.send((execution_state_evaluation, oneshot_sender)).await.expect("Failed to send oneshot signal");
    Box::pin(future)
}

impl Default for ExecutionState {
    fn default() -> Self {
        ExecutionState {
            id: Uuid::new_v4(),
            parent_state_id: Uuid::nil(),
            op_counter: 0,
            evaluating_id: 0,
            evaluating_name: None,
            evaluating_fn: None,
            operation_mutation: None,
            graph_sender: None,
            exec_queue: VecDeque::new(),
            marked_for_consumption: HashSet::new(),
            state: Default::default(),
            fresh_values: vec![],
            operation_name_to_id: Default::default(),
            operation_by_id: Default::default(),
            function_name_to_metadata: Default::default(),
            has_been_set: Default::default(),
            dependency_map: Default::default(),
            execution_event_sender: None,
            chat_message_queue_head: 0,
        }
    }
}

impl ExecutionState {
    pub fn new_with_random_id() -> Self {
        ExecutionState {
            ..Self::default()
        }
    }

    pub fn new_with_graph_sender(parent_state_id: ExecutionNodeId, graph_sender: Arc<tokio::sync::mpsc::Sender<ExecutionGraphSendPayload>>) -> Self {
        ExecutionState {
            id: Uuid::nil(),
            parent_state_id,
            graph_sender: Some(graph_sender),
            ..Self::default()
        }
    }

    pub fn clone_with_new_id(&self) -> Self {
        let mut new = self.clone();
        new.parent_state_id = new.id;
        new.fresh_values = vec![];
        new.id = Uuid::new_v4();
        new
    }

    pub fn have_all_operations_been_set_at_least_once(&self) -> bool {
        return self.has_been_set.len() == self.operation_by_id.len()
    }

    pub fn state_get(&self, operation_id: &OperationId) -> Option<&OperationFnOutput> {
        self.state.get(operation_id).map(|x| x.as_ref())
    }

    pub fn state_get_value(&self, operation_id: &OperationId) -> Option<&RkyvSerializedValue> {
        self.state.get(operation_id).map(|x| x.as_ref()).map(|o| &o.output)
    }

    pub fn check_if_previously_set(&self, operation_id: &OperationId) -> bool {
        self.has_been_set.contains(operation_id)
    }

    #[tracing::instrument]
    pub fn state_consume_marked(&mut self, marked_for_consumption: &HashSet<usize>) {
        for key in marked_for_consumption.iter() {
            self.state.remove(key);
        }
    }

    #[tracing::instrument]
    pub fn state_insert(&mut self, operation_id: OperationId, value: OperationFnOutput) {
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
                &|_, e| String::new(), // Edge attributes, assuming you don't need to modify this
                &|_, n| {
                    // Node attributes
                    if let Some(op) = self.operation_by_id.get(n.1) {
                        let op = op.lock().unwrap();
                        let default = format!("{:?}", n.1);
                        let name = get_cell_name(&op.cell).as_ref().unwrap_or(&default);
                        format!("label=\"{}\"", name) // Assuming get_name() fetches the cell name
                    } else {
                        String::new()
                    }
                }
            )
        );
    }

    #[tracing::instrument]
    pub fn get_dependency_graph_flattened(&self) -> Vec<(OperationId, OperationId, Vec<DependencyReference>)> {
        let edges = self.get_dependency_graph();
        edges.all_edges().map(|x| (x.0, x.1, x.2.clone())).collect()
    }

    #[tracing::instrument]
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

    #[tracing::instrument]
    pub fn update_op(
        &self,
        cell: CellTypes,
        op_id: Option<usize>,
    ) -> anyhow::Result<(ExecutionState, usize)> {
        let mut op = match &cell {
            CellTypes::Code(c, r) => crate::cells::code_cell::code_cell(self.id.clone(), c, r),
            CellTypes::Prompt(c, r) => crate::cells::llm_prompt_cell::llm_prompt_cell(self.id.clone(), c, r),
            CellTypes::Embedding(c, r) => crate::cells::embedding_cell::llm_embedding_cell(self.id.clone(), c, r),
            CellTypes::Web(c, r) => crate::cells::web_cell::web_cell(self.id.clone(), c, r),
            CellTypes::Template(c, r) => crate::cells::template_cell::template_cell(self.id.clone(), c, r),
            CellTypes::Memory(c, r) => crate::cells::memory_cell::memory_cell(self.id.clone(), c, r),
            CellTypes::CodeGen(c, r) => crate::cells::code_gen_cell::code_gen_cell(self.id.clone(), c, r),
        }?;
        op.attach_cell(cell.clone());
        let new_state = self.clone();
        let (op_id, new_state) = new_state.upsert_operation(op, op_id);
        let mutations = Self::assign_dependencies_to_operations(&new_state)?;
        let mut final_state = new_state.apply_dependency_graph_mutations(mutations);
        final_state.operation_mutation = Some(cell);
        Ok((final_state, op_id))
    }

    #[tracing::instrument]
    fn assign_dependencies_to_operations(new_state: &ExecutionState) -> anyhow::Result<Vec<DependencyGraphMutation>> {
        let (available_values, available_functions) = Self::get_possible_dependencies(new_state)?;

        // TODO: we need to report on INVOKED functions - these functions are calls to
        //       functions with the locals assigned in a particular way. But then how do we handle compositions of these?
        //       Well we just need to invoke them in the correct pattern as determined by operations in that context.

        // Anywhere there is a matched value, we create a dependency graph edge
        let mut mutations = vec![];

        // let mut unsatisfied_dependencies = vec![];
        // For each destination cell, we inspect their input signatures and accumulate the
        // mutation operations that we need to apply to the dependency graph.
        for (destination_cell_id, op) in new_state.operation_by_id.iter() {
            // The currently running operation will be locked and will fail this condition, but we're not updating it.
            let Ok(operation) = op.try_lock() else { continue; };
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
        Ok(mutations)
    }

    #[tracing::instrument]
    fn get_possible_dependencies(new_state: &ExecutionState) -> anyhow::Result<(HashMap<String, &OperationId>, HashMap<String, &OperationId>)> {
        let mut available_values = HashMap::new();
        let mut available_functions = HashMap::new();

        // For all reported cells, add their exposed values to the available values
        for (id, op) in new_state.operation_by_id.iter() {
            // The currently running operation will be locked and will fail this condition, but we're not updating it.
            let Ok(op_lock) = op.try_lock() else { continue; };
            let output_signature = &op_lock.signature.output_signature;

            // Store values that are available as globals
            for (key, value) in output_signature.globals.iter() {
                let insert_result = available_values.insert(key.clone(), id);
                if insert_result.is_some() {
                    return Err(anyhow::Error::msg(format!("Naming collision detected for value {} when storing op #{}", key, id)));
                }
            }

            for (key, value) in output_signature.functions.iter() {
                let insert_result = available_functions.insert(key.clone(), id);
                if insert_result.is_some() {
                    return Err(anyhow::Error::msg(format!("Naming collision detected for value {}", key)));
                }
            }
        }
        Ok((available_values, available_functions))
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
        s.update_callable_functions();
        s.exec_queue.push_back(op_id);
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

    #[tracing::instrument]
    fn update_callable_functions(&mut self) {
        for (id, op) in &self.operation_by_id {
            if let Ok(mut op_node) = op.try_lock() {
                for (function_name, function_config) in &op_node.signature.output_signature.functions {
                    self.function_name_to_metadata.insert(function_name.clone(), FunctionMetadata {
                        operation_id: id.clone(),
                        input_signature: if let OutputItemConfiguration::Function{ input_signature, .. } = function_config {
                            input_signature.clone()
                        } else {
                            InputSignature::new()
                        }
                    });
                }
            }

        }
    }

    /// Invoke a function made available by the execution state, this accepts arguments derived in the context
    /// of a parent function's scope. This targets a specific function by name that we've identified a dependence on.
    // TODO: this should create a coroutine that yields with the result of the function invocation
    #[tracing::instrument]
    pub async fn dispatch(&self, function_name: &str, payload: RkyvSerializedValue) -> anyhow::Result<(RkyvSerializedValue, ExecutionState)> {
        println!("Running dispatch {:?}", function_name);

        // Store the invocation payload into an execution state and record this before executing
        let mut state = self.clone_with_new_id();

        let meta = self.function_name_to_metadata.get(function_name).map(|meta| {
            meta
        }).expect("Failed to find named function");

        state.state_insert(usize::MAX, OperationFnOutput {
            execution_state: None,
            output: payload.clone(),
            stdout: vec![],
            stderr: vec![],
        });

        let op = state.operation_by_id.get(&meta.operation_id).unwrap().lock().unwrap();
        let op_name = op.name.clone();
        let cell = &op.cell.clone();
        state.evaluating_fn = Some(function_name.to_string());
        state.evaluating_id = meta.operation_id;
        state.evaluating_name = op_name;

        // modify code cell to indicate execution of the target function
        // reconstruction of the cell
        let clone_function_name = function_name.to_string();
        let mut op = match cell {
            CellTypes::Code(c, r) => {
                let mut c = c.clone();
                c.function_invocation =
                    Some(clone_function_name.to_string());
                crate::cells::code_cell::code_cell(Uuid::nil(), &c, r)?
            }
            CellTypes::Prompt(c, r) => {
                let mut c = c.clone();
                match c {
                    LLMPromptCell::Chat{ref mut function_invocation, ..} => {
                        *function_invocation = true;
                        crate::cells::llm_prompt_cell::llm_prompt_cell(Uuid::nil(), &c, r)?
                    }
                    _ => {
                        crate::cells::llm_prompt_cell::llm_prompt_cell(Uuid::nil(), &c, r)?
                    }
                }
            }
            CellTypes::Embedding(c, r) => {
                crate::cells::embedding_cell::llm_embedding_cell(Uuid::nil(), &c, r)?
            }
            _ => {
                unreachable!("Unsupported cell type");
            }
        };

        let mut before_execution_state = state.clone();
        let mut after_execution_state = state.clone_with_new_id();
        // When we receive a message from the graph_sender, execution of this coroutine will resume.
        if let Some(graph_sender) = self.graph_sender.as_ref() {
            let s = graph_sender.clone();
            let result = pause_future_with_oneshot(ExecutionStateEvaluation::Complete(before_execution_state.clone()), &s).await;
            let recv = result.await;
        }

        // invocation of the operation
        // TODO: the total arg payload here does not include necessary function calls for this cell itself
        let result = op.execute(&before_execution_state, payload, None, None).await?;
        after_execution_state.state_insert(usize::MAX, result.clone());

        // TODO: Add result into a new execution state

        // TODO: capture the value of the output
        if let Some(graph_sender) = self.graph_sender.as_ref() {
            let s = graph_sender.clone();
            let result = pause_future_with_oneshot(ExecutionStateEvaluation::Complete(after_execution_state.clone()), &s).await;
            let recv = result.await;
        }

        // Return the result, to be used in the context of the parent function
        Ok((result.output, after_execution_state))
    }

    // TODO: extend this with an "event", steps can occur as events are flushed based on a previous state we were in
    #[tracing::instrument]
    pub async fn step_execution(
        &self,
        sender: &tokio::sync::mpsc::Sender<ExecutionGraphSendPayload>
    ) -> anyhow::Result<(ExecutionStateEvaluation, Vec<(usize, OperationFnOutput)>)> {
        let previous_state = self;
        let mut new_state = previous_state.clone_with_new_id();
        new_state.operation_mutation = None;
        let mut operation_by_id = previous_state.operation_by_id.clone();
        let dependency_graph = previous_state.get_dependency_graph();
        let mut marked_for_consumption = self.marked_for_consumption.clone();

        let mut outputs = vec![];

        // We handle a sorted queue of operations to evaluate, giving a deterministic order
        // to how our operations and run, and executing only a single operation each tick.
        // We churn through the queue in this ordering until we have a valid node to evaluate.
        let mut exec_queue = self.exec_queue.clone();
        let (
            op_node,
            next_operation_id,
            args,
            kwargs,
            globals,
            functions
        ) = 'traverse_nodes: loop {
            println!("Looping step execution, traverse nodes {:?}", &exec_queue);

            let next_operation_id = if let Some(next_operation_id) = exec_queue.pop_front() {
                next_operation_id
            } else {
                // if all operations have been evaluated during this step_execution and none progressed
                // to execution, consume all marked values, complete the execution state with empty output.
                new_state.state_consume_marked(&marked_for_consumption);
                let mut operation_ids: Vec<OperationId> = operation_by_id.keys().copied().collect();
                operation_ids.sort();
                exec_queue.extend(operation_ids.iter());
                new_state.exec_queue = exec_queue;
                // continue;
                // TODO: don't return
                return Ok((ExecutionStateEvaluation::Complete(new_state), outputs));
            };
            println!("Looping step execution, traverse nodes {:?}", next_operation_id);

            // We skip nodes that are currently locked due to long running execution
            // TODO: we can regenerate async nodes if necessary by creating them from their original cells
            let mut op_node = operation_by_id
                .get_mut(&next_operation_id)
                .unwrap()
                .lock()
                .unwrap();

            println!("============================================================");
            println!("Evaluating operation {}: {:?}", next_operation_id, op_node.name);
            // TODO: add to the execution state the id and name of the executed operation (and function name)

            let signature = &op_node.signature.input_signature;

            let mut args = HashMap::new();
            let mut kwargs = HashMap::new();
            let mut globals = HashMap::new();
            let mut functions = HashMap::new();
            signature.prepopulate_defaults(&mut args, &mut kwargs, &mut globals);

            // TODO: state should contain an event queue as well as the stateful globals

            // Ops with 0 deps should only execute once, by do execute by default
            if signature.is_empty() {
                if previous_state.check_if_previously_set(&next_operation_id) {
                    // TODO: don't continue if we've visited the whole set
                    continue 'traverse_nodes;
                }
            }

            // Fetch the values from the previous execution cycle for each edge on this node
            for (from, _to, argument_indices) in
            dependency_graph.edges_directed(next_operation_id, Direction::Incoming)
            {
                println!("Argument indices: {:?}", argument_indices);
                // TODO: we don't need a value from previous state for function invocation dependencies
                if let Some(output) = previous_state.state_get(&from) {
                    let output_value = &output.output;
                    marked_for_consumption.insert(from.clone());

                    // TODO: we can implement prioritization between different values here
                    for argument_index in argument_indices {
                        match argument_index {
                            DependencyReference::Positional(pos) => {
                                args.insert(format!("{}", pos), output_value.clone());
                            }
                            DependencyReference::Keyword(kw) => {
                                kwargs.insert(kw.clone(), output_value.clone());
                            }
                            DependencyReference::Global(name) => {
                                if let RkyvSerializedValue::Object(value) = &output.output {
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

            break (
                op_node,
                next_operation_id,
                args,
                kwargs,
                globals,
                functions
            );
        };
        new_state.exec_queue = exec_queue;

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
        println!("Executing node {} ({:?}) with payload {:?}", next_operation_id, op_node.name, argument_payload);
        new_state.evaluating_fn = None;
        new_state.evaluating_id = next_operation_id;
        new_state.evaluating_name = op_node.name.clone();
        let op_node_execute = op_node.execute(&new_state, argument_payload, None, None);
        if op_node.is_long_running_background_thread {
            let sender_clone = sender.clone();
            let (oneshot_sender, oneshot_receiver) = tokio::sync::oneshot::channel();

            // Run the target long running function in a background thread
            tokio::spawn(async move {
                // This is another thread that handles annotating these events with additional metadata (operationId)
                // let (internal_sender, internal_receiver) = mpsc::channel();
                // std::thread::spawn(move || {
                //     loop {
                //         match internal_receiver.try_recv() {
                //             Ok((execution_state, continue_oneshot)) => {
                //             // Ok((prev_execution_id, value)) => {
                //                 // sender_clone.send((prev_execution_id, next_operation_id, value)).unwrap();
                //             },
                //             Err(mpsc::TryRecvError::Empty) => {
                //                 // No messages available, take this time to sleep a bit
                //                 std::thread::sleep(Duration::from_millis(10)); // Sleep for 10 milliseconds
                //             },
                //             Err(mpsc::TryRecvError::Disconnected) => {
                //                 // Handle the case where the sender has disconnected and no more messages will be received
                //                 break; // or handle it according to your application logic
                //             },
                //         }
                //     }
                // });

                // Long-running execution
                dbg!("Long-running execution");
                let _ = op_node_execute.await;
                dbg!("Completed");
                let _ = oneshot_sender.send(());
            });
            oneshot_receiver.await.expect("Failed to receive oneshot signal");
            // outputs.push((operation_id, result.clone()));
            // new_state.state_insert(operation_id, result);
        } else {
            let result = op_node_execute.await?;
            println!("Executed node {} with result {:?}", next_operation_id, &result);
            outputs.push((next_operation_id, result.clone()));

            // TODO: support overriding execution state entirely
            if let Some(s) = &result.execution_state {
                new_state = s.clone();
            }
            new_state.state_insert(next_operation_id, result);
        }
        new_state.marked_for_consumption = marked_for_consumption;
        Ok((ExecutionStateEvaluation::Complete(new_state), outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_insert_and_get_value() {
        let mut exec_state = ExecutionState::new_with_random_id();
        let operation_id = 1;
        let value = RkyvSerializedValue::Number(1);
        let value = OperationFnOutput {
            execution_state: None,
            output: value,
            stdout: vec![],
            stderr: vec![],
        };
        exec_state.state_insert(operation_id, value.clone());

        assert_eq!(exec_state.state_get_value(&operation_id).unwrap(), &value.output);
        assert!(exec_state.check_if_previously_set(&operation_id));
    }

    #[test]
    fn test_dependency_graph_mutation() {
        let mut exec_state = ExecutionState::new_with_random_id();
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
        let mut exec_state = ExecutionState::new_with_random_id();
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
        let mut exec_state = ExecutionState::new_with_random_id();
        let operation_id = 1;
        let depends_on = vec![(2, DependencyReference::Positional(0))];
        let create_mutation = DependencyGraphMutation::Create {
            operation_id,
            depends_on,
        };
        exec_state = exec_state.apply_dependency_graph_mutations(vec![create_mutation]);
    }
}

