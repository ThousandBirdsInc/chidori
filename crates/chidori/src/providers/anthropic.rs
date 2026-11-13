use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use std::sync::Arc;
use std::time::Duration;

use super::rate_limit::RateLimiter;
use super::{
    url_host, CacheTtl, ContentBlock, LlmProvider, LlmRequest, LlmResponse, TokenSink, ToolCall,
};

pub(crate) const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta header required for the extended (1h) cache TTL; only sent when a
/// breakpoint actually requests it.
const ANTHROPIC_EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
/// Anthropic allows at most 4 cache breakpoints per request.
const ANTHROPIC_MAX_CACHE_BREAKPOINTS: usize = 4;

/// A provider speaking the Anthropic Messages API shape (`POST /v1/messages`,
/// `x-api-key` + `anthropic-version` headers, block-based content, SSE
/// `content_block_delta` streaming). Targets api.anthropic.com by default, but
/// the same wire format is served by a growing set of other vendors and
/// proxies (MiniMax, Moonshot/Kimi, Zhipu GLM, DeepSeek's Anthropic endpoint,
/// LiteLLM, Bedrock/Vertex gateways…), so the endpoint and the model
/// namespace it claims are both configurable — mirroring
/// [`super::openai::OpenAiProvider`] for the OpenAI chat-completions shape.
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: Client,
    /// Model name prefixes this provider handles. Empty means the default
    /// Anthropic namespace (`claude*`); a single empty-string prefix matches
    /// every model (catch-all for a redirected/compatible endpoint).
    model_prefixes: Vec<String>,
    rate_limiter: Option<Arc<RateLimiter>>,
    /// Display name used in error messages: "Anthropic" for the real
    /// endpoint, otherwise "Anthropic-compatible endpoint <host>" so a
    /// failure is attributed to the provider the user actually configured.
    label: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: ANTHROPIC_API_URL.to_string(),
            client: Client::new(),
            model_prefixes: Vec::new(),
            rate_limiter: None,
            label: "Anthropic".to_string(),
        }
    }

    /// Create a provider pointing at any endpoint that speaks the Anthropic
    /// Messages API. `base_url` must be the full `/v1/messages` URL (see
    /// [`super::messages_url`] for normalizing a user-supplied base). An empty
    /// `model_prefixes` keeps the default `claude*` matching.
    pub fn with_base_url(api_key: String, base_url: String, model_prefixes: Vec<String>) -> Self {
        let label = match url_host(&base_url) {
            Some(host) if host != "api.anthropic.com" => {
                format!("Anthropic-compatible endpoint {host}")
            }
            _ => "Anthropic".to_string(),
        };
        Self {
            api_key,
            base_url,
            client: Client::new(),
            model_prefixes,
            rate_limiter: None,
            label,
        }
    }

    pub fn with_rate_limit(mut self, rpm: u32) -> Self {
        self.rate_limiter = Some(Arc::new(RateLimiter::new(rpm)));
        self
    }
}

#[derive(Deserialize)]
struct AnthropicResponseBody {
    content: Vec<AnthropicResponseBlock>,
    usage: AnthropicUsage,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    /// Tokens written to the prompt cache this request. Absent on responses
    /// that involve no caching (and from older API versions), so default to 0.
    #[serde(default)]
    cache_creation_input_tokens: u64,
    /// Tokens served from the prompt cache this request.
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
struct AnthropicError {
    error: AnthropicErrorBody,
}

#[derive(Deserialize)]
struct AnthropicErrorBody {
    message: String,
}

fn cache_control_json(ttl: CacheTtl) -> Value {
    match ttl {
        CacheTtl::FiveMinutes => json!({ "type": "ephemeral" }),
        CacheTtl::OneHour => json!({ "type": "ephemeral", "ttl": "1h" }),
    }
}

/// Build the Anthropic request body, emitting `cache_control` markers for the
/// request's cache layout, and report whether any marker needs the extended
/// (1h) TTL beta header. A request with no markers produces a body identical
/// to the pre-caching wire format (plain string `system`, unannotated blocks).
///
/// Anthropic caps breakpoints at 4. The system and tools marks always survive
/// (they cover the most stable prefix); when message-level marks exceed the
/// remaining budget the *latest* marks win — a later mark's prefix subsumes an
/// earlier one's, so dropping the oldest loses no coverage.
fn build_request_body(request: &LlmRequest, stream: bool) -> Result<(Value, bool)> {
    let mut needs_one_hour = false;
    let mut note_ttl = |ttl: CacheTtl| {
        if ttl == CacheTtl::OneHour {
            needs_one_hour = true;
        }
        cache_control_json(ttl)
    };

    let mut budget = ANTHROPIC_MAX_CACHE_BREAKPOINTS;
    if request.cache.system.is_some() && request.system.is_some() {
        budget -= 1;
    }
    if request.cache.tools.is_some() && !request.tools.is_empty() {
        budget -= 1;
    }
    let marked_indices: Vec<usize> = request
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.cache_control.map(|_| i))
        .collect();
    let kept_message_marks: std::collections::HashSet<usize> =
        marked_indices.iter().rev().take(budget).copied().collect();
    if marked_indices.len() > kept_message_marks.len() {
        tracing::debug!(
            dropped = marked_indices.len() - kept_message_marks.len(),
            "coalesced cache breakpoints beyond Anthropic's limit of {ANTHROPIC_MAX_CACHE_BREAKPOINTS}"
        );
    }

    let messages: Vec<Value> = request
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut blocks: Vec<Value> = m
                .content
                .iter()
                .map(content_block_to_anthropic_json)
                .collect();
            if let Some(ttl) = m.cache_control.filter(|_| kept_message_marks.contains(&i)) {
                if let Some(last) = blocks.last_mut() {
                    last["cache_control"] = note_ttl(ttl);
                }
            }
            json!({ "role": m.role, "content": blocks })
        })
        .collect();

    let mut tools_json: Vec<Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect();
    if let Some(ttl) = request.cache.tools {
        if let Some(last) = tools_json.last_mut() {
            last["cache_control"] = note_ttl(ttl);
        }
    }

    let mut body = json!({
        "model": resolve_alias(&request.model),
        "messages": messages,
        "max_tokens": request.max_tokens,
    });
    if stream {
        body["stream"] = Value::Bool(true);
    }
    if let Some(ref system) = request.system {
        body["system"] = match request.cache.system {
            // Caching the system prompt requires the structured-content form.
            Some(ttl) => json!([{
                "type": "text",
                "text": system,
                "cache_control": note_ttl(ttl),
            }]),
            None => Value::String(system.clone()),
        };
    }
    if !tools_json.is_empty() {
        body["tools"] = Value::Array(tools_json);
    }
    Ok((body, needs_one_hour))
}

/// Map common short aliases to canonical Anthropic model ids. Agents
/// in the wild often write `model = "claude-sonnet"` rather than the
/// full versioned id (`claude-sonnet-4-6`); without this resolution
/// Anthropic returns 404.
///
/// Falls through unchanged for any model name that already looks
/// versioned, so user-supplied full ids keep working.
pub(crate) fn resolve_alias(model: &str) -> &str {
    match model {
        "claude-sonnet" => "claude-sonnet-4-6",
        "claude-opus" => "claude-opus-4-7",
        "claude-haiku" => "claude-haiku-4-5",
        // Common 3.x-era shorthand still floating around in older
        // example files. Map to the latest 4.x family member so they
        // run rather than 404.
        "claude-3-sonnet" | "claude-3-5-sonnet" | "claude-3-7-sonnet" => "claude-sonnet-4-6",
        "claude-3-opus" => "claude-opus-4-7",
        "claude-3-haiku" | "claude-3-5-haiku" => "claude-haiku-4-5",
        other => other,
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
    fn supports_model(&self, model: &str) -> bool {
        if !self.model_prefixes.is_empty() {
            return self
                .model_prefixes
                .iter()
                .any(|p| model.starts_with(p.as_str()));
        }
        model.starts_with("claude")
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let (body, needs_one_hour) = build_request_body(request, false)?;

        let mut attempt = 0u32;
        loop {
            if let Some(ref rl) = self.rate_limiter {
                rl.acquire().await;
            }

            let mut req = self
                .client
                .post(&self.base_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json");
            if needs_one_hour {
                req = req.header("anthropic-beta", ANTHROPIC_EXTENDED_CACHE_TTL_BETA);
            }
            let resp = req
                .json(&body)
                .send()
                .await
                .with_context(|| format!("Failed to send request to {}", self.base_url))?;

            let status = resp.status();

            if status.as_u16() == 429 {
                attempt += 1;
                if attempt >= 8 {
                    bail!("{} rate limit: exceeded max retries after 429", self.label);
                }
                let wait = retry_after_duration(resp.headers(), attempt);
                tracing::warn!(attempt, ?wait, "{} 429 — backing off", self.label);
                tokio::time::sleep(wait).await;
                continue;
            }

            let resp_text = resp
                .text()
                .await
                .with_context(|| format!("Failed to read {} response", self.label))?;
            if !status.is_success() {
                if let Ok(err) = serde_json::from_str::<AnthropicError>(&resp_text) {
                    bail!(
                        "{} API error ({}): {}",
                        self.label,
                        status,
                        err.error.message
                    );
                }
                bail!("{} API error ({}): {}", self.label, status, resp_text);
            }

            let parsed: AnthropicResponseBody = serde_json::from_str(&resp_text)
                .with_context(|| format!("Failed to parse {} response", self.label))?;

            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();
            let mut blocks = Vec::new();
            for block in parsed.content {
                match block {
                    AnthropicResponseBlock::Text { text } => {
                        text_parts.push(text.clone());
                        blocks.push(ContentBlock::Text { text });
                    }
                    AnthropicResponseBlock::ToolUse { id, name, input } => {
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        });
                        blocks.push(ContentBlock::ToolUse { id, name, input });
                    }
                    AnthropicResponseBlock::Other => {}
                }
            }

            return Ok(LlmResponse {
                content: text_parts.join(""),
                blocks,
                tool_calls,
                stop_reason: parsed.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
                cache_creation_tokens: parsed.usage.cache_creation_input_tokens,
                cache_read_tokens: parsed.usage.cache_read_input_tokens,
                reasoning: None,
            });
        }
    }

    /// Anthropic SSE streaming. Sends the request with `stream: true` and
    /// parses the resulting `event: <name>\ndata: <json>\n\n` frames. On
    /// every `content_block_delta` with a `text_delta` chunk, we invoke
    /// `on_delta`. Tool-use blocks and usage totals are accumulated the
    /// same way as the non-streaming path so the returned LlmResponse is
    /// identical in shape.
    async fn stream(&self, request: &LlmRequest, on_delta: &mut TokenSink) -> Result<LlmResponse> {
        use futures::StreamExt;

        if let Some(ref rl) = self.rate_limiter {
            rl.acquire().await;
        }

        let (body, needs_one_hour) = build_request_body(request, true)?;

        let resp = 'retry: {
            let mut attempt = 0u32;
            loop {
                if let Some(ref rl) = self.rate_limiter {
                    rl.acquire().await;
                }
                let mut req = self
                    .client
                    .post(&self.base_url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream");
                if needs_one_hour {
                    req = req.header("anthropic-beta", ANTHROPIC_EXTENDED_CACHE_TTL_BETA);
                }
                let r = req
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("streaming request to {}", self.base_url))?;

                if r.status().as_u16() == 429 {
                    attempt += 1;
                    if attempt >= 8 {
                        bail!("{} rate limit: exceeded max retries after 429", self.label);
                    }
                    let wait = retry_after_duration(r.headers(), attempt);
                    tracing::warn!(attempt, ?wait, "{} 429 (stream) — backing off", self.label);
                    tokio::time::sleep(wait).await;
                    continue;
                }

                break 'retry r;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("{} stream error ({}): {}", self.label, status, text);
        }

        // Accumulators for the final response. Text chunks land in
        // `text_buf`; tool_use blocks arrive as a start event followed by
        // input_json_delta events carrying partial JSON — we reassemble.
        let mut text_buf = String::new();
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut cache_creation_tokens: u64 = 0;
        let mut cache_read_tokens: u64 = 0;
        let mut stop_reason: String = "end_turn".into();

        // Per content-block scratch space keyed by the block index.
        let mut pending_text_idx: Option<usize> = None;
        let mut pending_text: String = String::new();
        let mut pending_tool: Option<(String, String, String)> = None; // (id, name, json_buf)

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("reading {} stream", self.label))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // SSE frames are separated by \n\n. Drain complete frames and
            // leave any partial tail in `buffer`.
            while let Some(idx) = buffer.find("\n\n") {
                let frame = buffer[..idx].to_string();
                buffer.drain(..idx + 2);

                // Each frame is one or more `field: value` lines; only the
                // `data:` line carries JSON. `event:` gives the event name
                // but the JSON itself includes `type` so we can ignore it.
                let data_line = frame
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .unwrap_or("");
                if data_line.is_empty() || data_line == "[DONE]" {
                    continue;
                }
                let Ok(event): std::result::Result<Value, _> = serde_json::from_str(data_line)
                else {
                    continue;
                };
                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "message_start" => {
                        if let Some(usage) = event.get("message").and_then(|m| m.get("usage")) {
                            input_tokens = usage
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            cache_creation_tokens = usage
                                .get("cache_creation_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            cache_read_tokens = usage
                                .get("cache_read_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                        }
                    }
                    "content_block_start" => {
                        let idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let block = event.get("content_block");
                        match block.and_then(|b| b.get("type")).and_then(|v| v.as_str()) {
                            Some("text") => {
                                pending_text_idx = Some(idx);
                                pending_text.clear();
                            }
                            Some("tool_use") => {
                                let id = block
                                    .and_then(|b| b.get("id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .and_then(|b| b.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                pending_tool = Some((id, name, String::new()));
                            }
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        if let Some(delta) = event.get("delta") {
                            match delta.get("type").and_then(|v| v.as_str()) {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                        pending_text.push_str(text);
                                        text_buf.push_str(text);
                                        on_delta(text);
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let (Some(tool), Some(partial)) = (
                                        pending_tool.as_mut(),
                                        delta.get("partial_json").and_then(|v| v.as_str()),
                                    ) {
                                        tool.2.push_str(partial);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "content_block_stop" => {
                        if pending_text_idx.is_some() {
                            blocks.push(ContentBlock::Text {
                                text: pending_text.clone(),
                            });
                            pending_text_idx = None;
                            pending_text.clear();
                        }
                        if let Some((id, name, json_buf)) = pending_tool.take() {
                            // Zero-argument tools (e.g. list_subagents) emit no
                            // input_json_delta events, leaving json_buf empty.
                            // Anthropic's API rejects replayed conversations
                            // whose tool_use blocks carry `input: null` with
                            // "messages.input: Input should be an object", so
                            // coerce empty / unparsable / non-object payloads
                            // to `{}` here at the source.
                            let parsed = serde_json::from_str::<Value>(&json_buf).ok();
                            let input = match parsed {
                                Some(v) if v.is_object() => v,
                                _ => json!({}),
                            };
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            });
                            blocks.push(ContentBlock::ToolUse { id, name, input });
                        }
                    }
                    "message_delta" => {
                        if let Some(reason) = event
                            .get("delta")
                            .and_then(|d| d.get("stop_reason"))
                            .and_then(|v| v.as_str())
                        {
                            stop_reason = reason.to_string();
                        }
                        if let Some(usage) = event.get("usage") {
                            if let Some(t) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                                output_tokens = t;
                            }
                        }
                    }
                    "message_stop" => {}
                    _ => {}
                }
            }
        }

        Ok(LlmResponse {
            content: text_buf,
            blocks,
            tool_calls,
            stop_reason,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            reasoning: None,
        })
    }
}

/// Pick a wait duration for a 429 retry. Uses the `retry-after` header
/// (seconds) when present; otherwise exponential backoff: 1s, 2s, 4s, 8s…
fn retry_after_duration(headers: &reqwest::header::HeaderMap, attempt: u32) -> Duration {
    if let Some(v) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(secs) = v.parse::<f64>() {
            return Duration::from_secs_f64(secs.max(0.5));
        }
    }
    Duration::from_secs(1u64 << attempt.min(6))
}

fn content_block_to_anthropic_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::ToolUse { id, name, input } => {
            // Anthropic requires `input` to be an object. Defense in depth
            // against any already-stored history (or non-streaming producer)
            // that left a null / non-object here; see the parse-time guard
            // in the stream handler for the original failure mode.
            let input_obj = if input.is_object() {
                input.clone()
            } else {
                json!({})
            };
            json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input_obj,
            })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{CacheLayout, Message, ToolSchema};

    fn base_request() -> LlmRequest {
        LlmRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![Message::user_text("hello")],
            system: Some("be brief".to_string()),
            temperature: 0.0,
            max_tokens: 16,
            tools: vec![ToolSchema {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({ "type": "object", "properties": {} }),
            }],
            cache: CacheLayout::default(),
        }
    }

    #[test]
    fn unmarked_request_body_has_no_cache_control() {
        let (body, one_hour) = build_request_body(&base_request(), false).unwrap();
        assert!(!one_hour);
        // System stays the plain string form; nothing carries cache_control.
        assert_eq!(body["system"], json!("be brief"));
        assert!(!body.to_string().contains("cache_control"));
    }

    #[test]
    fn marked_request_emits_cache_control_layout() {
        let mut request = base_request();
        request.cache.system = Some(CacheTtl::FiveMinutes);
        request.cache.tools = Some(CacheTtl::FiveMinutes);
        request.messages.last_mut().unwrap().cache_control = Some(CacheTtl::FiveMinutes);

        let (body, one_hour) = build_request_body(&request, false).unwrap();
        assert!(!one_hour);
        // Cached system requires the structured-content form.
        assert_eq!(
            body["system"],
            json!([{ "type": "text", "text": "be brief", "cache_control": { "type": "ephemeral" } }])
        );
        // The tools mark lands on the last tool entry.
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        // The message mark lands on the message's last content block.
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn one_hour_ttl_sets_extended_ttl_and_beta_flag() {
        let mut request = base_request();
        request.cache.system = Some(CacheTtl::OneHour);
        let (body, one_hour) = build_request_body(&request, false).unwrap();
        assert!(one_hour);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({ "type": "ephemeral", "ttl": "1h" })
        );
    }

    #[test]
    fn message_marks_beyond_breakpoint_budget_keep_the_latest() {
        let mut request = base_request();
        request.cache.system = Some(CacheTtl::FiveMinutes);
        request.cache.tools = Some(CacheTtl::FiveMinutes);
        // 3 message marks + system + tools = 5 > 4; the oldest message mark drops.
        request.messages = (0..3)
            .map(|i| {
                let mut m = Message::user_text(format!("turn {i}"));
                m.cache_control = Some(CacheTtl::FiveMinutes);
                m
            })
            .collect();

        let (body, _) = build_request_body(&request, false).unwrap();
        let marked: Vec<bool> = (0..3)
            .map(|i| {
                body["messages"][i]["content"][0]
                    .get("cache_control")
                    .is_some()
            })
            .collect();
        assert_eq!(marked, vec![false, true, true]);
    }

    /// Spin up a local server speaking the Anthropic Messages shape and drive
    /// a provider pointed at it, proving the shape is endpoint-agnostic: the
    /// request lands on the configured URL with the Anthropic headers, and
    /// both the JSON and SSE response forms parse into the same `LlmResponse`.
    #[tokio::test]
    async fn compatible_endpoint_receives_anthropic_shape_and_parses_both_response_forms() {
        use axum::{extract::State, routing::post, Json, Router};
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct Seen {
            headers: Arc<Mutex<Vec<(String, String)>>>,
            bodies: Arc<Mutex<Vec<Value>>>,
        }

        async fn messages(
            State(seen): State<Seen>,
            headers: axum::http::HeaderMap,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            seen.headers.lock().unwrap().extend(
                headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())),
            );
            let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
            seen.bodies.lock().unwrap().push(body);
            if stream {
                let frames = [
                    r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0}}}"#,
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
                    r#"{"type":"content_block_stop","index":0}"#,
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                    r#"{"type":"message_stop"}"#,
                ];
                let sse: String = frames
                    .iter()
                    .map(|f| format!("event: x\ndata: {f}\n\n"))
                    .collect();
                ([("content-type", "text/event-stream")], sse).into_response()
            } else {
                Json(json!({
                    "content": [{ "type": "text", "text": "hello" }],
                    "stop_reason": "end_turn",
                    "usage": { "input_tokens": 7, "output_tokens": 2 }
                }))
                .into_response()
            }
        }

        let seen = Seen::default();
        let app = Router::new()
            .route("/anthropic/v1/messages", post(messages))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let provider = AnthropicProvider::with_base_url(
            "test-key".to_string(),
            super::super::messages_url(format!("http://{addr}/anthropic")),
            vec![String::new()],
        );
        assert!(provider.supports_model("MiniMax-M2"));
        assert_eq!(provider.label(), "Anthropic-compatible endpoint 127.0.0.1");

        let mut request = base_request();
        request.model = "MiniMax-M2".to_string();

        let sent = provider.send(&request).await.unwrap();
        assert_eq!(sent.content, "hello");
        assert_eq!(sent.stop_reason, "end_turn");
        assert_eq!((sent.input_tokens, sent.output_tokens), (7, 2));

        let deltas = Arc::new(Mutex::new(String::new()));
        let sink_deltas = Arc::clone(&deltas);
        let mut sink: TokenSink = Box::new(move |d: &str| sink_deltas.lock().unwrap().push_str(d));
        let streamed = provider.stream(&request, &mut sink).await.unwrap();
        assert_eq!(*deltas.lock().unwrap(), "hello");
        assert_eq!(streamed.content, "hello");
        assert_eq!((streamed.input_tokens, streamed.output_tokens), (7, 2));

        let headers = seen.headers.lock().unwrap();
        assert!(headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "test-key"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "anthropic-version" && v == ANTHROPIC_VERSION));
        let bodies = seen.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        for body in bodies.iter() {
            // Non-Anthropic model ids pass through untouched (no alias
            // mapping), with the Messages-shape fields.
            assert_eq!(body["model"], json!("MiniMax-M2"));
            assert_eq!(body["system"], json!("be brief"));
            assert_eq!(body["tools"][0]["input_schema"]["type"], json!("object"));
            assert_eq!(body["messages"][0]["content"][0]["type"], json!("text"));
        }
    }

    #[test]
    fn usage_cache_token_fields_parse_and_default() {
        let with_cache: AnthropicUsage = serde_json::from_str(
            r#"{ "input_tokens": 10, "output_tokens": 5,
                 "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 2000 }"#,
        )
        .unwrap();
        assert_eq!(with_cache.cache_creation_input_tokens, 1000);
        assert_eq!(with_cache.cache_read_input_tokens, 2000);

        let without: AnthropicUsage =
            serde_json::from_str(r#"{ "input_tokens": 10, "output_tokens": 5 }"#).unwrap();
        assert_eq!(without.cache_creation_input_tokens, 0);
        assert_eq!(without.cache_read_input_tokens, 0);
    }
}
