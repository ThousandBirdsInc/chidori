/// An Operation is a function, which can be executed on the graph. It
/// can be pure or impure, and it can be mutable or immutable. Each Operation
/// has a unique identifier within a given graph.
use crate::execution::integration::triggerable::TriggerContext;
use std::collections::HashMap;
use std::fmt;

#[derive(PartialEq, Debug)]
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
pub type OperationFn = dyn FnMut(Vec<&Option<Vec<u8>>>) -> Vec<u8>;

pub struct OperationNodeDefinition {
    /// The operation function itself
    pub(crate) operation: Option<Box<OperationFn>>,

    /// Dependencies of this node
    pub(crate) dependency_count: usize,
}

pub struct OperationNode {
    pub(crate) id: usize,

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
    signature: Signature,

    /// The operation function itself
    operation: Option<Box<OperationFn>>,

    /// Dependencies of this node
    pub(crate) arity: usize,
    pub(crate) dependency_count: usize,
    pub(crate) unresolved_dependencies: Vec<usize>,

    /// Partial application arena - this stores partially applied arguments for this OperationNode
    partial_application: Vec<u8>,
}

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
            .field("operation", &self.operation.is_some())
            .field("dependency_count", &self.dependency_count)
            .field("unresolved_dependencies", &self.unresolved_dependencies)
            .field("partial_application", &self.partial_application)
            .finish()
    }
}

impl Default for OperationNode {
    fn default() -> Self {
        OperationNode {
            id: 0,
            purity: Purity::Pure,
            mutability: Mutability::Mutable,
            changed_at: 0,
            verified_at: 0,
            height: 0,
            dirty: true,
            signature: Signature::new(),
            operation: None,
            arity: 0,
            dependency_count: 0,
            unresolved_dependencies: vec![],
            partial_application: Vec::new(),
        }
    }
}

impl OperationNode {
    pub(crate) fn new(args: usize, f: Option<Box<OperationFn>>) -> Self {
        let mut node = OperationNode::default();
        node.operation = f;
        node.dependency_count = args;
        node
    }

    pub(crate) fn from(mut d: OperationNodeDefinition) -> Self {
        let mut node = OperationNode::default();
        node.operation = d.operation.take();
        node
    }

    fn apply() -> Self {
        unimplemented!();
    }

    pub(crate) fn execute(&mut self, context: Vec<&Option<Vec<u8>>>) -> Option<Vec<u8>> {
        if let Some(exec) = self.operation.as_deref_mut() {
            Some(exec(context))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_with_operation() {
        let mut executed = false;
        let operation: Box<OperationFn> =
            Box::new(|context: Vec<&Option<Vec<u8>>>| -> Vec<u8> { vec![0, 1] });

        let mut node = OperationNode::default();
        node.operation = Some(operation);

        let bytes = vec![1, 2, 3];
        node.execute(vec![&Some(bytes)]);

        assert_eq!(executed, true);
    }

    #[test]
    fn test_execute_without_operation() {
        let mut node = OperationNode::default();

        let bytes = vec![1, 2, 3];
        node.execute(vec![&Some(bytes)]); // should not panic
    }

    // TODO: test application of Operations/composition
    // TODO: test manual evaluation of a composition of operations
}
