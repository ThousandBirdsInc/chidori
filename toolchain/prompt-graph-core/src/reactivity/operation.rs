use std::collections::HashMap;
use crate::reactivity::triggerable::{TriggerContext};

pub struct Signature {
    /// Signature of the total inputs for this graph
    input_signature: HashMap<usize, usize>,

    /// Signature of the total outputs for this graph
    output_signature: HashMap<usize, usize>,
}

impl Signature {
    pub(crate) fn new() -> Self {
        Self {
            input_signature: HashMap::new(),
            output_signature: HashMap::new(),
        }
    }
}


enum Purity {
    Pure,
    Impure
}

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
pub type OperationFn = dyn FnMut(&[u8]) -> Vec<u8>;

pub struct OperationNodeDefinition {
    /// The operation function itself
    operation: Option<Box<OperationFn>>,

    /// Dependencies of this node
    pub(crate) dependencies: Vec<usize>,
}

pub struct OperationNode {

    /// Is the node pure or impure, does it have side effects? Does it depend on external state?
    purity: Purity,

    /// Is the node mutable or immutable, can its value change after an execution?
    mutability: Mutability,

    /// Is this node observed by a consumer?
    is_observed: bool,

    /// When did the output of this node last actually change
    changed_at: usize,

    /// When was this operation last brought up to date
    verified_at: usize,

    /// Is this operation dirty
    pub(crate) dirty: bool,

    /// The height of this node
    pub(crate) height: usize,

    /// Signature of the inputs and outputs of this node
    signature: Signature,

    /// The operation function itself
    operation: Option<Box<OperationFn>>,

    /// Dependencies of this node
    pub(crate) dependencies: Vec<usize>,

    /// Partial application arena - this stores partially applied arguments for this OperationNode
    partial_application: Vec<u8>
}

impl Default for OperationNode {
    fn default() -> Self {
        OperationNode {
            purity: Purity::Pure,
            mutability: Mutability::Mutable,
            is_observed: true,
            changed_at: 0,
            verified_at: 0,
            height: 0,
            dirty: false,
            signature: Signature::new(),
            operation: None,
            dependencies: vec![],
            partial_application: Vec::new(),
        }
    }
}

impl OperationNode {
    pub(crate) fn new(f: Option<Box<OperationFn>>) -> Self {
        let mut node = OperationNode::default();
        node.operation = f;
        node
    }

    pub(crate) fn from(d: &OperationNodeDefinition) -> Self {
        let mut node = OperationNode::default();
        node.operation = d.operation.clone();
        node.dependencies = d.dependencies.clone();
        node
    }

    fn apply() -> Self {
        unimplemented!();
    }

    pub(crate) fn execute(&self, context: &[u8]) {
        if let Some(exec) = self.operation.as_ref() {
            exec(context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_with_operation() {
        let mut executed = false;
        let operation: Box<OperationFn> = Box::new(|context: &[u8]| -> Vec<u8> {
            context.to_vec()
        });

        let mut node = OperationNode::default();
        node.operation = Some(operation);

        let bytes = vec![1, 2, 3];
        node.execute(&bytes);

        assert_eq!(executed, true);
    }

    #[test]
    fn test_execute_without_operation() {
        let node = OperationNode::default();

        let bytes = vec![1, 2, 3];
        node.execute(&bytes); // should not panic
    }
}
