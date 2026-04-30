use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use std::sync::Arc;

use super::rate_limit::RateLimiter;
use super::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, TokenSink, ToolCall};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    api_key: String,
    client: Client,
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: Client::new(),
            rate_limiter: None,
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
}

#[derive(Deserialize)]
struct AnthropicError {
    error: AnthropicErrorBody,
}

#[derive(Deserialize)]
struct AnthropicErrorBody {
    message: String,
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

/// Map common short aliases to canonical Anthropic model ids. Agents
/// in the wild often write `model = "claude-sonnet"` rather than the
/// full versioned id (`claude-sonnet-4-6`); without this resolution
/// Anthropic returns 404.
///
/// Falls through unchanged for any model name that already looks
/// versioned, so user-supplied full ids keep working.
fn resolve_alias(model: &str) -> &str {
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
        model.starts_with("claude")
    }

    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        if let Some(ref rl) = self.rate_limiter {
            rl.acquire().await;
        }

        // Build the messages array. Anthropic accepts content as an array of blocks.
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let blocks: Vec<Value> = m
                    .content
                    .iter()
                    .map(content_block_to_anthropic_json)
                    .collect();
                json!({
                    "role": m.role,
                    "content": blocks,
                })
            })
            .collect();

        let tools_json: Vec<AnthropicTool> = request
            .tools
            .iter()
            .map(|t| AnthropicTool {
                name: &t.name,
                description: &t.description,
                input_schema: &t.input_schema,
            })
            .collect();

        let mut body = json!({
            "model": resolve_alias(&request.model),
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        });
        if let Some(ref system) = request.system {
            body["system"] = Value::String(system.clone());
        }
        if !tools_json.is_empty() {
            body["tools"] = serde_json::to_value(&tools_json)?;
        }

        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        let status = resp.status();
        let resp_text = resp.text().await.context("Failed to read Anthropic response")?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<AnthropicError>(&resp_text) {
                bail!("Anthropic API error ({}): {}", status, err.error.message);
            }
            bail!("Anthropic API error ({}): {}", status, resp_text);
        }

        let parsed: AnthropicResponseBody =
            serde_json::from_str(&resp_text).context("Failed to parse Anthropic response")?;

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

        Ok(LlmResponse {
            content: text_parts.join(""),
            blocks,
            tool_calls,
            stop_reason: parsed.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
            input_tokens: parsed.usage.input_tokens,
            output_tokens: parsed.usage.output_tokens,
        })
    }

    /// Anthropic SSE streaming. Sends the request with `stream: true` and
    /// parses the resulting `event: <name>\ndata: <json>\n\n` frames. On
    /// every `content_block_delta` with a `text_delta` chunk, we invoke
    /// `on_delta`. Tool-use blocks and usage totals are accumulated the
    /// same way as the non-streaming path so the returned LlmResponse is
    /// identical in shape.
    async fn stream(
        &self,
        request: &LlmRequest,
        on_delta: &mut TokenSink,
    ) -> Result<LlmResponse> {
        use futures::StreamExt;

        if let Some(ref rl) = self.rate_limiter {
            rl.acquire().await;
        }

        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let blocks: Vec<Value> = m
                    .content
                    .iter()
                    .map(content_block_to_anthropic_json)
                    .collect();
                json!({ "role": m.role, "content": blocks })
            })
            .collect();
        let tools_json: Vec<AnthropicTool> = request
            .tools
            .iter()
            .map(|t| AnthropicTool {
                name: &t.name,
                description: &t.description,
                input_schema: &t.input_schema,
            })
            .collect();
        let mut body = json!({
            "model": resolve_alias(&request.model),
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
        });
        if let Some(ref system) = request.system {
            body["system"] = Value::String(system.clone());
        }
        if !tools_json.is_empty() {
            body["tools"] = serde_json::to_value(&tools_json)?;
        }

        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .context("streaming request to Anthropic")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("Anthropic stream error ({}): {}", status, text);
        }

        // Accumulators for the final response. Text chunks land in
        // `text_buf`; tool_use blocks arrive as a start event followed by
        // input_json_delta events carrying partial JSON — we reassemble.
        let mut text_buf = String::new();
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut stop_reason: String = "end_turn".into();

        // Per content-block scratch space keyed by the block index.
        let mut pending_text_idx: Option<usize> = None;
        let mut pending_text: String = String::new();
        let mut pending_tool: Option<(String, String, String)> = None; // (id, name, json_buf)

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading Anthropic stream")?;
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
                        }
                    }
                    "content_block_start" => {
                        let idx = event
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
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
                                    if let Some(text) =
                                        delta.get("text").and_then(|v| v.as_str())
                                    {
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
                            let input: Value =
                                serde_json::from_str(&json_buf).unwrap_or(Value::Null);
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            });
                            blocks.push(ContentBlock::ToolUse { id, name, input });
                        }
                    }
                    "message_delta" => {
                        if let Some(reason) =
                            event.get("delta").and_then(|d| d.get("stop_reason")).and_then(|v| v.as_str())
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
        })
    }
}

fn content_block_to_anthropic_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
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
