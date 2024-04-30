use std::collections::LinkedList;

use crate::tidy_tree::geometry::Coord;

pub struct LinkedYList {
    pub index: usize,
    y: Coord,
    next: Option<Box<LinkedYList>>,
}

impl LinkedYList {
    pub fn new(index: usize, y: Coord) -> Self {
        LinkedYList {
            index,
            y,
            next: None,
        }
    }

    pub fn bottom(&self) -> Coord {
        self.y
    }

    pub fn update(self, index: usize, y: Coord) -> Self {
        let mut node = self;
        while node.y <= y {
            if let Some(next) = node.next.take() {
                node = *next;
            } else {
                return LinkedYList {
                    index,
                    y,
                    next: None,
                };
            }
        }

        LinkedYList {
            index,
            y,
            next: Some(Box::new(node)),
        }
    }

    pub fn pop(mut self) -> Option<Self> {
        self.next.take().map(|next| *next)
    }
}
