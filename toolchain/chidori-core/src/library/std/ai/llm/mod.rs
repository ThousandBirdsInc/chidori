pub mod openai;

use async_trait::async_trait;
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::pin::Pin;
use chidori_prompt_format::templating::templates::{ChatModelRoles, TemplateWithSource};
use crate::cells::{LLMCodeGenCellChatConfiguration, LLMPromptCellChatConfiguration, TextRange};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::operation::InputSignature;
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue, serialized_value_to_json_value};
use crate::library::std::ai::llm::openai::OpenAIChatModel;
use crate::sdk::md::interpret_code_block;

#[derive(Debug)]
pub enum LLMErrors {
    ConnectionError(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        }
    }
}

pub struct LLMStream {
    response: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    first_chunk: bool,
    usage: Usage,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    System,
    Assistant,
    Function,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateMessage {
    pub role: MessageRole,
    pub content: String,
    pub name: Option<String>,
    pub function_call: Option<FunctionCall>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Function {
    pub name: String,
    pub description: Option<String>,
    pub parameters: FunctionParameters,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum JSONSchemaType {
    Object,
    Number,
    String,
    Array,
    Null,
    Boolean,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct JSONSchemaDefine {
    pub schema_type: Option<JSONSchemaType>,
    pub description: Option<String>,
    pub enum_values: Option<Vec<String>>,
    pub properties: Option<HashMap<String, Box<JSONSchemaDefine>>>,
    pub required: Option<Vec<String>>,
    pub items: Option<Box<JSONSchemaDefine>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionParameters {
    pub schema_type: JSONSchemaType,
    pub properties: Option<HashMap<String, Box<JSONSchemaDefine>>>,
    pub required: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Tool {
    tool_type: String,
    function: Function,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ToolChoiceType {
    None,
    Auto,
    ToolChoice { tool: Tool },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionReq {
    pub config: LLMPromptCellChatConfiguration,
    pub template_messages: Vec<TemplateMessage>,
    pub tool_choice: Option< crate::library::std::ai::llm::ToolChoiceType >,
    pub tools: Option<Vec< crate::library::std::ai::llm::Tool >>
}

impl Default for ChatCompletionReq {
    fn default() -> Self {
        Self {
            config: LLMPromptCellChatConfiguration {
                import: None,
                function_name: None,
                model: String::from("gpt-3.5-turbo"),
                api_url: None,
                frequency_penalty: None,
                max_tokens: None,
                presence_penalty: None,
                stop: None,
                temperature: None,
                logit_bias: None,
                user: None,
                seed: None,
                top_p: None,
            },
            template_messages: Vec::new(),
            tool_choice: None,
            tools: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionToolCallFunction {
    pub name: Option<String>,
    pub arguments: Option<RkyvSerializedValue>
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionToolCall {
    pub id: String,
    pub ty: String,
    pub function: ChatCompletionToolCallFunction
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionChoice {
    pub text: Option<String>,
    pub index: i32,
    pub logprobs: Option<Value>,
    pub finish_reason: String,
    pub tool_calls: Option<Vec<ChatCompletionToolCall>>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionRes {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EmbeddingReq {
    content: String,
    model: String,
    frequency_penalty: Option<f32>,
    max_tokens: Option<i32>,
    presence_penalty: Option<f32>,
    stop: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletionReq {
    model: String,
    frequency_penalty: Option<f32>,
    max_tokens: Option<i32>,
    presence_penalty: Option<f32>,
    stop: Option<Vec<String>>,
}

// TODO: streams should return a struct that includes the stream and a method to capture the usage

#[async_trait]
pub trait ChatModelBatch {
    async fn batch(
        &self,
        chat_completion_req: ChatCompletionReq,
    ) -> Result<ChatCompletionRes, String>;
}

#[async_trait]
trait ChatModelStream {
    async fn stream(&self, chat_completion_req: ChatCompletionReq) -> Result<LLMStream, String>;
}

#[async_trait]
trait CompletionModel {
    async fn batch(
        &self,
        chat_completion_req: ChatCompletionReq,
    ) -> Result<ChatCompletionRes, String>;
    async fn stream(&self, chat_completion_req: ChatCompletionReq) -> Result<LLMStream, String>;
}

#[async_trait]
trait EmbeddingModel {
    async fn embed(&self, embedding_req: EmbeddingReq) -> Result<Vec<f32>, String>;
}


pub async fn ai_llm_run_completion_model(
    execution_state: &ExecutionState,
    payload: RkyvSerializedValue,
    role_blocks: Vec<(ChatModelRoles, Option<TemplateWithSource>)>,
    name: Option<String>,
    is_function_invocation: bool,
) -> RkyvSerializedValue {
    RkyvSerializedValue::Null
}


pub async fn ai_llm_run_embedding_model(
    execution_state: &ExecutionState,
    payload: RkyvSerializedValue,
    template: TemplateWithSource,
    name: Option<String>,
    is_function_invocation: bool,
) -> RkyvSerializedValue {
    let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
    let api_url_v1: &str = "https://api.openai.com/v1";
    let model = OpenAIChatModel::new(api_url_v1.to_string(), api_key);
    let data = template_data_payload_from_rkyv(&payload);
    let result = model.embed(EmbeddingReq {
        content: chidori_prompt_format::templating::templates::render_template_prompt(&template.source, &data, &HashMap::new()).unwrap(),
        model: "text-embedding-3-small".to_string(),
        frequency_penalty: None,
        max_tokens: None,
        presence_penalty: None,
        stop: None,
    }).await;
    if let Ok(result) = result {
        // if invoked as a function don't nest the result in a named key, return the response as a direct string
        let mut result_map = HashMap::new();
        if !is_function_invocation {
            if let Some(name) = &name {
                result_map.insert(name.clone(), RkyvSerializedValue::Array(result.iter().map(|v| RkyvSerializedValue::Float(*v)).collect()));
                return RkyvSerializedValue::Object(result_map);
            }
        }
        RkyvSerializedValue::Array(result.iter().map(|v| RkyvSerializedValue::Float(*v)).collect())
    } else {
        RkyvSerializedValue::Null
    }
}

fn input_signature_to_json_properties(input_signature: InputSignature) -> HashMap<String, Box<JSONSchemaDefine>> {
    let mut properties = HashMap::new();
    for (k, v) in input_signature.args {
        properties.insert(k, Box::new(JSONSchemaDefine {
            schema_type: Some(JSONSchemaType::String),
            description: None,
            enum_values: None,
            properties: None,
            required: None,
            items: None,
        }));
    }
    for (k, v) in input_signature.kwargs {
        properties.insert(k, Box::new(JSONSchemaDefine {
            schema_type: Some(JSONSchemaType::String),
            description: None,
            enum_values: None,
            properties: None,
            required: None,
            items: None,
        }));
    }
    for (k, v) in input_signature.globals {
        properties.insert(k, Box::new(JSONSchemaDefine {
            schema_type: Some(JSONSchemaType::String),
            description: None,
            enum_values: None,
            properties: None,
            required: None,
            items: None,
        }));
    }
    properties
}

pub async fn ai_llm_run_chat_model(
    execution_state: &ExecutionState,
    payload: RkyvSerializedValue,
    role_blocks: Vec<(ChatModelRoles, Option<TemplateWithSource>)>,
    name: Option<String>,
    is_function_invocation: bool,
    configuration: LLMPromptCellChatConfiguration
) -> anyhow::Result<(RkyvSerializedValue, Option<ExecutionState>)> {
    let mut template_messages: Vec<TemplateMessage> = Vec::new();
    let data = template_data_payload_from_rkyv(&payload);

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

    let tools = infer_tool_usage_from_imports(execution_state, &configuration.import);

    let api_url_v1 = configuration.api_url.clone();
    let c = crate::library::std::ai::llm::openai::OpenAIChatModel::new(api_url_v1.unwrap_or("http://localhost:4000/v1".to_string()), "".to_string());



    let result = c.batch(ChatCompletionReq {
        config: configuration.clone(),
        template_messages,
        tool_choice: None,
        tools: if tools.is_empty() {
            None
        } else {
            Some(tools)
        },
    }).await;


    if let Ok(ChatCompletionRes { choices, .. }) = result {
        let mut results = vec![];
        for choice in choices {

            // TODO: how do we handle tools in the case of reference as a function

            let mut result_map = HashMap::new();
            match choice.tool_calls {
                Some(tool_calls) => {
                    for tool_call in tool_calls {
                        if let Some(function_name) = tool_call.function.name {
                            let args = tool_call.function.arguments.unwrap_or(RkyvSerializedValue::Null);
                            let args = RkyvObjectBuilder::new().insert_value("kwargs", args).build();
                            let (dispatch_result, _) = execution_state.dispatch(&function_name, args).await?;
                            result_map.insert(function_name, dispatch_result);
                        }
                    }
                    let result = if is_function_invocation {
                        RkyvSerializedValue::Object(result_map)
                    } else {
                        RkyvObjectBuilder::new().insert_value(name.as_deref().unwrap(), RkyvSerializedValue::Object(result_map)).build()
                    };
                    results.push(result);
                }
                None => {
                    let result = if is_function_invocation {
                        RkyvSerializedValue::String(choice.text.as_ref().unwrap().clone())
                    } else {
                        let name = name.as_ref().unwrap();
                        let text = choice.text.as_ref().unwrap().clone();
                        result_map.insert(name.clone(), RkyvSerializedValue::String(text));
                        RkyvSerializedValue::Object(result_map)
                    };
                    results.push(result)
                }
            }
        }


        let out = if results.len() == 1 {
            results[0].clone()
        } else {
            RkyvSerializedValue::Array(results)
        };
        Ok((out, None))
    } else {
        Ok((RkyvSerializedValue::Null, None))
    }
}

pub async fn ai_llm_code_generation_chat_model(
    execution_state: &ExecutionState,
    payload: RkyvSerializedValue,
    role_blocks: Vec<(ChatModelRoles, Option<TemplateWithSource>)>,
    name: Option<String>,
    is_function_invocation: bool,
    configuration: LLMCodeGenCellChatConfiguration
) -> anyhow::Result<(RkyvSerializedValue, Option<ExecutionState>)> {
    let mut template_messages: Vec<TemplateMessage> = Vec::new();
    let data = template_data_payload_from_rkyv(&payload);

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

    let api_url_v1 = configuration.api_url.unwrap_or("http://localhost:4000/v1".to_string());
    let c = crate::library::std::ai::llm::openai::OpenAIChatModel::new(api_url_v1, "".to_string());

    let result = c.batch(ChatCompletionReq {
        config: LLMPromptCellChatConfiguration {
            import: None,
            function_name: None,
            model: configuration.model.clone(),
            api_url: None,
            frequency_penalty: configuration.frequency_penalty.clone(),
            max_tokens: configuration.max_tokens.clone(),
            presence_penalty: configuration.presence_penalty.clone(),
            stop: configuration.stop.clone(),
            temperature: configuration.temperature.clone(),
            logit_bias: configuration.logit_bias.clone(),
            user: configuration.user.clone(),
            seed: configuration.seed.clone(),
            top_p: configuration.top_p.clone(),
        },
        template_messages,
        tool_choice: None,
        tools: None,
    }).await;


    if let Ok(ChatCompletionRes { choices, .. }) = result {
        for choice in choices {
            let text = choice.text.as_ref().unwrap().clone();
            println!("Code generation cell run, returning this payload: {}", &text);
            let mut new_execution_state = execution_state.clone();

            let mut cells = vec![];
            crate::sdk::md::extract_code_blocks(&text)
                .iter()
                .filter_map(|block| interpret_code_block(block))
                .for_each(|block| { cells.push(block); });
            cells.sort();

            for cell in cells {
                let (s, _) = new_execution_state.update_op(cell, None)?;
                new_execution_state = s;
            }

            return Ok((RkyvSerializedValue::String(text.clone()), Some(new_execution_state)));
        }
        Ok((RkyvSerializedValue::Null, None))
    } else {
        Ok((RkyvSerializedValue::Null, None))
    }
}


pub fn infer_tool_usage_from_imports(execution_state: &ExecutionState, imports: &Option<Vec<String>>) -> Vec<Tool> {
    let mut tools = vec![];
    if let Some(imports) = imports {
        let mut imports = imports.clone();
        for import in imports {
            let function = execution_state.function_name_to_metadata.get(&import).unwrap();
            tools.push(Tool {
                tool_type: "function".to_string(),
                function: Function {
                    name: import.to_string(),
                    description: None,
                    parameters: FunctionParameters {
                        schema_type: JSONSchemaType::Object,
                        properties: Some(input_signature_to_json_properties(function.input_signature.clone())),
                        required: None,
                    },
                },
            });
        }
    }
    tools
}

fn template_data_payload_from_rkyv(payload: &RkyvSerializedValue) -> Value {
    let data = if let RkyvSerializedValue::Object(ref m) = payload {
        if let Some(m) = m.get("globals") {
            serialized_value_to_json_value(m)
        } else if let Some(m) = m.get("kwargs") {
            serialized_value_to_json_value(m)
        } else {
            serialized_value_to_json_value(&payload)
        }
    } else {
        serialized_value_to_json_value(&payload)
    };
    data
}


#[cfg(test)]
mod test {
    use indoc::indoc;
    use crate::cells::{CellTypes, CodeCell, LLMPromptCellChatConfiguration, SupportedLanguage, TextRange};
    use crate::execution::execution::ExecutionState;
    use crate::library::std::ai::llm::infer_tool_usage_from_imports;

    #[tokio::test]
    async fn test_tool_usage_inference() -> anyhow::Result<()> {
        let mut state = ExecutionState::new_with_random_id();
        let (mut state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        import asyncio
                        async def demo():
                            await asyncio.sleep(1)
                            return 100 + await demo_second_function_call()
                        "#}),
            function_invocation: None,
        }, TextRange::default()), Some(0))?;
        let (mut state, _) = state.update_op(CellTypes::Code(CodeCell {
            name: None,
            language: SupportedLanguage::PyO3,
            source_code: String::from(indoc! {r#"
                        def complex_args(a, b, c=2, d=3):
                            return a + b + c + d
                        "#}),
            function_invocation: None,
        }, TextRange::default()), Some(0))?;

        insta::with_settings!({
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(infer_tool_usage_from_imports(&state, &Some(vec![
                "demo".to_string(),
                "complex_args".to_string()
            ])),
                {
                    "[].function.parameters.properties" => insta::sorted_redaction(),
                    "[]" => insta::sorted_redaction(),
                }
            );
        });
        Ok(())
    }
}