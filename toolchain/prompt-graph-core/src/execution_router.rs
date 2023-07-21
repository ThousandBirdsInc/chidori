use crate::build_runtime_graph::graph_parse::CleanedDefinitionGraph;
use crate::proto2::{ChangeValue, ChangeValueWithCounter, DispatchResult, Item, NodeWillExecute, Path, WrappedChangeValue};

pub trait ExecutionState {
    fn get_count_node_execution(&self, node: &[u8]) -> Option<u64>;
    fn inc_counter_node_execution(&mut self, node: &[u8]) -> u64;
    fn get_value(&self, address: &[u8]) -> Option<(u64, ChangeValue)>;
    fn set_value(&mut self, address: &[u8], counter: u64, value: ChangeValue);
}


/// This is used to evaluate if a newly introduced node should be immediately evaluated
/// against the state of the system.
pub fn evaluate_changes_against_node(
    state: &impl ExecutionState,
    paths_to_satisfy: &Vec<Vec<String>>
) -> Option<Vec<WrappedChangeValue>> {
    // for each of the matched nodes, we need to evaluate the query against the current state
    // check if the updated state object is satisfying all necessary paths for this query
    let mut satisfied_paths = vec![];

    for path in paths_to_satisfy {
        if let Some(change_value) = state.get_value(path.join(":").as_bytes()) {
            satisfied_paths.push(change_value.clone());
        }
    }

    if satisfied_paths.len() != paths_to_satisfy.len() { return None }

    Some(satisfied_paths.into_iter().map(|(counter, v)| WrappedChangeValue {
        monotonic_counter: counter,
        change_value: Some(v),
    }).collect())
}


/// This dispatch method is responsible for identifying which nodes should execute based on
/// a current key value state and a clean definition graph. It returns a list of nodes that
/// should be executed, and the path references that they were satisfied with. This exists in
/// the core implementation because it may be used in client code or our server. It will mutate
/// the provided ExecutionState to reflect the application of the provided change. Execution state
/// may internally persist records of what environment this change occurred in.
pub fn dispatch_and_mutate_state(
    clean_definition_graph: &CleanedDefinitionGraph,
    state: &mut impl ExecutionState,
    change_value_with_counter: &ChangeValueWithCounter
) -> DispatchResult {
    let g = clean_definition_graph;

    // TODO: dispatch with an vec![] address path should do what?

    // First pass we update the values present in the change
    for filled_value in &change_value_with_counter.filled_values {
        let filled_value_address = &filled_value.clone().path.unwrap().address;

        // In order to avoid double-execution of nodes, we need to check if the value has changed.
        // matching here means that the state we are assessing execution of has already bee applied to our state.
        // The state may have already been applied in a parent branch if the execution is taking place there as well.
        if let Some((prev_counter, _prev_change_value)) = state.get_value(filled_value_address.join(":").as_bytes()) {
            if prev_counter >= change_value_with_counter.monotonic_counter {
                // Value has not updated - skip this change reflecting it and continue to the next change
                continue
            }
        }

        state.set_value(
            filled_value_address.join(":").as_bytes().clone(),
            change_value_with_counter.monotonic_counter,
            filled_value.clone());

    }


    // node_executions looks like a list of node names and their inputs
    // Nodes may execute _multiple times_ in response to some changes that might occur.
    let mut node_executions: Vec<NodeWillExecute> = vec![];
    // Apply a second pass to resolve into nodes that should execute
    for filled_value in &change_value_with_counter.filled_values {
        let filled_value_address = &filled_value.clone().path.unwrap().address;

        // TODO: if we're subscribed to all of the outputs of a node this will-eval a lot
        // filter to nodes matched by the affected path -> name
        // nodes with no queries are referred to by the empty string (derived from empty vec![]) and are always matched
        if let Some(matched_node_names) = g.dispatch_table.get(filled_value_address.join(":").as_str()) {
            for node_that_should_exec in matched_node_names {
                if let Some(choice_paths_to_satisfy) = g.query_paths.get(node_that_should_exec) {
                    for (idx, opt_paths_to_satisfy) in choice_paths_to_satisfy.iter().enumerate() {
                        // TODO: NodeWillExecute should include _which_ query was satisfied
                        if let Some(paths_to_satisfy) = opt_paths_to_satisfy {
                            if let Some(change_values_used_in_execution)  = evaluate_changes_against_node(state, paths_to_satisfy) {
                                let node_will_execute = NodeWillExecute {
                                    source_node: node_that_should_exec.clone(),
                                    change_values_used_in_execution,
                                    matched_query_index: idx as u64
                                };
                                node_executions.push(node_will_execute);
                            }
                        } else {
                            // No paths to satisfy
                            // we've already executed this node, so we don't need to do it again
                            if state.get_count_node_execution(node_that_should_exec.as_bytes()).unwrap_or(0) > 0 {
                                continue;
                            }
                            node_executions.push(NodeWillExecute {
                                source_node: node_that_should_exec.clone(),
                                change_values_used_in_execution: vec![],
                                matched_query_index: idx as u64
                            });
                        }
                        state.inc_counter_node_execution(node_that_should_exec.as_bytes());

                    }

                }
            }
        }
    }

    // we only _tell_ what we think should happen. We don't actually do it.
    // it is up to the wrapping SDK what to do or not do with our information
    DispatchResult {
        operations: node_executions,
    }
}


#[cfg(test)]
mod tests {
    use crate::proto2::{File, item, ItemCore, OutputType, PromptGraphNodeEcho, Query};
    use crate::graph_definition::DefinitionGraph;
    use std::collections::HashMap;

    use super::*;

    #[derive(Debug)]
    pub struct TestState {
        value: HashMap<Vec<u8>, (u64, ChangeValue)>,
        node_executions: HashMap<Vec<u8>, u64>
    }
    impl TestState {
        fn new() -> Self {
            Self {
                value: HashMap::new(),
                node_executions: HashMap::new()
            }
        }
    }

    impl ExecutionState for TestState {
        fn inc_counter_node_execution(&mut self, node: &[u8]) -> u64 {
            let v = self.node_executions.entry(node.to_vec()).or_insert(0);
            *v += 1;
            *v
        }

        fn get_count_node_execution(&self, node: &[u8]) -> Option<u64> {
            self.node_executions.get(node).map(|x| *x)
        }

        fn get_value(&self, address: &[u8]) -> Option<(u64, ChangeValue)> {
            self.value.get(address).cloned()
        }

        fn set_value(&mut self, address: &[u8], counter: u64, value: ChangeValue) {
            self.value.insert(address.to_vec(), (counter, value));
        }
    }

    fn get_file_empty_query() -> File {
        File {
            id: "test".to_string(),
            nodes: vec![Item{
                core: Some(ItemCore {
                    name: "EmptyNode".to_string(),
                    queries: vec![Query{ query: None}],
                    output: Some(OutputType {
                        output: "type O {}".to_string(),
                    }),
                    output_tables: vec![],
                }),
                item: Some(item::Item::NodeEcho(PromptGraphNodeEcho {
                }))}],
        }
    }

    fn get_file() -> File {
        File {
            id: "test".to_string(),
            nodes: vec![Item{
                core: Some(ItemCore {
                    name: "".to_string(),
                    queries: vec![Query {
                        query: None,
                    }],
                    output: Some(OutputType {
                        output: "type O {} ".to_string(),
                    }),
                    output_tables: vec![]
                }),
                item: Some(item::Item::NodeEcho(PromptGraphNodeEcho {
                }))}],
        }
    }


    #[test]
    fn test_dispatch_with_file_and_change() {
        let mut state = TestState::new();
        let file = get_file();
        let d = DefinitionGraph::from_file(file);
        let g = CleanedDefinitionGraph::new(&d);
        let c =  ChangeValueWithCounter {
            filled_values: vec![],
            parent_monotonic_counters: vec![],
            monotonic_counter: 0,
            branch: 0,
            source_node: "".to_string(),
        };
        let result = dispatch_and_mutate_state(&g, &mut state, &c);
        assert_eq!(result.operations.len(), 0);
    }

    #[test]
    fn test_we_dispatch_nodes_that_have_no_query_once() {
        let mut state = TestState::new();
        let file = get_file_empty_query();
        let d = DefinitionGraph::from_file(file);
        let g = CleanedDefinitionGraph::new(&d);
        let c =  ChangeValueWithCounter {
            filled_values: vec![ChangeValue {
                path: Some(Path {
                    address: vec![],
                }),
                value: None,
                branch: 0,
            }],
            parent_monotonic_counters: vec![],
            monotonic_counter: 0,
            branch: 0,
            source_node: "EmptyNode".to_string(),
        };
        let result = dispatch_and_mutate_state(&g, &mut state, &c);
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0], NodeWillExecute {
            source_node: "EmptyNode".to_string(),
            change_values_used_in_execution: vec![],
            matched_query_index: 0
        });

        // Does not re-execute
        let result = dispatch_and_mutate_state(&g, &mut state, &c);
        assert_eq!(result.operations.len(), 0);
    }

}
