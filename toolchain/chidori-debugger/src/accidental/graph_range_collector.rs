use std::cmp::Ordering;
use petgraph::prelude::{Dfs, StableGraph};
use chidori_core::execution::execution::execution_graph::ChronologyId;
use petgraph::graph::NodeIndex;
use std::collections::{HashMap, HashSet};
use petgraph::Outgoing;
use chidori_core::execution::execution::execution_state::EnclosedState;
use crate::chidori::ChidoriState;

#[derive(Clone, Default, Debug)]
pub struct StateRange {
    start_chronology_id: ChronologyId,
    end_chronology_id: ChronologyId,
    pub(crate) elements: Vec<ElementDimensions>,
    path: Vec<NodeIndex>,
    // Maximum depth of nested paths contained within this path
    pub(crate) nesting_depth: usize,
    pub is_explicit_close: bool,
}

impl StateRange {
    pub(crate) fn id(&self) -> (ChronologyId, ChronologyId) {
        (self.start_chronology_id, self.end_chronology_id)
    }

}

#[derive(Clone, Debug)]
pub struct ElementDimensions {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) x: f32,
    pub(crate) y: f32,
}

pub struct RangeCollector {
    pub(crate) ranges: Vec<StateRange>,
}


impl RangeCollector {
    pub(crate) fn new() -> Self {
        RangeCollector {
            ranges: Vec::new(),
        }
    }

    pub(crate) fn collect_paths(
        &mut self,
        graph: &StableGraph<ChronologyId, ()>,
        start_idx: NodeIndex,
        chronology_id: ChronologyId,
        dimensions_map: &HashMap<NodeIndex, ElementDimensions>,
        chidori_state: &ChidoriState,
    ) {
        // First collect all paths
        let mut dfs = Dfs::new(graph, start_idx);
        let mut potential_endpoints = Vec::new();

        while let Some(node) = dfs.next(graph) {
            if let Some(node_weight) = graph.node_weight(node) {
                if let Some(state) = chidori_state.get_execution_state_at_id(node_weight) {
                    if matches!(state.evaluating_enclosed_state, EnclosedState::Close(_)) {
                        if state.resolving_execution_node_state_id == chronology_id {
                            potential_endpoints.push((node, true));
                        }
                    }
                }
            }

            // If no outgoing edges, consider it an endpoint
            if graph.neighbors_directed(node, Outgoing).count() == 0 {
                potential_endpoints.push((node, false));
            }
        }

        // Sort by node index
        potential_endpoints.sort_by(|a, b| {
            a.0.cmp(&b.0)
        });
        // Remove duplicates from potential_endpoints
        potential_endpoints.dedup();

        for end_idx in potential_endpoints {
            let mut visited = HashSet::new();
            let mut current_path = Vec::new();

            self.collect_path_recursive(
                graph,
                start_idx,
                end_idx.0,
                end_idx.1,
                &mut visited,
                &mut current_path,
                dimensions_map,
                &chronology_id,
            );
        }
    }

    fn collect_path_recursive(
        &mut self,
        graph: &StableGraph<ChronologyId, ()>,
        current: NodeIndex,
        target: NodeIndex,
        is_explicit_close: bool,
        visited: &mut HashSet<NodeIndex>,
        current_path: &mut Vec<NodeIndex>,
        dimensions_map: &HashMap<NodeIndex, ElementDimensions>,
        chronology_id: &ChronologyId,
    ) {
        visited.insert(current);
        current_path.push(current);

        let Some(target_chronology_id) = graph.node_weight(target) else {
            return;
        };

        if current == target {
            let mut elements = Vec::new();
            for idx in current_path.iter() {
                if let Some(dims) = dimensions_map.get(&idx) {
                    elements.push(dims.clone());
                }
            }

            let range = StateRange {
                start_chronology_id: *chronology_id,
                end_chronology_id: *target_chronology_id,
                is_explicit_close,
                elements,
                path: current_path.clone(),
                nesting_depth: 0, // Will be calculated later
            };
            self.ranges.push(range);
        } else {
            for neighbor in graph.neighbors_directed(current, Outgoing) {
                // because we traversed here we know there's definitely a path to get here
                if !visited.contains(&neighbor) {
                    self.collect_path_recursive(
                        graph,
                        neighbor,
                        target,
                        is_explicit_close,
                        visited,
                        current_path,
                        dimensions_map,
                        chronology_id,
                    );
                }
            }
        }

        current_path.pop();
        visited.remove(&current);
    }


    pub(crate) fn remove_implicitly_ended_ranges(&mut self) {
        // Group ranges by start chronology IDs
        let mut range_groups: HashMap<ChronologyId, Vec<StateRange>> = HashMap::new();

        // First, group all ranges by their start and end IDs
        for range in self.ranges.drain(..) {
            let key = range.start_chronology_id;
            range_groups.entry(key).or_default().push(range);
        }

        // Process each group
        for ranges in range_groups.values() {
            // Check if there's an explicitly closed range in the group
            let explicit_ranges: Vec<_> = ranges.iter()
                .filter(|r| r.is_explicit_close)
                .collect();

            if !explicit_ranges.is_empty() {
                // If there are explicit ranges, only keep the first explicit one
                self.ranges.push(explicit_ranges[0].clone());
            } else {
                // If no explicit ranges, keep all ranges in the group
                self.ranges.extend(ranges.iter().cloned());
            }
        }

        // Sort ranges by start_chronology_id for consistent ordering
        self.ranges.sort_by_key(|r| r.start_chronology_id);
    }

    pub(crate) fn calculate_nesting_depths(&mut self) {
        // For each path, count how many other paths are fully contained within it
        self.ranges.sort_by(|a, b| {
            if a.elements.len() > b.elements.len() {
                Ordering::Greater
            } else if a.elements.len() < b.elements.len() {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        });
        for i in 0..self.ranges.len() {
            let mut max_contained_depth = 0;
            let parent_path = &self.ranges[i].path;

            // compare path with all other paths
            for j in 0..self.ranges.len() {

                // skip the same path
                if i != j {

                    let child_path = &self.ranges[j].path;

                    // Check if child_path is completely contained within parent_path
                    if is_subpath(parent_path, child_path) {
                        // Get the depth of the child path + 1
                        let child_depth = self.ranges[j].nesting_depth + 1;
                        max_contained_depth = max_contained_depth.max(child_depth);
                    }
                }
            }

            self.ranges[i].nesting_depth = max_contained_depth;
        }
    }
}

// Helper function to check if one path is completely contained within another
fn is_subpath(parent: &[NodeIndex], child: &[NodeIndex]) -> bool {
    if child.len() > parent.len() {
        return false;
    }

    // Find the first element of child in parent
    for window in parent.windows(child.len()) {
        if window == child {
            return true;
        }
    }
    false
}


#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::NodeIndex;
    use petgraph::prelude::StableGraph;
    use std::collections::HashMap;
    use uuid::Uuid;
    use chidori_core::execution::execution::execution_state::CloseReason;
    use chidori_core::execution::execution::ExecutionState;

    // Helper function to create a basic graph setup
    fn create_test_graph() -> (StableGraph<ChronologyId, ()>, HashMap<NodeIndex, ElementDimensions>) {
        let mut graph = StableGraph::new();
        let mut dimensions_map = HashMap::new();

        // Create basic dimensions for test nodes
        let test_dimensions = ElementDimensions {
            width: 10.0,
            height: 10.0,
            x: 0.0,
            y: 0.0,
        };

        // Add nodes with UUIDs and store their dimensions
        for _ in 0..5 {
            let node = graph.add_node(Uuid::now_v7());
            dimensions_map.insert(node, test_dimensions.clone());
        }

        (graph, dimensions_map)
    }

    #[test]
    fn test_single_path_with_close_endpoint() {
        let (mut graph, dimensions_map) = create_test_graph();
        let mut chidori_state = ChidoriState::default();

        // Create a linear path: 0 -> 1 -> 2 (close)
        let n0 = NodeIndex::new(0);
        let n1 = NodeIndex::new(1);
        let n2 = NodeIndex::new(2);

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());

        // Get the chronology IDs from the graph
        let start_id = *graph.node_weight(n0).unwrap();
        let end_id = *graph.node_weight(n2).unwrap();

        // Set up the close state for node 2
        let close_state = EnclosedState::Close(CloseReason::Complete);
        chidori_state.set_execution_state_at_id(
            &end_id,
            ExecutionState {
                evaluating_enclosed_state: close_state,
                resolving_execution_node_state_id: start_id,
                ..Default::default()
            },
        );

        let mut collector = RangeCollector::new();
        collector.collect_paths(&graph, n0, start_id, &dimensions_map, &chidori_state);

        assert_eq!(collector.ranges.len(), 1);
        assert_eq!(collector.ranges[0].path.len(), 3);
        assert_eq!(collector.ranges[0].start_chronology_id, start_id);
        assert_eq!(collector.ranges[0].end_chronology_id, end_id);
    }

    #[test]
    fn test_multiple_paths_with_close_endpoints() {
        let (mut graph, dimensions_map) = create_test_graph();
        let mut chidori_state = ChidoriState::default();

        // Create a branching path: 0 -> 1 -> 2 (close)
        //                         0 -> 3 -> 4 (close)
        let n0 = NodeIndex::new(0);
        let n1 = NodeIndex::new(1);
        let n2 = NodeIndex::new(2);
        let n3 = NodeIndex::new(3);
        let n4 = NodeIndex::new(4);

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());
        graph.add_edge(n0, n3, ());
        graph.add_edge(n3, n4, ());

        // Get chronology IDs from the graph
        let start_id = *graph.node_weight(n0).unwrap();
        let end_id_1 = *graph.node_weight(n2).unwrap();
        let end_id_2 = *graph.node_weight(n4).unwrap();

        // Set up close states for both endpoints
        for end_id in [end_id_1, end_id_2] {
            chidori_state.set_execution_state_at_id(
                &end_id,
                ExecutionState {
                    evaluating_enclosed_state: EnclosedState::Close(CloseReason::Complete),
                    resolving_execution_node_state_id: start_id,
                    ..Default::default()
                },
            );
        }

        let mut collector = RangeCollector::new();
        collector.collect_paths(&graph, n0, start_id, &dimensions_map, &chidori_state);

        assert_eq!(collector.ranges.len(), 2);

        // Verify both paths start from node 0
        for range in &collector.ranges {
            assert_eq!(range.start_chronology_id, start_id);
            assert_eq!(range.path[0], n0);
        }

        // Verify we have both paths: 0->1->2 and 0->3->4
        let paths: Vec<Vec<NodeIndex>> = collector.ranges.iter()
            .map(|r| r.path.clone())
            .collect();

        assert!(paths.contains(&vec![n0, n1, n2]));
        assert!(paths.contains(&vec![n0, n3, n4]));
    }

    #[test]
    fn test_nested_paths() {
        let (mut graph, dimensions_map) = create_test_graph();
        let mut chidori_state = ChidoriState::default();

        // Create nested paths:     0 -> 1 -> 2 (close)
        //                              ↳ 3 -> 4 (close)
        let n0 = NodeIndex::new(0);
        let n1 = NodeIndex::new(1);
        let n2 = NodeIndex::new(2);
        let n3 = NodeIndex::new(3);
        let n4 = NodeIndex::new(4);

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());
        graph.add_edge(n1, n3, ());
        graph.add_edge(n3, n4, ());

        // Get chronology IDs from the graph
        let start_id = *graph.node_weight(n0).unwrap();
        let end_id_1 = *graph.node_weight(n2).unwrap();
        let end_id_2 = *graph.node_weight(n4).unwrap();

        // Set up close states
        for end_id in [end_id_1, end_id_2] {
            chidori_state.set_execution_state_at_id(
                &end_id,
                ExecutionState {
                    evaluating_enclosed_state: EnclosedState::Close(CloseReason::Complete),
                    resolving_execution_node_state_id: start_id,
                    ..Default::default()
                },
            );
        }

        let mut collector = RangeCollector::new();
        collector.collect_paths(&graph, n0, start_id, &dimensions_map, &chidori_state);

        assert_eq!(collector.ranges.len(), 2);

        // Check nesting depths
        let path_0_1_2 = collector.ranges.iter()
            .find(|r| r.path == vec![n0, n1, n2])
            .unwrap();
        let path_0_1_3_4 = collector.ranges.iter()
            .find(|r| r.path == vec![n0, n1, n3, n4])
            .unwrap();

        assert!(path_0_1_2.nesting_depth >= 1, "Outer path should have nesting depth >= 1");
        assert_eq!(path_0_1_3_4.nesting_depth, 0, "Inner path should have nesting depth 0");
    }

    #[test]
    fn test_no_close_endpoints() {
        let (mut graph, dimensions_map) = create_test_graph();
        let chidori_state = ChidoriState::default();

        // Create a linear path without close states: 0 -> 1 -> 2
        let n0 = NodeIndex::new(0);
        let n1 = NodeIndex::new(1);
        let n2 = NodeIndex::new(2);

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());

        let start_id = *graph.node_weight(n0).unwrap();
        let end_id = *graph.node_weight(n2).unwrap();

        let mut collector = RangeCollector::new();
        collector.collect_paths(&graph, n0, start_id, &dimensions_map, &chidori_state);

        assert_eq!(collector.ranges.len(), 1);
        assert_eq!(collector.ranges[0].path.len(), 3);
        assert_eq!(collector.ranges[0].end_chronology_id, end_id);
    }

    #[test]
    fn test_path_with_wrong_resolve_id() {
        let (mut graph, dimensions_map) = create_test_graph();
        let mut chidori_state = ChidoriState::default();

        // Create a path where close state has different resolve ID
        let n0 = NodeIndex::new(0);
        let n1 = NodeIndex::new(1);
        let n2 = NodeIndex::new(2);

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());

        let start_id = *graph.node_weight(n0).unwrap();
        let end_id = *graph.node_weight(n2).unwrap();
        let different_id = Uuid::now_v7();

        // Set up close state with different resolve ID
        chidori_state.set_execution_state_at_id(
            &end_id,
            ExecutionState {
                evaluating_enclosed_state: EnclosedState::Close(CloseReason::Complete),
                resolving_execution_node_state_id: different_id,
                ..Default::default()
            },
        );

        let mut collector = RangeCollector::new();
        collector.collect_paths(&graph, n0, start_id, &dimensions_map, &chidori_state);

        // Should treat n2 as regular endpoint since Close state resolves to different ID
        assert_eq!(collector.ranges.len(), 1);
        assert_eq!(collector.ranges[0].path.len(), 3);
        assert_eq!(collector.ranges[0].end_chronology_id, end_id);
    }

    #[test]
    fn test_cyclic_graph() {
        let (mut graph, dimensions_map) = create_test_graph();
        let mut chidori_state = ChidoriState::default();

        // Create a cyclic graph: 0 -> 1 -> 2 -> 1 (cycle)
        let n0 = NodeIndex::new(0);
        let n1 = NodeIndex::new(1);
        let n2 = NodeIndex::new(2);

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());
        graph.add_edge(n2, n1, ()); // Creates cycle

        let start_id = *graph.node_weight(n0).unwrap();
        let end_id = *graph.node_weight(n2).unwrap();

        // Set up close state
        chidori_state.set_execution_state_at_id(
            &end_id,
            ExecutionState {
                evaluating_enclosed_state: EnclosedState::Close(CloseReason::Complete),
                resolving_execution_node_state_id: start_id,
                ..Default::default()
            },
        );

        let mut collector = RangeCollector::new();
        collector.collect_paths(&graph, n0, start_id, &dimensions_map, &chidori_state);

        // Should still find the path despite the cycle
        assert_eq!(collector.ranges.len(), 1);
        assert_eq!(collector.ranges[0].path, vec![n0, n1, n2]);
    }
}