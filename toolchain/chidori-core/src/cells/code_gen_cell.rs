use crate::cells::{LLMCodeGenCell, LLMPromptCell, SupportedModelProviders, TextRange};
use crate::execution::primitives::operation::{InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, RkyvSerializedValue, serialized_value_to_json_value};
use futures_util::FutureExt;
use crate::execution::execution::execution_graph::ExecutionNodeId;


#[tracing::instrument]
pub fn code_gen_cell(execution_state_id: ExecutionNodeId, cell: &LLMCodeGenCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    let LLMCodeGenCell {
        configuration,
        name,
        provider,
        req,
        function_invocation,
        ..
    } = cell;
    let schema = chidori_prompt_format::templating::templates::analyze_referenced_partials(&&req);
    let mut role_blocks = chidori_prompt_format::templating::templates::extract_roles_from_template(r#"
{{#system}}
   You are a developer working on a code generation tool. You have been tasked with creating a function that performs the described functionality.
   Output only the source code for the function. Do not include examples of running the function.
{{/system}}
    "#);
    role_blocks.extend(chidori_prompt_format::templating::templates::extract_roles_from_template(&&req));

    let mut output_signature = OutputSignature::new();
    if let Some(fn_name) = &configuration.function_name {
        output_signature.functions.insert(
            fn_name.clone(),
            OutputItemConfiguration::Value,
        );
    }
    if let Some(name) = name {
        // The result of executing the prompt is available as the name of the cell
        // when the cell is named.
        output_signature.globals.insert(
            name.clone(),
            OutputItemConfiguration::Value,
        );
    }

    let mut input_signature = InputSignature::new();

    // We only require the globals to be passed in if the user has not specified this prompt as a function
    if configuration.function_name.is_none() {
        for (key, value) in &schema.items {
            input_signature.globals.insert(
                key.clone(),
                InputItemConfiguration {
                    ty: Some(InputType::String),
                    default: None,
                },
            );
        }
    }

    let name = name.clone();
    let configuration = configuration.clone();
    let is_function_invocation = function_invocation.clone();
    if configuration.function_name.is_none() && is_function_invocation {
        return panic!("Cell is called as a function invocation without a declared fn name");
    }

    match provider {
        SupportedModelProviders::OpenAI => Ok(OperationNode::new(
            name.clone(),
            execution_state_id,
            input_signature,
            output_signature,
            Box::new(move |s, payload, _, _| {
                let closure_span = tracing::span!(tracing::Level::INFO, "code_generation_cell");
                let _enter = closure_span.enter();
                let role_blocks = role_blocks.clone();
                let name = name.clone();
                if configuration.function_name.is_some() && !is_function_invocation {
                    return async move { Ok(OperationFnOutput::with_value(RkyvSerializedValue::Null)) }.boxed();
                }
                let s = s.clone();
                let configuration = configuration.clone();
                async move {
                    let (value, state) = crate::library::std::ai::llm::ai_llm_code_generation_chat_model(
                        &s,
                        payload,
                        role_blocks,
                        name,
                        is_function_invocation,
                        configuration.clone()
                    ).await?;
                    Ok(OperationFnOutput {
                        has_error: false,
                        execution_state: state,
                        output: Ok(value),
                        stdout: vec![],
                        stderr: vec![],
                    })
                }.boxed()
            }),
        )),
    }
}
