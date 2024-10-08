use petgraph::graph::{Graph, Neighbors, NodeIndex};
use petgraph::Direction;
use std::collections::{HashMap, VecDeque};
use std::iter::Map;
use petgraph::visit::Bfs;
use crate::tidy_tree::{geometry::Coord, layout::BoundingBox};

#[derive(Debug, Clone)]
pub struct TidyData {
    pub thread_left: Option<NodeIndex>,
    pub thread_right: Option<NodeIndex>,
    pub extreme_left: Option<NodeIndex>,
    pub extreme_right: Option<NodeIndex>,
    pub shift_acceleration: Coord,
    pub shift_change: Coord,
    pub modifier_to_subtree: Coord,
    pub modifier_thread_left: Coord,
    pub modifier_thread_right: Coord,
    pub modifier_extreme_left: Coord,
    pub modifier_extreme_right: Coord,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub external_id: usize,
    pub width: Coord,
    pub height: Coord,
    pub x: Coord,
    pub y: Coord,
    pub relative_x: Coord,
    pub relative_y: Coord,
    pub bbox: BoundingBox,
    pub tidy: Option<TidyData>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            external_id: usize::MAX,
            width: 0.,
            height: 0.,
            x: 0.,
            y: 0.,
            relative_x: 0.,
            relative_y: 0.,
            bbox: Default::default(),
            tidy: None,
        }
    }
}

impl Node {
    pub fn new(id: usize, width: Coord, height: Coord) -> Self {
        Node {
            external_id: id,
            width,
            height,
            bbox: Default::default(),
            x: 0.,
            y: 0.,
            relative_x: 0.,
            relative_y: 0.,
            tidy: None,
        }
    }

    pub fn bottom(&self) -> Coord {
        self.height + self.y
    }

    pub fn tidy_mut(&mut self) -> &mut TidyData {
        self.tidy.as_mut().unwrap()
    }

    pub fn tidy(&self) -> &TidyData {
        self.tidy.as_ref().unwrap()
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.x - self.width / 2. < other.x + other.width / 2.
            && self.x + self.width / 2. > other.x - other.width / 2.
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

pub struct TreeGraph {
    pub(crate) graph: Graph<Node, ()>,
    pub(crate) root: NodeIndex,
    pub(crate) external_id_mapping: HashMap<usize, NodeIndex>
}

impl TreeGraph {
    pub fn new(root: Node) -> Self {
        let mut graph = Graph::new();
        let root_id = root.external_id.clone();
        let root_index = graph.add_node(root);
        let mut external_id_mapping: HashMap<usize, NodeIndex> = HashMap::new();
        external_id_mapping.insert(root_id, root_index);
        Self {
            graph,
            root: root_index,
            external_id_mapping,
        }
    }

    pub fn add_child(&mut self, parent: NodeIndex, child: Node) -> NodeIndex {
        let child_external_id = child.external_id.clone();
        let child_index = self.graph.add_node(child);
        self.graph.add_edge(parent, child_index, ());
        self.external_id_mapping.insert(child_external_id, child_index);
        child_index
    }

    pub fn depth(&self, node: NodeIndex) -> usize {
        let mut depth = 0;
        let mut current = node;
        while let Some(parent) = self.graph.neighbors_directed(current, Direction::Incoming).next() {
            current = parent;
            depth += 1;
        }
        depth
    }

    pub fn bfs_traversal_with_depth_mut(
        &mut self
    ) -> impl Iterator<Item = (NodeIndex, usize)> + '_ {
        let mut bfs = Bfs::new(&self.graph, self.root);
        let mut depth_map = std::collections::HashMap::new();
        depth_map.insert(self.root, 0);

        std::iter::from_fn(move || {
            while let Some(nx) = bfs.next(&self.graph) {
                let depth = *depth_map.get(&nx).unwrap_or(&0);

                // Update depth map for neighbors
                let children: Vec<_> = self.graph.neighbors(nx).collect();
                for neighbor in children {
                    if !depth_map.contains_key(&neighbor) {
                        depth_map.insert(neighbor, depth + 1);
                    }
                }

                return Some((nx, depth));
            }
            None
        })
    }

    pub fn pre_order_traversal_with_depth_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Node, usize),
    {
        self.pre_order_traversal_with_depth_mut_helper(self.root, 0, &mut f);
    }

    fn pre_order_traversal_with_depth_mut_helper<F>(&mut self, node: NodeIndex, depth: usize, f: &mut F)
    where
        F: FnMut(&mut Node, usize),
    {
        f(&mut self.graph[node], depth);
        let children: Vec<_> = self.graph.neighbors(node).collect();
        for child in children {
            self.pre_order_traversal_with_depth_mut_helper(child, depth + 1, f);
        }
    }

    pub fn str(&self) -> String {
        let mut s = String::new();
        self.str_helper(self.root, 0, &mut s);
        s
    }

    fn str_helper(&self, node: NodeIndex, depth: usize, s: &mut String) {
        let node_data = &self.graph[node];
        let indent = "    ".repeat(depth);

        if node_data.tidy.is_some() {
            s.push_str(&format!(
                "{}x: {}, y: {}, width: {}, height: {}, rx: {}, mod: {}, id: {}\n",
                indent,
                node_data.x,
                node_data.y,
                node_data.width,
                node_data.height,
                node_data.relative_x,
                node_data.tidy().modifier_to_subtree,
                node_data.external_id
            ));
        } else {
            s.push_str(&format!(
                "{}x: {}, y: {}, width: {}, height: {}, rx: {}, id: {}\n",
                indent,
                node_data.x,
                node_data.y,
                node_data.width,
                node_data.height,
                node_data.relative_x,
                node_data.external_id
            ));
        }

        for child in self.graph.neighbors(node) {
            self.str_helper(child, depth + 1, s);
        }
    }

    pub fn get_from_external_id(&self, external_id: &usize) -> Option<(&NodeIndex, &Node)> {
        self.external_id_mapping.get(external_id).map(|idx| (idx, &self.graph[*idx]))
    }

    pub fn get_children(&self, idx: NodeIndex) -> Vec<NodeIndex> {
        let children: Vec<_> = self.graph.neighbors(idx).collect();
        children
    }
}