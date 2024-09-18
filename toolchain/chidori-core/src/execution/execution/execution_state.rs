use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::execution::primitives::operation::{InputSignature, OperationFnOutput, OperationNode, OutputItemConfiguration};
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use im::{HashMap as ImHashMap, HashSet as ImHashSet};

use indexmap::set::IndexSet;
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
use no_deadlocks::{Mutex, MutexGuard};
use std::sync::mpsc::Sender;
use std::time::Duration;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeStruct};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use anyhow::Error;
use tokio::sync::oneshot;
use futures_util::FutureExt;
use log::info;
use sha1::digest::typenum::op;
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

#[derive(thiserror::Error, Debug, PartialOrd, PartialEq, Clone, Serialize)]
pub enum ExecutionStateErrors {
    #[error("the execution of this graph has reached a fixed point and will not continue without outside influence")]
    NoFurtherExecutionDetected,
    #[error("an unexpected error has occurred during the evaluation of state {0}")]
    CellExecutionUnexpectedFailure(ExecutionNodeId, String),
    #[error("unknown execution state error: {0}")]
    Unknown(String),
    #[error("Anyhow Error: {0}")]
    AnyhowError(String),
}

impl From<anyhow::Error> for ExecutionStateErrors {
    fn from(err: anyhow::Error) -> Self {
        ExecutionStateErrors::AnyhowError(err.to_string())
    }
}

#[derive(Clone)]
pub enum ExecutionStateEvaluation {
    /// An exception was thrown
    Error(ExecutionState),
    /// An eval function indicated that we should return
    EvalFailure(ExecutionNodeId),
    /// Execution complete
    Complete(ExecutionState),
    /// Execution in progress
    Executing(ExecutionState)
}

impl ExecutionStateEvaluation {
    pub fn state_get(&self, operation_id: &OperationId) -> Option<&OperationFnOutput> {
        match self {

            ExecutionStateEvaluation::Complete(ref state) => state.state_get(operation_id),
            ExecutionStateEvaluation::Executing(..) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::Error(_) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),
        }
    }

    pub fn state_get_value(&self, operation_id: &OperationId) -> Option<&Result<RkyvSerializedValue, ExecutionStateErrors>> {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => state.state_get(operation_id).map(|o| &o.output),
            ExecutionStateEvaluation::Executing(..) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::Error(_) => unreachable!("Cannot get state from a future state"),
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),
        }
    }
}

impl Debug for ExecutionStateEvaluation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionStateEvaluation::Complete(ref state) => f.debug_tuple("Complete").field(state).finish(),
            ExecutionStateEvaluation::Executing(ref state) => f.debug_tuple("Executing").field(state).finish(),
            ExecutionStateEvaluation::Error(ref state) => f.debug_tuple("Error").field(state).finish(),
            ExecutionStateEvaluation::EvalFailure(_) => unreachable!("Cannot get state from a future state"),
        }
    }
}




#[derive(Debug, Clone)]
pub struct FunctionMetadata {
    operation_id: OperationId,
    pub(crate) input_signature: InputSignature,
}


// TODO: make this thread-safe
#[derive(Clone)]
pub struct ExecutionState {
    pub(crate) op_counter: usize,
    pub id: ExecutionNodeId,
    pub stack: VecDeque<ExecutionNodeId>,
    pub parent_state_id: ExecutionNodeId,

    pub external_event_queue_head: usize,

    pub evaluating_id: OperationId,
    pub evaluating_name: Option<String>,
    pub evaluating_fn: Option<String>,
    pub evaluating_arguments: Option<RkyvSerializedValue>,

    /// CellType applied, by a state that is mutating cell definitions
    pub operation_mutation: Option<CellTypes>,

    /// Channel sender used to update the execution graph and resume execution
    pub graph_sender: Option<Arc<tokio::sync::mpsc::Sender<ExecutionGraphSendPayload>>>,

    /// Queue of operations to evaluate
    pub exec_queue: VecDeque<OperationId>,

    // TODO: call_stack is only ever a single coroutine at a time and instead its the stack of execution states being resolved?
    // pub call_stack: Arc<Mutex<Pin<Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>>>>,

    /// Map of operation_id -> output value of that operation
    pub state: ImHashMap<OperationId, Arc<OperationFnOutput>>,

    /// Values that were introduced specifically by this state being evaluated, used to identity most recent changes
    pub fresh_values: IndexSet<OperationId>,

    /// Map of name of operation -> operation_id
    pub operation_name_to_id: ImHashMap<String, OperationId>,

    /// Map of operation_id -> OperationNode definition
    pub operation_by_id: ImHashMap<OperationId, Arc<Mutex<OperationNode>>>,

    /// Map of operation_id -> Cell definition
    pub cells_by_id: ImHashMap<OperationId, CellTypes>,

    /// This is a mapping of function names to operation ids. Function calls are dispatched to the associated
    /// OperationId that they are initialized by. When a function is invoked, it is dispatched to the operation
    /// node that initialized it where we re-use that OperationNode's runtime in order to invoke the function.
    pub function_name_to_metadata: ImHashMap<String, FunctionMetadata>,

    /// Note what keys have _ever_ been set, which is an optimization to avoid needing to do
    /// a complete historical traversal to verify that a value has been set.
    pub has_been_set: ImHashSet<OperationId>,

    /// Dependency graph of the computable elements in the graph
    ///
    /// The dependency graph is a directed graph where the nodes are the ids of the operations and the
    /// weights are the index of the input of the next operation. This is represented as the Key
    /// is the Operation that is consuming from the Operations indicated by the Value.
    ///
    /// The usize::MAX index is a no-op that indicates that the operation is ready to run, an execution
    /// order dependency rather than a value dependency.
    dependency_map: ImHashMap<OperationId, IndexSet<(OperationId, DependencyReference)>>,

    value_freshness_map: ImHashMap<OperationId, usize>,

    execution_event_sender: Option<mpsc::Sender<OperationExecutionStatus>>,
    pub exec_counter: usize,
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
            table.push_str("\n");
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
        println!("============= should resume =============");
        RkyvSerializedValue::Null
    };
    sender.send((execution_state_evaluation, Some(oneshot_sender))).await.expect("Failed to send oneshot signal to the graph receiver");
    Box::pin(future)
}

impl Default for ExecutionState {
    fn default() -> Self {
        ExecutionState {
            exec_counter: 0,
            id: Uuid::new_v4(),
            stack: Default::default(),
            parent_state_id: Uuid::nil(),
            op_counter: 0,
            evaluating_id: Uuid::nil(),
            evaluating_name: None,
            evaluating_fn: None,
            evaluating_arguments: None,
            operation_mutation: None,
            graph_sender: None,
            exec_queue: VecDeque::new(),
            state: Default::default(),
            fresh_values: Default::default(),
            operation_name_to_id: Default::default(),
            operation_by_id: Default::default(),
            cells_by_id: Default::default(),
            function_name_to_metadata: Default::default(),
            has_been_set: Default::default(),
            dependency_map: Default::default(),
            value_freshness_map: Default::default(),
            execution_event_sender: None,
            external_event_queue_head: 0,
        }
    }
}

// New struct to encapsulate operation inputs
#[derive(Debug, Clone)]
pub struct OperationInputs {
    pub(crate) args: HashMap<String, RkyvSerializedValue>,
    pub(crate) kwargs: HashMap<String, RkyvSerializedValue>,
    pub(crate) globals: HashMap<String, RkyvSerializedValue>,
    pub(crate) functions: HashMap<String, RkyvSerializedValue>,
}

impl OperationInputs {
    fn new() -> Self {
        Self {
            args: HashMap::new(),
            kwargs: HashMap::new(),
            globals: HashMap::new(),
            functions: HashMap::new(),
        }
    }

    fn to_serialized_value(&self) -> RkyvSerializedValue {
        RkyvSerializedValue::Object(HashMap::from_iter(vec![
            ("args".to_string(), RkyvSerializedValue::Object(self.args.clone())),
            ("kwargs".to_string(), RkyvSerializedValue::Object(self.kwargs.clone())),
            ("globals".to_string(), RkyvSerializedValue::Object(self.globals.clone())),
            ("functions".to_string(), RkyvSerializedValue::Object(self.functions.clone())),
        ]))
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
        new.fresh_values = IndexSet::new();
        new.id = Uuid::new_v4();
        new.exec_counter += 1;
        new
    }

    pub fn have_all_operations_been_set_at_least_once(&self) -> bool {
        return self.has_been_set.len() == self.operation_by_id.len()
    }

    pub fn state_get(&self, operation_id: &OperationId) -> Option<&OperationFnOutput> {
        self.state.get(operation_id).map(|x| x.as_ref())
    }

    pub fn state_get_value(&self, operation_id: &OperationId) -> Option<&Result<RkyvSerializedValue, ExecutionStateErrors>> {
        self.state.get(operation_id).map(|x| x.as_ref()).map(|o| &o.output)
    }

    pub fn check_if_previously_set(&self, operation_id: &OperationId) -> bool {
        self.has_been_set.contains(operation_id)
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
        op_id: OperationId,
    ) -> anyhow::Result<(ExecutionState, OperationId)> {
        let mut op = match &cell {
            CellTypes::Code(c, r) => crate::cells::code_cell::code_cell(self.id.clone(), c, r),
            CellTypes::Prompt(c, r) => crate::cells::llm_prompt_cell::llm_prompt_cell(self.id.clone(), c, r),
            CellTypes::Embedding(c, r) => crate::cells::embedding_cell::llm_embedding_cell(self.id.clone(), c, r),
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
    pub fn upsert_operation(&self, mut operation_node: OperationNode, op_id: OperationId) -> (OperationId, Self) {
        let mut s = self.clone();
        operation_node.name.as_ref()
            .and_then(|name| s.operation_name_to_id.get(name).copied())
            .unwrap_or_else(|| {
                let new_id = Uuid::new_v4();
                if let Some(name) = &operation_node.name {
                    s.operation_name_to_id.insert(name.clone(), op_id);
                }
                new_id
            });
        operation_node.id = op_id;
        s.cells_by_id.insert(op_id, operation_node.cell.clone());
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
                    s.value_freshness_map.insert(operation_id, 0);
                    if let Some(e) = s.dependency_map.get_mut(&operation_id) {
                        e.clear();
                        e.extend(depends_on.into_iter());
                    } else {
                        s.dependency_map
                            .entry(operation_id)
                            .or_insert(IndexSet::from_iter(depends_on.into_iter()));
                    }
                }
                DependencyGraphMutation::Delete { operation_id } => {
                    s.value_freshness_map.remove(&operation_id);
                    s.dependency_map.remove(&operation_id) ;
                }
            }
        }
        s
    }

    #[tracing::instrument]
    fn update_callable_functions(&mut self) {
        // Ensure no stale data exists
        self.function_name_to_metadata.clear();

        for (id, op) in &self.operation_by_id {
            let Ok(op_node) = op.try_lock() else { continue };
            self.function_name_to_metadata.extend(
                op_node.signature.output_signature.functions.iter().map(|(name, config)| {
                    let input_signature = match config {
                        OutputItemConfiguration::Function { input_signature, .. } => input_signature.clone(),
                        _ => InputSignature::new(),
                    };

                    (name.clone(), FunctionMetadata {
                        operation_id: id.clone(),
                        input_signature,
                    })
                })
            );
        }
    }

    /// Invoke a function made available by the execution state, this accepts arguments derived in the context
    /// of a parent function's scope. This targets a specific function by name that we've identified a dependence on.
    // TODO: this should create a coroutine that yields with the result of the function invocation
    #[tracing::instrument(parent = parent_span_id.clone(), skip(self, payload))]
    pub async fn dispatch(&self, function_name: &str, payload: RkyvSerializedValue, parent_span_id: Option<tracing::Id>) -> anyhow::Result<(Result<RkyvSerializedValue, ExecutionStateErrors>, ExecutionState)> {
        println!("Running dispatch {:?}", function_name);

        // Store the invocation payload into an execution state and record this before executing
        let mut state = self.clone_with_new_id();
        state.stack.push_back(self.id);

        let meta = self.function_name_to_metadata.get(function_name).map(|meta| {
            meta
        }).expect("Failed to find named function");

        let op_name;
        let cell;
        {
            let op = state.operation_by_id.get(&meta.operation_id).unwrap().lock().unwrap();
            op_name = op.name.clone();
            cell = op.cell.clone();
        }
        state.evaluating_fn = Some(function_name.to_string());
        state.evaluating_id = meta.operation_id;
        state.evaluating_name = op_name;
        state.evaluating_arguments = Some(payload.clone());

        // modify code cell to indicate execution of the target function
        // reconstruction of the cell
        let clone_function_name = function_name.to_string();
        let mut op = match cell {
            CellTypes::Code(c, r) => {
                let mut c = c.clone();
                c.function_invocation =
                    Some(clone_function_name.to_string());
                crate::cells::code_cell::code_cell(Uuid::nil(), &c, &r)?
            }
            CellTypes::Prompt(c, r) => {
                let mut c = c.clone();
                match c {
                    LLMPromptCell::Chat{ref mut function_invocation, ..} => {
                        *function_invocation = true;
                        crate::cells::llm_prompt_cell::llm_prompt_cell(Uuid::nil(), &c, &r)?
                    }
                    _ => {
                        crate::cells::llm_prompt_cell::llm_prompt_cell(Uuid::nil(), &c, &r)?
                    }
                }
            }
            CellTypes::Embedding(c, r) => {
                crate::cells::embedding_cell::llm_embedding_cell(Uuid::nil(), &c, &r)?
            }
            _ => {
                unreachable!("Unsupported cell type");
            }
        };

        let mut before_execution_state = state.clone();
        // When we receive a message from the graph_sender, execution of this coroutine will resume.
        if let Some(graph_sender) = self.graph_sender.as_ref() {
            let s = graph_sender.clone();
            let result = pause_future_with_oneshot(ExecutionStateEvaluation::Executing(before_execution_state.clone()), &s).await;
            let _recv = result.await;
        }

        // invocation of the operation
        // TODO: the total arg payload here does not include necessary function calls for this cell itself
        /// Receiver that we pass to the exec for it to capture oneshot RPC communication
        let exec = op.operation.deref();
        let result = exec(&before_execution_state, payload, None, None).await?;

        // Add result into a new execution state
        let mut after_execution_state = state.clone();
        after_execution_state.stack.pop_back();
        after_execution_state.state_insert(Uuid::nil(), result.clone());
        after_execution_state.fresh_values.insert(Uuid::nil());

        // TODO: if result is an error, return an error instead and then pause

        if let Some(graph_sender) = self.graph_sender.as_ref() {
            let s = graph_sender.clone();
            if result.output.is_err() {
                let result = pause_future_with_oneshot(ExecutionStateEvaluation::Error(after_execution_state.clone()), &s).await;
                let _recv = result.await;
            } else {
                let result = pause_future_with_oneshot(ExecutionStateEvaluation::Complete(after_execution_state.clone()), &s).await;
                let _recv = result.await;
            }
        }

        // Return the result, to be used in the context of the parent function
        Ok((result.output, after_execution_state))
    }

    fn get_operation_node(&self, operation_id: OperationId) -> anyhow::Result<MutexGuard<OperationNode>> {
        self.operation_by_id
            .get(&operation_id)
            .ok_or_else(|| anyhow::anyhow!("Operation not found"))?
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to lock operation node"))
    }

    fn update_state(
        &self,
        new_state: &mut ExecutionState,
        next_operation_id: OperationId,
        result: OperationFnOutput,
    ) {
        if let Some(s) = &result.execution_state {
            *new_state = s.clone();
        }
        new_state.fresh_values.insert(next_operation_id);
        new_state.state_insert(next_operation_id, result);
        new_state.value_freshness_map.insert(next_operation_id, self.exec_counter);
    }

    async fn execute_operation(
        &self,
        new_state: &mut ExecutionState,
        mut op_node: MutexGuard<'_, OperationNode>,
        next_operation_id: OperationId,
        inputs: OperationInputs,
    ) -> anyhow::Result<OperationFnOutput> {
        let argument_payload = inputs.to_serialized_value();

        new_state.evaluating_fn = None;
        new_state.evaluating_id = next_operation_id;
        new_state.evaluating_name = op_node.name.clone();

        op_node.execute(new_state, argument_payload, None, None).await
    }

    fn prepare_operation_inputs(
        &self,
        signature: &InputSignature,
        operation_id: OperationId,
        dependency_graph: DiGraphMap<OperationId, Vec<DependencyReference>>,
    ) -> anyhow::Result<OperationInputs> {
        let mut inputs = OperationInputs::new();

        signature.prepopulate_defaults(&mut inputs);

        for (from, _, argument_indices) in dependency_graph.edges_directed(operation_id, Direction::Incoming) {
            if let Some(output) = self.state_get(&from) {
                let output_value = &output.output;

                for argument_index in argument_indices {
                    match argument_index {
                        DependencyReference::Positional(pos) => {
                            inputs.args.insert(pos.to_string(), output_value.clone().unwrap());
                        }
                        DependencyReference::Keyword(kw) => {
                            inputs.kwargs.insert(kw.clone(), output_value.clone().unwrap());
                        }
                        DependencyReference::Global(name) => {
                            if let RkyvSerializedValue::Object(value) = &output.output.clone().unwrap() {
                                inputs.globals.insert(name.clone(), value.get(name).ok_or_else(|| anyhow::anyhow!("Expected value with name: {:?} to be available", name))?.clone());
                            }
                        }
                        DependencyReference::FunctionInvocation(name) => {
                            let op = self.operation_by_id.get(&from).ok_or_else(|| anyhow::anyhow!("Operation must exist"))?.lock().map_err(|_| anyhow::anyhow!("Failed to lock operation"))?;
                            inputs.functions.insert(name.clone(), RkyvSerializedValue::Cell(op.cell.clone()));
                        }
                        DependencyReference::Ordering => {}
                    }
                }
                ();
            }
        }

        Ok(inputs)
    }


    fn process_next_operation(
        &self,
        exec_queue: &mut VecDeque<OperationId>,
    ) -> anyhow::Result<Option<(Option<String>, OperationId, OperationInputs)>> {
        let next_operation_id = match exec_queue.pop_front() {
            Some(id) => id,
            None => {
                // Continue to loop after refreshing the queue
                return Ok(None);
            }
        };

        let mut op_node = self.get_operation_node(next_operation_id)?;
        let name = op_node.name.clone();
        let signature = &op_node.signature.input_signature;

        // If the signature has no dependencies and the operation has been run once, skip it.
        if signature.is_empty() {
            if self.check_if_previously_set(&next_operation_id) {
                println!("Zero dep operation already set, continuing");
                return Ok(None)
            }
        }


        let dependency_graph = self.get_dependency_graph();

        // If none of the inputs are more fresh than our own operation freshness, skip this node
        if !signature.is_empty() {
            let our_freshness = self.value_freshness_map.get(&next_operation_id).copied().unwrap_or(0);
            let any_more_fresh = dependency_graph
                .edges_directed(next_operation_id, Direction::Incoming)
                .any(|(from, _, _)| {
                    let their_freshness = self.value_freshness_map.get(&from).copied().unwrap_or(0);
                    their_freshness >= our_freshness
                });
            if !any_more_fresh {
                println!("None more fresh, continuing");
                return Ok(None);
            }
        }

        let inputs = self.prepare_operation_inputs(signature, next_operation_id, dependency_graph)?;

        if !signature.check_input_against_signature(&inputs) {
            println!("Signature validation failed, continuing");
            return Ok(None);
        }

        Ok(Some((name, next_operation_id, inputs)))
    }

    fn initialize_new_state(&self) -> ExecutionState {
        let mut new_state = self.clone_with_new_id();
        new_state.operation_mutation = None;
        new_state
    }


    #[tracing::instrument]
    pub fn determine_next_operation(
        &self,
    ) -> anyhow::Result<ExecutionStateEvaluation> {
        let mut exec_queue = self.exec_queue.clone();
        let operation_count = self.operation_by_id.keys().count();
        let mut count_loops = 0;
        loop {
            println!("looping {:?} {:?}", self.exec_queue, count_loops);
            count_loops += 1;
            if count_loops >= operation_count * 2 {
                return Err(Error::msg("Looped through all operations without detecting an execution"));
            }
            match self.process_next_operation(&mut exec_queue) {
                Ok(Some((op_node_name, next_operation_id, inputs))) => {
                    let mut new_state = self.initialize_new_state();
                    new_state.evaluating_fn = None;
                    new_state.evaluating_id = next_operation_id;
                    new_state.evaluating_name = op_node_name;
                    new_state.evaluating_arguments = Some(inputs.to_serialized_value());
                    new_state.exec_queue = exec_queue;
                    return Ok(ExecutionStateEvaluation::Executing(new_state));
                }
                Ok(None) => {
                    // There was no operation to evaluate
                    // If the queue is empty we reload it and continue looping
                    if exec_queue.is_empty() {
                        let mut operation_ids: Vec<OperationId> = self.operation_by_id.keys().copied().collect();
                        operation_ids.sort();
                        exec_queue.extend(operation_ids.iter());
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[tracing::instrument]
    pub async fn step_execution(
        &self,
        eval: ExecutionStateEvaluation,
    ) -> anyhow::Result<(ExecutionStateEvaluation, Vec<(OperationId, OperationFnOutput)>)> {
        let mut outputs = vec![];
        let ExecutionStateEvaluation::Executing(mut new_state) = eval else {
            return Err(Error::msg("attempting to step invalid state"));
        };
        // Send this event to the graph so that we can update in progress execution
        if let Some(graph_sender) = self.graph_sender.as_ref() {
            let s = graph_sender.clone();
            let result = pause_future_with_oneshot(ExecutionStateEvaluation::Executing(new_state.clone()), &s).await;
            let _recv = result.await;
        }
        let op_node = self.get_operation_node(new_state.evaluating_id)?;
        let args = new_state.evaluating_arguments.take().unwrap();
        let next_operation_id = new_state.evaluating_id.clone();
        let result = op_node.execute(&mut new_state, args, None, None).await?;
        outputs.push((next_operation_id, result.clone()));
        self.update_state(&mut new_state, next_operation_id, result);
        return Ok((ExecutionStateEvaluation::Complete(new_state), outputs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::{CellTypes, SupportedLanguage, TextRange};
    use crate::cells::CodeCell;
    use crate::execution::primitives::operation::{InputItemConfiguration, InputType, OutputSignature, Signature, TriggerConfiguration};

    #[test]
    fn test_state_insert_and_get_value() {
        let mut exec_state = ExecutionState::new_with_random_id();
        let operation_id = 1;
        let value = RkyvSerializedValue::Number(1);
        let value = OperationFnOutput {
            has_error: false,
            execution_state: None,
            output: Ok(value),
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
            Some(&IndexSet::from_iter(depends_on.into_iter()))
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

    #[test]
    fn test_clone_with_new_id() {
        let original = ExecutionState::new_with_random_id();
        let cloned = original.clone_with_new_id();

        assert_ne!(original.id, cloned.id);
        assert_eq!(original.id, cloned.parent_state_id);
        assert_eq!(original.exec_counter + 1, cloned.exec_counter);
        assert!(cloned.fresh_values.is_empty());
    }

    #[test]
    fn test_update_op() {
        let state = ExecutionState::new_with_random_id();
        let cell = CellTypes::Code(CodeCell {
            name: Some(String::from("a")),
            language: SupportedLanguage::PyO3,
            source_code: String::from("y = x + 1"),
            function_invocation: None,
        }, Default::default());
        
        let (new_state, op_id) = state.update_op(cell.clone(), None).unwrap();
        
        assert!(new_state.operation_by_id.contains_key(&op_id));
        assert_eq!(new_state.operation_mutation, Some(cell));
    }

    #[test]
    fn test_upsert_operation() {
        let state = ExecutionState::new_with_random_id();
        let op_node = OperationNode::default();
        
        let (op_id, new_state) = state.upsert_operation(op_node, None);
        
        assert!(new_state.operation_by_id.contains_key(&op_id));
        assert_eq!(new_state.op_counter, state.op_counter + 1);
        assert!(new_state.exec_queue.contains(&op_id));
    }

    #[test]
    fn test_apply_dependency_graph_mutations() {
        let state = ExecutionState::new_with_random_id();
        let mutations = vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(2, DependencyReference::Positional(0))],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(3, DependencyReference::Global("x".to_string()))],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![],
            },
        ];
        
        let new_state = state.apply_dependency_graph_mutations(mutations);
        
        assert!(new_state.dependency_map.contains_key(&1));
        assert!(new_state.dependency_map.contains_key(&2));
        assert!(new_state.value_freshness_map.contains_key(&2));
        assert!(new_state.value_freshness_map.contains_key(&3));
    }

    #[tokio::test]
    async fn test_dispatch() {
        let mut state = ExecutionState::new_with_random_id();
        let mut op_node = OperationNode::default();
        op_node.cell = CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: "def test_fn(): return 2".to_string(),
            function_invocation: None,
        }, TextRange::default());

        let (op_id, mut new_state) = state.upsert_operation(op_node, None);
        
        new_state.function_name_to_metadata.insert("test_fn".to_string(), FunctionMetadata {
            operation_id: op_id,
            input_signature: InputSignature::new(),
        });
        
        let payload = RkyvSerializedValue::Null;
        let (result, _) = new_state.dispatch("test_fn", payload, None).await.unwrap();
        
        assert_eq!(result.unwrap(), RkyvSerializedValue::Number(2));
    }

    #[test]
    fn test_get_dependency_graph() {
        let mut state = ExecutionState::new_with_random_id();
        state.dependency_map.insert(1, IndexSet::from_iter(vec![(2, DependencyReference::Positional(0))]));
        state.dependency_map.insert(2, IndexSet::from_iter(vec![(3, DependencyReference::Global("x".to_string()))]));
        
        let graph = state.get_dependency_graph();
        
        assert!(graph.contains_edge(2, 1));
        assert!(graph.contains_edge(3, 2));
    }

    #[test]
    fn test_input_signature_check() {
        let mut exec_state = ExecutionState::new_with_random_id();
        
        // Create an operation with a specific input signature
        let mut op = OperationNode::default();
        op.signature = Signature {
            trigger_on: TriggerConfiguration::OnChange,
            input_signature: InputSignature {
                args: HashMap::from([("0".to_string(), InputItemConfiguration {
                    ty: Some(InputType::String),
                    default: None,
                })]),
                kwargs: HashMap::from([("kwarg1".to_string(), InputItemConfiguration {
                    ty: Some(InputType::String),
                    default: None,
                })]),
                globals: HashMap::from([("global1".to_string(), InputItemConfiguration {
                    ty: Some(InputType::String),
                    default: None,
                })]),
            },
            output_signature: OutputSignature {
                globals: HashMap::new(),
                functions: HashMap::new(),
            },
        };
        
        let (op_id, exec_state) = exec_state.upsert_operation(op, None);
        
        // Prepare inputs
        let mut inputs = OperationInputs::new();
        inputs.args.insert("0".to_string(), RkyvSerializedValue::String("value1".to_string()));
        inputs.kwargs.insert("kwarg1".to_string(), RkyvSerializedValue::Number(42));
        inputs.globals.insert("global1".to_string(), RkyvSerializedValue::Boolean(true));
        
        // Get the operation node
        let op_node = exec_state.get_operation_node(op_id).unwrap();
        
        // Check if the inputs match the signature
        let signature = &op_node.signature.input_signature;
        assert!(signature.check_input_against_signature(&inputs));
        
        // Test with missing required input
        let mut incomplete_inputs = inputs.clone();
        incomplete_inputs.args.clear();
        assert!(!signature.check_input_against_signature(&incomplete_inputs));
        
        // Test with extra input
        let mut extra_inputs = inputs.clone();
        extra_inputs.kwargs.insert("extra_kwarg".to_string(), RkyvSerializedValue::Null);
        assert!(signature.check_input_against_signature(&extra_inputs));
    }
}

