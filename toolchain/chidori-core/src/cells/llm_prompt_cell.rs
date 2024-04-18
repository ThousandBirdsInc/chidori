use chidori_prompt_format::templating::templates::{ChatModelRoles, TemplateWithSource};
use std::collections::HashMap;
use std::env;
use tokio::runtime;
use crate::cells::{LLMPromptCell, SupportedModelProviders};
use crate::execution::primitives::operation::{InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, RkyvSerializedValue, serialized_value_to_json_value};
use futures_util::FutureExt;
use crate::execution::execution::ExecutionState;
use crate::library::std::ai::llm::ai_llm_run_embedding_model;



/// LLM Prompt Cells allow notebooks to invoke language models to generate text.
#[tracing::instrument]
pub fn llm_prompt_cell(cell: &LLMPromptCell) -> OperationNode {
    match cell {
        LLMPromptCell::Chat {
            function_invocation,
            configuration,
            name,
            provider,
            req,
            ..
        } => {
            let schema =
                chidori_prompt_format::templating::templates::analyze_referenced_partials(&&req);
            let role_blocks =
                chidori_prompt_format::templating::templates::extract_roles_from_template(&&req);

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


            if let Some(imports) = &configuration.import {
                for key in imports {
                    input_signature.globals.insert(
                        key.clone(),
                        InputItemConfiguration {
                            ty: Some(InputType::String),
                            default: None,
                        },
                    );
                }
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
                        if configuration.function_name.is_some() && !is_function_invocation {
                            return async move { OperationFnOutput::with_value(RkyvSerializedValue::Null) }.boxed();

                        }
                        let s = s.clone();
                        let configuration = configuration.clone();
                        async move {
                            OperationFnOutput::with_value(
                            crate::library::std::ai::llm::ai_llm_run_chat_model(
                                &s,
                                payload,
                                role_blocks,
                                name,
                                is_function_invocation,
                                configuration.clone()
                            ).await)
                        }.boxed()
                    }),
                ),
            }
        }
        LLMPromptCell::Completion { .. } => OperationNode::new(
            None,
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, x, _, _| async move { OperationFnOutput::with_value(x) }.boxed()),
        ),
        LLMPromptCell::Embedding {
            function_invocation,
            configuration,
            req,
            name,
            ..
        } => {
            let schema =
                chidori_prompt_format::templating::templates::analyze_referenced_partials(&&req);
            let mut role_blocks =
                chidori_prompt_format::templating::templates::extract_roles_from_template(&&req);

            let mut output_signature = OutputSignature::new();
            if let Some(fn_name) = configuration.get("fn") {
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
                        OperationFnOutput::with_value(ai_llm_run_embedding_model(&s, x, template, name.clone(), is_function_invocation).await)
                    }.boxed()
                }),
            )
        },
    }
}
