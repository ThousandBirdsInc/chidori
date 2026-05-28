pub mod anthropic;
pub mod openai;
pub mod rate_limit;

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A content block inside a message. Mirrors Anthropic's block-based content model;
/// providers translate to their own wire format on send.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// A chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: blocks,
        }
    }
}

/// A tool schema exposed to the LLM for function calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the tool's input parameters.
    pub input_schema: Value,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A request to an LLM provider.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub temperature: f64,
    pub max_tokens: u64,
    pub tools: Vec<ToolSchema>,
}

/// A response from an LLM provider.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// Concatenated text content from the response (empty if only tool_use blocks).
    pub content: String,
    /// Raw content blocks the assistant produced; used verbatim as the next
    /// assistant message when continuing a tool-use loop.
    pub blocks: Vec<ContentBlock>,
    /// Tool calls the model requested.
    pub tool_calls: Vec<ToolCall>,
    /// Stop reason reported by the provider, normalized to "end_turn" | "tool_use" | other.
    pub stop_reason: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Sink for streaming token deltas while an LLM response is in flight.
/// `Box<dyn FnMut(&str) + Send>` rather than a concrete channel so different
/// callers can route deltas to SSE / tracing / discard as they wish.
pub type TokenSink = Box<dyn FnMut(&str) + Send>;

/// Trait for LLM provider implementations.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// Check if this provider can handle the given model name.
    fn supports_model(&self, model: &str) -> bool;

    /// Send a request and get a response.
    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse>;

    /// Streaming variant. Invokes `on_delta` with each incremental chunk of
    /// text as the provider emits it, then returns the fully assembled
    /// response. The default implementation falls back to `send()` and emits
    /// a single synthetic delta containing the full text — this keeps every
    /// provider usable from the streaming prompt() path, and providers with
    /// real SSE support override it.
    async fn stream(&self, request: &LlmRequest, on_delta: &mut TokenSink) -> Result<LlmResponse> {
        let response = self.send(request).await?;
        if !response.content.is_empty() {
            on_delta(&response.content);
        }
        Ok(response)
    }
}

fn rpm_env(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
}

/// Registry of LLM providers. Routes requests to the right provider based on model name.
pub struct ProviderRegistry {
    providers: Vec<Box<dyn LlmProvider>>,
}

struct StaticProvider {
    response: String,
    tool_call: Option<Value>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for StaticProvider {
    fn supports_model(&self, _model: &str) -> bool {
        true
    }

    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        if call_index == 0 && !request_has_tool_result(request) {
            if let Some(tool_call) = self.tool_call.as_ref() {
                let id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("test-tool-call")
                    .to_string();
                let name = tool_call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("read")
                    .to_string();
                let input = tool_call.get("input").cloned().unwrap_or(Value::Null);
                return Ok(LlmResponse {
                    content: String::new(),
                    blocks: vec![ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }],
                    tool_calls: vec![ToolCall { id, name, input }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 0,
                    output_tokens: 0,
                });
            }
        }
        Ok(LlmResponse {
            content: self.response.clone(),
            blocks: vec![ContentBlock::Text {
                text: self.response.clone(),
            }],
            tool_calls: Vec::new(),
            stop_reason: "end_turn".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        })
    }
}

fn request_has_tool_result(request: &LlmRequest) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_provider_returns_configured_response() {
        let provider = StaticProvider {
            response: "fixed response".to_string(),
            tool_call: None,
            calls: AtomicUsize::new(0),
        };
        let request = LlmRequest {
            model: "any-model".to_string(),
            messages: vec![Message::user_text("hello")],
            system: None,
            temperature: 0.0,
            max_tokens: 10,
            tools: Vec::new(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let response = rt.block_on(provider.send(&request)).unwrap();

        assert_eq!(response.content, "fixed response");
        assert_eq!(response.stop_reason, "end_turn");
    }

    #[test]
    fn static_provider_returns_configured_tool_call_once() {
        let provider = StaticProvider {
            response: "done".to_string(),
            tool_call: Some(serde_json::json!({
                "id": "call_1",
                "name": "read",
                "input": { "path": "notes.txt" }
            })),
            calls: AtomicUsize::new(0),
        };
        let request = LlmRequest {
            model: "any-model".to_string(),
            messages: vec![Message::user_text("hello")],
            system: None,
            temperature: 0.0,
            max_tokens: 10,
            tools: Vec::new(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();

        let first = rt.block_on(provider.send(&request)).unwrap();
        assert_eq!(first.content, "");
        assert_eq!(first.stop_reason, "tool_use");
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].id, "call_1");
        assert_eq!(first.tool_calls[0].name, "read");
        assert_eq!(first.tool_calls[0].input["path"], "notes.txt");

        let second = rt.block_on(provider.send(&request)).unwrap();
        assert_eq!(second.content, "done");
        assert_eq!(second.stop_reason, "end_turn");
        assert!(second.tool_calls.is_empty());
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn LlmProvider>) {
        self.providers.push(provider);
    }

    /// Build a registry from environment variables, registering all available providers.
    ///
    /// Checks for:
    ///   ANTHROPIC_API_KEY — registers the Anthropic provider
    ///   OPENAI_API_KEY — registers the OpenAI provider
    ///   LITELLM_API_URL + LITELLM_API_KEY — registers an OpenAI-compatible provider
    ///     that matches all model names (acts as a catch-all fallback)
    pub fn from_env() -> Self {
        let mut registry = Self::new();

        if let Ok(response) = std::env::var("CHIDORI_TEST_LLM_RESPONSE") {
            let tool_call = std::env::var("CHIDORI_TEST_LLM_TOOL_CALL")
                .ok()
                .and_then(|value| serde_json::from_str(&value).ok());
            registry.register(Box::new(StaticProvider {
                response,
                tool_call,
                calls: AtomicUsize::new(0),
            }));
            return registry;
        }

        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            let mut p = anthropic::AnthropicProvider::new(api_key);
            if let Some(rpm) = rpm_env("CHIDORI_ANTHROPIC_RPM") {
                p = p.with_rate_limit(rpm);
            }
            registry.register(Box::new(p));
        }

        if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            let mut p = openai::OpenAiProvider::new(api_key);
            if let Some(rpm) = rpm_env("CHIDORI_OPENAI_RPM") {
                p = p.with_rate_limit(rpm);
            }
            registry.register(Box::new(p));
        }

        if let Ok(base_url) = std::env::var("LITELLM_API_URL") {
            let api_key = std::env::var("LITELLM_API_KEY").unwrap_or_default();
            let url = if base_url.ends_with("/chat/completions") {
                base_url
            } else {
                format!("{}/chat/completions", base_url.trim_end_matches('/'))
            };
            let mut p = openai::OpenAiProvider::with_base_url(api_key, url, vec!["".to_string()]);
            if let Some(rpm) = rpm_env("CHIDORI_LITELLM_RPM") {
                p = p.with_rate_limit(rpm);
            }
            registry.register(Box::new(p));
        }

        registry
    }

    /// Send a request, routing to the appropriate provider based on model name.
    pub async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        for provider in &self.providers {
            if provider.supports_model(&request.model) {
                return provider.send(request).await;
            }
        }
        bail!(
            "No provider found for model '{}'. Set ANTHROPIC_API_KEY or OPENAI_API_KEY.",
            request.model
        );
    }

    /// Streaming send: routes to the provider's `stream()` implementation
    /// (which falls back to `send()` for providers that don't override it).
    pub async fn stream(
        &self,
        request: &LlmRequest,
        on_delta: &mut TokenSink,
    ) -> Result<LlmResponse> {
        for provider in &self.providers {
            if provider.supports_model(&request.model) {
                return provider.stream(request, on_delta).await;
            }
        }
        bail!(
            "No provider found for model '{}'. Set ANTHROPIC_API_KEY or OPENAI_API_KEY.",
            request.model
        );
    }
}
