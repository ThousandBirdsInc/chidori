pub mod anthropic;
pub mod openai;
pub mod openrouter;
pub mod rate_limit;

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cache lifetime for a provider prompt-cache breakpoint. Serialized as the
/// author-facing short form (`"5m"` / `"1h"`) so it round-trips through prompt
/// options, call-log args, and the request digest unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

/// Request-level cache markers for the stable head of a prompt: the system
/// block and the tool schemas. Conversation-head markers ride on the message
/// they freeze ([`Message::cache_control`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheLayout {
    pub system: Option<CacheTtl>,
    pub tools: Option<CacheTtl>,
}

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
    /// When set, providers that support explicit prompt caching place a cache
    /// breakpoint covering everything up to and including this message (emitted
    /// on its last content block). Pure request metadata: it never changes the
    /// response, only how the prefix is billed. Skipped in serialization when
    /// unset so existing checkpoints round-trip unchanged.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache_control: Option<CacheTtl>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: text.into() }],
            cache_control: None,
        }
    }

    pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: blocks,
            cache_control: None,
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
    /// Cache markers for the system block and tool schemas. Providers without
    /// explicit cache markers (OpenAI caches exact prefixes automatically)
    /// ignore this; an empty layout emits a request byte-identical to before
    /// caching existed.
    pub cache: CacheLayout,
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
    /// Fresh (non-cached) input tokens. Cache reads/creations are reported
    /// separately below so cost accounting can price each at its own rate.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens written to the provider's prompt cache this request
    /// (Anthropic `cache_creation_input_tokens`; billed ~1.25x base input).
    pub cache_creation_tokens: u64,
    /// Input tokens served from the provider's prompt cache (Anthropic
    /// `cache_read_input_tokens` / OpenAI `cached_tokens`; billed at a steep
    /// discount).
    pub cache_read_tokens: u64,
    /// Hidden reasoning text reported by reasoning models (OpenAI-compatible
    /// `reasoning_content`, e.g. DeepSeek). Never fed back into the
    /// conversation; surfaced so authors can inspect why a model spent
    /// output budget before its visible answer.
    pub reasoning: Option<String>,
}

impl Default for LlmResponse {
    fn default() -> Self {
        Self {
            content: String::new(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            stop_reason: "end_turn".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            reasoning: None,
        }
    }
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

    /// Human-readable name for logs and error attribution ("Anthropic",
    /// "OpenAI-compatible endpoint api.deepseek.com", …).
    fn label(&self) -> String {
        "provider".to_string()
    }

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

/// Best-effort host extraction for error labels; avoids pulling in a URL crate.
pub(crate) fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

/// Normalize a user-supplied base URL to its chat-completions endpoint:
/// `https://api.deepseek.com` and `https://api.deepseek.com/v1` both work,
/// as does a full `/chat/completions` URL.
fn chat_completions_url(base_url: String) -> String {
    if base_url.ends_with("/chat/completions") {
        base_url
    } else {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }
}

/// Normalize a user-supplied base URL to its Anthropic Messages endpoint.
/// Follows the `ANTHROPIC_BASE_URL` convention of the official SDKs, which
/// append `/v1/messages` to the base: `https://api.minimax.io/anthropic`,
/// `https://api.moonshot.ai/anthropic/v1` and a full `/v1/messages` URL all
/// resolve to the same endpoint.
pub(crate) fn messages_url(base_url: String) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1/messages") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        format!("{trimmed}/v1/messages")
    }
}

/// Parse a `CHIDORI_*_MODELS` value — a comma-separated list of model-name
/// prefixes a provider should claim — into the prefix list providers match
/// on. `None` (unset/blank) means "every model" for a redirected or
/// compatible endpoint, whose model namespace is its own.
fn parse_model_prefixes(raw: Option<String>) -> Vec<String> {
    let prefixes: Vec<String> = raw
        .unwrap_or_default()
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if prefixes.is_empty() {
        // A single empty prefix matches every model name.
        vec![String::new()]
    } else {
        prefixes
    }
}

/// Whether a prefix list is the "every model" catch-all from
/// [`parse_model_prefixes`].
fn is_catch_all(prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| p.is_empty())
}

fn parse_rpm(value: Option<String>) -> Option<u32> {
    value.and_then(|v| v.parse::<u32>().ok()).filter(|n| *n > 0)
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Shared tail for "no provider matched" errors: every way to configure one.
pub(crate) const NO_PROVIDER_HINT: &str = "Set ANTHROPIC_API_KEY or OPENAI_API_KEY, point \
CHIDORI_OPENAI_COMPAT_URL + CHIDORI_OPENAI_COMPAT_KEY at any OpenAI-compatible endpoint \
(DeepSeek, Groq, Ollama, vLLM, ...), point CHIDORI_ANTHROPIC_COMPAT_URL + \
CHIDORI_ANTHROPIC_COMPAT_KEY at any Anthropic-compatible endpoint (MiniMax, Moonshot, \
Zhipu, LiteLLM, ...), or run `chidori model-login` to sign in with OpenRouter.";

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
                    ..LlmResponse::default()
                });
            }
        }
        Ok(LlmResponse {
            content: self.response.clone(),
            blocks: vec![ContentBlock::Text {
                text: self.response.clone(),
            }],
            ..LlmResponse::default()
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

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
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
    /// Two wire formats are supported, and either one can serve any model:
    ///
    /// Anthropic Messages API shape (`/v1/messages`):
    ///   ANTHROPIC_API_KEY — registers the Anthropic provider for `claude-*`
    ///     models; ANTHROPIC_BASE_URL (the official SDKs' convention)
    ///     redirects it at any Anthropic-compatible endpoint and widens it to
    ///     match all model names
    ///   CHIDORI_ANTHROPIC_COMPAT_URL + CHIDORI_ANTHROPIC_COMPAT_KEY — registers
    ///     an Anthropic-compatible provider (MiniMax, Moonshot/Kimi, Zhipu GLM,
    ///     LiteLLM, a Bedrock/Vertex gateway, …) matching all model names
    ///
    /// OpenAI chat-completions shape (`/chat/completions`):
    ///   OPENAI_API_KEY — registers the OpenAI provider for `gpt-*`/`o*`
    ///     models; OPENAI_BASE_URL redirects it at any OpenAI-compatible
    ///     endpoint and widens it to match all model names
    ///   CHIDORI_OPENAI_COMPAT_URL + CHIDORI_OPENAI_COMPAT_KEY — registers an
    ///     OpenAI-compatible provider (DeepSeek, Groq, Ollama, vLLM, LiteLLM,
    ///     …) matching all model names. LITELLM_API_URL + LITELLM_API_KEY are
    ///     accepted as legacy aliases.
    ///
    /// Each of the four can be narrowed to an explicit model namespace with a
    /// comma-separated prefix list: CHIDORI_ANTHROPIC_MODELS,
    /// CHIDORI_ANTHROPIC_COMPAT_MODELS, CHIDORI_OPENAI_MODELS,
    /// CHIDORI_OPENAI_COMPAT_MODELS. Routing is first-match, and providers
    /// with an explicit namespace (a prefix list, or the un-redirected vendor
    /// defaults) are consulted before catch-alls, so e.g. `ANTHROPIC_BASE_URL`
    /// pointed at a Claude gateway never steals `deepseek-*` requests from a
    /// `CHIDORI_OPENAI_COMPAT_MODELS=deepseek` endpoint. OpenRouter
    /// (`chidori model-login`) is the final fallback.
    pub fn from_env() -> Self {
        let lookup = |name: &str| std::env::var(name).ok();
        Self::from_env_with(&lookup, openrouter::saved_api_key())
    }

    /// [`Self::from_env`] with the environment abstracted for tests: `env`
    /// stands in for `std::env::var`, `openrouter_key` for the saved/explicit
    /// OpenRouter credential.
    pub(crate) fn from_env_with(
        env: &dyn Fn(&str) -> Option<String>,
        openrouter_key: Option<String>,
    ) -> Self {
        let mut registry = Self::new();

        if let Some(response) = env("CHIDORI_TEST_LLM_RESPONSE") {
            let tool_call = env("CHIDORI_TEST_LLM_TOOL_CALL")
                .and_then(|value| serde_json::from_str(&value).ok());
            registry.register(Box::new(StaticProvider {
                response,
                tool_call,
                calls: AtomicUsize::new(0),
            }));
            return registry;
        }

        // (provider, claims an explicit model namespace). Explicit providers
        // register ahead of catch-alls regardless of the order below, so a
        // user's narrowing always wins over a "match everything" endpoint.
        let mut staged: Vec<(Box<dyn LlmProvider>, bool)> = Vec::new();

        if let Some(api_key) = env("ANTHROPIC_API_KEY") {
            let models = non_blank(env("CHIDORI_ANTHROPIC_MODELS"));
            let (mut p, explicit) = match non_blank(env("ANTHROPIC_BASE_URL")) {
                Some(base_url) => {
                    let prefixes = parse_model_prefixes(models);
                    let explicit = !is_catch_all(&prefixes);
                    (
                        anthropic::AnthropicProvider::with_base_url(
                            api_key,
                            messages_url(base_url),
                            prefixes,
                        ),
                        explicit,
                    )
                }
                // Un-redirected: the vendor's own `claude*` namespace unless
                // the user names one.
                None => match models {
                    Some(list) => {
                        let prefixes = parse_model_prefixes(Some(list));
                        let explicit = !is_catch_all(&prefixes);
                        (
                            anthropic::AnthropicProvider::with_base_url(
                                api_key,
                                anthropic::ANTHROPIC_API_URL.to_string(),
                                prefixes,
                            ),
                            explicit,
                        )
                    }
                    None => (anthropic::AnthropicProvider::new(api_key), true),
                },
            };
            if let Some(rpm) = parse_rpm(env("CHIDORI_ANTHROPIC_RPM")) {
                p = p.with_rate_limit(rpm);
            }
            staged.push((Box::new(p), explicit));
        }

        if let Some(api_key) = env("OPENAI_API_KEY") {
            let models = non_blank(env("CHIDORI_OPENAI_MODELS"));
            // OPENAI_BASE_URL is the ecosystem-wide convention for pointing
            // OpenAI clients at a compatible endpoint (DeepSeek, Groq, Ollama,
            // vLLM…). When set, the provider targets it and matches every
            // model name, since the endpoint's model namespace is its own.
            let (mut p, explicit) = match non_blank(env("OPENAI_BASE_URL")) {
                Some(base_url) => {
                    let prefixes = parse_model_prefixes(models);
                    let explicit = !is_catch_all(&prefixes);
                    (
                        openai::OpenAiProvider::with_base_url(
                            api_key,
                            chat_completions_url(base_url),
                            prefixes,
                        ),
                        explicit,
                    )
                }
                None => match models {
                    Some(list) => {
                        let prefixes = parse_model_prefixes(Some(list));
                        let explicit = !is_catch_all(&prefixes);
                        (
                            openai::OpenAiProvider::with_base_url(
                                api_key,
                                openai::OPENAI_API_URL.to_string(),
                                prefixes,
                            ),
                            explicit,
                        )
                    }
                    None => (openai::OpenAiProvider::new(api_key), true),
                },
            };
            if let Some(rpm) = parse_rpm(env("CHIDORI_OPENAI_RPM")) {
                p = p.with_rate_limit(rpm);
            }
            staged.push((Box::new(p), explicit));
        }

        // Vendor-neutral Anthropic-compatible endpoint: anything serving the
        // Messages API shape under its own model namespace.
        if let Some(base_url) = non_blank(env("CHIDORI_ANTHROPIC_COMPAT_URL")) {
            let api_key = env("CHIDORI_ANTHROPIC_COMPAT_KEY").unwrap_or_default();
            let prefixes = parse_model_prefixes(non_blank(env("CHIDORI_ANTHROPIC_COMPAT_MODELS")));
            let explicit = !is_catch_all(&prefixes);
            let mut p = anthropic::AnthropicProvider::with_base_url(
                api_key,
                messages_url(base_url),
                prefixes,
            );
            if let Some(rpm) = parse_rpm(env("CHIDORI_ANTHROPIC_COMPAT_RPM")) {
                p = p.with_rate_limit(rpm);
            }
            staged.push((Box::new(p), explicit));
        }

        // Vendor-neutral OpenAI-compatible endpoint. CHIDORI_OPENAI_COMPAT_*
        // is the documented pair; LITELLM_API_URL/LITELLM_API_KEY remain as
        // legacy aliases from when a LiteLLM proxy was the only tested shape.
        let compat_url =
            non_blank(env("CHIDORI_OPENAI_COMPAT_URL").or_else(|| env("LITELLM_API_URL")));
        if let Some(base_url) = compat_url {
            let api_key = env("CHIDORI_OPENAI_COMPAT_KEY")
                .or_else(|| env("LITELLM_API_KEY"))
                .unwrap_or_default();
            let prefixes = parse_model_prefixes(non_blank(env("CHIDORI_OPENAI_COMPAT_MODELS")));
            let explicit = !is_catch_all(&prefixes);
            let mut p = openai::OpenAiProvider::with_base_url(
                api_key,
                chat_completions_url(base_url),
                prefixes,
            );
            if let Some(rpm) = parse_rpm(env("CHIDORI_OPENAI_COMPAT_RPM"))
                .or_else(|| parse_rpm(env("CHIDORI_LITELLM_RPM")))
            {
                p = p.with_rate_limit(rpm);
            }
            staged.push((Box::new(p), explicit));
        }

        // Stable partition: explicit namespaces first, catch-alls after, each
        // group keeping the declaration order above.
        let (explicit, catch_all): (Vec<_>, Vec<_>) =
            staged.into_iter().partition(|(_, explicit)| *explicit);
        for (p, _) in explicit.into_iter().chain(catch_all) {
            registry.register(p);
        }

        // OpenRouter is the zero-config fallback: an `OPENROUTER_API_KEY`, or a
        // key saved by a prior `chidori model-login` / demo OAuth sign-in. Registered
        // last and matching every model, so it only handles requests no
        // explicit provider above claimed. See [`openrouter`] for the flow.
        if let Some(api_key) = openrouter_key {
            let mut p = openrouter::OpenRouterProvider::new(api_key);
            if let Some(rpm) = parse_rpm(env("CHIDORI_OPENROUTER_RPM")) {
                p = p.with_rate_limit(rpm);
            }
            registry.register(Box::new(p));
        }

        registry
    }

    /// The provider that would serve `model`, if any (first match wins).
    pub fn route(&self, model: &str) -> Option<&dyn LlmProvider> {
        self.providers
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.supports_model(model))
    }

    /// Send a request, routing to the appropriate provider based on model name.
    pub async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        match self.route(&request.model) {
            Some(provider) => {
                tracing::debug!(model = %request.model, provider = %provider.label(), "routing prompt");
                provider.send(request).await
            }
            None => bail!(
                "No provider found for model '{}'. {NO_PROVIDER_HINT}",
                request.model
            ),
        }
    }

    /// Streaming send: routes to the provider's `stream()` implementation
    /// (which falls back to `send()` for providers that don't override it).
    pub async fn stream(
        &self,
        request: &LlmRequest,
        on_delta: &mut TokenSink,
    ) -> Result<LlmResponse> {
        match self.route(&request.model) {
            Some(provider) => {
                tracing::debug!(model = %request.model, provider = %provider.label(), "routing prompt (stream)");
                provider.stream(request, on_delta).await
            }
            None => bail!(
                "No provider found for model '{}'. {NO_PROVIDER_HINT}",
                request.model
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn registry_from(pairs: &[(&str, &str)]) -> ProviderRegistry {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let lookup = move |name: &str| map.get(name).cloned();
        ProviderRegistry::from_env_with(&lookup, None)
    }

    fn route_label(registry: &ProviderRegistry, model: &str) -> Option<String> {
        registry.route(model).map(|p| p.label())
    }

    #[test]
    fn messages_url_normalizes_anthropic_base_urls() {
        for base in [
            "https://api.minimax.io/anthropic",
            "https://api.minimax.io/anthropic/",
            "https://api.minimax.io/anthropic/v1",
            "https://api.minimax.io/anthropic/v1/",
            "https://api.minimax.io/anthropic/v1/messages",
        ] {
            assert_eq!(
                messages_url(base.to_string()),
                "https://api.minimax.io/anthropic/v1/messages",
                "base {base}"
            );
        }
        assert_eq!(
            messages_url("http://localhost:4000".to_string()),
            "http://localhost:4000/v1/messages"
        );
    }

    #[test]
    fn model_prefix_lists_parse_with_catch_all_default() {
        assert_eq!(parse_model_prefixes(None), vec![String::new()]);
        assert!(is_catch_all(&parse_model_prefixes(Some("  ".into()))));
        let parsed = parse_model_prefixes(Some("MiniMax-, glm-,,".into()));
        assert_eq!(parsed, vec!["MiniMax-".to_string(), "glm-".to_string()]);
        assert!(!is_catch_all(&parsed));
    }

    #[test]
    fn url_host_extracts_hosts() {
        assert_eq!(
            url_host("https://api.anthropic.com/v1/messages"),
            Some("api.anthropic.com")
        );
        assert_eq!(
            url_host("http://user:pw@localhost:11434/v1"),
            Some("localhost")
        );
        assert_eq!(url_host("https://"), None);
    }

    #[test]
    fn vendor_keys_route_by_default_namespaces() {
        let registry = registry_from(&[("ANTHROPIC_API_KEY", "a"), ("OPENAI_API_KEY", "o")]);
        assert_eq!(
            route_label(&registry, "claude-sonnet-4-6").as_deref(),
            Some("Anthropic")
        );
        assert_eq!(route_label(&registry, "gpt-4o").as_deref(), Some("OpenAI"));
        assert_eq!(route_label(&registry, "deepseek-chat"), None);
    }

    #[test]
    fn anthropic_base_url_redirects_and_widens_behind_vendor_defaults() {
        // A redirected Anthropic key serves every model the un-redirected
        // OpenAI key does not claim — it never steals `gpt-*`.
        let registry = registry_from(&[
            ("ANTHROPIC_API_KEY", "a"),
            ("ANTHROPIC_BASE_URL", "https://api.moonshot.ai/anthropic"),
            ("OPENAI_API_KEY", "o"),
        ]);
        let moonshot = "Anthropic-compatible endpoint api.moonshot.ai";
        assert_eq!(route_label(&registry, "kimi-k2").as_deref(), Some(moonshot));
        assert_eq!(
            route_label(&registry, "claude-sonnet-4-6").as_deref(),
            Some(moonshot)
        );
        assert_eq!(route_label(&registry, "gpt-4o").as_deref(), Some("OpenAI"));
    }

    #[test]
    fn anthropic_compat_pair_registers_a_catch_all() {
        let registry = registry_from(&[
            (
                "CHIDORI_ANTHROPIC_COMPAT_URL",
                "https://api.minimax.io/anthropic",
            ),
            ("CHIDORI_ANTHROPIC_COMPAT_KEY", "k"),
        ]);
        assert_eq!(
            route_label(&registry, "MiniMax-M2").as_deref(),
            Some("Anthropic-compatible endpoint api.minimax.io")
        );
        assert_eq!(
            route_label(&registry, "gpt-4o").as_deref(),
            Some("Anthropic-compatible endpoint api.minimax.io")
        );
    }

    #[test]
    fn both_api_shapes_coexist_when_one_is_narrowed() {
        // Two catch-alls of different shapes: the one with an explicit model
        // list wins its namespace, the other takes everything else — in
        // either declaration order.
        let registry = registry_from(&[
            (
                "CHIDORI_ANTHROPIC_COMPAT_URL",
                "https://api.minimax.io/anthropic",
            ),
            ("CHIDORI_ANTHROPIC_COMPAT_KEY", "k"),
            ("CHIDORI_OPENAI_COMPAT_URL", "https://api.deepseek.com"),
            ("CHIDORI_OPENAI_COMPAT_KEY", "k"),
            ("CHIDORI_OPENAI_COMPAT_MODELS", "deepseek"),
        ]);
        assert_eq!(
            route_label(&registry, "deepseek-chat").as_deref(),
            Some("OpenAI-compatible endpoint api.deepseek.com")
        );
        assert_eq!(
            route_label(&registry, "MiniMax-M2").as_deref(),
            Some("Anthropic-compatible endpoint api.minimax.io")
        );

        let registry = registry_from(&[
            (
                "CHIDORI_ANTHROPIC_COMPAT_URL",
                "https://api.minimax.io/anthropic",
            ),
            ("CHIDORI_ANTHROPIC_COMPAT_KEY", "k"),
            ("CHIDORI_ANTHROPIC_COMPAT_MODELS", "MiniMax-"),
            ("CHIDORI_OPENAI_COMPAT_URL", "https://api.deepseek.com"),
            ("CHIDORI_OPENAI_COMPAT_KEY", "k"),
        ]);
        assert_eq!(
            route_label(&registry, "MiniMax-M2").as_deref(),
            Some("Anthropic-compatible endpoint api.minimax.io")
        );
        assert_eq!(
            route_label(&registry, "deepseek-chat").as_deref(),
            Some("OpenAI-compatible endpoint api.deepseek.com")
        );
    }

    #[test]
    fn same_model_can_be_served_over_either_shape() {
        // The shape is a deployment choice, not a property of the model:
        // `claude-*` through an OpenAI-compatible gateway…
        let registry = registry_from(&[
            ("CHIDORI_OPENAI_COMPAT_URL", "http://localhost:4000"),
            ("CHIDORI_OPENAI_COMPAT_KEY", "k"),
        ]);
        assert_eq!(
            route_label(&registry, "claude-sonnet-4-6").as_deref(),
            Some("OpenAI-compatible endpoint localhost")
        );
        // …and `gpt-*` through an Anthropic-compatible one.
        let registry = registry_from(&[
            ("CHIDORI_ANTHROPIC_COMPAT_URL", "http://localhost:4000"),
            ("CHIDORI_ANTHROPIC_COMPAT_KEY", "k"),
        ]);
        assert_eq!(
            route_label(&registry, "gpt-4o").as_deref(),
            Some("Anthropic-compatible endpoint localhost")
        );
    }

    #[test]
    fn explicit_vendor_models_override_default_namespace() {
        // CHIDORI_ANTHROPIC_MODELS widens the un-redirected vendor provider
        // to names Anthropic itself serves under aliases/custom deployments.
        let registry = registry_from(&[
            ("ANTHROPIC_API_KEY", "a"),
            ("CHIDORI_ANTHROPIC_MODELS", "claude,my-finetune-"),
        ]);
        assert_eq!(
            route_label(&registry, "my-finetune-1").as_deref(),
            Some("Anthropic")
        );
        assert_eq!(route_label(&registry, "gpt-4o"), None);
    }

    #[test]
    fn openrouter_stays_the_last_fallback() {
        let lookup = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| "a".to_string());
        let registry = ProviderRegistry::from_env_with(&lookup, Some("or-key".to_string()));
        assert_eq!(
            route_label(&registry, "claude-sonnet-4-6").as_deref(),
            Some("Anthropic")
        );
        assert_eq!(
            route_label(&registry, "deepseek-chat").as_deref(),
            Some("OpenRouter")
        );
    }

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
            cache: CacheLayout::default(),
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
            cache: CacheLayout::default(),
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
