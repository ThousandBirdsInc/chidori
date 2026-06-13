use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mcp::McpManager;
use crate::policy::{Decision, PolicyCache, PolicyConfig};
use crate::providers::{
    CacheLayout, CacheTtl, ContentBlock, LlmRequest, Message as LlmMessage, ProviderRegistry,
    ToolSchema,
};
use crate::runtime::call_log::CallRecord;
use crate::runtime::context::{InputMode, PendingApproval, RuntimeContext, PAUSE_MARKER};
use crate::runtime::host_core;
use crate::runtime::snapshot::RuntimePolicy;
use crate::runtime::template::TemplateEngine;
/// A recorded host effect call (function name + JSON args). Used by the
/// recorder backend to capture the sequence of `chidori.*` effects an agent
/// performs, for tests and tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBindingCall {
    pub function: String,
    pub args: serde_json::Value,
}
use crate::tools::{ToolBackend, ToolDef, ToolRegistry};

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) enum HostBindingBackend {
    Recorder(HostBindingRecorder),
    Runtime {
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<PolicyConfig>,
        policy_cache: Arc<StdMutex<PolicyCache>>,
        runtime_policy: RuntimePolicy,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    },
}

impl HostBindingBackend {
    fn durable_call(
        &self,
        function: &str,
        args: serde_json::Value,
        live: impl FnOnce() -> std::result::Result<serde_json::Value, String>,
    ) -> std::result::Result<Option<serde_json::Value>, String> {
        match self {
            HostBindingBackend::Recorder(recorder) => {
                let result = live()?;
                recorder.push(function, args);
                Ok(Some(result))
            }
            HostBindingBackend::Runtime { runtime_ctx, .. } => {
                host_core::execute_durable_json_call(runtime_ctx, function, args, || {
                    live().map_err(|err| anyhow::anyhow!(err))
                })
                .map(Some)
                .map_err(|err| err.to_string())
            }
        }
    }

    fn template_engine(&self) -> Arc<TemplateEngine> {
        match self {
            HostBindingBackend::Recorder(_) => Arc::new(TemplateEngine::new(".")),
            HostBindingBackend::Runtime {
                template_engine, ..
            } => template_engine.clone(),
        }
    }

    fn workspace_root(&self) -> Option<PathBuf> {
        match self {
            HostBindingBackend::Recorder(_) => std::env::var_os("CHIDORI_WORKSPACE_ROOT")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            HostBindingBackend::Runtime { runtime_ctx, .. } => runtime_ctx.workspace_root(),
        }
    }

    fn workspace_call(
        &self,
        action: &str,
        args: serde_json::Value,
        live: impl FnOnce() -> std::result::Result<serde_json::Value, String>,
    ) -> std::result::Result<serde_json::Value, String> {
        // Route every workspace effect through the policy gate before it runs,
        // the same way `http` and `tool:` calls are gated. With the default
        // AlwaysAllow policy this is a no-op; a restrictive profile can now
        // deny or gate `workspace:write` / `workspace:delete` (or any action)
        // by target. Enforcing before recording means a denied or paused call
        // never lands in the journal.
        self.enforce_policy(&format!("workspace:{action}"), &args)?;
        let call_args = serde_json::json!({
            "action": action,
            "args": args,
        });
        match self {
            HostBindingBackend::Recorder(recorder) => {
                let result = live()?;
                recorder.push("workspace", call_args);
                Ok(result)
            }
            HostBindingBackend::Runtime { runtime_ctx, .. } => {
                let seq = runtime_ctx.next_seq();
                let started = chrono::Utc::now();
                let result = live();
                let duration_ms = chrono::Utc::now()
                    .signed_duration_since(started)
                    .num_milliseconds()
                    .max(0) as u64;
                match result {
                    Ok(result) => {
                        runtime_ctx.record_call(CallRecord {
                            seq,
                            parent_seq: None,
                            function: "workspace".to_string(),
                            args: call_args,
                            result: result.clone(),
                            duration_ms,
                            token_usage: None,
                            timestamp: started,
                            error: None,
                        });
                        Ok(result)
                    }
                    Err(err) => {
                        runtime_ctx.record_call(CallRecord {
                            seq,
                            parent_seq: None,
                            function: "workspace".to_string(),
                            args: call_args,
                            result: serde_json::Value::Null,
                            duration_ms,
                            token_usage: None,
                            timestamp: started,
                            error: Some(err.clone()),
                        });
                        Err(err)
                    }
                }
            }
        }
    }

    fn prompt(
        &self,
        text: String,
        options: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime {
            runtime_ctx,
            providers,
            tokio_rt,
            tools,
            ..
        } = self
        else {
            return Err("chidori.prompt requires the runtime host backend".to_string());
        };

        let config = runtime_ctx.config();
        let model = options
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&config.model)
            .to_string();
        let temperature = options
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(config.temperature);
        let max_tokens = options
            .get("maxTokens")
            .or_else(|| options.get("max_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(config.max_tokens);
        let max_turns = options
            .get("maxTurns")
            .or_else(|| options.get("max_turns"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(config.max_turns)
            .max(1);
        let system = options
            .get("system")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let format = options
            .get("format")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let prompt_type = options
            .get("type")
            .or_else(|| options.get("streamType"))
            .or_else(|| options.get("stream_type"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let tool_names: Vec<String> = options
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let posture = host_core::cache_posture_from_options(&options);

        // `chidori.context(...).prompt()/.respond()` forwards its flattened
        // segment chain in `__context`; everything below the seed differs from
        // the single-text path, so branch early.
        if let Some(segments) = options
            .get("__context")
            .and_then(serde_json::Value::as_array)
        {
            let respond = options
                .get("__mode")
                .and_then(serde_json::Value::as_str)
                .map(|mode| mode == "respond")
                .unwrap_or(false);
            let parts = context_request_parts(segments)?;
            let mut all_tool_names = parts.tool_names.clone();
            for name in &tool_names {
                if !all_tool_names.contains(name) {
                    all_tool_names.push(name.clone());
                }
            }
            let mut tool_schemas = Vec::new();
            for name in &all_tool_names {
                let tool_def = tools
                    .get(name)
                    .ok_or_else(|| format!("Unknown tool in context tools: {name}"))?;
                tool_schemas.push(tool_def_to_schema(tool_def));
            }
            let system = match (parts.system, system) {
                (Some(ctx_system), Some(opt_system)) => {
                    Some(format!("{ctx_system}\n\n{opt_system}"))
                }
                (Some(ctx_system), None) => Some(ctx_system),
                (None, opt_system) => opt_system,
            };
            let mut messages = parts.messages;
            if messages.is_empty() {
                return Err(
                    "chidori.context prompt requires at least one user turn (add .user(...))"
                        .to_string(),
                );
            }
            let seed_len = messages.len();
            let build_request = |messages: &[LlmMessage]| {
                let mut request = LlmRequest {
                    model: model.clone(),
                    messages: messages.to_vec(),
                    system: system.clone(),
                    temperature,
                    max_tokens,
                    tools: tool_schemas.clone(),
                    cache: parts.cache.clone(),
                };
                host_core::auto_mark_prompt_cache(&mut request, posture);
                request
            };
            let call_args = |request: &LlmRequest, turn: Option<u64>| {
                serde_json::json!({
                    "model": model,
                    "type": prompt_type,
                    "tools": all_tool_names,
                    "turn": turn,
                    "context_segments": segments.len(),
                    "request_digest": host_core::prompt_request_digest(request),
                })
            };

            if respond {
                // Single structured turn: the author drives any tool loop
                // explicitly by appending toolResult segments.
                let request = build_request(&messages);
                let args = call_args(&request, None);
                let response = host_core::execute_prompt_response(
                    runtime_ctx,
                    providers,
                    tokio_rt,
                    request,
                    args,
                    prompt_type.clone(),
                )
                .map_err(|err| err.to_string())?;
                let new_messages = vec![LlmMessage::assistant_blocks(response.blocks.clone())];
                return Ok(serde_json::json!({
                    "text": response.content,
                    "response": host_core::llm_response_to_json(&response),
                    "messages": new_messages,
                }));
            }

            if tool_schemas.is_empty() {
                let request = build_request(&messages);
                let args = call_args(&request, None);
                let result = host_core::execute_prompt_text(
                    runtime_ctx,
                    providers,
                    tokio_rt,
                    request,
                    args,
                    prompt_type.clone(),
                )
                .map_err(|err| err.to_string())?;
                let text = result.as_str().unwrap_or("").to_string();
                let new_messages = vec![LlmMessage::assistant_blocks(vec![ContentBlock::Text {
                    text: text.clone(),
                }])];
                return Ok(serde_json::json!({
                    "text": text,
                    "messages": new_messages,
                }));
            }

            // Tool loop seeded from the context: identical machinery to the
            // single-text loop, but every appended turn is returned so the JS
            // side can extend the context with the full exchange.
            let mut final_text = String::new();
            for turn in 0..max_turns {
                let request = build_request(&messages);
                let args = call_args(&request, Some(turn));
                let response = host_core::execute_prompt_response(
                    runtime_ctx,
                    providers,
                    tokio_rt,
                    request,
                    args,
                    prompt_type.clone(),
                )
                .map_err(|err| err.to_string())?;
                final_text = response.content.clone();
                messages.push(LlmMessage::assistant_blocks(response.blocks.clone()));
                if response.tool_calls.is_empty() {
                    break;
                }
                let result_blocks = self.run_tool_calls(response.tool_calls)?;
                messages.push(LlmMessage {
                    role: "user".to_string(),
                    content: result_blocks,
                    cache_control: None,
                });
            }
            let new_messages = messages.split_off(seed_len);
            return Ok(serde_json::json!({
                "text": final_text,
                "messages": new_messages,
            }));
        }

        let mut tool_schemas = Vec::new();
        for name in &tool_names {
            let tool_def = tools
                .get(name)
                .ok_or_else(|| format!("Unknown tool in prompt tools: {name}"))?;
            tool_schemas.push(tool_def_to_schema(tool_def));
        }

        if !tool_schemas.is_empty() {
            let mut messages = vec![LlmMessage::user_text(text.clone())];
            let mut final_text = String::new();
            for turn in 0..max_turns {
                let mut request = LlmRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    system: system.clone(),
                    temperature,
                    max_tokens,
                    tools: tool_schemas.clone(),
                    cache: CacheLayout::default(),
                };
                host_core::auto_mark_prompt_cache(&mut request, posture);
                let request_digest = host_core::prompt_request_digest(&request);
                let response = host_core::execute_prompt_response(
                    runtime_ctx,
                    providers,
                    tokio_rt,
                    request,
                    serde_json::json!({
                        "text": text,
                        "model": model,
                        "type": prompt_type,
                        "tools": tool_names,
                        "turn": turn,
                        "max_turns": max_turns,
                        "request_digest": request_digest,
                    }),
                    prompt_type.clone(),
                )
                .map_err(|err| err.to_string())?;

                final_text = response.content.clone();
                if response.tool_calls.is_empty() {
                    break;
                }
                messages.push(LlmMessage::assistant_blocks(response.blocks.clone()));
                let result_blocks = self.run_tool_calls(response.tool_calls)?;
                messages.push(LlmMessage {
                    role: "user".to_string(),
                    content: result_blocks,
                    cache_control: None,
                });
            }
            if format.as_deref() == Some("json") {
                return serde_json::from_str::<serde_json::Value>(&final_text)
                    .or(Ok(serde_json::Value::String(final_text)));
            }
            return Ok(serde_json::Value::String(final_text));
        }

        let mut request = LlmRequest {
            model: model.clone(),
            messages: vec![LlmMessage::user_text(text.clone())],
            system: system.clone(),
            temperature,
            max_tokens,
            tools: Vec::new(),
            cache: CacheLayout::default(),
        };
        host_core::auto_mark_prompt_cache(&mut request, posture);
        let request_digest = host_core::prompt_request_digest(&request);
        let result = host_core::execute_prompt_text(
            runtime_ctx,
            providers,
            tokio_rt,
            request,
            serde_json::json!({
                "text": text,
                "model": model,
                "type": prompt_type,
                "request_digest": request_digest,
            }),
            prompt_type.clone(),
        )
        .map_err(|err| err.to_string())?;

        if format.as_deref() == Some("json") {
            if let Some(content) = result.as_str() {
                serde_json::from_str::<serde_json::Value>(content)
                    .or(Ok(serde_json::Value::String(content.to_string())))
            } else {
                Ok(result)
            }
        } else {
            Ok(result)
        }
    }

    /// Execute the tool calls from one assistant turn and frame each result as
    /// a `tool_result` block. A pause inside a tool propagates; other errors
    /// land in the block as `is_error` so the model can react.
    fn run_tool_calls(
        &self,
        calls: Vec<crate::providers::ToolCall>,
    ) -> std::result::Result<Vec<ContentBlock>, String> {
        let mut result_blocks = Vec::new();
        for call in calls {
            match self.tool(call.name.clone(), call.input.clone()) {
                Ok(value) => result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content: serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()),
                    is_error: false,
                }),
                Err(err) if err.contains(PAUSE_MARKER) => return Err(err),
                Err(err) => result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id,
                    content: err,
                    is_error: true,
                }),
            }
        }
        Ok(result_blocks)
    }

    /// Pure digest of the request a context would assemble — backs the JS-side
    /// `Context.digest()`. Deterministic in its inputs, so it is dispatched
    /// directly and never recorded in the call log.
    fn context_digest(
        &self,
        segments: &[serde_json::Value],
        opts: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime {
            runtime_ctx, tools, ..
        } = self
        else {
            return Err("chidori.context requires the runtime host backend".to_string());
        };
        let config = runtime_ctx.config();
        let parts = context_request_parts(segments)?;
        let mut tool_schemas = Vec::new();
        for name in &parts.tool_names {
            let tool_def = tools
                .get(name)
                .ok_or_else(|| format!("Unknown tool in context tools: {name}"))?;
            tool_schemas.push(tool_def_to_schema(tool_def));
        }
        let mut request = LlmRequest {
            model: opts
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&config.model)
                .to_string(),
            messages: parts.messages,
            system: parts.system,
            temperature: opts
                .get("temperature")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(config.temperature),
            max_tokens: opts
                .get("maxTokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(config.max_tokens),
            tools: tool_schemas,
            cache: parts.cache,
        };
        host_core::auto_mark_prompt_cache(
            &mut request,
            host_core::cache_posture_from_options(opts),
        );
        Ok(serde_json::Value::String(host_core::prompt_request_digest(
            &request,
        )))
    }

    fn input(&self, prompt: String) -> std::result::Result<String, String> {
        let HostBindingBackend::Runtime { runtime_ctx, .. } = self else {
            return Err("chidori.input requires the runtime host backend".to_string());
        };

        let result =
            host_core::execute_input(runtime_ctx, &serde_json::json!({ "prompt": prompt }))
                .map_err(|err| err.to_string())?;
        Ok(result.as_str().unwrap_or("").to_string())
    }

    fn signal(&self, a: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime { runtime_ctx, .. } = self else {
            return Err("chidori.signal requires the runtime host backend".to_string());
        };
        host_core::execute_signal(runtime_ctx, a).map_err(|err| err.to_string())
    }

    fn poll_signal(&self, a: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime { runtime_ctx, .. } = self else {
            return Err("chidori.pollSignal requires the runtime host backend".to_string());
        };
        host_core::execute_poll_signal(runtime_ctx, a).map_err(|err| err.to_string())
    }

    fn signal_any(&self, a: &serde_json::Value) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime { runtime_ctx, .. } = self else {
            return Err("chidori.signalAny requires the runtime host backend".to_string());
        };
        host_core::execute_signal_any(runtime_ctx, a).map_err(|err| err.to_string())
    }

    fn enforce_policy(
        &self,
        target: &str,
        args: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        let HostBindingBackend::Runtime {
            runtime_ctx,
            policy,
            policy_cache,
            ..
        } = self
        else {
            return Ok(());
        };

        let (decision, reason) = policy.decide(target, args);
        match decision {
            Decision::AlwaysAllow => Ok(()),
            Decision::NeverAllow => Err(format!(
                "policy: `{}` denied{}",
                target,
                reason.map(|r| format!(" ({})", r)).unwrap_or_default()
            )),
            Decision::AskBefore => {
                {
                    let cache = policy_cache.lock().unwrap();
                    if cache.is_approved(target, args) {
                        return Ok(());
                    }
                }
                if std::env::var("CHIDORI_POLICY_AUTO_APPROVE").ok().as_deref() == Some("1") {
                    policy_cache.lock().unwrap().approve(target, args);
                    return Ok(());
                }
                if runtime_ctx.input_mode() == InputMode::Pause {
                    runtime_ctx.set_pending_approval(PendingApproval {
                        target: target.to_string(),
                        args: args.clone(),
                        reason: reason.clone(),
                    });
                    return Err(PAUSE_MARKER.to_string());
                }
                Err(format!(
                    "policy: `{}` requires approval{}. Set CHIDORI_POLICY_AUTO_APPROVE=1 to \
                     auto-approve, or run through the server so the approval flow can pause.",
                    target,
                    reason.map(|r| format!(" - {}", r)).unwrap_or_default()
                ))
            }
        }
    }

    fn tool(
        &self,
        name: String,
        kwargs: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime {
            runtime_ctx,
            tokio_rt,
            tools,
            mcp,
            ..
        } = self
        else {
            return Err("chidori.tool requires the runtime host backend".to_string());
        };
        let kwargs = match kwargs {
            serde_json::Value::Object(map) => map,
            serde_json::Value::Null => serde_json::Map::new(),
            other => {
                return Err(format!(
                    "chidori.tool args must be an object, got {}",
                    other
                ));
            }
        };
        let args = serde_json::json!({
            "name": name,
            "kwargs": kwargs,
        });

        host_core::execute_tool_call(
            runtime_ctx,
            args.get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            serde_json::Value::Object(kwargs.clone()),
            || {
                let tool_name = args
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let kwargs = args
                    .get("kwargs")
                    .and_then(serde_json::Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let tool_def = tools
                    .get(tool_name)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!(tools.describe_miss(tool_name)))?;
                self.enforce_policy(
                    &format!("tool:{tool_name}"),
                    &serde_json::Value::Object(kwargs.clone()),
                )
                .map_err(|err| anyhow::anyhow!(err))?;

                match &tool_def.backend {
                    ToolBackend::Mcp {
                        server_id,
                        remote_name,
                    } => tokio_rt.block_on(async {
                        mcp.call_tool(&server_id, &remote_name, &serde_json::Value::Object(kwargs))
                            .await
                    }),
                    ToolBackend::TypeScript => {
                        // Nested execution: run the nested TS tool on the rust
                        // engine, threading the same backend (`self`) so host
                        // effects nest under this tool call and a suspension
                        // propagates.
                        crate::runtime::rust_engine::run_tool_file(
                            &tool_def.source_path,
                            &serde_json::Value::Object(kwargs),
                            self,
                        )
                    }
                    ToolBackend::Native => {
                        tools.dispatch_native(tool_name, serde_json::Value::Object(kwargs))
                    }
                }
            },
        )
        .map_err(|err| err.to_string())
    }

    fn call_agent(
        &self,
        path: String,
        input: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime { runtime_ctx, .. } = self else {
            return Err("chidori.callAgent requires the runtime host backend".to_string());
        };
        let args = serde_json::json!({
            "path": path,
            "input": input,
        });
        host_core::execute_call_agent(runtime_ctx, args.clone(), || {
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let input = args.get("input").unwrap_or(&serde_json::Value::Null);
            match Path::new(path).extension().and_then(|ext| ext.to_str()) {
                Some("ts") => {
                    // Nested execution: run the sub-agent on the rust engine,
                    // sharing this backend so the child's host effects nest
                    // under the callAgent and a suspension propagates to the
                    // parent run.
                    crate::runtime::rust_engine::run_agent_file(Path::new(path), input, self)
                }
                _ => Err(anyhow::anyhow!("chidori.callAgent supports .ts agents")),
            }
        })
        .map_err(|err| err.to_string())
    }

    fn block_on_http(
        &self,
        args: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let HostBindingBackend::Runtime { tokio_rt, .. } = self else {
            return Err("network requests require the runtime host backend".to_string());
        };

        host_core::execute_http(tokio_rt, args).map_err(|err| err.to_string())
    }

    /// Build the runtime backend. Shared by the QuickJS bindings and the
    /// pure-Rust engine's effect dispatcher so both route host effects through
    /// identical durable machinery.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_runtime(
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<PolicyConfig>,
        policy_cache: Arc<StdMutex<PolicyCache>>,
        runtime_policy: RuntimePolicy,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    ) -> Self {
        HostBindingBackend::Runtime {
            runtime_ctx,
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            runtime_policy,
            tools,
            mcp,
        }
    }

    /// Clone this runtime backend with a different [`RuntimeContext`] — same
    /// providers, policy (and approval cache), tools, and MCP. Used by
    /// `chidori.branch` to run each branch sub-run on its own context while
    /// enforcing the parent's policy. `None` for the recorder backend.
    pub(crate) fn with_runtime_ctx(&self, runtime_ctx: RuntimeContext) -> Option<Self> {
        match self {
            HostBindingBackend::Runtime {
                providers,
                template_engine,
                tokio_rt,
                policy,
                policy_cache,
                runtime_policy,
                tools,
                mcp,
                ..
            } => Some(HostBindingBackend::Runtime {
                runtime_ctx,
                providers: providers.clone(),
                template_engine: template_engine.clone(),
                tokio_rt: tokio_rt.clone(),
                policy: policy.clone(),
                policy_cache: policy_cache.clone(),
                runtime_policy: runtime_policy.clone(),
                tools: tools.clone(),
                mcp: mcp.clone(),
            }),
            HostBindingBackend::Recorder(_) => None,
        }
    }

    /// The runtime context, when this is the runtime backend.
    pub(crate) fn runtime_ctx(&self) -> Option<&RuntimeContext> {
        match self {
            HostBindingBackend::Runtime { runtime_ctx, .. } => Some(runtime_ctx),
            HostBindingBackend::Recorder(_) => None,
        }
    }

    /// The durable runtime policy, when this is the runtime backend. The rust
    /// engine reads it to install the captured-effect natives (`node:` crypto /
    /// fs / timers) under the same fs/crypto/timer gates the QuickJS path honors.
    pub(crate) fn runtime_policy(&self) -> Option<crate::runtime::snapshot::RuntimePolicy> {
        match self {
            HostBindingBackend::Runtime { runtime_policy, .. } => Some(runtime_policy.clone()),
            HostBindingBackend::Recorder(_) => None,
        }
    }

    /// Route one `chidori.<effect>(args)` call from the pure-Rust engine through
    /// the same durable host machinery the QuickJS bindings use. The arg shapes
    /// mirror what `chidori-js`'s `install_chidori_effects` forwards, so the two
    /// engines produce identical call logs.
    pub(crate) fn dispatch(
        &self,
        effect: &str,
        a: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        // A live `chidori.step` callback must be pure compute: skipping it on
        // replay skips everything it did, so any effect it performed would be
        // lost (state) or desynchronize the journal (records). Refuse loudly.
        // `step_begin`/`step_end` are the step protocol itself and
        // `contextDigest` is a pure inline hash that records nothing.
        if !matches!(effect, "step_begin" | "step_end" | "contextDigest") {
            if let Some(step) = self.runtime_ctx().and_then(|ctx| ctx.active_step_name()) {
                return Err(format!(
                    "chidori.{effect} is not allowed inside chidori.step(\"{step}\"): \
                     step callbacks must be pure, synchronous computation \
                     (run host effects outside the step and pass their results in)"
                ));
            }
        }
        let opt_null = |v: Option<serde_json::Value>| v.unwrap_or(serde_json::Value::Null);
        match effect {
            "log" => {
                let message = a.get("message").cloned().unwrap_or(serde_json::Value::Null);
                let args = serde_json::json!({ "message": message });
                self.durable_call("log", args.clone(), || {
                    host_core::execute_log(&args).map_err(|err| err.to_string())
                })
                .map(opt_null)
            }
            "input" => {
                let prompt = a
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.input(prompt).map(serde_json::Value::String)
            }
            "signal" => self.signal(a),
            "poll_signal" => self.poll_signal(a),
            "signal_any" => self.signal_any(a),
            // The two halves of `chidori.step(name, fn)` — the durable value
            // checkpoint (docs/value-checkpoints.md). The engine binding probes
            // for a recorded result, runs the callback only on a miss, then
            // reports the outcome; one `step` CallRecord results either way.
            "step_begin" => match self {
                HostBindingBackend::Runtime { runtime_ctx, .. } => {
                    host_core::execute_step_begin(runtime_ctx, a).map_err(|err| err.to_string())
                }
                // The recorder has no replay log, so the callback always runs.
                HostBindingBackend::Recorder(_) => Ok(serde_json::json!({ "cached": false })),
            },
            "step_end" => match self {
                HostBindingBackend::Runtime { runtime_ctx, .. } => {
                    host_core::execute_step_end(runtime_ctx, a).map_err(|err| err.to_string())
                }
                HostBindingBackend::Recorder(recorder) => {
                    let name = a.get("name").cloned().unwrap_or(serde_json::Value::Null);
                    recorder.push("step", serde_json::json!({ "name": name }));
                    Ok(a.get("value").cloned().unwrap_or(serde_json::Value::Null))
                }
            },
            "checkpoint" => {
                let args = serde_json::json!({
                    "label": a.get("label").cloned().unwrap_or(serde_json::Value::Null),
                    "data": a.get("data").cloned().unwrap_or(serde_json::Value::Null),
                });
                self.durable_call("checkpoint", args, || Ok(serde_json::Value::Null))
                    .map(opt_null)
            }
            "prompt" => {
                let text = a
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let options = a
                    .get("opts")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .unwrap_or_else(|| serde_json::json!({}));
                self.prompt(text, options)
            }
            "tool" => {
                let name = a
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let kwargs = a
                    .get("kwargs")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .unwrap_or_else(|| serde_json::json!({}));
                self.tool(name, kwargs)
            }
            "memory" => {
                let action = a
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let key = a.get("key").cloned().filter(|v| !v.is_null());
                let value = a.get("value").cloned().filter(|v| !v.is_null());
                let options = a.get("opts").cloned().unwrap_or(serde_json::Value::Null);
                let namespace = options
                    .get("namespace")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("default");
                let prefix = options
                    .get("prefix")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let args = serde_json::json!({
                    "action": action,
                    "key": key,
                    "namespace": namespace,
                    "prefix": prefix,
                    "value": value,
                });
                self.durable_call("memory", args.clone(), || {
                    host_core::execute_memory(&args).map_err(|err| err.to_string())
                })
                .map(opt_null)
            }
            "template" => {
                let template = a
                    .get("template")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let vars = a
                    .get("vars")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .unwrap_or_else(|| serde_json::json!({}));
                let args = serde_json::json!({ "template": template, "vars": vars });
                let template_engine = self.template_engine();
                self.durable_call("template", args.clone(), || {
                    host_core::execute_template(&template_engine, &args)
                        .map_err(|err| err.to_string())
                })
                .map(opt_null)
            }
            "http" => {
                let first = a.get("arg0").cloned().unwrap_or(serde_json::Value::Null);
                let second = a.get("arg1").cloned().unwrap_or(serde_json::Value::Null);
                let (url, options) = if let Some(url) = first.as_str() {
                    let options = if second.is_null() {
                        serde_json::json!({})
                    } else {
                        second
                    };
                    (url.to_string(), options)
                } else if let serde_json::Value::Object(ref map) = first {
                    let url = map
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            "http request options must include a string url".to_string()
                        })?
                        .to_string();
                    (url, first.clone())
                } else {
                    return Err(
                        "http request requires a URL string or options object".to_string()
                    );
                };
                let mut method = options
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("GET")
                    .to_uppercase();
                if method.is_empty() {
                    method = "GET".to_string();
                }
                let headers = options.get("headers").and_then(|v| match v {
                    serde_json::Value::Object(m) => Some(m.clone()),
                    _ => None,
                });
                let body = options.get("body").cloned();
                let params = options
                    .get("params")
                    .or_else(|| options.get("query"))
                    .and_then(|v| match v {
                        serde_json::Value::Object(m) => Some(m.clone()),
                        _ => None,
                    });
                let args = serde_json::json!({
                    "url": url,
                    "method": method,
                    "headers": headers,
                    "body": body,
                    "params": params,
                });
                self.durable_call("http", args.clone(), || {
                    self.enforce_policy(
                        "http",
                        &serde_json::json!({
                            "url": args.get("url").cloned().unwrap_or_default(),
                            "method": args.get("method").cloned().unwrap_or_default(),
                        }),
                    )?;
                    self.block_on_http(&args)
                })
                .map(opt_null)
            }
            "callAgent" => {
                let path = a
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = a
                    .get("input")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .unwrap_or_else(|| serde_json::json!({}));
                self.call_agent(path, input)
            }
            "branch" => crate::runtime::host_branch::run_branches(self, a),
            "workspace" => self.dispatch_workspace(a),
            "contextDigest" => {
                let segments = a
                    .get("segments")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let opts = a.get("opts").cloned().unwrap_or(serde_json::Value::Null);
                self.context_digest(&segments, &opts)
            }
            other => Err(format!(
                "chidori.{other} is not supported on the rust engine"
            )),
        }
    }

    /// Sub-dispatch for `chidori.workspace.<action>(args)`.
    fn dispatch_workspace(
        &self,
        a: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let action = a
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let args = a.get("args").cloned().unwrap_or(serde_json::Value::Null);
        match action {
            "list" => {
                let complete_only = args
                    .get("completeOnly")
                    .or_else(|| args.get("complete_only"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                self.workspace_call("list", args.clone(), || {
                    let root = workspace_root(self)?;
                    workspace_list(&root, complete_only)
                })
            }
            "read" => {
                let path = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.workspace_call("read", serde_json::json!({ "path": path }), || {
                    let root = workspace_root(self)?;
                    workspace_read(&root, &path).map(serde_json::Value::String)
                })
            }
            "write" => {
                let path = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let content = args
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let options = args
                    .get("options")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let call_args = serde_json::json!({
                    "path": path,
                    "bytes": content.len(),
                    "options": options,
                });
                self.workspace_call("write", call_args, || {
                    let root = workspace_root(self)?;
                    workspace_write(&root, &path, &content, &options)
                })
            }
            "delete" | "remove" => {
                let path = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let reason = args
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                self.workspace_call(
                    "delete",
                    serde_json::json!({ "path": path, "reason": reason }),
                    || {
                        let root = workspace_root(self)?;
                        workspace_delete(&root, &path, reason.as_deref())
                    },
                )
            }
            "manifest" => self.workspace_call("manifest", serde_json::json!({}), || {
                let root = workspace_root(self)?;
                workspace_manifest(&root)
            }),
            other => Err(format!(
                "chidori.workspace.{other} is not supported on the rust engine"
            )),
        }
    }
}

/// The request-shaped pieces flattened out of a `chidori.context` segment
/// chain: folded system text, tool names, the message turns, and the cache
/// layout the author's explicit `cacheBreakpoint()` calls produced.
struct ContextRequestParts {
    system: Option<String>,
    tool_names: Vec<String>,
    messages: Vec<LlmMessage>,
    cache: CacheLayout,
}

fn cache_ttl_from_str(value: &str) -> CacheTtl {
    match value {
        "1h" => CacheTtl::OneHour,
        _ => CacheTtl::FiveMinutes,
    }
}

/// Flatten the JS-side context segment chain (oldest first) into request
/// parts. A `cacheBreakpoint` freezes everything appended so far: it marks the
/// latest message when one exists, otherwise the tools or system head.
fn context_request_parts(
    segments: &[serde_json::Value],
) -> std::result::Result<ContextRequestParts, String> {
    let str_field = |seg: &serde_json::Value, field: &str| -> String {
        seg.get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut system_parts: Vec<String> = Vec::new();
    let mut tool_names: Vec<String> = Vec::new();
    let mut messages: Vec<LlmMessage> = Vec::new();
    let mut cache = CacheLayout::default();
    for seg in segments {
        let kind = seg
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match kind {
            "system" => system_parts.push(str_field(seg, "text")),
            "tools" => {
                for name in seg
                    .get("names")
                    .and_then(serde_json::Value::as_array)
                    .map(|names| names.iter().filter_map(serde_json::Value::as_str))
                    .into_iter()
                    .flatten()
                {
                    if !tool_names.iter().any(|existing| existing == name) {
                        tool_names.push(name.to_string());
                    }
                }
            }
            "doc" => {
                let label = str_field(seg, "label");
                let text = str_field(seg, "text");
                messages.push(LlmMessage::user_text(format!(
                    "<document label=\"{label}\">\n{text}\n</document>"
                )));
            }
            // The recorded product of `Context.compact()`: prior turns folded
            // into one durable summary, framed so the model knows it reads
            // condensed history rather than a live user turn.
            "summary" => messages.push(LlmMessage::user_text(format!(
                "<conversation-summary>\n{}\n</conversation-summary>",
                str_field(seg, "text")
            ))),
            "user" => messages.push(LlmMessage::user_text(str_field(seg, "text"))),
            "assistant" => messages.push(LlmMessage::assistant_blocks(vec![ContentBlock::Text {
                text: str_field(seg, "text"),
            }])),
            "toolResult" => messages.push(LlmMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: str_field(seg, "id"),
                    content: str_field(seg, "content"),
                    is_error: seg
                        .get("isError")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                }],
                cache_control: None,
            }),
            "message" => {
                let message = seg
                    .get("message")
                    .cloned()
                    .ok_or_else(|| "context message segment is missing `message`".to_string())?;
                messages.push(
                    serde_json::from_value::<LlmMessage>(message)
                        .map_err(|err| format!("invalid context message segment: {err}"))?,
                );
            }
            "cacheBreakpoint" => {
                let ttl = cache_ttl_from_str(&str_field(seg, "ttl"));
                if let Some(last) = messages.last_mut() {
                    last.cache_control = Some(ttl);
                } else if !tool_names.is_empty() {
                    cache.tools = Some(ttl);
                } else if !system_parts.is_empty() {
                    cache.system = Some(ttl);
                }
            }
            other => return Err(format!("unknown context segment kind `{other}`")),
        }
    }
    Ok(ContextRequestParts {
        system: if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        },
        tool_names,
        messages,
        cache,
    })
}

#[derive(Debug, Clone, Default)]
pub struct HostBindingRecorder {
    // Arc<Mutex> (not Rc<RefCell>) so HostBindingBackend is Send — branch
    // sub-runs execute on their own threads when `chidori.branch` runs with
    // `concurrency > 1`.
    calls: Arc<StdMutex<Vec<HostBindingCall>>>,
}

impl HostBindingRecorder {
    #[allow(dead_code)]
    pub fn calls(&self) -> Vec<HostBindingCall> {
        self.calls.lock().unwrap().clone()
    }

    fn push(&self, function: impl Into<String>, args: serde_json::Value) {
        self.calls.lock().unwrap().push(HostBindingCall {
            function: function.into(),
            args,
        });
    }
}

fn workspace_root(backend: &HostBindingBackend) -> std::result::Result<PathBuf, String> {
    backend.workspace_root().ok_or_else(|| {
        "chidori.workspace requires CHIDORI_WORKSPACE_ROOT or a runtime workspace root".to_string()
    })
}

const WORKSPACE_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceManifest {
    version: u32,
    #[serde(default)]
    manifest_version: u64,
    #[serde(default)]
    active_attempt: u64,
    #[serde(default)]
    files: BTreeMap<String, WorkspaceFileEntry>,
    #[serde(default)]
    deleted: BTreeMap<String, WorkspaceDeletedEntry>,
}

impl Default for WorkspaceManifest {
    fn default() -> Self {
        Self {
            version: WORKSPACE_MANIFEST_VERSION,
            manifest_version: 0,
            active_attempt: workspace_attempt().unwrap_or(0),
            files: BTreeMap::new(),
            deleted: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceFileEntry {
    status: WorkspaceFileStatus,
    sha256: String,
    bytes: u64,
    language: Option<String>,
    attempt: Option<u64>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WorkspaceFileStatus {
    Complete,
    Writing,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDeletedEntry {
    attempt: Option<u64>,
    reason: Option<String>,
}

pub(crate) fn workspace_list(
    root: &Path,
    complete_only: bool,
) -> std::result::Result<serde_json::Value, String> {
    let manifest = read_workspace_manifest(root)?;
    let entries = manifest
        .files
        .into_iter()
        .filter(|(_, entry)| !complete_only || entry.status == WorkspaceFileStatus::Complete)
        .map(|(path, entry)| {
            serde_json::json!({
                "path": path,
                "status": entry.status,
                "sha256": entry.sha256,
                "bytes": entry.bytes,
                "language": entry.language,
                "attempt": entry.attempt,
                "updatedAt": entry.updated_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::Value::Array(entries))
}

pub(crate) fn workspace_read(root: &Path, path: &str) -> std::result::Result<String, String> {
    let relative = sanitize_workspace_path(path)?;
    let absolute = workspace_path(root, &relative)?;
    ensure_no_symlink_path(root, &absolute)?;
    std::fs::read_to_string(&absolute)
        .map_err(|err| format!("workspace.read {}: {err}", relative.display()))
}

pub(crate) fn workspace_write(
    root: &Path,
    path: &str,
    content: &str,
    options: &serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let relative = sanitize_workspace_path(path)?;
    let absolute = workspace_path(root, &relative)?;
    ensure_no_symlink_path(root, &absolute)?;
    let manifest_path = workspace_manifest_path(root);
    ensure_workspace_layout(root)?;
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create workspace parent {}: {err}", parent.display()))?;
    }
    let tmp = root
        .join(".generation")
        .join("tmp")
        .join(format!("write-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, content.as_bytes())
        .map_err(|err| format!("workspace temp write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &absolute).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("workspace atomic rename {}: {err}", absolute.display())
    })?;

    let mut manifest = read_workspace_manifest(root)?;
    let path = relative_path_string(&relative)?;
    let language = options
        .get("language")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| Some(language_for_path(&relative)));
    let entry = WorkspaceFileEntry {
        status: WorkspaceFileStatus::Complete,
        sha256: sha256_hex(content.as_bytes()),
        bytes: content.len() as u64,
        language,
        attempt: workspace_attempt(),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    if let Some(attempt) = workspace_attempt() {
        manifest.active_attempt = attempt;
    }
    manifest.files.insert(path.clone(), entry.clone());
    manifest.deleted.remove(&path);
    write_workspace_manifest(&manifest_path, &manifest)?;
    Ok(serde_json::json!({
        "path": path,
        "status": entry.status,
        "sha256": entry.sha256,
        "bytes": entry.bytes,
        "language": entry.language,
        "attempt": entry.attempt,
        "updatedAt": entry.updated_at,
    }))
}

pub(crate) fn workspace_delete(
    root: &Path,
    path: &str,
    reason: Option<&str>,
) -> std::result::Result<serde_json::Value, String> {
    let relative = sanitize_workspace_path(path)?;
    let absolute = workspace_path(root, &relative)?;
    ensure_no_symlink_path(root, &absolute)?;
    if absolute.exists() {
        std::fs::remove_file(&absolute)
            .map_err(|err| format!("workspace.delete {}: {err}", relative.display()))?;
    }
    let mut manifest = read_workspace_manifest(root)?;
    let path = relative_path_string(&relative)?;
    manifest.files.remove(&path);
    manifest.deleted.insert(
        path,
        WorkspaceDeletedEntry {
            attempt: workspace_attempt(),
            reason: reason.map(ToOwned::to_owned),
        },
    );
    write_workspace_manifest(&workspace_manifest_path(root), &manifest)?;
    Ok(serde_json::Value::Null)
}

pub(crate) fn workspace_manifest(root: &Path) -> std::result::Result<serde_json::Value, String> {
    read_workspace_manifest(root)
        .and_then(|manifest| serde_json::to_value(manifest).map_err(|err| err.to_string()))
}

fn read_workspace_manifest(root: &Path) -> std::result::Result<WorkspaceManifest, String> {
    ensure_workspace_layout(root)?;
    let path = workspace_manifest_path(root);
    if !path.exists() {
        let manifest = WorkspaceManifest::default();
        write_workspace_manifest(&path, &manifest)?;
        return Ok(manifest);
    }
    let bytes = std::fs::read(&path)
        .map_err(|err| format!("read workspace manifest {}: {err}", path.display()))?;
    let manifest: WorkspaceManifest = serde_json::from_slice(&bytes)
        .map_err(|err| format!("parse workspace manifest {}: {err}", path.display()))?;
    if manifest.version != WORKSPACE_MANIFEST_VERSION {
        return Err(format!(
            "unsupported workspace manifest version {}",
            manifest.version
        ));
    }
    Ok(manifest)
}

fn write_workspace_manifest(
    path: &Path,
    manifest: &WorkspaceManifest,
) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create workspace manifest dir {}: {err}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|err| format!("serialize workspace manifest: {err}"))?;
    std::fs::write(path, bytes)
        .map_err(|err| format!("write workspace manifest {}: {err}", path.display()))
}

fn ensure_workspace_layout(root: &Path) -> std::result::Result<(), String> {
    std::fs::create_dir_all(root.join(".generation").join("tmp"))
        .map_err(|err| format!("create workspace metadata dirs {}: {err}", root.display()))
}

fn workspace_manifest_path(root: &Path) -> PathBuf {
    root.join(".generation").join("manifest.json")
}

fn sanitize_workspace_path(path: &str) -> std::result::Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("workspace path must not be empty".to_string());
    }
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("invalid workspace path: {path}"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(format!("invalid workspace path: {path}"));
    }
    Ok(relative)
}

fn workspace_path(root: &Path, relative: &Path) -> std::result::Result<PathBuf, String> {
    let absolute = root.join(relative);
    if !absolute.starts_with(root) {
        return Err(format!(
            "workspace path escapes root: {}",
            relative.display()
        ));
    }
    Ok(absolute)
}

fn ensure_no_symlink_path(root: &Path, absolute: &Path) -> std::result::Result<(), String> {
    let mut current = root.to_path_buf();
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| format!("workspace path escapes root: {}", absolute.display()))?;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(format!("invalid workspace path: {}", relative.display()));
        };
        current.push(part);
        if let Ok(metadata) = std::fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "workspace path must not traverse a symlink: {}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn relative_path_string(path: &Path) -> std::result::Result<String, String> {
    let value = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => Ok(part.to_string_lossy().to_string()),
            _ => Err(format!("invalid workspace path: {}", path.display())),
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("/");
    if value.is_empty() {
        Err(format!("invalid workspace path: {}", path.display()))
    } else {
        Ok(value)
    }
}

fn workspace_attempt() -> Option<u64> {
    std::env::var("CHIDORI_WORKSPACE_ATTEMPT")
        .ok()
        .and_then(|value| value.parse().ok())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn language_for_path(path: &Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "json" => "json",
        "md" | "mdx" => "markdown",
        "py" => "python",
        "rs" => "rust",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "css" => "css",
        "html" => "html",
        _ => "text",
    }
    .to_string()
}

fn tool_def_to_schema(def: &ToolDef) -> ToolSchema {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in &def.params {
        let mut prop = serde_json::Map::new();
        prop.insert(
            "type".to_string(),
            serde_json::Value::String(param.param_type.clone()),
        );
        if let Some(description) = &param.description {
            prop.insert(
                "description".to_string(),
                serde_json::Value::String(description.clone()),
            );
        }
        properties.insert(param.name.clone(), serde_json::Value::Object(prop));
        if param.required {
            required.push(serde_json::Value::String(param.name.clone()));
        }
    }
    ToolSchema {
        name: def.name.clone(),
        description: def.description.clone(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}
