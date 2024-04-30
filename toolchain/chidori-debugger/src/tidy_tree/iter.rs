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

fn recursive_iter<'a>(node: &'a Node, nodes: &mut Vec<&'a Node>) {
    nodes.push(node);
    for child in node.children.iter() {
        recursive_iter(child, nodes);
    }
}

impl Node {
    #[inline]
    pub fn iter(&self) -> Iter {
        let mut nodes = Vec::new();
        recursive_iter(self, &mut nodes);
        nodes.reverse();
        Iter { nodes }
    }
}

#[cfg(test)]
mod iter_test {
    use super::*;

    #[test]
    fn test_node_iter() {
        let mut root = Node::new_with_child(0, 1., 1., Node::new(1, 2., 2.));
        assert_eq!(root.iter().count(), 2);
        root.append_child(Node::new(2, 3., 3.));
        assert_eq!(root.iter().count(), 3);
        root.append_child(Node::new(3, 3., 3.));
        assert_eq!(root.iter().count(), 4);
        root.children[2].append_child(Node::new(4, 3., 3.));
        assert_eq!(root.iter().count(), 5);

        for (i, node) in root.iter().enumerate() {
            assert_eq!(i, node.id);
        }
    }
}
