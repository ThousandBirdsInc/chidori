use num::Float;

use super::Layout;
use crate::tidy_tree::{geometry::Coord, node::Node};
use std::cmp::{max, min};

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
    fn layout(&mut self, root: &mut Node) {
        root.pre_order_traversal_mut(|node| {
            node.tidy = None;
            node.x = 0.;
            node.y = 0.;
            node.relative_x = 0.;
            node.relative_y = 0.;
        });
        root.post_order_traversal_mut(|node| {
            self.update_meta(node);
        });
        root.pre_order_traversal_mut(|node| {
            if let Some(mut parent) = node.parent {
                let parent = unsafe { parent.as_mut() };
                node.x = parent.x + node.relative_x;
                node.y = parent.y + node.relative_y;
            }
        });
    }

    fn partial_layout(&mut self, root: &mut Node, changed: &[std::ptr::NonNull<Node>]) {
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
    fn update_meta(&mut self, node: &mut Node) {
        node.bbox = BoundingBox {
            total_height: node.height,
            total_width: node.width,
        };
        let children: *mut _ = &mut node.children;
        let children = unsafe { &mut *children };
        let n = children.len() as Coord;
        if n > 0. {
            let mut temp_x = 0.;
            let mut max_height = 0.;
            let n = children.len();
            for (i, child) in children.iter_mut().enumerate() {
                child.relative_y = node.height + self.parent_child_margin;
                child.relative_x = temp_x + child.bbox.total_width / 2.;
                temp_x += child.bbox.total_width + self.peer_margin;
                max_height = Float::max(child.bbox.total_height, max_height);
            }

            let children_width = temp_x - self.peer_margin;
            let shift_x = -children_width / 2.;
            for child in children.iter_mut() {
                child.relative_x += shift_x;
            }

            node.bbox.total_width = Float::max(children_width, node.width);
            node.bbox.total_height = node.height + self.parent_child_margin + max_height;
        }
    }
}

#[cfg(test)]
mod basic_layout_test {
    use super::{BasicLayout, BoundingBox};
    use crate::tidy_tree::{layout::Layout, Node};

    #[test]
    fn easy_test_0() {
        let mut root = Node::new(0, 10., 10.);
        root.append_child(Node::new(1, 10., 10.));
        let mut second = Node::new(2, 10., 10.);
        second.append_child(Node::new(3, 10., 10.));
        root.append_child(second);
        root.append_child(Node::new(4, 10., 10.));
        let mut layout = BasicLayout {
            parent_child_margin: 10.,
            peer_margin: 5.,
        };
        layout.layout(&mut root);
        println!("{:#?}", root);
    }
}
