use petgraph::graph::{NodeIndex};
use petgraph::graph::DiGraph;
use petgraph::visit::EdgeRef;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use petgraph::graphmap::DiGraphMap;
use serde_json::json;
use serde::Serialize;
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};

#[derive(Serialize, Debug)]
pub struct Node {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) node_type: String,
    pub(crate) data: Data,
    pub(crate) position: Rect,
}

#[derive(Serialize, Debug)]
pub struct Data {
    pub(crate) label: String,
}

#[derive(Serialize, Debug)]
pub struct Rect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) layer: usize,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Serialize, Debug)]
pub struct Edge {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) edge_type: String,
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) label: String,
}

pub struct DagreLayout<'a> {
    graph: &'a mut DiGraph<&'a mut Node, &'a mut Edge>,
    direction: Direction,
    node_separation: f32,
    edge_separation: f32,
    rank_separation: f32,
}

pub enum Direction {
    TopToBottom,
    LeftToRight,
}

impl<'a> DagreLayout<'a> {
    pub(crate) fn new(graph: &'a mut DiGraph<&'a mut Node, &'a mut Edge>, direction: Direction, node_separation: f32, edge_separation: f32, rank_separation: f32) -> Self {
        DagreLayout {
            graph,
            direction,
            node_separation,
            edge_separation,
            rank_separation,
        }
    }

    pub fn layout(&mut self) {
        self.initialize();
        self.layering();
        self.ordering();
        self.positioning();
        // self.edge_routing();
    }

    fn initialize(&mut self) {
        // Assign initial layers using longest path algorithm
        let mut layers = HashMap::new();
        for node in self.graph.node_indices() {
            let mut max_layer = 0;
            for edge in self.graph.edges_directed(node, petgraph::Outgoing) {
                let target = edge.target();
                max_layer = max_layer.max(layers.get(&target).cloned().unwrap_or(0) + 1);
            }
            layers.insert(node, max_layer);
        }
        for (node, layer) in layers {
            self.graph[node].position.layer = layer;
        }
    }

    fn layering(&mut self) {
        // Iteratively update node layers
        let mut updated = true;
        while updated {
            updated = false;
            for node in self.graph.node_indices() {
                let mut min_layer = self.graph[node].position.layer;
                let mut max_layer = self.graph[node].position.layer;
                for edge in self.graph.edges_directed(node, petgraph::Incoming) {
                    let source = edge.source();
                    min_layer = min_layer.min(self.graph[source].position.layer);
                }
                for edge in self.graph.edges_directed(node, petgraph::Outgoing) {
                    let target = edge.target();
                    max_layer = max_layer.max(self.graph[target].position.layer + 1);
                }
                if min_layer != self.graph[node].position.layer || max_layer != self.graph[node].position.layer {
                    self.graph[node].position.layer = (min_layer + max_layer) / 2;
                    updated = true;
                }
            }
        }
    }

    fn ordering(&mut self) {
        // Order nodes within each layer to minimize edge crossings
        for layer in 0..self.graph.node_count() {
            let nodes: Vec<NodeIndex> = self.graph.node_indices().filter(|&n| self.graph[n].position.layer == layer).collect();
            let mut ordered_nodes = Vec::new();
            let mut remaining_nodes = nodes.clone();
            while !remaining_nodes.is_empty() {
                let mut min_crossings = std::usize::MAX;
                let mut best_node = remaining_nodes[0];
                for &node in &remaining_nodes {
                    let mut crossings = 0;
                    for edge in self.graph.edges_directed(node, petgraph::Incoming) {
                        let source = edge.source();
                        if let Some(source_pos) = ordered_nodes.iter().position(|&n| n == source) {
                            for &other_node in &ordered_nodes[source_pos + 1..] {
                                if self.graph.contains_edge(other_node, node) {
                                    crossings += 1;
                                }
                            }
                        }
                    }
                    if crossings < min_crossings {
                        min_crossings = crossings;
                        best_node = node;
                    }
                }
                ordered_nodes.push(best_node);
                remaining_nodes.retain(|&n| n != best_node);
            }
            for (i, &node) in ordered_nodes.iter().enumerate() {
                self.graph[node].position.x = i as f32;
            }
        }
    }

    fn positioning(&mut self) {
        // Assign y-coordinates based on layers
        let mut y = 0.0;
        for layer in 0..self.graph.node_count() {
            let max_height = self.graph.node_indices()
                .filter(|&n| self.graph[n].position.layer == layer)
                .map(|n| self.graph[n].position.height)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                .unwrap_or(0.0);
            let node_idxs: Vec<NodeIndex> = self.graph.node_indices().filter(|&n| self.graph[n].position.layer == layer).collect();
            for node in node_idxs {
                self.graph[node].position.y = y + max_height / 2.0;
            }
            y += max_height + self.rank_separation;
        }

        // Assign x-coordinates within each layer using Barycentre Method
        for layer in 0..self.graph.node_count() {
            let nodes: Vec<NodeIndex> = self.graph.node_indices().filter(|&n| self.graph[n].position.layer == layer).collect();
            let mut barycenters: HashMap<NodeIndex, f32> = HashMap::new();

            // Calculate barycenters for each node
            for &node in &nodes {
                let mut total_weight = 0.0;
                let mut weighted_sum = 0.0;
                for edge in self.graph.edges_directed(node, petgraph::Incoming) {
                    let source = edge.source();
                    let weight = 1.0; // Assume equal weight for all edges
                    weighted_sum += self.graph[source].position.x * weight;
                    total_weight += weight;
                }
                for edge in self.graph.edges_directed(node, petgraph::Outgoing) {
                    let target = edge.target();
                    let weight = 1.0; // Assume equal weight for all edges
                    weighted_sum += self.graph[target].position.x * weight;
                    total_weight += weight;
                }
                let barycenter = if total_weight > 0.0 {
                    weighted_sum / total_weight
                } else {
                    0.0
                };
                barycenters.insert(node, barycenter);
            }

            // Sort nodes based on their barycenters
            let mut sorted_nodes: Vec<NodeIndex> = nodes.clone();
            sorted_nodes.sort_by(|&a, &b| barycenters[&a].partial_cmp(&barycenters[&b]).unwrap_or(Ordering::Equal));

            // Assign x-coordinates based on the sorted order
            let mut x = 0.0;
            for &node in &sorted_nodes {
                self.graph[node].position.x = x;
                x += self.graph[node].position.width + self.node_separation;
            }
        }
    }

   //  fn edge_routing(&mut self) {
   //      // Route edges using polyline with bends at layer boundaries
   //      for edge in self.graph.edge_indices() {
   //          let source = self.graph.edge_endpoints(edge).unwrap().0;
   //          let target = self.graph.edge_endpoints(edge).unwrap().1;
   //          let mut points = Vec::new();
   //          points.push((self.graph[source].position.x + self.graph[source].position.width / 2.0, self.graph[source].position.y));
   //          let source_layer = self.graph[source].position.layer;
   //          let target_layer = self.graph[target].position.layer;
   //          for layer in source_layer + 1..target_layer {
   //              let y = self.graph.node_indices()
   //                  .filter(|&n| self.graph[n].position.layer == layer)
   //                  .map(|n| self.graph[n].position.y)
   //                  .min_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
   //                  .unwrap_or(0.0);
   //              points.push((self.graph[source].position.x + self.graph[source].position.width / 2.0, y - self.edge_separation));
   //              points.push((self.graph[target].position.x + self.graph[target].position.width / 2.0, y - self.edge_separation));
   //          }
   //          points.push((self.graph[target].position.x + self.graph[target].position.width / 2.0, self.graph[target].position.y));
   //          self.graph[edge].points = points;
   //      }
   // }
}

#[cfg(test)]
mod test {
    use super::*;

}