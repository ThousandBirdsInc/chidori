#![allow(dead_code, unused_imports, unused_variables)]
pub mod geometry;
mod iter;
mod layout;
mod node;
mod utils;
pub use iter::Iter;
use std::{any::Any, collections::HashMap, ptr::NonNull};

use geometry::Coord;
use layout::BoundingBox;
pub use layout::{BasicLayout, Layout, TidyLayout};
pub use node::Node;

#[derive(PartialEq, Eq)]
pub enum LayoutType {
    Basic,
    Tidy,
    LayeredTidy,
}

pub struct TidyTree {
    root: Node,
    layout_type: LayoutType,
    layout: Box<dyn Layout>,
    map: HashMap<usize, NonNull<Node>>,
}

impl TidyTree {
    pub fn with_basic_layout(parent_child_margin: Coord, peer_margin: Coord) -> Self {
        TidyTree {
            layout_type: LayoutType::Basic,
            root: Default::default(),
            layout: Box::new(BasicLayout {
                parent_child_margin,
                peer_margin,
            }),
            map: HashMap::new(),
        }
    }

    pub fn with_tidy_layout(parent_child_margin: Coord, peer_margin: Coord) -> Self {
        TidyTree {
            layout_type: LayoutType::Tidy,
            root: Default::default(),
            layout: Box::new(TidyLayout::new(parent_child_margin, peer_margin)),
            map: HashMap::new(),
        }
    }

    pub fn with_layered_tidy(parent_child_margin: Coord, peer_margin: Coord) -> Self {
        TidyTree {
            layout_type: LayoutType::Tidy,
            root: Default::default(),
            layout: Box::new(TidyLayout::new_layered(parent_child_margin, peer_margin)),
            map: HashMap::new(),
        }
    }

    pub fn change_layout(&mut self, layout_type: LayoutType) {
        if layout_type == self.layout_type {
            return;
        }

        let parent_child_margin = self.layout.parent_child_margin();
        let peer_margin = self.layout.peer_margin();
        match layout_type {
            LayoutType::Basic => {
                self.layout = Box::new(BasicLayout {
                    parent_child_margin,
                    peer_margin,
                });
            }
            LayoutType::Tidy => {
                self.layout = Box::new(TidyLayout::new(parent_child_margin, peer_margin));
            }
            LayoutType::LayeredTidy => {
                self.layout = Box::new(TidyLayout::new_layered(parent_child_margin, peer_margin))
            }
        }

        self.layout_type = layout_type;
    }

    pub fn is_empty(&self) -> bool {
        self.root.id == usize::MAX
    }

    pub fn add_node(&mut self, id: usize, width: Coord, height: Coord, parent_id: usize) {
        let node = Node::new(id, width, height);
        if self.is_empty() || parent_id == usize::MAX {
            self.root = node;
            self.map.insert(id, (&self.root).into());
            return;
        }

        let mut parent = *self.map.get(&parent_id).unwrap();
        let parent = unsafe { parent.as_mut() };

        let ptr = parent.append_child(node);
        self.map.insert(id, ptr);
    }

    pub fn remove_node(&mut self, id: usize) {
        if self.is_empty() {
            return;
        }

        if let Some(node) = self.map.get(&id) {
            let node = unsafe { &mut *node.as_ptr() };
            node.pre_order_traversal(|node| {
                self.map.remove(&node.id);
            });
            node.parent_mut().unwrap().remove_child(id);
        }
    }

    pub fn data(&mut self, id: &[usize], width: &[Coord], height: &[Coord], parent_id: &[usize]) {
        for (i, &id) in id.iter().enumerate() {
            let width = width[i];
            let height = height[i];
            let parent_id = parent_id[i];
            self.add_node(id, width, height, parent_id);
        }
    }

    pub fn layout(&mut self) {
        if self.is_empty() {
            return;
        }

        self.layout.layout(&mut self.root);
    }

    pub fn get_pos(&self) -> Vec<Coord> {
        let mut ans = vec![];
        for (id, node) in self.map.iter() {
            let node = unsafe { node.as_ref() };
            ans.push((*id) as Coord);
            ans.push(node.x);
            ans.push(node.y);
        }

        ans
    }
}

pub const NULL_ID: usize = usize::MAX;
