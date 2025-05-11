use crate::tidy_tree::{geometry::Coord, node::Node};
use std::ptr::NonNull;
use petgraph::graph::NodeIndex;

mod basic_layout;
mod linked_y_list;
pub(crate) mod tidy_layout;

pub use basic_layout::{BasicLayout, BoundingBox};
pub use tidy_layout::TidyLayout;
pub use tidy_layout::Orientation;
use crate::tidy_tree::node::TreeGraph;

pub trait Layout {
    fn layout(&mut self, tree: &mut TreeGraph);
    fn partial_layout(&mut self, tree: &mut TreeGraph, changed: &[NodeIndex]);
    fn parent_child_margin(&self) -> Coord;
    fn peer_margin(&self) -> Coord;
}
