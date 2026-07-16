use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use std::sync::Arc;

use super::rate_limit::RateLimiter;
use super::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, TokenSink, ToolCall};

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    client: Client,
    /// Optional list of model name prefixes this provider handles.
    /// If empty, uses default matching (gpt, o1, o3).
    model_prefixes: Vec<String>,
    rate_limiter: Option<Arc<RateLimiter>>,
    /// Display name used in error messages. "OpenAI" for the real endpoint;
    /// for OpenAI-compatible endpoints it names the host (e.g.
    /// "OpenAI-compatible endpoint api.deepseek.com") so a failure is
    /// attributed to the provider the user actually configured.
    label: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: OPENAI_API_URL.to_string(),
            client: Client::new(),
            model_prefixes: Vec::new(),
            rate_limiter: None,
            label: "OpenAI".to_string(),
        }
    }

    /// Create a provider pointing at an OpenAI-compatible endpoint (e.g.
    /// DeepSeek, Groq, Ollama, vLLM, LiteLLM). The error label is derived
    /// from the endpoint's host.
    pub fn with_base_url(api_key: String, base_url: String, model_prefixes: Vec<String>) -> Self {
        let label = match url_host(&base_url) {
            Some(host) if host != "api.openai.com" => {
                format!("OpenAI-compatible endpoint {host}")
            }
            _ => "OpenAI".to_string(),
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

/// Best-effort host extraction for error labels; avoids pulling in a URL crate.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

#[derive(Deserialize)]
struct OpenAiResponseBody {
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCall>>,
    /// Hidden chain-of-thought emitted by reasoning models on
    /// OpenAI-compatible backends (DeepSeek et al.). Not part of the
    /// conversation; captured so authors can inspect it.
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiToolCall {
    id: String,
    function: OpenAiToolCallFunction,
}

#[derive(Deserialize)]
struct OpenAiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    /// OpenAI's prompt caching is automatic on exact prefixes; the cached
    /// share of `prompt_tokens` is reported here. Absent on older/compatible
    /// backends, so default to none.
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
}

#[derive(Deserialize, Default)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct OpenAiError {
    error: OpenAiErrorBody,
}

#[derive(Deserialize)]
struct OpenAiErrorBody {
    message: String,
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiProvider {
    fn supports_model(&self, model: &str) -> bool {
        if !self.model_prefixes.is_empty() {
            return self
                .model_prefixes
                .iter()
                .any(|p| model.starts_with(p.as_str()));
        }
        model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3")
    }

    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        if let Some(ref rl) = self.rate_limiter {
            rl.acquire().await;
        }

        let mut messages: Vec<Value> = Vec::new();

        if let Some(ref system) = request.system {
            messages.push(json!({ "role": "system", "content": system }));
        }

        for m in &request.messages {
            messages.extend(message_to_openai_json(m));
        }

        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        });

        if !request.tools.is_empty() {
            let tools_json: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools_json);
        }

        let resp = self
            .client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to send request to {}", self.base_url))?;

        let status = resp.status();
        let resp_text = resp
            .text()
            .await
            .with_context(|| format!("Failed to read {} response", self.label))?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<OpenAiError>(&resp_text) {
                bail!("{} API error ({}): {}", self.label, status, err.error.message);
            }
            bail!("{} API error ({}): {}", self.label, status, resp_text);
        }

        let parsed: OpenAiResponseBody = serde_json::from_str(&resp_text)
            .with_context(|| format!("Failed to parse {} response", self.label))?;

        let (text, finish_reason, raw_tool_calls, reasoning) = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| {
                (
                    c.message.content.unwrap_or_default(),
                    c.finish_reason.unwrap_or_default(),
                    c.message.tool_calls.unwrap_or_default(),
                    c.message.reasoning_content.filter(|r| !r.is_empty()),
                )
            })
            .unwrap_or_default();

        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text: text.clone() });
        }

        let mut tool_calls = Vec::new();
        for call in raw_tool_calls {
            let input: Value = serde_json::from_str(&call.function.arguments)
                .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
            tool_calls.push(ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input: input.clone(),
            });
            blocks.push(ContentBlock::ToolUse {
                id: call.id,
                name: call.function.name,
                input,
            });
        }

        let stop_reason = match finish_reason.as_str() {
            "tool_calls" => "tool_use".to_string(),
            "stop" | "" => "end_turn".to_string(),
            other => other.to_string(),
        };

        let (input_tokens, output_tokens, cache_read_tokens) = match parsed.usage {
            Some(usage) => {
                let cached = usage
                    .prompt_tokens_details
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                // OpenAI counts cached tokens inside prompt_tokens; split them
                // out so input_tokens means "fresh input" like Anthropic's.
                (
                    usage.prompt_tokens.saturating_sub(cached),
                    usage.completion_tokens,
                    cached,
                )
            }
            None => (0, 0, 0),
        };

        Ok(LlmResponse {
            content: text,
            blocks,
            tool_calls,
            stop_reason,
            input_tokens,
            output_tokens,
            cache_creation_tokens: 0,
            cache_read_tokens,
            reasoning,
        })
    }

    /// OpenAI SSE streaming. Matches the `chat/completions` streaming shape
    /// used by OpenAI itself and by most OpenAI-compatible backends
    /// (Azure, LiteLLM, Ollama, vLLM). Each data frame is a JSON chunk
    /// with `choices[0].delta`; text deltas invoke `on_delta`, tool-call
    /// deltas accumulate `function.name` + `function.arguments` per
    /// `tool_calls[i].index`.
    async fn stream(&self, request: &LlmRequest, on_delta: &mut TokenSink) -> Result<LlmResponse> {
        use futures::StreamExt;

        if let Some(ref rl) = self.rate_limiter {
            rl.acquire().await;
        }

        let mut messages: Vec<Value> = Vec::new();
        if let Some(ref system) = request.system {
            messages.push(json!({ "role": "system", "content": system }));
        }
        for m in &request.messages {
            messages.extend(message_to_openai_json(m));
        }

        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !request.tools.is_empty() {
            let tools_json: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools_json);
        }

        let resp = self
            .client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("streaming request to {}", self.base_url))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("{} stream error ({}): {}", self.label, status, text);
        }

        // Accumulators.
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        // Tool calls keyed by index (OpenAI streams partial tool calls across
        // multiple chunks, each with an index field).
        let mut tool_acc: std::collections::BTreeMap<u64, (String, String, String)> =
            std::collections::BTreeMap::new(); // idx -> (id, name, args_json_buf)
        let mut finish_reason: String = "stop".into();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut cache_read_tokens: u64 = 0;

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading OpenAI stream")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(idx) = buffer.find("\n\n") {
                let frame = buffer[..idx].to_string();
                buffer.drain(..idx + 2);

                let data_line = frame
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .unwrap_or("");
                if data_line.is_empty() {
                    continue;
                }
                if data_line == "[DONE]" {
                    continue;
                }
                let Ok(event): std::result::Result<Value, _> = serde_json::from_str(data_line)
                else {
                    continue;
                };

                // Usage may appear in a chunk with an empty `choices` array
                // when stream_options.include_usage is set.
                if let Some(usage) = event.get("usage") {
                    if let Some(t) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                        input_tokens = t;
                    }
                    if let Some(t) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                        output_tokens = t;
                    }
                    if let Some(t) = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        cache_read_tokens = t;
                    }
                }

                let Some(choice) = event.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                    finish_reason = reason.to_string();
                }
                let Some(delta) = choice.get("delta") else {
                    continue;
                };
                if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text_buf.push_str(text);
                        on_delta(text);
                    }
                }
                // Reasoning deltas accumulate silently: they are not part of
                // the visible answer, so they never reach the token sink.
                if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                    reasoning_buf.push_str(text);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let i = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                        let entry = tool_acc
                            .entry(i)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            if entry.0.is_empty() {
                                entry.0 = id.to_string();
                            }
                        }
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                if entry.1.is_empty() {
                                    entry.1 = name.to_string();
                                }
                            }
                            if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
            }
        }

        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !text_buf.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text_buf.clone(),
            });
        }
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for (_idx, (id, name, args)) in tool_acc {
            let input: Value = serde_json::from_str(&args).unwrap_or(Value::String(args));
            tool_calls.push(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            blocks.push(ContentBlock::ToolUse { id, name, input });
        }

        let stop_reason = match finish_reason.as_str() {
            "tool_calls" => "tool_use".to_string(),
            "stop" | "" => "end_turn".to_string(),
            other => other.to_string(),
        };

        Ok(LlmResponse {
            content: text_buf,
            blocks,
            tool_calls,
            stop_reason,
            // Same split as the non-streaming path: report fresh input apart
            // from the cached share.
            input_tokens: input_tokens.saturating_sub(cache_read_tokens),
            output_tokens,
            cache_creation_tokens: 0,
            cache_read_tokens,
            reasoning: (!reasoning_buf.is_empty()).then_some(reasoning_buf),
        })
    }
}

/// Translate our unified Message (Anthropic-style blocks) into one or more
/// OpenAI chat messages. Assistant messages may contain text + tool_calls in a
/// single message; tool_result blocks become separate role="tool" messages.
fn message_to_openai_json(m: &super::Message) -> Vec<Value> {
    let role = m.role.as_str();

    if role == "assistant" {
        let mut text = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        for block in &m.content {
            match block {
                ContentBlock::Text { text: t } => text.push_str(t),
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                        }
                    }));
                }
                ContentBlock::ToolResult { .. } => {}
            }
        }
        let mut msg = json!({ "role": "assistant", "content": text });
        if !tool_calls.is_empty() {
            msg["tool_calls"] = Value::Array(tool_calls);
        }
        return vec![msg];
    }

    // User messages: emit text as a single user message, tool_results as
    // separate role="tool" messages.
    let mut out: Vec<Value> = Vec::new();
    let mut text = String::new();
    for block in &m.content {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
            ContentBlock::ToolUse { .. } => {}
        }
    }
    if !text.is_empty() {
        out.insert(0, json!({ "role": role, "content": text }));
    }
    out
}
