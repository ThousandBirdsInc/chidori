use chidori_prompt_format::templating::templates::{ChatModelRoles, TemplateWithSource};
use std::collections::HashMap;
use std::env;
use tokio::runtime;
use crate::cells::{LLMPromptCell, SupportedModelProviders};
use crate::execution::primitives::operation::{InputItemConfiguation, InputSignature, InputType, OperationNode, OutputItemConfiguation, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, RkyvSerializedValue, serialized_value_to_json_value};
use futures_util::FutureExt;
use crate::execution::execution::ExecutionState;
use crate::library::std::ai::llm::ai_llm_run_embedding_model;



/// LLM Prompt Cells allow notebooks to invoke language models to generate text.
#[tracing::instrument]
pub fn llm_prompt_cell(cell: &LLMPromptCell) -> OperationNode {
    match cell {
        LLMPromptCell::Chat {function_invocation, configuration, name, provider, req, .. } => {
            let (role_blocks, output_signature, input_signature) = convert_signatures(configuration, name, &req);
            let name = name.clone();
            let configuration = configuration.clone();
            let is_function_invocation = function_invocation.clone();
            if configuration.get("fn").is_some() && !is_function_invocation {
                return panic!("Cell is called as a function invocation without a declared fn name");
            }
            match provider {
                SupportedModelProviders::OpenAI => OperationNode::new(
                    name.clone(),
                    input_signature,
                    output_signature,
                    Box::new(move |s, payload, _, _| {
                        let role_blocks = role_blocks.clone();
                        let name = name.clone();
                        // TODO: this state should error? or what should this do
                        if configuration.get("fn").is_some() && !is_function_invocation {
                            return async move { RkyvSerializedValue::Null }.boxed();
                        }
                        let s = s.clone();
                        async move {
                            crate::library::std::ai::llm::ai_llm_run_chat_model(
                                &s,
                                payload,
                                role_blocks,
                                name,
                                is_function_invocation,
                            ).await
                        }.boxed()
                    }),
                ),
            }
        }
        LLMPromptCell::Completion { .. } => OperationNode::new(
            None,
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, x, _, _| async move { x }.boxed()),
        ),
        LLMPromptCell::Embedding { function_invocation, configuration, req, name, .. } => {
            let (mut role_blocks, output_signature, input_signature) = convert_signatures(configuration, name, &req);
            let (_, template) = role_blocks.drain(..).next().unwrap();
            let template = template.expect("Must include template");
            let name = name.clone();
            let is_function_invocation = function_invocation.clone();
            if configuration.get("fn").is_some() && !is_function_invocation {
                return panic!("Cell is called as a function invocation without a declared fn name");
            }
            OperationNode::new(
                name.clone(),
                input_signature,
                output_signature,
                Box::new(move |s, x, _, _|  {
                    let template = template.clone();
                    let name = name.clone();
                    let s = s.clone();
                    async move {
                        ai_llm_run_embedding_model(&s, x, template, name.clone(), is_function_invocation).await
                    }.boxed()
                }),
            )
        },
    }
}

fn convert_signatures(configuration: &HashMap<String, String>, name: &Option<String>, req: &&String) -> (Vec<(ChatModelRoles, Option<TemplateWithSource>)>, OutputSignature, InputSignature) {
    let schema =
        chidori_prompt_format::templating::templates::analyze_referenced_partials(&req);
    let role_blocks =
        chidori_prompt_format::templating::templates::extract_roles_from_template(&req);

    let mut output_signature = OutputSignature::new();
    if let Some(fn_name) = configuration.get("fn") {
        output_signature.functions.insert(
            fn_name.clone(),
            OutputItemConfiguation {
                ty: Some(InputType::String),
            },
        );
    }
    if let Some(name) = name {
        // The result of executing the prompt is available as the name of the cell
        // when the cell is named.
        output_signature.globals.insert(
            name.clone(),
            OutputItemConfiguation {
                ty: Some(InputType::String),
            },
        );
    }

    let mut input_signature = InputSignature::new();
    // We only require the globals to be passed in if the user has not specified this prompt as a function
    if configuration.get("fn").is_none() {
        for (key, value) in &schema.items {
            input_signature.globals.insert(
                key.clone(),
                InputItemConfiguation {
                    ty: Some(InputType::String),
                    default: None,
                },
            );
        }
    }
    (role_blocks, output_signature, input_signature)
}
