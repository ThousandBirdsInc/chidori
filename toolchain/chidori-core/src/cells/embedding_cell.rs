use futures_util::FutureExt;
use crate::cells::{LLMEmbeddingCell, LLMPromptCell, TextRange};
use crate::execution::primitives::operation::{InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use crate::library::std::ai::llm::ai_llm_run_embedding_model;

#[tracing::instrument]
pub fn llm_embedding_cell(cell: &LLMEmbeddingCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    let LLMEmbeddingCell {
        function_invocation,
        configuration,
        req,
        name,
        ..
    } = cell;
    let schema =
        chidori_prompt_format::templating::templates::analyze_referenced_partials(&&req);
    let mut role_blocks =
        chidori_prompt_format::templating::templates::extract_roles_from_template(&&req);

    let mut output_signature = OutputSignature::new();
    if let Some(name) = name {
        output_signature.functions.insert(
            name.clone(),
            OutputItemConfiguration::Value,
        );
    }

    let mut input_signature = InputSignature::new();

    // We only require the globals to be passed in if the user has not specified this prompt as a function
    if configuration.get("fn").is_none() {
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

    let (_, template) = role_blocks.drain(..).next().unwrap();
    let template = template.expect("Must include template");
    let name = name.clone();
    let is_function_invocation = function_invocation.clone();
    Ok(OperationNode::new(
        name.clone(),
        input_signature,
        output_signature,
        Box::new(move |s, x, _, _|  {
            let template = template.clone();
            let name = name.clone();
            let s = s.clone();
            async move {
                Ok(OperationFnOutput::with_value(ai_llm_run_embedding_model(&s, x, template, name.clone(), is_function_invocation).await))
            }.boxed()
        }),
    ))
}
