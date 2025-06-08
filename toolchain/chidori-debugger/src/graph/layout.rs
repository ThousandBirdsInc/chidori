//! Graph layout algorithms and node positioning systems.
//! 
//! This file implements layout algorithms for positioning nodes and edges in the graph
//! visualization. It handles converting execution graph data into spatial coordinates,
//! managing layout updates when the graph changes, and applying tree layout algorithms
//! to create visually appealing and readable graph arrangements.

use crate::application::ChidoriState;
use crate::graph::types::*;
use crate::vendored::tidy_tree::{Layout, Orientation, TidyLayout, TreeGraph};
use bevy::prelude::*;
use chidori_core::execution::execution::execution_graph::{ChronologyId, ExecutionNodeId};
use dashmap::DashMap;
use num::ToPrimitive;
use petgraph::prelude::{NodeIndex, StableGraph};
use petgraph::visit::Topo;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use crate::accidental::tree_grouping::group_tree;

pub fn update_graph_system_data_structures(
    mut graph_res: ResMut<GraphResource>,
    chidori_state: Res<ChidoriState>,
) {
    // If the execution graph has changed, clear the graph and reconstruct it
    if graph_res.hash_graph != hash_graph(&chidori_state.execution_graph) {
        let (dataset, node_ids) = chidori_state.construct_stablegraph_from_chidori_execution_graph(&chidori_state.execution_graph);
        graph_res.node_ids = node_ids;

        let (grouped_dataset, grouped_tree, group_dep_graph) = group_tree(&dataset, &chidori_state.grouped_nodes);

        // TODO: handle support for displaying groups
        // graph_res.execution_graph = grouped_dataset;
        graph_res.execution_graph = dataset;
        graph_res.grouped_tree = grouped_tree;
        graph_res.group_dependency_graph = group_dep_graph;
        graph_res.hash_graph = hash_graph(&chidori_state.execution_graph);
        graph_res.is_layout_dirty = true;
    }
}

pub fn hash_graph(input: &Vec<(ExecutionNodeId, ExecutionNodeId)>) -> u64 {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

pub fn hash_tuple(input: &ExecutionNodeId) -> usize {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish() as usize
}

pub fn generate_tree_layout(
    execution_graph: &&StableGraph<ChronologyId, ()>,
    node_dimensions: &DashMap<ChronologyId, (f32, f32)>
) -> TreeGraph {
    println!("=== GENERATE_TREE_LAYOUT CALLED ===");
    let mut tidy = TidyLayout::new(100., 100., Orientation::Vertical);
    let mut root = crate::vendored::tidy_tree::Node::new(0, 800., 100., None);
    root.y = 0.0;
    root.x = 0.0;
    let mut tree_graph = crate::vendored::tidy_tree::TreeGraph::new(root);
    println!("Created tree graph with root node");

    // Initialize nodes within a TreeGraph using our ExecutionGraph
    let mut topo = petgraph::visit::Topo::new(&execution_graph);
    let mut processed_count = 0;
    let mut added_count = 0;
    while let Some(x) = topo.next(&execution_graph) {
        if let Some(node) = &execution_graph.node_weight(x) {
            processed_count += 1;
            if x.index() == 0 {
                println!("Skipping root node at index 0");
                let dims = node_dimensions.entry(**node).or_insert((800.0, 100.0));
                continue;
            }
            let dims = node_dimensions.entry(**node).or_insert((800.0, 300.0));
            let width = dims.0;
            let height = dims.1;
            let tree_node = crate::vendored::tidy_tree::Node::new(x.index(), (width) as f64, (height) as f64, Some(Orientation::Vertical));

            // Get parent of this node and attach it if there is one
            let mut parents = &mut execution_graph
                .neighbors_directed(x, petgraph::Direction::Incoming);

            // Only a single parent ever occurs
            if let Some(parent) = &mut parents.next() {
                println!("Node {} has parent {}", x.index(), parent.index());
                // TODO: this is the wrong parent identity, this is the parent in the execution graph
                // needs to be in the tree graph
                if let Some(parent_index) = tree_graph.external_id_mapping.get(&parent.index()) {
                    println!("Found parent {} in tree, adding child {}", parent.index(), x.index());
                    let _ = tree_graph.add_child(parent_index.clone(), tree_node);
                    added_count += 1;
                } else {
                    println!("WARNING: Parent {} not found in tree for node {}", parent.index(), x.index());
                }
            } else {
                println!("Node {} has no parents, should be root", x.index());
            }
        }
    }
    println!("Processed {} nodes, added {} to tree", processed_count, added_count);

    tidy.layout(&mut tree_graph);

    if let Some(root) = tree_graph.graph.node_weight_mut(tree_graph.root) {
        root.y = 0.0;
    }

    let mut max_y: f32 = 0.0;
    let mut max_x: f32 = 0.0;
    let mut min_x: f32 = 0.0;
    let mut min_y: f32 = 0.0;
    for node in tree_graph.graph.node_weights() {
        max_x = max_x.max(node.x as f32);
        min_x = min_x.min(node.x as f32);
        max_y = max_y.max(node.y as f32);
        min_y = min_y.min(node.y as f32);
    }

    tree_graph
} 