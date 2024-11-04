use std::collections::{HashMap, HashSet};
use std::path::Path;
use super::*;
use chidori_core::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue};
use indoc::indoc;
use uuid::Uuid;
use chidori_core::cells::{CellTypes, CodeCell, LLMPromptCell, LLMPromptCellChatConfiguration, SupportedLanguage, SupportedModelProviders, TextRange};
use chidori_core::sdk::interactive_chidori_wrapper::InteractiveChidoriWrapper;
use chidori_core::sdk::chidori_runtime_instance::ChidoriRuntimeInstance;
use chidori_core::utils;

#[tokio::test]
async fn test_execute_cells_with_global_dependency() -> anyhow::Result<()> {
    let mut env = ChidoriRuntimeInstance::new();
    let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        language: SupportedLanguage::PyO3,
        source_code: String::from(indoc! { r#"
                        x = 20
                        "#}),
        function_invocation: None,
    }, TextRange::default()),
                                       Uuid::now_v7())?;
    let (_, op_id_y) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        language: SupportedLanguage::PyO3,
        source_code: String::from(indoc! { r#"
                        y = x + 1
                        "#}),
        function_invocation: None,
    }, TextRange::default()),
                                       Uuid::now_v7())?;
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
    let mut env = ChidoriRuntimeInstance::new();
    let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        language: SupportedLanguage::PyO3,
        source_code: String::from(indoc! { r#"
                        x = "Here is a sample string"
                        "#}),
        function_invocation: None,
    }, TextRange::default()),
                                       Uuid::now_v7())?;
    let (_, op_id_y) = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
        backing_file_reference: None,
        is_function_invocation: false,
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
                                       Uuid::now_v7())?;
    let (_, op_id_z) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        language: SupportedLanguage::PyO3,
        source_code: String::from(indoc! { r#"
                        z = await example(x=x)
                        "#}),
        function_invocation: None,
    }, TextRange::default()),
                                       Uuid::now_v7())?;

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
    let mut env = ChidoriRuntimeInstance::new();
    let (_, op_id_x) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        language: SupportedLanguage::PyO3,
        source_code: String::from(indoc! { r#"
                        y = generate_names(x="John")
                        "#}),
        function_invocation: None,
    }, TextRange::default()),
                                       Uuid::now_v7())?;
    let (_, op_id_y) = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
        backing_file_reference: None,
        is_function_invocation: false,
        configuration: LLMPromptCellChatConfiguration::default(),
        name: Some("generate_names".to_string()),
        provider: SupportedModelProviders::OpenAI,
        complete_body: "".to_string(),
        req: "\
                      Generate names starting with {{x}}
                    "
            .to_string(),
    }, TextRange::default()),
                                       Uuid::now_v7())?;
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
    let mut env = ChidoriRuntimeInstance::new();
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
                                    Uuid::now_v7())?;
    let (_, id_b) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        function_invocation: None,
        language: SupportedLanguage::PyO3,
        source_code: String::from(indoc! { r#"
                        y = await add(2, 3)
                        "#}),
    }, TextRange::default()),
                                    Uuid::now_v7())?;
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
    let mut env = ChidoriRuntimeInstance::new();
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
                                    Uuid::now_v7())?;
    let (_, id_b) = env.upsert_cell(CellTypes::Code(CodeCell {
        backing_file_reference: None,
        name: None,
        function_invocation: None,
        language: SupportedLanguage::Deno,
        source_code: String::from(indoc! { r#"
                        const y = await add(2, 3);
                        "#}),
    }, TextRange::default()),
                                    Uuid::now_v7())?;
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let id_0 = Uuid::now_v7();
    let id_1 = Uuid::now_v7();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let id_0 = Uuid::now_v7();
    assert_eq!(
        env.get_state_at_current_execution_head().state_get_value(&id_0),
        Some(&Ok(RkyvObjectBuilder::new()
            .insert_string("add", "function".to_string())
            .build()))
    );
    let id_1 = Uuid::now_v7();
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

    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
    let mut ee = InteractiveChidoriWrapper::new();
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
