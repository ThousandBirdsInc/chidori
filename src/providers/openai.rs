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
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: OPENAI_API_URL.to_string(),
            client: Client::new(),
            model_prefixes: Vec::new(),
            rate_limiter: None,
        }
    }

    /// Create a provider pointing at an OpenAI-compatible endpoint (e.g. LiteLLM, Ollama).
    pub fn with_base_url(api_key: String, base_url: String, model_prefixes: Vec<String>) -> Self {
        Self {
            api_key,
            base_url,
            client: Client::new(),
            model_prefixes,
            rate_limiter: None,
        }
    }

    pub fn with_rate_limit(mut self, rpm: u32) -> Self {
        self.rate_limiter = Some(Arc::new(RateLimiter::new(rpm)));
        self
    }
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
            return self.model_prefixes.iter().any(|p| model.starts_with(p.as_str()));
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
        let resp_text = resp.text().await.context("Failed to read OpenAI response")?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<OpenAiError>(&resp_text) {
                bail!("OpenAI API error ({}): {}", status, err.error.message);
            }
            bail!("OpenAI API error ({}): {}", status, resp_text);
        }

        let parsed: OpenAiResponseBody =
            serde_json::from_str(&resp_text).context("Failed to parse OpenAI response")?;

        let (text, finish_reason, raw_tool_calls) = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| {
                (
                    c.message.content.unwrap_or_default(),
                    c.finish_reason.unwrap_or_default(),
                    c.message.tool_calls.unwrap_or_default(),
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

        let (input_tokens, output_tokens) = match parsed.usage {
            Some(usage) => (usage.prompt_tokens, usage.completion_tokens),
            None => (0, 0),
        };

        Ok(LlmResponse {
            content: text,
            blocks,
            tool_calls,
            stop_reason,
            input_tokens,
            output_tokens,
        })
    }

    /// OpenAI SSE streaming. Matches the `chat/completions` streaming shape
    /// used by OpenAI itself and by most OpenAI-compatible backends
    /// (Azure, LiteLLM, Ollama, vLLM). Each data frame is a JSON chunk
    /// with `choices[0].delta`; text deltas invoke `on_delta`, tool-call
    /// deltas accumulate `function.name` + `function.arguments` per
    /// `tool_calls[i].index`.
    async fn stream(
        &self,
        request: &LlmRequest,
        on_delta: &mut TokenSink,
    ) -> Result<LlmResponse> {
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
            bail!("OpenAI stream error ({}): {}", status, text);
        }

        // Accumulators.
        let mut text_buf = String::new();
        // Tool calls keyed by index (OpenAI streams partial tool calls across
        // multiple chunks, each with an index field).
        let mut tool_acc: std::collections::BTreeMap<u64, (String, String, String)> =
            std::collections::BTreeMap::new(); // idx -> (id, name, args_json_buf)
        let mut finish_reason: String = "stop".into();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;

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
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let i = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                        let entry = tool_acc.entry(i).or_insert_with(|| {
                            (String::new(), String::new(), String::new())
                        });
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
            let input: Value =
                serde_json::from_str(&args).unwrap_or_else(|_| Value::String(args));
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
            input_tokens,
            output_tokens,
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
