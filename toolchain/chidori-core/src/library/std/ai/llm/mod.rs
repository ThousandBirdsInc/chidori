pub mod openai;

use async_trait::async_trait;
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::pin::Pin;
use ts_rs::TS;
use chidori_prompt_format::templating::templates::{ChatModelRoles, TemplateWithSource};
use crate::execution::execution::ExecutionState;
use crate::execution::primitives::serialized_value::{RkyvSerializedValue, serialized_value_to_json_value};
use crate::library::std::ai::llm::openai::OpenAIChatModel;

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

#[derive(TS, Debug, Serialize, Deserialize)]
#[ts(export, export_to = "package_node/types/")]
pub enum MessageRole {
    User,
    System,
    Assistant,
    Function,
}

#[derive(TS, Debug, Serialize, Deserialize)]
#[ts(export, export_to = "package_node/types/")]
pub struct FunctionCall {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(TS, Debug, Serialize, Deserialize)]
#[ts(export, export_to = "package_node/types/")]
pub struct TemplateMessage {
    pub role: MessageRole,
    pub content: String,
    pub name: Option<String>,
    pub function_call: Option<FunctionCall>,
}

#[derive(TS, Debug, Serialize, Deserialize)]
#[ts(export, export_to = "package_node/types/")]
pub struct ChatCompletionReq {
    pub model: String,
    pub frequency_penalty: Option<f64>,

    #[ts(type = "number | null")]
    pub max_tokens: Option<i64>,

    pub presence_penalty: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub temperature: Option<f64>,

    #[ts(type = "any")]
    pub response_format: Option<Value>,
    pub logit_bias: Option<HashMap<String, i32>>,
    pub user: Option<String>,

    #[ts(type = "number | null")]
    pub seed: Option<i64>,
    pub top_p: Option<f64>,
    pub template_messages: Vec<TemplateMessage>,
}

impl Default for ChatCompletionReq {
    fn default() -> Self {
        Self {
            model: String::from("davinci"),
            frequency_penalty: None,
            max_tokens: None,
            presence_penalty: None,
            stop: None,
            temperature: None,
            response_format: None,
            logit_bias: None,
            user: None,
            seed: None,
            top_p: None,
            template_messages: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionChoice {
    pub text: Option<String>,
    pub index: i32,

    pub logprobs: Option<Value>,
    pub finish_reason: String,
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

pub async fn ai_llm_run_chat_model(
    execution_state: &ExecutionState,
    payload: RkyvSerializedValue,
    role_blocks: Vec<(ChatModelRoles, Option<TemplateWithSource>)>,
    name: Option<String>,
    is_function_invocation: bool,
) -> RkyvSerializedValue {
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

    // TODO: replace this to being fetched from configuration
    let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
    let api_url_v1: &str = "https://api.openai.com/v1";
    let c = crate::library::std::ai::llm::openai::OpenAIChatModel::new(api_url_v1.to_string(), api_key);
    let result = c.batch(ChatCompletionReq {
        model: "gpt-3.5-turbo".to_string(),
        frequency_penalty: None,
        max_tokens: None,
        presence_penalty: None,
        stop: None,
        temperature: None,
        response_format: None,
        logit_bias: None,
        user: None,
        seed: None,
        top_p: None,
        template_messages,
    }).await;
    if let Ok(ChatCompletionRes { choices, .. }) = result {
        // if invoked as a function don't nest the result in a named key, return the response as a direct string
        let mut result_map = HashMap::new();
        if !is_function_invocation {
            if let Some(name) = &name {
                result_map.insert(name.clone(), RkyvSerializedValue::String(choices[0].text.as_ref().unwrap().clone()));
                return RkyvSerializedValue::Object(result_map);
            }
        }
        RkyvSerializedValue::String(choices[0].text.as_ref().unwrap().clone())
    } else {
        RkyvSerializedValue::Null
    }
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
