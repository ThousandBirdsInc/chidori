use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::execution::execution::execution_state::ExecutionState;
use crate::execution::execution::DependencyGraphMutation;
use crate::cells::CellTypes;
use crate::execution::primitives::identifiers::DependencyReference;
use crate::execution::primitives::serialized_value::{
    RkyvSerializedValue as RKV, RkyvSerializedValue,
};
use crate::sdk::md::{interpret_code_block, load_folder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{fmt, thread};
use std::path::Path;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use crate::utils::telemetry::init_internal_telemetry;

/// This is an SDK for building execution graphs. It is designed to be used interactively.

type Func = fn(RKV) -> RKV;

#[derive(PartialEq, Debug)]
enum PlaybackState {
    Paused,
    Running,
}

pub struct InstancedEnvironment {
    db: ExecutionGraph,
    pub state: ExecutionState,
    state_id: (usize, usize),
    op_counter: usize,
    playback_state: PlaybackState,
    sender: Option<Sender<String>>,
}

impl std::fmt::Debug for InstancedEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstancedEnvironment")
            .finish()
    }
}

impl InstancedEnvironment {
    fn new() -> InstancedEnvironment {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let playback_state = PlaybackState::Paused;
        InstancedEnvironment {
            db,
            state,
            state_id,
            op_counter: 0,
            sender: None,
            playback_state
        }
    }

    /// Free-wheel execution of the graph
    #[tracing::instrument]
    pub fn run(&mut self) {
        self.playback_state = PlaybackState::Running;
        let _maybe_guard = self.sender.as_ref().map(|sender| {
            tracing::subscriber::set_default(init_internal_telemetry(sender.clone()))
        });
        loop {
            if self.playback_state == PlaybackState::Paused {
                break;
            }
            self.step();
        }
    }

    /// Increment the execution graph by one step
    #[tracing::instrument]
    pub(crate) fn step(&mut self) -> Vec<(usize, RkyvSerializedValue)> {
        let ((state_id, state), outputs) = self.db.step_execution(self.state_id, &self.state);
        self.state_id = state_id;
        self.state = state;
        outputs
    }

    /// Add a cell into the execution graph
    #[tracing::instrument]
    pub fn upsert_cell(&mut self, cell: CellTypes) -> usize {
        self.op_counter += 1;
        let id = self.op_counter;
        let mut op = match &cell {
            CellTypes::Code(c) => crate::cells::code_cell::code_cell(c),
            CellTypes::Prompt(c) => crate::cells::llm_prompt_cell::llm_prompt_cell(c),
            CellTypes::Web(c) => crate::cells::web_cell::web_cell(c),
            CellTypes::Template(c) => crate::cells::template_cell::template_cell(c),
        };
        op.attach_cell(cell);

        self.state = self.state.add_operation(id, op);
        // TODO: we collect and throw errors for: naming collisions, missing dependencies, and missing arguments

        // TODO: add a cell report to the execution engine, updating the execution graph
        // TODO: we need a model of dependencies between cells and the number of arguments they require

        self.op_counter
    }

    /// Resolve the set of dependencies currently available, making necessary changes to the operator graph
    #[tracing::instrument]
    pub fn resolve_dependencies_from_input_signature(&mut self) -> anyhow::Result<&ExecutionState> {
        // TODO: when there is a dependency on a function invocation we need to
        //       instantiate a new instance of the function operation node.
        //       It itself is not part of the call graph until it has such a dependency.

        let mut available_values = HashMap::new();
        let mut available_functions = HashMap::new();

        // For all reported cells, add their exposed values to the available values
        for (id, op) in self.state.operation_by_id.iter() {
            let output_signature = &op.lock().unwrap().signature.output_signature;

            // Store values that are available as globals
            for (key, value) in output_signature.globals.iter() {
                // TODO: throw an error if there is a naming collision
                available_values.insert(key.clone(), id);
            }

            for (key, value) in output_signature.functions.iter() {
                // TODO: throw an error if there is a naming collision
                available_functions.insert(key.clone(), id);
            }

            // TODO: Store triggerable functions that may be passed as values as well
        }

        // TODO: we need to report on INVOKED functions - these functions are calls to
        //       functions with the locals assigned in a particular way. But then how do we handle compositions of these?
        //       Well we just need to invoke them in the correct pattern as determined by operations in that context.

        // Anywhere there is a matched value, we create a dependency graph edge
        let mut mutations = vec![];
        for (destination_cell_id, op) in self.state.operation_by_id.iter() {
            let operation = op.lock().unwrap();
            let input_signature = &operation.signature.input_signature;
            for (value_name, value) in input_signature.globals.iter() {
                // TODO: we need to handle collisions between the two of these
                if let Some(source_cell_id) = available_functions.get(value_name) {
                    if source_cell_id != &destination_cell_id {
                        mutations.push(DependencyGraphMutation::Create {
                            operation_id: destination_cell_id.clone(),
                            depends_on: vec![(
                                *source_cell_id.clone(),
                                DependencyReference::FunctionInvocation(value_name.to_string()),
                            )],
                        });
                    }
                }

                if let Some(source_cell_id) = available_values.get(value_name) {
                    if source_cell_id != &destination_cell_id {
                        mutations.push(DependencyGraphMutation::Create {
                            operation_id: destination_cell_id.clone(),
                            depends_on: vec![(
                                *source_cell_id.clone(),
                                DependencyReference::Global(value_name.to_string()),
                            )],
                        });
                    }
                }
            }
        }

        self.state = self.state.apply_dependency_graph_mutations(mutations);
        Ok(&self.state)
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}
}

pub struct Chidori {
    loaded_cells: HashMap<usize, Vec<CellTypes>>,
    load_counter: usize,
    sender: Option<Sender<String>>,
}

impl std::fmt::Debug for Chidori {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Environment")
            .finish()
    }
}

impl Chidori {
    pub fn new() -> Self {
        Chidori {
            loaded_cells: HashMap::new(),
            load_counter: 0,
            sender: None,
        }
    }

    pub fn new_with_events(sender: Sender<String>) -> Self {
        Chidori {
            loaded_cells: HashMap::new(),
            load_counter: 0,
            sender: Some(sender),
        }
    }

    pub fn load_md_string(&mut self, s: &str) -> anyhow::Result<()> {
        let mut cells = vec![];
        crate::sdk::md::extract_code_blocks(s)
            .iter()
            .filter_map(|block| interpret_code_block(block))
            .for_each(|block| { cells.push(block); });
        self.load_counter += 1;
        self.loaded_cells.insert(self.load_counter, cells);
        Ok(())
    }

    pub fn load_md_directory(&mut self, path: &Path) -> anyhow::Result<()> {
        let files = load_folder(path)?;
        let mut cells = vec![];
        for file in files {
            for block in file.result {
                if let Some(block) = interpret_code_block(&block) {
                    cells.push(block);
                }
            }
        }
        self.load_counter += 1;
        self.loaded_cells.insert(self.load_counter, cells);
        Ok(())
    }

    pub fn get_instance(&self) -> anyhow::Result<InstancedEnvironment> {
        if let Some(cells) = self.loaded_cells.get(&self.load_counter) {
            let mut db = ExecutionGraph::new();
            let mut state = ExecutionState::new();
            let state_id = (0, 0);
            let playback_state = PlaybackState::Paused;
            let mut e = InstancedEnvironment {
                db,
                state,
                state_id,
                op_counter: 0,
                sender: self.sender.clone(),
                playback_state
            };
            for cell in cells {
                e.upsert_cell(cell.clone());
            }
            e.resolve_dependencies_from_input_signature()?;
            Ok(e)
        } else {
            Err(anyhow::anyhow!("No cells loaded"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;
    use tokio::runtime::Runtime;
    use crate::cells::{CodeCell, LLMPromptCell, SupportedLanguage, SupportedModelProviders};
    use crate::utils;

    #[test]
    fn test_execute_cells_with_global_dependency() {
        let mut env = InstancedEnvironment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                x = 20
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                y = x + 1
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
    }

    #[test]
    fn test_execute_cells_between_code_and_llm() {
        dotenv::dotenv().ok();
        let mut env = InstancedEnvironment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                x = "Here is a sample string"
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            path: None,
            provider: SupportedModelProviders::OpenAI,
            req: "\
              Say only a single word. Give no additional explanation.
              What is the first word of the following: {{x}}.
            "
            .to_string(),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(
                &RkyvObjectBuilder::new()
                    .insert_string("x", "Here is a sample string".to_string())
                    .build()
            )
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RKV::String("Here".to_string()))
        );
    }

    #[test]
    fn test_execute_cells_via_prompt_calling_api() {
        let mut env = InstancedEnvironment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                import chidori as ch
                x = ch.prompt("generate_names", x="John")
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            path: Some("generate_names".to_string()),
            provider: SupportedModelProviders::OpenAI,
            req: "\
              Generate names starting with {{x}}
            "
            .to_string(),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
    }

    #[test]
    fn test_execute_cells_invoking_a_function() {
        let mut env = InstancedEnvironment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                def add(x, y):
                    return x + y
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            function_invocation: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                y = add(2, 3)
                "#}),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        // Empty object from the function declaration
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
    }

    #[test]
    fn test_execute_inter_runtime_code() {
        let mut env = InstancedEnvironment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                def add(x, y):
                    return x + y
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            function_invocation: None,
            language: SupportedLanguage::Deno,
            source_code: String::from(indoc! { r#"
                const y = add(2, 3);
                "#}),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        // Function declaration cell
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
    }

    #[test]
    fn test_execute_inter_runtime_code_md() {
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
            ```python
            def add(x, y):
                return x + y
            ```

            ```javascript
            ---
            a: 2
            ---
            const y = add(2, 3);
            ```

            ```prompt (multi_prompt)
            Multiply {y} times {x}
            ```
            "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.state.render_dependency_graph();
        env.step();
        // Function declaration cell
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
    }

    #[test]
    fn test_execute_webservice_and_handle_request_with_code_cell_md() {
        let runtime = Runtime::new().unwrap();

        runtime.block_on(async {
            // initialize tracing
            let _guard = utils::init_telemetry("http://localhost:7281").unwrap();

            let mut ee = Chidori::new();
            ee.load_md_string(indoc! { r#"
                ```python
                def add(x, y):
                    return x + y
                ```

                ```web
                ---
                port: 3838
                ---
                POST / add [a, b]
                ```
                "#
            }).unwrap();
            let mut env = ee.get_instance().unwrap();
            env.state.render_dependency_graph();

            // This will initialize the service
            env.step();
            env.step();
            env.step();

            // Function declaration cell
            let client = reqwest::Client::new();
            let mut payload = HashMap::new();
            payload.insert("a", 123); // Replace 123 with your desired value for "a"
            payload.insert("b", 456); // Replace 456 with your desired value for "b"

            let res = client.post(format!("http://127.0.0.1:{}", 3838))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
                .expect("Failed to send request");

            assert_eq!(res.text().await.unwrap(), "579");
        });
    }

    #[test]
    fn test_execute_webservice_and_serve_html() {
        let runtime = Runtime::new().unwrap();

        runtime.block_on(async {
            // initialize tracing
            let _guard = utils::init_telemetry("http://localhost:7281").unwrap();
            let mut ee = Chidori::new();
            ee.load_md_string(indoc! { r#"
                ```html (example)
                <div>Example</div>
                ```

                ```web
                ---
                port: 3838
                ---
                GET / example
                ```
                "#
            }).unwrap();
            let mut env = ee.get_instance().unwrap();
            env.state.render_dependency_graph();

            // This will initialize the service
            env.step();
            env.step();
            env.step();

            let mut payload = HashMap::new();
            payload.insert("a", 123); // Replace 123 with your desired value for "a"
            payload.insert("b", 456); // Replace 456 with your desired value for "b"

            // Function declaration cell
            let client = reqwest::Client::new();
            let res = client.get(format!("http://127.0.0.1:{}", 3838))
                .send()
                .await
                .expect("Failed to send request");

            // TODO: why is this wrapped in quotes
            assert_eq!(res.text().await.unwrap(), "<div>Example</div>");
        });
    }
}

