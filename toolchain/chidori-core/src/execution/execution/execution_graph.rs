use crate::execution::execution::execution_state::{DependencyGraphMutation, ExecutionState};

use crate::execution::primitives::identifiers::{ArgumentIndex, OperationId};

use petgraph::data::Build;
use petgraph::dot::Dot;

use petgraph::graphmap::DiGraphMap;
use petgraph::visit::IntoEdgesDirected;
use petgraph::Direction;

// TODO: update all of these identifies to include a "space" they're within

type EdgeIdentity = (OperationId, OperationId, ArgumentIndex);

/// This models the network of reactive relationships between different components.
///
/// This is heavily inspired by works such as Salsa, Verde, Incremental, Adapton, and Differential Dataflow.
pub struct ExecutionGraph {
    /// Global revision number for modifications to the graph itself
    revision: usize,

    /// This is the graph of dependent execution state
    ///
    /// (branch, counter) -> steam_outputs_at_head
    /// The dependency graph is stored within the execution graph, allowing us to model changes
    /// to the dependency graph during the process of execution.
    /// This is a graph of the mutations to the dependency graph.
    /// As we make changes to the dependency graph itself, we track those transitions here.
    /// This is roughly equivalent to a git history of the dependency graph.
    ///
    /// We store immutable representations of the history of the dependency graph. These
    /// can be used to reconstruct a traversable dependency graph at any point in time.
    ///
    /// Identifiers on this graph refer to points in the execution graph. In execution terms, changes
    /// along those edges are always considered to have occurred _after_ the target step.
    execution_graph: DiGraphMap<(usize, usize), ExecutionState>,
}

impl ExecutionGraph {
    /// Initialize a new reactivity database. This will create a default input and output node,
    /// graphs default to being the unit function x -> x.
    pub fn new() -> Self {
        ExecutionGraph {
            execution_graph: Default::default(),
            revision: 0,
        }
    }

    fn add_execution_edge(
        &mut self,
        prev_execution_id: (usize, usize),
        new_state: ExecutionState,
        output_new_state: ExecutionState,
    ) -> ((usize, usize), ExecutionState) {
        let edges = self
            .execution_graph
            .edges_directed(prev_execution_id, Direction::Outgoing);

        let new_id = if let Some((_, max_to, _)) =
            edges.max_by(|(_, a_to, _), (_, b_to, _)| (a_to.0).cmp(&(b_to.0)))
        {
            // Create an edge in the execution graph from the previous state to this new one
            let id = (max_to.0 + 1, prev_execution_id.1 + 1);
            self.execution_graph
                .add_edge(prev_execution_id, id.clone(), new_state);
            id
        } else {
            // Create an edge in the execution graph from the previous state to this new one
            let id = (0, prev_execution_id.1 + 1);
            self.execution_graph
                .add_edge(prev_execution_id, id.clone(), new_state);
            id
        };

        (new_id, output_new_state)
    }

    pub fn render_execution_graph(&self) {
        println!("================ Execution graph ================");
        println!("{:?}", Dot::with_config(&self.execution_graph, &[]));
    }

    pub fn step_execution(
        &mut self,
        prev_execution_id: (usize, usize),
        previous_state: ExecutionState,
    ) -> ((usize, usize), ExecutionState) {
        let new_state = previous_state.step_execution();
        // The edge from this node is the greatest branching id + 1
        // if we re-evaluate execution at a given node, we get a new execution branch.
        self.add_execution_edge(prev_execution_id, new_state.clone(), new_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::primitives::serialized_value::RkyvSerializedValue as RSV;
    use crate::execution::primitives::serialized_value::{
        deserialize_from_buf, serialize_to_vec, ArchivedRkyvSerializedValue,
    };
    use log::warn;
    use rkyv::ser::serializers::AllocSerializer;
    use rkyv::ser::Serializer;
    use rkyv::{archived_root, Deserialize, Serialize};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /*
    Testing the execution of individual nodes. Validating that operations as defined can be executed.
     */

    #[test]
    fn test_evaluation_single_node() {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let mut state = state.add_operation(
            1,
            0,
            Box::new(|_args| {
                let v = RSV::Number(1);
                return serialize_to_vec(&v);
            }),
        );
        let mut state = state.add_operation(
            2,
            0,
            Box::new(|_args| {
                let v = RSV::Number(1);
                return serialize_to_vec(&v);
            }),
        );
        let mut state = state.add_operation(
            3,
            2,
            Box::new(|args| {
                let arg0 = deserialize_from_buf(args[0].as_ref().unwrap().as_slice());
                let arg1 = deserialize_from_buf(args[1].as_ref().unwrap().as_slice());

                if let (RSV::Number(a), RSV::Number(b)) = (arg0, arg1) {
                    let v = RSV::Number(a + b);
                    return serialize_to_vec(&v);
                }

                panic!("Invalid arguments")
            }),
        );

        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(1, 0), (2, 1)],
            }]);

        let v0 = RSV::Number(1);
        let v1 = RSV::Number(2);
        let arg0 = serialize_to_vec(&v0);
        let arg1 = serialize_to_vec(&v1);

        // Manually manipulating the state to insert the arguments for this test
        state.state_insert(1, Some(arg0));
        state.state_insert(2, Some(arg1));

        let (_, new_state) = db.step_execution(state_id, state.clone());

        assert!(new_state.state_get(&3).is_some());
        let result = new_state.state_get(&3).unwrap();
        let result_val = deserialize_from_buf(&result.as_ref().clone().unwrap());
        assert_eq!(result_val, RSV::Number(3));
    }

    /*
    Testing the traverse of the dependency graph. Validating that execution of the graph moves through
    the graph as expected.
     */

    #[test]
    fn test_traverse_single_node() {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        let mut state = state.add_operation(0, 0, Box::new(|_args| vec![0, 0, 0]));
        let mut state = state.add_operation(1, 0, Box::new(|_args| vec![1, 1, 1]));
        let mut state =
            state.apply_dependency_graph_mutations(vec![DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0)],
            }]);

        let (_, new_state) = db.step_execution(state_id, state);
        assert_eq!(new_state.state_get(&1).unwrap(), &Some(vec![1, 1, 1]));
    }

    #[test]
    fn test_traverse_linear_chain() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //    |
        //    2

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        let mut state = state.add_operation(0, 0, Box::new(|args| RSV::Number(0).into()));

        let mut state = state.add_operation(1, 1, Box::new(|args| RSV::Number(1).into()));

        let mut state = state.add_operation(2, 1, Box::new(|args| RSV::Number(2).into()));

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, 0)],
            },
        ]);

        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), Some(RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(2)));
    }

    #[test]
    fn test_traverse_branching() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        let mut state = state.add_operation(0, 0, Box::new(|args| RSV::Number(0).into()));
        let mut state = state.add_operation(1, 1, Box::new(|args| RSV::Number(1).into()));
        let mut state = state.add_operation(2, 1, Box::new(|args| RSV::Number(2).into()));
        let mut state = state.add_operation(3, 1, Box::new(|args| RSV::Number(3).into()));

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(1, 0)],
            },
        ]);

        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), Some(RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(2)));
        assert_eq!(state.state_get_value(&3), Some(RSV::Number(3)));
    }

    #[test]
    fn test_traverse_branching_and_convergence() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //   / \
        //  2   3
        //   \ /
        //    4

        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let mut state = state.add_operation(0, 0, Box::new(|args| RSV::Number(0).into()));
        let mut state = state.add_operation(1, 1, Box::new(|args| RSV::Number(1).into()));
        let mut state = state.add_operation(2, 1, Box::new(|args| RSV::Number(2).into()));
        let mut state = state.add_operation(3, 1, Box::new(|args| RSV::Number(3).into()));
        let mut state = state.add_operation(4, 2, Box::new(|args| RSV::Number(4).into()));

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(1, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 4,
                depends_on: vec![(2, 0), (3, 1)],
            },
        ]);

        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), Some(RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(2)));
        assert_eq!(state.state_get_value(&3), Some(RSV::Number(3)));
        assert_eq!(state.state_get(&4), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get_value(&4), Some(RSV::Number(4)));
    }

    #[test]
    fn test_traverse_cycle() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure _with the following cycle_
        //    0
        //    |
        //    1 * depends 1 -> 3
        //   / \
        //  2   3
        //   \ / * depends 3 -> 4
        //    4
        //    |
        //    5

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        // We start with the number 1 at node 0
        let mut state = state.add_operation(
            0,
            0,
            Box::new(|_args| {
                let v = RSV::Number(1);
                return serialize_to_vec(&v);
            }),
        );

        // Each node adds 1 to the inbound item (all nodes only have one dependency per index)
        let f1 = |args: Vec<&Option<Vec<u8>>>| {
            let arg0 = deserialize_from_buf(args[0].as_ref().unwrap().as_slice());

            if let RSV::Number(a) = arg0 {
                let v = RSV::Number(a + 1);
                return serialize_to_vec(&v);
            }

            panic!("Invalid arguments")
        };

        let mut state = state.add_operation(1, 1, Box::new(f1));
        let mut state = state.add_operation(2, 1, Box::new(f1));
        let mut state = state.add_operation(3, 1, Box::new(f1));
        let mut state = state.add_operation(4, 1, Box::new(f1));
        let mut state = state.add_operation(5, 1, Box::new(f1));
        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0), (3, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 3,
                depends_on: vec![(4, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 4,
                depends_on: vec![(2, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 5,
                depends_on: vec![(4, 0)],
            },
        ]);

        // We expect to see the value at each node increment repeatedly.
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);

        assert_eq!(state.state_get_value(&1), Some(RSV::Number(2)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(3)));
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get(&5), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get_value(&4), Some(RSV::Number(4)));
        assert_eq!(state.state_get(&5), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get_value(&3), Some(RSV::Number(5)));
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get_value(&5), Some(RSV::Number(5)));
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), Some(RSV::Number(6)));
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get_value(&5), Some(RSV::Number(5)));
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(7)));
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get_value(&5), Some(RSV::Number(5)));
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get(&3), None);
        assert_eq!(state.state_get_value(&4), Some(RSV::Number(8)));
        assert_eq!(state.state_get_value(&5), Some(RSV::Number(5)));
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        assert_eq!(state.state_get_value(&3), Some(RSV::Number(9)));
        assert_eq!(state.state_get(&4), None);
        assert_eq!(state.state_get_value(&5), Some(RSV::Number(9)));
    }

    #[test]
    fn test_branching_multiple_state_paths() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1
        //    |
        //    2

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        // We start with the number 1 at node 0
        let mut state = state.add_operation(
            0,
            0,
            Box::new(|_args| {
                let v = RSV::Number(1);
                return serialize_to_vec(&v);
            }),
        );

        // Globally mutates this value, making each call to this function side-effecting
        static atomic_usize: AtomicUsize = AtomicUsize::new(0);
        let f_side_effect = |args: Vec<&Option<Vec<u8>>>| {
            let arg0 = deserialize_from_buf(args[0].as_ref().unwrap().as_slice());

            if let RSV::Number(a) = arg0 {
                let plus = atomic_usize.fetch_add(1, Ordering::SeqCst);
                let v = RSV::Number(a + plus as i32);
                return serialize_to_vec(&v);
            }

            panic!("Invalid arguments")
        };

        let mut state = state.add_operation(1, 1, Box::new(f_side_effect));
        let mut state = state.add_operation(2, 1, Box::new(f_side_effect));

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, 0)],
            },
        ]);

        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get(&2), None);
        let (x_state_id, x_state) = db.step_execution(state_id, state);
        assert_eq!(x_state.state_get_value(&1), Some(RSV::Number(1)));
        assert_eq!(x_state.state_get(&2), None);

        let (state_id, state) = db.step_execution(x_state_id.clone(), x_state.clone());
        assert_eq!(state_id.0, 0);
        assert_eq!(state.state_get(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(2)));

        // When we re-evaluate from a previous point, we should get a new branch
        let (state_id, state) = db.step_execution(x_state_id.clone(), x_state);
        // The state_id.0 being incremented indicates that we're on a new branch
        assert_eq!(state_id.0, 1);
        assert_eq!(state.state_get(&1), None);
        // Op 2 should re-evaluate to 3, since it's on a new branch but continuing to mutate the stateful counter
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(3)));
    }

    #[test]
    fn test_mutation_of_the_dependency_graph_on_branches() {
        let mut db = ExecutionGraph::new();

        // Nodes are in this structure
        //    0
        //    |
        //    1 * we're going to be changing the definiton of the function of this node on one branch
        //    |
        //    2

        let mut state = ExecutionState::new();
        let state_id = (0, 0);

        // We start with the number 0 at node 0
        let mut state = state.add_operation(
            0,
            0,
            Box::new(|_args| {
                let v = RSV::Number(0);
                return serialize_to_vec(&v);
            }),
        );

        let f_v1 = |args: Vec<&Option<Vec<u8>>>| {
            let arg0 = deserialize_from_buf(args[0].as_ref().unwrap().as_slice());

            if let RSV::Number(a) = arg0 {
                let v = RSV::Number(a + 1);
                return serialize_to_vec(&v);
            }

            panic!("Invalid arguments")
        };

        let f_v2 = |args: Vec<&Option<Vec<u8>>>| {
            let arg0 = deserialize_from_buf(args[0].as_ref().unwrap().as_slice());

            if let RSV::Number(a) = arg0 {
                let v = RSV::Number(a + 200);
                return serialize_to_vec(&v);
            }

            panic!("Invalid arguments")
        };

        let mut state = state.add_operation(1, 1, Box::new(f_v1));
        let mut state = state.add_operation(2, 1, Box::new(f_v1));

        let mut state = state.apply_dependency_graph_mutations(vec![
            DependencyGraphMutation::Create {
                operation_id: 1,
                depends_on: vec![(0, 0)],
            },
            DependencyGraphMutation::Create {
                operation_id: 2,
                depends_on: vec![(1, 0)],
            },
        ]);

        let (x_state_id, mut x_state) = db.step_execution(state_id, state);
        assert_eq!(x_state.state_get(&1), None);
        assert_eq!(x_state.state_get(&2), None);
        let (state_id, state) = db.step_execution(x_state_id, x_state.clone());
        assert_eq!(state.state_get_value(&1), Some(RSV::Number(1)));
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(2)));

        // Change the definition of the operation "1" to add 200 instead of 1, then re-evaluate
        let mut state = x_state.add_operation(1, 1, Box::new(f_v2));
        let (state_id, state) = db.step_execution(state_id, state.clone());
        assert_eq!(state.state_get_value(&1), Some(RSV::Number(200)));
        assert_eq!(state.state_get(&2), None);
        let (state_id, state) = db.step_execution(state_id, state);
        assert_eq!(state.state_get_value(&1), None);
        assert_eq!(state.state_get_value(&2), Some(RSV::Number(201)));
    }

    #[test]
    fn test_conditional_propagation_to_children_slots() {
        // TODO: we should add support for propgating only to a given child under certain circumstances
    }

    #[test]
    fn test_composition_across_nodes() {
        // TODO: take two operators, and compose them into a single operator
    }
}
