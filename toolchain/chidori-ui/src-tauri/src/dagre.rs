use std::cell::RefCell;
use petgraph::graph::{NodeIndex};
use petgraph::graph::DiGraph;
use petgraph::visit::{EdgeRef, Topo};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use petgraph::graphmap::DiGraphMap;
use petgraph::prelude::Dfs;
use serde_json::json;
use serde::Serialize;
use chidori_core::execution::primitives::identifiers::{DependencyReference, OperationId};

#[derive(Serialize, Debug, Clone)]
pub struct Node {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) node_type: String,
    pub(crate) data: Data,
    pub(crate) position: Rect,
}

impl PartialEq<Self> for Node {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Node {}

impl Hash for Node {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct Data {
    pub(crate) label: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct Rect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) layer: usize,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Hash, PartialEq, Eq, Serialize, Debug, Clone)]
pub struct Edge {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) edge_type: String,
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) label: String,
}

pub struct DagreLayout<'a> {
    graph: &'a DiGraph<RefCell<&'a mut Node>, RefCell<&'a mut Edge>>,
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
    pub(crate) fn new(graph: &'a DiGraph<RefCell<&'a mut Node>, RefCell<&'a mut Edge>>, direction: Direction, node_separation: f32, edge_separation: f32, rank_separation: f32) -> Self {
        DagreLayout {
            graph,
            direction,
            node_separation,
            edge_separation,
            rank_separation,
        }
    }

    pub fn assert_no_cycles(&self) {
        // Cycle detection
        let first_node = self.graph.node_indices().next();
        if first_node.is_none() {
            return;
        }
        let first_node = first_node.unwrap();
        let mut dfs = Dfs::new(&self.graph, first_node);
        let mut on_stack = HashSet::new();  // Track nodes on the recursion stack
        let mut visited = HashSet::new();   // Track visited nodes

        while let Some(node) = dfs.next(&self.graph) {
            // println!("node: {:?} {:?}", node, self.graph.node_weight(node));

            if !visited.insert(node) {
                continue; // Skip if already visited
            }
            on_stack.insert(node); // Add to stack when first visited

            for edge in self.graph.edges_directed(node, petgraph::Outgoing) {
                let target = edge.target();
                // println!("target: {:?} {:?}", target, self.graph.node_weight(target));

                if visited.contains(&target) {
                    continue; // Skip if already visited
                }

                // Target not visited, but is on stack -> cycle detected
                if on_stack.contains(&target) {
                    panic!("Graph contains a cycle, cannot initialize layers. {:?}", target);
                }

                dfs.move_to(target); // Continue DFS from the target node
            }

            on_stack.remove(&node);  // Remove from stack when backtracking
        }
    }

    pub fn layout(&mut self) {
        self.assert_no_cycles();
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
            self.graph[node].borrow_mut().position.layer = layer;
        }
    }

    fn layering(&mut self) {
        // Initialize all nodes with a layer of 0
        for node in self.graph.node_indices() {
            self.graph[node].borrow_mut().position.layer = 0;
        }

        // Create an immutable reference for the topological sort
        let mut topo = Topo::new(self.graph);

        while let Some(node) = topo.next(self.graph) {
            let mut max_pred_layer = 0;
            // Look at all predecessors to find the maximum layer
            for edge in self.graph.edges_directed(node, petgraph::Incoming) {
                let pred = edge.source();
                max_pred_layer = max_pred_layer.max(self.graph[pred].borrow().position.layer);
            }
            // Temporarily reborrow self.graph mutably to update the node layer
            self.graph[node].borrow_mut().position.layer = max_pred_layer + 1;
        }
    }


    fn ordering(&mut self) {
        // Order nodes within each layer to minimize edge crossings
        for layer in 0..self.graph.node_count() {
            let nodes: Vec<NodeIndex> = self.graph.node_indices().filter(|&n| self.graph[n].borrow().position.layer == layer).collect();
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
                self.graph[node].borrow_mut().position.x = i as f32;
            }
        }
    }

    fn positioning(&mut self) {
        // Assign y-coordinates based on layers
        let mut y = 0.0;
        for layer in 0..self.graph.node_count() {
            let max_height = self.graph.node_indices()
                .filter(|&n| self.graph[n].borrow().position.layer == layer)
                .map(|n| self.graph[n].borrow().position.height)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                .unwrap_or(0.0);
            let node_idxs: Vec<NodeIndex> = self.graph.node_indices().filter(|&n| self.graph[n].borrow().position.layer == layer).collect();
            for node in node_idxs {
                self.graph[node].borrow_mut().position.y = y + max_height / 2.0;
            }
            y += max_height + self.rank_separation;
        }

        // Assign x-coordinates within each layer using Barycentre Method
        for layer in 0..self.graph.node_count() {
            let nodes: Vec<NodeIndex> = self.graph.node_indices().filter(|&n| self.graph[n].borrow().position.layer == layer).collect();
            let mut barycenters: HashMap<NodeIndex, f32> = HashMap::new();

            // Calculate barycenters for each node
            for &node in &nodes {
                let mut total_weight = 0.0;
                let mut weighted_sum = 0.0;
                for edge in self.graph.edges_directed(node, petgraph::Incoming) {
                    let source = edge.source();
                    let weight = 1.0; // Assume equal weight for all edges
                    weighted_sum += self.graph[source].borrow().position.x * weight;
                    total_weight += weight;
                }
                for edge in self.graph.edges_directed(node, petgraph::Outgoing) {
                    let target = edge.target();
                    let weight = 1.0; // Assume equal weight for all edges
                    weighted_sum += self.graph[target].borrow().position.x * weight;
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
                self.graph[node].borrow_mut().position.x = x;
                x += self.graph[node].borrow_mut().position.width + self.node_separation;
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