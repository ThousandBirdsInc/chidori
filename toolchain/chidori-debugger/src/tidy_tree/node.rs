use std::{collections::VecDeque, ptr::NonNull};

use crate::tidy_tree::{geometry::Coord, layout::BoundingBox};

#[derive(Debug)]
pub struct TidyData {
    pub thread_left: Option<NonNull<Node>>,
    pub thread_right: Option<NonNull<Node>>,
    /// ```text
    /// this.extreme_left == this.thread_left.extreme_left ||
    /// this.extreme_left == this.children[0].extreme_left
    /// ```
    pub extreme_left: Option<NonNull<Node>>,
    /// ```text
    /// this.extreme_right == this.thread_right.extreme_right ||
    /// this.extreme_right == this.children[-1].extreme_right
    /// ```
    pub extreme_right: Option<NonNull<Node>>,

    /// Cached change of x position.
    pub shift_acceleration: Coord,
    /// Cached change of x position
    pub shift_change: Coord,

    /// this.x = parent.x + modifier_to_subtree
    pub modifier_to_subtree: Coord,
    /// this.x + modifier_thread_left == thread_left.x
    pub modifier_thread_left: Coord,
    /// this.x + modifier_thread_right == thread_right.x
    pub modifier_thread_right: Coord,
    /// this.x + modifier_extreme_left == extreme_left.x
    pub modifier_extreme_left: Coord,
    /// this.x + modifier_extreme_right == extreme_right.x
    pub modifier_extreme_right: Coord,
}

#[derive(Debug)]
pub struct Node {
    pub id: usize,
    pub width: Coord,
    pub height: Coord,
    pub x: Coord,
    pub y: Coord,
    /// node x position relative to its parent
    pub relative_x: Coord,
    /// node y position relative to its parent
    pub relative_y: Coord,
    pub bbox: BoundingBox,
    pub parent: Option<NonNull<Node>>,
    /// Children need boxing to get a stable addr in the heap
    pub children: Vec<Box<Node>>,
    pub tidy: Option<Box<TidyData>>,
}

impl Clone for Node {
    fn clone(&self) -> Self {
        let mut root = Self {
            id: self.id,
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            relative_x: self.relative_x,
            relative_y: self.relative_y,
            bbox: self.bbox.clone(),
            parent: None,
            children: self.children.clone(),
            tidy: None,
        };

        if self.parent.is_none() {
            root.post_order_traversal_mut(|node| {
                let node_ptr = node.into();
                for child in node.children.iter_mut() {
                    child.parent = Some(node_ptr);
                }
            });
        }

        root
    }
}

impl Default for Node {
    fn default() -> Self {
        Self {
            id: usize::MAX,
            width: 0.,
            height: 0.,
            x: 0.,
            y: 0.,
            relative_x: 0.,
            relative_y: 0.,
            children: vec![],
            parent: None,
            bbox: Default::default(),
            tidy: None,
        }
    }
}

impl Node {
    pub fn new(id: usize, width: Coord, height: Coord) -> Self {
        Node {
            id,
            width,
            height,
            bbox: Default::default(),
            x: 0.,
            y: 0.,
            relative_x: 0.,
            relative_y: 0.,
            children: vec![],
            parent: None,
            tidy: None,
        }
    }

    pub fn depth(&self) -> usize {
        let mut depth = 0;
        let mut node = self;
        while node.parent.is_some() {
            node = node.parent().unwrap();
            depth += 1;
        }

        depth
    }

    pub fn parent_mut(&mut self) -> Option<&mut Self> {
        unsafe { self.parent.map(|mut node| node.as_mut()) }
    }

    pub fn parent(&self) -> Option<&Self> {
        unsafe { self.parent.map(|node| node.as_ref()) }
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

    fn reset_parent_link_of_children(&mut self) {
        if self.children.is_empty() {
            return;
        }

        let ptr = self.into();
        for child in self.children.iter_mut() {
            child.parent = Some(ptr);
        }
    }

    pub fn append_child(&mut self, mut child: Self) -> NonNull<Self> {
        child.parent = Some(self.into());
        let mut boxed = Box::new(child);
        boxed.reset_parent_link_of_children();
        let ptr = boxed.as_mut().into();
        self.children.push(boxed);
        ptr
    }

    pub fn new_with_child(id: usize, width: Coord, height: Coord, child: Self) -> Self {
        let mut node = Node::new(id, width, height);
        node.append_child(child);
        node
    }

    pub fn new_with_children(id: usize, width: Coord, height: Coord, children: Vec<Self>) -> Self {
        let mut node = Node::new(id, width, height);
        for child in children {
            node.append_child(child);
        }
        node
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.x - self.width / 2. < other.x + other.width / 2.
            && self.x + self.width / 2. > other.x - other.width / 2.
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    pub fn post_order_traversal<F>(&self, mut f: F)
    where
        F: FnMut(&Node),
    {
        let mut stack: Vec<(NonNull<Self>, bool)> = vec![(self.into(), true)];
        while let Some((mut node_ptr, is_first)) = stack.pop() {
            let node = unsafe { node_ptr.as_mut() };
            if !is_first {
                f(node);
                continue;
            }

            stack.push((node_ptr, false));
            for child in node.children.iter_mut() {
                stack.push((child.as_mut().into(), true));
            }
        }
    }

    pub fn post_order_traversal_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Node),
    {
        let mut stack: Vec<(NonNull<Self>, bool)> = vec![(self.into(), true)];
        while let Some((mut node_ptr, is_first)) = stack.pop() {
            let node = unsafe { node_ptr.as_mut() };
            if !is_first {
                f(node);
                continue;
            }

            stack.push((node_ptr, false));
            for child in node.children.iter_mut() {
                stack.push((child.as_mut().into(), true));
            }
        }
    }

    pub fn pre_order_traversal<F>(&self, mut f: F)
    where
        F: FnMut(&Node),
    {
        let mut stack: Vec<NonNull<Self>> = vec![self.into()];
        while let Some(mut node) = stack.pop() {
            let node = unsafe { node.as_mut() };
            f(node);
            for child in node.children.iter_mut() {
                stack.push(child.as_mut().into());
            }
        }
    }

    pub fn pre_order_traversal_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Node),
    {
        let mut stack: Vec<NonNull<Self>> = vec![self.into()];
        while let Some(mut node) = stack.pop() {
            let node = unsafe { node.as_mut() };
            f(node);
            for child in node.children.iter_mut() {
                stack.push(child.as_mut().into());
            }
        }
    }

    pub fn bfs_traversal_with_depth_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Node, usize),
    {
        let mut queue: VecDeque<(NonNull<Self>, usize)> = VecDeque::new();
        queue.push_back((self.into(), 0));
        while let Some((mut node, depth)) = queue.pop_front() {
            let node = unsafe { node.as_mut() };
            f(node, depth);
            for child in node.children.iter_mut() {
                queue.push_back((child.as_mut().into(), depth + 1));
            }
        }
    }

    pub fn remove_child(&mut self, id: usize) {
        let pos = self.children.iter().position(|node| node.id == id);
        if let Some(index) = pos {
            self.children.remove(index);
        }
    }

    pub fn pre_order_traversal_with_depth_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Node, usize),
    {
        let mut stack: Vec<(NonNull<Self>, usize)> = vec![(self.into(), 0)];
        while let Some((mut node, depth)) = stack.pop() {
            let node = unsafe { node.as_mut() };
            f(node, depth);
            for child in node.children.iter_mut() {
                stack.push((child.as_mut().into(), depth + 1));
            }
        }
    }

    pub fn str(&self) -> String {
        let mut s = String::new();
        if self.tidy.is_some() {
            s.push_str(&format!(
                "x: {}, y: {}, width: {}, height: {}, rx: {}, mod: {}, id: {}\n",
                self.x,
                self.y,
                self.width,
                self.height,
                self.relative_x,
                self.tidy().modifier_to_subtree,
                self.id
            ));
        } else {
            s.push_str(&format!(
                "x: {}, y: {}, width: {}, height: {}, rx: {}, id: {}\n",
                self.x, self.y, self.width, self.height, self.relative_x, self.id
            ));
        }
        for child in self.children.iter() {
            for line in child.str().split('\n') {
                if line.is_empty() {
                    continue;
                }

                s.push_str(&format!("    {}\n", line));
            }
        }

        s
    }
}
