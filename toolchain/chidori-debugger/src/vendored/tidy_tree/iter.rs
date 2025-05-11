use crate::tidy_tree::Node;

pub struct Iter<'a> {
    nodes: Vec<&'a Node>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        self.nodes.pop()
    }
}

