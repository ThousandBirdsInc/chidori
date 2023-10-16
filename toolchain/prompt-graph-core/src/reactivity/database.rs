use std::collections::HashMap;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::graphmap::DiGraphMap;
use crate::reactivity::triggerable::{Subscribable, TriggerContext};
use petgraph::visit::{Dfs, VisitMap, Walker};
use std::collections::HashSet;
use petgraph::Direction;
use crate::proto::NodeWillExecute;
use im::OrdMap;
use crate::reactivity::operation::{OperationFn, OperationNode, OperationNodeDefinition, Signature};

// TODO: should we use im or im_rc?


enum Change {
    Update {
        node_id: usize,
        operation_node: OperationNodeDefinition
    },
    Delete {
        node_id: usize,
    }
}

struct ChangeResult {
    // output value of a node
}

// TODO: partial_application can include references to other functions
// TODO: partial_application includes a set of (serialized, structure) pairs

enum ExecutionSpan {
    Started(usize),
    Ended(usize)
}

// TODO: track "observed" nodes, and ignore unobserved paths
// TODO: maintain the revision number element
// TODO: add a "Storage" and "Durability" api
// TODO: should this registry instead model queues for each of the elements?
// TODO: we should know which elements are currently executing and when they complete
// TODO: a durability property on computations which represents if they're likely to change
/// This models the network of reactive relationships between different components.
///
/// This is heavily inspired by works such as Salsa, Verde, Incremental, Adapton, and Differential Dataflow.
/// Most of the ideas here are not new. However, most critically, we are implementing support for
/// runtime changes to the structure of the graph and we allow for cycles in that graph.
pub struct ReactivityDatabase {
    /// The current head in our execution order plan
    execution_head: usize,

    /// The order of node execution currently planned
    execution_order: Vec<usize>,

    /// Operation and its id
    operation_by_id: HashMap<usize, OperationNode>,

    /// Captured outputs of operation execution
    operation_with_output: HashMap<usize, [u8]>,

    /// Dependency graph of the computable elements in the graph
    ///
    /// The dependency graph is a directed graph where the nodes are the ids of the operations and the
    /// weights are the index of the input of the next operation.
    ///
    /// The usize::MAX index is a no-op that indicates that the operation is ready to run, an execution
    /// order dependency rather than a value dependency.
    dependency_graph: DiGraphMap<usize, usize>,

    /// Signature of the total inputs and outputs for this graph
    signature: Signature,

    /// Global revision number
    revision: usize,

    /// Log of start and stop spans for each operation
    execution_log: Vec<ExecutionSpan>
}

// TODO:
// 1) Build a table of dependency relationships between elements
// 2) Build a representation of recently modified for those elements
// 3) Build a representation of topological dependence between elements
// 4) Build a representation of dependency on mutable vs immutable elements
// 5) Build a list of all dependencies for a given element so that it must be fully satisfied in order to run
// 6) Include a queue of changes to be processed

// TODO: all nodes accept an ordered list of values, indices of those values may have aliases,
//       and may be optional/have defaults

// TODO: all reactivity databases have an input node and an output node by default

// TODO: should be able to compose elements that operate on composed streams


impl ReactivityDatabase {
    /// Initialize a new reactivity database. This will create a default input and output node,
    /// graphs default to being the unit function x -> x.
    pub fn new() -> Self {
        let mut dependency_graph = DiGraphMap::new();
        dependency_graph.add_edge(0, 1, 0);

        let mut operation_by_id = HashMap::new();
        operation_by_id.insert(0, OperationNode::new(None));
        operation_by_id.insert(1, OperationNode::new(None));

        ReactivityDatabase {
            execution_head: 0,
            execution_order: vec![0, 1],
            operation_by_id,
            operation_with_output: HashMap::new(),
            dependency_graph,
            signature: Signature::new(),
            revision: 0,
            execution_log: vec![],
        }
    }

    /// This adds an operation into the database and returns a handle to it.
    pub fn add_operation(&mut self, node: usize, func: Box<OperationFn>) -> usize {
        self.operation_by_id.insert(node, OperationNode::new(Some(func)));
        usize
    }

    /// Indicates that this operation depends on the output of the given node
    pub fn add_value_dependency_to_operation(&mut self, node: usize, depends_on: usize, index: usize) {
        self.dependency_graph.add_edge(depends_on, node, index);
    }

    pub fn add_execution_only_dependency_to_operation(&mut self, node: usize, depends_on: usize) {
        self.dependency_graph.add_edge(depends_on, node, usize::MAX);
    }

    fn traverse_and_handle(&mut self, id: usize, visited: &mut HashSet<usize>, stack: &mut Vec<usize>) {
        if visited.contains(&id) {
            return;
        }

        // If a cycle is detected, we set the height of all nodes in the cycle to the height of the node where the cycle was detected.
        // This is done to ensure that all nodes in the cycle have the same execution order.
        // Additionally, we mark all nodes in the cycle as dirty to indicate that their values need to be recomputed.
        if stack.contains(&id) {
            if let Some(op) = self.operation_by_id.get(&id) {
                let cycle_height = op.height.clone();
                for &node_id in stack.iter().rev() {
                    let op_node = self.operation_by_id.get_mut(&node_id).unwrap();
                    op_node.height = cycle_height.clone();
                    op_node.dirty = true;  // Mark node as dirty
                    if node_id == id {
                        break;
                    }
                }
                return;
            }
        }

        stack.push(id.clone());

        // Initialize the maximum height to 0
        let mut max_height = 0;
        // Iterate over the neighbors of the current node in the dependency graph
        for neighbor in self.dependency_graph.neighbors_directed(id.clone(), Direction::Incoming) {
            // Recursively traverse and handle each neighbor
            self.traverse_and_handle(neighbor, visited, stack);
            // If the neighbor operation exists, update the maximum height
            if let Some(op) = self.operation_by_id.get(&neighbor) {
                max_height = max_height.max((op.height).clone());
            }
        }

        let op_node = self.operation_by_id.get_mut(&id).unwrap();
        op_node.height = max_height + 1;

        // Mark node as dirty
        op_node.dirty = true;

        stack.pop();
        visited.insert(id);
    }


    // TODO: values are operations of the unit type that return a value
    pub fn handle_operation_change(&mut self, node_id: usize, incoming_change: Change) {
        // Changes the operation of a cell

        let existing_op_node = match &incoming_change {
            Change::Update{node_id, ..} | Change::Delete{node_id, ..} => {
                self.operation_by_id.get(node_id)
            }
            _ => None
        };

        if let Change::Delete {node_id, ..} = &incoming_change {
            // TODO: tombstone the target node
            // Tombstone the target node
            self.operation_by_id.remove(node_id);
            // Remove edges from the dependency graph
            self.dependency_graph.remove_node(node_id.clone());
        }

        if let Some(existing_op_node) = existing_op_node {
            if let Change::Delete {node_id, ..} = &incoming_change {

            }
            if let Change::Update {node_id, operation_node} = &incoming_change {
                // Overwrite the existing node definition
                self.operation_by_id.insert(node_id.clone(), OperationNode::from(operation_node.clone()));

                // Update dependency graph
                let existing_dependencies = &existing_op_node.dependencies;
                let new_dependencies = &operation_node.dependencies;
                if existing_dependencies != new_dependencies {
                    for neighbor in existing_dependencies {
                        self.dependency_graph.remove_edge(neighbor.clone(), node_id.clone());
                    }
                    for (idx, dependency) in new_dependencies.iter().enumerate() {
                        self.dependency_graph.add_edge(dependency.clone(), node_id.clone(), idx);
                    }
                }
            }
        } else {
            // TODO: existing node does not exist - create the node
            if let Change::Update{node_id, operation_node} = &incoming_change {
                self.operation_by_id.insert(node_id.clone(), OperationNode::from(operation_node.clone()));
                for (idx, dependency) in operation_node.dependencies.iter().enumerate() {
                    self.add_value_dependency_to_operation(node_id.clone(), dependency.clone(), idx);
                }
            }
        }

        // 1. Dirty all operations that depend on the changed operation, updating heights of dependent operations
        let mut visited = HashSet::new();
        self.traverse_and_handle(node_id, &mut visited, &mut Vec::new());

        // 2. Push the dependent nodes onto a queue, we delay execution of the nodes so that we can run that incrementally
        // TODO: execution_order represents the target execution
        // TODO: push a new copy of an execution order onto the queue (updating after the latest execution head location)
        // TODO: only traverse cycles at a consistent rate versus other execution paths
        // TODO: in the editor - identify where cycles exist and set an execution rate (hot and cold paths)
        let mut execution_order: Vec<usize> = self.operation_by_id.keys().cloned().collect();
        execution_order.sort_by_key(|&node_id| {
            self.operation_by_id.get(&node_id).map_or(0, |op_node| op_node.height)
        });

        self.execution_queue.push(execution_order);
    }

    /// Run one step of evaluation
    ///
    /// Evaluation is done in the following steps:
    /// 1. Find the lowest height dirty node
    /// 2. Execute that node
    /// 3. Mark that node as clean
    /// 4. Repeat until there are no more dirty nodes
    pub fn evaluation_step(&mut self) {
        self.execution_head += 1;
        let idx = *self.execution_head;
        let node_id = self.execution_order[idx];
        let op_node = self.operation_by_id.get_mut(&node_id).unwrap();
        self.execution_log.push(ExecutionSpan::Started(node_id));
        op_node.execute(&[]);
        self.execution_log.push(ExecutionSpan::Ended(node_id));
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    struct DummyContext;
    impl TriggerContext for DummyContext {}

    #[test]
    fn test_dirty_dependent_operation_single_dependency() {
        let mut db = ReactivityDatabase::<DummyContext>::new();
        db.add_operation(2, Box::new(|_| {}));
        db.add_value_dependency_to_operation(2, 1, 0);
        db.dirty_dependent_operation(1);

        assert_eq!(db.operation_by_id[&1].dirty, true);
        assert_eq!(db.operation_by_id[&2].dirty, true);
    }

    #[test]
    fn test_dirty_dependent_operation_multiple_dependencies() {
        let mut db = ReactivityDatabase::<DummyContext>::new();
        db.add_operation(2, Box::new(|_| {}));
        db.add_operation(3, Box::new(|_| {}));
        db.add_value_dependency_to_operation(2, 1, 0);
        db.add_value_dependency_to_operation(3, 1, 0);
        db.dirty_dependent_operation(1);

        assert_eq!(db.operation_by_id[&1].dirty, true);
        assert_eq!(db.operation_by_id[&2].dirty, true);
        assert_eq!(db.operation_by_id[&3].dirty, true);
    }

    #[test]
    fn test_dirty_dependent_operation_chain_dependency() {
        let mut db = ReactivityDatabase::<DummyContext>::new();
        db.add_operation(2, Box::new(|_| {}));
        db.add_operation(3, Box::new(|_| {}));
        db.add_value_dependency_to_operation(2, 1, 0);
        db.add_value_dependency_to_operation(3, 2, 0);
        db.dirty_dependent_operation(1);

        assert_eq!(db.operation_by_id[&1].dirty, true);
        assert_eq!(db.operation_by_id[&2].dirty, true);
        assert_eq!(db.operation_by_id[&3].dirty, true);
    }

    #[test]
    fn test_dirty_dependent_operation_no_dependency() {
        let mut db = ReactivityDatabase::<DummyContext>::new();
        db.add_operation(2, Box::new(|_| {}));
        db.add_value_dependency_to_operation(2, 1, 0);
        db.dirty_dependent_operation(3);

        assert_eq!(db.operation_by_id[&1].dirty, false);
        assert_eq!(db.operation_by_id[&2].dirty, false);
    }
}
