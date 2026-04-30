pub mod anthropic;
pub mod openai;
pub mod rate_limit;

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
#[derive(Debug, Clone)]
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
    async fn stream(
        &self,
        request: &LlmRequest,
        on_delta: &mut TokenSink,
    ) -> Result<LlmResponse> {
        let response = self.send(request).await?;
        if !response.content.is_empty() {
            on_delta(&response.content);
        }
        Ok(response)
    }
}

fn rpm_env(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok()).filter(|n| *n > 0)
}

/// Registry of LLM providers. Routes requests to the right provider based on model name.
pub struct ProviderRegistry {
    providers: Vec<Box<dyn LlmProvider>>,
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
            let mut p = openai::OpenAiProvider::with_base_url(
                api_key,
                url,
                vec!["".to_string()],
            );
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
