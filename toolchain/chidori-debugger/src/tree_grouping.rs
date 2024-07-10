use petgraph::graph::{Graph, NodeIndex};
use petgraph::Direction;
use std::collections::{HashMap, HashSet, VecDeque};
use petgraph::prelude::StableGraph;
use chidori_core::execution::execution::execution_graph::ExecutionNodeId;
// Example structure tree:
//
// A
// ├── B
// │   ├── D
// │   └── E
// └── C
//     └── F
//
// In this structure, D and E will be grouped under B,
// F will be grouped under C, and B and C will be grouped under A.



pub fn group_tree(
    original_tree: &StableGraph<ExecutionNodeId, (), petgraph::Directed>,
    groups: &HashSet<ExecutionNodeId>,
) -> (StableGraph<ExecutionNodeId, (), petgraph::Directed>, HashMap<ExecutionNodeId, StableGraph<ExecutionNodeId, (), petgraph::Directed>>) {
    let mut modified_tree = StableGraph::new();
    let mut grouped_trees: HashMap<ExecutionNodeId, StableGraph<ExecutionNodeId, (), petgraph::Directed>> = HashMap::new();

    // Function to process a subtree
    fn process_subtree(
        node: NodeIndex,
        original_tree: &StableGraph<ExecutionNodeId, (), petgraph::Directed>,
        groups: &HashSet<ExecutionNodeId>,
        current_graph: &mut StableGraph<ExecutionNodeId, (), petgraph::Directed>,
        grouped_trees: &mut HashMap<ExecutionNodeId, StableGraph<ExecutionNodeId, (), petgraph::Directed>>,
    ) -> NodeIndex {
        let node_id = original_tree[node];
        let current_node = current_graph.add_node(node_id);

        let mut queue = VecDeque::new();
        queue.push_back((node, current_node));

        while let Some((orig_node, parent_node)) = queue.pop_front() {
            for child in original_tree.neighbors_directed(orig_node, Direction::Outgoing) {
                let child_id = original_tree[child];
                if groups.contains(&child_id) {
                    // If child is a group, add it to the current graph but process its children in a new subgraph
                    let child_node = current_graph.add_node(child_id);
                    current_graph.add_edge(parent_node, child_node, ());

                    if !grouped_trees.contains_key(&child_id) {
                        let mut child_subgraph = StableGraph::new();
                        process_subtree(child, original_tree, groups, &mut child_subgraph, grouped_trees);
                        grouped_trees.insert(child_id, child_subgraph);
                    }
                } else {
                    // If child is not a group, add it to the current graph and process its children
                    let child_node = current_graph.add_node(child_id);
                    current_graph.add_edge(parent_node, child_node, ());
                    queue.push_back((child, child_node));
                }
            }
        }

        current_node
    }

    // Process the entire tree
    if let Some(root) = original_tree.node_indices().next() {
        let root_id = original_tree[root];
        if groups.contains(&root_id) {
            // If the root is a group, create a single-node modified tree
            modified_tree.add_node(root_id);

            // Create the subgraph for the root group
            let mut root_subgraph = StableGraph::new();
            process_subtree(root, original_tree, groups, &mut root_subgraph, &mut grouped_trees);
            grouped_trees.insert(root_id, root_subgraph);
        } else {
            // If the root is not a group, process the tree normally
            process_subtree(root, original_tree, groups, &mut modified_tree, &mut grouped_trees);
        }
    }

    (modified_tree, grouped_trees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::{Graph, NodeIndex};
    use petgraph::Direction;
    use std::collections::{HashMap, HashSet};
    use uuid::{ExecutionNodeId, Uuid};
    use chidori_core::execution::execution::execution_graph::ExecutionNodeId;

    fn create_uuid() -> ExecutionNodeId {
        Uuid::new_v4()
    }

    fn assert_tree_structure(tree: &StableGraph<ExecutionNodeId, (), petgraph::Directed>, expected_edges: &[(ExecutionNodeId, ExecutionNodeId)]) {
        for (source, target) in expected_edges {
            assert!(
                tree.node_indices()
                    .filter(|&n| tree[n] == *source)
                    .any(|n| tree.neighbors(n).any(|m| tree[m] == *target)),
                "Edge from {:?} to {:?} not found",
                source,
                target
            );
        }
    }

    #[test]
    fn test_simple_grouping() {
        // ASCII Diagram:
        //
        // Original Tree:   Groups:    Modified Tree:   Subgraph:
        //      A            {A}             A          A: A
        //     / \                                         / \
        //    B   C                                       B   C
        //
        let mut original_tree = StableGraph::new();

        let a = create_uuid();
        let b = create_uuid();
        let c = create_uuid();

        let a_node = original_tree.add_node(a);
        let b_node = original_tree.add_node(b);
        let c_node = original_tree.add_node(c);
        original_tree.add_edge(a_node, b_node, ());
        original_tree.add_edge(a_node, c_node, ());

        let groups = vec![a].into_iter().collect();

        let (modified_tree, grouped_trees) = group_tree(&original_tree, &groups);

        assert_eq!(modified_tree.node_count(), 1);
        assert_eq!(modified_tree.edge_count(), 0);
        assert!(modified_tree.node_weights().any(|&node| node == a));

        assert_eq!(grouped_trees.len(), 1);
        assert!(grouped_trees.contains_key(&a));

        let a_tree = &grouped_trees[&a];
        assert_eq!(a_tree.node_count(), 3);
        assert_eq!(a_tree.edge_count(), 2);
        assert_tree_structure(a_tree, &[(a, b), (a, c)]);
    }

    #[test]
    fn test_nested_grouping() {
        // ASCII Diagram:
        //
        // Original Tree:   Groups:    Modified Tree:   Subgraphs:
        //      A           {B,C}            A          B: B
        //     / \                          / \            / \
        //    B   C                        B   C          D   E
        //   / \   \
        //  D   E   F                                  C: C
        //                                                |
        //                                                F
        let mut original_tree = StableGraph::new();

        let a = create_uuid();
        let b = create_uuid();
        let c = create_uuid();
        let d = create_uuid();
        let e = create_uuid();
        let f = create_uuid();

        let a_node = original_tree.add_node(a);
        let b_node = original_tree.add_node(b);
        let c_node = original_tree.add_node(c);
        let d_node = original_tree.add_node(d);
        let e_node = original_tree.add_node(e);
        let f_node = original_tree.add_node(f);
        original_tree.add_edge(a_node, b_node, ());
        original_tree.add_edge(a_node, c_node, ());
        original_tree.add_edge(b_node, d_node, ());
        original_tree.add_edge(b_node, e_node, ());
        original_tree.add_edge(c_node, f_node, ());

        let groups = vec![b, c].into_iter().collect();

        let (modified_tree, grouped_trees) = group_tree(&original_tree, &groups);

        assert_eq!(modified_tree.node_count(), 3);
        assert_eq!(modified_tree.edge_count(), 2);
        assert_tree_structure(&modified_tree, &[(a, b), (a, c)]);

        assert_eq!(grouped_trees.len(), 2);
        assert!(grouped_trees.contains_key(&b));
        assert!(grouped_trees.contains_key(&c));

        let b_tree = &grouped_trees[&b];
        assert_eq!(b_tree.node_count(), 3);
        assert_eq!(b_tree.edge_count(), 2);
        assert_tree_structure(b_tree, &[(b, d), (b, e)]);

        let c_tree = &grouped_trees[&c];
        assert_eq!(c_tree.node_count(), 2);
        assert_eq!(c_tree.edge_count(), 1);
        assert_tree_structure(c_tree, &[(c, f)]);
    }

    #[test]
    fn test_partial_grouping() {
        // ASCII Diagram:
        //
        // Original Tree:   Groups:    Modified Tree:   Subgraph:
        //      A            {B}             A          B: B
        //    / | \                         / \            |
        //   B  C  D                       B   D           E
        //  /                                 |
        // E                                  C
        //
        let mut original_tree = StableGraph::new();

        let a = create_uuid();
        let b = create_uuid();
        let c = create_uuid();
        let d = create_uuid();
        let e = create_uuid();

        let a_node = original_tree.add_node(a);
        let b_node = original_tree.add_node(b);
        let c_node = original_tree.add_node(c);
        let d_node = original_tree.add_node(d);
        let e_node = original_tree.add_node(e);
        original_tree.add_edge(a_node, b_node, ());
        original_tree.add_edge(a_node, c_node, ());
        original_tree.add_edge(a_node, d_node, ());
        original_tree.add_edge(b_node, e_node, ());

        let groups = vec![b].into_iter().collect();

        let (modified_tree, grouped_trees) = group_tree(&original_tree, &groups);

        assert_eq!(modified_tree.node_count(), 4);
        assert_eq!(modified_tree.edge_count(), 3);
        assert_tree_structure(&modified_tree, &[(a, b), (a, c), (a, d)]);

        assert_eq!(grouped_trees.len(), 1);
        assert!(grouped_trees.contains_key(&b));

        let b_tree = &grouped_trees[&b];
        assert_eq!(b_tree.node_count(), 2);
        assert_eq!(b_tree.edge_count(), 1);
        assert_tree_structure(b_tree, &[(b, e)]);
    }

    #[test]
    fn test_nested_group_within_group() {
        // ASCII Diagram:
        //
        // Original Tree:   Groups:    Modified Tree:   Subgraphs:
        //      A           {A,C}            A          A: A
        //     / \                                         / \
        //    B   C                                       B   C
        //   / \   \                                     / \
        //  D   E   F                                   D   E
        //     / \                                         / \
        //    G   H                                       G   H
        //
        //                                            C: C
        //                                               |
        //                                               F

        let mut original_tree = StableGraph::new();

        let a = create_uuid();
        let b = create_uuid();
        let c = create_uuid();
        let d = create_uuid();
        let e = create_uuid();
        let f = create_uuid();
        let g = create_uuid();
        let h = create_uuid();

        let a_node = original_tree.add_node(a);
        let b_node = original_tree.add_node(b);
        let c_node = original_tree.add_node(c);
        let d_node = original_tree.add_node(d);
        let e_node = original_tree.add_node(e);
        let f_node = original_tree.add_node(f);
        let g_node = original_tree.add_node(g);
        let h_node = original_tree.add_node(h);

        original_tree.add_edge(a_node, b_node, ());
        original_tree.add_edge(a_node, c_node, ());
        original_tree.add_edge(b_node, d_node, ());
        original_tree.add_edge(b_node, e_node, ());
        original_tree.add_edge(c_node, f_node, ());
        original_tree.add_edge(e_node, g_node, ());
        original_tree.add_edge(e_node, h_node, ());

        let groups = vec![a, c].into_iter().collect();

        let (modified_tree, grouped_trees) = group_tree(&original_tree, &groups);

        // Check the modified tree
        assert_eq!(modified_tree.node_count(), 1);
        assert_eq!(modified_tree.edge_count(), 0);
        assert!(modified_tree.node_weights().any(|&w| w == a));

        // Check the grouped trees
        assert_eq!(grouped_trees.len(), 2);
        assert!(grouped_trees.contains_key(&a));
        assert!(grouped_trees.contains_key(&c));

        // Check group A
        let a_tree = &grouped_trees[&a];
        assert_eq!(a_tree.node_count(), 7);
        assert_eq!(a_tree.edge_count(), 6);
        assert_tree_structure(a_tree, &[
            (a, b), (a, c),
            (b, d), (b, e),
            (e, g), (e, h)
        ]);
        // Ensure F is not in A's subgraph
        assert!(!a_tree.node_weights().any(|&w| w == f));

        // Check group C
        let c_tree = &grouped_trees[&c];
        assert_eq!(c_tree.node_count(), 2);
        assert_eq!(c_tree.edge_count(), 1);
        assert_tree_structure(c_tree, &[(c, f)]);

        // Verify that B and its entire subtree are in A's subgraph
        for &node in &[b, d, e, g, h] {
            assert!(a_tree.node_weights().any(|&w| w == node), "Node {:?} should be in A's subgraph", node);
            assert!(!c_tree.node_weights().any(|&w| w == node), "Node {:?} should not be in C's subgraph", node);
        }

        // Verify that F is only in C's subgraph
        assert!(c_tree.node_weights().any(|&w| w == f), "F should be in C's subgraph");
        assert!(!a_tree.node_weights().any(|&w| w == f), "F should not be in A's subgraph");
    }
}


