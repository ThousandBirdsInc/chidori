pub mod openai;
use async_trait::async_trait;
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use ts_rs::TS;

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
    model: String,
    frequency_penalty: Option<f64>,

    #[ts(type = "number | null")]
    max_tokens: Option<i64>,

    presence_penalty: Option<f64>,
    stop: Option<Vec<String>>,
    temperature: Option<f64>,

    #[ts(type = "any")]
    response_format: Option<Value>,
    logit_bias: Option<HashMap<String, i32>>,
    user: Option<String>,

    #[ts(type = "number | null")]
    seed: Option<i64>,
    top_p: Option<f64>,
    template_messages: Vec<TemplateMessage>,
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
    async fn embed(&self, chat_completion_req: ChatCompletionReq) -> Result<Vec<f32>, String>;
}
