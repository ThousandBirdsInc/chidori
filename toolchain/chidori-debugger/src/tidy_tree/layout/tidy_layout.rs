use std::{collections::HashSet};
use petgraph::graph::{NodeIndex, Graph};
use num::Float;
use petgraph::prelude::Dfs;
use tinyset::SetUsize;

use crate::tidy_tree::{geometry::Coord, node::{TidyData}};

use crate::tidy_tree::node::{Node, TreeGraph};
use crate::tidy_tree::layout::{Layout};
use super::linked_y_list::LinkedYList;

pub struct TidyLayout {
    pub parent_child_margin: Coord,
    pub peer_margin: Coord,
    is_layered: bool,
    /// this is only for layered layout
    depth_to_y: Vec<Coord>,
}

impl TidyLayout {
    pub fn new(parent_child_margin: Coord, peer_margin: Coord) -> Self {
        TidyLayout {
            parent_child_margin,
            peer_margin,
            is_layered: false,
            depth_to_y: vec![],
        }
    }

    pub fn new_layered(parent_child_margin: Coord, peer_margin: Coord) -> Self {
        TidyLayout {
            parent_child_margin,
            peer_margin,
            is_layered: true,
            depth_to_y: vec![],
        }
    }
}

struct Contour {
    is_left: bool,
    pub current: Option<NodeIndex>,
    modifier_sum: Coord,
}

impl Contour {
    pub fn new(is_left: bool, current: NodeIndex, graph: &Graph<Node, ()>) -> Self {
        Contour {
            is_left,
            current: Some(current),
            modifier_sum: graph[current].tidy().modifier_to_subtree,
        }
    }

    fn node<'a>(&self, graph: &'a Graph<Node, ()>) -> &'a Node {
        &graph[self.current.unwrap()]
    }

    pub fn is_none(&self) -> bool {
        self.current.is_none()
    }

    pub fn left(&self, graph: &Graph<Node, ()>) -> Coord {
        let node = self.node(graph);
        self.modifier_sum + node.relative_x - node.width / 2.
    }

    pub fn right(&self, graph: &Graph<Node, ()>) -> Coord {
        let node = self.node(graph);
        self.modifier_sum + node.relative_x + node.width / 2.
    }

    pub fn bottom(&self, graph: &Graph<Node, ()>) -> Coord {
        match self.current {
            Some(node_index) => {
                let node = &graph[node_index];
                node.y + node.height
            }
            None => 0.,
        }
    }

    pub fn next(&mut self, graph: &Graph<Node, ()>) {
        if let Some(current) = self.current {
            let node = &graph[current];
            if self.is_left {
                if let Some(first_child) = graph.neighbors(current).next() {
                    self.current = Some(first_child);
                    let new_node = &graph[first_child];
                    self.modifier_sum += new_node.tidy().modifier_to_subtree;
                } else {
                    self.modifier_sum += node.tidy().modifier_thread_left;
                    self.current = node.tidy().thread_left;
                }
            } else {
                if let Some(last_child) = graph.neighbors(current).last() {
                    self.current = Some(last_child);
                    let new_node = &graph[last_child];
                    self.modifier_sum += new_node.tidy().modifier_to_subtree;
                } else {
                    self.modifier_sum += node.tidy().modifier_thread_right;
                    self.current = node.tidy().thread_right;
                }
            }
        }
    }
}

impl TidyLayout {
    fn separate(
        &mut self,
        tree: &mut TreeGraph,
        node: NodeIndex,
        child_index: usize,
        mut y_list: LinkedYList,
    ) -> LinkedYList {
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        let left_child = children[child_index - 1];
        let right_child = children[child_index];

        // right contour of the left
        let mut left = Contour::new(false, left_child, &tree.graph);
        // left contour of the right
        let mut right = Contour::new(true, right_child, &tree.graph);

        while !left.is_none() && !right.is_none() {
            if left.bottom(&tree.graph) > y_list.bottom() {
                let b = y_list.bottom();
                let top = y_list.pop();
                if top.is_none() {
                    println!(
                        "Err\n\n{}\n\nleft.bottom={}\nyList.bottom={}",
                        tree.str(),
                        left.bottom(&tree.graph),
                        b
                    );
                }

                y_list = top.unwrap();
            }

            let dist = left.right(&tree.graph) - right.left(&tree.graph) + self.peer_margin;
            if dist > 0. {
                // left and right are too close. move right part with distance of dist
                right.modifier_sum += dist;
                self.move_subtree(tree, node, child_index, y_list.index, dist);
            }

            let left_bottom = left.bottom(&tree.graph);
            let right_bottom = right.bottom(&tree.graph);
            if left_bottom <= right_bottom {
                left.next(&tree.graph);
            }
            if left_bottom >= right_bottom {
                right.next(&tree.graph);
            }
        }

        if left.is_none() && !right.is_none() {
            self.set_left_thread(tree, node, child_index, right.current.unwrap(), right.modifier_sum);
        } else if !left.is_none() && right.is_none() {
            self.set_right_thread(tree, node, child_index, left.current.unwrap(), left.modifier_sum);
        }

        y_list
    }

    fn set_left_thread(
        &mut self,
        tree: &mut TreeGraph,
        node: NodeIndex,
        current_index: usize,
        target: NodeIndex,
        modifier: Coord,
    ) {
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        let first = children[0];
        let current = children[current_index];
        let diff = modifier
            - tree.graph[first].tidy().modifier_extreme_left
            - tree.graph[first].tidy().modifier_to_subtree;

        let extreme_left = tree.graph[first].tidy().extreme_left.unwrap();
        tree.graph[extreme_left].tidy_mut().thread_left = Some(NodeIndex::new(target.index()));
        tree.graph[extreme_left].tidy_mut().modifier_thread_left = diff;
        tree.graph[first].tidy_mut().extreme_left = tree.graph[current].tidy().extreme_left;
        tree.graph[first].tidy_mut().modifier_extreme_left = tree.graph[current].tidy().modifier_extreme_left
            + tree.graph[current].tidy().modifier_to_subtree
            - tree.graph[first].tidy().modifier_to_subtree;
    }

    fn set_right_thread(
        &mut self,
        tree: &mut TreeGraph,
        node: NodeIndex,
        current_index: usize,
        target: NodeIndex,
        modifier: Coord,
    ) {
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        let current = children[current_index];
        let diff = modifier
            - tree.graph[current].tidy().modifier_extreme_right
            - tree.graph[current].tidy().modifier_to_subtree;

        let extreme_right = tree.graph[current].tidy().extreme_right.unwrap();
        tree.graph[extreme_right].tidy_mut().thread_right = Some(NodeIndex::new(target.index()));
        tree.graph[extreme_right].tidy_mut().modifier_thread_right = diff;
        let prev = children[current_index - 1];
        tree.graph[current].tidy_mut().extreme_right = tree.graph[prev].tidy().extreme_right;
        tree.graph[current].tidy_mut().modifier_extreme_right = tree.graph[prev].tidy().modifier_extreme_right
            + tree.graph[prev].tidy().modifier_to_subtree
            - tree.graph[current].tidy().modifier_to_subtree;
    }

    fn move_subtree(
        &mut self,
        tree: &mut TreeGraph,
        node: NodeIndex,
        current_index: usize,
        from_index: usize,
        distance: Coord,
    ) {
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        let child = children[current_index];
        tree.graph[child].tidy_mut().modifier_to_subtree += distance;

        // distribute extra space to nodes between from_index to current_index
        if from_index != current_index - 1 {
            let index_diff = (current_index - from_index) as Coord;
            tree.graph[children[from_index + 1]].tidy_mut().shift_acceleration += distance / index_diff;
            tree.graph[children[current_index]].tidy_mut().shift_acceleration -= distance / index_diff;
            tree.graph[children[current_index]].tidy_mut().shift_change -=
                distance - distance / index_diff;
        }
    }

    fn set_y_recursive(&mut self, tree: &mut TreeGraph) {
        if !self.is_layered {
            let mut dfs = Dfs::new(&tree.graph, tree.root);
            while let Some(nx) = dfs.next(&tree.graph) {
                let parent = tree.graph.neighbors_directed(nx, petgraph::Direction::Incoming).next();
                let parent_y = parent.map(|p| tree.graph[p].y + tree.graph[p].height).unwrap_or(0.);
                let new_y = parent_y + self.parent_child_margin;
                tree.graph[nx].y = new_y;
            }
        } else {
            let depth_to_y = &mut self.depth_to_y;
            depth_to_y.clear();
            let margin = self.parent_child_margin;
            let collected_nodes: Vec<_> = tree.bfs_traversal_with_depth_mut().collect();

            for (nx, depth) in collected_nodes {
                while depth >= depth_to_y.len() {
                    depth_to_y.push(0.);
                }

                if tree.graph.neighbors_directed(nx, petgraph::Direction::Incoming).next().is_none() || depth == 0 {
                    let node = &mut tree.graph[nx];
                    node.y = 0.;
                    continue;
                }

                let parent = tree.graph.neighbors_directed(nx, petgraph::Direction::Incoming).next().unwrap();
                depth_to_y[depth] = Float::max(
                    depth_to_y[depth],
                    depth_to_y[depth - 1] + tree.graph[parent].height + self.parent_child_margin,
                );
            }
            tree.pre_order_traversal_with_depth_mut(|node, depth| {
                node.y = depth_to_y[depth];
            })
        }
    }

    fn first_walk(&mut self, tree: &mut TreeGraph, node: NodeIndex) {
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        if children.is_empty() {
            self.set_extreme(tree, node);
            return;
        }

        self.first_walk(tree, children[0]);
        let mut y_list = LinkedYList::new(0, tree.graph[tree.graph[children[0]].tidy().extreme_right.unwrap()].bottom());
        for i in 1..children.len() {
            let current_child = children[i];
            self.first_walk(tree, current_child);
            let max_y = tree.graph[tree.graph[current_child].tidy().extreme_left.unwrap()].bottom();
            y_list = self.separate(tree, node, i, y_list);
            y_list = y_list.update(i, max_y);
        }

        self.position_root(tree, node);
        self.set_extreme(tree, node);
    }

    fn first_walk_with_filter(&mut self, tree: &mut TreeGraph, node: NodeIndex, set: &SetUsize) {
        if !set.contains(node.index()) {
            self.invalidate_extreme_thread(tree, node);
            return;
        }

        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        if children.is_empty() {
            self.set_extreme(tree, node);
            return;
        }

        self.first_walk_with_filter(tree, children[0], set);
        let mut y_list = LinkedYList::new(0, tree.graph[tree.graph[children[0]].tidy().extreme_right.unwrap()].bottom());
        for i in 1..children.len() {
            let current_child = children[i];
            tree.graph[current_child].tidy_mut().modifier_to_subtree = -tree.graph[current_child].relative_x;
            self.first_walk_with_filter(tree, current_child, set);
            let max_y = tree.graph[tree.graph[current_child].tidy().extreme_left.unwrap()].bottom();
            y_list = self.separate(tree, node, i, y_list);
            y_list = y_list.update(i, max_y);
        }

        self.position_root(tree, node);
        self.set_extreme(tree, node);
    }

    fn second_walk(&mut self, tree: &mut TreeGraph, node: NodeIndex, mut mod_sum: Coord) {
        mod_sum += tree.graph[node].tidy().modifier_to_subtree;
        tree.graph[node].x = tree.graph[node].relative_x + mod_sum;
        self.add_child_spacing(tree, node);

        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        for child in children {
            self.second_walk(tree, child, mod_sum);
        }
    }

    fn second_walk_with_filter(&mut self, tree: &mut TreeGraph, node: NodeIndex, mut mod_sum: Coord, set: &SetUsize) {
        mod_sum += tree.graph[node].tidy().modifier_to_subtree;
        let new_x = tree.graph[node].relative_x + mod_sum;
        if (new_x - tree.graph[node].x).abs() < 1e-8 && !set.contains(node.index()) {
            return;
        }

        tree.graph[node].x = new_x;
        self.add_child_spacing(tree, node);

        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        for child in children {
            self.second_walk_with_filter(tree, child, mod_sum, set);
        }
    }

    fn set_extreme(&mut self, tree: &mut TreeGraph, node: NodeIndex) {
        let node_ptr = NodeIndex::new(node.index());
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        if children.is_empty() {
            tree.graph[node].tidy_mut().extreme_left = Some(node_ptr);
            tree.graph[node].tidy_mut().extreme_right = Some(node_ptr);
            tree.graph[node].tidy_mut().modifier_extreme_left = 0.;
            tree.graph[node].tidy_mut().modifier_extreme_right = 0.;
        } else {
            let first = children[0];
            tree.graph[node].tidy_mut().extreme_left = tree.graph[first].tidy().extreme_left;
            tree.graph[node].tidy_mut().modifier_extreme_left = tree.graph[first].tidy().modifier_to_subtree + tree.graph[first].tidy().modifier_extreme_left;
            let last = children.last().unwrap();
            tree.graph[node].tidy_mut().extreme_right = tree.graph[*last].tidy().extreme_right;
            tree.graph[node].tidy_mut().modifier_extreme_right = tree.graph[*last].tidy().modifier_to_subtree + tree.graph[*last].tidy().modifier_extreme_right;
        }
    }

    fn position_root(&mut self, tree: &mut TreeGraph, node: NodeIndex) {
        let children: Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        let first = children[0];
        let first_child_pos = tree.graph[first].relative_x + tree.graph[first].tidy().modifier_to_subtree;
        let last = *children.last().unwrap();
        let last_child_pos = tree.graph[last].relative_x + tree.graph[last].tidy().modifier_to_subtree;
        tree.graph[node].relative_x = (first_child_pos + last_child_pos) / 2.;
        // make modifier_to_subtree + relative_x = 0. so that
        // there will always be collision in `separation()`'s first loop
        tree.graph[node].tidy_mut().modifier_to_subtree = -tree.graph[node].relative_x;
    }

    fn add_child_spacing(&mut self, tree: &mut TreeGraph, node: NodeIndex) {
        let mut speed = 0.;
        let mut delta = 0.;
        let children : Vec<NodeIndex> = tree.graph.neighbors(node).collect();
        for child in children {
            let child_tidy = tree.graph[child].tidy_mut();
            speed += child_tidy.shift_acceleration;
            delta += speed + child_tidy.shift_change;
            child_tidy.modifier_to_subtree += delta;
            child_tidy.shift_acceleration = 0.;
            child_tidy.shift_change = 0.;
        }
    }
}

impl Layout for TidyLayout {
    fn layout(&mut self, tree: &mut TreeGraph) {
        // tree.pre_order_traversal_mut(|node| init_node(node));
        let mut dfs = Dfs::new(&tree.graph, tree.root);
        while let Some(nx) = dfs.next(&tree.graph) {
            let node = &mut tree.graph[nx];
            init_node(node);
        }
        self.set_y_recursive(tree);
        self.first_walk(tree, tree.root);
        self.second_walk(tree, tree.root, 0.);
    }

    fn parent_child_margin(&self) -> Coord {
        self.parent_child_margin
    }

    fn peer_margin(&self) -> Coord {
        self.peer_margin
    }

    fn partial_layout(&mut self, tree: &mut TreeGraph, changed: &[NodeIndex]) {
        // not implemented for layered
        if self.is_layered {
            self.layout(tree);
            return;
        }

        for &node_index in changed.iter() {
            let node = &mut tree.graph[node_index];
            if node.tidy.is_none() {
                init_node(node);
            }

            // TODO: can be lazy
            self.set_y_recursive(tree);
        }

        let mut set: SetUsize = SetUsize::new();
        for &node_index in changed.iter() {
            set.insert(node_index.index());
            let mut current = node_index;
            while let Some(parent) = tree.graph.neighbors_directed(current, petgraph::Direction::Incoming).next() {
                self.invalidate_extreme_thread(tree, current);
                set.insert(parent.index());
                current = parent;
            }
        }

        self.first_walk_with_filter(tree, tree.root, &set);
        // TODO: this can be optimized with onscreen detection,
        // then all nodes' absolute x position can be evaluate lazily
        self.second_walk_with_filter(tree, tree.root, 0., &set);
    }
}

fn init_node(node: &mut Node) {
    if node.tidy.is_some() {
        let tidy = node.tidy_mut();
        tidy.extreme_left = None;
        tidy.extreme_right = None;
        tidy.shift_acceleration = 0.;
        tidy.shift_change = 0.;
        tidy.modifier_to_subtree = 0.;
        tidy.modifier_extreme_left = 0.;
        tidy.modifier_extreme_right = 0.;
        tidy.thread_left = None;
        tidy.thread_right = None;
        tidy.modifier_thread_left = 0.;
        tidy.modifier_thread_right = 0.;
    } else {
        node.tidy = Some(TidyData {
            extreme_left: None,
            extreme_right: None,
            shift_acceleration: 0.,
            shift_change: 0.,
            modifier_to_subtree: 0.,
            modifier_extreme_left: 0.,
            modifier_extreme_right: 0.,
            thread_left: None,
            thread_right: None,
            modifier_thread_left: 0.,
            modifier_thread_right: 0.,
        });
    }

    node.x = 0.;
    node.y = 0.;
    node.relative_x = 0.;
    node.relative_y = 0.;
}

impl TidyLayout {
    fn invalidate_extreme_thread(&mut self, tree: &mut TreeGraph, node: NodeIndex) {
        self.set_extreme(tree, node);
        let e_left = tree.graph[node].tidy().extreme_left.unwrap();
        tree.graph[e_left].tidy_mut().thread_left = None;
        tree.graph[e_left].tidy_mut().thread_right = None;
        tree.graph[e_left].tidy_mut().modifier_thread_left = 0.;
        tree.graph[e_left].tidy_mut().modifier_thread_right = 0.;
        let e_right = tree.graph[node].tidy().extreme_right.unwrap();
        tree.graph[e_right].tidy_mut().thread_left = None;
        tree.graph[e_right].tidy_mut().thread_right = None;
        tree.graph[e_right].tidy_mut().modifier_thread_left = 0.;
        tree.graph[e_right].tidy_mut().modifier_thread_right = 0.;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::tidy_tree::node::{Node, TreeGraph};

    #[test]
    fn test_tidy_layout() {
        let mut tidy = TidyLayout::new(1., 1.);
        let mut tree = TreeGraph::new(Node::new(0, 1., 1.));
        let root = tree.root;

        let first_child = tree.add_child(root, Node::new(1, 1., 1.));
        let first_grandchild = tree.add_child(first_child, Node::new(10, 1., 1.));
        tree.add_child(first_grandchild, Node::new(100, 1., 1.));

        let second_child = tree.add_child(root, Node::new(2, 1., 1.));
        let second_grandchild = tree.add_child(second_child, Node::new(11, 1., 1.));
        tree.add_child(second_grandchild, Node::new(101, 1., 1.));

        tree.add_child(root, Node::new(3, 1., 2.));

        tidy.layout(&mut tree);
        println!("{}", tree.str());
    }
}
