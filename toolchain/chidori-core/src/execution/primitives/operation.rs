use crate::execution::primitives::cells::{CellTypes, CodeCell, SupportedLanguage};
use crate::execution::primitives::serialized_value::RkyvSerializedValue;
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};

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

    pub fn validate_input_against_signature(
        &self,
        args: &HashMap<String, RkyvSerializedValue>,
        kwargs: &HashMap<String, RkyvSerializedValue>,
        globals: &HashMap<String, RkyvSerializedValue>,
    ) -> bool {
        // Validate args
        for (key, config) in &self.args {
            if config.default.is_none() && !args.contains_key(key) {
                return false;
            }
        }

        // Validate kwargs
        for (key, config) in &self.kwargs {
            if config.default.is_none() && !kwargs.contains_key(key) {
                return false;
            }
        }

        // Validate globals
        for (key, config) in &self.globals {
            if config.default.is_none() && !globals.contains_key(key) {
                return false;
            }
        }

        true
    }

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
pub type OperationFn = dyn FnMut(RkyvSerializedValue) -> RkyvSerializedValue;

// TODO: rather than dep_count operation node should have a specific dep mapping
pub struct OperationNode {
    pub(crate) id: usize,

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

    /// The height of this node
    pub(crate) height: usize,

    /// Signature of the inputs and outputs of this node
    pub(crate) signature: Signature,

    /// The operation function itself
    operation: Box<OperationFn>,

    /// Dependencies of this node
    pub(crate) arity: usize,
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

// TODO: OperationNode need

impl fmt::Debug for OperationNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationNode")
            .field("id", &self.id)
            .field("purity", &self.purity)
            .field("mutability", &self.mutability)
            .field("changed_at", &self.changed_at)
            .field("verified_at", &self.verified_at)
            .field("dirty", &self.dirty)
            .field("height", &self.height)
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
            cell: CellTypes::Code(CodeCell {
                language: SupportedLanguage::Python,
                source_code: "".to_string(),
                function_invocation: None,
            }),
            purity: Purity::Pure,
            mutability: Mutability::Mutable,
            changed_at: 0,
            verified_at: 0,
            height: 0,
            dirty: true,
            signature: Signature::new(),
            operation: Box::new(|x| x),
            arity: 0,
            unresolved_dependencies: vec![],
            partial_application: Vec::new(),
        }
    }
}

impl OperationNode {
    pub(crate) fn new(
        input_signature: InputSignature,
        output_signature: OutputSignature,
        f: Box<OperationFn>,
    ) -> Self {
        let mut node = OperationNode::default();
        node.signature.input_signature = input_signature;
        node.signature.output_signature = output_signature;
        node.operation = f;
        node
    }

    pub fn attach_cell(&mut self, cell: CellTypes) {
        self.cell = cell;
    }

    fn apply() -> Self {
        unimplemented!();
    }

    pub(crate) fn execute(&mut self, context: RkyvSerializedValue) -> RkyvSerializedValue {
        let exec = self.operation.deref_mut();
        exec(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // TODO: test application of Operations/composition
    // TODO: test manual evaluation of a composition of operations

    #[test]
    fn test_execute_with_operation() {
        let mut executed = false;
        let operation: Box<OperationFn> =
            Box::new(|context| -> RkyvSerializedValue { RkyvSerializedValue::Boolean(true) });

        let mut node = OperationNode::default();
        node.operation = operation;

        let result = node.execute(RkyvSerializedValue::Null);

        assert_eq!(result, RkyvSerializedValue::Boolean(true));
    }

    #[test]
    fn test_execute_without_operation() {
        let mut node = OperationNode::default();
        node.execute(RkyvSerializedValue::Boolean(true)); // should not panic
    }
}
