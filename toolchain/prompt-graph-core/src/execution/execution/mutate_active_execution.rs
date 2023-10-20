use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::execution::primitives::operation::{OperationNode, OperationNodeDefinition};

pub enum GraphMutation {
    Update {
        node_id: usize,
        operation_node: OperationNodeDefinition,
    },
    Delete {
        node_id: usize,
    },
}

impl ExecutionGraph {
    /// This function is called when a change is made to the definition of the graph.
    /// When a change is made to the graph, we need to identify which elements are now dirtied and must
    /// be re-executed
    pub fn handle_operation_change(&mut self, node_id: usize, incoming_change: GraphMutation) {
        // Changes the operation of a cell
        let node_id = match &incoming_change {
            GraphMutation::Update { node_id, .. } | GraphMutation::Delete { node_id, .. } => {
                node_id.clone()
            }
            _ => return,
        };

        if let GraphMutation::Delete { node_id, .. } = &incoming_change {
            // TODO: tombstone the target node
            unimplemented!();
        }

        if let Some(existing_op_node) = self.operation_by_id.remove(&node_id) {
            if let GraphMutation::Delete { node_id, .. } = &incoming_change {}
            if let GraphMutation::Update {
                node_id,
                operation_node,
            } = incoming_change
            {
                // Update dependency graph
                let existing_dependencies = &existing_op_node.dependency_count;
                let new_dependencies = &operation_node.dependency_count;
                if existing_dependencies != new_dependencies {
                    // for neighbor in existing_dependencies {
                    //     self.dependency_graph
                    //         .remove_edge(neighbor.clone(), node_id.clone());
                    // }
                    // for (idx, dependency) in new_dependencies.iter().enumerate() {
                    //     self.dependency_graph
                    //         .add_edge(dependency.clone(), node_id.clone(), idx);
                    // }
                }

                // Overwrite the existing node definition
                self.operation_by_id
                    .insert(node_id.clone(), OperationNode::from(operation_node));
            }
        } else {
            // TODO: existing node does not exist - create the node
            if let GraphMutation::Update {
                node_id,
                operation_node,
            } = incoming_change
            {
                // for (idx, dependency) in operation_node.dependencies.iter().enumerate() {
                //     self.add_value_dependency_to_operation(
                //         node_id.clone(),
                //         dependency.clone(),
                //         idx,
                //     );
                // }
                self.operation_by_id
                    .insert(node_id.clone(), OperationNode::from(operation_node));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::primitives::operation::OperationNodeDefinition;
    /*
    Testing the operation change event handler - giving structure to modifications of the evaluation graph
     */

    #[test]
    fn test_handle_operation_change_add() {
        let mut db = ExecutionGraph::new();
        let node_id = 2;
        let operation_node = OperationNodeDefinition {
            operation: None,
            dependency_count: 0,
        };

        db.handle_operation_change(
            node_id,
            GraphMutation::Update {
                node_id,
                operation_node,
            },
        );
    }

    #[test]
    fn test_handle_operation_change_update() {
        let mut db = ExecutionGraph::new();
        let node_id = 1;
        let operation_node = OperationNodeDefinition {
            operation: None,
            dependency_count: 0,
        };

        db.handle_operation_change(
            node_id,
            GraphMutation::Update {
                node_id,
                operation_node,
            },
        );
    }

    #[test]
    fn test_handle_operation_change_delete() {
        let mut db = ExecutionGraph::new();
        let node_id = 1;

        db.handle_operation_change(node_id, GraphMutation::Delete { node_id });

        assert!(db.operation_by_id.get(&node_id).is_none());
    }

    #[test]
    fn test_handle_operation_change_execution_order() {
        let mut db = ExecutionGraph::new();
        let node_id = 2;
        let operation_node = OperationNodeDefinition {
            operation: None,
            dependency_count: 0,
        };

        db.handle_operation_change(
            node_id,
            GraphMutation::Update {
                node_id,
                operation_node,
            },
        );
    }

    /*
    Testing stepwise evaluation of the graph based on the execution order
     */
}
