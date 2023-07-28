use std::collections::{HashMap, HashSet};
use std::env;
use std::iter::zip;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use deno_core::anyhow;
use prost::Message;
use anyhow::Result;
use log::debug;
use sled::Tree;
use tokio::runtime::Runtime;
use prompt_graph_core::create_change_value;
use prompt_graph_core::execution_router::{dispatch_and_mutate_state, evaluate_changes_against_node, ExecutionState};
use prompt_graph_core::graph_definition::DefinitionGraph;
use prompt_graph_core::build_runtime_graph::graph_parse::CleanedDefinitionGraph;
use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter, DispatchResult, File, InputProposal, Item, MemoryAction, NodeWillExecute, PromptGraphConstant, PromptGraphMap, PromptGraphNodeCode, PromptGraphNodeCodeSourceCode, PromptGraphNodeComponent, PromptGraphNodeMemory, PromptGraphNodeObservation, PromptGraphNodePrompt, PromptGraphParameterNode, SerializedValue, SupportedEmebddingModel, SupportedSourceCodeLanguages, SupportedVectorDatabase, PromptGraphNodeCustom, PromptLibraryRecord, ItemCore};
use prompt_graph_core::proto2 as dsl;
use prompt_graph_core::proto2::{CounterWithPath, NodeWillExecuteOnBranch, Path};
use prompt_graph_core::proto2::item;
use prompt_graph_core::proto2::prompt_graph_node_code::Source;
use prompt_graph_core::proto2::prompt_graph_node_component::Transclusion;
use prompt_graph_core::proto2::serialized_value::Val;
use prompt_graph_core::templates::render_template_prompt;

use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tokio::sync::broadcast::Receiver;
use tokio::time;

use crate::db_operations::playback::get_is_playing_status;
use crate::db_operations::branches::{create_root_branch, get_branch};
use crate::db_operations::changes::get_next_pending_change_on_branch;
use crate::db_operations::changes::resolve_pending_change;
use crate::db_operations::state_path_storage::{state_get, state_get_count_node_execution, state_inc_counter_node_execution};
use crate::db_operations::state_path_storage::state_insert;
use crate::db_operations::changes::subscribe_to_pending_change_events;
use crate::db_operations::input_proposals_and_responses::scan_all_input_responses;
use crate::db_operations::input_proposals_and_responses::insert_input_proposal;
use crate::db_operations::changes::insert_new_change_value_with_counter;
use crate::db_operations::graph_mutations::{get_next_pending_graph_mutation_on_branch, resolve_pending_graph_mutation, subscribe_to_pending_graph_mutations};
use crate::db_operations::executing_nodes::{insert_will_execute, move_will_execute_event_to_complete_by_will_exec};
use crate::db_operations::prompt_library::resolve_all_partials;
use crate::db_operations::update_change_counter_for_branch;
use crate::runtime_nodes::{node_code, node_custom, node_loader, node_map, node_memory, node_prompt};


/// We preserve all internal state, this implementation includes
/// a reference to the point in time that the state is at, and a reference
/// branch identifier. This is a "Handler" rather than the state itself because
/// the state is stored in an underlying sled::Tree.
///
/// The InternalStateHandler acts as a lens over the underlying state. Filtering
/// to the current counter horizon and branch.
pub struct InternalStateHandler<'a> {
    pub tree: &'a sled::Tree,
    pub branch: u64,
    pub counter: u64,
}

impl<'a> ExecutionState for InternalStateHandler<'a> {
    fn get_count_node_execution(&self, node: &[u8]) -> Option<u64> {
        state_get_count_node_execution(self.tree, node, self.counter, self.branch)
    }

    fn inc_counter_node_execution(&mut self, node: &[u8]) -> u64 {
        state_inc_counter_node_execution(self.tree, node, self.counter, self.branch)
    }

    fn get_value(&self, address: &[u8]) -> Option<(u64, ChangeValue)> {
        state_get(self.tree, address, self.counter, self.branch)
    }

    fn set_value(&mut self, address: &[u8], counter: u64, value: ChangeValue) {
        state_insert(self.tree, address, counter, self.branch, value);
    }
}

#[derive(Debug)]
pub struct NodeExecutionContext<'a> {
    pub node_will_execute_on_branch: &'a NodeWillExecuteOnBranch,
    pub item_core: &'a ItemCore,
    pub item: &'a item::Item,
    pub namespaces: &'a HashSet<String>,
    pub template_partials: &'a HashMap<String, PromptLibraryRecord>,
    pub tree: &'a sled::Tree,
}


/// There are two counters used in the system:
/// 1. Head counter: this is a counter that is incremented every time a change is
///    introduced to the system, it represents the furthest point of execution in the system.
/// 2. Horizon counter: this counter represents the point of execution that is is represented
///    by the current internal state of the system.
#[derive(Debug)]
pub struct Executor {
    pub clean_definition_graph: CleanedDefinitionGraph,
    pub tree: sled::Tree,

    // These are used to retain long running tasks
    // pub scheduled_events: HashMap<u64, ScheduledEvent>,
    // pub running_code: HashMap<u64, CodeHandle>,
}

impl Executor {
    pub fn new(tree: sled::Tree) -> Self {
        create_root_branch(&tree);
        Self {
            clean_definition_graph: CleanedDefinitionGraph::zero(), // This is stateless/deterministic
            tree,
        }
    }

    /// Listen to signals from the db and execute when we have the appropriate conditions
    ///
    /// During execution, we emit an initialization to all nodes on creation. This is handled by
    /// nodes with no inputs as their first change.
    ///
    /// Run is invoked by the main loop of the executor. Each invocation takes a piece of work from
    /// the queue and resolves it. The work is either a node that is ready to execute or an input.
    #[tracing::instrument]
    pub async fn run(&mut self) -> Result<()> {
        // TODO: every run is a new initial branch, it depends on branches

        // TODO: if we play from a later counter, we need to cite what branch we expect to start from

        // TODO: this run is for a particular batch of initial graph mutations
        // TODO: run accepts a specific point in the history of graph mutations
        let mut tasks = vec![
            subscribe_to_pending_graph_mutations(&self.tree),
            subscribe_to_pending_change_events(&self.tree),
        ];

        // At initialization of a run, we always submit this no op change to the system.
        // This induces any None query nodes to execute.
        insert_new_change_value_with_counter(&self.tree, ChangeValueWithCounter {
            source_node: "__initialization__".to_string(),
            filled_values: vec![
                create_change_value(
                    vec![],
                    None,
                    0)
            ],
            parent_monotonic_counters: vec![],
            monotonic_counter: 0,
            branch: 0,
        });


        // TODO: Collapse all initial changes that are queued
        self.progress_mutations().await?;
        self.progress_changes().await?;
        loop {
            debug!("run loop exec");
            let completed = futures::future::select_all(&mut tasks).await;
            let event = completed.0.expect("task failed");
            let idx = completed.1;
            match idx {
                // Pending graph mutation
                0 => {
                    if let sled::Event::Insert { key, value } = event {
                        self.progress_mutations().await?;
                    }
                }
                // Pending change
                1 => {
                    if let sled::Event::Insert { key, value } = event {
                        self.progress_changes().await?;
                    }
                }
                _ => {}
            }
        }
    }

    /// At the start of execution or introduction of these nodes, they need to initialize some state in the system.
    /// Effectively speaking these nodes are executed in this way as soon as they are introduced.
    #[tracing::instrument]
    async fn handle_graph_mutations(&mut self, file: &mut File, branch: u64, counter: u64) -> Result<()> {
        debug!("handle_graph_mutations");

        // Merge each of the nodes into clean file
        let updated_nodes = self.clean_definition_graph.merge_file(file).unwrap();

        // If the node had no query, we need to insert a change value with no query
        // TODO: only for the introduced mutation
        // TODO: something is producing Items with null core and item
        for node in &file.nodes {
            let node_name = &node.core.as_ref().unwrap().name;

            // TODO: for each query path set for the node, evaluate the query against the current system state
            let query_paths = self.clean_definition_graph.query_paths.get(node_name).expect("Node should be guaranteed to exist").clone();
            for (idx, opt_query_path) in query_paths.iter().enumerate() {
                if let Some(query_paths) = opt_query_path {
                    let mut state = InternalStateHandler {
                        tree: &self.tree,
                        branch,
                        counter
                    };

                    if let Some(change_values_used_in_execution) = evaluate_changes_against_node(&mut state, &query_paths) {
                        insert_new_change_value_with_counter(&self.tree, ChangeValueWithCounter {
                            source_node: node_name.clone(),
                            filled_values: change_values_used_in_execution.iter().map(|x| x.change_value.clone().unwrap()).collect(),
                            parent_monotonic_counters: change_values_used_in_execution.iter().map(|x| x.monotonic_counter).collect(),
                            monotonic_counter: counter,
                            branch,
                        });
                    }
                } else {
                    // When processed this should induce nodes with no query to execute
                    // TODO: just submit the resulting evaluation directly
                    insert_new_change_value_with_counter(&self.tree, ChangeValueWithCounter {
                        source_node: node_name.clone(),
                        filled_values: vec![
                            create_change_value(
                                vec![],
                                None,
                                branch)
                        ],
                        parent_monotonic_counters: vec![],
                        monotonic_counter: counter,
                        branch,
                    });
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument]
    pub async fn progress_next_mutation(&mut self) -> Result<bool> {
        if let Some(is_playing) = get_is_playing_status(&self.tree) {
            if !is_playing {
                return Ok(false);
            }
        }
        if let Some(((branch, counter), mut file)) = get_next_pending_graph_mutation_on_branch(&self.tree, 0) {
            debug!("Found pending graph mutation: {:?}", file);
            resolve_pending_graph_mutation(&self.tree, branch, counter);
            self.handle_graph_mutations(&mut file, branch, counter).await?;
            Ok(true)
        } else {
            Ok(false)
        }

    }

    #[tracing::instrument]
    pub async fn progress_mutations(&mut self) -> Result<()> {
        while self.progress_next_mutation().await? { }
        Ok(())
    }

    #[tracing::instrument]
    pub async fn progress_next_change(&mut self) -> Result<bool> {
        if let Some(is_playing) = get_is_playing_status(&self.tree) {
            if !is_playing {
                return Ok(false);
            }
        }
        if let Some(change) = get_next_pending_change_on_branch(&self.tree, 0) {
            debug!("Found pending change: {:?}", change);
            resolve_pending_change(&self.tree, change.branch, change.monotonic_counter);
            self.exec_change(change.clone(), change.branch).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[tracing::instrument]
    pub async fn progress_changes(&mut self) -> Result<bool> {
        while self.progress_next_change().await? { }
        Ok(true)
    }

    /// Exec change receives the output of other nodes, represented as a ChangeValueWithCounter.
    /// This invokes the "dispatch_and_mutate_state" method from "core" which identifies the nodes to activate.
    /// Subsequently this invokes those nodes with that state, it may execute those nodes in any
    /// way it sees fit.
    #[tracing::instrument]
    async fn exec_change(&mut self, change_value_with_counter: ChangeValueWithCounter, branch: u64) -> Result<()> {
        debug!("Executing change: {:?}", change_value_with_counter);
        let mut state = InternalStateHandler {
            tree: &self.tree,
            branch,
            counter: change_value_with_counter.monotonic_counter
        };

        // Get the dispatch event for this change, what nodes should execute?
        // TODO: definition graph should be on a counter as well
        let dispatch = dispatch_and_mutate_state(
            &self.clean_definition_graph,
            &mut state,
            &change_value_with_counter);

        // TODO: this can be executed in parallel
        // Multiple nodes may be activated by this change, so we need to execute them all
        for node_will_execute in dispatch.operations {
            let node = self.clean_definition_graph.get_node(&node_will_execute.source_node);
            if node.is_none() { continue; }
            self.process_node_will_execute(branch, &node_will_execute, node.unwrap().clone()).await?;
        }
        Ok(())
    }

    // TODO: allow these to execute for arbitrarily long durations
    #[tracing::instrument]
    async fn process_node_will_execute(&mut self, branch: u64, node_will_execute: &NodeWillExecute, item: Item) -> Result<()> {
        debug!("Processing node will execute: {:?}", &node_will_execute);
        // Produce will execute event and the counter that represents progressing this execution
        // The event_execution_graph_executing_node handles the input and output graph.
        let name = &node_will_execute.source_node;

        // TODO: is it still useful to store output paths? they're definitely derivable
        // let output_paths = self.clean_definition_graph.output_paths.get(name).unwrap().clone();

        // Each node invocation is a single counter value - each output is a path from that counter
        let result_counter = update_change_counter_for_branch(&self.tree, branch).unwrap();

        // Custom nodes must extract type_name
        let node_type_name = if let Some(item::Item::NodeCustom(PromptGraphNodeCustom{type_name})) = &item.item {
            Some(type_name )
        } else {
            None
        };

        let node_will_execute_on_branch = NodeWillExecuteOnBranch {
            custom_node_type_name: node_type_name.cloned(),
            node: Some(node_will_execute.clone()),
            counter: result_counter,
            branch
        };
        insert_will_execute(&self.tree, node_will_execute_on_branch.clone());

        // Get counters used by all inputs
        let parent_monotonic_counters: Vec<u64> = node_will_execute
            .change_values_used_in_execution
            .iter()
            .map(|p| p.monotonic_counter).collect();

        let ctx = NodeExecutionContext {
            node_will_execute_on_branch: &node_will_execute_on_branch,
            item_core: &item.core.as_ref().unwrap(),
            item: &item.item.as_ref().unwrap(),
            namespaces: &self.clean_definition_graph.node_to_output_tables.get(name).unwrap().clone(),
            template_partials: &resolve_all_partials(&self.tree),
            tree: &self.tree,
        };
        debug!("Executing node with context: {:?}", &ctx);

        let filled_values = match (item.core.as_ref().unwrap(), item.item.as_ref().unwrap()) {
            (c, item::Item::Map(n)) => {
                node_map::execute_node_map(&ctx)
            }
            (c, item::Item::NodeCode(n)) => {
                node_code::node::execute_node_code(&ctx)
            }
            (c, item::Item::NodePrompt(n)) => {
                node_prompt::node::execute_node_prompt(&ctx).await
            }
            (c, item::Item::NodeMemory(n)) => {
                node_memory::node::execute_node_memory(&ctx).await?
            }
            (c, item::Item::NodeEcho(n)) => {
                let change_set: Vec<ChangeValue> = node_will_execute
                    .change_values_used_in_execution.iter().map(|x| x.change_value.as_ref().unwrap().clone()).collect();
                change_set
            }
            (c, item::Item::NodeLoader(n)) => {
                node_loader::node::execute_node_loader(&ctx)?
            }
            (c, item::Item::NodeCustom(n)) => {
                node_custom::execute_node_custom(&ctx).await?
            }
            (c, item::Item::NodeJoin(n)) => {
                vec![]
            },

            // item::Item::NodeComponent(n) => {
            //     // TODO: execute subgraph
            // }
            _ => {
                vec![]
            }
        };

        let new_change = ChangeValueWithCounter {
            source_node: node_will_execute.source_node.clone(),
            filled_values,
            parent_monotonic_counters,
            monotonic_counter: result_counter,
            branch
        };

        debug!("Inserting new ChangeValueWithCounter {:?}", &new_change);
        move_will_execute_event_to_complete_by_will_exec(&self.tree, &node_will_execute_on_branch);
        insert_new_change_value_with_counter(&self.tree, new_change);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use indoc::indoc;
    use sled::Config;
    use prompt_graph_core::graph_definition::{create_code_node, SourceNodeType};
    use prompt_graph_core::proto2::{File, ItemCore};
    use prompt_graph_core::proto2::Item;
    use prompt_graph_core::proto2::OutputType;
    use prompt_graph_core::proto2::prompt_graph_node_code;
    use prompt_graph_core::proto2::PromptGraphNodeEcho;
    use prompt_graph_core::proto2::Query;
    use prompt_graph_core::proto2::PromptGraphNodeCodeSourceCode;
    use prompt_graph_core::proto2::SupportedSourceCodeLanguages;

    use crate::db_operations::get_change_counter_for_branch;
    use crate::db_operations::branches::{create_branch, list_branches};
    use crate::db_operations::changes::scan_all_pending_changes;
    use crate::db_operations::state_path_storage::{debug_scan_all_state_branch, debug_scan_all_state_counters};
    use crate::db_operations::graph_mutations::insert_pending_graph_mutation;
    use crate::db_operations::playback::{pause_execution_at_frame, play_execution_at_frame};

    use super::*;

    fn gen_item_hello() -> Item {
        create_code_node(
            "code_node_test".to_string(),
            vec![None],
            r#" { output: String } "#.to_string(),
            SourceNodeType::Code(String::from("DENO"),
                 indoc! { r#"
                            return {
                                "output": "hello"
                            }
                        "#}.to_string(),
                false
            ),
            vec![]
        )
    }

    fn gen_item_hello_plus_world() -> Item {
        create_code_node(
            "code_node_test_dep".to_string(),
            vec![Some( r#" SELECT output FROM code_node_test "#.to_string(),
            )],
            r#"{ result: String }"#.to_string(),
            SourceNodeType::Code(
                String::from("DENO"),
                indoc! { r#"
                    return {
                        "result": "{{code_node_test.output}}" + " world"
                    }
                "#}.to_string(),
                true
            ),
            vec![]
        )
    }

    fn gen_item_non_deterministic() -> Item {
        create_code_node(
            "code_node_test_dep".to_string(),
            vec![Some( r#"SELECT output FROM code_node_test"#.to_string(),
            )],
            r#"{ result: String }"#.to_string(),
            SourceNodeType::Code(
                String::from("DENO"), indoc! { r#"
                return {
                    "result": "{{code_node_test.output}}" + Math.random()
                }
            "#}.to_string(),
                true
            ),
            vec![]
        )


    }

    #[tokio::test]
    async fn test_pushing_mutation_to_file() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let mut executor = Executor::new(tree);
        let file_with_mutation = File {
            id: "test".to_string(),
            nodes: vec![Item{
                core: Some(ItemCore {
                    name: "new_echo_node".to_string(),
                    queries: vec![Query {
                        query: None,
                    }],
                    output: Some(OutputType {
                        output: "{ echo: String }".to_string(),
                    }),
                    output_tables: vec![]
                }),
                item: Some(item::Item::NodeEcho(PromptGraphNodeEcho { }))}],
        };
        insert_pending_graph_mutation(&executor.tree, 0, file_with_mutation);
        executor.progress_changes().await;
        executor.progress_mutations().await;
        assert_eq!(executor.clean_definition_graph.node_by_name.len(), 1);
        assert_eq!(executor.clean_definition_graph.node_by_name.contains_key("new_echo_node"), true)
    }


    #[tokio::test]
    async fn test_generation_of_change_value_from_no_query_node_at_initialization() {
        env_logger::init();
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let mut file = File {
            id: "test".to_string(),
            nodes: vec![gen_item_hello()]
        };
        let mut executor = Executor::new(tree);
        executor.handle_graph_mutations(&mut file, 0, 0).await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        dbg!(&v);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].monotonic_counter, 0);
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        dbg!(&v);
        assert_eq!(v[0].monotonic_counter, 1);
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address[0], "code_node_test".to_string());
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address[1], "output".to_string());
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        dbg!(&v);
        assert_eq!(v.len(), 0);
    }


    #[tokio::test]
    async fn test_dependencies_between_nodes() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let mut file = File {
            id: "test".to_string(),
            nodes: vec![
                gen_item_hello(),
                gen_item_hello_plus_world()
            ],
        };
        let mut executor = Executor::new(tree);
        assert_eq!(&executor.clean_definition_graph.dispatch_table, &HashMap::new());
        executor.handle_graph_mutations(&mut file, 0, 0).await.unwrap();
        assert_eq!(&executor.clean_definition_graph.dispatch_table, &(vec![
            ("".to_string(), vec!["code_node_test".to_string()]),
            ("code_node_test:output".to_string(), vec!["code_node_test_dep".to_string()])]
            .iter()
            .cloned()
            .collect::<HashMap<String, Vec<String>>>())
        );
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        assert_eq!(v[0].filled_values[0].value.as_ref().unwrap().val.as_ref().unwrap(), &Val::String("hello world".to_string()));
    }


    // TODO: mutation of the file during execution - when we introduce a query, it should be evaluated based on existing state
    // TODO: will need to clarify "RENAME", "CREATE", "UPDATE"
    // TODO: mutations _always_ result in new branches
    #[tokio::test]
    async fn test_mutation_of_a_file_during_execution() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let mut file = File {
            id: "test".to_string(),
            nodes: vec![gen_item_hello()],
        };
        let mut executor = Executor::new(tree);
        executor.handle_graph_mutations(&mut file, 0, 0).await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].monotonic_counter, 0);
        assert_eq!(v[0].source_node, "code_node_test".to_string(), "This should be the initialization change for the node");
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address, Vec::<String>::new());
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        assert_eq!(v[0].monotonic_counter, 1);
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address[0], "code_node_test".to_string());
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address[1], "output".to_string());
        assert_eq!(v[0].filled_values[0].value.as_ref().unwrap().val.as_ref().unwrap(), &Val::String("hello".to_string()));
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        // TODO: I think that the state of node execution is being cleared when a mutation to the file occurs
        assert_eq!(v.len(), 0);
        let mutation_file = File {
            id: "test".to_string(),
            nodes: vec![
                gen_item_hello_plus_world()
            ],
        };
        debug_scan_all_state_counters(&executor.tree,0 );
        insert_pending_graph_mutation(&executor.tree, 0, mutation_file);

        // Progress the next mutation to the file adding the hello plus world
        executor.progress_next_mutation().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        assert_eq!(v.len(), 1);


        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        debug_scan_all_state_counters(&executor.tree,0 );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].filled_values[0].value.as_ref().unwrap().val.as_ref().unwrap(), &Val::String("hello world".to_string()));
        // TODO: on introduction of a new node, it should be evaluated based on existing state at that frame
    }


    // TODO: validate pause functionality - no changes should be processed while paused
    #[tokio::test]
    async fn test_pause_at_counter() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let mut file = File {
            id: "test".to_string(),
            nodes: vec![
                gen_item_hello(),
                gen_item_hello_plus_world()
            ],
        };
        let mut executor = Executor::new(tree);
        executor.handle_graph_mutations(&mut file, 0, 0).await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();

        // Pause the execution at frame 0
        pause_execution_at_frame(&executor.tree, 0);

        // Should do nothing
        assert_eq!(executor.progress_next_change().await.unwrap(), false);
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(executor.progress_next_change().await.unwrap(), false);
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(executor.progress_next_change().await.unwrap(), false);
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();
        // changes should have a single item that is the initialization of the file
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].source_node, "code_node_test".to_string());
        assert_eq!(v[0].monotonic_counter, 0);

        // Play execution again
        play_execution_at_frame(&executor.tree, 0);

        // Should process the first frame and get result of code_node_test
        executor.progress_next_change().await;
        let v: Vec<_> = scan_all_pending_changes(&executor.tree).collect();

        assert_eq!(v.len(), 1);
        assert_eq!(v[0].monotonic_counter, 1);
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address[0], "code_node_test".to_string());
        assert_eq!(v[0].filled_values[0].path.as_ref().unwrap().address[1], "output".to_string());
    }


    // TODO: execution order of nodes and re-execution
}
