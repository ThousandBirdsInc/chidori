//! OrcaRouter provider — a named OpenAI-compatible routing gateway.
//!
//! OrcaRouter (<https://www.orcarouter.ai>) is a multi-provider routing
//! gateway: one OpenAI-compatible endpoint in front of Anthropic, OpenAI,
//! Google, DeepSeek, xAI, and more, plus smart routing (`orcarouter/auto`).
//! Setting `ORCAROUTER_API_KEY` registers it, and Chidori then routes every
//! model the explicit providers above didn't claim through it — the same
//! catch-all role the OpenRouter fallback plays, but as a named gateway a
//! user opts into with their own key rather than the OAuth sign-in.
//!
//! The wire format is OpenAI chat-completions, so like OpenRouter this is a
//! thin wrapper over [`OpenAiProvider`] pointed at OrcaRouter's base URL. The
//! only extra work is translating Chidori's model ids (`claude-sonnet-4-6`)
//! into OrcaRouter's namespaced catalog ids (`anthropic/claude-sonnet-4.6`)
//! on the way out.

use anyhow::Result;

use super::openai::OpenAiProvider;
use super::openrouter::hyphen_version_to_dot;
use super::{LlmProvider, LlmRequest, LlmResponse, TokenSink};

/// OrcaRouter's OpenAI-compatible chat endpoint.
const ORCAROUTER_CHAT_URL: &str = "https://api.orcarouter.ai/v1/chat/completions";

/// Env var carrying an OrcaRouter key directly (mirrors the other providers).
pub const ORCAROUTER_API_KEY_ENV: &str = "ORCAROUTER_API_KEY";

/// An OrcaRouter-backed LLM provider. Acts as a catch-all (`supports_model`
/// always true), so it slots in behind any explicit provider and ahead of the
/// OpenRouter fallback — an explicit `ORCAROUTER_API_KEY` wins whenever both
/// gateways are configured.
pub struct OrcaRouterProvider {
    inner: OpenAiProvider,
}

impl OrcaRouterProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            // A single empty prefix makes the inner OpenAI provider match every
            // model; we own routing via `supports_model` below.
            inner: OpenAiProvider::with_base_url(
                api_key,
                ORCAROUTER_CHAT_URL.to_string(),
                vec![String::new()],
            ),
        }
    }

    pub fn with_rate_limit(mut self, rpm: u32) -> Self {
        self.inner = self.inner.with_rate_limit(rpm);
        self
    }
}

#[async_trait::async_trait]
impl LlmProvider for OrcaRouterProvider {
    fn supports_model(&self, _model: &str) -> bool {
        true
    }

    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let mut req = request.clone();
        req.model = to_orcarouter_slug(&request.model);
        self.inner.send(&req).await
    }

    async fn stream(&self, request: &LlmRequest, on_delta: &mut TokenSink) -> Result<LlmResponse> {
        let mut req = request.clone();
        req.model = to_orcarouter_slug(&request.model);
        self.inner.stream(&req, on_delta).await
    }
}

/// Translate a Chidori model id into an OrcaRouter catalog id.
///
/// - Anything already containing `/` is assumed to be an OrcaRouter catalog id
///   and passes through untouched (`orcarouter/auto`, `deepseek/deepseek-chat`).
/// - Claude ids are canonicalized via the Anthropic alias table, then the
///   trailing `-<major>-<minor>` version is rewritten with a dot to match
///   OrcaRouter (`claude-sonnet-4-6` → `anthropic/claude-sonnet-4.6`).
/// - OpenAI ids (`gpt*`, `o1*`, `o3*`, `o4*`) are prefixed with `openai/`.
/// - The bare `auto` router alias is namespaced to `orcarouter/auto` — the
///   OrcaRouter backend keys its routing channels on the namespaced id.
/// - Anything else passes through so an explicit catalog id always wins.
pub fn to_orcarouter_slug(model: &str) -> String {
    if model.contains('/') {
        return model.to_string();
    }
    let canonical = super::anthropic::resolve_alias(model);
    let lower = canonical.to_ascii_lowercase();
    if lower == "auto" {
        return "orcarouter/auto".to_string();
    }
    if lower.starts_with("claude") {
        return format!("anthropic/{}", hyphen_version_to_dot(canonical));
    }
    if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        return format!("openai/{canonical}");
    }
    canonical.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_default_claude_model_to_orcarouter_slug() {
        assert_eq!(
            to_orcarouter_slug("claude-sonnet-4-6"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            to_orcarouter_slug("claude-opus-4-7"),
            "anthropic/claude-opus-4.7"
        );
        assert_eq!(
            to_orcarouter_slug("claude-haiku-4-5"),
            "anthropic/claude-haiku-4.5"
        );
    }

    #[test]
    fn maps_claude_aliases_before_slugging() {
        assert_eq!(
            to_orcarouter_slug("claude-sonnet"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            to_orcarouter_slug("claude-3-5-sonnet"),
            "anthropic/claude-sonnet-4.6"
        );
    }

    #[test]
    fn maps_openai_models() {
        assert_eq!(to_orcarouter_slug("gpt-4o"), "openai/gpt-4o");
        assert_eq!(to_orcarouter_slug("gpt-4.1-mini"), "openai/gpt-4.1-mini");
        assert_eq!(to_orcarouter_slug("o3-mini"), "openai/o3-mini");
    }

    #[test]
    fn namespaces_bare_auto_router_alias() {
        assert_eq!(to_orcarouter_slug("auto"), "orcarouter/auto");
        assert_eq!(to_orcarouter_slug("Auto"), "orcarouter/auto");
    }

    #[test]
    fn passes_through_explicit_catalog_ids_and_unknowns() {
        assert_eq!(to_orcarouter_slug("orcarouter/auto"), "orcarouter/auto");
        assert_eq!(
            to_orcarouter_slug("anthropic/claude-sonnet-4.6"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            to_orcarouter_slug("deepseek/deepseek-chat"),
            "deepseek/deepseek-chat"
        );
        assert_eq!(to_orcarouter_slug("some-local-model"), "some-local-model");
    }
}
