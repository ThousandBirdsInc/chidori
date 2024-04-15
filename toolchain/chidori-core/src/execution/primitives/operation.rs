use crate::execution::primitives::serialized_value::RkyvSerializedValue;

use log::warn;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use futures_util::FutureExt;
use tracing::{Level, span};
use crate::cells::{CellTypes, CodeCell, SupportedLanguage};
use crate::execution::execution::ExecutionState;


// args, kwargs, locals and their configurations

#[derive(Debug)]
pub enum InputType {
    String,
    Function,
}

#[derive(Debug, Default)]
pub struct InputItemConfiguation {
    // TODO: should represent object and vec types
    pub ty: Option<InputType>,
    pub default: Option<RkyvSerializedValue>,
}

#[derive(Debug)]
pub struct InputSignature {
    pub args: HashMap<String, InputItemConfiguation>,
    pub kwargs: HashMap<String, InputItemConfiguation>,
    pub globals: HashMap<String, InputItemConfiguation>,
}

impl InputSignature {
    pub fn new() -> Self {
        Self {
            args: HashMap::new(),
            kwargs: HashMap::new(),
            globals: HashMap::new(),
        }
    }

    pub fn from_args_list(args: Vec<&str>) -> Self {
        let mut args_map = HashMap::new();
        for (i, arg) in args.iter().enumerate() {
            args_map.insert(format!("{}", i), InputItemConfiguation::default());
        }
        Self {
            args: args_map,
            kwargs: HashMap::new(),
            globals: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.args.is_empty() && self.kwargs.is_empty() && self.globals.is_empty()
    }

    #[tracing::instrument]
    pub fn check_input_against_signature(
        &self,
        args: &HashMap<String, RkyvSerializedValue>,
        kwargs: &HashMap<String, RkyvSerializedValue>,
        globals: &HashMap<String, RkyvSerializedValue>,
        functions: &HashMap<String, RkyvSerializedValue>,
    ) -> bool {
        let mut missing_keys = HashSet::new();

        // Validate args
        for (key, config) in &self.args {
            if config.default.is_none() && !args.contains_key(key) {
                missing_keys.insert(format!("args: {}", key));
            }
        }

        // Validate kwargs
        for (key, config) in &self.kwargs {
            if config.default.is_none() && !kwargs.contains_key(key) {
                missing_keys.insert(format!("kwargs: {}", key));
            }
        }

        // Validate globals
        for (key, config) in &self.globals {
            if config.default.is_none()
                && (!globals.contains_key(key) && !functions.contains_key(key))
            {
                missing_keys.insert(format!("globals or functions: {}", key));
            }
        }

        if !missing_keys.is_empty() {
            println!("Check failed for missing keys: {:?}", missing_keys);
            false
        } else {
            println!("Check passed");
            true
        }
    }

    #[tracing::instrument]
    pub fn prepopulate_defaults(
        &self,
        args: &mut HashMap<String, RkyvSerializedValue>,
        kwargs: &mut HashMap<String, RkyvSerializedValue>,
        globals: &mut HashMap<String, RkyvSerializedValue>,
    ) {
        // Prepopulate args defaults
        for (key, config) in &self.args {
            if let Some(default) = &config.default {
                args.entry(key.clone()).or_insert_with(|| default.clone());
            }
        }

        // Prepopulate kwargs defaults
        for (key, config) in &self.kwargs {
            if let Some(default) = &config.default {
                kwargs.entry(key.clone()).or_insert_with(|| default.clone());
            }
        }

        // Prepopulate globals defaults
        for (key, config) in &self.globals {
            if let Some(default) = &config.default {
                globals
                    .entry(key.clone())
                    .or_insert_with(|| default.clone());
            }
        }
    }
}

#[derive(Debug)]
enum TriggerConfiguration {
    OnChange,
    OnEvent,
    Manual,
}

#[derive(Debug, Default)]
pub struct OutputItemConfiguation {
    // TODO: should represent object and vec types
    pub ty: Option<InputType>,
}

#[derive(Debug)]
pub struct OutputSignatureFunction {
    input_signature: InputSignature,
    emit_event: Vec<String>,
    trigger_on: Vec<String>,
}

#[derive(Debug)]
pub struct OutputSignature {
    pub globals: HashMap<String, OutputItemConfiguation>,
    pub functions: HashMap<String, OutputItemConfiguation>,
}

impl OutputSignature {
    pub fn new() -> Self {
        Self {
            globals: HashMap::new(),
            functions: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct Signature {
    pub trigger_on: TriggerConfiguration,

    /// Signature of the total inputs for this graph
    pub input_signature: InputSignature,

    /// Signature of the total outputs for this graph
    pub output_signature: OutputSignature,
}

impl Signature {
    pub(crate) fn new() -> Self {
        Self {
            trigger_on: TriggerConfiguration::OnChange,
            input_signature: InputSignature {
                args: HashMap::new(),
                kwargs: HashMap::new(),
                globals: HashMap::new(),
            },
            output_signature: OutputSignature {
                globals: HashMap::new(),
                functions: HashMap::new(),
            },
        }
    }
}

#[derive(PartialEq, Debug)]
enum Purity {
    Pure,
    Impure,
}

#[derive(PartialEq, Debug)]
enum Mutability {
    Mutable,
    Immutable,
}



/// This is an object that is passed to OperationNode OperationFn that allows
/// them to expose an interactive internal environment. This is used to provide
/// mutable internal state within the execution of the OperationNode.
///
/// It provides:
///    - oneshot for the node to expose its callable interface to the execution graph without completing execution
///    - receiver for the node to receive messages from the execution graph
///         - the receiver is sent tuples of inputs and a oneshot sender to reply with its output
pub struct AsyncRPCCommunication {
    pub(crate) callable_interface_sender: oneshot::Sender<Vec<String>>,
    pub(crate) receiver: tokio::sync::mpsc::UnboundedReceiver<(String, RkyvSerializedValue, oneshot::Sender<RkyvSerializedValue>)>,
}

impl AsyncRPCCommunication {
    pub(crate) fn new() -> (AsyncRPCCommunication, tokio::sync::mpsc::UnboundedSender<(String, RkyvSerializedValue, tokio::sync::oneshot::Sender<RkyvSerializedValue>)>, oneshot::Receiver<Vec<String>>) {
        let (callable_interface_sender, callable_interface_receiver) = oneshot::channel();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let async_rpc_communication = AsyncRPCCommunication {
            callable_interface_sender,
            receiver,
        };
        (async_rpc_communication, sender, callable_interface_receiver)
    }
}

impl fmt::Debug for AsyncRPCCommunication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncRPCCommunication")
            .finish()
    }
}

/// OperationFn represents functions that can be executed on the graph
/// they accept a byte array and return a new byte vector. This is to allow
/// for the generic operation over any data type across any programming language.
/// Both of these values are serialized and deserialized using zero-copy representations.
///
/// The function is NOT required to be pure. It can have side effects, and it can
/// depend on external state. It is up to the user to ensure that the function is pure
/// if they want to use it in a pure context.
///
/// The function is NOT required to be deterministic. It can return different values
/// for the same input. It is up to the user to ensure that the function is deterministic
/// if they want to use it in a deterministic context.
///
/// The structure of these input and output values should be key-value maps.
/// It is up to the user to structure those maps in such a way that they don't collide with other
/// values being represented in the state of our system. These inputs and outputs are managed
/// by our Execution Database.
pub type OperationFn = dyn Fn(
    &ExecutionState,
    RkyvSerializedValue,
    Option<Sender<((usize, usize), RkyvSerializedValue)>>,
    Option<AsyncRPCCommunication>
) -> Pin<Box<dyn Future<Output = RkyvSerializedValue> + Send>> + Send;

// TODO: rather than dep_count operation node should have a specific dep mapping
pub struct OperationNode {
    pub(crate) id: usize,
    pub(crate) name: Option<String>,

    /// Should this operation run in a background thread
    pub is_long_running_background_thread: bool,

    pub cell: CellTypes,

    /// Is the node pure or impure, does it have side effects? Does it depend on external state?
    purity: Purity,

    /// Is the node mutable or immutable, can its value change after an execution?
    mutability: Mutability,

    /// When did the output of this node last actually change
    changed_at: usize,

    /// When was this operation last brought up to date
    verified_at: usize,

    /// Is this operation dirty
    pub(crate) dirty: bool,

    /// Signature of the inputs and outputs of this node
    pub(crate) signature: Signature,

    /// The operation function itself
    operation: Box<OperationFn>,

    /// Dependencies of this node
    pub(crate) unresolved_dependencies: Vec<usize>,

    /// Partial application arena - this stores partially applied arguments for this OperationNode
    partial_application: Vec<u8>,
}

impl core::hash::Hash for OperationNode {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialEq for OperationNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for OperationNode {}

impl fmt::Debug for OperationNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationNode")
            .field("id", &self.id)
            .field("purity", &self.purity)
            .field("mutability", &self.mutability)
            .field("changed_at", &self.changed_at)
            .field("verified_at", &self.verified_at)
            .field("dirty", &self.dirty)
            .field("signature", &self.signature)
            .field("unresolved_dependencies", &self.unresolved_dependencies)
            .field("partial_application", &self.partial_application)
            .finish()
    }
}

impl Default for OperationNode {
    fn default() -> Self {
        OperationNode {
            id: 0,
            name: None,
            is_long_running_background_thread: false,
            cell: CellTypes::Code(CodeCell {
                name: None,
                language: SupportedLanguage::PyO3,
                source_code: "".to_string(),
                function_invocation: None,
            }),
            purity: Purity::Pure,
            mutability: Mutability::Mutable,
            changed_at: 0,
            verified_at: 0,
            dirty: true,
            signature: Signature::new(),
            operation: Box::new(|_, x, _, _| async move { x }.boxed()),
            unresolved_dependencies: vec![],
            partial_application: Vec::new(),
        }
    }
}

impl OperationNode {
    pub(crate) fn new(
        name: Option<String>,
        input_signature: InputSignature,
        output_signature: OutputSignature,
        f: Box<OperationFn>,
    ) -> Self {
        let mut node = OperationNode::default();
        node.signature.input_signature = input_signature;
        node.signature.output_signature = output_signature;
        node.operation = f;
        node.name = name;
        node
    }

    pub fn attach_cell(&mut self, cell: CellTypes) {
        self.cell = cell;
    }

    fn apply() -> Self {
        unimplemented!();
    }

    #[tracing::instrument]
    pub(crate) fn execute(
        &self,
        state: &ExecutionState,
        argument_payload: RkyvSerializedValue,
        intermediate_output_channel_tx: Option<Sender<((usize, usize), RkyvSerializedValue)>>,
        async_communication_channel: Option<AsyncRPCCommunication>,
    ) -> Pin<Box<dyn Future<Output=RkyvSerializedValue> + Send>> {
        /// Receiver that we pass to the exec for it to capture oneshot RPC communication
        let exec = self.operation.deref();
        exec(state, argument_payload, intermediate_output_channel_tx, async_communication_channel)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use futures_util::FutureExt;
    use super::*;
    // TODO: test application of Operations/composition
    // TODO: test manual evaluation of a composition of operations

    #[tokio::test]
    async fn test_execute_with_operation() {
        let operation: Box<OperationFn> =
            Box::new(|_, context, _, _| { async move { RkyvSerializedValue::Boolean(true) }.boxed() });

        let mut node = OperationNode::default();
        node.operation = operation;

        let result = node.execute(&ExecutionState::new(), RkyvSerializedValue::Null, None, None).await;

        assert_eq!(result, RkyvSerializedValue::Boolean(true));
    }

    #[test]
    fn test_execute_without_operation() {
        let mut node = OperationNode::default();
        node.execute(&ExecutionState::new(), RkyvSerializedValue::Boolean(true), None, None); // should not panic
    }

    /// Demonstrates mutable internal execution of a closure and an RPC interface for interacting with it
    #[tokio::test]
    async fn test_async_communication_rpc() {
        let (async_rpc_communication, rpc_sender, callable_interface_receiver) = AsyncRPCCommunication::new();
        let op = OperationNode {
            id: 0,
            name: None,
            is_long_running_background_thread: false,
            cell: CellTypes::Code(CodeCell {
                name: None,
                language: SupportedLanguage::PyO3,
                source_code: "".to_string(),
                function_invocation: None,
            }),
            purity: Purity::Pure,
            mutability: Mutability::Mutable,
            changed_at: 0,
            verified_at: 0,
            dirty: true,
            signature: Signature::new(),
            operation: Box::new(|_, p: RkyvSerializedValue, _, async_rpccommunication: Option<AsyncRPCCommunication>| async move {
                let mut state = 0;
                let mut async_rpccommunication: AsyncRPCCommunication = async_rpccommunication.unwrap();
                async_rpccommunication.callable_interface_sender.send(vec!["run".to_string()]).unwrap();
                tokio::spawn(async move {
                    loop {
                        if let Ok((key, value, sender)) = async_rpccommunication.receiver.try_recv() {
                            sender.send(RkyvSerializedValue::Number(state)).unwrap();
                            state += 1;
                        } else {
                            tokio::time::sleep(Duration::from_millis(10)).await; // Sleep for 10 milliseconds
                        }
                    }
                }).await;
                RkyvSerializedValue::Null
            }.boxed()),
            unresolved_dependencies: vec![],
            partial_application: Vec::new(),
        };
        let ex = op.execute(&ExecutionState::new(), RkyvSerializedValue::Boolean(true), None, Some(async_rpc_communication));
        let join_handle = tokio::spawn(async move {
            ex.await;
        });
        let callable_interface = callable_interface_receiver.await;
        assert_eq!(callable_interface, Ok(vec!["run".to_string()]));
        let (s, r) = tokio::sync::oneshot::channel();
        rpc_sender.send(("run".to_string(), RkyvSerializedValue::Boolean(true), s)).unwrap();
        let result = r.await.unwrap();
        assert_eq!(result, RkyvSerializedValue::Number(0));
        let (s, r) = tokio::sync::oneshot::channel();
        rpc_sender.send(("run".to_string(), RkyvSerializedValue::Boolean(true), s)).unwrap();
        let result = r.await.unwrap();
        assert_eq!(result, RkyvSerializedValue::Number(1));
        let (s, r) = tokio::sync::oneshot::channel();
        rpc_sender.send(("run".to_string(), RkyvSerializedValue::Boolean(true), s)).unwrap();
        let result = r.await.unwrap();
        assert_eq!(result, RkyvSerializedValue::Number(2));
    }
}
