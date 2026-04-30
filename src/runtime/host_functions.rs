use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde_json::{json, Value};
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::AstModule;

use crate::runtime::dialect::studio_dialect;
use starlark::values::dict::{DictRef, UnpackDictEntries};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::{Heap, ProvidesStaticType, StringValue, Value as StarlarkValue};

use std::sync::Mutex as StdMutex;

use crate::mcp::McpManager;
use crate::policy::{Decision, PolicyCache, PolicyConfig};
use crate::providers::{
    ContentBlock, LlmRequest, LlmResponse, Message as LlmMessage, ProviderRegistry, ToolCall,
    ToolSchema,
};
use crate::runtime::call_log::{CallRecord, TokenUsage};
use crate::runtime::context::{
    InputMode, PendingApproval, PendingInput, RuntimeContext, PAUSE_MARKER,
};
use crate::runtime::sandbox;

/// Divergence-checked replay lookup. Wraps `RuntimeContext::try_replay_checked`
/// and converts the mismatch string into a `starlark::Error` so host functions
/// can use `?` to bail out of a divergent replay.
fn replay_or_live(
    ctx: &RuntimeContext,
    seq: u64,
    expected_fn: &str,
) -> starlark::Result<Option<CallRecord>> {
    ctx.try_replay_checked(seq, expected_fn)
        .map_err(|msg| starlark::Error::new_other(anyhow::anyhow!("{}", msg)))
}
use crate::runtime::template::TemplateEngine;
use crate::tools::{ToolDef, ToolRegistry};

/// Extra data attached to the Starlark Evaluator via `extra`.
/// Provides host functions access to the runtime context, providers, and template engine.
#[derive(ProvidesStaticType, allocative::Allocative)]
pub struct HostState {
    #[allocative(skip)]
    pub ctx: RuntimeContext,
    #[allocative(skip)]
    pub providers: Arc<ProviderRegistry>,
    #[allocative(skip)]
    pub template_engine: Arc<TemplateEngine>,
    #[allocative(skip)]
    pub tokio_rt: Arc<tokio::runtime::Runtime>,
    #[allocative(skip)]
    pub tools: Arc<ToolRegistry>,
    #[allocative(skip)]
    pub policy: Arc<PolicyConfig>,
    #[allocative(skip)]
    pub policy_cache: Arc<StdMutex<PolicyCache>>,
    #[allocative(skip)]
    pub mcp: Arc<McpManager>,
}

/// Resolve a host function call against the permission policy. Returns Ok
/// if the call should proceed, Err with a user-facing message otherwise.
///
/// AlwaysAllow → pass through.
/// AskBefore   → consult the per-run approval cache; if not yet approved,
///               surface the prompt via input() (reuses the pause mechanism).
///               Caching the approval means repeated calls in the same run
///               don't ask again.
/// NeverAllow  → refuse, with the rule's reason if present.
pub(crate) fn enforce_policy(
    host: &HostState,
    target: &str,
    args: &Value,
) -> anyhow::Result<()> {
    let (decision, reason) = host.policy.decide(target, args);
    match decision {
        Decision::AlwaysAllow => Ok(()),
        Decision::NeverAllow => Err(anyhow::anyhow!(
            "policy: `{}` denied{}",
            target,
            reason.map(|r| format!(" ({})", r)).unwrap_or_default()
        )),
        Decision::AskBefore => {
            {
                let cache = host.policy_cache.lock().unwrap();
                if cache.is_approved(target, args) {
                    return Ok(());
                }
            }
            if std::env::var("CHIDORI_POLICY_AUTO_APPROVE").ok().as_deref() == Some("1") {
                host.policy_cache.lock().unwrap().approve(target, args);
                return Ok(());
            }
            // In server mode (Pause) stash a PendingApproval on the context
            // and raise the pause sentinel — the engine's error handler
            // catches it and returns Paused so the HTTP layer can render an
            // approval UI and later call /sessions/{id}/approve.
            if host.ctx.input_mode() == InputMode::Pause {
                host.ctx.set_pending_approval(PendingApproval {
                    target: target.to_string(),
                    args: args.clone(),
                    reason: reason.clone(),
                });
                return Err(anyhow::anyhow!("{}", PAUSE_MARKER));
            }
            Err(anyhow::anyhow!(
                "policy: `{}` requires approval{}. Set CHIDORI_POLICY_AUTO_APPROVE=1 to \
                 auto-approve, or run through the server so the approval flow can pause.",
                target,
                reason.map(|r| format!(" — {}", r)).unwrap_or_default()
            ))
        }
    }
}

fn get_state<'a>(eval: &'a Evaluator) -> &'a HostState {
    eval.extra
        .expect("HostState not set on evaluator")
        .downcast_ref::<HostState>()
        .expect("extra is not HostState")
}

/// Convert a Starlark value to a serde_json Value (recursive).
pub fn starlark_to_json(v: StarlarkValue) -> Value {
    if v.is_none() {
        Value::Null
    } else if let Some(b) = v.unpack_bool() {
        Value::Bool(b)
    } else if let Some(i) = v.unpack_i32() {
        Value::Number(i.into())
    } else if let Some(s) = v.unpack_str() {
        Value::String(s.to_string())
    } else if let Some(dict) = DictRef::from_value(v) {
        let mut map = serde_json::Map::new();
        for (k, val) in dict.iter() {
            let key = k
                .unpack_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| k.to_repr());
            map.insert(key, starlark_to_json(val));
        }
        Value::Object(map)
    } else if let Some(list) = ListRef::from_value(v) {
        Value::Array(list.iter().map(starlark_to_json).collect())
    } else {
        // Fall back to string representation for complex types.
        let s = v.to_repr();
        let json_str = s
            .replace("True", "true")
            .replace("False", "false")
            .replace("None", "null")
            .replace('\'', "\"");
        serde_json::from_str(&json_str).unwrap_or(Value::String(s))
    }
}

/// Convert a serde_json Value to a Starlark value allocated on the given heap.
pub fn json_to_starlark<'v>(heap: &'v Heap, v: &Value) -> StarlarkValue<'v> {
    match v {
        Value::Null => StarlarkValue::new_none(),
        Value::Bool(b) => StarlarkValue::new_bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                heap.alloc(i as i32)
            } else if let Some(f) = n.as_f64() {
                heap.alloc(f)
            } else {
                StarlarkValue::new_none()
            }
        }
        Value::String(s) => heap.alloc_str(s).to_value(),
        Value::Array(arr) => {
            let items: Vec<StarlarkValue> =
                arr.iter().map(|item| json_to_starlark(heap, item)).collect();
            heap.alloc(AllocList(items))
        }
        Value::Object(map) => {
            let entries: Vec<(StarlarkValue, StarlarkValue)> = map
                .iter()
                .map(|(k, v)| {
                    let key = heap.alloc_str(k).to_value();
                    let val = json_to_starlark(heap, v);
                    (key, val)
                })
                .collect();
            let dict = starlark::values::dict::Dict::new(
                entries
                    .into_iter()
                    .map(|(k, v)| {
                        let hashed = k.get_hashed().unwrap();
                        (hashed, v)
                    })
                    .collect(),
            );
            heap.alloc(dict)
        }
    }
}

#[starlark_module]
pub fn host_functions(builder: &mut GlobalsBuilder) {
    /// Set agent-level configuration defaults.
    /// Called at module scope: config(model = "claude-sonnet-4-6", temperature = 0.7)
    fn config<'v>(
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let state = get_state(eval);
        state.ctx.update_config(|cfg| {
            for (key, val) in kwargs.entries {
                match key.as_str() {
                    "model" => {
                        if let Some(s) = val.unpack_str() {
                            cfg.model = s.to_string();
                        }
                    }
                    "temperature" => {
                        if let Some(f) = val.unpack_i32() {
                            cfg.temperature = f as f64;
                        } else if let Ok(f) = val.to_repr().parse::<f64>() {
                            cfg.temperature = f;
                        }
                    }
                    "max_tokens" => {
                        if let Some(i) = val.unpack_i32() {
                            cfg.max_tokens = i as u64;
                        }
                    }
                    "max_turns" => {
                        if let Some(i) = val.unpack_i32() {
                            cfg.max_turns = i as u64;
                        }
                    }
                    "timeout" => {
                        if let Some(i) = val.unpack_i32() {
                            cfg.timeout = i as u64;
                        }
                    }
                    _ => {} // Ignore unknown config keys for now.
                }
            }
        });
        Ok(NoneType)
    }

    /// Send a prompt to an LLM and return the response.
    ///
    /// prompt("What is 2+2?")
    /// prompt("Analyze data", model="claude-opus-4-6", temperature=0.2, format="json")
    /// prompt("Look up the weather", tools=["get_weather"], max_turns=5)
    fn prompt<'v>(
        text: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        // Snapshot state pieces so we don't hold a borrow across nested tool
        // evaluators in the tool-use loop.
        let (
            agent_config,
            ctx,
            providers,
            template_engine,
            tokio_rt,
            tools_registry,
            policy,
            policy_cache,
            mcp,
        ) = {
            let state = get_state(eval);
            (
                state.ctx.config(),
                state.ctx.clone(),
                state.providers.clone(),
                state.template_engine.clone(),
                state.tokio_rt.clone(),
                state.tools.clone(),
                state.policy.clone(),
                state.policy_cache.clone(),
                state.mcp.clone(),
            )
        };

        // Parse kwargs.
        let mut model = agent_config.model.clone();
        let mut temperature = agent_config.temperature;
        let mut max_tokens = agent_config.max_tokens;
        let mut max_turns = agent_config.max_turns;
        let mut system: Option<String> = None;
        let mut format: Option<String> = None;
        let mut tool_names: Vec<String> = Vec::new();

        for (key, val) in &kwargs.entries {
            match key.as_str() {
                "model" => {
                    if let Some(s) = val.unpack_str() {
                        model = s.to_string();
                    }
                }
                "temperature" => {
                    if let Some(i) = val.unpack_i32() {
                        temperature = i as f64;
                    } else if let Ok(f) = val.to_repr().parse::<f64>() {
                        temperature = f;
                    }
                }
                "max_tokens" => {
                    if let Some(i) = val.unpack_i32() {
                        max_tokens = i as u64;
                    }
                }
                "max_turns" => {
                    if let Some(i) = val.unpack_i32() {
                        max_turns = i as u64;
                    }
                }
                "system" => {
                    if let Some(s) = val.unpack_str() {
                        system = Some(s.to_string());
                    }
                }
                "format" => {
                    if let Some(s) = val.unpack_str() {
                        format = Some(s.to_string());
                    }
                }
                "tools" => {
                    if let Some(list) = ListRef::from_value(*val) {
                        for item in list.iter() {
                            if let Some(s) = item.unpack_str() {
                                tool_names.push(s.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Resolve tool names to schemas + keep the ToolDefs around for invocation.
        let mut tool_defs: Vec<ToolDef> = Vec::new();
        let mut tool_schemas: Vec<ToolSchema> = Vec::new();
        for name in &tool_names {
            let def = tools_registry.get(name).cloned().ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "Unknown tool in prompt(tools=...): {}",
                    name
                ))
            })?;
            tool_schemas.push(tool_def_to_schema(&def));
            tool_defs.push(def);
        }

        // Simple single-call path when no tools are in play. Keeps the call-log
        // shape identical to the pre-tool-use world for agents that don't use
        // LLM function calling.
        let prompt_text = text.as_str().to_string();
        if tool_schemas.is_empty() {
            let seq = ctx.next_seq();
            if let Some(cached) = replay_or_live(&ctx, seq, "prompt")? {
                if format.as_deref() == Some("json") {
                    if let Ok(val) = serde_json::from_str::<Value>(
                        cached.result.as_str().unwrap_or(""),
                    ) {
                        return Ok(json_to_starlark(eval.heap(), &val));
                    }
                }
                let content = cached.result.as_str().unwrap_or("").to_string();
                return Ok(eval.heap().alloc_str(&content).to_value());
            }

            let request = LlmRequest {
                model: model.clone(),
                messages: vec![LlmMessage::user_text(prompt_text.clone())],
                system: system.clone(),
                temperature,
                max_tokens,
                tools: Vec::new(),
            };

            let start = Instant::now();
            // Stream token deltas through the runtime event sender when
            // one is attached (i.e. the SSE endpoint is consuming events).
            // Otherwise fall back to a plain send() with no streaming cost.
            let response = if ctx.has_event_sender() {
                let ctx_for_cb = ctx.clone();
                let seq_for_cb = seq;
                let mut sink: crate::providers::TokenSink = Box::new(move |delta: &str| {
                    ctx_for_cb.emit_token_delta(seq_for_cb, delta.to_string());
                });
                tokio_rt.block_on(async { providers.stream(&request, &mut sink).await })
            } else {
                tokio_rt.block_on(async { providers.send(&request).await })
            };
            let duration_ms = start.elapsed().as_millis() as u64;

            return match response {
                Ok(resp) => {
                    ctx.record_call(CallRecord {
                        seq,
                        function: "prompt".to_string(),
                        args: json!({ "text": prompt_text, "model": model }),
                        result: json!(resp.content),
                        duration_ms,
                        token_usage: Some(TokenUsage {
                            input_tokens: resp.input_tokens,
                            output_tokens: resp.output_tokens,
                        }),
                        timestamp: Utc::now(),
                        error: None,
                    });
                    if format.as_deref() == Some("json") {
                        let json_str = extract_json(&resp.content);
                        match serde_json::from_str::<Value>(&json_str) {
                            Ok(val) => Ok(json_to_starlark(eval.heap(), &val)),
                            Err(_) => Ok(eval.heap().alloc_str(&resp.content).to_value()),
                        }
                    } else {
                        Ok(eval.heap().alloc_str(&resp.content).to_value())
                    }
                }
                Err(e) => {
                    ctx.record_call(CallRecord {
                        seq,
                        function: "prompt".to_string(),
                        args: json!({ "text": prompt_text, "model": model }),
                        result: Value::Null,
                        duration_ms,
                        token_usage: None,
                        timestamp: Utc::now(),
                        error: Some(e.to_string()),
                    });
                    Err(starlark::Error::new_other(e))
                }
            };
        }

        // Tool-use loop. Each LLM turn and each tool invocation consume their
        // own sequence number so the whole loop is replayable.
        let mut messages = vec![LlmMessage::user_text(prompt_text.clone())];
        let mut final_text = String::new();

        for _turn in 0..max_turns.max(1) {
            let seq = ctx.next_seq();

            // Try to replay this LLM turn. The cached result is a JSON object
            // with {content, blocks, tool_calls, stop_reason, usage}.
            let resp: LlmResponse = if let Some(cached) = replay_or_live(&ctx, seq, "prompt")? {
                match llm_response_from_record(&cached) {
                    Some(r) => r,
                    None => {
                        return Err(starlark::Error::new_other(anyhow::anyhow!(
                            "Cached prompt() record at seq {} is not a tool-use turn",
                            seq
                        )));
                    }
                }
            } else {
                let request = LlmRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    system: system.clone(),
                    temperature,
                    max_tokens,
                    tools: tool_schemas.clone(),
                };
                let start = Instant::now();
                let response = tokio_rt.block_on(async { providers.send(&request).await });
                let duration_ms = start.elapsed().as_millis() as u64;
                match response {
                    Ok(resp) => {
                        ctx.record_call(CallRecord {
                            seq,
                            function: "prompt".to_string(),
                            args: json!({
                                "text": prompt_text,
                                "model": model,
                                "tools": tool_names,
                                "turn": _turn,
                            }),
                            result: llm_response_to_json(&resp),
                            duration_ms,
                            token_usage: Some(TokenUsage {
                                input_tokens: resp.input_tokens,
                                output_tokens: resp.output_tokens,
                            }),
                            timestamp: Utc::now(),
                            error: None,
                        });
                        resp
                    }
                    Err(e) => {
                        ctx.record_call(CallRecord {
                            seq,
                            function: "prompt".to_string(),
                            args: json!({ "text": prompt_text, "model": model }),
                            result: Value::Null,
                            duration_ms,
                            token_usage: None,
                            timestamp: Utc::now(),
                            error: Some(e.to_string()),
                        });
                        return Err(starlark::Error::new_other(e));
                    }
                }
            };

            final_text = resp.content.clone();

            if resp.tool_calls.is_empty() {
                break;
            }

            // Push the assistant's response blocks verbatim so the next turn's
            // tool_result messages line up with the tool_use ids.
            messages.push(LlmMessage::assistant_blocks(resp.blocks.clone()));

            // Invoke each requested tool and build a user message containing
            // the tool_result blocks.
            let mut result_blocks: Vec<ContentBlock> = Vec::new();
            for call in &resp.tool_calls {
                let block = invoke_tool_call(
                    call,
                    &tool_defs,
                    HostState {
                        ctx: ctx.clone(),
                        providers: providers.clone(),
                        template_engine: template_engine.clone(),
                        tokio_rt: tokio_rt.clone(),
                        tools: tools_registry.clone(),
                        policy: policy.clone(),
                        policy_cache: policy_cache.clone(),
                        mcp: mcp.clone(),
                    },
                );
                result_blocks.push(block);
            }
            messages.push(LlmMessage {
                role: "user".to_string(),
                content: result_blocks,
            });
        }

        if format.as_deref() == Some("json") {
            let json_str = extract_json(&final_text);
            if let Ok(val) = serde_json::from_str::<Value>(&json_str) {
                return Ok(json_to_starlark(eval.heap(), &val));
            }
        }
        Ok(eval.heap().alloc_str(&final_text).to_value())
    }

    /// Render a Jinja2 template with the given variables.
    ///
    /// template("Hello {{ name }}!", name="world")
    /// template("prompts/analysis.jinja", items=items, format="detailed")
    fn template<'v>(
        template_str: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StringValue<'v>> {
        let state = get_state(eval);

        let mut vars = serde_json::Map::new();
        for (key, val) in kwargs.entries {
            vars.insert(key, starlark_to_json(val));
        }

        let result = state
            .template_engine
            .render(template_str.as_str(), &Value::Object(vars))
            .map_err(starlark::Error::new_other)?;

        Ok(eval.heap().alloc_str(&result))
    }

    /// Invoke a registered tool by name.
    ///
    /// tool("web_search", query="starlark rust", max_results=5)
    fn tool<'v>(
        tool_name: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        // Snapshot everything we need from state so we don't hold its
        // borrow across the nested evaluator below.
        let tool_name = tool_name.as_str().to_string();
        let (tool_def, ctx, providers, template_engine, tokio_rt, tools, policy, policy_cache, mcp) = {
            let state = get_state(eval);
            let tool_def = state.tools.get(&tool_name).cloned().ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!("Unknown tool: {}", tool_name))
            })?;
            (
                tool_def,
                state.ctx.clone(),
                state.providers.clone(),
                state.template_engine.clone(),
                state.tokio_rt.clone(),
                state.tools.clone(),
                state.policy.clone(),
                state.policy_cache.clone(),
                state.mcp.clone(),
            )
        };

        let mut args_json = serde_json::Map::new();
        for (k, v) in &kwargs.entries {
            args_json.insert(k.clone(), starlark_to_json(*v));
        }

        let seq = ctx.next_seq();

        // Replay hit — return the cached result directly.
        if let Some(cached) = replay_or_live(&ctx, seq, "tool")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        // Permission policy gate. Target string is "tool:<name>" so rules can
        // target specific tools rather than painting the whole class.
        let policy_target = format!("tool:{}", tool_name);
        let policy_args = serde_json::Value::Object(args_json.clone());
        {
            let probe = HostState {
                ctx: ctx.clone(),
                providers: providers.clone(),
                template_engine: template_engine.clone(),
                tokio_rt: tokio_rt.clone(),
                tools: tools.clone(),
                policy: policy.clone(),
                policy_cache: policy_cache.clone(),
                mcp: mcp.clone(),
            };
            if let Err(e) = enforce_policy(&probe, &policy_target, &policy_args) {
                ctx.record_call(CallRecord {
                    seq,
                    function: "tool".to_string(),
                    args: json!({"name": tool_name, "kwargs": args_json}),
                    result: Value::Null,
                    duration_ms: 0,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                return Err(starlark::Error::new_other(e));
            }
        }

        // MCP tools are dispatched to the persistent server process rather
        // than a local Starlark file. The ToolDef's backend field (added with
        // MCP support) carries the server id; if unset, fall through to the
        // Starlark invocation path.
        if let Some((server_id, remote_name)) = tool_def.mcp_backend() {
            let start = Instant::now();
            let mcp_result = tokio_rt.block_on(async {
                mcp.call_tool(&server_id, &remote_name, &Value::Object(args_json.clone()))
                    .await
            });
            let duration_ms = start.elapsed().as_millis() as u64;
            return match mcp_result {
                Ok(val) => {
                    ctx.record_call(CallRecord {
                        seq,
                        function: "tool".to_string(),
                        args: json!({"name": tool_name, "kwargs": args_json, "backend": "mcp"}),
                        result: val.clone(),
                        duration_ms,
                        token_usage: None,
                        timestamp: Utc::now(),
                        error: None,
                    });
                    Ok(json_to_starlark(eval.heap(), &val))
                }
                Err(e) => {
                    ctx.record_call(CallRecord {
                        seq,
                        function: "tool".to_string(),
                        args: json!({"name": tool_name, "kwargs": args_json, "backend": "mcp"}),
                        result: Value::Null,
                        duration_ms,
                        token_usage: None,
                        timestamp: Utc::now(),
                        error: Some(e.to_string()),
                    });
                    Err(starlark::Error::new_other(e))
                }
            };
        }

        let start = Instant::now();

        let result_json = invoke_tool_starlark(
            &tool_def.source_path.display().to_string(),
            &tool_def.source,
            &tool_name,
            &args_json,
            HostState {
                ctx: ctx.clone(),
                providers,
                template_engine,
                tokio_rt,
                tools,
                policy,
                policy_cache,
                mcp,
            },
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        match result_json {
            Ok(val) => {
                ctx.record_call(CallRecord {
                    seq,
                    function: "tool".to_string(),
                    args: json!({"name": tool_name, "kwargs": args_json}),
                    result: val.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &val))
            }
            Err(e) => {
                ctx.record_call(CallRecord {
                    seq,
                    function: "tool".to_string(),
                    args: json!({"name": tool_name, "kwargs": args_json}),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Make an HTTP request.
    ///
    /// http("https://api.example.com/users", method="POST",
    ///      headers={"Authorization": "Bearer ..."}, body={"name": "x"})
    ///
    /// Returns {"status": int, "headers": dict, "body": parsed-json-or-string}.
    fn http<'v>(
        url: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);

        let url_str = url.as_str().to_string();
        let mut method = "GET".to_string();
        let mut headers: Option<serde_json::Map<String, Value>> = None;
        let mut body: Option<Value> = None;
        let mut params: Option<serde_json::Map<String, Value>> = None;

        for (key, val) in &kwargs.entries {
            match key.as_str() {
                "method" => {
                    if let Some(s) = val.unpack_str() {
                        method = s.to_uppercase();
                    }
                }
                "headers" => {
                    if let Value::Object(m) = starlark_to_json(*val) {
                        headers = Some(m);
                    }
                }
                "body" => {
                    body = Some(starlark_to_json(*val));
                }
                "params" => {
                    if let Value::Object(m) = starlark_to_json(*val) {
                        params = Some(m);
                    }
                }
                _ => {}
            }
        }

        let seq = state.ctx.next_seq();

        if let Some(cached) = replay_or_live(&state.ctx, seq, "http")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        if let Err(e) = enforce_policy(
            state,
            "http",
            &json!({ "url": url_str, "method": method }),
        ) {
            return Err(starlark::Error::new_other(e));
        }

        let start = Instant::now();

        let request_method = method.clone();
        let request_url = url_str.clone();
        let request_headers = headers.clone();
        let request_body = body.clone();
        let request_params = params.clone();

        let response: anyhow::Result<Value> = state.tokio_rt.block_on(async move {
            let client = reqwest::Client::new();
            let m = reqwest::Method::from_bytes(request_method.as_bytes())
                .unwrap_or(reqwest::Method::GET);
            let mut req = client.request(m, &request_url);

            if let Some(h) = request_headers {
                for (k, v) in h {
                    if let Some(s) = v.as_str() {
                        req = req.header(k, s);
                    }
                }
            }
            if let Some(p) = request_params {
                let pairs: Vec<(String, String)> = p
                    .into_iter()
                    .map(|(k, v)| {
                        let vs = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
                        (k, vs)
                    })
                    .collect();
                req = req.query(&pairs);
            }
            if let Some(b) = request_body {
                req = req.json(&b);
            }

            let resp = req.send().await?;
            let status = resp.status().as_u16();
            let mut resp_headers = serde_json::Map::new();
            for (name, value) in resp.headers().iter() {
                if let Ok(s) = value.to_str() {
                    resp_headers.insert(name.as_str().to_string(), Value::String(s.to_string()));
                }
            }
            let bytes = resp.bytes().await?;
            let text = String::from_utf8_lossy(&bytes).to_string();
            let body_val = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));

            Ok(json!({
                "status": status,
                "headers": resp_headers,
                "body": body_val,
            }))
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match response {
            Ok(val) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "http".to_string(),
                    args: json!({
                        "url": url_str,
                        "method": method,
                        "headers": headers,
                        "body": body,
                        "params": params,
                    }),
                    result: val.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &val))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "http".to_string(),
                    args: json!({"url": url_str, "method": method}),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Read an environment variable.
    ///
    /// api_key = env("MY_API_KEY")
    fn env<'v>(
        name: StringValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        match std::env::var(name.as_str()) {
            Ok(val) => Ok(eval.heap().alloc_str(&val).to_value()),
            Err(_) => Ok(StarlarkValue::new_none()),
        }
    }

    /// Structured logging.
    ///
    /// log("Processing batch", count=len(items))
    fn log<'v>(
        message: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let state = get_state(eval);

        let mut fields = serde_json::Map::new();
        for (key, val) in kwargs.entries {
            fields.insert(key, starlark_to_json(val));
        }

        let seq = state.ctx.next_seq();
        // Check replay even for log() so divergence detection catches edits
        // that only shift around logging calls. On a hit we skip the live
        // tracing emit — the cached log line already exists in the checkpoint.
        if replay_or_live(&state.ctx, seq, "log")?.is_some() {
            return Ok(NoneType);
        }

        if fields.is_empty() {
            tracing::info!("{}", message.as_str());
        } else {
            let fields_str = serde_json::to_string(&Value::Object(fields)).unwrap_or_default();
            tracing::info!(message = message.as_str(), fields = %fields_str);
        }

        state.ctx.record_call(CallRecord {
            seq,
            function: "log".to_string(),
            args: json!({"message": message.as_str()}),
            result: Value::Null,
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        });

        Ok(NoneType)
    }

    /// Pause for human input and return the provided response as a string.
    ///
    /// answer = input("Approve this action? [y/n]")
    ///
    /// In CLI / interactive mode, reads one line from stdin.
    /// In server mode, raises a pause sentinel; the session goes to status
    /// "paused" and the agent resumes after a POST to /sessions/{id}/resume.
    /// When replaying from a checkpoint with a cached input response, returns
    /// the cached value immediately.
    fn input<'v>(
        prompt: StringValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let prompt_text = prompt.as_str().to_string();
        let seq = state.ctx.next_seq();

        if let Some(cached) = replay_or_live(&state.ctx, seq, "input")? {
            let content = cached.result.as_str().unwrap_or("").to_string();
            return Ok(eval.heap().alloc_str(&content).to_value());
        }

        match state.ctx.input_mode() {
            InputMode::Stdin => {
                eprintln!("{}", prompt_text);
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("stdin read: {}", e)))?;
                let response = line.trim_end_matches(&['\r', '\n'][..]).to_string();
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "input".to_string(),
                    args: json!({ "prompt": prompt_text }),
                    result: Value::String(response.clone()),
                    duration_ms: 0,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(eval.heap().alloc_str(&response).to_value())
            }
            InputMode::Pause => {
                state.ctx.set_pending_input(PendingInput {
                    seq,
                    prompt: prompt_text.clone(),
                });
                Err(starlark::Error::new_other(anyhow::anyhow!(
                    "{}: {}",
                    PAUSE_MARKER,
                    prompt_text
                )))
            }
        }
    }

    /// Call another .star agent file as a sub-agent.
    ///
    /// Named `call_agent` (not `agent`) to avoid shadowing the user's own
    /// `def agent(...)` binding at module scope.
    ///
    /// result = call_agent("agents/researcher.star", topic="rust")
    ///
    /// The sub-agent runs with the parent's HostState, so its host function
    /// calls are logged into the parent's call log and are replayable.
    fn call_agent<'v>(
        path: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let path_str = path.as_str().to_string();
        let (ctx, providers, template_engine, tokio_rt, tools, policy, policy_cache, mcp) = {
            let state = get_state(eval);
            (
                state.ctx.clone(),
                state.providers.clone(),
                state.template_engine.clone(),
                state.tokio_rt.clone(),
                state.tools.clone(),
                state.policy.clone(),
                state.policy_cache.clone(),
                state.mcp.clone(),
            )
        };

        let mut args_json = serde_json::Map::new();
        for (k, v) in &kwargs.entries {
            args_json.insert(k.clone(), starlark_to_json(*v));
        }

        let seq = ctx.next_seq();

        if let Some(cached) = replay_or_live(&ctx, seq, "call_agent")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let source = match std::fs::read_to_string(&path_str) {
            Ok(s) => s,
            Err(e) => {
                ctx.record_call(CallRecord {
                    seq,
                    function: "call_agent".to_string(),
                    args: json!({"path": path_str, "kwargs": args_json}),
                    result: Value::Null,
                    duration_ms: 0,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(format!("Failed to read sub-agent file: {}", e)),
                });
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "Failed to read sub-agent {}: {}",
                    path_str,
                    e
                )));
            }
        };

        let start = Instant::now();

        let result = invoke_tool_starlark(
            &path_str,
            &source,
            "agent",
            &args_json,
            HostState {
                ctx: ctx.clone(),
                providers,
                template_engine,
                tokio_rt,
                tools,
                policy,
                policy_cache,
                mcp,
            },
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(val) => {
                ctx.record_call(CallRecord {
                    seq,
                    function: "call_agent".to_string(),
                    args: json!({"path": path_str, "kwargs": args_json}),
                    result: val.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &val))
            }
            Err(e) => {
                ctx.record_call(CallRecord {
                    seq,
                    function: "call_agent".to_string(),
                    args: json!({"path": path_str, "kwargs": args_json}),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Execute a WebAssembly module in a bounded sandbox.
    ///
    /// exec(wat_source, function="add", args=[2, 3], fuel=1000000, memory_pages=16)
    ///
    /// The first positional arg can be either WAT text or raw .wasm bytes
    /// (as a string). Sandboxing is enforced by wasmer: Cranelift+metering
    /// caps instruction count via `fuel`, and bounded Tunables cap linear
    /// memory at `memory_pages` * 64 KiB. Returns a dict with `returns` (a
    /// list of the function's return values) and `fuel_remaining`.
    fn exec<'v>(
        source: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);

        let mut function = "main".to_string();
        let mut args_json: Value = Value::Array(Vec::new());
        let mut fuel: u64 = 1_000_000;
        let mut memory_pages: u32 = 16;
        for (k, v) in &kwargs.entries {
            match k.as_str() {
                "function" => {
                    if let Some(s) = v.unpack_str() {
                        function = s.to_string();
                    }
                }
                "args" => args_json = starlark_to_json(*v),
                "fuel" => {
                    if let Some(i) = v.unpack_i32() {
                        fuel = i.max(0) as u64;
                    }
                }
                "memory_pages" => {
                    if let Some(i) = v.unpack_i32() {
                        memory_pages = i.max(1) as u32;
                    }
                }
                _ => {}
            }
        }

        let source_bytes = source.as_str().as_bytes().to_vec();
        let seq = state.ctx.next_seq();

        if let Some(cached) = replay_or_live(&state.ctx, seq, "exec")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let wasm_args = match sandbox::parse_args(&args_json) {
            Ok(a) => a,
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec".to_string(),
                    args: json!({ "function": function, "args": args_json }),
                    result: Value::Null,
                    duration_ms: 0,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                return Err(starlark::Error::new_other(e));
            }
        };

        let req = sandbox::ExecRequest {
            wasm_source: source_bytes,
            function: function.clone(),
            args: wasm_args,
            fuel,
            memory_pages,
            // Route sandboxed `host.log(ptr, len)` calls through the host's
            // tracing subsystem so they show up under --verbose.
            log_callback: Some(Arc::new(|msg: &str| {
                tracing::info!(target: "wasm_sandbox", "{}", msg);
            })),
        };

        let start = Instant::now();
        let result = sandbox::exec_wasm(req);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(exec_result) => {
                let result_json = json!({
                    "returns": exec_result.returns,
                    "fuel_remaining": exec_result.fuel_remaining,
                });
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec".to_string(),
                    args: json!({ "function": function, "args": args_json, "fuel": fuel }),
                    result: result_json.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &result_json))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec".to_string(),
                    args: json!({ "function": function, "args": args_json, "fuel": fuel }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Run a JavaScript program inside the WASM sandbox.
    ///
    /// Uses the embedded `sandbox-js` WASI binary, which links against
    /// `boa_engine` compiled to `wasm32-wasip1`. The program's final
    /// expression is returned as `String(value)`; errors surface as
    /// Starlark errors.
    ///
    /// out = exec_js("6 * 7")                                 # "42"
    /// out = exec_js("[1,2,3].map(x => x*x).reduce((a,b)=>a+b, 0)")  # "14"
    fn exec_js<'v>(
        source: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let source_str = source.as_str().to_string();

        let mut fuel: u64 = 200_000_000;
        for (k, v) in &kwargs.entries {
            if k == "fuel" {
                if let Some(i) = v.unpack_i32() {
                    fuel = (i.max(0) as u64).max(1);
                }
            }
        }

        let seq = state.ctx.next_seq();
        if let Some(cached) = replay_or_live(&state.ctx, seq, "exec_js")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let start = Instant::now();
        let result = sandbox::exec_js(&source_str, fuel);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(out) => {
                let result_json = Value::String(out.clone());
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec_js".to_string(),
                    args: json!({ "source": source_str, "fuel": fuel }),
                    result: result_json.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &result_json))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec_js".to_string(),
                    args: json!({ "source": source_str, "fuel": fuel }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Run a Python program inside the WASM sandbox.
    ///
    /// Uses the embedded `sandbox-python` WASI binary, which links against
    /// `rustpython-vm` compiled to `wasm32-wasip1`. The host side drives a
    /// minimal hand-rolled WASI preview 1 shim (stdin preloaded with the
    /// source, stdout captured, fixed clock, zero preopens) instead of
    /// pulling in `wasmer-wasix`. Fuel metering from the main wasmer
    /// sandbox still applies.
    ///
    /// The program should assign its final value to a top-level `result`
    /// variable — the sandbox returns `repr(result)` as the string result.
    ///
    /// out = exec_python("result = sum(range(10))")           # "45"
    /// out = exec_python("def f(x): return x*x\nresult = f(7)") # "49"
    fn exec_python<'v>(
        source: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let source_str = source.as_str().to_string();

        let mut fuel: u64 = 200_000_000;
        for (k, v) in &kwargs.entries {
            if k == "fuel" {
                if let Some(i) = v.unpack_i32() {
                    fuel = (i.max(0) as u64).max(1);
                }
            }
        }

        let seq = state.ctx.next_seq();
        if let Some(cached) = replay_or_live(&state.ctx, seq, "exec_python")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let start = Instant::now();
        let result = sandbox::exec_python(&source_str, fuel);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(out) => {
                let result_json = Value::String(out.clone());
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec_python".to_string(),
                    args: json!({ "source": source_str, "fuel": fuel }),
                    result: result_json.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &result_json))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec_python".to_string(),
                    args: json!({ "source": source_str, "fuel": fuel }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Evaluate a miniscript expression inside the WASM sandbox.
    ///
    /// result = exec_expr("(2 + 3) * 4", fuel = 100000)        # "20"
    /// result = exec_expr("a * b + 1", vars = {"a": 7, "b": 6}) # "43"
    /// result = exec_expr("if a > b then a else b", vars = {"a": 3, "b": 9})  # "9"
    ///
    /// Uses the embedded `sandbox-runtime` WASM binary — a no_std Rust
    /// recursive-descent interpreter cross-compiled to wasm32 and shipped
    /// inside the host binary via `include_bytes!`. The language supports
    /// integers, booleans, `+ - * / %`, `< <= > >= == !=`, `&& || !`,
    /// `let NAME = EXPR in EXPR`, and `if EXPR then EXPR else EXPR`.
    /// Host-supplied vars are prepended as `let name = value in …` chains
    /// before the source enters the sandbox. Participates in the replay
    /// cache + divergence detection.
    fn exec_expr<'v>(
        source: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let source_str = source.as_str().to_string();

        let mut fuel: u64 = 1_000_000;
        let mut vars_json = serde_json::Map::new();
        for (k, v) in &kwargs.entries {
            match k.as_str() {
                "fuel" => {
                    if let Some(i) = v.unpack_i32() {
                        fuel = i.max(0) as u64;
                    }
                }
                "vars" => {
                    if let Value::Object(m) = starlark_to_json(*v) {
                        vars_json = m;
                    }
                }
                _ => {}
            }
        }

        let seq = state.ctx.next_seq();
        if let Some(cached) = replay_or_live(&state.ctx, seq, "exec_expr")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let start = Instant::now();
        let result = sandbox::exec_expr(&source_str, &vars_json, fuel);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(out) => {
                let result_json = Value::String(out.clone());
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec_expr".to_string(),
                    args: json!({ "source": source_str, "vars": vars_json, "fuel": fuel }),
                    result: result_json.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &result_json))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "exec_expr".to_string(),
                    args: json!({ "source": source_str, "vars": vars_json, "fuel": fuel }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Execute a whitelisted shell command and capture its output.
    ///
    /// shell("ls", args=["-la", "."], timeout_ms=2000)
    /// shell("git", args=["log", "--oneline", "-n", "5"], cwd="/repo")
    ///
    /// Security model: the command name must appear in the
    /// `CHIDORI_SHELL_ALLOW` env var (comma-separated, e.g. `"ls,cat,echo,git"`)
    /// or be literally `*` (allow anything — do not set in production).
    /// The allow list is checked against the *bare* command name, not a full
    /// path, and args are passed directly to the OS via `execvp`-style
    /// invocation — no shell interpreter, so no quoting / globbing /
    /// substitution vulnerabilities.
    ///
    /// Returns a dict: `{stdout: str, stderr: str, exit_code: int, timed_out: bool}`.
    /// Participates in the replay cache and divergence detection.
    fn shell<'v>(
        command: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let cmd = command.as_str().to_string();

        let mut args: Vec<String> = Vec::new();
        let mut cwd: Option<String> = None;
        let mut timeout_ms: u64 = 5000;
        let mut env_vars: Vec<(String, String)> = Vec::new();

        for (k, v) in &kwargs.entries {
            match k.as_str() {
                "args" => {
                    if let Some(list) = ListRef::from_value(*v) {
                        for item in list.iter() {
                            if let Some(s) = item.unpack_str() {
                                args.push(s.to_string());
                            } else {
                                args.push(item.to_repr());
                            }
                        }
                    }
                }
                "cwd" => {
                    if let Some(s) = v.unpack_str() {
                        cwd = Some(s.to_string());
                    }
                }
                "timeout_ms" => {
                    if let Some(i) = v.unpack_i32() {
                        timeout_ms = i.max(0) as u64;
                    }
                }
                "env" => {
                    if let Value::Object(m) = starlark_to_json(*v) {
                        for (k, val) in m {
                            if let Some(s) = val.as_str() {
                                env_vars.push((k, s.to_string()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let seq = state.ctx.next_seq();
        if let Some(cached) = replay_or_live(&state.ctx, seq, "shell")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        // Policy check on top of the allow list.
        let policy_args = json!({ "command": cmd, "args": args });
        if let Err(e) = enforce_policy(state, "shell", &policy_args) {
            return Err(starlark::Error::new_other(e));
        }

        // Enforce the allow list before we spend any syscalls.
        if let Err(e) = check_shell_allowed(&cmd) {
            state.ctx.record_call(CallRecord {
                seq,
                function: "shell".to_string(),
                args: json!({ "command": cmd, "args": args }),
                result: Value::Null,
                duration_ms: 0,
                token_usage: None,
                timestamp: Utc::now(),
                error: Some(e.to_string()),
            });
            return Err(starlark::Error::new_other(e));
        }

        let start = Instant::now();
        let cmd_clone = cmd.clone();
        let args_clone = args.clone();
        let cwd_clone = cwd.clone();
        let env_clone = env_vars.clone();

        // Run under tokio so we get cancellation-on-timeout for free. The
        // process is killed when the future is dropped on timeout.
        let exec_outcome: anyhow::Result<ShellOutcome> =
            state.tokio_rt.block_on(async move {
                use tokio::process::Command as TokioCommand;
                let mut command = TokioCommand::new(&cmd_clone);
                command
                    .args(&args_clone)
                    .kill_on_drop(true)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .stdin(std::process::Stdio::null());
                if let Some(ref dir) = cwd_clone {
                    command.current_dir(dir);
                }
                // Start from an empty env and add just what the caller asked
                // for — prevents secret leakage via PATH / AWS_* / etc.
                command.env_clear();
                for (k, v) in &env_clone {
                    command.env(k, v);
                }

                let fut = command.output();
                let dur = std::time::Duration::from_millis(timeout_ms);
                match tokio::time::timeout(dur, fut).await {
                    Ok(Ok(output)) => Ok(ShellOutcome {
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        exit_code: output.status.code().unwrap_or(-1),
                        timed_out: false,
                    }),
                    Ok(Err(e)) => Err(anyhow::anyhow!("spawn `{}`: {}", cmd_clone, e)),
                    Err(_) => Ok(ShellOutcome {
                        stdout: String::new(),
                        stderr: format!("timed out after {}ms", timeout_ms),
                        exit_code: -1,
                        timed_out: true,
                    }),
                }
            });

        let duration_ms = start.elapsed().as_millis() as u64;

        match exec_outcome {
            Ok(outcome) => {
                let result = json!({
                    "stdout": outcome.stdout,
                    "stderr": outcome.stderr,
                    "exit_code": outcome.exit_code,
                    "timed_out": outcome.timed_out,
                });
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "shell".to_string(),
                    args: json!({
                        "command": cmd,
                        "args": args,
                        "cwd": cwd,
                        "timeout_ms": timeout_ms,
                    }),
                    result: result.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &result))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "shell".to_string(),
                    args: json!({ "command": cmd, "args": args }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Read a file from disk and return its contents as a string.
    ///
    /// content = read_file("data/input.txt")
    fn read_file<'v>(
        path: StringValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let path_str = path.as_str().to_string();
        let seq = state.ctx.next_seq();

        if let Some(cached) = replay_or_live(&state.ctx, seq, "read_file")? {
            let content = cached.result.as_str().unwrap_or("").to_string();
            return Ok(eval.heap().alloc_str(&content).to_value());
        }

        let start = Instant::now();
        let result = std::fs::read_to_string(&path_str);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(content) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "read_file".to_string(),
                    args: json!({ "path": path_str }),
                    result: Value::String(content.clone()),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(eval.heap().alloc_str(&content).to_value())
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "read_file".to_string(),
                    args: json!({ "path": path_str }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(anyhow::anyhow!(
                    "read_file({}): {}",
                    path_str,
                    e
                )))
            }
        }
    }

    /// Write a string to a file, creating parent directories as needed.
    ///
    /// write_file("out/report.md", "# Report\n...")
    fn write_file<'v>(
        path: StringValue<'v>,
        content: StringValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let state = get_state(eval);
        let path_str = path.as_str().to_string();
        let content_str = content.as_str().to_string();
        let seq = state.ctx.next_seq();

        if replay_or_live(&state.ctx, seq, "write_file")?.is_some() {
            return Ok(NoneType);
        }

        if let Err(e) = enforce_policy(state, "write_file", &json!({ "path": path_str })) {
            return Err(starlark::Error::new_other(e));
        }

        let start = Instant::now();
        let result: anyhow::Result<()> = (|| {
            if let Some(parent) = std::path::Path::new(&path_str).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&path_str, &content_str)?;
            Ok(())
        })();
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(()) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "write_file".to_string(),
                    args: json!({ "path": path_str, "bytes": content_str.len() }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(NoneType)
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "write_file".to_string(),
                    args: json!({ "path": path_str }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(anyhow::anyhow!(
                    "write_file({}): {}",
                    path_str,
                    e
                )))
            }
        }
    }

    /// List entries in a directory. Returns a list of `{name, is_dir}` dicts.
    ///
    /// for entry in list_dir("data/"):
    ///     if not entry["is_dir"]:
    ///         log("file", name=entry["name"])
    fn list_dir<'v>(
        path: StringValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let path_str = path.as_str().to_string();
        let seq = state.ctx.next_seq();

        if let Some(cached) = replay_or_live(&state.ctx, seq, "list_dir")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let start = Instant::now();
        let result: anyhow::Result<Value> = (|| {
            let mut entries: Vec<Value> = Vec::new();
            for entry in std::fs::read_dir(&path_str)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                entries.push(json!({ "name": name, "is_dir": is_dir }));
            }
            Ok(Value::Array(entries))
        })();
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(val) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "list_dir".to_string(),
                    args: json!({ "path": path_str }),
                    result: val.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &val))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "list_dir".to_string(),
                    args: json!({ "path": path_str }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(anyhow::anyhow!(
                    "list_dir({}): {}",
                    path_str,
                    e
                )))
            }
        }
    }

    /// Persistent key-value memory backed by a JSON file per namespace.
    ///
    /// memory("set", "user_pref", {"theme": "dark"})
    /// pref = memory("get", "user_pref")                        # or None
    /// memory("delete", "user_pref")
    /// all = memory("list", prefix="user_", namespace="profile")
    ///
    /// Storage: `.chidori/memory/<namespace>.json`. Goes through the
    /// replay cache so replays return the values observed at record time,
    /// independent of what's on disk now.
    fn memory<'v>(
        action: StringValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let action_str = action.as_str().to_string();

        let mut key: Option<String> = None;
        let mut value: Option<Value> = None;
        let mut namespace = "default".to_string();
        let mut prefix = String::new();
        for (k, v) in &kwargs.entries {
            match k.as_str() {
                "key" => {
                    if let Some(s) = v.unpack_str() {
                        key = Some(s.to_string());
                    }
                }
                "value" => value = Some(starlark_to_json(*v)),
                "namespace" => {
                    if let Some(s) = v.unpack_str() {
                        namespace = s.to_string();
                    }
                }
                "prefix" => {
                    if let Some(s) = v.unpack_str() {
                        prefix = s.to_string();
                    }
                }
                _ => {}
            }
        }

        let seq = state.ctx.next_seq();
        if let Some(cached) = replay_or_live(&state.ctx, seq, "memory")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let start = Instant::now();
        let result = memory_execute(&action_str, &namespace, key.as_deref(), value.as_ref(), &prefix);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(val) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "memory".to_string(),
                    args: json!({
                        "action": action_str,
                        "key": key,
                        "namespace": namespace,
                        "prefix": prefix,
                    }),
                    result: val.clone(),
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
                Ok(json_to_starlark(eval.heap(), &val))
            }
            Err(e) => {
                state.ctx.record_call(CallRecord {
                    seq,
                    function: "memory".to_string(),
                    args: json!({
                        "action": action_str,
                        "key": key,
                        "namespace": namespace,
                    }),
                    result: Value::Null,
                    duration_ms,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: Some(e.to_string()),
                });
                Err(starlark::Error::new_other(e))
            }
        }
    }

    /// Run a list of callables and return their results in order.
    ///
    /// results = parallel([
    ///     lambda: prompt("summarize doc A"),
    ///     lambda: prompt("summarize doc B"),
    /// ])
    ///
    /// Note: today each branch executes sequentially — the Starlark
    /// evaluator is single-threaded and lambdas are bound to its heap, so
    /// they can't cross threads. The API is in place so agents can be
    /// written against it; a future implementation may run branches truly
    /// concurrently when the runtime supports it.
    fn parallel<'v>(
        branches: StarlarkValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let list = ListRef::from_value(branches).ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "parallel() expects a list of callables"
            ))
        })?;

        let callables: Vec<StarlarkValue<'v>> = list.iter().collect();
        let mut results: Vec<StarlarkValue<'v>> = Vec::with_capacity(callables.len());
        for callable in callables {
            let value = eval.eval_function(callable, &[], &[])?;
            results.push(value);
        }
        Ok(eval.heap().alloc(AllocList(results)))
    }

    /// Invoke a callable and capture any error without propagating.
    ///
    /// Returns a dict `{"value": result, "error": None}` on success or
    /// `{"value": None, "error": "<message>"}` on failure.
    ///
    /// result = try_call(lambda: prompt("..."))
    /// if result["error"]:
    ///     log("failed", err=result["error"])
    fn try_call<'v>(
        callable: StarlarkValue<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let heap = eval.heap();
        match eval.eval_function(callable, &[], &[]) {
            Ok(value) => {
                let entries = vec![
                    (heap.alloc_str("value").to_value(), value),
                    (heap.alloc_str("error").to_value(), StarlarkValue::new_none()),
                ];
                Ok(alloc_dict(heap, entries))
            }
            Err(e) => {
                let msg = heap.alloc_str(&e.to_string()).to_value();
                let entries = vec![
                    (heap.alloc_str("value").to_value(), StarlarkValue::new_none()),
                    (heap.alloc_str("error").to_value(), msg),
                ];
                Ok(alloc_dict(heap, entries))
            }
        }
    }

    /// Retry a callable until it succeeds or attempts are exhausted.
    ///
    /// retry(lambda: prompt("..."), max_attempts=3, backoff="exponential", initial_delay_ms=200)
    ///
    /// backoff: "constant" | "linear" | "exponential" (default "exponential")
    /// On final failure, the last error is raised.
    fn retry<'v>(
        callable: StarlarkValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let mut max_attempts: u32 = 3;
        let mut backoff = "exponential".to_string();
        let mut initial_delay_ms: u64 = 200;

        for (key, val) in &kwargs.entries {
            match key.as_str() {
                "max_attempts" => {
                    if let Some(i) = val.unpack_i32() {
                        max_attempts = i.max(1) as u32;
                    }
                }
                "backoff" => {
                    if let Some(s) = val.unpack_str() {
                        backoff = s.to_string();
                    }
                }
                "initial_delay_ms" => {
                    if let Some(i) = val.unpack_i32() {
                        initial_delay_ms = i.max(0) as u64;
                    }
                }
                _ => {}
            }
        }

        let mut last_err: Option<starlark::Error> = None;
        for attempt in 0..max_attempts {
            match eval.eval_function(callable, &[], &[]) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 == max_attempts {
                        break;
                    }
                    let delay_ms = match backoff.as_str() {
                        "constant" => initial_delay_ms,
                        "linear" => initial_delay_ms * (attempt as u64 + 1),
                        _ => initial_delay_ms * (1u64 << attempt),
                    };
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!("retry() failed with no attempts"))
        }))
    }

    /// Summarize a list of prior messages into a single condensed string.
    ///
    /// compact(messages, keep_last=3, system="You are a summarizer.")
    ///
    /// `messages` is a list of dicts `{role, content}` (same shape agents use
    /// to build their own histories). `keep_last` is the tail length to leave
    /// untouched. Returns a dict `{summary, kept}` where `kept` is the tail
    /// that was preserved. The summary call flows through the same provider
    /// + replay cache as prompt(), so compaction is deterministic under
    /// replay.
    fn compact<'v>(
        messages: StarlarkValue<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<String, StarlarkValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkValue<'v>> {
        let state = get_state(eval);
        let list = ListRef::from_value(messages).ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "compact() expects a list of {{role, content}} dicts"
            ))
        })?;

        let mut keep_last: usize = 3;
        let mut system_prompt: Option<String> = None;
        let mut model = state.ctx.config().model.clone();
        for (k, v) in &kwargs.entries {
            match k.as_str() {
                "keep_last" => {
                    if let Some(i) = v.unpack_i32() {
                        keep_last = i.max(0) as usize;
                    }
                }
                "system" => {
                    if let Some(s) = v.unpack_str() {
                        system_prompt = Some(s.to_string());
                    }
                }
                "model" => {
                    if let Some(s) = v.unpack_str() {
                        model = s.to_string();
                    }
                }
                _ => {}
            }
        }

        let all: Vec<Value> = list.iter().map(starlark_to_json).collect();
        let total = all.len();
        let split = total.saturating_sub(keep_last);
        let (head, tail) = all.split_at(split);

        let seq = state.ctx.next_seq();
        if let Some(cached) = replay_or_live(&state.ctx, seq, "compact")? {
            return Ok(json_to_starlark(eval.heap(), &cached.result));
        }

        let flattened: String = head
            .iter()
            .map(|m| {
                let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                let content = m
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| m.get("content").map(|v| v.to_string()).unwrap_or_default());
                format!("{}: {}", role, content)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let request = LlmRequest {
            model: model.clone(),
            messages: vec![LlmMessage::user_text(format!(
                "Summarize the following conversation into a dense recap that preserves \
                 decisions, open questions, and any concrete facts the assistant has committed to. \
                 Keep under 400 words.\n\n{}",
                flattened
            ))],
            system: system_prompt.clone().or_else(|| {
                Some("You compact long conversations into dense recaps.".to_string())
            }),
            temperature: 0.2,
            max_tokens: 800,
            tools: Vec::new(),
        };

        let start = Instant::now();
        let response = state
            .tokio_rt
            .block_on(async { state.providers.send(&request).await });
        let duration_ms = start.elapsed().as_millis() as u64;

        let (summary, usage, err) = match response {
            Ok(resp) => (
                resp.content.clone(),
                Some(TokenUsage {
                    input_tokens: resp.input_tokens,
                    output_tokens: resp.output_tokens,
                }),
                None,
            ),
            Err(e) => (String::new(), None, Some(e.to_string())),
        };

        let result = json!({
            "summary": summary,
            "kept": tail,
            "compacted": head.len(),
        });

        state.ctx.record_call(CallRecord {
            seq,
            function: "compact".to_string(),
            args: json!({ "model": model, "keep_last": keep_last, "total": total }),
            result: result.clone(),
            duration_ms,
            token_usage: usage,
            timestamp: Utc::now(),
            error: err.clone(),
        });

        if let Some(e) = err {
            return Err(starlark::Error::new_other(anyhow::anyhow!(e)));
        }

        Ok(json_to_starlark(eval.heap(), &result))
    }
}

/// Allocate a Starlark dict from an ordered list of (key, value) pairs.
fn alloc_dict<'v>(
    heap: &'v Heap,
    entries: Vec<(StarlarkValue<'v>, StarlarkValue<'v>)>,
) -> StarlarkValue<'v> {
    let pairs: starlark::collections::SmallMap<_, _> = entries
        .into_iter()
        .map(|(k, v)| (k.get_hashed().unwrap(), v))
        .collect();
    heap.alloc(starlark::values::dict::Dict::new(pairs))
}

/// Load a tool .star file into a fresh evaluator, locate the named function,
/// invoke it with the given kwargs (as JSON), and return the result as JSON.
///
/// The sub-evaluator shares the same `HostState` so the tool can transitively
/// use `prompt()`, `template()`, `http()`, and even `tool()` itself.
fn invoke_tool_starlark(
    source_name: &str,
    source: &str,
    fn_name: &str,
    kwargs: &serde_json::Map<String, Value>,
    host: HostState,
) -> anyhow::Result<Value> {
    let ast = AstModule::parse(source_name, source.to_string(), &studio_dialect())
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let mut builder = GlobalsBuilder::standard();
    host_functions(&mut builder);
    let globals = builder.build();

    // Evaluate the tool module.
    let tool_module = Module::new();
    {
        let mut tool_eval = Evaluator::new(&tool_module);
        tool_eval.extra = Some(&host);
        tool_eval
            .eval_module(ast, &globals)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    let frozen = tool_module
        .freeze()
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let fn_val = frozen
        .get(fn_name)
        .map_err(|_| anyhow::anyhow!("Tool function `{}` not found in module", fn_name))?;

    // Call the function in a fresh evaluator.
    let call_module = Module::new();
    let mut call_eval = Evaluator::new(&call_module);
    call_eval.extra = Some(&host);

    let heap = call_eval.heap();
    let owned: Vec<(String, StarlarkValue)> = kwargs
        .iter()
        .map(|(k, v)| (k.clone(), json_to_starlark(heap, v)))
        .collect();
    let refs: Vec<(&str, StarlarkValue)> = owned.iter().map(|(k, v)| (k.as_str(), *v)).collect();

    let result = call_eval
        .eval_function(fn_val.value(), &[], &refs)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(starlark_to_json(result))
}

/// Captured output of a sandboxed `shell()` call. Mirrors the dict we hand
/// back to Starlark so the call log and replay cache see the same shape.
struct ShellOutcome {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

/// Enforce the `CHIDORI_SHELL_ALLOW` whitelist for `shell()`.
///
/// Default: empty → every command is refused. Users opt in by setting
/// `CHIDORI_SHELL_ALLOW` to a comma-separated list (e.g. `"ls,cat,git"`)
/// or to `*` to allow anything. The allow list matches on the *bare*
/// command name; it does not do path-based or regex matching.
fn check_shell_allowed(command: &str) -> anyhow::Result<()> {
    let allow = std::env::var("CHIDORI_SHELL_ALLOW").unwrap_or_default();
    let trimmed = allow.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!(
            "shell() is disabled: set CHIDORI_SHELL_ALLOW to a comma-separated \
             allow list (e.g. \"ls,cat,git\") to enable specific commands"
        ));
    }
    if trimmed == "*" {
        return Ok(());
    }
    // Match on the basename only — if a caller passes `/usr/bin/ls`, we
    // still check against `ls`. Keeps PATH confusion from becoming a
    // bypass vector.
    let basename = std::path::Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    let allowed: Vec<&str> = trimmed.split(',').map(str::trim).collect();
    if allowed.iter().any(|a| *a == basename) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "shell() command `{}` is not in CHIDORI_SHELL_ALLOW ({})",
            command,
            trimmed
        ))
    }
}

/// Build a ToolSchema for the LLM from a registered ToolDef.
fn tool_def_to_schema(def: &ToolDef) -> ToolSchema {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in &def.params {
        let mut prop = serde_json::Map::new();
        prop.insert("type".to_string(), Value::String(param.param_type.clone()));
        if let Some(ref desc) = param.description {
            prop.insert("description".to_string(), Value::String(desc.clone()));
        }
        properties.insert(param.name.clone(), Value::Object(prop));
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }
    ToolSchema {
        name: def.name.clone(),
        description: def.description.clone(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

/// Serialize an LlmResponse into JSON for the call log so it can be replayed.
fn llm_response_to_json(resp: &LlmResponse) -> Value {
    json!({
        "content": resp.content,
        "blocks": resp.blocks,
        "tool_calls": resp.tool_calls.iter().map(|c| json!({
            "id": c.id,
            "name": c.name,
            "input": c.input,
        })).collect::<Vec<_>>(),
        "stop_reason": resp.stop_reason,
        "input_tokens": resp.input_tokens,
        "output_tokens": resp.output_tokens,
    })
}

/// Reconstruct an LlmResponse from a cached call-log record (tool-use turn).
fn llm_response_from_record(record: &CallRecord) -> Option<LlmResponse> {
    let obj = record.result.as_object()?;
    let content = obj.get("content")?.as_str().unwrap_or("").to_string();
    let blocks: Vec<ContentBlock> = obj
        .get("blocks")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let tool_calls: Vec<ToolCall> = obj
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(ToolCall {
                        id: c.get("id")?.as_str()?.to_string(),
                        name: c.get("name")?.as_str()?.to_string(),
                        input: c.get("input").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(LlmResponse {
        content,
        blocks,
        tool_calls,
        stop_reason: obj
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn")
            .to_string(),
        input_tokens: obj.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    })
}

/// Invoke a tool the LLM asked for and return a tool_result ContentBlock.
/// Records the invocation in the call log so it is replayable.
fn invoke_tool_call(
    call: &ToolCall,
    tool_defs: &[ToolDef],
    host: HostState,
) -> ContentBlock {
    let seq = host.ctx.next_seq();

    // This helper is called from within the tool-use loop of prompt(), which
    // already returns starlark::Result — but `invoke_tool_call` itself returns
    // a ContentBlock. On divergence, surface the error as a tool_result error
    // block so the next LLM turn can see it; a strict caller could choose to
    // promote this to a hard failure later.
    match host.ctx.try_replay_checked(seq, "tool") {
        Ok(Some(cached)) => {
            let content = cached.result.as_str().map(|s| s.to_string()).unwrap_or_else(
                || cached.result.to_string(),
            );
            return ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content,
                is_error: cached.error.is_some(),
            };
        }
        Ok(None) => {}
        Err(msg) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: msg,
                is_error: true,
            };
        }
    }

    let def = match tool_defs.iter().find(|d| d.name == call.name) {
        Some(d) => d.clone(),
        None => {
            let err = format!("Tool not available: {}", call.name);
            host.ctx.record_call(CallRecord {
                seq,
                function: "tool".to_string(),
                args: json!({"name": call.name, "kwargs": call.input}),
                result: Value::Null,
                duration_ms: 0,
                token_usage: None,
                timestamp: Utc::now(),
                error: Some(err.clone()),
            });
            return ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: err,
                is_error: true,
            };
        }
    };

    let args_map: serde_json::Map<String, Value> = match &call.input {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };

    let start = Instant::now();
    let result = invoke_tool_starlark(
        &def.source_path.display().to_string(),
        &def.source,
        &def.name,
        &args_map,
        HostState {
            ctx: host.ctx.clone(),
            providers: host.providers.clone(),
            template_engine: host.template_engine.clone(),
            tokio_rt: host.tokio_rt.clone(),
            tools: host.tools.clone(),
            policy: host.policy.clone(),
            policy_cache: host.policy_cache.clone(),
            mcp: host.mcp.clone(),
        },
    );
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(val) => {
            let content_str = val
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| val.to_string());
            host.ctx.record_call(CallRecord {
                seq,
                function: "tool".to_string(),
                args: json!({"name": call.name, "kwargs": args_map}),
                result: Value::String(content_str.clone()),
                duration_ms,
                token_usage: None,
                timestamp: Utc::now(),
                error: None,
            });
            ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: content_str,
                is_error: false,
            }
        }
        Err(e) => {
            let err = e.to_string();
            host.ctx.record_call(CallRecord {
                seq,
                function: "tool".to_string(),
                args: json!({"name": call.name, "kwargs": args_map}),
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: Utc::now(),
                error: Some(err.clone()),
            });
            ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: err,
                is_error: true,
            }
        }
    }
}

/// Execute a memory() action against the JSON-file backend.
/// Storage layout: `.chidori/memory/<namespace>.json` containing a flat
/// object `{ key: value, ... }`. One file per namespace keeps actions atomic
/// (each call reads + writes the whole file) and keeps the backend dependency-
/// free. A SQLite backend is a later upgrade.
fn memory_execute(
    action: &str,
    namespace: &str,
    key: Option<&str>,
    value: Option<&Value>,
    prefix: &str,
) -> anyhow::Result<Value> {
    let dir = std::path::PathBuf::from(".chidori").join("memory");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", sanitize_namespace(namespace)));

    let load = || -> anyhow::Result<serde_json::Map<String, Value>> {
        if !file.exists() {
            return Ok(serde_json::Map::new());
        }
        let text = std::fs::read_to_string(&file)?;
        if text.trim().is_empty() {
            return Ok(serde_json::Map::new());
        }
        match serde_json::from_str::<Value>(&text)? {
            Value::Object(m) => Ok(m),
            _ => Ok(serde_json::Map::new()),
        }
    };

    let save = |map: &serde_json::Map<String, Value>| -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
        std::fs::write(&file, text)?;
        Ok(())
    };

    match action {
        "get" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("memory(\"get\") requires key="))?;
            let map = load()?;
            Ok(map.get(key).cloned().unwrap_or(Value::Null))
        }
        "set" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("memory(\"set\") requires key="))?;
            let value = value
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("memory(\"set\") requires value="))?;
            let mut map = load()?;
            map.insert(key.to_string(), value);
            save(&map)?;
            Ok(Value::Null)
        }
        "delete" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("memory(\"delete\") requires key="))?;
            let mut map = load()?;
            let existed = map.remove(key).is_some();
            save(&map)?;
            Ok(Value::Bool(existed))
        }
        "list" => {
            let map = load()?;
            let items: Vec<Value> = map
                .into_iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| json!({ "key": k, "value": v }))
                .collect();
            Ok(Value::Array(items))
        }
        "clear" => {
            save(&serde_json::Map::new())?;
            Ok(Value::Null)
        }
        other => Err(anyhow::anyhow!(
            "Unknown memory() action: {}. Expected get | set | delete | list | clear",
            other
        )),
    }
}

/// Keep namespace strings safe for use as a filename.
fn sanitize_namespace(ns: &str) -> String {
    ns.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Try to extract a JSON string from LLM output that may be wrapped in markdown code fences.
fn extract_json(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.starts_with("```json") {
        if let Some(end) = trimmed.rfind("```") {
            let start = trimmed.find('\n').unwrap_or(7) + 1;
            if start < end {
                return trimmed[start..end].trim().to_string();
            }
        }
    }
    if trimmed.starts_with("```") {
        if let Some(end) = trimmed[3..].rfind("```") {
            let start = trimmed.find('\n').unwrap_or(3) + 1;
            let end = end + 3;
            if start < end {
                return trimmed[start..end].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_plain() {
        assert_eq!(extract_json(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn test_extract_json_fenced() {
        let input = "```json\n{\"a\": 1}\n```";
        assert_eq!(extract_json(input), r#"{"a": 1}"#);
    }

    // Serialize shell-whitelist tests so concurrent test threads don't
    // stomp on each other's CHIDORI_SHELL_ALLOW env value.
    fn with_env<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn test_shell_whitelist_default_closed() {
        with_env("CHIDORI_SHELL_ALLOW", None, || {
            let err = check_shell_allowed("echo").unwrap_err();
            assert!(err.to_string().contains("disabled"));
        });
    }

    #[test]
    fn test_shell_whitelist_specific() {
        with_env("CHIDORI_SHELL_ALLOW", Some("ls, cat , echo"), || {
            assert!(check_shell_allowed("echo").is_ok());
            assert!(check_shell_allowed("cat").is_ok());
            // basename match: full path still resolves to `ls`.
            assert!(check_shell_allowed("/usr/bin/ls").is_ok());
            // not in the list:
            assert!(check_shell_allowed("rm").is_err());
        });
    }

    #[test]
    fn test_shell_whitelist_star() {
        with_env("CHIDORI_SHELL_ALLOW", Some("*"), || {
            assert!(check_shell_allowed("rm").is_ok());
            assert!(check_shell_allowed("arbitrary-command").is_ok());
        });
    }
}
