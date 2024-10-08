use chidori_prompt_format::templating::templates::{ChatModelRoles, TemplateWithSource};
use std::collections::HashMap;
use std::env;
use tokio::runtime;
use crate::cells::{LLMPromptCell, LLMPromptCellChatConfiguration, SupportedModelProviders, TextRange};
use crate::execution::primitives::operation::{InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue as RKV, RkyvSerializedValue, serialized_value_to_json_value};
use futures_util::FutureExt;
use crate::execution::execution::execution_graph::ExecutionNodeId;
use crate::execution::execution::ExecutionState;
use crate::library::std::ai::llm::ai_llm_run_embedding_model;



/// LLM Prompt Cells allow notebooks to invoke language models to generate text.
#[tracing::instrument]
pub fn llm_prompt_cell(execution_state_id: ExecutionNodeId, cell: &LLMPromptCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    match cell {
        LLMPromptCell::Chat {
            function_invocation,
            name,
            provider,
            complete_body,
            ..
        } => {
            let (frontmatter, req) = chidori_prompt_format::templating::templates::split_frontmatter(&complete_body).map_err(|e| {
                anyhow::Error::msg(e.to_string())
            })?;
            let configuration: LLMPromptCellChatConfiguration = serde_yaml::from_str(&frontmatter)?;
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
                for (key, value) in &schema.unwrap().items {
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
                SupportedModelProviders::OpenAI => Ok(OperationNode::new(
                    name.clone(),
                    execution_state_id,
                    input_signature,
                    output_signature,
                    Box::new(move |s, payload, _, _| {
                        let role_blocks = role_blocks.clone();
                        let name = name.clone();
                        // TODO: this state should error? or what should this do
                        if configuration.function_name.is_some() && !is_function_invocation {
                            // Return the declared name of the function
                            let fn_name = configuration.function_name.as_ref().unwrap().clone();
                            return async move { Ok(OperationFnOutput::with_value(RkyvObjectBuilder::new().insert_string(&fn_name, "function".to_string())
                                .build()
                            )) }.boxed();
                        }
                        let s = s.clone();
                        let configuration = configuration.clone();
                        async move {
                            let (value, state) = crate::library::std::ai::llm::ai_llm_run_chat_model(
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
                                output: value,
                                stdout: vec![],
                                stderr: vec![],
                            })
                        }.boxed()
                    }),
                )),
            }
        }
        LLMPromptCell::Completion { .. } => Ok(OperationNode::new(
            None,
            execution_state_id,
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, x, _, _| async move { Ok(OperationFnOutput::with_value(x)) }.boxed()),
        )),
    }
}
