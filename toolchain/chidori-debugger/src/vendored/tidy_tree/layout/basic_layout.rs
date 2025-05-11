use num::Float;
use petgraph::graph::NodeIndex;

use crate::tidy_tree::layout::Layout;
use crate::tidy_tree::{geometry::Coord, node::{Node, TreeGraph}};
use std::cmp::{max, min};
use petgraph::prelude::Dfs;
use petgraph::visit::DfsPostOrder;

/// <img src="https://i.ibb.co/BLCfz0g/image.png" width="300" alt="Relative position"/>
///
/// Relative position illustration
pub struct BasicLayout {
    pub parent_child_margin: Coord,
    pub peer_margin: Coord,
}

/// <img src="https://i.ibb.co/BLCfz0g/image.png" width="300" alt="Relative position"/>
///
/// Relative position illustration
#[derive(Debug, Clone)]
pub struct BoundingBox {
    pub total_width: Coord,
    pub total_height: Coord,
}

impl Default for BoundingBox {
    fn default() -> Self {
        Self {
            total_height: 0.,
            total_width: 0.,
        }
    }
}

impl Layout for BasicLayout {
    fn layout(&mut self, tree: &mut TreeGraph) {
        let mut dfs = Dfs::new(&tree.graph, tree.root);
        while let Some(nx) = dfs.next(&tree.graph) {
            let mut node = &mut tree.graph[nx];
            node.tidy = None;
            node.x = 0.;
            node.y = 0.;
            node.relative_x = 0.;
            node.relative_y = 0.;
        }


        let mut dfs = DfsPostOrder::new(&tree.graph, tree.root);
        while let Some(nx) = dfs.next(&tree.graph) {
            self.update_meta(tree, nx);
        }

        let mut dfs = Dfs::new(&tree.graph, tree.root);
        while let Some(nx) = dfs.next(&tree.graph) {
            if let Some(parent) = tree.graph.neighbors_directed(nx, petgraph::Direction::Incoming).next() {
                let parent_node = tree.graph[parent].clone();
                let node = &mut tree.graph[nx];
                node.x = parent_node.x + node.relative_x;
                node.y = parent_node.y + node.relative_y;
            }
        }
    }

    fn partial_layout(&mut self, tree: &mut TreeGraph, changed: &[NodeIndex]) {
        todo!()
    }

    fn parent_child_margin(&self) -> Coord {
        self.parent_child_margin
    }

    fn peer_margin(&self) -> Coord {
        self.peer_margin
    }
}

impl BasicLayout {
    fn update_meta(&mut self, tree: &mut TreeGraph, node_index: NodeIndex) {
        {
            let mut node = &mut tree.graph[node_index];
            node.bbox = BoundingBox {
                total_height: node.height,
                total_width: node.width,
            };
        }

        let mut node = tree.graph[node_index].clone();
        let children: Vec<NodeIndex> = tree.graph.neighbors(node_index).collect();
        let n = children.len() as Coord;
        if n > 0. {
            let mut temp_x = 0.;
            let mut max_height = 0.;
            for (i, &child_index) in children.iter().enumerate() {
                let child = &mut tree.graph[child_index];
                child.relative_y = node.height + self.parent_child_margin;
                child.relative_x = temp_x + child.bbox.total_width / 2.;
                temp_x += child.bbox.total_width + self.peer_margin;
                max_height = Float::max(child.bbox.total_height, max_height);
            }

            let children_width = temp_x - self.peer_margin;
            let shift_x = -children_width / 2.;
            for &child_index in &children {
                let child = &mut tree.graph[child_index];
                child.relative_x += shift_x;
            }

            let mut node = &mut tree.graph[node_index]; // Reborrow after mutable borrows
            node.bbox.total_width = Float::max(children_width, node.width);
            node.bbox.total_height = node.height + self.parent_child_margin + max_height;
        }
    }
}

#[cfg(test)]
mod basic_layout_test {
    use super::{BasicLayout, BoundingBox};
    use crate::tidy_tree::{layout::Layout};
    use crate::tidy_tree::node::{Node, TreeGraph};

    #[test]
    fn easy_test_0() {
        let mut tree = TreeGraph::new(Node::new(0, 10., 10., None));
        let root_index = tree.root;
        let child1 = tree.add_child(root_index, Node::new(1, 10., 10., None));
        let child2 = tree.add_child(root_index, Node::new(2, 10., 10., None));
        tree.add_child(child2, Node::new(3, 10., 10., None));
        tree.add_child(root_index, Node::new(4, 10., 10., None));

        let mut layout = BasicLayout {
            parent_child_margin: 10.,
            peer_margin: 5.,
        };
        layout.layout(&mut tree);
    }
}