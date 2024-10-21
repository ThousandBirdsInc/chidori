use crate::execution::execution::execution_graph::{ExecutionGraph, ExecutionNodeId, MergedStateHistory};
use crate::execution::execution::execution_state::{ExecutionState, ExecutionStateErrors, ExecutionStateEvaluation, OperationExecutionStatus};
use crate::cells::{CellTypes, get_cell_name, LLMPromptCell};
use crate::execution::primitives::identifiers::{DependencyReference, OperationId};
use crate::execution::primitives::serialized_value::{
    RkyvSerializedValue as RKV, RkyvSerializedValue,
};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, mpsc, Mutex, MutexGuard};
use dashmap::DashMap;
use futures_util::{FutureExt, StreamExt};
use serde::ser::SerializeMap;
use uuid::Uuid;
use crate::sdk::instanced_environment::InstancedEnvironment;

/// This is an SDK for building execution graphs. It is designed to be used interactively.
///


const USER_INTERACTION_RECEIVER_TIMEOUT_MS: u64 = 500;

type Func = fn(RKV) -> RKV;

#[derive(PartialEq, Debug, Clone)]
pub enum PlaybackState {
    Paused,
    Step,
    Running,
}


// TODO: set up a channel between the host and the instance
//     so that we can send events to instances while they run on another thread

#[derive(Debug)]
pub enum UserInteractionMessage {
    Play,
    Pause,
    UserAction(String),
    RevertToState(Option<ExecutionNodeId>),
    ReloadCells,
    FetchStateAt(ExecutionNodeId),
    FetchCells,
    MutateCell(CellHolder),
    Shutdown,
    Step,
    ChatMessage(String),
    RunCellInIsolation(CellHolder, RkyvSerializedValue)
}


// https://github.com/rust-lang/rust/issues/22750
// TODO: we can't serialize these within the Tauri application due to some kind of issue
//       with serde versions once we introduced a deeper dependency on Deno.
//       we attempted the following patch to no avail:
//
//       [patch.crates-io]
//       deno = {path = "../../deno/cli"}
//       deno_runtime = {path = "../../deno/runtime"}
//       serde = {path = "../../serde/serde" }
//       serde_derive = {path = "../../serde/serde_derive" }
//       tauri = {path = "../../tauri/core/tauri" }
//
// TODO: in each of these we resolved to the same serde version.
//       we need to figure out how to resolve this issue, but to move forward
//       for now we will serialize these to Strings on this side of the interface
//       the original type of this object is as follows:
//
#[derive(Clone, Debug)]
pub enum EventsFromRuntime {
    PlaybackState(PlaybackState),
    DefinitionGraphUpdated(Vec<(OperationId, OperationId, Vec<DependencyReference>)>),
    ExecutionGraphUpdated((Vec<(ExecutionNodeId, ExecutionNodeId)>, HashSet<ExecutionNodeId>)),
    ExecutionStateChange(MergedStateHistory),
    EditorCellsUpdated(HashMap<OperationId, CellHolder>),
    StateAtId(ExecutionNodeId, ExecutionState),
    UpdateExecutionHead(ExecutionNodeId),
    ReceivedChatMessage(String),
    ExecutionStateCellsViewUpdated(Vec<CellHolder>),
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct CellHolder {
    pub cell: CellTypes,
    pub op_id: OperationId,
    pub applied_at: Option<ExecutionNodeId>,
    pub needs_update: bool
}

#[derive(Debug)]
pub struct SharedState {
    pub execution_id_to_evaluation: Arc<DashMap<ExecutionNodeId, ExecutionStateEvaluation>>,
    pub execution_state_head_id: ExecutionNodeId,
    pub latest_state: Option<ExecutionState>,
    pub editor_cells: HashMap<OperationId, CellHolder>,
    pub at_execution_state_cells: Vec<CellHolder>,
}


impl Serialize for SharedState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
    {
        let mut state = serializer.serialize_map(None)?;
        if let Some(map) = &self.latest_state {
            for (k, v) in &map.state {
                state.serialize_entry(&k, &v.deref().output)?; // Dereference `Arc` to serialize the value inside
            }
        }
        state.end()
    }
}

impl SharedState {
    pub fn new() -> Self {
        SharedState {
            execution_id_to_evaluation: Default::default(),
            execution_state_head_id: Uuid::nil(),
            latest_state: None,
            editor_cells: Default::default(),
            at_execution_state_cells: vec![],
        }
    }
}


#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use super::*;
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use indoc::indoc;
    use tokio::runtime::Runtime;
    use crate::cells::{CodeCell, LLMPromptCell, LLMPromptCellChatConfiguration, SupportedLanguage, SupportedModelProviders, TextRange};
    use crate::sdk::chidori::Chidori;
    use crate::utils;
    use crate::utils::telemetry::init_test_telemetry;

    #[tokio::test]
    async fn test_execute_cells_with_global_dependency() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        x = 20
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           Uuid::new_v4())?;
        let (_, op_id_y) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        y = x + 1
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           Uuid::new_v4())?;
        // env.resolve_dependencies_from_input_signature();
        env.get_state_at_current_execution_head().render_dependency_graph();
        // ExecutionGraph::immutable_external_step_execution(env.execution_head_state_id, env.)
        env.step().await;
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&op_id_x),
            Some(&Ok(RkyvObjectBuilder::new().insert_number("x", 20).build()))
        );
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&op_id_y), None);
        env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&op_id_x),
                   Some(&Ok(RkyvObjectBuilder::new().insert_number("x", 20).build())));
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&op_id_y),
            Some(&Ok(RkyvObjectBuilder::new().insert_number("y", 21).build()))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_cells_between_code_and_llm() -> anyhow::Result<()> {
        dotenv::dotenv().ok();
        let mut env = InstancedEnvironment::new();
        let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        x = "Here is a sample string"
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           Uuid::new_v4())?;
        let (_, op_id_y) = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            backing_file_reference: None,
            function_invocation: false,
            configuration: LLMPromptCellChatConfiguration {
                model: Some("gpt-3.5-turbo".into()),
                ..Default::default()
            },
            name: Some("example".into()),
            provider: SupportedModelProviders::OpenAI,
            complete_body: "".to_string(),
            req: "\
                      Say only a single word. Give no additional explanation.
                      What is the first word of the following: {{x}}.
                    "
                .to_string(),
        }, TextRange::default()),
                                           Uuid::new_v4())?;
        let (_, op_id_z) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        z = await example(x=x)
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           Uuid::new_v4())?;

        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await;
        assert_eq!(
            out.as_ref().unwrap().first().unwrap().1.output,
            Ok(RkyvObjectBuilder::new()
                .insert_string("x", "Here is a sample string".to_string())
                .build())
        );
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&op_id_x),
            Some(
                &Ok(RkyvObjectBuilder::new()
                    .insert_string("x", "Here is a sample string".to_string())
                    .build())
            )
        );
        let out = env.step().await;
        assert_eq!(
            out.as_ref().unwrap().first().unwrap().1.output,
            Ok(RkyvObjectBuilder::new()
                .insert_string("example", "Here".to_string())
                .build())
        );
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&op_id_y),
            Some(&Ok(RkyvObjectBuilder::new()
                    .insert_string("example", "Here".to_string())
                    .build()))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_cells_prompts_as_functions() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        y = generate_names(x="John")
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                           Uuid::new_v4())?;
        let (_, op_id_y) = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            backing_file_reference: None,
            function_invocation: false,
            configuration: LLMPromptCellChatConfiguration::default(),
            name: Some("generate_names".to_string()),
            provider: SupportedModelProviders::OpenAI,
            complete_body: "".to_string(),
            req: "\
                      Generate names starting with {{x}}
                    "
                .to_string(),
        }, TextRange::default()),
                                           Uuid::new_v4())?;
        env.get_state_at_current_execution_head().render_dependency_graph();
        dbg!(env.step().await);
        dbg!(env.step().await);
        dbg!(env.step().await);
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&op_id_x),
            None
        );
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&op_id_y), None);
        Ok(())
    }

    // #[tokio::test]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_execute_cells_invoking_a_function() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        env.wait_until_ready().await.unwrap();
        let (_, id_a) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        def add(x, y):
                            return x + y
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                      Uuid::new_v4())?;
        let (_, id_b) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            function_invocation: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        y = await add(2, 3)
                        "#}),
        }, TextRange::default()),
                                      Uuid::new_v4())?;
        env.get_state_at_current_execution_head().render_dependency_graph();
        env.step().await;
        // Empty object from the function declaration
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_a),
            Some(&Ok(RkyvObjectBuilder::new().insert_string("add", String::from("function")).build()))
        );
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_b), None);
        env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_a),
                   Some(&Ok(RkyvObjectBuilder::new().insert_string("add", String::from("function")).build()))
            );
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_b),
            Some(&Ok(RkyvObjectBuilder::new().insert_number("y", 5).build()))
        );
        env.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_execute_inter_runtime_code_plain() -> anyhow::Result<()> {
        let mut env = InstancedEnvironment::new();
        env.wait_until_ready().await.unwrap();
        let (_, id_a) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! { r#"
                        def add(x, y):
                            return x + y
                        "#}),
            function_invocation: None,
        }, TextRange::default()),
                                      Uuid::new_v4())?;
        let (_, id_b) = env.upsert_cell(CellTypes::Code(CodeCell {
            backing_file_reference: None,
            name: None,
            function_invocation: None,
            language: SupportedLanguage::Deno,
            source_code: String::from(indoc! { r#"
                        const y = await add(2, 3);
                        "#}),
        }, TextRange::default()),
                                      Uuid::new_v4())?;
        env.get_state_at_current_execution_head().render_dependency_graph();
        env.step().await;
        // Function declaration cell
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_a),
            Some(&Ok(RkyvObjectBuilder::new().insert_string("add", String::from("function")).build()))
        );
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_b),
                   None);
        env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_a),
                   Some(&Ok(RkyvObjectBuilder::new().insert_string("add", String::from("function")).build()))
        );
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_b),
            Some(&Ok(RkyvObjectBuilder::new().insert_number("y", 5).build()))
        );
        env.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_multiple_dependencies_across_nodes() -> anyhow::Result<()> {
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
            ```python
            v = 40
            def squared_value(x):
                return x * x
            ```

            ```python
            y = v * 20
            z = await squared_value(y)
            ```
            "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        env.step().await;
        // Function declaration cell
        let id_0 = Uuid::new_v4();
        let id_1 = Uuid::new_v4();
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_0),
            Some(&Ok(RkyvObjectBuilder::new()
                .insert_number("v", 40)
                .insert_string("squared_value", "function".to_string())
                .build()))
        );
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_1), None);
        env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_0),
                   Some(&Ok(RkyvObjectBuilder::new().insert_number("v", 40)
                       .insert_string("squared_value", "function".to_string())
                       .build()))
        );
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_1),
            Some(&Ok(RkyvObjectBuilder::new().insert_number("z", 640000).insert_number("y", 800).build()))
        );
        env.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_inter_runtime_code_with_markdown() {
        let mut ee = Chidori::new();
        ee.load_md_string(indoc! { r#"
            ```python
            def add(x, y):
                return x + y
            ```

            ```javascript
            const y = await add(2, 3);
            ```

            ```prompt (multi_prompt)
            ---
            model: gpt-3.5-turbo
            ---
            Multiply {y} times {x}
            ```
            "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        let s = env.get_state_at_current_execution_head();
        env.reload_cells();
        s.render_dependency_graph();
        env.step().await;
        // Function declaration cell
        let id_0 = Uuid::new_v4();
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_0),
            Some(&Ok(RkyvObjectBuilder::new()
                .insert_string("add", "function".to_string())
                .build()))
        );
        let id_1 = Uuid::new_v4();
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_1), None);
        env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().state_get_value(&id_0),
                   Some(&Ok(RkyvObjectBuilder::new()
                       .insert_string("add", "function".to_string())
                       .build()))
        );
        assert_eq!(
            env.get_state_at_current_execution_head().state_get_value(&id_1),
            Some(&Ok(RkyvObjectBuilder::new().insert_number("y", 5).build()))
        );
    }

    #[ignore]
    #[tokio::test]
    async fn test_execute_webservice_and_handle_request_with_code_cell_md() {
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
                port: 3839
                ---
                POST / add [a, b]
                ```
                "#
            }).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();

        // This will initialize the service
        env.step().await;
        env.step().await;
        env.step().await;

        // Function declaration cell
        let client = reqwest::Client::new();
        let mut payload = HashMap::new();
        payload.insert("a", 123);
        payload.insert("b", 456);

        let res = client.post(format!("http://127.0.0.1:{}", 3839))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(res.text().await.unwrap(), "579");
    }

    #[ignore]
    #[tokio::test]
    async fn test_execute_webservice_and_serve_html() {
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
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();

        // This will initialize the service
        env.step().await;
        env.step().await;
        env.step().await;

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
    }

    #[tokio::test]
    async fn test_core1_simple_math() -> anyhow::Result<()>{
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core1_simple_math")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await?;
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_number("x", 20).build()));
        let out = env.step().await?;
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_number("y", 400).build()));
        let out = env.step().await?;
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_number("zj", 420).build()));
        Ok(())
    }

    #[tokio::test]
    async fn test_core2_marshalling() -> anyhow::Result<()> {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core2_marshalling")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new()
            .insert_value("x2", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_object("x3", RkyvObjectBuilder::new()
                .insert_number("a", 1)
                .insert_number("b", 2)
                .insert_number("c", 3)
            )
            .insert_number("x0", 1)
            .insert_value("x5", RkyvSerializedValue::Float(1.0))
            .insert_value("x6", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_value("x1", RkyvSerializedValue::String("string".to_string()))
            .insert_value("x4", RkyvSerializedValue::Boolean(false))
            .insert_value("x7", RkyvSerializedValue::Set(HashSet::from_iter(vec![
                RkyvSerializedValue::String("c".to_string()),
                RkyvSerializedValue::String("a".to_string()),
                RkyvSerializedValue::String("b".to_string()),
            ].iter().cloned())))
            .build()));
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new()
            .insert_object("y3", RkyvObjectBuilder::new()
                .insert_number("a", 1)
                .insert_number("b", 2)
                .insert_number("c", 3)
            )
            .insert_value("y2", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_number("y0", 1)
            .insert_number("y5", 1)
            .insert_value("y6", RkyvSerializedValue::Array(vec![
                RkyvSerializedValue::Number(1),
                RkyvSerializedValue::Number(2),
                RkyvSerializedValue::Number(3),
            ]))
            .insert_value("y1", RkyvSerializedValue::String("string".to_string()))
            .insert_value("y4", RkyvSerializedValue::Boolean(false))
            .build()));
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert!(out[0].1.stderr.contains(&"OK".to_string()));
        Ok(())
    }


    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core3_function_invocations() -> anyhow::Result<()> {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core3_function_invocations")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_string("add_two", "function".to_string()).build()));

        // TODO: there's nothing to test on this instance but that's an issue
        dbg!(env.step().await);

        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_string("addTwo", "function".to_string()).build()));
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.stderr.contains(&"OK".to_string()), true);
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
        env.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core4_async_function_invocations() -> anyhow::Result<()> {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core4_async_function_invocations")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_string("add_two", "function".to_string()).build()));

        // TODO: there's nothing to test on this instance but that's an issue
        dbg!(env.step().await);

        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.output, Ok(RkyvObjectBuilder::new().insert_string("addTwo", "function".to_string()).build()));
        let mut out = env.step().await?;
        assert_eq!(out[0].0, Uuid::nil());
        assert_eq!(out[0].1.stderr.contains(&"OK".to_string()), true);
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
        env.shutdown().await;
        Ok(())
    }


    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core5_prompts_invoked_as_functions() -> anyhow::Result<()>  {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core5_prompts_invoked_as_functions")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await;
        dbg!(out);
        let out = env.step().await;
        dbg!(out);
        let out = env.step().await;
        dbg!(out);
        let out = env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
        env.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core6_prompts_leveraging_function_calling() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core6_prompts_leveraging_function_calling")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
    }

    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core7_rag_stateful_memory_cells() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core7_rag_stateful_memory_cells")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
    }

    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core8_prompt_code_generation_and_execution() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core8_prompt_code_generation_and_execution")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
    }

    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_core9_multi_agent_simulation() {
        let mut ee = Chidori::new();
        ee.load_md_directory(Path::new("./examples/core9_multi_agent_simulation")).unwrap();
        let mut env = ee.get_instance().unwrap();
        env.reload_cells();
        env.get_state_at_current_execution_head().render_dependency_graph();
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        let out = env.step().await;
        assert_eq!(env.get_state_at_current_execution_head().have_all_operations_been_set_at_least_once(), true);
    }
}

