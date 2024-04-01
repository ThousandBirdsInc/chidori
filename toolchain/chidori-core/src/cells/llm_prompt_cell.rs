use chidori_prompt_format::templating::templates::ChatModelRoles;
use std::collections::HashMap;
use std::env;
use tokio::runtime;
use crate::cells::{LLMPromptCell, SupportedModelProviders};
use crate::execution::primitives::operation::{InputItemConfiguation, InputSignature, InputType, OperationNode, OutputItemConfiguation, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, RkyvSerializedValue, serialized_value_to_json_value};
use crate::library::std::ai::llm::{ChatCompletionReq, ChatCompletionRes, ChatModelBatch, MessageRole, TemplateMessage};
use futures_util::FutureExt;


/// LLM Prompt Cells allow notebooks to invoke language models to generate text.
#[tracing::instrument]
pub fn llm_prompt_cell(cell: &LLMPromptCell) -> OperationNode {
    match cell {
        LLMPromptCell::Chat {function_invocation, configuration, name, provider, req, .. } => {
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
            if let Some(name)  = name {
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
                    // input_signature.kwargs.insert(
                    //     key.clone(),
                    //     InputItemConfiguation {
                    //         ty: Some(InputType::String),
                    //         default: None,
                    //     },
                    // );
                    input_signature.globals.insert(
                        key.clone(),
                        InputItemConfiguation {
                            ty: Some(InputType::String),
                            default: None,
                        },
                    );
                }
            }


            let name = name.clone();
            let configuration = configuration.clone();
            let function_invocation = function_invocation.clone();
            match provider {
                SupportedModelProviders::OpenAI => OperationNode::new(
                    name.clone(),
                    input_signature,
                    output_signature,
                    Box::new(move |_, x, _| {
                        let role_blocks = role_blocks.clone();
                        let name = name.clone();
                        dbg!(&function_invocation);
                        if configuration.get("fn").is_some() && !function_invocation {
                            return async move { RkyvSerializedValue::Null }.boxed();
                        }
                        async move {
                            let mut template_messages: Vec<TemplateMessage> = Vec::new();
                            let data = if let RKV::Object(m) = x {
                                serialized_value_to_json_value(
                                    &m.get("globals").expect("should have globals key"),
                                )
                            } else {
                                serialized_value_to_json_value(&x)
                            };

                            for (a, b) in &role_blocks.clone() {
                                template_messages.push(TemplateMessage {
                                    role: match a {
                                        ChatModelRoles::User => MessageRole::User,
                                        ChatModelRoles::System => MessageRole::System,
                                        ChatModelRoles::Assistant => MessageRole::Assistant,
                                    },
                                    content: chidori_prompt_format::templating::templates::render_template_prompt(&b.as_ref().unwrap().source, &data, &HashMap::new()).unwrap(),
                                    name: None,
                                    function_call: None,
                                });
                            }

                            // TODO: replace this to being fetched from configuration
                            let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
                            let c = crate::library::std::ai::llm::openai::OpenAIChatModel::new(api_key);
                            let result = c.batch(ChatCompletionReq {
                                model: "gpt-3.5-turbo".to_string(),
                                template_messages,
                                ..ChatCompletionReq::default()
                            }).await;
                            if let Ok(ChatCompletionRes { choices, .. }) = result {
                                let mut result_map = HashMap::new();
                                if let Some(name) = &name {
                                    result_map.insert(name.clone(), RKV::String(choices[0].text.as_ref().unwrap().clone()));
                                    return RkyvSerializedValue::Object(result_map);
                                }
                                RKV::String(choices[0].text.as_ref().unwrap().clone())
                            } else {
                                RKV::Null
                            }

                        }.boxed()
                    }),
                ),
            }
        }
        LLMPromptCell::Completion { .. } => OperationNode::new(
            None,
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, x, _| async move { x }.boxed()),
        ),
        LLMPromptCell::Embedding { .. } => OperationNode::new(
            None,
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|_, x, _|  async move { x }.boxed()),
        ),
    }
}
