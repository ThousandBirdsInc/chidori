use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

use crate::providers::{
    ContentBlock, LlmRequest, LlmResponse, ProviderRegistry, TokenSink, ToolCall,
};
use crate::runtime::call_log::{CallRecord, TokenUsage};
use crate::runtime::context::{InputMode, PendingInput, RuntimeContext, PAUSE_MARKER};
use crate::runtime::memory::execute_memory_action;
use crate::runtime::snapshot::{HostPromiseState, PendingHostOperationKind};
use crate::runtime::template::TemplateEngine;
use crate::tools::ToolRegistry;

pub fn execute_durable_json_call(
    ctx: &RuntimeContext,
    function: &str,
    args: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let seq = ctx.next_seq();
    execute_durable_json_call_at_seq(ctx, seq, function, args, live)
}

pub fn execute_durable_json_call_at_seq(
    ctx: &RuntimeContext,
    seq: u64,
    function: &str,
    args: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    if let Some(record) = ctx
        .try_replay_checked(seq, function)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        // A replayed call's `live()` is skipped. If it was a container (a tool
        // or call_agent whose body made its own host calls), those nested calls
        // burned sequence numbers that won't be re-consumed now — absorb the
        // recorded subtree so the outer sequence stays aligned and the nested
        // records survive in the trace. No-op for leaf calls.
        ctx.absorb_replayed_subtree(seq);
        return Ok(record.result);
    }

    let operation_kind = host_operation_kind(function);
    if let Some(kind) = operation_kind.clone() {
        if let Some(result) = replay_completed_host_operation(ctx, seq, function, kind, &args)? {
            return Ok(result);
        }
    }

    let host_operation = operation_kind.map(|kind| {
        ctx.begin_host_operation_with_function(seq, kind, Some(function.to_string()), args.clone())
    });
    if let Some(id) = host_operation {
        ctx.run_host_operation_safepoint(id)?;
    }
    let started = Utc::now();
    // Mark this call as executing so any calls made inside `live()` (a
    // sub-agent's host calls, when `function == "call_agent"`) nest under it.
    ctx.enter_call(seq);
    let result = live();
    ctx.exit_call(seq);
    let duration_ms = Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;

    match result {
        Ok(result) => {
            if let Some(id) = host_operation {
                ctx.resolve_host_operation(id, result.clone())?;
            }
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args,
                result: result.clone(),
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: None,
            });
            if let Some(id) = host_operation {
                ctx.run_host_operation_completion_safepoint(id)?;
            }
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            if message.contains(PAUSE_MARKER) {
                return Err(anyhow::anyhow!(message));
            }
            if let Some(id) = host_operation {
                ctx.reject_host_operation(id, message.clone())?;
            }
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args,
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message.clone()),
            });
            if let Some(id) = host_operation {
                ctx.run_host_operation_completion_safepoint(id)?;
            }
            Err(anyhow::anyhow!(message))
        }
    }
}

fn replay_completed_host_operation(
    ctx: &RuntimeContext,
    seq: u64,
    function: &str,
    kind: PendingHostOperationKind,
    args: &Value,
) -> Result<Option<Value>> {
    let Some(record) = ctx.completed_host_operation(seq, kind, args) else {
        return Ok(None);
    };

    match record.state {
        HostPromiseState::Resolved {
            value,
            completed_at,
        } => {
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args: args.clone(),
                result: value.clone(),
                duration_ms: 0,
                token_usage: None,
                timestamp: completed_at,
                error: None,
            });
            Ok(Some(value))
        }
        HostPromiseState::Rejected {
            error,
            completed_at,
        } => {
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args: args.clone(),
                result: Value::Null,
                duration_ms: 0,
                token_usage: None,
                timestamp: completed_at,
                error: Some(error.clone()),
            });
            Err(anyhow::anyhow!(error))
        }
        HostPromiseState::Pending => Ok(None),
    }
}

pub fn execute_log(args: &Value) -> Result<Value> {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("log requires string message"))?;
    let fields = args.get("fields").and_then(Value::as_object);

    match fields {
        Some(fields) if !fields.is_empty() => {
            let fields_str = serde_json::to_string(&Value::Object(fields.clone()))?;
            tracing::info!(message = message, fields = %fields_str);
        }
        _ => tracing::info!("{}", message),
    }

    Ok(Value::Null)
}

fn host_operation_kind(function: &str) -> Option<PendingHostOperationKind> {
    match function {
        "prompt" => Some(PendingHostOperationKind::Prompt),
        "input" => Some(PendingHostOperationKind::Input),
        "tool" => Some(PendingHostOperationKind::Tool),
        "call_agent" => Some(PendingHostOperationKind::CallAgent),
        "http" => Some(PendingHostOperationKind::Http),
        "template" => Some(PendingHostOperationKind::Template),
        "memory" => Some(PendingHostOperationKind::Memory),
        "checkpoint" => Some(PendingHostOperationKind::Checkpoint),
        "log" => Some(PendingHostOperationKind::Log),
        _ => None,
    }
}

pub fn execute_input(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("input requires string prompt"))?
        .to_string();
    let seq = ctx.next_seq();
    if let Some(record) = ctx
        .try_replay_checked(seq, "input")
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) =
        replay_completed_host_operation(ctx, seq, "input", PendingHostOperationKind::Input, args)?
    {
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Input,
        Some("input".to_string()),
        args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    match ctx.input_mode() {
        InputMode::Stdin => {
            eprintln!("{}", prompt);
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let response = line.trim_end_matches(&['\r', '\n'][..]).to_string();
            ctx.resolve_host_operation(host_operation, Value::String(response.clone()))?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "input".to_string(),
                args: json!({ "prompt": prompt }),
                result: Value::String(response.clone()),
                duration_ms: 0,
                token_usage: None,
                timestamp: Utc::now(),
                error: None,
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Ok(Value::String(response))
        }
        InputMode::Pause => {
            ctx.set_pending_input(PendingInput {
                seq,
                prompt: prompt.clone(),
            });
            Err(anyhow::anyhow!("{PAUSE_MARKER}: {prompt}"))
        }
    }
}

/// Apply the runtime context's model override (Pi-style save point) to an
/// outgoing prompt request. A no-op unless the host installed an override hook
/// that currently yields a model. This is the single point where a mid-run
/// model change takes effect for every prompt path — the native agent loop and
/// the TypeScript interactive engine both call the prompt bindings below.
fn apply_model_override(ctx: &RuntimeContext, request: &mut LlmRequest) {
    if let Some(model) = ctx.resolve_model_override() {
        request.model = model;
    }
}

pub fn execute_prompt_text(
    ctx: &RuntimeContext,
    providers: &ProviderRegistry,
    tokio_rt: &tokio::runtime::Runtime,
    mut request: LlmRequest,
    args: Value,
    prompt_type: Option<String>,
) -> Result<Value> {
    apply_model_override(ctx, &mut request);
    let seq = ctx.next_seq();
    if let Some(record) = ctx
        .try_replay_checked(seq, "prompt")
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "prompt",
        PendingHostOperationKind::Prompt,
        &args,
    )? {
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Prompt,
        Some("prompt".to_string()),
        args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    let started = Utc::now();
    let response = send_prompt_request(ctx, providers, tokio_rt, seq, &request, prompt_type)?;
    let duration_ms = Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;

    match response {
        Ok(response) => {
            let result = Value::String(response.content.clone());
            ctx.resolve_host_operation(host_operation, result.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result: result.clone(),
                duration_ms,
                token_usage: Some(TokenUsage {
                    input_tokens: response.input_tokens,
                    output_tokens: response.output_tokens,
                }),
                timestamp: started,
                error: None,
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            ctx.reject_host_operation(host_operation, message.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message.clone()),
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

pub fn execute_prompt_response(
    ctx: &RuntimeContext,
    providers: &ProviderRegistry,
    tokio_rt: &tokio::runtime::Runtime,
    mut request: LlmRequest,
    args: Value,
    prompt_type: Option<String>,
) -> Result<LlmResponse> {
    apply_model_override(ctx, &mut request);
    let seq = ctx.next_seq();
    if let Some(record) = ctx
        .try_replay_checked(seq, "prompt")
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return llm_response_from_json(&record.result).ok_or_else(|| {
            anyhow::anyhow!("cached prompt record at seq {seq} is not a tool-use turn")
        });
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "prompt",
        PendingHostOperationKind::Prompt,
        &args,
    )? {
        return llm_response_from_json(&result).ok_or_else(|| {
            anyhow::anyhow!("completed prompt host operation at seq {seq} is not a tool-use turn")
        });
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Prompt,
        Some("prompt".to_string()),
        args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    let started = Utc::now();
    let response = send_prompt_request(ctx, providers, tokio_rt, seq, &request, prompt_type)?;
    let duration_ms = Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;

    match response {
        Ok(response) => {
            let result = llm_response_to_json(&response);
            ctx.resolve_host_operation(host_operation, result.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result,
                duration_ms,
                token_usage: Some(TokenUsage {
                    input_tokens: response.input_tokens,
                    output_tokens: response.output_tokens,
                }),
                timestamp: started,
                error: None,
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Ok(response)
        }
        Err(err) => {
            let message = err.to_string();
            ctx.reject_host_operation(host_operation, message.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message.clone()),
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

fn send_prompt_request(
    ctx: &RuntimeContext,
    providers: &ProviderRegistry,
    tokio_rt: &tokio::runtime::Runtime,
    seq: u64,
    request: &LlmRequest,
    prompt_type: Option<String>,
) -> Result<anyhow::Result<LlmResponse>> {
    if ctx.has_event_sender() {
        let stream_id = ctx.begin_prompt_stream(seq, prompt_type.clone(), request.model.clone());
        let ctx_for_cb = ctx.clone();
        let stream_id_for_cb = stream_id.clone();
        let prompt_type_for_cb = prompt_type.clone();
        let mut sink: TokenSink = Box::new(move |delta: &str| {
            if let Some(stream_id) = stream_id_for_cb.clone() {
                ctx_for_cb.emit_prompt_delta(
                    stream_id,
                    seq,
                    prompt_type_for_cb.clone(),
                    delta.to_string(),
                );
            }
        });
        let response = tokio_rt.block_on(async { providers.stream(request, &mut sink).await });
        if let Some(stream_id) = stream_id {
            ctx.end_prompt_stream(
                stream_id,
                seq,
                prompt_type,
                response.as_ref().err().map(|err| err.to_string()),
            );
        }
        Ok(response)
    } else {
        Ok(tokio_rt.block_on(async { providers.send(request).await }))
    }
}

pub fn llm_response_to_json(response: &LlmResponse) -> Value {
    json!({
        "content": response.content,
        "blocks": response.blocks,
        "tool_calls": response.tool_calls.iter().map(|call| json!({
            "id": call.id,
            "name": call.name,
            "input": call.input,
        })).collect::<Vec<_>>(),
        "stop_reason": response.stop_reason,
        "input_tokens": response.input_tokens,
        "output_tokens": response.output_tokens,
    })
}

pub fn llm_response_from_json(value: &Value) -> Option<LlmResponse> {
    Some(LlmResponse {
        content: value.get("content")?.as_str()?.to_string(),
        blocks: serde_json::from_value::<Vec<ContentBlock>>(value.get("blocks")?.clone()).ok()?,
        tool_calls: value
            .get("tool_calls")?
            .as_array()?
            .iter()
            .filter_map(|call| {
                Some(ToolCall {
                    id: call.get("id")?.as_str()?.to_string(),
                    name: call.get("name")?.as_str()?.to_string(),
                    input: call.get("input").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
        stop_reason: value.get("stop_reason")?.as_str()?.to_string(),
        input_tokens: value.get("input_tokens")?.as_u64()?,
        output_tokens: value.get("output_tokens")?.as_u64()?,
    })
}

pub fn execute_memory(args: &Value) -> Result<Value> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let namespace = args
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let key = args.get("key").and_then(Value::as_str);
    let value = args.get("value").filter(|value| !value.is_null());
    let prefix = args.get("prefix").and_then(Value::as_str).unwrap_or("");

    execute_memory_action(action, namespace, key, value, prefix)
}

pub fn execute_template(template_engine: &TemplateEngine, args: &Value) -> Result<Value> {
    let template = args
        .get("template")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("template requires string template"))?;
    let vars = args.get("vars").cloned().unwrap_or_else(|| json!({}));
    template_engine.render(template, &vars).map(Value::String)
}

pub fn execute_tool_call(
    ctx: &RuntimeContext,
    name: &str,
    kwargs: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    execute_durable_json_call(
        ctx,
        "tool",
        json!({
            "name": name,
            "kwargs": kwargs,
        }),
        live,
    )
}

pub fn execute_tool_call_at_seq(
    ctx: &RuntimeContext,
    seq: u64,
    name: &str,
    kwargs: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    execute_durable_json_call_at_seq(
        ctx,
        seq,
        "tool",
        json!({
            "name": name,
            "kwargs": kwargs,
        }),
        live,
    )
}

#[allow(dead_code)]
pub fn execute_native_tool_call(
    ctx: &RuntimeContext,
    registry: &ToolRegistry,
    name: &str,
    kwargs: Value,
) -> Result<Value> {
    execute_tool_call(ctx, name, kwargs.clone(), || {
        registry.dispatch_native(name, kwargs)
    })
}

#[allow(dead_code)]
pub fn execute_native_tool_call_at_seq(
    ctx: &RuntimeContext,
    seq: u64,
    registry: &ToolRegistry,
    name: &str,
    kwargs: Value,
) -> Result<Value> {
    execute_tool_call_at_seq(ctx, seq, name, kwargs.clone(), || {
        registry.dispatch_native(name, kwargs)
    })
}

pub fn execute_call_agent(
    ctx: &RuntimeContext,
    args: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    execute_durable_json_call(ctx, "call_agent", args, live)
}

/// User-Agent that identifies chidori-issued requests to the wider internet.
/// Hosts like Wikimedia reject the bare `reqwest/X.Y` default with a 403 and a
/// link to their robot policy, so we ship a UA that names the runtime, its
/// version, and a contact URL by default. Callers can still override it by
/// including a `User-Agent` header in the http args.
const DEFAULT_USER_AGENT: &str = concat!(
    "chidori/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ThousandBirdsInc/chidori)",
);

pub fn execute_http(tokio_rt: &tokio::runtime::Runtime, args: &Value) -> Result<Value> {
    execute_http_with_secrets(
        tokio_rt,
        args,
        crate::runtime::secret_env::SecretStore::global(),
    )
}

/// `execute_http` with an explicit secret store — split out so tests can
/// inject a store without touching the process-wide one.
fn execute_http_with_secrets(
    tokio_rt: &tokio::runtime::Runtime,
    args: &Value,
    secrets: &crate::runtime::secret_env::SecretStore,
) -> Result<Value> {
    let method = args
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_string();
    let mut url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("http requires string url"))?
        .to_string();
    let mut headers = args.get("headers").and_then(|value| match value {
        Value::Object(map) => Some(map.clone()),
        _ => None,
    });
    let mut body = args.get("body").filter(|value| !value.is_null()).cloned();
    let mut params = args.get("params").and_then(|value| match value {
        Value::Object(map) => Some(map.clone()),
        _ => None,
    });

    // Secret broker: guest code only ever holds opaque placeholder tokens
    // (its `process.env` is built that way by the harness); the real values
    // are substituted here — after the durable call log captured the args in
    // token form — and only for hosts the secret's allowlist permits. The
    // substitution happens on the local copies above, so recorded args,
    // traces, and anything the guest can observe keep the token form.
    if !secrets.is_empty() {
        let host = url::Url::parse(&url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned));
        match host {
            Some(host) => {
                let deny = |err: String| anyhow::anyhow!("http secret substitution: {err}");
                url = secrets.substitute_str(&url, &host).map_err(deny)?;
                if let Some(map) = headers.as_mut() {
                    for (_, value) in map.iter_mut() {
                        secrets.substitute_value(value, &host).map_err(deny)?;
                    }
                }
                if let Some(map) = params.as_mut() {
                    for (_, value) in map.iter_mut() {
                        secrets.substitute_value(value, &host).map_err(deny)?;
                    }
                }
                if let Some(value) = body.as_mut() {
                    secrets.substitute_value(value, &host).map_err(deny)?;
                }
            }
            None => {
                // Unparseable URL: only an error if the request references a
                // secret token; otherwise let reqwest produce its usual error.
                let mentions_token =
                    crate::runtime::secret_env::SecretStore::looks_like_token(&args.to_string());
                if mentions_token {
                    anyhow::bail!(
                        "http secret substitution: cannot determine request host from url"
                    );
                }
            }
        }
    }

    let caller_set_user_agent = headers
        .as_ref()
        .is_some_and(|map| map.keys().any(|key| key.eq_ignore_ascii_case("user-agent")));

    tokio_rt.block_on(async move {
        // `gzip(true)` (with the reqwest `gzip` feature) sends Accept-Encoding
        // and transparently decompresses gzipped response bodies, so tools get
        // readable text instead of a binary blob.
        let mut client_builder = reqwest::Client::builder().gzip(true);
        // Only install the default UA when the caller hasn't named their own —
        // reqwest's `header()` appends rather than replaces, so setting both
        // would put two User-Agent headers on the wire.
        if !caller_set_user_agent {
            client_builder = client_builder.user_agent(DEFAULT_USER_AGENT);
        }
        let client = client_builder.build().map_err(|err| anyhow::anyhow!(err))?;
        let request_method =
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut req = client.request(request_method, &url);

        if let Some(headers) = headers {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    req = req.header(name, value);
                }
            }
        }

        if let Some(params) = params {
            let pairs: Vec<(String, String)> = params
                .into_iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    (key, value)
                })
                .collect();
            req = req.query(&pairs);
        }

        if let Some(body) = body {
            // A string body goes on the wire verbatim (so `fetch`/`node:http`
            // callers that pre-serialize with `JSON.stringify` aren't double-
            // encoded, and they keep control of `Content-Type` via headers).
            // Any other JSON value is sent as a JSON body with reqwest setting
            // `Content-Type: application/json` — the original `chidori.http`
            // object-body convenience.
            match body {
                Value::String(text) => {
                    req = req.body(text);
                }
                other => {
                    req = req.json(&other);
                }
            }
        }

        // Everything returned from here flows into the durable call log and
        // OTEL export, so secret values must never appear: transport errors
        // can embed the full (substituted) URL, and APIs may echo credentials
        // back in bodies or headers. `redact` maps them to [REDACTED:<KEY>].
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(err) => {
                if err.is_builder() {
                    return Err(anyhow::anyhow!(secrets.redact(&err.to_string())));
                }
                return Ok(json!({
                    "status": 0,
                    "headers": {},
                    "body": null,
                    "error": secrets.redact(&err.to_string()),
                }));
            }
        };
        let status = resp.status().as_u16();
        let mut response_headers = serde_json::Map::new();
        for (name, value) in resp.headers() {
            if let Ok(value) = value.to_str() {
                response_headers.insert(
                    name.as_str().to_string(),
                    Value::String(secrets.redact(value)),
                );
            }
        }
        let bytes = match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                return Ok(json!({
                    "status": status,
                    "headers": response_headers,
                    "body": null,
                    "error": secrets.redact(&err.to_string()),
                }));
            }
        };
        let text = secrets.redact(&String::from_utf8_lossy(&bytes));
        let body = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text));

        Ok(json!({
            "status": status,
            "headers": response_headers,
            "body": body,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Message as LlmMessage;
    use crate::runtime::context::{
        HostOperationCompletionSafepoint, HostOperationSafepoint, RuntimeContext,
    };
    use crate::runtime::snapshot::{
        HostOperationId, HostPromiseRecord, HostPromiseState, PendingHostOperation,
        PENDING_HOST_OPERATION_FILE,
    };

    #[test]
    fn durable_json_call_replays_without_live_execution() {
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "template".to_string(),
            args: json!({ "template": "ignored", "vars": {} }),
            result: json!("cached"),
            duration_ms: 1,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx = RuntimeContext::with_replay(replay);

        let result = execute_durable_json_call(
            &ctx,
            "template",
            json!({ "template": "{{ broken", "vars": {} }),
            || anyhow::bail!("live path should not run"),
        )
        .unwrap();

        assert_eq!(result, json!("cached"));
        assert_eq!(ctx.call_log().into_records().len(), 1);
    }

    #[test]
    fn replay_keeps_sequence_aligned_after_nested_host_call() {
        // Record: a container call (call_agent) whose `live()` makes a nested
        // host call (log), followed by an outer call (prompt). The nested log
        // burns a sequence number that sits *between* the container and the
        // next outer call.
        let ctx = RuntimeContext::new();
        let agent_result = execute_durable_json_call(
            &ctx,
            "call_agent",
            json!({ "path": "/child.ts", "input": { "value": 1 } }),
            || {
                // Nested host call inside the container's execution.
                execute_durable_json_call(&ctx, "log", json!({ "message": "inside" }), || {
                    Ok(Value::Null)
                })?;
                Ok(json!({ "child": 2 }))
            },
        )
        .unwrap();
        assert_eq!(agent_result, json!({ "child": 2 }));
        let prompt_result =
            execute_durable_json_call(&ctx, "prompt", json!({ "model": "m" }), || {
                Ok(json!("answer"))
            })
            .unwrap();
        assert_eq!(prompt_result, json!("answer"));

        let records = ctx.call_log().into_records();
        // call_agent(seq 1), nested log(seq 2, parent 1), prompt(seq 3).
        assert_eq!(records.len(), 3);
        assert!(records
            .iter()
            .any(|r| r.function == "log" && r.parent_seq == Some(1)));

        // Replay: the container short-circuits without re-running `live()`, so
        // the nested log's seq is never re-consumed. Before the subtree-absorb
        // fix the prompt would land on the log's seq and diverge.
        let replay_ctx = RuntimeContext::with_replay(records);
        let replayed_agent = execute_durable_json_call(
            &replay_ctx,
            "call_agent",
            json!({ "path": "/child.ts", "input": { "value": 1 } }),
            || anyhow::bail!("container live path must not run on replay"),
        )
        .unwrap();
        assert_eq!(replayed_agent, json!({ "child": 2 }));

        // This is the call that diverged before the fix.
        let replayed_prompt =
            execute_durable_json_call(&replay_ctx, "prompt", json!({ "model": "m" }), || {
                anyhow::bail!("prompt live path must not run on replay")
            })
            .unwrap();
        assert_eq!(replayed_prompt, json!("answer"));

        // The nested record is preserved in the replayed trace.
        let replayed_records = replay_ctx.call_log().into_records();
        assert_eq!(replayed_records.len(), 3);
        assert!(replayed_records
            .iter()
            .any(|r| r.function == "log" && r.parent_seq == Some(1)));
    }

    #[test]
    fn native_tool_call_logs_and_replays_without_callback_execution() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let calls_for_handler = calls.clone();
        registry.register_native("echo", "Echo input", Vec::new(), move |args| {
            calls_for_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(args)
        });

        let ctx = RuntimeContext::new();
        let result =
            execute_native_tool_call(&ctx, &registry, "echo", json!({ "value": 42 })).unwrap();
        assert_eq!(result, json!({ "value": 42 }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "tool");
        assert_eq!(
            records[0].args,
            json!({ "name": "echo", "kwargs": { "value": 42 } })
        );

        let replay_ctx = RuntimeContext::with_replay(records);
        let replayed = execute_native_tool_call(
            &replay_ctx,
            &registry,
            "echo",
            json!({ "value": "different live args" }),
        )
        .unwrap();

        assert_eq!(replayed, json!({ "value": 42 }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(replay_ctx.call_log().into_records().len(), 1);
    }

    #[test]
    fn durable_json_call_replays_completed_host_operation_without_live_execution() {
        let args = json!({ "name": "echo", "kwargs": { "value": 42 } });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Tool,
                args.clone(),
            ),
            state: HostPromiseState::Resolved {
                value: json!({ "value": 42 }),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);

        let result = execute_durable_json_call(&ctx, "tool", args, || {
            anyhow::bail!("live path should not run after completed host operation")
        })
        .unwrap();

        assert_eq!(result, json!({ "value": 42 }));
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "tool");
        assert_eq!(call_log[0].result, json!({ "value": 42 }));
    }

    #[test]
    fn durable_json_call_replays_rejected_host_operation_without_live_execution() {
        let args = json!({ "url": "https://example.invalid" });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Http,
                args.clone(),
            ),
            state: HostPromiseState::Rejected {
                error: "network failed after persistence".to_string(),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);

        let err = execute_durable_json_call(&ctx, "http", args, || {
            anyhow::bail!("live path should not run after rejected host operation")
        })
        .unwrap_err();

        assert!(err.to_string().contains("network failed after persistence"));
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "http");
        assert_eq!(
            call_log[0].error.as_deref(),
            Some("network failed after persistence")
        );
    }

    #[test]
    fn model_override_swaps_request_model_before_send() {
        use crate::providers::{LlmProvider, LlmResponse, TokenSink};
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        struct RecordingProvider {
            seen_model: StdArc<StdMutex<Option<String>>>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for RecordingProvider {
            fn supports_model(&self, _model: &str) -> bool {
                true
            }
            async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
                *self.seen_model.lock().unwrap() = Some(request.model.clone());
                Ok(LlmResponse {
                    content: "ok".to_string(),
                    blocks: vec![ContentBlock::Text {
                        text: "ok".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    stop_reason: "end_turn".to_string(),
                    input_tokens: 1,
                    output_tokens: 1,
                })
            }
            async fn stream(
                &self,
                request: &LlmRequest,
                _on_delta: &mut TokenSink,
            ) -> Result<LlmResponse> {
                self.send(request).await
            }
        }

        let seen_model = StdArc::new(StdMutex::new(None));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(RecordingProvider {
            seen_model: StdArc::clone(&seen_model),
        }));
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();

        let ctx = RuntimeContext::new();
        ctx.set_model_override(crate::runtime::context::ModelOverride::new(|| {
            Some("override-model".to_string())
        }));

        let request = LlmRequest {
            model: "request-model".to_string(),
            messages: vec![LlmMessage::user_text("hi".to_string())],
            system: None,
            temperature: 0.0,
            max_tokens: 16,
            tools: Vec::new(),
        };
        let _ =
            execute_prompt_response(&ctx, &providers, &tokio_rt, request, json!({}), None).unwrap();

        assert_eq!(
            seen_model.lock().unwrap().as_deref(),
            Some("override-model"),
            "the provider must receive the overridden model, not the request model"
        );
    }

    #[test]
    fn prompt_text_replays_completed_host_operation_without_provider_call() {
        let args = json!({
            "text": "hello",
            "model": "test-model",
            "type": "progress",
        });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Prompt,
                args.clone(),
            ),
            state: HostPromiseState::Resolved {
                value: json!("cached response"),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();

        let result = execute_prompt_text(
            &ctx,
            &ProviderRegistry::new(),
            &tokio_rt,
            LlmRequest {
                model: "test-model".to_string(),
                messages: vec![LlmMessage::user_text("hello".to_string())],
                system: None,
                temperature: 0.0,
                max_tokens: 16,
                tools: Vec::new(),
            },
            args,
            Some("progress".to_string()),
        )
        .unwrap();

        assert_eq!(result, json!("cached response"));
        assert_eq!(ctx.call_log().into_records().len(), 1);
    }

    #[test]
    fn template_call_uses_json_args() {
        let engine = TemplateEngine::new(".");
        let result = execute_template(
            &engine,
            &json!({
                "template": "Hello {{ name }}!",
                "vars": { "name": "core" },
            }),
        )
        .unwrap();

        assert_eq!(result, json!("Hello core!"));
    }

    #[test]
    fn durable_json_call_resolves_host_operation_after_success() {
        let ctx = RuntimeContext::new();
        let result = execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": {} }),
            || Ok(json!({ "ok": true })),
        )
        .unwrap();

        assert_eq!(result, json!({ "ok": true }));
        let records = ctx.host_promise_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation.kind, PendingHostOperationKind::Tool);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
    }

    #[test]
    fn durable_json_call_rejects_host_operation_after_error() {
        let ctx = RuntimeContext::new();
        let err = execute_durable_json_call(&ctx, "http", json!({ "url": "bad" }), || {
            anyhow::bail!("network failed")
        })
        .unwrap_err();

        assert!(err.to_string().contains("network failed"));
        let records = ctx.host_promise_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation.kind, PendingHostOperationKind::Http);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Rejected { .. }
        ));
    }

    #[test]
    fn execute_http_returns_status_zero_for_transport_error() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let result = execute_http(
            &tokio_rt,
            &json!({
                "url": "http://127.0.0.1:9/chidori-connection-refused",
                "method": "GET",
            }),
        )
        .unwrap();

        assert_eq!(result["status"], json!(0));
        assert!(result["error"].as_str().unwrap_or_default().len() > 0);
        assert!(result["headers"].as_object().unwrap().is_empty());
        assert!(result["body"].is_null());
    }

    /// Spin up a one-shot TCP server that captures the raw request bytes from
    /// the first connection. Returns the `(url, JoinHandle<raw_request>)` so
    /// the test can issue a request to `url` and then read what landed on the
    /// wire. The server replies with a minimal 200 response so reqwest doesn't
    /// surface a transport error.
    async fn one_shot_http_capture() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/ua-check");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            let _ = stream.shutdown().await;
            request
        });
        (url, handle)
    }

    fn user_agent_header(request: &str) -> Option<String> {
        request
            .lines()
            .find_map(|line| {
                line.strip_prefix("user-agent: ")
                    .or_else(|| line.strip_prefix("User-Agent: "))
            })
            .map(ToOwned::to_owned)
    }

    #[test]
    fn execute_http_sends_default_chidori_user_agent() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        execute_http(&tokio_rt, &json!({ "url": url, "method": "GET" })).unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        let ua = user_agent_header(&request).expect("missing User-Agent header");
        assert!(
            ua.starts_with("chidori/"),
            "expected chidori-prefixed UA, got {ua:?}"
        );
        assert!(
            ua.contains("github.com/ThousandBirdsInc/chidori"),
            "default UA should include contact URL, got {ua:?}"
        );
        // Exactly one User-Agent header should be on the wire — Wikimedia
        // rejects requests that send the bare `reqwest/` default *or* two UAs.
        let ua_count = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .count();
        assert_eq!(ua_count, 1, "request had {ua_count} User-Agent headers");
    }

    #[test]
    fn execute_http_caller_user_agent_overrides_default() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        execute_http(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "GET",
                "headers": { "User-Agent": "my-agent/1.0 (contact@example.com)" },
            }),
        )
        .unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        let ua = user_agent_header(&request).expect("missing User-Agent header");
        assert_eq!(ua, "my-agent/1.0 (contact@example.com)");
        // The default mustn't tag along behind the caller-supplied override.
        let ua_count = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .count();
        assert_eq!(ua_count, 1, "request had {ua_count} User-Agent headers");
    }

    #[test]
    fn execute_http_sends_string_body_verbatim() {
        // A string body must land on the wire unchanged (no JSON quoting), so a
        // `fetch`/`node:http` caller that pre-serialized with `JSON.stringify`
        // isn't double-encoded.
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        execute_http(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "POST",
                "headers": { "content-type": "application/json" },
                "body": "{\"a\":1}",
            }),
        )
        .unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        assert!(
            request.ends_with("{\"a\":1}"),
            "string body should be sent verbatim, got request:\n{request}"
        );
    }

    fn secret_test_store() -> crate::runtime::secret_env::SecretStore {
        use crate::runtime::secret_env::{SecretEntry, SECRET_TOKEN_PREFIX};
        crate::runtime::secret_env::SecretStore::for_tests(vec![
            (
                format!("{SECRET_TOKEN_PREFIX}aaaa1111__"),
                SecretEntry {
                    key: "LOCAL_API_KEY".into(),
                    value: "sk-local-secret-value".into(),
                    allowed_hosts: vec!["127.0.0.1".into()],
                    allow_any_host: false,
                },
            ),
            (
                format!("{SECRET_TOKEN_PREFIX}bbbb2222__"),
                SecretEntry {
                    key: "REMOTE_ONLY_KEY".into(),
                    value: "remote-only-value".into(),
                    allowed_hosts: vec!["api.example.com".into()],
                    allow_any_host: false,
                },
            ),
        ])
    }

    #[test]
    fn execute_http_substitutes_secret_for_allowed_host() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        let token = format!(
            "{}aaaa1111__",
            crate::runtime::secret_env::SECRET_TOKEN_PREFIX
        );
        execute_http_with_secrets(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "GET",
                "headers": { "Authorization": format!("Bearer {token}") },
            }),
            &secret_test_store(),
        )
        .unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        assert!(
            request.contains("Bearer sk-local-secret-value"),
            "wire request should carry the substituted secret, got: {request}"
        );
        assert!(
            !request.contains("__CHIDORI_SECRET__"),
            "placeholder token must not reach the wire: {request}"
        );
    }

    #[test]
    fn execute_http_fails_closed_for_disallowed_host() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        let token = format!(
            "{}bbbb2222__",
            crate::runtime::secret_env::SECRET_TOKEN_PREFIX
        );
        let err = execute_http_with_secrets(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "GET",
                "headers": { "Authorization": format!("Bearer {token}") },
            }),
            &secret_test_store(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("REMOTE_ONLY_KEY"),
            "error names the key: {err}"
        );
        assert!(err.contains("127.0.0.1"), "error names the host: {err}");
        assert!(
            !err.contains("remote-only-value"),
            "error must not leak the value: {err}"
        );
        // The request never went out: the capture server is still waiting.
        assert!(
            !server.is_finished(),
            "no request should reach the listener"
        );
        server.abort();
    }

    #[test]
    fn execute_http_redacts_echoed_secret_from_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{addr}/echo");
            let handle = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = "leaked: sk-local-secret-value";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Echo: sk-local-secret-value\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
            (url, handle)
        });
        let result = execute_http_with_secrets(
            &tokio_rt,
            &json!({ "url": url, "method": "GET" }),
            &secret_test_store(),
        )
        .unwrap();
        tokio_rt.block_on(server).unwrap();
        assert_eq!(
            result["body"],
            json!("leaked: [REDACTED:LOCAL_API_KEY]"),
            "response body must be redacted"
        );
        assert_eq!(
            result["headers"]["x-echo"],
            json!("[REDACTED:LOCAL_API_KEY]"),
            "response headers must be redacted"
        );
    }

    #[test]
    fn durable_json_call_pause_leaves_pending_host_operation_unrecorded() {
        let ctx = RuntimeContext::new();
        let err =
            execute_durable_json_call(&ctx, "tool", json!({ "name": "ask", "kwargs": {} }), || {
                anyhow::bail!("{PAUSE_MARKER}: approval required")
            })
            .unwrap_err();

        assert!(err.to_string().contains(PAUSE_MARKER));
        let pending = ctx.pending_host_operations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, PendingHostOperationKind::Tool);
        assert_eq!(pending[0].args, json!({ "name": "ask", "kwargs": {} }));
        assert!(ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn durable_json_call_persists_pending_operation_before_live_side_effect() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-operation-before-side-effect-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let pending_path = run_dir.join(PENDING_HOST_OPERATION_FILE);

        execute_durable_json_call(
            &ctx,
            "http",
            json!({ "url": "https://example.test" }),
            || {
                let pending: PendingHostOperation =
                    serde_json::from_slice(&std::fs::read(&pending_path)?)?;
                assert_eq!(pending.kind, PendingHostOperationKind::Http);
                assert_eq!(pending.args, json!({ "url": "https://example.test" }));
                Ok(json!({ "status": 200 }))
            },
        )
        .unwrap();

        assert!(!pending_path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn durable_json_call_runs_safepoint_after_pending_persist_before_live_side_effect() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-operation-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let pending_path = run_dir.join(PENDING_HOST_OPERATION_FILE);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let safepoint_events = events.clone();
        let safepoint_pending_path = pending_path.clone();
        ctx.set_host_operation_safepoint(HostOperationSafepoint::new(move |operation| {
            let pending: PendingHostOperation =
                serde_json::from_slice(&std::fs::read(&safepoint_pending_path)?)?;
            assert_eq!(pending.id, operation.id);
            assert_eq!(pending.kind, PendingHostOperationKind::Http);
            safepoint_events.lock().unwrap().push("safepoint");
            Ok(())
        }));

        execute_durable_json_call(
            &ctx,
            "http",
            json!({ "url": "https://example.test" }),
            || {
                events.lock().unwrap().push("live");
                Ok(json!({ "status": 200 }))
            },
        )
        .unwrap();

        assert_eq!(*events.lock().unwrap(), vec!["safepoint", "live"]);
        assert!(!pending_path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn durable_json_call_safepoint_failure_blocks_live_side_effect() {
        let ctx = RuntimeContext::new();
        let live_ran = std::sync::Arc::new(std::sync::Mutex::new(false));
        ctx.set_host_operation_safepoint(HostOperationSafepoint::new(|operation| {
            assert_eq!(operation.kind, PendingHostOperationKind::Http);
            anyhow::bail!("snapshot persistence failed")
        }));

        let live_ran_in_closure = live_ran.clone();
        let err = execute_durable_json_call(
            &ctx,
            "http",
            json!({ "url": "https://example.test" }),
            move || {
                *live_ran_in_closure.lock().unwrap() = true;
                Ok(json!({ "status": 200 }))
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("snapshot persistence failed"));
        assert!(!*live_ran.lock().unwrap());
        assert_eq!(ctx.pending_host_operations().len(), 1);
        assert!(ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn durable_json_call_runs_completion_safepoint_after_result_record() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-operation-completion-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let table_path = run_dir.join(crate::runtime::snapshot::HOST_PROMISE_TABLE_FILE);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let completion_events = events.clone();
        let completion_table_path = table_path.clone();
        ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
            move |record| {
                let records: Vec<HostPromiseRecord> =
                    serde_json::from_slice(&std::fs::read(&completion_table_path)?)?;
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].operation.id, record.operation.id);
                assert!(matches!(
                    records[0].state,
                    HostPromiseState::Resolved { .. }
                ));
                completion_events.lock().unwrap().push("completion");
                Ok(())
            },
        ));

        execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": {} }),
            || {
                events.lock().unwrap().push("live");
                Ok(json!({ "ok": true }))
            },
        )
        .unwrap();

        assert_eq!(*events.lock().unwrap(), vec!["live", "completion"]);
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "tool");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn durable_json_call_completion_safepoint_failure_keeps_completed_result_for_replay() {
        let ctx = RuntimeContext::new();
        ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
            |record| {
                assert!(matches!(record.state, HostPromiseState::Resolved { .. }));
                anyhow::bail!("snapshot persistence failed after result")
            },
        ));

        let err = execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": { "value": 41 } }),
            || Ok(json!({ "value": 42 })),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("snapshot persistence failed after result"));
        let records = ctx.host_promise_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation.kind, PendingHostOperationKind::Tool);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "tool");
        assert_eq!(call_log[0].result, json!({ "value": 42 }));
    }

    #[test]
    fn input_pause_leaves_pending_host_operation() {
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);

        let err = execute_input(&ctx, &json!({ "prompt": "Approve?" })).unwrap_err();

        assert!(err.to_string().contains(PAUSE_MARKER));
        let pending = ctx.pending_host_operations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, PendingHostOperationKind::Input);
        assert_eq!(pending[0].args, json!({ "prompt": "Approve?" }));
    }
}
