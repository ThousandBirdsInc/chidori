use crate::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId, MergedStateHistory};
use crate::execution::execution::execution_state::ExecutionState;
use crate::execution::execution::DependencyGraphMutation;
use crate::cells::CellTypes;
use crate::execution::primitives::identifiers::DependencyReference;
use crate::execution::primitives::serialized_value::{
    RkyvSerializedValue as RKV, RkyvSerializedValue,
};
use crate::sdk::md::{interpret_code_block, load_folder};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use std::{fmt, thread};
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, mpsc, Mutex, MutexGuard};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::sleep;
use serde::ser::SerializeMap;
use crate::utils::telemetry::{init_internal_telemetry, TraceEvents};


/// This is an SDK for building execution graphs. It is designed to be used interactively.

type Func = fn(RKV) -> RKV;

#[derive(PartialEq, Debug)]
enum PlaybackState {
    Paused,
    Running,
}

// TODO: set up a channel between the host and the instance
//     so that we can send events to instances while they run on another thread

/// Instanced environments are not Send and live on a single thread.
/// They execute their operations across multiple threads, but individual OperationNodes
/// must remain on the given thread they're initialized on.
pub struct InstancedEnvironment {
    env_rx: Receiver<UserInteractionMessage>,
    pub db: ExecutionGraph,
    pub execution_head_latest_state: ExecutionState,
    execution_head_state_id: (usize, usize),
    playback_state: PlaybackState,
    runtime_event_sender: Option<Sender<EventsFromRuntime>>,
    sender: Option<Sender<TraceEvents>>,
    shared_state: Arc<Mutex<SharedState>>
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
        // TODO: handle this better, this just makes our tests pass until its resolved
        let (tx, rx) = mpsc::channel();
        InstancedEnvironment {
            env_rx: rx,
            db,
            execution_head_latest_state: state,
            execution_head_state_id: state_id,
            runtime_event_sender: None,
            sender: None,
            playback_state,
            shared_state: Arc::new(Mutex::new(SharedState::new())),
        }
    }

    fn reload_cells(&mut self) {
        let cells_to_upsert: Vec<_> = {
            let shared_state = self.shared_state.lock().unwrap();
            shared_state.cells.iter().map(|cell| cell.cell.clone()).collect()
        };

        let mut ids = vec![];
        for cell in cells_to_upsert {
            ids.push(self.upsert_cell(cell));
        }

        let mut shared_state = self.shared_state.lock().unwrap();
        for (i, cell) in shared_state.cells.iter_mut().enumerate() {
            cell.op_id = ids[i];
        }

        self.runtime_event_sender.as_mut().map(|sender| {
            sender.send(EventsFromRuntime::CellsUpdated(shared_state.cells.clone())).unwrap();
        });
    }

    /// Entrypoint for execution of an instanced environment, handles messages from the host
    #[tracing::instrument]
    pub fn run(&mut self) {
        self.playback_state = PlaybackState::Paused;
        let _maybe_guard = self.sender.as_ref().map(|sender| {
            tracing::subscriber::set_default(init_internal_telemetry(sender.clone()))
        });
        loop {
            if let Ok(message) = self.env_rx.try_recv() {
                match message {
                    UserInteractionMessage::Play => {
                        self.playback_state = PlaybackState::Running;
                    },
                    UserInteractionMessage::Pause => {
                        self.playback_state = PlaybackState::Paused;
                    },
                    UserInteractionMessage::ReloadCells(cells) => {
                        dbg!("Reloading cells");
                        self.shared_state.lock().as_deref_mut().unwrap().cells = cells.iter().map(|cell| CellHolder {
                            cell: cell.clone(),
                            op_id: 0,
                        }).collect();
                        self.reload_cells();
                    },
                    UserInteractionMessage::RevertToState(id) => {
                        dbg!("Reverting to state", &id);
                        if let Some(id) = id {
                            self.execution_head_state_id = id;
                            self.execution_head_latest_state = self.db.get_state_at_id(id).unwrap();
                            self.runtime_event_sender.as_mut().map(|sender| {
                                sender.send(EventsFromRuntime::ExecutionStateChange(self.db.get_merged_state_history(&id))).unwrap();
                            });
                        }
                    },
                    _ => {}
                }
            }
            if self.playback_state == PlaybackState::Paused {
                sleep(std::time::Duration::from_millis(1000));
            } else {
                let output = self.step();
                if output.is_empty() {
                    sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }

    /// Increment the execution graph by one step
    #[tracing::instrument]
    pub(crate) fn step(&mut self) -> Vec<(usize, RkyvSerializedValue)> {
        let state = self.db.get_state_at_id(self.execution_head_state_id);
        if let Some(state) = state {
            let ((state_id, state), outputs) = self.db.step_execution(self.execution_head_state_id, &state);

            let merged_state = self.db.get_merged_state_history(&state_id);
            let execution_graph = self.db.get_execution_graph_elements();

            // TODO: this should accumulate the state such that we can refer to the whole execution history along a selected thread
            self.runtime_event_sender.as_mut().map(|sender| {
                sender.send(EventsFromRuntime::ExecutionGraphUpdated(execution_graph)).unwrap();
                sender.send(EventsFromRuntime::ExecutionStateChange(merged_state)).unwrap();
            });

            self.execution_head_state_id = state_id;
            self.execution_head_latest_state = state;
            outputs
        } else {
            vec![]
        }
    }

    /// Add a cell into the execution graph
    #[tracing::instrument]
    pub fn upsert_cell(&mut self, cell: CellTypes) -> usize {
        let state = self.db.get_state_at_id(self.execution_head_state_id);
        // TODO: handle this if condition properly
        if let Some(state) = state {
            let ((state_id, state), op_id) = self.db.mutate_graph(self.execution_head_state_id, &state, cell);
            let merged_state = self.db.get_merged_state_history(&state_id);
            self.runtime_event_sender.as_mut().map(|sender| {
                sender.send(EventsFromRuntime::ExecutionStateChange(merged_state)).unwrap();
            });
            self.execution_head_state_id = state_id;
            self.execution_head_latest_state = state;
            op_id
        } else {
            // TODO: should error
            0
        }
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}
}

#[derive(Debug)]
pub enum UserInteractionMessage {
    Play,
    Pause,
    UserAction(String),
    RevertToState(Option<(usize, usize)>),
    ReloadCells(Vec<CellTypes>),
    FetchCells,
    MutateCell
}


#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EventsFromRuntime {
    ExecutionGraphUpdated(Vec<(ExecutionNodeId, ExecutionNodeId)>),
    ExecutionStateChange(MergedStateHistory),
    CellsUpdated(Vec<CellHolder>)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CellHolder {
    cell: CellTypes,
    op_id: usize,
}

#[derive(Debug)]
pub struct SharedState {
    latest_state: Option<ExecutionState>,
    cells: Vec<CellHolder>,
}


impl Serialize for SharedState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        let mut state = serializer.serialize_map(None)?;
        if let Some(map) = &self.latest_state {
            for (k, v) in &map.state {
                state.serialize_entry(&k, v.deref())?; // Dereference `Arc` to serialize the value inside
            }
        }
        state.end()
    }
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            latest_state: None,
            cells: vec![],
        }
    }
}


/// Chidori is the high level interface for interacting with our runtime.
/// It is responsible for loading cells and creating instances of the environment.
/// It is expected to run on a "main thread" while instances may run in background threads.
pub struct Chidori {

    /// Sender to push user requests to the instance, these events result in
    /// state changes within the instance
    instanced_env_tx: Option<Sender<UserInteractionMessage>>,

    /// Sender to pass changes in state within instances back to the main thread
    runtime_event_sender: Option<Sender<EventsFromRuntime>>,

    /// Sender to collect trace events from instances
    trace_event_sender: Option<Sender<TraceEvents>>,

    shared_state: Arc<Mutex<SharedState>>,
    pub loaded_path: Option<String>,
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
            instanced_env_tx: None,
            runtime_event_sender: None,
            trace_event_sender: None,
            loaded_path: None,
            shared_state: Arc::new(Mutex::new(SharedState {
                cells: vec![],
                latest_state: None,
            })),
        }
    }

    pub fn new_with_events(sender: Sender<TraceEvents>, runtime_event_sender: Sender<EventsFromRuntime>) -> Self {
        Chidori {
            instanced_env_tx: None,
            runtime_event_sender: Some(runtime_event_sender),
            trace_event_sender: Some(sender),
            loaded_path: None,
            shared_state: Arc::new(Mutex::new(SharedState {
                cells: vec![],
                latest_state: None,
            })),
        }
    }

    pub fn get_shared_state(&self) -> MutexGuard<'_, SharedState> {
        self.shared_state.lock().unwrap()
    }

    pub fn get_cells(&self) -> Vec<CellTypes> {
        vec![]
    }

    #[tracing::instrument]
    pub fn handle_user_action(&self, action: UserInteractionMessage) {
        if let Some(tx) = &self.instanced_env_tx {
            tx.send(action).unwrap();
        }
    }

    pub fn load_md_string(&mut self, s: &str) -> anyhow::Result<()> {
        let mut cells = vec![];
        crate::sdk::md::extract_code_blocks(s)
            .iter()
            .filter_map(|block| interpret_code_block(block))
            .for_each(|block| { cells.push(block); });
        self.handle_user_action(UserInteractionMessage::ReloadCells(cells));
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
        self.loaded_path = Some(path.to_str().unwrap().to_string());
        dbg!("handle user action load md directory");
        self.handle_user_action(UserInteractionMessage::ReloadCells(cells));
        Ok(())
    }

    pub fn get_instance(&mut self) -> anyhow::Result<InstancedEnvironment> {
        let (instanced_env_tx, env_rx) = mpsc::channel();
        self.instanced_env_tx = Some(instanced_env_tx);
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        let playback_state = PlaybackState::Paused;
        Ok(InstancedEnvironment {
            env_rx,
            db,
            execution_head_latest_state: state,
            execution_head_state_id: state_id,
            runtime_event_sender: self.runtime_event_sender.clone(),
            sender: self.trace_event_sender.clone(),
            playback_state,
            shared_state: self.shared_state.clone(),
        })
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
        let op_id_x = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                x = 20
                "#}),
            function_invocation: None,
        }));
        assert_eq!(op_id_x, 0);
        let op_id_y = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                y = x + 1
                "#}),
            function_invocation: None,
        }));
        assert_eq!(op_id_y, 1);
        // env.resolve_dependencies_from_input_signature();
        env.execution_head_latest_state.render_dependency_graph();
        dbg!(&env.step());
        assert_eq!(
            env.execution_head_latest_state.state_get(&op_id_x),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.execution_head_latest_state.state_get(&op_id_y), None);
        env.step();
        assert_eq!(env.execution_head_latest_state.state_get(&op_id_x), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&op_id_y),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
    }

    #[test]
    fn test_execute_cells_between_code_and_llm() {
        dotenv::dotenv().ok();
        let mut env = InstancedEnvironment::new();
        let op_id_x = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                x = "Here is a sample string"
                "#}),
            function_invocation: None,
        }));
        assert_eq!(op_id_x, 0);
        let op_id_y = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            name: None,
            provider: SupportedModelProviders::OpenAI,
            req: "\
              Say only a single word. Give no additional explanation.
              What is the first word of the following: {{x}}.
            "
            .to_string(),
        }));
        assert_eq!(op_id_y, 1);
        env.execution_head_latest_state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.execution_head_latest_state.state_get(&op_id_x),
            Some(
                &RkyvObjectBuilder::new()
                    .insert_string("x", "Here is a sample string".to_string())
                    .build()
            )
        );
        assert_eq!(env.execution_head_latest_state.state_get(&op_id_y), None);
        env.step();
        assert_eq!(env.execution_head_latest_state.state_get(&op_id_x), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&op_id_y),
            Some(&RKV::String("Here".to_string()))
        );
    }

    #[test]
    fn test_execute_cells_via_prompt_calling_api() {
        let mut env = InstancedEnvironment::new();
        let op_id_x = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                import chidori as ch
                x = ch.prompt("generate_names", x="John")
                "#}),
            function_invocation: None,
        }));
        assert_eq!(op_id_x, 0);
        let op_id_y = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            name: Some("generate_names".to_string()),
            provider: SupportedModelProviders::OpenAI,
            req: "\
              Generate names starting with {{x}}
            "
            .to_string(),
        }));
        assert_eq!(op_id_y, 1);
        env.execution_head_latest_state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.execution_head_latest_state.state_get(&op_id_x),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.execution_head_latest_state.state_get(&op_id_y), None);
        env.step();
        assert_eq!(env.execution_head_latest_state.state_get(&op_id_x), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&op_id_y),
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
        assert_eq!(id, 0);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            function_invocation: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                y = add(2, 3)
                "#}),
        }));
        assert_eq!(id, 1);
        env.execution_head_latest_state.render_dependency_graph();
        env.step();
        // Empty object from the function declaration
        assert_eq!(
            env.execution_head_latest_state.state_get(&0),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.execution_head_latest_state.state_get(&1), None);
        env.step();
        assert_eq!(env.execution_head_latest_state.state_get(&0), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&1),
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
        assert_eq!(id, 0);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            function_invocation: None,
            language: SupportedLanguage::Deno,
            source_code: String::from(indoc! { r#"
                const y = add(2, 3);
                "#}),
        }));
        assert_eq!(id, 1);
        env.execution_head_latest_state.render_dependency_graph();
        env.step();
        // Function declaration cell
        assert_eq!(
            env.execution_head_latest_state.state_get(&0),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.execution_head_latest_state.state_get(&1), None);
        env.step();
        assert_eq!(env.execution_head_latest_state.state_get(&0), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("y", 5).build())
        );
    }
    #[test]
    fn test_multiple_dependencies_across_nodes() {
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
            ```python
            v = 40
            def add(x, y):
                return x + y
            ```

            ```python
            y = v * 20
            z = add(y, y)
            ```
            "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.execution_head_latest_state.render_dependency_graph();
        env.step();
        // Function declaration cell
        assert_eq!(
            env.execution_head_latest_state.state_get(&0),
            Some(&RkyvObjectBuilder::new().insert_number("v", 40).build())
        );
        assert_eq!(env.execution_head_latest_state.state_get(&1), None);
        env.step();
        for x in env.execution_head_latest_state.operation_by_id.values() {
            dbg!(&x.lock().as_ref().unwrap().signature);
        }
        assert_eq!(env.execution_head_latest_state.state_get(&0), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("z", 1600).insert_number("y", 800).build())
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
        env.execution_head_latest_state.render_dependency_graph();
        env.step();
        // Function declaration cell
        assert_eq!(
            env.execution_head_latest_state.state_get(&0),
            Some(&RkyvObjectBuilder::new().build())
        );
        assert_eq!(env.execution_head_latest_state.state_get(&1), None);
        env.step();
        assert_eq!(env.execution_head_latest_state.state_get(&0), None);
        assert_eq!(
            env.execution_head_latest_state.state_get(&1),
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
            env.execution_head_latest_state.render_dependency_graph();

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
            env.execution_head_latest_state.render_dependency_graph();

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
