use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};

// Only the test-only JS-glue installers below use the anyhow `Result` alias.
#[cfg(test)]
use anyhow::Result;
// rquickjs is a dev-dependency; the JS-glue installers below are test-only. The
// host backend itself (used by the pure-Rust engine under `rust-engine`) is
// rquickjs-free.
#[cfg(test)]
use rquickjs::function::{Func, MutFn, Opt};
#[cfg(test)]
use rquickjs::{Ctx, Exception, Function, Object, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mcp::McpManager;
use crate::policy::{Decision, PolicyCache, PolicyConfig};
use crate::providers::{
    ContentBlock, LlmRequest, Message as LlmMessage, ProviderRegistry, ToolSchema,
};
use crate::runtime::call_log::CallRecord;
use crate::runtime::context::{InputMode, PendingApproval, RuntimeContext, PAUSE_MARKER};
use crate::runtime::host_core;
use crate::runtime::snapshot::RuntimePolicy;
use crate::runtime::template::TemplateEngine;
use crate::runtime::typescript::engine::{HostBindingCall, TypeScriptVmRuntime};
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
                let request = LlmRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    system: system.clone(),
                    temperature,
                    max_tokens,
                    tools: tool_schemas.clone(),
                };
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
                    }),
                    prompt_type.clone(),
                )
                .map_err(|err| err.to_string())?;

                final_text = response.content.clone();
                if response.tool_calls.is_empty() {
                    break;
                }
                messages.push(LlmMessage::assistant_blocks(response.blocks.clone()));
                let mut result_blocks = Vec::new();
                for call in response.tool_calls {
                    match self.tool(call.name.clone(), call.input.clone()) {
                        Ok(value) => result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: serde_json::to_string(&value)
                                .unwrap_or_else(|_| value.to_string()),
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
                messages.push(LlmMessage {
                    role: "user".to_string(),
                    content: result_blocks,
                });
            }
            if format.as_deref() == Some("json") {
                return serde_json::from_str::<serde_json::Value>(&final_text)
                    .or(Ok(serde_json::Value::String(final_text)));
            }
            return Ok(serde_json::Value::String(final_text));
        }

        let request = LlmRequest {
            model: model.clone(),
            messages: vec![LlmMessage::user_text(text.clone())],
            system: system.clone(),
            temperature,
            max_tokens,
            tools: Vec::new(),
        };
        let result = host_core::execute_prompt_text(
            runtime_ctx,
            providers,
            tokio_rt,
            request,
            serde_json::json!({ "text": text, "model": model, "type": prompt_type }),
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

    fn input(&self, prompt: String) -> std::result::Result<String, String> {
        let HostBindingBackend::Runtime { runtime_ctx, .. } = self else {
            return Err("chidori.input requires the runtime host backend".to_string());
        };

        let result =
            host_core::execute_input(runtime_ctx, &serde_json::json!({ "prompt": prompt }))
                .map_err(|err| err.to_string())?;
        Ok(result.as_str().unwrap_or("").to_string())
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
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            runtime_policy,
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
                        // Native nested execution (G4): when the rust engine is
                        // the active runtime, run the nested TS tool on it too,
                        // threading the same backend (`self`) so host effects
                        // nest under this tool call and a suspension propagates.
                        // Otherwise re-enter the QuickJS `TypeScriptVmRuntime`.
                        #[cfg(feature = "rust-engine")]
                        if matches!(
                            crate::runtime::rust_engine::selected_engine(),
                            crate::runtime::rust_engine::EngineKind::Rust
                        ) {
                            return crate::runtime::rust_engine::run_tool_file(
                                &tool_def.source_path,
                                &serde_json::Value::Object(kwargs),
                                self,
                            );
                        }
                        TypeScriptVmRuntime::new(runtime_policy.clone())?
                            .run_tool_file_with_context(
                                &tool_def.source_path,
                                &serde_json::Value::Object(kwargs),
                                runtime_ctx.clone(),
                                providers.clone(),
                                template_engine.clone(),
                                tokio_rt.clone(),
                                policy.clone(),
                                policy_cache.clone(),
                                tools.clone(),
                                mcp.clone(),
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
        let HostBindingBackend::Runtime {
            runtime_ctx,
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            runtime_policy,
            tools,
            mcp,
        } = self
        else {
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
                    // Native nested execution (G4): run the sub-agent on the
                    // rust engine when it's active, sharing this backend so the
                    // child's host effects nest under the callAgent and a
                    // suspension propagates to the parent run.
                    #[cfg(feature = "rust-engine")]
                    if matches!(
                        crate::runtime::rust_engine::selected_engine(),
                        crate::runtime::rust_engine::EngineKind::Rust
                    ) {
                        return crate::runtime::rust_engine::run_agent_file(
                            Path::new(path),
                            input,
                            self,
                        );
                    }
                    let runtime = TypeScriptVmRuntime::new(runtime_policy.clone())?;
                    runtime.run_agent_file_with_context(
                        Path::new(path),
                        input,
                        runtime_ctx.clone(),
                        providers.clone(),
                        template_engine.clone(),
                        tokio_rt.clone(),
                        policy.clone(),
                        policy_cache.clone(),
                        tools.clone(),
                        mcp.clone(),
                    )
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
            return Err("chidori.http requires the runtime host backend".to_string());
        };

        host_core::execute_http(tokio_rt, args).map_err(|err| err.to_string())
    }

    fn sandbox_exec(
        &self,
        function: &str,
        source: String,
        fuel: u64,
    ) -> std::result::Result<String, String> {
        let result = self.durable_call(
            function,
            serde_json::json!({
                "source": source,
                "fuel": fuel,
            }),
            || {
                let args = serde_json::json!({
                    "source": source,
                    "fuel": fuel,
                });
                host_core::execute_sandbox_string(function, &args).map_err(|err| err.to_string())
            },
        )?;
        result
            .unwrap_or(serde_json::Value::Null)
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("{function} replay result must be a string"))
    }

    fn sandbox_exec_wasm(
        &self,
        source: String,
        function: String,
        args: serde_json::Value,
        fuel: u64,
        memory_pages: u32,
    ) -> std::result::Result<serde_json::Value, String> {
        let result = self.durable_call(
            "exec",
            serde_json::json!({
                "source": source,
                "function": function,
                "args": args,
                "fuel": fuel,
                "memory_pages": memory_pages,
            }),
            || {
                let args = serde_json::json!({
                    "source": source,
                    "function": function,
                    "args": args,
                    "fuel": fuel,
                    "memory_pages": memory_pages,
                });
                host_core::execute_sandbox_wasm(&args).map_err(|err| err.to_string())
            },
        )?;
        Ok(result.unwrap_or(serde_json::Value::Null))
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
        let opt_null =
            |v: Option<serde_json::Value>| v.unwrap_or(serde_json::Value::Null);
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
                        .ok_or_else(|| "chidori.http options must include string url".to_string())?
                        .to_string();
                    (url, first.clone())
                } else {
                    return Err(
                        "chidori.http requires a URL string or options object".to_string()
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
            "execJs" | "execPython" => {
                let source = a
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let options = a.get("opts").cloned().unwrap_or(serde_json::Value::Null);
                let fuel = options
                    .get("fuel")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(200_000_000)
                    .max(1);
                let function = if effect == "execJs" {
                    "exec_js"
                } else {
                    "exec_python"
                };
                self.sandbox_exec(function, source, fuel)
                    .map(serde_json::Value::String)
            }
            "execWasm" => {
                let source = a
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let options = a.get("opts").cloned().unwrap_or(serde_json::Value::Null);
                let function = options
                    .get("function")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("main")
                    .to_string();
                let args = options
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
                let fuel = options
                    .get("fuel")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1_000_000)
                    .max(1);
                let memory_pages = options
                    .get("memoryPages")
                    .or_else(|| options.get("memory_pages"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(16)
                    .max(1) as u32;
                self.sandbox_exec_wasm(source, function, args, fuel, memory_pages)
            }
            "workspace" => self.dispatch_workspace(a),
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
        let action = a.get("action").and_then(serde_json::Value::as_str).unwrap_or("");
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

#[derive(Debug, Clone, Default)]
pub struct HostBindingRecorder {
    calls: Rc<RefCell<Vec<HostBindingCall>>>,
}

impl HostBindingRecorder {
    #[allow(dead_code)]
    pub fn calls(&self) -> Vec<HostBindingCall> {
        self.calls.borrow().clone()
    }

    fn push(&self, function: impl Into<String>, args: serde_json::Value) {
        self.calls.borrow_mut().push(HostBindingCall {
            function: function.into(),
            args,
        });
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub fn install_chidori_object<'js>(
    ctx: &Ctx<'js>,
    recorder: HostBindingRecorder,
) -> Result<Object<'js>> {
    install_chidori_object_with_backend(ctx, HostBindingBackend::Recorder(recorder))
}

#[cfg(test)]
pub fn install_chidori_object_for_runtime<'js>(
    ctx: &Ctx<'js>,
    runtime_ctx: RuntimeContext,
    providers: Arc<ProviderRegistry>,
    template_engine: Arc<TemplateEngine>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    policy: Arc<PolicyConfig>,
    policy_cache: Arc<StdMutex<PolicyCache>>,
    runtime_policy: RuntimePolicy,
    tools: Arc<ToolRegistry>,
    mcp: Arc<McpManager>,
) -> Result<Object<'js>> {
    install_chidori_object_with_backend(
        ctx,
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
        },
    )
}

#[cfg(test)]
fn install_chidori_object_with_backend<'js>(
    ctx: &Ctx<'js>,
    backend: HostBindingBackend,
) -> Result<Object<'js>> {
    let chidori = Object::new(ctx.clone())?;

    let log_backend = backend.clone();
    chidori.set(
        "log",
        Func::from(MutFn::from(move |ctx: Ctx<'js>, message: String| {
            let args = serde_json::json!({ "message": message });
            log_backend
                .durable_call("log", args.clone(), || {
                    host_core::execute_log(&args).map_err(|err| err.to_string())
                })
                .map(|_| ())
                .map_err(|err| Exception::throw_message(&ctx, &err))
        })),
    )?;

    let checkpoint_backend = backend.clone();
    chidori.set(
        "checkpoint",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  label: Opt<String>,
                  data: Opt<Value<'js>>|
                  -> rquickjs::Result<()> {
                let data = match data.0 {
                    Some(value) => js_value_to_json(&ctx, value)?,
                    None => serde_json::Value::Null,
                };
                checkpoint_backend
                    .durable_call(
                        "checkpoint",
                        serde_json::json!({
                            "label": label.0,
                            "data": data,
                        }),
                        || Ok(serde_json::Value::Null),
                    )
                    .map(|_| ())
                    .map_err(|err| Exception::throw_message(&ctx, &err))
            },
        )),
    )?;

    let prompt_backend = backend.clone();
    chidori.set(
        "prompt",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  text: String,
                  options: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let options = match options.0 {
                    Some(options) => js_value_to_json(&ctx, options)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let result = prompt_backend
                    .prompt(text, options)
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    let memory_backend = backend.clone();
    chidori.set(
        "memory",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  action: String,
                  key: Opt<String>,
                  value: Opt<Value<'js>>,
                  options: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let value = match value.0 {
                    Some(value) => Some(js_value_to_json(&ctx, value)?),
                    None => None,
                };
                let options = match options.0 {
                    Some(options) => js_value_to_json(&ctx, options)?,
                    None => serde_json::Value::Null,
                };
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
                    "key": key.0,
                    "namespace": namespace,
                    "prefix": prefix,
                    "value": value,
                });

                let result = memory_backend
                    .durable_call("memory", args.clone(), || {
                        host_core::execute_memory(&args).map_err(|err| err.to_string())
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result.unwrap_or(serde_json::Value::Null))
            },
        )),
    )?;

    let template_backend = backend.clone();
    chidori.set(
        "template",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  template: String,
                  vars: Opt<Value<'js>>,
                  _options: Opt<Value<'js>>|
                  -> rquickjs::Result<String> {
                let vars = match vars.0 {
                    Some(vars) => js_value_to_json(&ctx, vars)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let args = serde_json::json!({
                    "template": template,
                    "vars": vars,
                });
                let template_engine = template_backend.template_engine();
                let result = template_backend
                    .durable_call("template", args.clone(), || {
                        host_core::execute_template(&template_engine, &args)
                            .map_err(|err| err.to_string())
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?
                    .unwrap_or(serde_json::Value::Null);
                result.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    Exception::throw_message(&ctx, "template replay result must be a string")
                })
            },
        )),
    )?;

    let input_backend = backend.clone();
    chidori.set(
        "input",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>, prompt: String, _options: Opt<Value<'js>>| {
                input_backend
                    .input(prompt)
                    .map_err(|err| Exception::throw_message(&ctx, &err))
            },
        )),
    )?;

    let http_backend = backend.clone();
    chidori.set(
        "http",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  url_or_options: Value<'js>,
                  options: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let first = js_value_to_json(&ctx, url_or_options)?;
                let (url, options) = if let Some(url) = first.as_str() {
                    let options = match options.0 {
                        Some(options) => js_value_to_json(&ctx, options)?,
                        None => serde_json::Value::Object(Default::default()),
                    };
                    (url.to_string(), options)
                } else if let serde_json::Value::Object(map) = first {
                    let url = map
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            Exception::throw_message(
                                &ctx,
                                "chidori.http options must include string url",
                            )
                        })?
                        .to_string();
                    (url, serde_json::Value::Object(map))
                } else {
                    return Err(Exception::throw_message(
                        &ctx,
                        "chidori.http requires a URL string or options object",
                    ));
                };
                let mut method = options
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("GET")
                    .to_uppercase();
                if method.is_empty() {
                    method = "GET".to_string();
                }
                let headers = options.get("headers").and_then(|value| match value {
                    serde_json::Value::Object(map) => Some(map.clone()),
                    _ => None,
                });
                let body = options.get("body").cloned();
                let params = options
                    .get("params")
                    .or_else(|| options.get("query"))
                    .and_then(|value| match value {
                        serde_json::Value::Object(map) => Some(map.clone()),
                        _ => None,
                    });
                let args = serde_json::json!({
                    "url": url,
                    "method": method,
                    "headers": headers,
                    "body": body,
                    "params": params,
                });

                let result = http_backend
                    .durable_call("http", args.clone(), || {
                        http_backend.enforce_policy(
                            "http",
                            &serde_json::json!({
                                "url": args.get("url").cloned().unwrap_or_default(),
                                "method": args.get("method").cloned().unwrap_or_default(),
                            }),
                        )?;
                        http_backend.block_on_http(&args)
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?
                    .unwrap_or(serde_json::Value::Null);
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    let call_agent_backend = backend.clone();
    chidori.set(
        "callAgent",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  path: String,
                  input: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let input = match input.0 {
                    Some(input) => js_value_to_json(&ctx, input)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let result = call_agent_backend
                    .call_agent(path, input)
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    let tool_backend = backend.clone();
    chidori.set(
        "tool",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  name: String,
                  args: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let args = match args.0 {
                    Some(args) => js_value_to_json(&ctx, args)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let result = tool_backend
                    .tool(name, args)
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    install_sandbox_binding(ctx, &chidori, backend.clone(), "execJs", "exec_js")?;
    install_sandbox_binding(ctx, &chidori, backend.clone(), "execPython", "exec_python")?;
    install_workspace_binding(ctx, &chidori, backend.clone())?;

    let wasm_backend = backend.clone();
    chidori.set(
        "execWasm",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  source: String,
                  options: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let options = match options.0 {
                    Some(options) => js_value_to_json(&ctx, options)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let function = options
                    .get("function")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("main")
                    .to_string();
                let args = options
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
                let fuel = options
                    .get("fuel")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1_000_000)
                    .max(1);
                let memory_pages = options
                    .get("memoryPages")
                    .or_else(|| options.get("memory_pages"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(16)
                    .max(1) as u32;
                let result = wasm_backend
                    .sandbox_exec_wasm(source, function, args, fuel, memory_pages)
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    install_js_helpers(ctx, &chidori)?;

    Ok(chidori)
}

#[cfg(test)]
fn install_sandbox_binding<'js>(
    _ctx: &Ctx<'js>,
    chidori: &Object<'js>,
    backend: HostBindingBackend,
    js_name: &'static str,
    function: &'static str,
) -> Result<()> {
    chidori.set(
        js_name,
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  source: String,
                  options: Opt<Value<'js>>|
                  -> rquickjs::Result<String> {
                let options = match options.0 {
                    Some(options) => js_value_to_json(&ctx, options)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let fuel = options
                    .get("fuel")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(200_000_000)
                    .max(1);
                backend
                    .sandbox_exec(function, source, fuel)
                    .map_err(|err| Exception::throw_message(&ctx, &err))
            },
        )),
    )?;
    Ok(())
}

#[cfg(test)]
fn install_workspace_binding<'js>(
    ctx: &Ctx<'js>,
    chidori: &Object<'js>,
    backend: HostBindingBackend,
) -> Result<()> {
    let workspace = Object::new(ctx.clone())?;

    let list_backend = backend.clone();
    workspace.set(
        "list",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>, options: Opt<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let options = match options.0 {
                    Some(options) => js_value_to_json(&ctx, options)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let complete_only = options
                    .get("completeOnly")
                    .or_else(|| options.get("complete_only"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let result = list_backend
                    .workspace_call("list", options, || {
                        let root = workspace_root(&list_backend)?;
                        workspace_list(&root, complete_only)
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    let read_backend = backend.clone();
    workspace.set(
        "read",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>, path: String| -> rquickjs::Result<String> {
                let result = read_backend
                    .workspace_call("read", serde_json::json!({ "path": path }), || {
                        let root = workspace_root(&read_backend)?;
                        workspace_read(&root, &path).map(serde_json::Value::String)
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                result.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    Exception::throw_message(&ctx, "workspace.read result must be a string")
                })
            },
        )),
    )?;

    let write_backend = backend.clone();
    workspace.set(
        "write",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>,
                  path: String,
                  content: String,
                  options: Opt<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let options = match options.0 {
                    Some(options) => js_value_to_json(&ctx, options)?,
                    None => serde_json::Value::Object(Default::default()),
                };
                let args = serde_json::json!({
                    "path": path,
                    "bytes": content.len(),
                    "options": options,
                });
                let result = write_backend
                    .workspace_call("write", args, || {
                        let root = workspace_root(&write_backend)?;
                        workspace_write(&root, &path, &content, &options)
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    let delete_backend = backend.clone();
    workspace.set(
        "delete",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>, path: String, reason: Opt<String>| -> rquickjs::Result<()> {
                delete_backend
                    .workspace_call(
                        "delete",
                        serde_json::json!({
                            "path": path,
                            "reason": reason.0,
                        }),
                        || {
                            let root = workspace_root(&delete_backend)?;
                            workspace_delete(&root, &path, reason.0.as_deref())
                        },
                    )
                    .map(|_| ())
                    .map_err(|err| Exception::throw_message(&ctx, &err))
            },
        )),
    )?;
    let remove_backend = backend.clone();
    workspace.set(
        "remove",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>, path: String, reason: Opt<String>| -> rquickjs::Result<()> {
                remove_backend
                    .workspace_call(
                        "delete",
                        serde_json::json!({
                            "path": path,
                            "reason": reason.0,
                        }),
                        || {
                            let root = workspace_root(&remove_backend)?;
                            workspace_delete(&root, &path, reason.0.as_deref())
                        },
                    )
                    .map(|_| ())
                    .map_err(|err| Exception::throw_message(&ctx, &err))
            },
        )),
    )?;

    let manifest_backend = backend.clone();
    workspace.set(
        "manifest",
        Func::from(MutFn::from(
            move |ctx: Ctx<'js>| -> rquickjs::Result<Value<'js>> {
                let result = manifest_backend
                    .workspace_call("manifest", serde_json::json!({}), || {
                        let root = workspace_root(&manifest_backend)?;
                        workspace_manifest(&root)
                    })
                    .map_err(|err| Exception::throw_message(&ctx, &err))?;
                json_to_js_value(&ctx, &result)
            },
        )),
    )?;

    chidori.set("workspace", workspace)?;
    Ok(())
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

#[cfg(test)]
fn install_js_helpers<'js>(ctx: &Ctx<'js>, chidori: &Object<'js>) -> Result<()> {
    let install: Function = ctx.eval(
        r#"
        (chidori) => {
            chidori.tryCall = async function tryCall(fn) {
                try {
                    return { ok: true, value: await fn() };
                } catch (err) {
                    return {
                        ok: false,
                        error: String(err && err.message ? err.message : err),
                    };
                }
            };

            chidori.retry = async function retry(fn, options) {
                const attempts = Math.max(1, Number(options && options.attempts) || 3);
                let lastErr;
                for (let i = 0; i < attempts; i += 1) {
                    try {
                        return await fn();
                    } catch (err) {
                        lastErr = err;
                    }
                }
                throw lastErr;
            };

            chidori.parallel = async function parallel(tasks, options) {
                if (!Array.isArray(tasks)) {
                    throw new Error("chidori.parallel expects an array of task functions");
                }
                for (const [index, task] of tasks.entries()) {
                    if (typeof task !== "function") {
                        throw new Error(`chidori.parallel task ${index} must be a function`);
                    }
                }
                const concurrency = Math.max(
                    1,
                    Math.min(
                        tasks.length || 1,
                        Number(options && options.concurrency) || tasks.length || 1,
                    ),
                );
                const results = new Array(tasks.length);
                let next = 0;

                async function worker() {
                    while (next < tasks.length) {
                        const index = next;
                        next += 1;
                        results[index] = await tasks[index]();
                    }
                }

                await Promise.all(
                    Array.from({ length: concurrency }, () => worker()),
                );
                return results;
            };

            if (typeof chidori.memory === "function") {
                const memoryCall = chidori.memory.__chidori_call || chidori.memory;
                function memory(...args) {
                    return memoryCall.call(chidori, ...args);
                }
                memory.__chidori_call = memoryCall;
                memory.set = function set(key, value, options) {
                    return memory("set", key, value, options);
                };
                memory.get = function get(key, options) {
                    return memory("get", key, null, options);
                };
                memory.delete = function deleteKey(key, options) {
                    return memory("delete", key, null, options);
                };
                memory.clear = function clear(options) {
                    return memory("clear", null, null, options);
                };
                chidori.memory = memory;
            }
        }
        "#,
    )?;
    install.call::<_, ()>((chidori.clone(),))?;
    Ok(())
}

#[cfg(test)]
fn js_value_to_json<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> rquickjs::Result<serde_json::Value> {
    let Some(json) = ctx.json_stringify(value)? else {
        return Ok(serde_json::Value::Null);
    };
    let json = json.to_string()?;
    serde_json::from_str(&json).map_err(|err| {
        Exception::throw_message(ctx, &format!("value is not JSON-compatible: {err}"))
    })
}

#[cfg(test)]
fn json_to_js_value<'js>(
    ctx: &Ctx<'js>,
    value: &serde_json::Value,
) -> rquickjs::Result<Value<'js>> {
    let json = serde_json::to_string(value).map_err(|err| {
        Exception::throw_message(ctx, &format!("value is not JSON-compatible: {err}"))
    })?;
    ctx.json_parse(json)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ContentBlock, LlmProvider, LlmResponse, TokenSink};
    use crate::runtime::call_log::{CallRecord, TokenUsage};
    use chrono::Utc;
    use rquickjs::{Context, Runtime};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StreamingProvider;

    #[async_trait::async_trait]
    impl LlmProvider for StreamingProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, _request: &LlmRequest) -> anyhow::Result<LlmResponse> {
            Ok(LlmResponse {
                content: "hello world".to_string(),
                blocks: vec![ContentBlock::Text {
                    text: "hello world".to_string(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 2,
                output_tokens: 3,
            })
        }

        async fn stream(
            &self,
            _request: &LlmRequest,
            on_delta: &mut TokenSink,
        ) -> anyhow::Result<LlmResponse> {
            on_delta("hello ");
            on_delta("world");
            self.send(_request).await
        }
    }

    struct ToolUseProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolUseProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, _request: &LlmRequest) -> anyhow::Result<LlmResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(LlmResponse {
                    content: String::new(),
                    blocks: vec![ContentBlock::ToolUse {
                        id: "toolu_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 41 }),
                    }],
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "toolu_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 41 }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 4,
                    output_tokens: 5,
                })
            } else {
                Ok(LlmResponse {
                    content: "final answer".to_string(),
                    blocks: vec![ContentBlock::Text {
                        text: "final answer".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    stop_reason: "end_turn".to_string(),
                    input_tokens: 6,
                    output_tokens: 7,
                })
            }
        }
    }

    fn template_engine() -> Arc<TemplateEngine> {
        Arc::new(TemplateEngine::new("."))
    }

    fn runtime_host() -> (
        Arc<tokio::runtime::Runtime>,
        Arc<PolicyConfig>,
        Arc<StdMutex<PolicyCache>>,
    ) {
        (
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(PolicyConfig::default()),
            Arc::new(StdMutex::new(PolicyCache::default())),
        )
    }

    fn install_runtime_chidori<'js>(ctx: &Ctx<'js>, runtime_ctx: RuntimeContext) -> Object<'js> {
        let (tokio_rt, policy, policy_cache) = runtime_host();
        install_chidori_object_for_runtime(
            ctx,
            runtime_ctx,
            Arc::new(ProviderRegistry::new()),
            template_engine(),
            tokio_rt,
            policy,
            policy_cache,
            RuntimePolicy::durable_default("ts-binding-test"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        )
        .unwrap()
    }

    fn install_runtime_chidori_with_policy<'js>(
        ctx: &Ctx<'js>,
        runtime_ctx: RuntimeContext,
        policy: Arc<PolicyConfig>,
    ) -> Object<'js> {
        let (tokio_rt, _, policy_cache) = runtime_host();
        install_chidori_object_for_runtime(
            ctx,
            runtime_ctx,
            Arc::new(ProviderRegistry::new()),
            template_engine(),
            tokio_rt,
            policy,
            policy_cache,
            RuntimePolicy::durable_default("ts-binding-test"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        )
        .unwrap()
    }

    fn install_runtime_chidori_with_providers<'js>(
        ctx: &Ctx<'js>,
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
    ) -> Object<'js> {
        let (tokio_rt, policy, policy_cache) = runtime_host();
        install_chidori_object_for_runtime(
            ctx,
            runtime_ctx,
            providers,
            template_engine(),
            tokio_rt,
            policy,
            policy_cache,
            RuntimePolicy::durable_default("ts-binding-test"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        )
        .unwrap()
    }

    fn install_runtime_chidori_with_tools<'js>(
        ctx: &Ctx<'js>,
        runtime_ctx: RuntimeContext,
        tools: Arc<ToolRegistry>,
    ) -> Object<'js> {
        let (tokio_rt, policy, policy_cache) = runtime_host();
        install_chidori_object_for_runtime(
            ctx,
            runtime_ctx,
            Arc::new(ProviderRegistry::new()),
            template_engine(),
            tokio_rt,
            policy,
            policy_cache,
            RuntimePolicy::durable_default("ts-binding-test"),
            tools,
            Arc::new(McpManager::new()),
        )
        .unwrap()
    }

    #[test]
    fn installs_log_binding_and_records_calls() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let recorder = HostBindingRecorder::default();

        context.with(|ctx| {
            let chidori = install_chidori_object(&ctx, recorder.clone()).unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"chidori.log("hello")"#).unwrap();
        });

        assert_eq!(
            recorder.calls(),
            vec![HostBindingCall {
                function: "log".to_string(),
                args: serde_json::json!({ "message": "hello" }),
            }]
        );
    }

    #[test]
    fn unsupported_bindings_throw() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let recorder = HostBindingRecorder::default();

        context.with(|ctx| {
            let chidori = install_chidori_object(&ctx, recorder).unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            let err = ctx.eval::<(), _>(r#"chidori.prompt("hello")"#).unwrap_err();
            assert!(matches!(err, rquickjs::Error::Exception));
        });
    }

    #[test]
    fn try_call_and_retry_helpers_work() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let recorder = HostBindingRecorder::default();

        context.with(|ctx| {
            let chidori = install_chidori_object(&ctx, recorder).unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__helperResult = (async () => {
                    let attempts = 0;
                    const value = await chidori.retry(async () => {
                        attempts += 1;
                        if (attempts < 2) {
                            throw new Error("again");
                        }
                        return 42;
                    }, { attempts: 3 });
                    const caught = await chidori.tryCall(async () => {
                        throw new Error("handled");
                    });
                    return { value, attempts, caught };
                })();
                "#,
            )
            .unwrap();
            let promise: rquickjs::Promise = ctx.eval("globalThis.__helperResult").unwrap();
            let value: Value = promise.finish().unwrap();
            let json = js_value_to_json(&ctx, value).unwrap();
            assert_eq!(
                json,
                serde_json::json!({
                    "value": 42,
                    "attempts": 2,
                    "caught": {
                        "ok": false,
                        "error": "handled",
                    },
                })
            );
        });
    }

    #[test]
    fn parallel_helper_runs_task_functions_in_order() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let recorder = HostBindingRecorder::default();

        context.with(|ctx| {
            let chidori = install_chidori_object(&ctx, recorder).unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__parallelResult = chidori.parallel([
                    async () => 1,
                    async () => 2,
                ]);
                "#,
            )
            .unwrap();
            let promise: rquickjs::Promise = ctx.eval("globalThis.__parallelResult").unwrap();
            let value: Value = promise.finish().unwrap();
            let json = js_value_to_json(&ctx, value).unwrap();
            assert_eq!(json, serde_json::json!([1, 2]));
        });
    }

    #[test]
    fn parallel_helper_honors_concurrency_limit() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let recorder = HostBindingRecorder::default();

        context.with(|ctx| {
            let chidori = install_chidori_object(&ctx, recorder).unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                let active = 0;
                let maxActive = 0;
                const task = async (value) => {
                    active += 1;
                    maxActive = Math.max(maxActive, active);
                    await Promise.resolve();
                    active -= 1;
                    return value;
                };
                globalThis.__parallelResult = (async () => {
                    const values = await chidori.parallel([
                        () => task(1),
                        () => task(2),
                        () => task(3),
                    ], { concurrency: 1 });
                    return { values, maxActive };
                })();
                "#,
            )
            .unwrap();
            let promise: rquickjs::Promise = ctx.eval("globalThis.__parallelResult").unwrap();
            let value: Value = promise.finish().unwrap();
            let json = js_value_to_json(&ctx, value).unwrap();
            assert_eq!(
                json,
                serde_json::json!({ "values": [1, 2, 3], "maxActive": 1 })
            );
        });
    }

    #[test]
    fn runtime_binding_records_log_call() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"chidori.log("hello")"#).unwrap();
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].function, "log");
        assert_eq!(records[0].args, serde_json::json!({ "message": "hello" }));
    }

    #[test]
    fn runtime_binding_records_checkpoint_call() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"chidori.checkpoint("draft", { count: 2 })"#)
                .unwrap();
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].function, "checkpoint");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "label": "draft",
                "data": { "count": 2 },
            })
        );
    }

    #[test]
    fn runtime_binding_workspace_uses_real_disk_and_manifest() {
        let root = std::env::temp_dir().join(format!(
            "chidori-workspace-binding-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        runtime_ctx.set_workspace_root(root.clone());

        let json = context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            let value: Value = ctx
                .eval(
                    r#"
                    (() => {
                        const entry = chidori.workspace.write(
                            "agent.ts",
                            "export default {};",
                            { language: "typescript" },
                        );
                        const content = chidori.workspace.read("agent.ts");
                        const list = chidori.workspace.list({ completeOnly: true });
                        chidori.workspace.delete("agent.ts", "regenerated");
                        const manifest = chidori.workspace.manifest();
                        return { entry, content, list, manifest };
                    })()
                    "#,
                )
                .unwrap();
            js_value_to_json(&ctx, value).unwrap()
        });

        assert_eq!(json["entry"]["path"], "agent.ts");
        assert_eq!(json["content"], "export default {};");
        assert_eq!(json["list"][0]["path"], "agent.ts");
        assert!(!root.join("agent.ts").exists());
        assert_eq!(
            json["manifest"]["deleted"]["agent.ts"]["reason"],
            "regenerated"
        );

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 5);
        assert!(records.iter().all(|record| record.function == "workspace"));
        assert_eq!(records[0].args["action"], "write");
        assert_eq!(records[3].args["action"], "delete");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_binding_replays_prompt_response() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "prompt".to_string(),
            args: serde_json::json!({
                "text": "hello",
                "model": "cached-model",
                "type": "progress",
            }),
            result: serde_json::json!("cached response"),
            duration_ms: 5,
            token_usage: Some(TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
            }),
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__promptResponse = chidori.prompt("hello", { type: "progress" })"#,
            )
            .unwrap();
            let response: String = ctx.eval("globalThis.__promptResponse").unwrap();
            assert_eq!(response, "cached response");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_streams_labelled_prompt_events() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        runtime_ctx.set_event_sender(tx);
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(StreamingProvider));
        let providers = Arc::new(providers);

        context.with(|ctx| {
            let chidori =
                install_runtime_chidori_with_providers(&ctx, runtime_ctx.clone(), providers);
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__promptResponse = chidori.prompt("hello", { type: "progress" })"#,
            )
            .unwrap();
            let response: String = ctx.eval("globalThis.__promptResponse").unwrap();
            assert_eq!(response, "hello world");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "prompt");
        assert_eq!(records[0].args["type"], serde_json::json!("progress"));
        assert_eq!(
            records[0]
                .token_usage
                .as_ref()
                .map(|usage| usage.output_tokens),
            Some(3)
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            crate::runtime::context::RuntimeEvent::PromptStart {
                prompt_type: Some(prompt_type),
                ..
            } if prompt_type == "progress"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::runtime::context::RuntimeEvent::PromptDelta {
                prompt_type: Some(prompt_type),
                delta,
                ..
            } if prompt_type == "progress" && delta == "hello "
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::runtime::context::RuntimeEvent::PromptEnd {
                prompt_type: Some(prompt_type),
                error: None,
                ..
            } if prompt_type == "progress"
        )));
    }

    #[test]
    fn runtime_binding_records_memory_calls() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        let namespace = format!("ts-memory-{}", uuid::Uuid::new_v4());
        let script = format!(
            r#"
            chidori.memory.set("answer", {{ value: 42 }}, {{ namespace: "{namespace}" }});
            globalThis.__memoryValue = chidori.memory.get("answer", {{ namespace: "{namespace}" }});
            "#
        );

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(script).unwrap();
            let value: i32 = ctx.eval(r#"globalThis.__memoryValue.value"#).unwrap();
            assert_eq!(value, 42);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].function, "memory");
        assert_eq!(records[0].result, serde_json::Value::Null);
        assert_eq!(records[1].function, "memory");
        assert_eq!(records[1].result, serde_json::json!({ "value": 42 }));
    }

    #[test]
    fn runtime_binding_replays_memory_without_touching_disk() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let namespace = format!("ts-memory-{}", uuid::Uuid::new_v4());
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "memory".to_string(),
            args: serde_json::json!({
                "action": "get",
                "key": "answer",
                "namespace": namespace,
                "prefix": "",
            }),
            result: serde_json::json!({ "value": "cached" }),
            duration_ms: 7,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());
        let script = format!(
            r#"
            globalThis.__memoryValue = chidori.memory("get", "answer", null, {{ namespace: "{namespace}" }});
            "#
        );

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(script).unwrap();
            let value: String = ctx.eval(r#"globalThis.__memoryValue.value"#).unwrap();
            assert_eq!(value, "cached");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_records_template_call() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__rendered = chidori.template("Hello {{ name }}!", { name: "TS" })"#,
            )
            .unwrap();
            let value: String = ctx.eval("globalThis.__rendered").unwrap();
            assert_eq!(value, "Hello TS!");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "template");
        assert_eq!(records[0].result, serde_json::json!("Hello TS!"));
    }

    #[test]
    fn runtime_binding_replays_template_without_rendering() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "template".to_string(),
            args: serde_json::json!({
                "template": "ignored {{ name",
                "vars": { "name": "live" },
            }),
            result: serde_json::json!("cached"),
            duration_ms: 7,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__rendered = chidori.template("ignored {{ name", { name: "live" })"#,
            )
            .unwrap();
            let value: String = ctx.eval("globalThis.__rendered").unwrap();
            assert_eq!(value, "cached");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_replays_http_without_network() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "http".to_string(),
            args: serde_json::json!({
                "url": "https://example.invalid/live",
                "method": "POST",
                "headers": null,
                "body": { "ignored": true },
                "params": null,
            }),
            result: serde_json::json!({
                "status": 202,
                "headers": { "content-type": "application/json" },
                "body": { "cached": true },
            }),
            duration_ms: 9,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__httpResponse = chidori.http(
                    "https://example.invalid/live",
                    { method: "POST", body: { ignored: true } },
                );
                "#,
            )
            .unwrap();
            let status: i32 = ctx.eval("globalThis.__httpResponse.status").unwrap();
            let cached: bool = ctx.eval("globalThis.__httpResponse.body.cached").unwrap();
            assert_eq!(status, 202);
            assert!(cached);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
        assert_eq!(records[0].duration_ms, 9);
    }

    #[test]
    fn runtime_binding_accepts_http_options_object() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "http".to_string(),
            args: serde_json::json!({
                "url": "https://example.invalid/live",
                "method": "GET",
                "headers": { "accept": "application/json" },
                "body": null,
                "params": { "q": "chidori" },
            }),
            result: serde_json::json!({
                "status": 200,
                "headers": {},
                "body": { "ok": true },
            }),
            duration_ms: 3,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__httpResponse = chidori.http({
                    url: "https://example.invalid/live",
                    method: "GET",
                    headers: { accept: "application/json" },
                    params: { q: "chidori" },
                });
                "#,
            )
            .unwrap();
            let ok: bool = ctx.eval("globalThis.__httpResponse.body.ok").unwrap();
            assert!(ok);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].args, replay[0].args);
    }

    #[test]
    fn runtime_binding_replays_input_answer() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "input".to_string(),
            args: serde_json::json!({ "prompt": "Approve?" }),
            result: serde_json::json!("yes"),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"globalThis.__inputAnswer = chidori.input("Approve?")"#)
                .unwrap();
            let answer: String = ctx.eval("globalThis.__inputAnswer").unwrap();
            assert_eq!(answer, "yes");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_input_pause_sets_pending_input() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        runtime_ctx.set_input_mode(InputMode::Pause);

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            let err = ctx
                .eval::<(), _>(r#"chidori.input("Approve?")"#)
                .unwrap_err();
            assert!(matches!(err, rquickjs::Error::Exception));
        });

        let pending = runtime_ctx.take_pending_input().unwrap();
        assert_eq!(pending.seq, 1);
        assert_eq!(pending.prompt, "Approve?");
        assert!(runtime_ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn runtime_binding_replays_sandbox_helpers_without_execution() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![
            CallRecord {
                seq: 1,
                parent_seq: None,
                function: "exec_js".to_string(),
                args: serde_json::json!({ "source": "throw new Error('live')", "fuel": 100 }),
                result: serde_json::json!("cached-js"),
                duration_ms: 3,
                token_usage: None,
                timestamp: Utc::now(),
                error: None,
            },
            CallRecord {
                seq: 2,
                parent_seq: None,
                function: "exec_python".to_string(),
                args: serde_json::json!({ "source": "raise Exception('live')", "fuel": 100 }),
                result: serde_json::json!("cached-python"),
                duration_ms: 4,
                token_usage: None,
                timestamp: Utc::now(),
                error: None,
            },
        ];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__sandboxResults = {
                    js: chidori.execJs("throw new Error('live')", { fuel: 100 }),
                    python: chidori.execPython("raise Exception('live')", { fuel: 100 }),
                };
                "#,
            )
            .unwrap();
            let js: String = ctx.eval("globalThis.__sandboxResults.js").unwrap();
            let python: String = ctx.eval("globalThis.__sandboxResults.python").unwrap();
            assert_eq!(js, "cached-js");
            assert_eq!(python, "cached-python");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].result, replay[0].result);
        assert_eq!(records[1].result, replay[1].result);
    }

    #[test]
    fn runtime_binding_executes_wasm_and_records_result() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__wasmResult = chidori.execWasm(`
                    (module
                        (func $add (export "add") (param i32 i32) (result i32)
                            local.get 0
                            local.get 1
                            i32.add)
                    )
                `, { function: "add", args: [2, 3], fuel: 1000000, memoryPages: 1 });
                "#,
            )
            .unwrap();
            let value: i32 = ctx.eval("globalThis.__wasmResult.returns[0]").unwrap();
            assert_eq!(value, 5);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "exec");
        assert_eq!(records[0].result["returns"], serde_json::json!([5]));
    }

    #[test]
    fn runtime_binding_replays_wasm_without_execution() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "exec".to_string(),
            args: serde_json::json!({
                "function": "missing",
                "args": [],
                "fuel": 1,
                "memory_pages": 1,
            }),
            result: serde_json::json!({
                "returns": [42],
                "fuel_remaining": 0,
            }),
            duration_ms: 5,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__wasmResult = chidori.execWasm("not wasm", { function: "missing", fuel: 1, memoryPages: 1 })"#,
            )
            .unwrap();
            let value: i32 = ctx.eval("globalThis.__wasmResult.returns[0]").unwrap();
            assert_eq!(value, 42);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_replays_call_agent_without_reading_file() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "call_agent".to_string(),
            args: serde_json::json!({
                "path": "/tmp/missing-child.ts",
                "input": { "value": 1 },
            }),
            result: serde_json::json!({ "child": 2 }),
            duration_ms: 6,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__childResult = chidori.callAgent(
                    "/tmp/missing-child.ts",
                    { value: 1 },
                );
                "#,
            )
            .unwrap();
            let value: i32 = ctx.eval("globalThis.__childResult.child").unwrap();
            assert_eq!(value, 2);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_invokes_typescript_tool_and_records_result() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-ts-tool-run-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &tool_path,
            r#"
            export const tool = {
              name: "echo",
              description: "Echo a value",
              parameters: {
                type: "object",
                properties: { value: { type: "number" } },
                required: ["value"],
              },
            };

            export async function run(args, chidori) {
              return { value: args.value + 1 };
            }
            "#,
        )
        .unwrap();

        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: Vec::new(),
            source_path: tool_path,
            source_fingerprint: None,
            backend: crate::tools::ToolBackend::TypeScript,
        });
        let registry = Arc::new(registry);

        context.with(|ctx| {
            let chidori =
                install_runtime_chidori_with_tools(&ctx, runtime_ctx.clone(), registry.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"globalThis.__toolResult = chidori.tool("echo", { value: 41 })"#)
                .unwrap();
            let value: i32 = ctx.eval("globalThis.__toolResult.value").unwrap();
            assert_eq!(value, 42);
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "tool");
        assert_eq!(records[0].result, serde_json::json!({ "value": 42 }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_binding_prompt_tool_loop_invokes_registered_tool() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-prompt-tool-loop-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &tool_path,
            r#"
            export const tool = {
              name: "echo",
              description: "Echo a value",
              parameters: {
                type: "object",
                properties: { value: { type: "number" } },
                required: ["value"],
              },
            };

            export async function run(args, chidori) {
              return { value: args.value + 1 };
            }
            "#,
        )
        .unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: vec![crate::tools::ToolParam {
                name: "value".to_string(),
                description: None,
                param_type: "number".to_string(),
                default: None,
                required: true,
            }],
            source_path: tool_path,
            source_fingerprint: None,
            backend: crate::tools::ToolBackend::TypeScript,
        });
        let tools = Arc::new(registry);
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ToolUseProvider {
            calls: AtomicUsize::new(0),
        }));
        let providers = Arc::new(providers);

        context.with(|ctx| {
            let (tokio_rt, policy, policy_cache) = runtime_host();
            let chidori = install_chidori_object_for_runtime(
                &ctx,
                runtime_ctx.clone(),
                providers,
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                RuntimePolicy::durable_default("ts-binding-test"),
                tools,
                Arc::new(McpManager::new()),
            )
            .unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__promptResponse = chidori.prompt("use tool", { tools: ["echo"] })"#,
            )
            .unwrap();
            let response: String = ctx.eval("globalThis.__promptResponse").unwrap();
            assert_eq!(response, "final answer");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].function, "prompt");
        assert_eq!(records[1].function, "tool");
        assert_eq!(records[1].result, serde_json::json!({ "value": 42 }));
        assert_eq!(records[2].function, "prompt");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_binding_prompt_tool_loop_honors_max_turns_option() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-prompt-tool-max-turns-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &tool_path,
            r#"
            export async function run(args, chidori) {
              return { value: args.value + 1 };
            }
            "#,
        )
        .unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: vec![crate::tools::ToolParam {
                name: "value".to_string(),
                description: None,
                param_type: "number".to_string(),
                default: None,
                required: true,
            }],
            source_path: tool_path,
            source_fingerprint: None,
            backend: crate::tools::ToolBackend::TypeScript,
        });
        let tools = Arc::new(registry);
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ToolUseProvider {
            calls: AtomicUsize::new(0),
        }));
        let providers = Arc::new(providers);

        context.with(|ctx| {
            let (tokio_rt, policy, policy_cache) = runtime_host();
            let chidori = install_chidori_object_for_runtime(
                &ctx,
                runtime_ctx.clone(),
                providers,
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                RuntimePolicy::durable_default("ts-binding-test"),
                tools,
                Arc::new(McpManager::new()),
            )
            .unwrap();
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(
                r#"globalThis.__promptResponse = chidori.prompt("use tool", { tools: ["echo"], maxTurns: 1 })"#,
            )
            .unwrap();
            let response: String = ctx.eval("globalThis.__promptResponse").unwrap();
            assert_eq!(response, "");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].function, "prompt");
        assert_eq!(records[0].args["max_turns"], serde_json::json!(1));
        assert_eq!(records[1].function, "tool");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_binding_replays_tool_without_registry_lookup() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "tool".to_string(),
            args: serde_json::json!({
                "name": "missing",
                "kwargs": { "value": 1 },
            }),
            result: serde_json::json!({ "value": "cached" }),
            duration_ms: 5,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"globalThis.__toolResult = chidori.tool("missing", { value: 1 })"#)
                .unwrap();
            let value: String = ctx.eval("globalThis.__toolResult.value").unwrap();
            assert_eq!(value, "cached");
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].result, replay[0].result);
    }

    #[test]
    fn runtime_binding_denies_http_by_policy_and_records_error() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let runtime_ctx = RuntimeContext::new();
        let policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: Decision::NeverAllow,
                match_args: None,
                reason: Some("test deny".to_string()),
            }],
            default: Decision::AlwaysAllow,
        });

        context.with(|ctx| {
            let chidori =
                install_runtime_chidori_with_policy(&ctx, runtime_ctx.clone(), policy.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            let err = ctx
                .eval::<(), _>(r#"chidori.http("https://example.invalid")"#)
                .unwrap_err();
            assert!(matches!(err, rquickjs::Error::Exception));
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "http");
        assert_eq!(records[0].result, serde_json::Value::Null);
        assert_eq!(
            records[0].error.as_deref(),
            Some("policy: `http` denied (test deny)")
        );
    }

    #[test]
    fn runtime_binding_replays_matching_log_call() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "log".to_string(),
            args: serde_json::json!({ "message": "cached" }),
            result: serde_json::Value::Null,
            duration_ms: 7,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx.clone());
            ctx.globals().set("chidori", chidori).unwrap();
            ctx.eval::<(), _>(r#"chidori.log("live")"#).unwrap();
        });

        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].args, replay[0].args);
        assert_eq!(records[0].duration_ms, 7);
    }

    #[test]
    fn runtime_binding_rejects_replay_divergence() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "checkpoint".to_string(),
            args: serde_json::json!({ "label": "cached", "data": null }),
            result: serde_json::Value::Null,
            duration_ms: 7,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay);

        context.with(|ctx| {
            let chidori = install_runtime_chidori(&ctx, runtime_ctx);
            ctx.globals().set("chidori", chidori).unwrap();
            let err = ctx.eval::<(), _>(r#"chidori.log("live")"#).unwrap_err();
            assert!(matches!(err, rquickjs::Error::Exception));
        });
    }
}
