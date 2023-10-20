use crate::execution::primitives::identifiers::{ArgumentIndex, OperationId};
use crate::execution::primitives::operation::{OperationFn, OperationNode};
use crate::execution::primitives::serialized_value::{
    deserialize_from_buf, RkyvSerializedValue as RSV,
};
use im::{HashMap as ImHashMap, HashSet as ImHashSet};
use indoc::indoc;
use petgraph::dot::{Config, Dot};
use petgraph::graphmap::DiGraphMap;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Formatter;
use std::rc::Rc;

pub enum DependencyGraphMutation {
    Create {
        operation_id: OperationId,
        depends_on: Vec<(OperationId, ArgumentIndex)>,
    },
    Delete {
        operation_id: OperationId,
    },
}

#[derive(Clone)]
pub struct ExecutionState {
    // TODO: update all operations to use this id instead of a separate representation
    id: (usize, usize),

    state: ImHashMap<usize, Rc<Option<Vec<u8>>>>,

    pub operation_by_id: ImHashMap<OperationId, Rc<RefCell<OperationNode>>>,

    /// Note what keys have _ever_ been set, which is an optimization to avoid needing to do
    /// a complete historical traversal to verify that a value has been set.
    has_been_set: ImHashSet<usize>,

    /// Dependency graph of the computable elements in the graph
    ///
    /// The dependency graph is a directed graph where the nodes are the ids of the operations and the
    /// weights are the index of the input of the next operation.
    ///
    /// The usize::MAX index is a no-op that indicates that the operation is ready to run, an execution
    /// order dependency rather than a value dependency.
    dependency_graph: ImHashMap<OperationId, HashSet<(OperationId, ArgumentIndex)>>,
}

impl std::fmt::Debug for ExecutionState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&render_map_as_table(self))
    }
}

fn render_map_as_table(exec_state: &ExecutionState) -> String {
    let mut table = String::from(indoc!(
        r"
            | Key | Value |
            |---|---|"
    ));

    for key in exec_state.state.keys() {
        if let Some(val) = exec_state.state_get_value(key) {
            table.push_str(&format!(
                indoc!(
                    r"
                | {} | {:?} |"
                ),
                key, val,
            ));
        }
    }

    table
}

impl ExecutionState {
    pub fn new() -> Self {
        ExecutionState {
            id: (0, 0),
            state: Default::default(),
            operation_by_id: Default::default(),
            has_been_set: Default::default(),
            dependency_graph: Default::default(),
        }
    }

    pub fn state_get_value(&self, operation_id: &OperationId) -> Option<RSV> {
        self.state_get(operation_id)
            .map(|x| deserialize_from_buf(x.as_ref().unwrap()))
    }

    pub fn state_get(&self, operation_id: &OperationId) -> Option<&Option<Vec<u8>>> {
        self.state.get(operation_id).map(|x| x.as_ref())
    }

    pub fn check_if_previously_set(&self, operation_id: &OperationId) -> bool {
        self.has_been_set.contains(operation_id)
    }

    pub fn state_consume_marked(&mut self, marked_for_consumption: HashSet<usize>) {
        for key in marked_for_consumption.clone().into_iter() {
            self.state.remove(&key);
        }
    }

    pub fn state_insert(&mut self, operation_id: OperationId, value: Option<Vec<u8>>) {
        self.state.insert(operation_id, Rc::new(value));
        self.has_been_set.insert(operation_id);
    }

    pub fn render_dependency_graph(&self) {
        println!("================ Dependency graph ================");
        println!(
            "{:?}",
            Dot::with_config(&self.get_dependency_graph(), &[Config::EdgeNoLabel])
        );
    }

    pub fn get_dependency_graph(&self) -> DiGraphMap<OperationId, Vec<ArgumentIndex>> {
        let mut graph = DiGraphMap::new();
        for (node, value) in self.dependency_graph.clone().into_iter() {
            graph.add_node(node);
            for (depends_on, index) in value.into_iter() {
                let r = graph.add_edge(depends_on, node, vec![index]);
                if r.is_some() {
                    graph
                        .edge_weight_mut(depends_on, node)
                        .unwrap()
                        .append(&mut r.unwrap());
                }
            }
        }
        graph
    }

    pub fn add_operation(&mut self, node: usize, args: usize, func: Box<OperationFn>) -> Self {
        let mut s = self.clone();
        s.operation_by_id.insert(
            node.clone(),
            Rc::new(RefCell::new(OperationNode::new(args, Some(func)))),
        );
        s
    }

    pub fn apply_dependency_graph_mutations(
        &self,
        mutations: Vec<DependencyGraphMutation>,
    ) -> Self {
        let mut s = self.clone();
        for mutation in mutations {
            match mutation {
                DependencyGraphMutation::Create {
                    operation_id,
                    depends_on,
                } => {
                    if let Some(e) = s.dependency_graph.get_mut(&operation_id) {
                        e.clear();
                        e.extend(depends_on.into_iter());
                    } else {
                        s.dependency_graph
                            .entry(operation_id)
                            .or_insert(HashSet::from_iter(depends_on.into_iter()));
                    }
                }
                DependencyGraphMutation::Delete { operation_id } => {
                    s.dependency_graph.remove(&operation_id);
                }
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {}
