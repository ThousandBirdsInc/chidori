use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, Any as CorsAny, CorsLayer};

use crate::acp::{self, AcpState};
use crate::mcp::{McpManager, McpServersConfig};
use crate::policy::{Decision, PolicyConfig};
use crate::providers::{
    ContentBlock, LlmRequest, Message as LlmMessage, ProviderRegistry, ToolSchema,
};
use crate::recipes::Recipe;
use crate::runtime::call_log::{CallRecord, TokenUsage};
use crate::runtime::context::{
    InputMode, PendingApproval, RuntimeContext, RuntimeEvent, PAUSE_MARKER,
};
use crate::runtime::engine::Engine;
use crate::runtime::snapshot::{
    HostOperationId, HostPromiseRecord, HostPromiseState, PendingHostOperation,
    PendingHostOperationKind, RuntimePolicy, SnapshotAbi, SnapshotBlobKind, SnapshotStore,
    SourceFingerprint, HOST_PROMISE_TABLE_FILE, PENDING_HOST_OPERATION_FILE,
};
use crate::runtime::template::TemplateEngine;
use crate::runtime::typescript::engine::TypeScriptVmRuntime;
use crate::scheduler::{self, SchedulerDeps};
use crate::storage::{build_session_store, SessionStatus, SessionStore, StoredSession};
use crate::tools::{ToolBackend, ToolDef, ToolRegistry};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    #[allow(dead_code)]
    providers: Arc<ProviderRegistry>,
    template_engine: Arc<TemplateEngine>,
    agent_path: PathBuf,
    run_base: PathBuf,
    session_store: Arc<dyn SessionStore>,
    policy: Arc<PolicyConfig>,
    mcp: Arc<McpManager>,
    mcp_tools: Arc<Vec<ToolDef>>,
    recipes: Arc<Vec<Recipe>>,
    /// Caps the number of agent runs executing concurrently.
    run_semaphore: Arc<Semaphore>,
    acquire_timeout: std::time::Duration,
    active_sessions: Arc<StdMutex<HashMap<String, ActiveSession>>>,
}

const PROMPT_TOOL_PAUSE_FILE: &str = "prompt_tool_pause.json";

#[derive(Clone)]
struct ActiveSession {
    cancelled: Arc<AtomicBool>,
    cancel_tx: tokio::sync::mpsc::UnboundedSender<String>,
    attempt_number: Option<u64>,
}

/// Render a StoredSession into the JSON shape historical clients expect.
fn session_view(s: &StoredSession) -> Value {
    json!({
        "id": s.id,
        "run_id": s.run_id,
        "status": s.status,
        "input": s.input,
        "output": s.output,
        "error": s.error,
        "call_count": s.call_log.len(),
        "pending_seq": s.pending_seq,
        "pending_prompt": s.pending_prompt,
        "pending_approval": s.pending_approval,
    })
}

fn store_or_500(store: &Arc<dyn SessionStore>, session: &StoredSession) -> Option<Response> {
    if let Err(e) = store.put(session) {
        return Some(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("session store: {}", e)})),
            )
                .into_response(),
        );
    }
    None
}

fn is_supported_agent_path(path: &std::path::Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("ts")
}

fn is_supported_agent_filename(name: &str) -> bool {
    let path = std::path::Path::new(name);
    !name.is_empty()
        && name.len() < 128
        && is_supported_agent_path(path)
        && path.file_name().and_then(|s| s.to_str()) == Some(name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn snapshot_manifest_for_session(app: &AppState, session: &StoredSession) -> Option<Value> {
    let run_id = session.run_id.as_ref()?;
    let store = SnapshotStore::new(app.run_base.join(run_id));
    let manifest = store.load_manifest().ok()?;
    serde_json::to_value(manifest).ok()
}

fn validate_snapshot_manifest_for_resume(
    run_base: &FsPath,
    run_id: Option<&str>,
    agent_path: &FsPath,
) -> anyhow::Result<()> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let store = SnapshotStore::new(run_base.join(run_id));
    let manifest = match store.load_manifest() {
        Ok(manifest) => manifest,
        Err(_) => return Ok(()),
    };
    let entry_source = std::fs::read_to_string(agent_path).map_err(|err| {
        anyhow::anyhow!("reading resume source {}: {}", agent_path.display(), err)
    })?;
    let current_entry = SourceFingerprint::from_source(agent_path, &entry_source);
    let mut current_modules = Vec::with_capacity(manifest.modules.len());
    for module in &manifest.modules {
        let source = std::fs::read_to_string(&module.path).map_err(|err| {
            anyhow::anyhow!(
                "reading resume module source {}: {}",
                module.path.display(),
                err
            )
        })?;
        current_modules.push(SourceFingerprint::from_source(&module.path, &source));
    }

    let expected_abi = SnapshotAbi::current("chidori-quickjs");
    let expected_policy = RuntimePolicy::from_env_for_durable_run(run_id)?;
    let current_module_graph = if manifest.module_graph.is_empty() {
        Vec::new()
    } else {
        crate::runtime::typescript::snapshot::snapshot_module_graph(
            agent_path,
            &entry_source,
            &expected_policy,
        )?
    };
    manifest.ensure_resume_compatible(
        &expected_abi,
        &expected_policy,
        &current_entry,
        &current_modules,
        &current_module_graph,
    )
}

fn restore_live_vm_runtime_for_resume(
    run_base: &FsPath,
    run_id: Option<&str>,
    agent_path: &FsPath,
) -> anyhow::Result<Option<chidori_quickjs::SnapshotRuntime>> {
    let Some(run_id) = run_id else {
        return Ok(None);
    };
    let store = SnapshotStore::new(run_base.join(run_id));
    let manifest = match store.load_manifest() {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if manifest.snapshot_kind != SnapshotBlobKind::LiveQuickJsVm {
        return Ok(None);
    }

    let entry_source = std::fs::read_to_string(agent_path).map_err(|err| {
        anyhow::anyhow!("reading resume source {}: {}", agent_path.display(), err)
    })?;
    let current_entry = SourceFingerprint::from_source(agent_path, &entry_source);
    let mut current_modules = Vec::with_capacity(manifest.modules.len());
    for module in &manifest.modules {
        let source = std::fs::read_to_string(&module.path).map_err(|err| {
            anyhow::anyhow!(
                "reading resume module source {}: {}",
                module.path.display(),
                err
            )
        })?;
        current_modules.push(SourceFingerprint::from_source(&module.path, &source));
    }

    let expected_policy = RuntimePolicy::from_env_for_durable_run(run_id)?;
    let current_module_graph = if manifest.module_graph.is_empty() {
        Vec::new()
    } else {
        crate::runtime::typescript::snapshot::snapshot_module_graph(
            agent_path,
            &entry_source,
            &expected_policy,
        )?
    };
    let snapshot = store.load_live_vm_for_resume(
        &SnapshotAbi::current("chidori-quickjs"),
        &expected_policy,
        &current_entry,
        &current_modules,
        &current_module_graph,
    )?;
    let runtime = chidori_quickjs::SnapshotRuntime::restore(&snapshot.blob)
        .map_err(|err| anyhow::anyhow!(err))?;
    Ok(Some(runtime))
}

fn try_resume_completed_live_vm_input(
    run_base: &FsPath,
    run_id: Option<&str>,
    agent_path: &FsPath,
    pending: &PendingHostOperation,
    response: &str,
    call_log: &mut Vec<CallRecord>,
    state: &AppState,
) -> anyhow::Result<Option<LiveVmResumeOutcome>> {
    let Some(mut runtime) = restore_live_vm_runtime_for_resume(run_base, run_id, agent_path)?
    else {
        return Ok(None);
    };
    let resume_snapshot = runtime.snapshot().map_err(|err| anyhow::anyhow!(err))?;
    runtime
        .resolve_host_promise(
            chidori_quickjs::HostPromiseId(pending.id.0),
            Value::String(response.to_string()),
        )
        .map_err(|err| anyhow::anyhow!(err))?;

    drive_live_vm_runtime_with_pause_snapshot(
        runtime,
        run_id,
        call_log,
        state,
        Some(resume_snapshot),
    )
}

fn try_resume_completed_nested_runtime_from_live_vm_input(
    run_base: &FsPath,
    run_id: Option<&str>,
    agent_path: &FsPath,
    pending: &PendingHostOperation,
    call_log: &mut Vec<CallRecord>,
    state: &AppState,
) -> anyhow::Result<Option<LiveVmResumeOutcome>> {
    if pending.kind != PendingHostOperationKind::Input {
        return Ok(None);
    }
    let host_promises = load_persisted_host_promises(run_base, run_id)?;
    let Some(parent_record) = host_promises
        .iter()
        .find(|record| {
            matches!(
                record.operation.kind,
                PendingHostOperationKind::Tool | PendingHostOperationKind::CallAgent
            ) && matches!(record.state, HostPromiseState::Pending)
        })
        .cloned()
    else {
        return Ok(None);
    };
    let Some(mut runtime) = restore_live_vm_runtime_for_resume(run_base, run_id, agent_path)?
    else {
        return Ok(None);
    };
    let args = parent_record.operation.args.clone();
    let function = match parent_record.operation.kind {
        PendingHostOperationKind::Tool => "tool",
        PendingHostOperationKind::CallAgent => "call_agent",
        _ => return Ok(None),
    };
    let execution = match parent_record.operation.kind {
        PendingHostOperationKind::Tool => {
            execute_live_vm_tool_with_host_promises(state, run_id, &args, host_promises.clone())
        }
        PendingHostOperationKind::CallAgent => execute_live_vm_call_agent_with_host_promises(
            state,
            run_id,
            &args,
            host_promises.clone(),
        ),
        _ => unreachable!("parent operation kind was checked above"),
    };
    match execution {
        Ok(value) => {
            if !advance_live_vm_runtime_to_host_promise(
                &mut runtime,
                parent_record.operation.id,
                &host_promises,
            )? {
                return Ok(None);
            }
            push_live_vm_call_record(call_log, function, args, value.clone());
            complete_persisted_host_promise_record(
                run_base,
                run_id,
                parent_record.operation.id,
                HostPromiseCompletion::Resolved(value.clone()),
            )?;
            runtime
                .resolve_host_promise(
                    chidori_quickjs::HostPromiseId(parent_record.operation.id.0),
                    value,
                )
                .map_err(|err| anyhow::anyhow!(err))?;
        }
        Err(err) => {
            let message = err.to_string();
            if !advance_live_vm_runtime_to_host_promise(
                &mut runtime,
                parent_record.operation.id,
                &host_promises,
            )? {
                return Ok(None);
            }
            push_live_vm_error_record(call_log, function, args, message.clone());
            complete_persisted_host_promise_record(
                run_base,
                run_id,
                parent_record.operation.id,
                HostPromiseCompletion::Rejected(message.clone()),
            )?;
            runtime
                .reject_host_promise(
                    chidori_quickjs::HostPromiseId(parent_record.operation.id.0),
                    format!("Error: {message}"),
                )
                .map_err(|err| anyhow::anyhow!(err))?;
        }
    }

    drive_live_vm_runtime(runtime, run_id, call_log, state)
}

fn try_resume_completed_prompt_tool_from_live_vm_input(
    run_base: &FsPath,
    run_id: Option<&str>,
    agent_path: &FsPath,
    pending: &PendingHostOperation,
    call_log: &mut Vec<CallRecord>,
    state: &AppState,
) -> anyhow::Result<Option<LiveVmResumeOutcome>> {
    if pending.kind != PendingHostOperationKind::Input {
        return Ok(None);
    }
    let Some(prompt_pause) = load_prompt_tool_pause(run_base, run_id)? else {
        return Ok(None);
    };
    let host_promises = load_persisted_host_promises(run_base, run_id)?;
    let Some(mut runtime) = restore_live_vm_runtime_for_resume(run_base, run_id, agent_path)?
    else {
        return Ok(None);
    };
    let parent_prompt_id = prompt_pause.parent_prompt_id;

    let tool_args = prompt_pause.tool_args.clone();
    let tool_result_for_prompt =
        match execute_live_vm_tool_with_host_promises(state, run_id, &tool_args, host_promises) {
            Ok(value) => {
                push_live_vm_call_record(call_log, "tool", tool_args, value.clone());
                Ok(value)
            }
            Err(err) => {
                let message = err.to_string();
                push_live_vm_error_record(call_log, "tool", tool_args, message.clone());
                Err(message)
            }
        };

    match continue_live_vm_prompt_after_tool(state, run_id, prompt_pause, tool_result_for_prompt)? {
        LiveVmPromptExecution::Completed(result) => {
            for record in result.records {
                push_live_vm_call_record_with_usage(
                    call_log,
                    record.function,
                    record.args,
                    record.result,
                    record.token_usage,
                    record.error,
                );
            }
            remove_prompt_tool_pause(run_base, run_id);
            complete_persisted_host_promise_record(
                run_base,
                run_id,
                parent_prompt_id,
                HostPromiseCompletion::Resolved(result.js_result.clone()),
            )
            .ok();
            runtime
                .resolve_host_promise(
                    chidori_quickjs::HostPromiseId(parent_prompt_id.0),
                    result.js_result,
                )
                .map_err(|err| anyhow::anyhow!(err))?;
            drive_live_vm_runtime(runtime, run_id, call_log, state)
        }
        LiveVmPromptExecution::Paused {
            mut pending,
            host_promises,
            mut prompt_pause,
            records,
        } => {
            for record in records {
                push_live_vm_call_record_with_usage(
                    call_log,
                    record.function,
                    record.args,
                    record.result,
                    record.token_usage,
                    record.error,
                );
            }
            let snapshot = runtime.snapshot().map_err(|err| anyhow::anyhow!(err))?;
            pending.seq = call_log
                .iter()
                .map(|record| record.seq)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            prompt_pause.parent_prompt_id = parent_prompt_id;
            pending =
                persist_prompt_tool_pause(state, run_id, &prompt_pause, pending, host_promises)?;
            Ok(Some(LiveVmResumeOutcome::Paused { pending, snapshot }))
        }
    }
}

fn try_resume_approved_live_vm_http(
    run_base: &FsPath,
    run_id: Option<&str>,
    agent_path: &FsPath,
    pending: &PendingHostOperation,
    call_log: &mut Vec<CallRecord>,
    state: &AppState,
) -> anyhow::Result<Option<LiveVmResumeOutcome>> {
    if pending.kind != PendingHostOperationKind::Http {
        return Ok(None);
    }
    let Some(mut runtime) = restore_live_vm_runtime_for_resume(run_base, run_id, agent_path)?
    else {
        return Ok(None);
    };
    let args = pending.args.clone();
    match execute_live_vm_http(&args) {
        Ok(value) => {
            push_live_vm_call_record(call_log, "http", args, value.clone());
            complete_persisted_pending_host_operation(
                run_base,
                run_id,
                Some((pending.seq, PendingHostOperationKind::Http)),
                HostPromiseCompletion::Resolved(value.clone()),
            )?;
            runtime
                .resolve_host_promise(chidori_quickjs::HostPromiseId(pending.id.0), value)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
        Err(err) => {
            let message = err.to_string();
            push_live_vm_error_record(call_log, "http", args, message.clone());
            complete_persisted_pending_host_operation(
                run_base,
                run_id,
                Some((pending.seq, PendingHostOperationKind::Http)),
                HostPromiseCompletion::Rejected(message.clone()),
            )?;
            runtime
                .reject_host_promise(chidori_quickjs::HostPromiseId(pending.id.0), message)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
    }

    drive_live_vm_runtime(runtime, run_id, call_log, state)
}

fn drive_live_vm_runtime(
    runtime: chidori_quickjs::SnapshotRuntime,
    run_id: Option<&str>,
    call_log: &mut Vec<CallRecord>,
    state: &AppState,
) -> anyhow::Result<Option<LiveVmResumeOutcome>> {
    drive_live_vm_runtime_with_pause_snapshot(runtime, run_id, call_log, state, None)
}

fn drive_live_vm_runtime_with_pause_snapshot(
    mut runtime: chidori_quickjs::SnapshotRuntime,
    run_id: Option<&str>,
    call_log: &mut Vec<CallRecord>,
    state: &AppState,
    pause_snapshot_fallback: Option<chidori_quickjs::RuntimeSnapshot>,
) -> anyhow::Result<Option<LiveVmResumeOutcome>> {
    loop {
        match runtime
            .run_jobs_until_blocked()
            .map_err(|err| anyhow::anyhow!(err))?
        {
            chidori_quickjs::RunState::Completed(value) => {
                return Ok(Some(LiveVmResumeOutcome::Completed(value)));
            }
            chidori_quickjs::RunState::BlockedOnHostOperation(id) => {
                let id = HostOperationId(id.0);
                let Some(call) = live_vm_host_call(&mut runtime, id)? else {
                    return Ok(None);
                };
                match call.get("method").and_then(Value::as_str) {
                    Some("input") => {
                        let Some(next_pending) = pending_input_from_live_vm_host_call(id, &call)
                        else {
                            return Ok(None);
                        };
                        let snapshot = runtime.snapshot().map_err(|err| anyhow::anyhow!(err))?;
                        return Ok(Some(LiveVmResumeOutcome::Paused {
                            pending: next_pending,
                            snapshot,
                        }));
                    }
                    Some("log") => {
                        let args = log_args_from_live_vm_host_call(&call)?;
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            "log",
                            args.clone(),
                            crate::runtime::host_core::execute_log(&args),
                        )?;
                    }
                    Some("template") => {
                        let args = template_args_from_live_vm_host_call(&call)?;
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            "template",
                            args.clone(),
                            crate::runtime::host_core::execute_template(
                                &state.template_engine,
                                &args,
                            ),
                        )?;
                    }
                    Some("memory") => {
                        let args = memory_args_from_live_vm_host_call(&call)?;
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            "memory",
                            args.clone(),
                            crate::runtime::host_core::execute_memory(&args),
                        )?;
                    }
                    Some("checkpoint") => {
                        let args = checkpoint_args_from_live_vm_host_call(&call)?;
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            "checkpoint",
                            args,
                            Ok(Value::Null),
                        )?;
                    }
                    Some("execJs") | Some("execPython") => {
                        let method =
                            call.get("method").and_then(Value::as_str).ok_or_else(|| {
                                anyhow::anyhow!("live VM sandbox call is missing method")
                            })?;
                        let function = match method {
                            "execJs" => "exec_js",
                            "execPython" => "exec_python",
                            _ => unreachable!("sandbox method match should be exhaustive"),
                        };
                        let args = sandbox_string_args_from_live_vm_host_call(&call)?;
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            function,
                            args.clone(),
                            crate::runtime::host_core::execute_sandbox_string(function, &args),
                        )?;
                    }
                    Some("execWasm") => {
                        let args = sandbox_wasm_args_from_live_vm_host_call(&call)?;
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            "exec",
                            args.clone(),
                            crate::runtime::host_core::execute_sandbox_wasm(&args),
                        )?;
                    }
                    Some("http") => {
                        let args = http_args_from_live_vm_host_call(&call)?;
                        let result = match live_vm_http_policy_decision(&state.policy, &args) {
                            LiveVmPolicyDecision::Allow => execute_live_vm_http(&args),
                            LiveVmPolicyDecision::Ask { approval } => {
                                let snapshot =
                                    runtime.snapshot().map_err(|err| anyhow::anyhow!(err))?;
                                return Ok(Some(LiveVmResumeOutcome::AwaitingApproval {
                                    pending: PendingHostOperation::new(
                                        id,
                                        0,
                                        PendingHostOperationKind::Http,
                                        args,
                                    ),
                                    approval,
                                    snapshot,
                                }));
                            }
                            LiveVmPolicyDecision::Deny { error } => Err(anyhow::anyhow!(error)),
                        };
                        complete_live_vm_host_result(
                            &mut runtime,
                            id,
                            call_log,
                            "http",
                            args,
                            result,
                        )?;
                    }
                    Some("tool") => {
                        let args = tool_args_from_live_vm_host_call(&call)?;
                        match live_vm_tool_requires_approval(&state.policy, &args) {
                            Ok(true) => return Ok(None),
                            Ok(false) => match execute_live_vm_tool_stateful(
                                state,
                                run_id,
                                &args,
                                Vec::new(),
                            )? {
                                LiveVmToolExecution::Completed(value) => {
                                    complete_live_vm_host_result(
                                        &mut runtime,
                                        id,
                                        call_log,
                                        "tool",
                                        args,
                                        Ok(value),
                                    )?;
                                }
                                LiveVmToolExecution::Paused {
                                    mut pending,
                                    host_promises,
                                } => {
                                    let snapshot =
                                        runtime.snapshot().map_err(|err| anyhow::anyhow!(err))?;
                                    pending.seq = call_log
                                        .iter()
                                        .map(|record| record.seq)
                                        .max()
                                        .unwrap_or(0)
                                        .saturating_add(1);
                                    pending = persist_nested_live_vm_tool_pause(
                                        state,
                                        run_id,
                                        id,
                                        args,
                                        pending,
                                        host_promises,
                                    )?;
                                    return Ok(Some(LiveVmResumeOutcome::Paused {
                                        pending,
                                        snapshot,
                                    }));
                                }
                            },
                            Err(err) => {
                                complete_live_vm_host_result(
                                    &mut runtime,
                                    id,
                                    call_log,
                                    "tool",
                                    args,
                                    Err(err),
                                )?;
                            }
                        }
                    }
                    Some("callAgent") => {
                        let args = call_agent_args_from_live_vm_host_call(&call)?;
                        let paused_snapshot = match runtime.snapshot() {
                            Ok(snapshot) => snapshot,
                            Err(err) => match pause_snapshot_fallback.clone() {
                                Some(snapshot) => snapshot,
                                None => return Err(anyhow::anyhow!(err)),
                            },
                        };
                        match execute_live_vm_call_agent_stateful(state, run_id, &args, Vec::new())?
                        {
                            LiveVmChildAgentExecution::Completed(value) => {
                                complete_live_vm_host_result(
                                    &mut runtime,
                                    id,
                                    call_log,
                                    "call_agent",
                                    args,
                                    Ok(value),
                                )?;
                            }
                            LiveVmChildAgentExecution::Paused {
                                mut pending,
                                host_promises,
                            } => {
                                pending.seq = call_log
                                    .iter()
                                    .map(|record| record.seq)
                                    .max()
                                    .unwrap_or(0)
                                    .saturating_add(1);
                                pending = persist_nested_live_vm_runtime_pause(
                                    state,
                                    run_id,
                                    id,
                                    PendingHostOperationKind::CallAgent,
                                    args,
                                    pending,
                                    host_promises,
                                )?;
                                return Ok(Some(LiveVmResumeOutcome::Paused {
                                    pending,
                                    snapshot: paused_snapshot,
                                }));
                            }
                        }
                    }
                    Some("prompt") => {
                        let prompt = prompt_call_from_live_vm_host_call(&call)?;
                        match execute_live_vm_prompt(state, run_id, prompt)? {
                            LiveVmPromptExecution::Completed(result) => {
                                complete_live_vm_prompt_result(
                                    &mut runtime,
                                    id,
                                    call_log,
                                    Ok(result),
                                )?;
                            }
                            LiveVmPromptExecution::Paused {
                                mut pending,
                                host_promises,
                                mut prompt_pause,
                                records,
                            } => {
                                for record in records {
                                    push_live_vm_call_record_with_usage(
                                        call_log,
                                        record.function,
                                        record.args,
                                        record.result,
                                        record.token_usage,
                                        record.error,
                                    );
                                }
                                let snapshot =
                                    runtime.snapshot().map_err(|err| anyhow::anyhow!(err))?;
                                pending.seq = call_log
                                    .iter()
                                    .map(|record| record.seq)
                                    .max()
                                    .unwrap_or(0)
                                    .saturating_add(1);
                                prompt_pause.parent_prompt_id = id;
                                pending = persist_prompt_tool_pause(
                                    state,
                                    run_id,
                                    &prompt_pause,
                                    pending,
                                    host_promises,
                                )?;
                                return Ok(Some(LiveVmResumeOutcome::Paused { pending, snapshot }));
                            }
                        }
                    }
                    _ => return Ok(None),
                }
            }
        }
    }
}

fn advance_live_vm_runtime_to_host_promise(
    runtime: &mut chidori_quickjs::SnapshotRuntime,
    target: HostOperationId,
    host_promises: &[HostPromiseRecord],
) -> anyhow::Result<bool> {
    loop {
        match runtime
            .run_jobs_until_blocked()
            .map_err(|err| anyhow::anyhow!(err))?
        {
            chidori_quickjs::RunState::Completed(_) => return Ok(false),
            chidori_quickjs::RunState::BlockedOnHostOperation(id) => {
                let id = HostOperationId(id.0);
                if id == target {
                    return Ok(true);
                }
                let Some(record) = host_promises
                    .iter()
                    .find(|record| record.operation.id == id)
                else {
                    return Ok(false);
                };
                match &record.state {
                    HostPromiseState::Resolved { value, .. } => {
                        runtime
                            .resolve_host_promise(
                                chidori_quickjs::HostPromiseId(id.0),
                                value.clone(),
                            )
                            .map_err(|err| anyhow::anyhow!(err))?;
                    }
                    HostPromiseState::Rejected { error, .. } => {
                        runtime
                            .reject_host_promise(
                                chidori_quickjs::HostPromiseId(id.0),
                                error.clone(),
                            )
                            .map_err(|err| anyhow::anyhow!(err))?;
                    }
                    HostPromiseState::Pending => return Ok(false),
                }
            }
        }
    }
}

enum LiveVmResumeOutcome {
    Completed(Value),
    Paused {
        pending: PendingHostOperation,
        snapshot: chidori_quickjs::RuntimeSnapshot,
    },
    AwaitingApproval {
        pending: PendingHostOperation,
        approval: PendingApproval,
        snapshot: chidori_quickjs::RuntimeSnapshot,
    },
}

fn live_vm_host_call(
    runtime: &mut chidori_quickjs::SnapshotRuntime,
    id: HostOperationId,
) -> anyhow::Result<Option<Value>> {
    let Some(calls) = runtime
        .restored_global_json("__chidori_host_calls")
        .map_err(|err| anyhow::anyhow!(err))?
    else {
        return Ok(None);
    };
    let Some(calls) = calls.as_array() else {
        return Ok(None);
    };
    Ok(calls
        .iter()
        .rev()
        .find(|call| call.get("id").and_then(Value::as_u64) == Some(id.0))
        .cloned())
}

fn pending_input_from_live_vm_host_call(
    id: HostOperationId,
    call: &Value,
) -> Option<PendingHostOperation> {
    if call.get("method").and_then(Value::as_str) != Some("input") {
        return None;
    }
    let prompt = call
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.first())
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(PendingHostOperation::new(
        id,
        0,
        PendingHostOperationKind::Input,
        json!({ "prompt": prompt }),
    ))
}

fn log_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM log host call is missing args"))?;
    let message = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM log host call is missing message"))?;
    Ok(json!({ "message": message }))
}

fn template_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM template host call is missing args"))?;
    let template = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM template host call is missing template"))?;
    let vars = args.get(1).cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "template": template,
        "vars": vars,
    }))
}

fn memory_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM memory host call is missing args"))?;
    let action = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM memory host call is missing action"))?;
    let options = args.get(3).and_then(Value::as_object);
    let namespace = options
        .and_then(|options| options.get("namespace"))
        .and_then(Value::as_str)
        .unwrap_or("default");
    let prefix = options
        .and_then(|options| options.get("prefix"))
        .and_then(Value::as_str)
        .unwrap_or("");

    Ok(json!({
        "action": action,
        "key": args.get(1).cloned().unwrap_or(Value::Null),
        "namespace": namespace,
        "prefix": prefix,
        "value": args.get(2).cloned().unwrap_or(Value::Null),
    }))
}

fn sandbox_string_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM sandbox host call is missing args"))?;
    let source = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM sandbox host call is missing source"))?;
    let options = args.get(1).and_then(Value::as_object);
    let fuel = options
        .and_then(|options| options.get("fuel"))
        .and_then(Value::as_u64)
        .unwrap_or(200_000_000)
        .max(1);

    Ok(json!({
        "source": source,
        "fuel": fuel,
    }))
}

fn sandbox_wasm_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM execWasm host call is missing args"))?;
    let source = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM execWasm host call is missing source"))?;
    let options = args.get(1).and_then(Value::as_object);
    let function = options
        .and_then(|options| options.get("function"))
        .and_then(Value::as_str)
        .unwrap_or("main");
    let wasm_args = options
        .and_then(|options| options.get("args"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let fuel = options
        .and_then(|options| options.get("fuel"))
        .and_then(Value::as_u64)
        .unwrap_or(1_000_000)
        .max(1);
    let memory_pages = options
        .and_then(|options| {
            options
                .get("memoryPages")
                .or_else(|| options.get("memory_pages"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(16)
        .max(1);

    Ok(json!({
        "source": source,
        "function": function,
        "args": wasm_args,
        "fuel": fuel,
        "memory_pages": memory_pages,
    }))
}

fn http_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM http host call is missing args"))?;
    let url = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM http host call is missing url"))?;
    let options = args.get(1).and_then(Value::as_object);
    let mut method = options
        .and_then(|options| options.get("method"))
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_uppercase();
    if method.is_empty() {
        method = "GET".to_string();
    }
    let headers = options
        .and_then(|options| options.get("headers"))
        .and_then(|value| match value {
            Value::Object(map) => Some(Value::Object(map.clone())),
            _ => None,
        })
        .unwrap_or(Value::Null);
    let body = options
        .and_then(|options| options.get("body"))
        .cloned()
        .unwrap_or(Value::Null);
    let params = options
        .and_then(|options| options.get("params").or_else(|| options.get("query")))
        .and_then(|value| match value {
            Value::Object(map) => Some(Value::Object(map.clone())),
            _ => None,
        })
        .unwrap_or(Value::Null);

    Ok(json!({
        "url": url,
        "method": method,
        "headers": headers,
        "body": body,
        "params": params,
    }))
}

fn tool_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM tool host call is missing args"))?;
    let name = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM tool host call is missing name"))?;
    let kwargs = match args.get(1) {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(Value::Null) | None => json!({}),
        Some(other) => {
            anyhow::bail!("chidori.tool args must be an object, got {other}");
        }
    };

    Ok(json!({
        "name": name,
        "kwargs": kwargs,
    }))
}

fn call_agent_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM callAgent host call is missing args"))?;
    let path = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM callAgent host call is missing path"))?;
    let input = args.get(1).cloned().unwrap_or_else(|| json!({}));

    Ok(json!({
        "path": path,
        "input": input,
    }))
}

struct LiveVmPromptCall {
    request: LlmRequest,
    record_args: Value,
    format: Option<String>,
    text: String,
    model: String,
    prompt_type: Option<String>,
    tool_names: Vec<String>,
    max_turns: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedLiveVmPromptCall {
    text: String,
    model: String,
    system: Option<String>,
    temperature: f64,
    max_tokens: u64,
    format: Option<String>,
    prompt_type: Option<String>,
    tool_names: Vec<String>,
    max_turns: u64,
}

impl From<&LiveVmPromptCall> for PersistedLiveVmPromptCall {
    fn from(prompt: &LiveVmPromptCall) -> Self {
        Self {
            text: prompt.text.clone(),
            model: prompt.model.clone(),
            system: prompt.request.system.clone(),
            temperature: prompt.request.temperature,
            max_tokens: prompt.request.max_tokens,
            format: prompt.format.clone(),
            prompt_type: prompt.prompt_type.clone(),
            tool_names: prompt.tool_names.clone(),
            max_turns: prompt.max_turns,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedPromptToolPause {
    parent_prompt_id: HostOperationId,
    prompt_args: Value,
    prompt: PersistedLiveVmPromptCall,
    messages: Vec<LlmMessage>,
    next_turn: u64,
    tool_call_id: String,
    tool_args: Value,
}

struct LiveVmPromptResult {
    js_result: Value,
    records: Vec<LiveVmRecord>,
}

struct LiveVmRecord {
    function: &'static str,
    args: Value,
    result: Value,
    token_usage: Option<TokenUsage>,
    error: Option<String>,
}

enum LiveVmToolExecution {
    Completed(Value),
    Paused {
        pending: PendingHostOperation,
        host_promises: Vec<HostPromiseRecord>,
    },
}

enum LiveVmPromptExecution {
    Completed(LiveVmPromptResult),
    Paused {
        pending: PendingHostOperation,
        host_promises: Vec<HostPromiseRecord>,
        prompt_pause: PersistedPromptToolPause,
        records: Vec<LiveVmRecord>,
    },
}

enum LiveVmChildAgentExecution {
    Completed(Value),
    Paused {
        pending: PendingHostOperation,
        host_promises: Vec<HostPromiseRecord>,
    },
}

fn prompt_call_from_live_vm_host_call(call: &Value) -> anyhow::Result<LiveVmPromptCall> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM prompt host call is missing args"))?;
    let text = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("live VM prompt host call is missing text"))?
        .to_string();
    let options = args.get(1).and_then(Value::as_object);
    let config = RuntimeContext::new().config();
    let model = options
        .and_then(|options| options.get("model"))
        .and_then(Value::as_str)
        .unwrap_or(&config.model)
        .to_string();
    let temperature = options
        .and_then(|options| options.get("temperature"))
        .and_then(Value::as_f64)
        .unwrap_or(config.temperature);
    let max_tokens = options
        .and_then(|options| {
            options
                .get("maxTokens")
                .or_else(|| options.get("max_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(config.max_tokens);
    let system = options
        .and_then(|options| options.get("system"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let format = options
        .and_then(|options| options.get("format"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let prompt_type = options
        .and_then(|options| {
            options
                .get("type")
                .or_else(|| options.get("streamType"))
                .or_else(|| options.get("stream_type"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let tool_names = options
        .and_then(|options| options.get("tools"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(LiveVmPromptCall {
        request: LlmRequest {
            model: model.clone(),
            messages: vec![LlmMessage::user_text(text.clone())],
            system,
            temperature,
            max_tokens,
            tools: Vec::new(),
        },
        record_args: json!({ "text": text.clone(), "model": model.clone(), "type": prompt_type.clone() }),
        format,
        text,
        model,
        prompt_type,
        tool_names,
        max_turns: config.max_turns,
    })
}

enum LiveVmPolicyDecision {
    Allow,
    Ask { approval: PendingApproval },
    Deny { error: String },
}

fn live_vm_http_policy_decision(policy: &PolicyConfig, args: &Value) -> LiveVmPolicyDecision {
    let policy_args = json!({
        "url": args.get("url").cloned().unwrap_or(Value::Null),
        "method": args.get("method").cloned().unwrap_or(Value::Null),
    });
    let (decision, reason) = policy.decide("http", &policy_args);
    match decision {
        Decision::AlwaysAllow => LiveVmPolicyDecision::Allow,
        Decision::AskBefore => LiveVmPolicyDecision::Ask {
            approval: PendingApproval {
                target: "http".to_string(),
                args: policy_args,
                reason,
            },
        },
        Decision::NeverAllow => LiveVmPolicyDecision::Deny {
            error: format!(
                "policy: `http` denied{}",
                reason.map(|r| format!(" ({})", r)).unwrap_or_default()
            ),
        },
    }
}

fn execute_live_vm_http(args: &Value) -> anyhow::Result<Value> {
    let args = args.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new()?;
        crate::runtime::host_core::execute_http(&rt, &args)
    })
    .join()
    .unwrap_or_else(|_| Err(anyhow::anyhow!("live VM http worker panicked")))
}

fn live_vm_tool_requires_approval(policy: &PolicyConfig, args: &Value) -> anyhow::Result<bool> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or("");
    let kwargs = args.get("kwargs").cloned().unwrap_or_else(|| json!({}));
    let target = format!("tool:{name}");
    let (decision, reason) = policy.decide(&target, &kwargs);
    match decision {
        Decision::AlwaysAllow => Ok(false),
        Decision::AskBefore => Ok(true),
        Decision::NeverAllow => Err(anyhow::anyhow!(
            "policy: `{}` denied{}",
            target,
            reason.map(|r| format!(" ({})", r)).unwrap_or_default()
        )),
    }
}

fn execute_live_vm_call_agent_with_host_promises(
    state: &AppState,
    run_id: Option<&str>,
    args: &Value,
    host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<Value> {
    match execute_live_vm_call_agent_stateful(state, run_id, args, host_promises)? {
        LiveVmChildAgentExecution::Completed(value) => Ok(value),
        LiveVmChildAgentExecution::Paused { .. } => {
            anyhow::bail!("callAgent execution paused on a nested host operation")
        }
    }
}

fn execute_live_vm_call_agent_stateful(
    state: &AppState,
    run_id: Option<&str>,
    args: &Value,
    host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<LiveVmChildAgentExecution> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("callAgent requires string path"))?
        .to_string();
    let input = args.get("input").cloned().unwrap_or(Value::Null);
    if std::path::Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        != Some("ts")
    {
        anyhow::bail!("chidori.callAgent supports .ts agents");
    }

    let runtime_policy =
        RuntimePolicy::from_env_for_durable_run(run_id.unwrap_or("live-vm-call-agent"))?;
    let providers = state.providers.clone();
    let template_engine = state.template_engine.clone();
    let policy = state.policy.clone();
    let mcp = state.mcp.clone();
    let tools_dir = state
        .agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("tools");
    let mut registry = ToolRegistry::load_from_dirs(&[tools_dir])?;
    for def in state.mcp_tools.iter() {
        registry.register(def.clone());
    }
    let registry = Arc::new(registry);

    std::thread::spawn(move || {
        let runtime_ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), host_promises);
        runtime_ctx.set_input_mode(InputMode::Pause);
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new()?);
        let result = TypeScriptVmRuntime::new(runtime_policy)?.run_agent_file_with_context(
            std::path::Path::new(&path),
            &input,
            runtime_ctx.clone(),
            providers,
            template_engine,
            tokio_rt,
            policy,
            Arc::new(StdMutex::new(crate::policy::PolicyCache::default())),
            registry,
            mcp,
        );
        match result {
            Ok(value) => Ok(LiveVmChildAgentExecution::Completed(value)),
            Err(err) => {
                if let Some(pending) = runtime_context_input_pause(&runtime_ctx, &err) {
                    return Ok(LiveVmChildAgentExecution::Paused {
                        pending,
                        host_promises: runtime_ctx.host_promise_records(),
                    });
                }
                Err(err)
            }
        }
    })
    .join()
    .unwrap_or_else(|_| Err(anyhow::anyhow!("live VM callAgent worker panicked")))
}

fn execute_live_vm_prompt(
    state: &AppState,
    run_id: Option<&str>,
    prompt: LiveVmPromptCall,
) -> anyhow::Result<LiveVmPromptExecution> {
    let providers = state.providers.clone();
    let state = state.clone();
    let run_id = run_id.map(ToOwned::to_owned);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new()?;
        let records = Vec::new();
        let messages = vec![LlmMessage::user_text(prompt.text.clone())];
        let tool_schemas = live_vm_tool_schemas(&state, &prompt.tool_names)?;
        drive_live_vm_prompt_loop(
            &state,
            run_id.as_deref(),
            &providers,
            &rt,
            prompt,
            tool_schemas,
            messages,
            0,
            records,
        )
    })
    .join()
    .unwrap_or_else(|_| Err(anyhow::anyhow!("live VM prompt worker panicked")))
}

#[allow(clippy::too_many_arguments)]
fn drive_live_vm_prompt_loop(
    state: &AppState,
    run_id: Option<&str>,
    providers: &ProviderRegistry,
    rt: &tokio::runtime::Runtime,
    prompt: LiveVmPromptCall,
    tool_schemas: Vec<ToolSchema>,
    mut messages: Vec<LlmMessage>,
    start_turn: u64,
    mut records: Vec<LiveVmRecord>,
) -> anyhow::Result<LiveVmPromptExecution> {
    let mut final_text = String::new();
    let turns = if tool_schemas.is_empty() {
        1
    } else {
        prompt.max_turns.max(1)
    };
    for turn in start_turn..turns {
        let request = if tool_schemas.is_empty() {
            prompt.request.clone()
        } else {
            LlmRequest {
                model: prompt.model.clone(),
                messages: messages.clone(),
                system: prompt.request.system.clone(),
                temperature: prompt.request.temperature,
                max_tokens: prompt.request.max_tokens,
                tools: tool_schemas.clone(),
            }
        };
        let response = rt.block_on(providers.send(&request))?;
        final_text = response.content.clone();
        let record_args = live_vm_prompt_record_args(&prompt, tool_schemas.is_empty(), turn);
        let record_result = if tool_schemas.is_empty() {
            Value::String(response.content.clone())
        } else {
            crate::runtime::host_core::llm_response_to_json(&response)
        };
        records.push(LiveVmRecord {
            function: "prompt",
            args: record_args,
            result: record_result,
            token_usage: Some(TokenUsage {
                input_tokens: response.input_tokens,
                output_tokens: response.output_tokens,
            }),
            error: None,
        });
        if response.tool_calls.is_empty() {
            break;
        }
        messages.push(LlmMessage::assistant_blocks(response.blocks.clone()));
        let mut result_blocks = Vec::new();
        for call in response.tool_calls {
            let args = json!({
                "name": call.name,
                "kwargs": call.input,
            });
            match live_vm_tool_requires_approval(&state.policy, &args) {
                Ok(false) => match execute_live_vm_tool_stateful(state, run_id, &args, Vec::new())?
                {
                    LiveVmToolExecution::Completed(value) => {
                        records.push(LiveVmRecord {
                            function: "tool",
                            args,
                            result: value.clone(),
                            token_usage: None,
                            error: None,
                        });
                        result_blocks.push(tool_result_block(call.id, Ok(value)));
                    }
                    LiveVmToolExecution::Paused {
                        pending,
                        host_promises,
                    } => {
                        return Ok(LiveVmPromptExecution::Paused {
                            pending,
                            host_promises,
                            prompt_pause: PersistedPromptToolPause {
                                parent_prompt_id: HostOperationId(0),
                                prompt_args: prompt.record_args.clone(),
                                prompt: PersistedLiveVmPromptCall::from(&prompt),
                                messages,
                                next_turn: turn.saturating_add(1),
                                tool_call_id: call.id,
                                tool_args: args,
                            },
                            records,
                        });
                    }
                },
                Ok(true) => {
                    let message = format!(
                        "policy: `tool:{}` requires approval",
                        args.get("name").and_then(Value::as_str).unwrap_or("")
                    );
                    records.push(LiveVmRecord {
                        function: "tool",
                        args,
                        result: Value::Null,
                        token_usage: None,
                        error: Some(message.clone()),
                    });
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: message,
                        is_error: true,
                    });
                }
                Err(err) => {
                    let message = err.to_string();
                    records.push(LiveVmRecord {
                        function: "tool",
                        args,
                        result: Value::Null,
                        token_usage: None,
                        error: Some(message.clone()),
                    });
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: call.id,
                        content: message,
                        is_error: true,
                    });
                }
            }
        }
        messages.push(LlmMessage {
            role: "user".to_string(),
            content: result_blocks,
        });
    }
    let js_result = if prompt.format.as_deref() == Some("json") {
        serde_json::from_str::<Value>(&final_text)
            .unwrap_or_else(|_| Value::String(final_text.clone()))
    } else {
        Value::String(final_text)
    };
    Ok(LiveVmPromptExecution::Completed(LiveVmPromptResult {
        js_result,
        records,
    }))
}

fn live_vm_prompt_record_args(prompt: &LiveVmPromptCall, plain: bool, turn: u64) -> Value {
    if plain {
        prompt.record_args.clone()
    } else {
        json!({
            "text": prompt.text.clone(),
            "model": prompt.model.clone(),
            "type": prompt.prompt_type.clone(),
            "tools": prompt.tool_names.clone(),
            "turn": turn,
        })
    }
}

fn tool_result_block(tool_use_id: String, result: Result<Value, String>) -> ContentBlock {
    match result {
        Ok(value) => ContentBlock::ToolResult {
            tool_use_id,
            content: serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()),
            is_error: false,
        },
        Err(message) => ContentBlock::ToolResult {
            tool_use_id,
            content: message,
            is_error: true,
        },
    }
}

fn continue_live_vm_prompt_after_tool(
    state: &AppState,
    run_id: Option<&str>,
    prompt_pause: PersistedPromptToolPause,
    tool_result: Result<Value, String>,
) -> anyhow::Result<LiveVmPromptExecution> {
    let providers = state.providers.clone();
    let state = state.clone();
    let run_id = run_id.map(ToOwned::to_owned);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new()?;
        let prompt =
            live_vm_prompt_call_from_persisted(&prompt_pause.prompt, prompt_pause.prompt_args);
        let tool_schemas = live_vm_tool_schemas(&state, &prompt.tool_names)?;
        let mut messages = prompt_pause.messages;
        messages.push(LlmMessage {
            role: "user".to_string(),
            content: vec![tool_result_block(prompt_pause.tool_call_id, tool_result)],
        });
        drive_live_vm_prompt_loop(
            &state,
            run_id.as_deref(),
            &providers,
            &rt,
            prompt,
            tool_schemas,
            messages,
            prompt_pause.next_turn,
            Vec::new(),
        )
    })
    .join()
    .unwrap_or_else(|_| Err(anyhow::anyhow!("live VM prompt resume worker panicked")))
}

fn live_vm_prompt_call_from_persisted(
    prompt: &PersistedLiveVmPromptCall,
    record_args: Value,
) -> LiveVmPromptCall {
    LiveVmPromptCall {
        request: LlmRequest {
            model: prompt.model.clone(),
            messages: vec![LlmMessage::user_text(prompt.text.clone())],
            system: prompt.system.clone(),
            temperature: prompt.temperature,
            max_tokens: prompt.max_tokens,
            tools: Vec::new(),
        },
        record_args,
        format: prompt.format.clone(),
        text: prompt.text.clone(),
        model: prompt.model.clone(),
        prompt_type: prompt.prompt_type.clone(),
        tool_names: prompt.tool_names.clone(),
        max_turns: prompt.max_turns,
    }
}

fn live_vm_tool_schemas(
    state: &AppState,
    tool_names: &[String],
) -> anyhow::Result<Vec<ToolSchema>> {
    if tool_names.is_empty() {
        return Ok(Vec::new());
    }
    let registry = load_live_vm_tool_registry(state)?;
    tool_names
        .iter()
        .map(|name| {
            let tool = registry
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown tool in prompt tools: {name}"))?;
            Ok(tool_def_to_schema(tool))
        })
        .collect()
}

fn tool_def_to_schema(def: &ToolDef) -> ToolSchema {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in &def.params {
        let mut prop = serde_json::Map::new();
        prop.insert("type".to_string(), Value::String(param.param_type.clone()));
        if let Some(description) = &param.description {
            prop.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
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

fn execute_live_vm_tool_with_host_promises(
    state: &AppState,
    run_id: Option<&str>,
    args: &Value,
    host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<Value> {
    match execute_live_vm_tool_stateful(state, run_id, args, host_promises)? {
        LiveVmToolExecution::Completed(value) => Ok(value),
        LiveVmToolExecution::Paused { .. } => {
            anyhow::bail!("tool execution paused on a nested host operation")
        }
    }
}

fn execute_live_vm_tool_stateful(
    state: &AppState,
    run_id: Option<&str>,
    args: &Value,
    host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<LiveVmToolExecution> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tool requires string name"))?
        .to_string();
    let kwargs = args
        .get("kwargs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let registry = load_live_vm_tool_registry(state)?;
    let tool_def = registry
        .get(&name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name}"))?;

    match tool_def.backend.clone() {
        ToolBackend::Mcp {
            server_id,
            remote_name,
        } => {
            let mcp = state.mcp.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(async {
                    mcp.call_tool(&server_id, &remote_name, &Value::Object(kwargs))
                        .await
                })
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("live VM MCP tool worker panicked")))
            .map(LiveVmToolExecution::Completed)
        }
        ToolBackend::TypeScript => {
            let runtime_policy =
                RuntimePolicy::from_env_for_durable_run(run_id.unwrap_or("live-vm-tool"))?;
            let path = tool_def.source_path.clone();
            let kwargs = Value::Object(kwargs);
            let providers = state.providers.clone();
            let template_engine = state.template_engine.clone();
            let policy = state.policy.clone();
            let mcp = state.mcp.clone();
            std::thread::spawn(move || {
                let runtime_ctx =
                    RuntimeContext::with_replay_and_host_promises(Vec::new(), host_promises);
                runtime_ctx.set_input_mode(InputMode::Pause);
                let tokio_rt = Arc::new(tokio::runtime::Runtime::new()?);
                let result = TypeScriptVmRuntime::new(runtime_policy)?.run_tool_file_with_context(
                    &path,
                    &kwargs,
                    runtime_ctx.clone(),
                    providers,
                    template_engine,
                    tokio_rt,
                    policy,
                    Arc::new(StdMutex::new(crate::policy::PolicyCache::default())),
                    registry,
                    mcp,
                );
                match result {
                    Ok(value) => Ok(LiveVmToolExecution::Completed(value)),
                    Err(err) => {
                        if let Some(pending) = runtime_context_input_pause(&runtime_ctx, &err) {
                            return Ok(LiveVmToolExecution::Paused {
                                pending,
                                host_promises: runtime_ctx.host_promise_records(),
                            });
                        }
                        Err(err)
                    }
                }
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("live VM TypeScript tool worker panicked")))
        }
        ToolBackend::Native => registry
            .dispatch_native(&name, Value::Object(kwargs))
            .map(LiveVmToolExecution::Completed),
    }
}

fn runtime_context_input_pause(
    runtime_ctx: &RuntimeContext,
    err: &anyhow::Error,
) -> Option<PendingHostOperation> {
    let has_pending_input_marker = runtime_ctx.take_pending_input().is_some();
    let input_operation = runtime_ctx
        .active_pending_host_operation()
        .filter(|pending| pending.kind == PendingHostOperationKind::Input)
        .or_else(|| {
            runtime_ctx
                .pending_host_operations()
                .into_iter()
                .rev()
                .find(|pending| pending.kind == PendingHostOperationKind::Input)
        });
    if has_pending_input_marker || err.to_string().contains(PAUSE_MARKER) {
        input_operation
    } else {
        None
    }
}

fn load_live_vm_tool_registry(state: &AppState) -> anyhow::Result<Arc<ToolRegistry>> {
    let tools_dir = state
        .agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("tools");
    let mut registry = ToolRegistry::load_from_dirs(&[tools_dir])?;
    for def in state.mcp_tools.iter() {
        registry.register(def.clone());
    }
    Ok(Arc::new(registry))
}

fn persist_nested_live_vm_tool_pause(
    state: &AppState,
    run_id: Option<&str>,
    tool_id: HostOperationId,
    tool_args: Value,
    pending: PendingHostOperation,
    child_host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<PendingHostOperation> {
    persist_nested_live_vm_runtime_pause(
        state,
        run_id,
        tool_id,
        PendingHostOperationKind::Tool,
        tool_args,
        pending,
        child_host_promises,
    )
}

fn persist_nested_live_vm_runtime_pause(
    state: &AppState,
    run_id: Option<&str>,
    parent_id: HostOperationId,
    parent_kind: PendingHostOperationKind,
    parent_args: Value,
    mut pending: PendingHostOperation,
    mut child_host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<PendingHostOperation> {
    let Some(run_id) = run_id else {
        return Ok(pending);
    };
    let table_path = state.run_base.join(run_id).join(HOST_PROMISE_TABLE_FILE);
    let mut records = load_persisted_host_promises(&state.run_base, Some(run_id))?;
    if !records
        .iter()
        .any(|record| record.operation.id == parent_id)
    {
        records.push(HostPromiseRecord {
            operation: PendingHostOperation::new(parent_id, 0, parent_kind, parent_args),
            state: HostPromiseState::Pending,
        });
    }
    remap_child_host_promise_ids(
        &records,
        &[parent_id],
        &mut pending,
        &mut child_host_promises,
    );
    for child in child_host_promises {
        if let Some(existing) = records
            .iter_mut()
            .find(|record| record.operation.id == child.operation.id)
        {
            *existing = child;
        } else {
            records.push(child);
        }
    }
    if !records
        .iter()
        .any(|record| record.operation.id == pending.id)
    {
        records.push(HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Pending,
        });
    }
    std::fs::write(table_path, serde_json::to_vec_pretty(&records)?)?;
    Ok(pending)
}

fn persist_prompt_tool_pause(
    state: &AppState,
    run_id: Option<&str>,
    prompt_pause: &PersistedPromptToolPause,
    mut pending: PendingHostOperation,
    mut child_host_promises: Vec<HostPromiseRecord>,
) -> anyhow::Result<PendingHostOperation> {
    let Some(run_id) = run_id else {
        return Ok(pending);
    };
    let run_dir = state.run_base.join(run_id);
    let table_path = run_dir.join(HOST_PROMISE_TABLE_FILE);
    let mut records = load_persisted_host_promises(&state.run_base, Some(run_id))?;
    if !records
        .iter()
        .any(|record| record.operation.id == prompt_pause.parent_prompt_id)
    {
        records.push(HostPromiseRecord {
            operation: PendingHostOperation::new(
                prompt_pause.parent_prompt_id,
                0,
                PendingHostOperationKind::Prompt,
                prompt_pause.prompt_args.clone(),
            ),
            state: HostPromiseState::Pending,
        });
    }
    remap_child_host_promise_ids(
        &records,
        &[prompt_pause.parent_prompt_id],
        &mut pending,
        &mut child_host_promises,
    );
    for child in child_host_promises {
        if let Some(existing) = records
            .iter_mut()
            .find(|record| record.operation.id == child.operation.id)
        {
            *existing = child;
        } else {
            records.push(child);
        }
    }
    if !records
        .iter()
        .any(|record| record.operation.id == pending.id)
    {
        records.push(HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Pending,
        });
    }
    std::fs::write(table_path, serde_json::to_vec_pretty(&records)?)?;
    std::fs::write(
        run_dir.join(PROMPT_TOOL_PAUSE_FILE),
        serde_json::to_vec_pretty(prompt_pause)?,
    )?;
    Ok(pending)
}

fn remap_child_host_promise_ids(
    existing: &[HostPromiseRecord],
    reserved: &[HostOperationId],
    pending: &mut PendingHostOperation,
    child_host_promises: &mut Vec<HostPromiseRecord>,
) {
    let mut used: std::collections::HashSet<u64> = existing
        .iter()
        .map(|record| record.operation.id.0)
        .collect();
    used.extend(reserved.iter().map(|id| id.0));
    let mut next_id = used.iter().copied().max().unwrap_or(0).saturating_add(1);
    let mut remapped = HashMap::<u64, u64>::new();

    for record in child_host_promises.iter_mut() {
        let old_id = record.operation.id.0;
        if used.contains(&old_id) {
            while used.contains(&next_id) {
                next_id = next_id.saturating_add(1);
            }
            remapped.insert(old_id, next_id);
            record.operation.id = HostOperationId(next_id);
            used.insert(next_id);
            next_id = next_id.saturating_add(1);
        } else {
            used.insert(old_id);
        }
    }

    if let Some(new_id) = remapped.get(&pending.id.0) {
        pending.id = HostOperationId(*new_id);
    }
}

fn load_prompt_tool_pause(
    run_base: &FsPath,
    run_id: Option<&str>,
) -> anyhow::Result<Option<PersistedPromptToolPause>> {
    let Some(run_id) = run_id else {
        return Ok(None);
    };
    let path = run_base.join(run_id).join(PROMPT_TOOL_PAUSE_FILE);
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&std::fs::read(&path)?)
        .map(Some)
        .map_err(|err| anyhow::anyhow!("parsing {}: {}", path.display(), err))
}

fn remove_prompt_tool_pause(run_base: &FsPath, run_id: Option<&str>) {
    let Some(run_id) = run_id else {
        return;
    };
    let path = run_base.join(run_id).join(PROMPT_TOOL_PAUSE_FILE);
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

fn checkpoint_args_from_live_vm_host_call(call: &Value) -> anyhow::Result<Value> {
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("live VM checkpoint host call is missing args"))?;
    Ok(json!({
        "label": args.first().cloned().unwrap_or(Value::Null),
        "data": args.get(1).cloned().unwrap_or(Value::Null),
    }))
}

fn push_live_vm_call_record(
    call_log: &mut Vec<CallRecord>,
    function: impl Into<String>,
    args: Value,
    result: Value,
) {
    push_live_vm_call_record_with_error(call_log, function, args, result, None);
}

fn push_live_vm_error_record(
    call_log: &mut Vec<CallRecord>,
    function: impl Into<String>,
    args: Value,
    error: String,
) {
    push_live_vm_call_record_with_error(call_log, function, args, Value::Null, Some(error));
}

fn push_live_vm_call_record_with_error(
    call_log: &mut Vec<CallRecord>,
    function: impl Into<String>,
    args: Value,
    result: Value,
    error: Option<String>,
) {
    push_live_vm_call_record_with_usage(call_log, function, args, result, None, error);
}

fn push_live_vm_call_record_with_usage(
    call_log: &mut Vec<CallRecord>,
    function: impl Into<String>,
    args: Value,
    result: Value,
    token_usage: Option<TokenUsage>,
    error: Option<String>,
) {
    let seq = call_log
        .iter()
        .map(|record| record.seq)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    call_log.push(CallRecord {
        seq,
        parent_seq: None,
        function: function.into(),
        args,
        result,
        duration_ms: 0,
        token_usage,
        timestamp: chrono::Utc::now(),
        error,
    });
}

fn complete_live_vm_host_result(
    runtime: &mut chidori_quickjs::SnapshotRuntime,
    id: HostOperationId,
    call_log: &mut Vec<CallRecord>,
    function: &'static str,
    args: Value,
    result: anyhow::Result<Value>,
) -> anyhow::Result<()> {
    match result {
        Ok(value) => {
            push_live_vm_call_record(call_log, function, args, value.clone());
            runtime
                .resolve_host_promise(chidori_quickjs::HostPromiseId(id.0), value)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
        Err(err) => {
            let message = err.to_string();
            push_live_vm_error_record(call_log, function, args, message.clone());
            runtime
                .reject_host_promise(chidori_quickjs::HostPromiseId(id.0), message)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
    }
    Ok(())
}

fn complete_live_vm_prompt_result(
    runtime: &mut chidori_quickjs::SnapshotRuntime,
    id: HostOperationId,
    call_log: &mut Vec<CallRecord>,
    result: anyhow::Result<LiveVmPromptResult>,
) -> anyhow::Result<()> {
    match result {
        Ok(result) => {
            for record in result.records {
                push_live_vm_call_record_with_usage(
                    call_log,
                    record.function,
                    record.args,
                    record.result,
                    record.token_usage,
                    record.error,
                );
            }
            runtime
                .resolve_host_promise(chidori_quickjs::HostPromiseId(id.0), result.js_result)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
        Err(err) => {
            let message = err.to_string();
            push_live_vm_error_record(call_log, "prompt", Value::Null, message.clone());
            runtime
                .reject_host_promise(chidori_quickjs::HostPromiseId(id.0), message)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
    }
    Ok(())
}

fn persist_live_vm_pending_snapshot(
    state: &AppState,
    run_id: Option<&str>,
    pending: &PendingHostOperation,
    snapshot: &chidori_quickjs::RuntimeSnapshot,
    call_log: &[CallRecord],
) -> anyhow::Result<()> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let mut host_promises = load_persisted_host_promises(&state.run_base, Some(run_id))?;
    if !host_promises
        .iter()
        .any(|record| record.operation.id == pending.id)
    {
        host_promises.push(HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Pending,
        });
    }
    let store = SnapshotStore::new(state.run_base.join(run_id));
    let mut manifest = store.load_manifest()?;
    manifest.pending = Some(pending.clone());
    manifest.host_promises = host_promises.clone();
    manifest.call_log_len = call_log.len();
    store.save_live_vm_snapshot(&manifest, snapshot, call_log)?;
    std::fs::write(
        state.run_base.join(run_id).join(HOST_PROMISE_TABLE_FILE),
        serde_json::to_vec_pretty(&host_promises)?,
    )?;
    Ok(())
}

enum HostPromiseCompletion {
    Resolved(Value),
    Rejected(String),
}

fn complete_persisted_pending_host_operation(
    run_base: &FsPath,
    run_id: Option<&str>,
    expected: Option<(u64, PendingHostOperationKind)>,
    completion: HostPromiseCompletion,
) -> anyhow::Result<Option<PendingHostOperation>> {
    let Some(run_id) = run_id else {
        return Ok(None);
    };
    let run_dir = run_base.join(run_id);
    let pending_path = run_dir.join(PENDING_HOST_OPERATION_FILE);
    if !pending_path.exists() {
        return Ok(None);
    }

    let pending: PendingHostOperation = serde_json::from_slice(&std::fs::read(&pending_path)?)?;
    if let Some((seq, kind)) = expected {
        if pending.seq != seq || pending.kind != kind {
            return Ok(None);
        }
    }

    let table_path = run_dir.join(HOST_PROMISE_TABLE_FILE);
    let mut records: Vec<HostPromiseRecord> = if table_path.exists() {
        serde_json::from_slice(&std::fs::read(&table_path)?)?
    } else {
        Vec::new()
    };
    let completed_at = chrono::Utc::now();
    let record = records
        .iter_mut()
        .find(|record| record.operation.id == pending.id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "pending host operation {:?} is missing from persisted host promise table",
                pending.id
            )
        })?;
    if !matches!(record.state, HostPromiseState::Pending) {
        anyhow::bail!(
            "pending host operation {:?} is already completed in persisted host promise table",
            pending.id
        );
    }
    record.state = match &completion {
        HostPromiseCompletion::Resolved(value) => HostPromiseState::Resolved {
            value: value.clone(),
            completed_at,
        },
        HostPromiseCompletion::Rejected(error) => HostPromiseState::Rejected {
            error: error.clone(),
            completed_at,
        },
    };
    std::fs::write(&table_path, serde_json::to_vec_pretty(&records)?)?;
    std::fs::remove_file(&pending_path)?;
    Ok(Some(pending))
}

fn complete_persisted_host_promise_record(
    run_base: &FsPath,
    run_id: Option<&str>,
    id: HostOperationId,
    completion: HostPromiseCompletion,
) -> anyhow::Result<()> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let table_path = run_base.join(run_id).join(HOST_PROMISE_TABLE_FILE);
    let mut records: Vec<HostPromiseRecord> = if table_path.exists() {
        serde_json::from_slice(&std::fs::read(&table_path)?)?
    } else {
        Vec::new()
    };
    let completed_at = chrono::Utc::now();
    let record = records
        .iter_mut()
        .find(|record| record.operation.id == id)
        .ok_or_else(|| {
            anyhow::anyhow!("host operation {:?} is missing from persisted table", id)
        })?;
    if !matches!(record.state, HostPromiseState::Pending) {
        anyhow::bail!(
            "host operation {:?} is already completed in persisted host promise table",
            id
        );
    }
    record.state = match completion {
        HostPromiseCompletion::Resolved(value) => HostPromiseState::Resolved {
            value,
            completed_at,
        },
        HostPromiseCompletion::Rejected(error) => HostPromiseState::Rejected {
            error,
            completed_at,
        },
    };
    std::fs::write(&table_path, serde_json::to_vec_pretty(&records)?)?;
    Ok(())
}

fn load_persisted_host_promises(
    run_base: &FsPath,
    run_id: Option<&str>,
) -> anyhow::Result<Vec<HostPromiseRecord>> {
    let Some(run_id) = run_id else {
        return Ok(Vec::new());
    };
    let table_path = run_base.join(run_id).join(HOST_PROMISE_TABLE_FILE);
    if !table_path.exists() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&std::fs::read(&table_path)?)
        .map_err(|err| anyhow::anyhow!("parsing {}: {}", table_path.display(), err))
}

#[allow(dead_code)]
fn resolve_persisted_pending_host_operation(
    run_base: &FsPath,
    run_id: Option<&str>,
    seq: u64,
    kind: PendingHostOperationKind,
    value: Value,
) -> anyhow::Result<()> {
    complete_persisted_pending_host_operation(
        run_base,
        run_id,
        Some((seq, kind)),
        HostPromiseCompletion::Resolved(value),
    )
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

pub async fn serve(
    providers: Arc<ProviderRegistry>,
    template_engine: Arc<TemplateEngine>,
    agent_path: PathBuf,
    port: u16,
) -> anyhow::Result<()> {
    // Configurable concurrency cap. Default 8 is low enough to keep one
    // LLM provider from being flooded and high enough that a small agent
    // fleet can saturate. Expose as env var so ops can tune without a
    // rebuild.
    let max_concurrent: usize = std::env::var("CHIDORI_MAX_CONCURRENT_SESSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(8);
    let acquire_timeout_ms: u64 = std::env::var("CHIDORI_ACQUIRE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);

    // Load the permission policy, MCP servers, recipes, and session store
    // up front so startup errors happen before we bind the listener.
    let policy = PolicyConfig::from_env();
    let mcp = Arc::new(McpManager::new());
    let mcp_cfg = McpServersConfig::load_from_env().unwrap_or_default();
    let mcp_tools = mcp.start_from_config(&mcp_cfg).await.unwrap_or_else(|e| {
        tracing::warn!("MCP startup: {}", e);
        Vec::new()
    });
    let mcp_tools = Arc::new(mcp_tools);

    let session_store = build_session_store();
    let run_base = agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".chidori")
        .join("runs");

    let recipe_dir = std::env::var("CHIDORI_RECIPE_DIR").ok().map(PathBuf::from);
    let recipes = recipe_dir
        .as_ref()
        .map(|d| Recipe::load_dir(d).unwrap_or_default())
        .unwrap_or_default();
    let recipes_arc = Arc::new(recipes.clone());

    // Spawn cron loops for every recipe with a schedule.
    scheduler::spawn_all(
        recipes,
        SchedulerDeps {
            template_engine: template_engine.clone(),
            session_store: session_store.clone(),
            policy: policy.clone(),
            mcp: mcp.clone(),
            mcp_tools: (*mcp_tools).clone(),
        },
    );

    let state = AppState {
        providers,
        template_engine,
        agent_path,
        run_base,
        session_store,
        policy,
        mcp,
        mcp_tools,
        recipes: recipes_arc,
        run_semaphore: Arc::new(Semaphore::new(max_concurrent)),
        acquire_timeout: std::time::Duration::from_millis(acquire_timeout_ms),
        active_sessions: Arc::new(StdMutex::new(HashMap::new())),
    };

    let auth_required = std::env::var("CHIDORI_API_KEY").is_ok();
    let cors_layer = build_cors_layer();

    // ACP router owns its own state so session lookups go through the same
    // SessionStore as the rest of the server.
    let acp_runner_state = state.clone();
    let acp_state = AcpState {
        store: state.session_store.clone(),
        run_prompt: Arc::new(move |inputs: Value| -> Result<Value, String> {
            run_agent_sync(&acp_runner_state, inputs).map_err(|e| e.to_string())
        }),
    };

    let app = Router::new()
        .route("/health", get(health))
        // Session API
        .route("/sessions", post(create_session))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/checkpoint", get(get_checkpoint))
        .route("/sessions/{id}/snapshot", get(get_snapshot_manifest))
        .route("/sessions/{id}/replay", post(replay_session))
        .route("/sessions/{id}/resume", post(resume_session))
        .route("/sessions/{id}/approve", post(approve_session))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/sessions/stream", post(stream_session))
        // Example agent discovery — peer directory of the server's
        // configured agent. Lets clients like util-trace-webgl pick an
        // example to run without restarting the server.
        .route("/agents", get(list_agents))
        // Recipes + scheduler
        .route("/recipes", get(list_recipes))
        .route("/recipes/{name}/run", post(run_recipe))
        .with_state(state.clone())
        // ACP endpoints (separate sub-router so it carries its own state).
        .merge(acp::router(acp_state))
        // Event-driven fallback
        .fallback(any(handle_event).with_state(state.clone()))
        .layer(middleware::from_fn(auth_middleware))
        .layer(cors_layer);

    let addr = format!("0.0.0.0:{port}");
    eprintln!("Listening on http://{addr}");
    eprintln!();
    eprintln!(
        "  Concurrency: max {} sessions, {}ms acquire timeout",
        max_concurrent, acquire_timeout_ms
    );
    eprintln!(
        "  Auth:        {}",
        if auth_required {
            "REQUIRED (Authorization: Bearer $CHIDORI_API_KEY)"
        } else {
            "disabled (set CHIDORI_API_KEY to enable)"
        }
    );
    eprintln!(
        "  CORS:        {}",
        match std::env::var("CHIDORI_CORS_ORIGINS").ok() {
            Some(v) if v.trim() == "*" => "open (Any)".to_string(),
            Some(v) => format!("allow: {}", v),
            None => "disabled (set CHIDORI_CORS_ORIGINS to enable)".to_string(),
        }
    );
    eprintln!();
    eprintln!("  Events:     ANY /*           → agent(event)");
    eprintln!("  Sessions:   POST /sessions   → create & run");
    eprintln!("              GET  /sessions   → list all");
    eprintln!("              GET  /sessions/{{id}}  → get result");
    eprintln!("              GET  /sessions/{{id}}/checkpoint → call log");
    eprintln!("              GET  /sessions/{{id}}/snapshot   → snapshot manifest");
    eprintln!("              POST /sessions/{{id}}/replay     → replay from checkpoint");
    eprintln!("              POST /sessions/{{id}}/resume     → resume paused input() call");
    eprintln!("              POST /sessions/{{id}}/cancel     → cancel running session");
    eprintln!("              POST /sessions/stream            → run with SSE events");
    eprintln!("  Health:     GET  /health");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Hardening layers: auth, CORS, concurrency limits
// ---------------------------------------------------------------------------

/// Middleware: if `CHIDORI_API_KEY` is set, require every non-health
/// request to carry a matching `Authorization: Bearer …` header. Health
/// stays open so container orchestrators can probe without a key.
///
/// When the env var is unset the middleware is a no-op, so the default
/// local-dev experience is unchanged.
async fn auth_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
    let Ok(expected) = std::env::var("CHIDORI_API_KEY") else {
        return next.run(req).await;
    };
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {}", expected))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": "missing or invalid bearer token"})),
        )
            .into_response()
    }
}

/// Build a CORS layer from `CHIDORI_CORS_ORIGINS`:
///
///  * unset     → no CORS headers emitted (same-origin only)
///  * `*`       → `Access-Control-Allow-Origin: *`, `Any` methods + headers
///  * `a,b,c`   → explicit allow-list of origins
fn build_cors_layer() -> CorsLayer {
    let Ok(raw) = std::env::var("CHIDORI_CORS_ORIGINS") else {
        return CorsLayer::new();
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return CorsLayer::new();
    }
    if raw == "*" {
        return CorsLayer::new()
            .allow_origin(CorsAny)
            .allow_methods(CorsAny)
            .allow_headers(CorsAny);
    }
    let origins: Vec<HeaderValue> = raw
        .split(',')
        .filter_map(|o| o.trim().parse::<HeaderValue>().ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods(CorsAny)
        .allow_headers(CorsAny)
}

/// Acquire a run permit or return a `503 Service Unavailable` response
/// after `state.acquire_timeout` elapses. The returned permit is bound to
/// the semaphore via `acquire_owned`, so holding it across an `.await`
/// (e.g. `spawn_blocking`) is fine — dropping the permit releases the
/// slot automatically.
async fn acquire_run_slot(
    state: &AppState,
) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, Response> {
    let sem = state.run_semaphore.clone();
    match tokio::time::timeout(state.acquire_timeout, sem.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "run semaphore closed"})),
        )
            .into_response()),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            Json(json!({
                "error": "server busy; all concurrent-session slots are in use",
                "acquire_timeout_ms": state.acquire_timeout.as_millis() as u64,
            })),
        )
            .into_response()),
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

// ---------------------------------------------------------------------------
// Shared engine builder
// ---------------------------------------------------------------------------

/// Construct a runtime Engine with the full set of goose-parity features
/// wired up: MCP tools merged into the ToolRegistry, permission policy, and
/// MCP manager. Every server handler that spawns an agent goes through here
/// so the config surface stays in one place.
fn build_engine(app: &AppState) -> Engine {
    let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
    let providers = Arc::new(ProviderRegistry::from_env());
    let tools_dir = app
        .agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("tools");
    let mut registry =
        ToolRegistry::load_from_dirs(&[tools_dir]).unwrap_or_else(|_| ToolRegistry::new());
    for def in app.mcp_tools.iter() {
        registry.register(def.clone());
    }
    Engine::new(providers, app.template_engine.clone(), rt)
        .with_tools(Arc::new(registry))
        .with_policy(app.policy.clone())
        .with_mcp(app.mcp.clone())
        .with_persist_base(app.run_base.clone())
}

/// Synchronous one-shot runner used by the ACP endpoint. Runs the agent on
/// the current thread (already inside spawn_blocking) and returns the output
/// JSON. Any error is bubbled as an anyhow::Error.
fn run_agent_sync(app: &AppState, inputs: Value) -> anyhow::Result<Value> {
    let engine = build_engine(app);
    let result = engine.run(&app.agent_path, &inputs)?;
    Ok(result.output)
}

// ---------------------------------------------------------------------------
// Recipes
// ---------------------------------------------------------------------------

async fn list_recipes(State(state): State<AppState>) -> impl IntoResponse {
    let recipes: Vec<Value> = state
        .recipes
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "agent": r.agent,
                "schedule": r.schedule,
                "description": r.description,
            })
        })
        .collect();
    Json(json!({ "recipes": recipes }))
}

async fn run_recipe(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(recipe) = state.recipes.iter().find(|r| r.name == name).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "recipe not found"})),
        )
            .into_response();
    };
    let deps = SchedulerDeps {
        template_engine: state.template_engine.clone(),
        session_store: state.session_store.clone(),
        policy: state.policy.clone(),
        mcp: state.mcp.clone(),
        mcp_tools: (*state.mcp_tools).clone(),
    };
    match scheduler::run_once(&recipe, &deps).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({"session_id": id}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Session API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSessionRequest {
    input: Value,
    /// Optional client-selected id. Useful for cancelling a streaming session
    /// before its final `done` event reports the generated id.
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    /// Optional generation attempt number stamped onto streaming events.
    #[serde(default, alias = "attemptNumber")]
    attempt_number: Option<u64>,
    /// Optional: provide a checkpoint (call log) to replay from.
    #[serde(default)]
    replay_from: Option<Vec<CallRecord>>,
    /// Optional: override the server's default agent for this session.
    /// Must be a bare filename (e.g. "hello.ts") resolved against
    /// the parent directory of the server's configured agent_path.
    /// Path traversal is rejected. When unset, the server's default
    /// agent is used.
    #[serde(default)]
    agent: Option<String>,
}

/// Resolve an optional per-session agent override against the server's
/// configured `agent_path`. Accepts only a bare agent filename in the
/// peer directory — no subdirectories, no `..`, no absolute paths.
/// Returns a `(StatusCode, message)` error suitable for short-circuit
/// rejection when the client passes something invalid.
fn resolve_agent_override(
    default_path: &std::path::Path,
    requested: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    if !is_supported_agent_filename(requested) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid agent name '{}': must be a bare `.ts` filename",
                requested
            ),
        ));
    }
    let dir = default_path.parent().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "server agent_path has no parent directory".to_string(),
    ))?;
    let candidate = dir.join(requested);
    if !candidate.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("agent '{}' not found in {}", requested, dir.display()),
        ));
    }
    Ok(candidate)
}

/// GET /agents — list the agent files in the peer directory of the
/// server's configured agent path. Returns `{agents: [{name, default}]}`
/// where `default = true` marks the server's configured agent.
async fn list_agents(State(state): State<AppState>) -> Response {
    let Some(dir) = state.agent_path.parent() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "server agent_path has no parent directory"})),
        )
            .into_response();
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("read_dir {}: {}", dir.display(), e)})),
            )
                .into_response();
        }
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !is_supported_agent_path(&path) {
                return None;
            }
            path.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    let default_name = state
        .agent_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let agents: Vec<Value> = names
        .into_iter()
        .map(|name| {
            let is_default = default_name.as_deref() == Some(name.as_str());
            json!({ "name": name, "default": is_default })
        })
        .collect();
    Json(json!({ "agents": agents })).into_response()
}

/// POST /sessions — create a new session and run the agent.
async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    let permit = match acquire_run_slot(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let id = body
        .session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let input = body.input.clone();
    let replay_from = body.replay_from.clone();
    // Resolve an optional per-session agent override before spawning
    // the blocking worker — cheaper to reject here than to take a
    // concurrency permit for an invalid request.
    let effective_agent_path = match body.agent.as_deref() {
        Some(requested) => match resolve_agent_override(&state.agent_path, requested) {
            Ok(p) => p,
            Err((status, msg)) => {
                return (status, Json(json!({"error": msg}))).into_response();
            }
        },
        None => state.agent_path.clone(),
    };
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state);
        match replay_from {
            Some(log) => engine.run_replay_pausable(&effective_agent_path, &body.input, log),
            None => engine.run_pausable(&effective_agent_path, &body.input),
        }
    })
    .await
    .unwrap();

    let session = match result {
        Ok(run_result) => {
            let (status, pending_seq, pending_prompt, pending_approval, output) =
                if let Some(pending) = run_result.paused {
                    (
                        SessionStatus::Paused,
                        Some(pending.seq),
                        Some(pending.prompt),
                        None,
                        None,
                    )
                } else if let Some(appr) = run_result.paused_approval {
                    (
                        SessionStatus::AwaitingApproval,
                        None,
                        None,
                        Some(appr),
                        None,
                    )
                } else {
                    (
                        SessionStatus::Completed,
                        None,
                        None,
                        None,
                        Some(run_result.output),
                    )
                };
            StoredSession {
                id: id.clone(),
                run_id: Some(run_result.run_id),
                status,
                input,
                output,
                call_log: run_result.call_log.into_records(),
                error: None,
                pending_seq,
                pending_prompt,
                pending_approval,
                approvals: Vec::new(),
                created_at: chrono::Utc::now(),
            }
        }
        Err(e) => StoredSession {
            id: id.clone(),
            run_id: None,
            status: SessionStatus::Failed,
            input,
            output: None,
            call_log: Vec::new(),
            error: Some(e.to_string()),
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        },
    };

    if let Some(err) = store_or_500(&state.session_store, &session) {
        return err;
    }
    drop(permit);
    (StatusCode::CREATED, Json(session_view(&session))).into_response()
}

/// GET /sessions — list all sessions.
async fn list_sessions(State(state): State<AppState>) -> Response {
    match state.session_store.list() {
        Ok(sessions) => {
            let list: Vec<Value> = sessions
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "status": s.status,
                        "error": s.error,
                        "created_at": s.created_at,
                    })
                })
                .collect();
            Json(json!({"sessions": list})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /sessions/:id — get session result.
async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.session_store.get(&id) {
        Ok(Some(session)) => (StatusCode::OK, Json(session_view(&session))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Session not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /sessions/:id/checkpoint — get the call log (checkpoint data).
async fn get_checkpoint(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.session_store.get(&id) {
        Ok(Some(s)) => Json(json!({
            "id": s.id,
            "run_id": s.run_id,
            "status": s.status,
            "call_log": s.call_log,
            "snapshot_manifest": snapshot_manifest_for_session(&state, &s),
        }))
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Session not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /sessions/:id/snapshot — get snapshot manifest metadata, not VM bytes.
async fn get_snapshot_manifest(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.session_store.get(&id) {
        Ok(Some(s)) => match snapshot_manifest_for_session(&state, &s) {
            Some(manifest) => Json(json!({
                "id": s.id,
                "run_id": s.run_id,
                "snapshot_manifest": manifest,
            }))
            .into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Snapshot manifest not found"})),
            )
                .into_response(),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Session not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /sessions/:id/replay — replay a session from its checkpoint.
async fn replay_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let original = match state.session_store.get(&id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let input = original.input.clone();
    let call_log = original.call_log.clone();
    let host_promises =
        match load_persisted_host_promises(&state.run_base, original.run_id.as_deref()) {
            Ok(host_promises) => host_promises,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        };
    let input_clone = input.clone();
    let approvals = original.approvals.clone();
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
        engine.run_with_replay_and_host_promises(
            &app_state.agent_path,
            &input_clone,
            call_log,
            host_promises,
        )
    })
    .await
    .unwrap();

    match result {
        Ok(run_result) => {
            let new_id = uuid::Uuid::new_v4().to_string();
            let session = StoredSession {
                id: new_id.clone(),
                run_id: Some(run_result.run_id),
                status: SessionStatus::Completed,
                input,
                output: Some(run_result.output.clone()),
                call_log: run_result.call_log.into_records(),
                error: None,
                pending_seq: None,
                pending_prompt: None,
                pending_approval: None,
                approvals: original.approvals.clone(),
                created_at: chrono::Utc::now(),
            };
            if let Some(err) = store_or_500(&state.session_store, &session) {
                return err;
            }
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": new_id,
                    "replayed_from": id,
                    "status": session.status,
                    "output": session.output,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /sessions/stream — run the agent and stream each host-function call
/// as a Server-Sent Event while it executes. Final event has `event: done`
/// carrying the session id and output.
fn stamp_attempt(mut value: Value, attempt_number: Option<u64>) -> Value {
    if let Some(attempt_number) = attempt_number {
        if let Some(object) = value.as_object_mut() {
            object.insert("attempt_number".to_string(), json!(attempt_number));
        }
    }
    value
}

fn runtime_event_to_sse_event(evt: RuntimeEvent, attempt_number: Option<u64>) -> Event {
    let (name, data) = match evt {
        RuntimeEvent::Call(record) => (
            "call",
            serde_json::to_string(&stamp_attempt(json!(record), attempt_number))
                .unwrap_or_else(|_| "{}".into()),
        ),
        RuntimeEvent::PromptStart {
            stream_id,
            seq,
            prompt_type,
            model,
        } => (
            "prompt_start",
            serde_json::to_string(&stamp_attempt(
                json!({
                    "stream_id": stream_id,
                    "seq": seq,
                    "prompt_type": prompt_type,
                    "model": model,
                }),
                attempt_number,
            ))
            .unwrap_or_else(|_| "{}".into()),
        ),
        RuntimeEvent::PromptDelta {
            stream_id,
            seq,
            prompt_type,
            delta,
        } => (
            "prompt_delta",
            serde_json::to_string(&stamp_attempt(
                json!({
                    "stream_id": stream_id,
                    "seq": seq,
                    "prompt_type": prompt_type,
                    "delta": delta,
                }),
                attempt_number,
            ))
            .unwrap_or_else(|_| "{}".into()),
        ),
        RuntimeEvent::PromptEnd {
            stream_id,
            seq,
            prompt_type,
            error,
        } => (
            "prompt_end",
            serde_json::to_string(&stamp_attempt(
                json!({
                    "stream_id": stream_id,
                    "seq": seq,
                    "prompt_type": prompt_type,
                    "error": error,
                }),
                attempt_number,
            ))
            .unwrap_or_else(|_| "{}".into()),
        ),
    };
    Event::default().event(name).data(data)
}

async fn stream_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    use tokio::sync::mpsc;

    // Gate on the concurrency semaphore. If we can't get a permit within
    // the acquire deadline, 503 before any streaming response headers are
    // committed so clients see the overflow cleanly.
    let permit = match acquire_run_slot(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let session_id = body
        .session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let attempt_number = body.attempt_number.or_else(|| {
        body.input
            .pointer("/generation/attemptNumber")
            .or_else(|| body.input.pointer("/generation/attempt_number"))
            .and_then(Value::as_u64)
    });
    let input = body.input.clone();
    let app_state = state.clone();

    let (event_tx, mut event_rx) =
        mpsc::unbounded_channel::<crate::runtime::context::RuntimeEvent>();
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<String>();
    let cancelled = Arc::new(AtomicBool::new(false));
    state.active_sessions.lock().unwrap().insert(
        session_id.clone(),
        ActiveSession {
            cancelled: cancelled.clone(),
            cancel_tx,
            attempt_number,
        },
    );
    let running_session = StoredSession {
        id: session_id.clone(),
        run_id: None,
        status: SessionStatus::Running,
        input: input.clone(),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_approval: None,
        approvals: Vec::new(),
        created_at: chrono::Utc::now(),
    };
    let _ = state.session_store.put(&running_session);

    let agent_path = app_state.agent_path.clone();
    let session_id_for_task = session_id.clone();
    let cancelled_for_task = cancelled.clone();

    // Move the permit into the blocking task so it's held for the entire
    // agent run. Dropping it at the end of the closure releases the slot.
    tokio::task::spawn_blocking(move || {
        let _run_permit = permit;
        let engine = build_engine(&app_state);

        let result = engine.run_streaming(&agent_path, &input, event_tx);
        let final_event = match result {
            Ok(run_result) => {
                let cancelled = cancelled_for_task.load(Ordering::SeqCst);
                let status = if cancelled {
                    SessionStatus::Cancelled
                } else {
                    SessionStatus::Completed
                };
                let output = if cancelled {
                    None
                } else {
                    Some(run_result.output.clone())
                };
                let error = if cancelled {
                    Some("session cancelled".to_string())
                } else {
                    None
                };
                let _ = app_state.session_store.put(&StoredSession {
                    id: session_id_for_task.clone(),
                    run_id: Some(run_result.run_id),
                    status,
                    input: input.clone(),
                    output,
                    call_log: run_result.call_log.into_records(),
                    error: error.clone(),
                    pending_seq: None,
                    pending_prompt: None,
                    pending_approval: None,
                    approvals: Vec::new(),
                    created_at: chrono::Utc::now(),
                });
                if cancelled {
                    json!({
                        "id": session_id_for_task,
                        "status": "cancelled",
                        "error": error,
                    })
                } else {
                    json!({
                        "id": session_id_for_task,
                        "status": "completed",
                        "output": run_result.output,
                    })
                }
            }
            Err(e) => {
                let status = if cancelled_for_task.load(Ordering::SeqCst) {
                    SessionStatus::Cancelled
                } else {
                    SessionStatus::Failed
                };
                let error = if status == SessionStatus::Cancelled {
                    "session cancelled".to_string()
                } else {
                    e.to_string()
                };
                let _ = app_state.session_store.put(&StoredSession {
                    id: session_id_for_task.clone(),
                    run_id: None,
                    status,
                    input: input.clone(),
                    output: None,
                    call_log: Vec::new(),
                    error: Some(error.clone()),
                    pending_seq: None,
                    pending_prompt: None,
                    pending_approval: None,
                    approvals: Vec::new(),
                    created_at: chrono::Utc::now(),
                });
                json!({
                    "id": session_id_for_task,
                    "status": if cancelled_for_task.load(Ordering::SeqCst) { "cancelled" } else { "failed" },
                    "error": error,
                })
            }
        };
        let _ = result_tx.send(final_event);
    });

    let state_for_stream = state.clone();
    let session_id_for_stream = session_id.clone();
    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                Some(evt) = event_rx.recv() => {
                    yield Ok::<_, std::convert::Infallible>(runtime_event_to_sse_event(evt, attempt_number));
                }
                Some(reason) = cancel_rx.recv() => {
                    cancelled.store(true, Ordering::SeqCst);
                    state_for_stream.active_sessions.lock().unwrap().remove(&session_id_for_stream);
                    let final_event = stamp_attempt(json!({
                        "id": session_id_for_stream,
                        "status": "cancelled",
                        "error": reason,
                    }), attempt_number);
                    let data = serde_json::to_string(&final_event).unwrap_or_else(|_| "{}".into());
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("done").data(data));
                    break;
                }
                Some(final_event) = result_rx.recv() => {
                    state_for_stream.active_sessions.lock().unwrap().remove(&session_id_for_stream);
                    let data = serde_json::to_string(&stamp_attempt(final_event, attempt_number)).unwrap_or_else(|_| "{}".into());
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("done").data(data));
                    break;
                }
                else => {
                    state_for_stream.active_sessions.lock().unwrap().remove(&session_id_for_stream);
                    break;
                },
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Deserialize)]
struct CancelSessionRequest {
    #[serde(default)]
    reason: Option<String>,
}

/// POST /sessions/:id/cancel — mark a session cancelled and notify a live
/// streaming run if this server is still supervising it.
async fn cancel_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<CancelSessionRequest>>,
) -> Response {
    let reason = body
        .and_then(|Json(body)| body.reason)
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "session cancelled".to_string());

    let active = state.active_sessions.lock().unwrap().remove(&id);
    let was_active = active.is_some();
    let active_attempt_number = active.as_ref().and_then(|active| active.attempt_number);
    if let Some(active) = &active {
        active.cancelled.store(true, Ordering::SeqCst);
        let _ = active.cancel_tx.send(reason.clone());
    }

    let mut session = match state.session_store.get(&id) {
        Ok(Some(session)) => session,
        Ok(None) if was_active => StoredSession {
            id: id.clone(),
            run_id: None,
            status: SessionStatus::Cancelled,
            input: Value::Null,
            output: None,
            call_log: Vec::new(),
            error: Some(reason.clone()),
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        },
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("session store: {}", e)})),
            )
                .into_response();
        }
    };

    if matches!(session.status, SessionStatus::Completed) && !was_active {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Session already completed"})),
        )
            .into_response();
    }

    session.status = SessionStatus::Cancelled;
    session.error = Some(reason.clone());
    session.output = None;
    if let Some(resp) = store_or_500(&state.session_store, &session) {
        return resp;
    }

    Json(json!({
        "id": id,
        "status": "cancelled",
        "active": was_active,
        "attempt_number": active_attempt_number,
        "reason": reason,
    }))
    .into_response()
}

/// POST /sessions/:id/resume — supply a response to the agent's pending
/// `input()` call and continue the run. Body: `{"response": "<string>"}`.
#[derive(Deserialize)]
struct ResumeRequest {
    response: String,
}

async fn resume_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ResumeRequest>,
) -> Response {
    let original = match state.session_store.get(&id) {
        Ok(Some(s)) if s.status == SessionStatus::Paused => s,
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "Session is not paused"})),
            )
                .into_response();
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let Some(seq) = original.pending_seq else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Paused session has no pending seq"})),
        )
            .into_response();
    };

    if let Err(err) = validate_snapshot_manifest_for_resume(
        &state.run_base,
        original.run_id.as_deref(),
        &state.agent_path,
    ) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }

    let completed_pending = match complete_persisted_pending_host_operation(
        &state.run_base,
        original.run_id.as_deref(),
        Some((seq, PendingHostOperationKind::Input)),
        HostPromiseCompletion::Resolved(Value::String(body.response.clone())),
    ) {
        Ok(pending) => pending,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };

    // Inject a synthetic `input` record at the pending seq so the replaying
    // engine returns the user's response to the agent's input() call.
    let mut call_log = original.call_log.clone();
    call_log.push(CallRecord {
        seq,
        parent_seq: None,
        function: "input".to_string(),
        args: json!({ "prompt": original.pending_prompt.clone().unwrap_or_default() }),
        result: Value::String(body.response.clone()),
        duration_ms: 0,
        token_usage: None,
        timestamp: chrono::Utc::now(),
        error: None,
    });

    if let Some(pending) = completed_pending.as_ref() {
        match try_resume_completed_prompt_tool_from_live_vm_input(
            &state.run_base,
            original.run_id.as_deref(),
            &state.agent_path,
            pending,
            &mut call_log,
            &state,
        ) {
            Ok(Some(LiveVmResumeOutcome::Completed(output))) => {
                let mut session = original;
                session.status = SessionStatus::Completed;
                session.output = Some(output.clone());
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = None;
                if let Some(run_id) = session.run_id.as_ref() {
                    let run_dir = state.run_base.join(run_id);
                    let _ = std::fs::write(
                        run_dir.join("output.json"),
                        serde_json::to_string_pretty(&output).unwrap_or_default(),
                    );
                }
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::Paused {
                mut pending,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                if let Err(e) = persist_live_vm_pending_snapshot(
                    &state,
                    original.run_id.as_deref(),
                    &pending,
                    &snapshot,
                    &call_log,
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
                let mut session = original;
                session.status = SessionStatus::Paused;
                session.call_log = call_log;
                session.pending_seq = Some(pending.seq);
                session.pending_prompt = pending
                    .args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                session.pending_approval = None;
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::AwaitingApproval {
                mut pending,
                approval,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                if let Err(e) = persist_live_vm_pending_snapshot(
                    &state,
                    original.run_id.as_deref(),
                    &pending,
                    &snapshot,
                    &call_log,
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
                let mut session = original;
                session.status = SessionStatus::AwaitingApproval;
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = Some(approval);
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    run_id = ?original.run_id,
                    error = %err,
                    "prompt tool live VM resume did not complete; trying nested runtime resume"
                );
            }
        }
        match try_resume_completed_nested_runtime_from_live_vm_input(
            &state.run_base,
            original.run_id.as_deref(),
            &state.agent_path,
            pending,
            &mut call_log,
            &state,
        ) {
            Ok(Some(LiveVmResumeOutcome::Completed(output))) => {
                let mut session = original;
                session.status = SessionStatus::Completed;
                session.output = Some(output.clone());
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = None;
                if let Some(run_id) = session.run_id.as_ref() {
                    let run_dir = state.run_base.join(run_id);
                    let _ = std::fs::write(
                        run_dir.join("output.json"),
                        serde_json::to_string_pretty(&output).unwrap_or_default(),
                    );
                }
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::Paused {
                mut pending,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                if let Err(e) = persist_live_vm_pending_snapshot(
                    &state,
                    original.run_id.as_deref(),
                    &pending,
                    &snapshot,
                    &call_log,
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
                let mut session = original;
                session.status = SessionStatus::Paused;
                session.call_log = call_log;
                session.pending_seq = Some(pending.seq);
                session.pending_prompt = pending
                    .args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                session.pending_approval = None;
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::AwaitingApproval {
                mut pending,
                approval,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                if let Err(e) = persist_live_vm_pending_snapshot(
                    &state,
                    original.run_id.as_deref(),
                    &pending,
                    &snapshot,
                    &call_log,
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
                let mut session = original;
                session.status = SessionStatus::AwaitingApproval;
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = Some(approval);
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    run_id = ?original.run_id,
                    error = %err,
                    "nested runtime live VM resume did not complete; trying ordinary live input resume"
                );
            }
        }
        match try_resume_completed_live_vm_input(
            &state.run_base,
            original.run_id.as_deref(),
            &state.agent_path,
            pending,
            &body.response,
            &mut call_log,
            &state,
        ) {
            Ok(Some(LiveVmResumeOutcome::Completed(output))) => {
                let mut session = original;
                session.status = SessionStatus::Completed;
                session.output = Some(output.clone());
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = None;
                if let Some(run_id) = session.run_id.as_ref() {
                    let run_dir = state.run_base.join(run_id);
                    let _ = std::fs::write(
                        run_dir.join("output.json"),
                        serde_json::to_string_pretty(&output).unwrap_or_default(),
                    );
                }
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::Paused {
                mut pending,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                let mut host_promises =
                    match load_persisted_host_promises(&state.run_base, original.run_id.as_deref())
                    {
                        Ok(host_promises) => host_promises,
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": e.to_string()})),
                            )
                                .into_response();
                        }
                    };
                if !host_promises
                    .iter()
                    .any(|record| record.operation.id == pending.id)
                {
                    host_promises.push(HostPromiseRecord {
                        operation: pending.clone(),
                        state: HostPromiseState::Pending,
                    });
                }
                if let Some(run_id) = original.run_id.as_ref() {
                    let store = SnapshotStore::new(state.run_base.join(run_id));
                    let mut manifest = match store.load_manifest() {
                        Ok(manifest) => manifest,
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": e.to_string()})),
                            )
                                .into_response();
                        }
                    };
                    manifest.pending = Some(pending.clone());
                    manifest.host_promises = host_promises.clone();
                    manifest.call_log_len = call_log.len();
                    if let Err(e) = store.save_live_vm_snapshot(&manifest, &snapshot, &call_log) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                    if let Err(e) = std::fs::write(
                        state.run_base.join(run_id).join(HOST_PROMISE_TABLE_FILE),
                        serde_json::to_vec_pretty(&host_promises).unwrap_or_default(),
                    ) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                }
                let mut session = original;
                session.status = SessionStatus::Paused;
                session.call_log = call_log;
                session.pending_seq = Some(pending.seq);
                session.pending_prompt = pending
                    .args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                session.pending_approval = None;
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::AwaitingApproval {
                mut pending,
                approval,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                let mut host_promises =
                    match load_persisted_host_promises(&state.run_base, original.run_id.as_deref())
                    {
                        Ok(host_promises) => host_promises,
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": e.to_string()})),
                            )
                                .into_response();
                        }
                    };
                if !host_promises
                    .iter()
                    .any(|record| record.operation.id == pending.id)
                {
                    host_promises.push(HostPromiseRecord {
                        operation: pending.clone(),
                        state: HostPromiseState::Pending,
                    });
                }
                if let Some(run_id) = original.run_id.as_ref() {
                    let store = SnapshotStore::new(state.run_base.join(run_id));
                    let mut manifest = match store.load_manifest() {
                        Ok(manifest) => manifest,
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": e.to_string()})),
                            )
                                .into_response();
                        }
                    };
                    manifest.pending = Some(pending.clone());
                    manifest.host_promises = host_promises.clone();
                    manifest.call_log_len = call_log.len();
                    if let Err(e) = store.save_live_vm_snapshot(&manifest, &snapshot, &call_log) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                    if let Err(e) = std::fs::write(
                        state.run_base.join(run_id).join(HOST_PROMISE_TABLE_FILE),
                        serde_json::to_vec_pretty(&host_promises).unwrap_or_default(),
                    ) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                }
                let mut session = original;
                session.status = SessionStatus::AwaitingApproval;
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = Some(approval);
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    run_id = ?original.run_id,
                    error = %err,
                    "live VM resume did not complete; falling back to replay"
                );
            }
        }
    }

    let input = original.input.clone();
    let input_clone = input.clone();
    let host_promises =
        match load_persisted_host_promises(&state.run_base, original.run_id.as_deref()) {
            Ok(host_promises) => host_promises,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        };
    let approvals = original.approvals.clone();
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
        engine.run_replay_pausable_with_host_promises(
            &app_state.agent_path,
            &input_clone,
            call_log,
            host_promises,
        )
    })
    .await
    .unwrap();

    let mut session = original;
    match result {
        Ok(run_result) => {
            session.run_id = Some(run_result.run_id);
            if let Some(pending) = run_result.paused {
                session.status = SessionStatus::Paused;
                session.call_log = run_result.call_log.into_records();
                session.pending_seq = Some(pending.seq);
                session.pending_prompt = Some(pending.prompt.clone());
                session.pending_approval = None;
            } else if let Some(appr) = run_result.paused_approval {
                session.status = SessionStatus::AwaitingApproval;
                session.call_log = run_result.call_log.into_records();
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = Some(appr);
            } else {
                session.status = SessionStatus::Completed;
                session.output = Some(run_result.output.clone());
                session.call_log = run_result.call_log.into_records();
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = None;
            }
            if let Some(err) = store_or_500(&state.session_store, &session) {
                return err;
            }
            (StatusCode::OK, Json(session_view(&session))).into_response()
        }
        Err(e) => {
            session.status = SessionStatus::Failed;
            session.error = Some(e.to_string());
            let _ = state.session_store.put(&session);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

/// POST /sessions/:id/approve — approve (or deny) a policy-gated call that
/// paused the run. On approve, the (target, args) is appended to the session's
/// approvals list and the agent is replayed; the pre-seeded PolicyCache makes
/// the previously-blocked call pass through. On deny, the session transitions
/// to failed.
#[derive(Deserialize)]
struct ApproveRequest {
    /// "allow" or "deny". Defaults to "allow" for convenience.
    #[serde(default = "default_decision")]
    decision: String,
}

fn default_decision() -> String {
    "allow".to_string()
}

async fn approve_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveRequest>,
) -> Response {
    let mut original = match state.session_store.get(&id) {
        Ok(Some(s)) if s.status == SessionStatus::AwaitingApproval => s,
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "Session is not awaiting approval"})),
            )
                .into_response();
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let Some(pending) = original.pending_approval.clone() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "No pending approval on session"})),
        )
            .into_response();
    };

    if let Err(err) = validate_snapshot_manifest_for_resume(
        &state.run_base,
        original.run_id.as_deref(),
        &state.agent_path,
    ) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }

    if body.decision != "allow" {
        let error = format!("policy: `{}` denied by operator", pending.target);
        if let Err(err) = complete_persisted_pending_host_operation(
            &state.run_base,
            original.run_id.as_deref(),
            None,
            HostPromiseCompletion::Rejected(error.clone()),
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }
        original.status = SessionStatus::Failed;
        original.error = Some(error);
        original.pending_approval = None;
        let _ = state.session_store.put(&original);
        return (StatusCode::OK, Json(session_view(&original))).into_response();
    }

    // Allow path: record the approval, then re-run the agent fresh (no
    // replay log) so the policy cache is seeded and the blocked call runs.
    // We deliberately re-run from scratch rather than replay: the paused
    // run didn't record the blocked call, and replay would expect the same
    // seq to now contain a tool record, which it doesn't.
    original
        .approvals
        .push((pending.target.clone(), pending.args.clone()));
    original.pending_approval = None;

    let live_pending = original.run_id.as_ref().and_then(|run_id| {
        SnapshotStore::new(state.run_base.join(run_id))
            .load_manifest()
            .ok()
            .and_then(|manifest| manifest.pending)
    });
    if let Some(live_pending) = live_pending {
        let mut call_log = original.call_log.clone();
        match try_resume_approved_live_vm_http(
            &state.run_base,
            original.run_id.as_deref(),
            &state.agent_path,
            &live_pending,
            &mut call_log,
            &state,
        ) {
            Ok(Some(LiveVmResumeOutcome::Completed(output))) => {
                let mut session = original;
                session.status = SessionStatus::Completed;
                session.output = Some(output.clone());
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = None;
                if let Some(run_id) = session.run_id.as_ref() {
                    let run_dir = state.run_base.join(run_id);
                    let _ = std::fs::write(
                        run_dir.join("output.json"),
                        serde_json::to_string_pretty(&output).unwrap_or_default(),
                    );
                }
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::Paused {
                mut pending,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                if let Err(e) = persist_live_vm_pending_snapshot(
                    &state,
                    original.run_id.as_deref(),
                    &pending,
                    &snapshot,
                    &call_log,
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
                let mut session = original;
                session.status = SessionStatus::Paused;
                session.call_log = call_log;
                session.pending_seq = Some(pending.seq);
                session.pending_prompt = pending
                    .args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                session.pending_approval = None;
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(Some(LiveVmResumeOutcome::AwaitingApproval {
                mut pending,
                approval,
                snapshot,
            })) => {
                let next_seq = call_log
                    .iter()
                    .map(|record| record.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                pending.seq = next_seq;
                if let Err(e) = persist_live_vm_pending_snapshot(
                    &state,
                    original.run_id.as_deref(),
                    &pending,
                    &snapshot,
                    &call_log,
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
                let mut session = original;
                session.status = SessionStatus::AwaitingApproval;
                session.call_log = call_log;
                session.pending_seq = None;
                session.pending_prompt = None;
                session.pending_approval = Some(approval);
                if let Some(err) = store_or_500(&state.session_store, &session) {
                    return err;
                }
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    run_id = ?original.run_id,
                    error = %err,
                    "live VM approval resume did not complete; falling back to replay"
                );
            }
        }
    }

    if let Err(err) = complete_persisted_pending_host_operation(
        &state.run_base,
        original.run_id.as_deref(),
        None,
        HostPromiseCompletion::Resolved(json!({
            "approved": true,
            "target": pending.target,
        })),
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }

    let input = original.input.clone();
    let approvals = original.approvals.clone();
    let app_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
        engine.run_pausable(&app_state.agent_path, &input)
    })
    .await
    .unwrap();

    let mut session = original;
    match result {
        Ok(run_result) => {
            session.run_id = Some(run_result.run_id);
            if let Some(pending) = run_result.paused {
                session.status = SessionStatus::Paused;
                session.pending_seq = Some(pending.seq);
                session.pending_prompt = Some(pending.prompt);
            } else if let Some(appr) = run_result.paused_approval {
                session.status = SessionStatus::AwaitingApproval;
                session.pending_approval = Some(appr);
            } else {
                session.status = SessionStatus::Completed;
                session.output = Some(run_result.output);
            }
            session.call_log = run_result.call_log.into_records();
            if let Some(err) = store_or_500(&state.session_store, &session) {
                return err;
            }
            (StatusCode::OK, Json(session_view(&session))).into_response()
        }
        Err(e) => {
            session.status = SessionStatus::Failed;
            session.error = Some(e.to_string());
            let _ = state.session_store.put(&session);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Event-driven handler (fallback for non-session routes)
// ---------------------------------------------------------------------------

async fn handle_event(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
    body: Bytes,
) -> impl IntoResponse {
    let mut header_map = serde_json::Map::new();
    for (key, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            header_map.insert(key.as_str().to_string(), Value::String(v.to_string()));
        }
    }

    let query_map: serde_json::Map<String, Value> = query
        .0
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();

    let body_str = String::from_utf8_lossy(&body).to_string();
    let body_value = serde_json::from_str::<Value>(&body_str).unwrap_or(Value::String(body_str));

    let event = json!({
        "method": method.as_str(),
        "path": uri.path(),
        "headers": header_map,
        "query": query_map,
        "body": body_value,
    });

    let input = json!({"event": event});
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state);
        engine.run(&app_state.agent_path, &input)
    })
    .await
    .unwrap();

    match result {
        Ok(result) => {
            if let Value::Object(ref map) = result.output {
                let status = map.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
                let status_code =
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

                if let Some(body) = map.get("body") {
                    let mut response_headers = HeaderMap::new();
                    if let Some(Value::Object(h)) = map.get("headers") {
                        for (k, v) in h {
                            if let (Ok(name), Some(val)) =
                                (k.parse::<axum::http::header::HeaderName>(), v.as_str())
                            {
                                if let Ok(hv) = val.parse() {
                                    response_headers.insert(name, hv);
                                }
                            }
                        }
                    }
                    return (status_code, response_headers, Json(body.clone())).into_response();
                }
            }
            (StatusCode::OK, Json(result.output)).into_response()
        }
        Err(e) => {
            eprintln!("Agent error: {e:#}");
            let error = json!({"error": e.to_string()});
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body;

    fn test_run_base(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_state(run_base: PathBuf, agent_path: PathBuf) -> AppState {
        AppState {
            providers: Arc::new(ProviderRegistry::new()),
            template_engine: Arc::new(TemplateEngine::new(".")),
            agent_path,
            run_base,
            session_store: Arc::new(crate::storage::MemoryStore::new()),
            policy: PolicyConfig::from_env(),
            mcp: Arc::new(McpManager::new()),
            mcp_tools: Arc::new(Vec::new()),
            recipes: Arc::new(Vec::new()),
            run_semaphore: Arc::new(Semaphore::new(1)),
            acquire_timeout: std::time::Duration::from_millis(1),
            active_sessions: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    async fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap();
        (status, value)
    }

    #[test]
    fn stamp_attempt_adds_attempt_number_to_object_events() {
        let stamped = stamp_attempt(json!({"stream_id": "s1"}), Some(42));
        assert_eq!(stamped["attempt_number"], 42);
    }

    #[tokio::test]
    async fn cancel_session_marks_active_session_cancelled() {
        let run_base = test_run_base("cancel_session_marks_active_session_cancelled");
        let agent_path = run_base.join("agent.ts");
        std::fs::write(
            &agent_path,
            "export default function agent() { return {}; }",
        )
        .unwrap();
        let state = test_state(run_base, agent_path);
        let (cancel_tx, mut cancel_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        state.active_sessions.lock().unwrap().insert(
            "session-1".to_string(),
            ActiveSession {
                cancelled: cancelled.clone(),
                cancel_tx,
                attempt_number: Some(7),
            },
        );
        state
            .session_store
            .put(&StoredSession {
                id: "session-1".to_string(),
                run_id: None,
                status: SessionStatus::Running,
                input: json!({"ok": true}),
                output: None,
                call_log: Vec::new(),
                error: None,
                pending_seq: None,
                pending_prompt: None,
                pending_approval: None,
                approvals: Vec::new(),
                created_at: chrono::Utc::now(),
            })
            .unwrap();

        let (status, body) = response_json(
            cancel_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Some(Json(CancelSessionRequest {
                    reason: Some("rewind".to_string()),
                })),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "cancelled");
        assert_eq!(body["active"], true);
        assert_eq!(body["attempt_number"], 7);
        assert_eq!(cancel_rx.recv().await.as_deref(), Some("rewind"));
        assert!(cancelled.load(Ordering::SeqCst));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.status, SessionStatus::Cancelled);
        assert_eq!(stored.error.as_deref(), Some("rewind"));
    }

    struct ServerStaticProvider {
        content: String,
        input_tokens: u64,
        output_tokens: u64,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for ServerStaticProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(
            &self,
            _request: &crate::providers::LlmRequest,
        ) -> anyhow::Result<crate::providers::LlmResponse> {
            Ok(crate::providers::LlmResponse {
                content: self.content.clone(),
                blocks: vec![crate::providers::ContentBlock::Text {
                    text: self.content.clone(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            })
        }
    }

    struct ServerToolUseProvider {
        calls: std::sync::atomic::AtomicUsize,
        tool_name: String,
        tool_input: Value,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for ServerToolUseProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(
            &self,
            _request: &crate::providers::LlmRequest,
        ) -> anyhow::Result<crate::providers::LlmResponse> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(crate::providers::LlmResponse {
                    content: String::new(),
                    blocks: vec![crate::providers::ContentBlock::ToolUse {
                        id: "toolu_1".to_string(),
                        name: self.tool_name.clone(),
                        input: self.tool_input.clone(),
                    }],
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "toolu_1".to_string(),
                        name: self.tool_name.clone(),
                        input: self.tool_input.clone(),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 2,
                    output_tokens: 3,
                })
            } else {
                Ok(crate::providers::LlmResponse {
                    content: "final answer".to_string(),
                    blocks: vec![crate::providers::ContentBlock::Text {
                        text: "final answer".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    stop_reason: "end_turn".to_string(),
                    input_tokens: 5,
                    output_tokens: 7,
                })
            }
        }
    }

    struct ServerRepeatedToolUseProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for ServerRepeatedToolUseProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(
            &self,
            _request: &crate::providers::LlmRequest,
        ) -> anyhow::Result<crate::providers::LlmResponse> {
            match self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => Ok(crate::providers::LlmResponse {
                    content: String::new(),
                    blocks: vec![crate::providers::ContentBlock::ToolUse {
                        id: "toolu_1".to_string(),
                        name: "ask".to_string(),
                        input: json!({ "prompt": "first tool?" }),
                    }],
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "toolu_1".to_string(),
                        name: "ask".to_string(),
                        input: json!({ "prompt": "first tool?" }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 2,
                    output_tokens: 3,
                }),
                1 => Ok(crate::providers::LlmResponse {
                    content: String::new(),
                    blocks: vec![crate::providers::ContentBlock::ToolUse {
                        id: "toolu_2".to_string(),
                        name: "ask".to_string(),
                        input: json!({ "prompt": "second tool?" }),
                    }],
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "toolu_2".to_string(),
                        name: "ask".to_string(),
                        input: json!({ "prompt": "second tool?" }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 4,
                    output_tokens: 5,
                }),
                _ => Ok(crate::providers::LlmResponse {
                    content: "final repeated answer".to_string(),
                    blocks: vec![crate::providers::ContentBlock::Text {
                        text: "final repeated answer".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    stop_reason: "end_turn".to_string(),
                    input_tokens: 6,
                    output_tokens: 7,
                }),
            }
        }
    }

    fn write_completed_host_promises(
        run_base: &FsPath,
        run_id: &str,
        records: Vec<HostPromiseRecord>,
    ) {
        write_host_promises(run_base, run_id, records);
    }

    fn write_host_promises(run_base: &FsPath, run_id: &str, records: Vec<HostPromiseRecord>) {
        let run_dir = run_base.join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join(HOST_PROMISE_TABLE_FILE),
            serde_json::to_vec_pretty(&records).unwrap(),
        )
        .unwrap();
    }

    fn write_snapshot_manifest_for_agent(run_base: &FsPath, run_id: &str, agent_path: &FsPath) {
        let source = std::fs::read_to_string(agent_path).unwrap();
        let manifest = crate::runtime::snapshot::SnapshotManifest::new(
            run_id,
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default(run_id),
            SourceFingerprint::from_source(agent_path, &source),
            Vec::new(),
            None,
            0,
        );
        SnapshotStore::new(run_base.join(run_id))
            .save(&manifest, b"snapshot-bytes", &[])
            .unwrap();
    }

    #[test]
    fn supported_agent_filenames_accept_ts_only() {
        assert!(is_supported_agent_filename("hello.ts"));

        assert!(!is_supported_agent_filename(""));
        assert!(!is_supported_agent_filename("legacy.star"));
        assert!(!is_supported_agent_filename("hello.js"));
        assert!(!is_supported_agent_filename("../hello.ts"));
        assert!(!is_supported_agent_filename("nested/hello.ts"));
        assert!(!is_supported_agent_filename("/tmp/hello.ts"));
        assert!(!is_supported_agent_filename("hello ts.ts"));
    }

    #[test]
    fn resolve_agent_override_accepts_ts_peer_file() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-server-agent-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&run_dir).unwrap();
        let default_agent = run_dir.join("default.ts");
        let alternate_agent = run_dir.join("alternate.ts");
        std::fs::write(&default_agent, "").unwrap();
        std::fs::write(&alternate_agent, "").unwrap();

        let resolved = resolve_agent_override(&default_agent, "alternate.ts").unwrap();
        assert_eq!(resolved, alternate_agent);

        let invalid = resolve_agent_override(&default_agent, "../alternate.ts").unwrap_err();
        assert_eq!(invalid.0, StatusCode::BAD_REQUEST);

        let missing = resolve_agent_override(&default_agent, "missing.ts").unwrap_err();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn checkpoint_snapshot_manifest_is_loaded_by_run_id() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-snapshot-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let store = crate::runtime::snapshot::SnapshotStore::new(run_base.join(run_id));
        let manifest = crate::runtime::snapshot::SnapshotManifest::new(
            run_id,
            crate::runtime::snapshot::SnapshotAbi::current("chidori-quickjs"),
            crate::runtime::snapshot::RuntimePolicy::durable_default(run_id),
            crate::runtime::snapshot::SourceFingerprint::from_source(
                "agent.ts",
                "export async function agent() {}",
            ),
            Vec::new(),
            None,
            0,
        );
        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

        let state = test_state(run_base, temp_dir.join("agent.ts"));
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Completed,
            input: Value::Null,
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };

        let loaded = snapshot_manifest_for_session(&state, &session).unwrap();
        assert_eq!(loaded["run_id"], run_id);
        assert_eq!(loaded["snapshot_file"], "runtime.snapshot");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn replay_session_uses_completed_prompt_host_promise_without_provider() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-replay-prompt-host-promise-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const text = await chidori.prompt("hello", { type: "progress" });
                    return { text };
                }
            "#,
        )
        .unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "old-run";
        write_completed_host_promises(
            &run_base,
            run_id,
            vec![HostPromiseRecord {
                operation: PendingHostOperation::new(
                    crate::runtime::snapshot::HostOperationId(1),
                    1,
                    PendingHostOperationKind::Prompt,
                    json!({
                        "text": "hello",
                        "model": "claude-sonnet-4-6",
                        "type": "progress",
                    }),
                ),
                state: HostPromiseState::Resolved {
                    value: json!("cached prompt"),
                    completed_at: chrono::Utc::now(),
                },
            }],
        );
        let state = test_state(run_base, agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Completed,
            input: json!({}),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) =
            response_json(replay_session(State(state), Path("session-1".to_string())).await).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["output"], json!({ "text": "cached prompt" }));
        assert_eq!(body["status"], json!("completed"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn replay_session_uses_completed_tool_host_promise_without_tool_registry() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-replay-tool-host-promise-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    return await chidori.tool("missing", { value: input.value });
                }
            "#,
        )
        .unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "old-run";
        write_completed_host_promises(
            &run_base,
            run_id,
            vec![HostPromiseRecord {
                operation: PendingHostOperation::new(
                    crate::runtime::snapshot::HostOperationId(1),
                    1,
                    PendingHostOperationKind::Tool,
                    json!({
                        "name": "missing",
                        "kwargs": { "value": 41 },
                    }),
                ),
                state: HostPromiseState::Resolved {
                    value: json!({ "value": 42 }),
                    completed_at: chrono::Utc::now(),
                },
            }],
        );
        let state = test_state(run_base, agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Completed,
            input: json!({ "value": 41 }),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) =
            response_json(replay_session(State(state), Path("session-1".to_string())).await).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["output"], json!({ "value": 42 }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn replay_session_uses_completed_call_agent_host_promise_without_child_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-replay-call-agent-host-promise-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("missing-child.ts");
        let child_path_string = child_path.display().to_string();
        let child_path_json = serde_json::to_string(&child_path_string).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    return await chidori.callAgent({child_path_json}, {{ value: input.value }});
                }}
                "#
            ),
        )
        .unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "old-run";
        write_completed_host_promises(
            &run_base,
            run_id,
            vec![HostPromiseRecord {
                operation: PendingHostOperation::new(
                    crate::runtime::snapshot::HostOperationId(1),
                    1,
                    PendingHostOperationKind::CallAgent,
                    json!({
                        "path": child_path_string,
                        "input": { "value": 41 },
                    }),
                ),
                state: HostPromiseState::Resolved {
                    value: json!({ "value": 42 }),
                    completed_at: chrono::Utc::now(),
                },
            }],
        );
        let state = test_state(run_base, agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Completed,
            input: json!({ "value": 41 }),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) =
            response_json(replay_session(State(state), Path("session-1".to_string())).await).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["output"], json!({ "value": 42 }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_uses_completed_prompt_host_promise_without_provider() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-resume-prompt-host-promise-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const text = await chidori.prompt("hello", { type: "progress" });
                    const approved = await chidori.input("continue?");
                    return { text, approved };
                }
            "#,
        )
        .unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "old-run";
        write_snapshot_manifest_for_agent(&run_base, run_id, &agent_path);
        let pending_input = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(2),
            2,
            PendingHostOperationKind::Input,
            json!({ "prompt": "continue?" }),
        );
        std::fs::write(
            run_base.join(run_id).join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending_input).unwrap(),
        )
        .unwrap();
        write_host_promises(
            &run_base,
            run_id,
            vec![
                HostPromiseRecord {
                    operation: PendingHostOperation::new(
                        crate::runtime::snapshot::HostOperationId(1),
                        1,
                        PendingHostOperationKind::Prompt,
                        json!({
                            "text": "hello",
                            "model": "claude-sonnet-4-6",
                            "type": "progress",
                        }),
                    ),
                    state: HostPromiseState::Resolved {
                        value: json!("cached prompt"),
                        completed_at: chrono::Utc::now(),
                    },
                },
                HostPromiseRecord {
                    operation: pending_input,
                    state: HostPromiseState::Pending,
                },
            ],
        );
        let state = test_state(run_base, agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: Some(2),
            pending_prompt: Some("continue?".to_string()),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "text": "cached prompt", "approved": "yes" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_uses_completed_tool_host_promise_without_tool_registry() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-resume-tool-host-promise-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const value = await chidori.tool("missing", { value: input.value });
                    const approved = await chidori.input("continue?");
                    return { value, approved };
                }
            "#,
        )
        .unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "old-run";
        write_snapshot_manifest_for_agent(&run_base, run_id, &agent_path);
        let pending_input = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(2),
            2,
            PendingHostOperationKind::Input,
            json!({ "prompt": "continue?" }),
        );
        std::fs::write(
            run_base.join(run_id).join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending_input).unwrap(),
        )
        .unwrap();
        write_host_promises(
            &run_base,
            run_id,
            vec![
                HostPromiseRecord {
                    operation: PendingHostOperation::new(
                        crate::runtime::snapshot::HostOperationId(1),
                        1,
                        PendingHostOperationKind::Tool,
                        json!({
                            "name": "missing",
                            "kwargs": { "value": 41 },
                        }),
                    ),
                    state: HostPromiseState::Resolved {
                        value: json!({ "value": 42 }),
                        completed_at: chrono::Utc::now(),
                    },
                },
                HostPromiseRecord {
                    operation: pending_input,
                    state: HostPromiseState::Pending,
                },
            ],
        );
        let state = test_state(run_base, agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Paused,
            input: json!({ "value": 41 }),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: Some(2),
            pending_prompt: Some("continue?".to_string()),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "value": { "value": 42 }, "approved": "yes" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_uses_completed_call_agent_host_promise_without_child_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-resume-call-agent-host-promise-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("missing-child.ts");
        let child_path_string = child_path.display().to_string();
        let child_path_json = serde_json::to_string(&child_path_string).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const value = await chidori.callAgent({child_path_json}, {{ value: input.value }});
                    const approved = await chidori.input("continue?");
                    return {{ value, approved }};
                }}
                "#
            ),
        )
        .unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "old-run";
        write_snapshot_manifest_for_agent(&run_base, run_id, &agent_path);
        let pending_input = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(2),
            2,
            PendingHostOperationKind::Input,
            json!({ "prompt": "continue?" }),
        );
        std::fs::write(
            run_base.join(run_id).join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending_input).unwrap(),
        )
        .unwrap();
        write_host_promises(
            &run_base,
            run_id,
            vec![
                HostPromiseRecord {
                    operation: PendingHostOperation::new(
                        crate::runtime::snapshot::HostOperationId(1),
                        1,
                        PendingHostOperationKind::CallAgent,
                        json!({
                            "path": child_path_string,
                            "input": { "value": 41 },
                        }),
                    ),
                    state: HostPromiseState::Resolved {
                        value: json!({ "value": 42 }),
                        completed_at: chrono::Utc::now(),
                    },
                },
                HostPromiseRecord {
                    operation: pending_input,
                    state: HostPromiseState::Pending,
                },
            ],
        );
        let state = test_state(run_base, agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.to_string()),
            status: SessionStatus::Paused,
            input: json!({ "value": 41 }),
            output: None,
            call_log: Vec::new(),
            error: None,
            pending_seq: Some(2),
            pending_prompt: Some("continue?".to_string()),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "value": { "value": 42 }, "approved": "yes" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_completes_from_live_vm_snapshot_without_replay_run() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-input-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    return { answer };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let run_id = paused.run_id.clone();
        let loaded = SnapshotStore::new(run_base.join(&run_id)).load().unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            SnapshotBlobKind::LiveQuickJsVm
        );

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(run_id));
        assert_eq!(body["output"], json!({ "answer": "yes" }));
        assert!(!run_base
            .join(&run_id)
            .join(PENDING_HOST_OPERATION_FILE)
            .exists());
        assert_eq!(
            serde_json::from_slice::<Value>(
                &std::fs::read(run_base.join(&run_id).join("output.json")).unwrap()
            )
            .unwrap(),
            json!({ "answer": "yes" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_restores_imported_typescript_module_state_from_live_vm() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-imported-module-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let lib_path = temp_dir.join("lib.ts");
        std::fs::write(
            &lib_path,
            r#"
                export const suffix = "from import";
                export function decorate(value) {
                    return `${value} ${suffix}`;
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                import { decorate, suffix } from "./lib.ts";

                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    return { answer: decorate(answer), suffix };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let run_id = paused.run_id.clone();
        let loaded = SnapshotStore::new(run_base.join(&run_id)).load().unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(loaded.manifest.modules.len(), 1);
        assert_eq!(loaded.manifest.modules[0].path, lib_path);
        assert!(loaded
            .manifest
            .module_graph
            .iter()
            .any(|entry| entry.path == agent_path
                && entry
                    .imports
                    .iter()
                    .any(|import| import.resolved_path == Some(lib_path.clone()))));

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(run_id));
        assert_eq!(
            body["output"],
            json!({ "answer": "yes from import", "suffix": "from import" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_log_call_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-log-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    await chidori.log("after input");
                    return { answer };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();
        let loaded = SnapshotStore::new(run_base.join(&original_run_id))
            .load()
            .unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            SnapshotBlobKind::LiveQuickJsVm
        );

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["output"], json!({ "answer": "yes" }));
        assert_eq!(body["run_id"], json!(original_run_id));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "log");
        assert_eq!(stored.call_log[1].args, json!({ "message": "after input" }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_template_call_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-template-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    const rendered = await chidori.template("Hello {{ name }}", { name: answer });
                    return { rendered };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "TS".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "rendered": "Hello TS" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "template");
        assert_eq!(
            stored.call_log[1].args,
            json!({ "template": "Hello {{ name }}", "vars": { "name": "TS" } })
        );
        assert_eq!(stored.call_log[1].result, json!("Hello TS"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_rejects_live_vm_host_call_and_continues_when_caught() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-rejection-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.input("continue?");
                    try {
                        await chidori.template("Hello {{ name", {});
                        return { caught: false };
                    } catch (err) {
                        return { caught: true, message: String(err && err.message ? err.message : err) };
                    }
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"]["caught"], json!(true));
        assert!(body["output"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("template"));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "template");
        assert!(stored.call_log[1].error.is_some());
        assert_eq!(stored.call_log[1].result, Value::Null);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_memory_call_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-memory-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let namespace = format!("server-live-vm-memory-{}", uuid::Uuid::new_v4());
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const answer = await chidori.input("continue?");
                    await chidori.memory("set", "answer", {{ value: answer }}, {{ namespace: "{namespace}" }});
                    const saved = await chidori.memory("get", "answer", null, {{ namespace: "{namespace}" }});
                    return {{ saved }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "stored".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "saved": { "value": "stored" } }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "memory");
        assert_eq!(stored.call_log[1].args["action"], json!("set"));
        assert_eq!(stored.call_log[1].result, Value::Null);
        assert_eq!(stored.call_log[2].function, "memory");
        assert_eq!(stored.call_log[2].args["action"], json!("get"));
        assert_eq!(stored.call_log[2].result, json!({ "value": "stored" }));

        let _ = std::fs::remove_file(
            std::path::Path::new(".chidori")
                .join("memory")
                .join(format!("{namespace}.json")),
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_checkpoint_call_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-checkpoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    await chidori.checkpoint("after-input", { answer });
                    return { answer };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "answer": "yes" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "checkpoint");
        assert_eq!(
            stored.call_log[1].args,
            json!({ "label": "after-input", "data": { "answer": "yes" } })
        );
        assert_eq!(stored.call_log[1].result, Value::Null);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_sandbox_exec_js_call_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-sandbox-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    const result = await chidori.execJs(`"value:" + ${JSON.stringify(answer)}`, { fuel: 200000000 });
                    return { result };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "ok".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "result": "value:ok" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "exec_js");
        assert_eq!(
            stored.call_log[1].args,
            json!({ "source": "\"value:\" + \"ok\"", "fuel": 200000000 })
        );
        assert_eq!(stored.call_log[1].result, json!("value:ok"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_rejects_policy_denied_http_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-http-deny-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.input("continue?");
                    try {
                        await chidori.http("https://example.invalid");
                        return { caught: false };
                    } catch (err) {
                        return { caught: true, message: String(err && err.message ? err.message : err) };
                    }
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let mut state = test_state(run_base.clone(), agent_path);
        state.policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: Decision::NeverAllow,
                match_args: None,
                reason: Some("test deny".to_string()),
            }],
            default: Decision::AlwaysAllow,
        });
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"]["caught"], json!(true));
        assert_eq!(
            body["output"]["message"],
            json!("policy: `http` denied (test deny)")
        );
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "http");
        assert_eq!(
            stored.call_log[1].args,
            json!({
                "url": "https://example.invalid",
                "method": "GET",
                "headers": null,
                "body": null,
                "params": null,
            })
        );
        assert_eq!(
            stored.call_log[1].error.as_deref(),
            Some("policy: `http` denied (test deny)")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_pauses_for_http_approval_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-http-approval-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    await chidori.http("https://example.invalid/live", { method: "POST", body: { answer } });
                    return { ok: true };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let mut state = test_state(run_base.clone(), agent_path);
        state.policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: Decision::AskBefore,
                match_args: None,
                reason: Some("needs approval".to_string()),
            }],
            default: Decision::AlwaysAllow,
        });
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("awaitingapproval"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_approval"]["target"], json!("http"));
        assert_eq!(
            body["pending_approval"]["args"],
            json!({ "url": "https://example.invalid/live", "method": "POST" })
        );
        assert_eq!(body["pending_approval"]["reason"], json!("needs approval"));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.status, SessionStatus::AwaitingApproval);
        assert_eq!(stored.call_log.len(), 1);
        assert_eq!(stored.call_log[0].function, "input");

        let manifest = SnapshotStore::new(run_base.join(&original_run_id))
            .load_manifest()
            .unwrap();
        let pending = manifest.pending.as_ref().unwrap();
        assert_eq!(pending.kind, PendingHostOperationKind::Http);
        assert_eq!(
            pending.args,
            json!({
                "url": "https://example.invalid/live",
                "method": "POST",
                "headers": null,
                "body": { "answer": "yes" },
                "params": null,
            })
        );
        assert_eq!(manifest.host_promises.len(), 2);
        assert!(manifest
            .host_promises
            .iter()
            .any(|record| record.operation.id == pending.id
                && matches!(record.state, HostPromiseState::Pending)));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn approve_session_continues_policy_gated_http_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-approve-http-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};

            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /approved?answer=yes HTTP/1.1"));
            let body = r#"{"approved":true}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const answer = await chidori.input("continue?");
                    const response = await chidori.http("http://{addr}/approved", {{ params: {{ answer }} }});
                    return {{ status: response.status, body: response.body }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let mut state = test_state(run_base.clone(), agent_path);
        state.policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: Decision::AskBefore,
                match_args: None,
                reason: Some("needs approval".to_string()),
            }],
            default: Decision::AlwaysAllow,
        });
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("awaitingapproval"));

        let (status, body) = response_json(
            approve_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ApproveRequest {
                    decision: "allow".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(
            body["output"],
            json!({ "status": 200, "body": { "approved": true } })
        );
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "http");
        assert_eq!(
            stored.call_log[1].args,
            json!({
                "url": format!("http://{addr}/approved"),
                "method": "GET",
                "headers": null,
                "body": null,
                "params": { "answer": "yes" },
            })
        );
        assert_eq!(stored.call_log[1].result["status"], json!(200));
        assert!(!run_base
            .join(&original_run_id)
            .join(PENDING_HOST_OPERATION_FILE)
            .exists());

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_allowed_http_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-http-allow-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};

            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /live?answer=yes HTTP/1.1"));
            let body = r#"{"ok":true,"value":42}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const answer = await chidori.input("continue?");
                    const response = await chidori.http("http://{addr}/live", {{ params: {{ answer }} }});
                    return {{ status: response.status, body: response.body }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(
            body["output"],
            json!({ "status": 200, "body": { "ok": true, "value": 42 } })
        );
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "http");
        assert_eq!(
            stored.call_log[1].args,
            json!({
                "url": format!("http://{addr}/live"),
                "method": "GET",
                "headers": null,
                "body": null,
                "params": { "answer": "yes" },
            })
        );
        assert_eq!(stored.call_log[1].result["status"], json!(200));
        assert_eq!(
            stored.call_log[1].result["body"],
            json!({ "ok": true, "value": 42 })
        );

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_typescript_tool_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-tool-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_dir.join("tools")).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            temp_dir.join("tools").join("echo.ts"),
            r#"
                export const tool = {
                    name: "echo",
                    description: "Echo a value",
                    parameters: {
                        type: "object",
                        properties: { value: { type: "string" } },
                        required: ["value"],
                    },
                };

                export async function run(args, chidori) {
                    return { value: `${args.value}!` };
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    const result = await chidori.tool("echo", { value: answer });
                    return { result };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "tool".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "result": { "value": "tool!" } }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "tool");
        assert_eq!(
            stored.call_log[1].args,
            json!({ "name": "echo", "kwargs": { "value": "tool" } })
        );
        assert_eq!(stored.call_log[1].result, json!({ "value": "tool!" }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_suspending_tool_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-suspending-tool-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_dir.join("tools")).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            temp_dir.join("tools").join("ask.ts"),
            r#"
                export const tool = {
                    name: "ask",
                    description: "Ask for input",
                    parameters: {
                        type: "object",
                        properties: { prompt: { type: "string" } },
                        required: ["prompt"],
                    },
                };

                export async function run(args, chidori) {
                    const answer = await chidori.input(args.prompt);
                    return { answer };
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const first = await chidori.input("first?");
                    const result = await chidori.tool("ask", { prompt: `tool ${first}?` });
                    return { result };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_prompt"], json!("tool go?"));
        let manifest = SnapshotStore::new(run_base.join(&original_run_id))
            .load_manifest()
            .unwrap();
        assert_eq!(
            manifest.pending.as_ref().map(|pending| &pending.kind),
            Some(&PendingHostOperationKind::Input)
        );
        assert!(manifest
            .host_promises
            .iter()
            .any(
                |record| record.operation.kind == PendingHostOperationKind::Tool
                    && matches!(record.state, HostPromiseState::Pending)
            ));
        assert!(manifest
            .host_promises
            .iter()
            .any(
                |record| record.operation.kind == PendingHostOperationKind::Input
                    && matches!(record.state, HostPromiseState::Pending)
            ));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "done".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "result": { "answer": "done" } }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "input");
        assert_eq!(stored.call_log[2].function, "tool");
        assert_eq!(
            stored.call_log[2].args,
            json!({ "name": "ask", "kwargs": { "prompt": "tool go?" } })
        );
        assert_eq!(stored.call_log[2].result, json!({ "answer": "done" }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_rejects_suspending_tool_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-suspending-tool-reject-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_dir.join("tools")).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            temp_dir.join("tools").join("fail.ts"),
            r#"
                export const tool = {
                    name: "fail",
                    description: "Fail after input",
                    parameters: {
                        type: "object",
                        properties: { prompt: { type: "string" } },
                        required: ["prompt"],
                    },
                };

                export async function run(args, chidori) {
                    const answer = await chidori.input(args.prompt);
                    throw new Error(`tool failed ${answer}`);
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const first = await chidori.input("first?");
                    try {
                        await chidori.tool("fail", { prompt: `tool ${first}?` });
                    } catch (err) {
                        return { caught: String(err) };
                    }
                    return { caught: null };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["pending_prompt"], json!("tool go?"));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "bad".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "caught": "Error: JavaScript exception: tool failed bad" })
        );
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "input");
        assert_eq!(stored.call_log[2].function, "tool");
        assert_eq!(stored.call_log[2].result, Value::Null);
        assert_eq!(
            stored.call_log[2].error.as_deref(),
            Some("JavaScript exception: tool failed bad")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_call_agent_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-call-agent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("child.ts");
        std::fs::write(
            &child_path,
            r#"
                export async function agent(input, chidori) {
                    return { child: `${input.value}!` };
                }
            "#,
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const answer = await chidori.input("continue?");
                    const result = await chidori.callAgent({child_path_json}, {{ value: answer }});
                    return {{ result }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "child".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "result": { "child": "child!" } }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "call_agent");
        assert_eq!(
            stored.call_log[1].args,
            json!({ "path": child_path.display().to_string(), "input": { "value": "child" } })
        );
        assert_eq!(stored.call_log[1].result, json!({ "child": "child!" }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_suspending_call_agent_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-suspending-call-agent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("child.ts");
        std::fs::write(
            &child_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input(`child ${input.value}?`);
                    return { child: answer };
                }
            "#,
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const first = await chidori.input("first?");
                    const result = await chidori.callAgent({child_path_json}, {{ value: first }});
                    return {{ result }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_prompt"], json!("child go?"));
        let manifest = SnapshotStore::new(run_base.join(&original_run_id))
            .load_manifest()
            .unwrap();
        assert_eq!(
            manifest.pending.as_ref().map(|pending| &pending.kind),
            Some(&PendingHostOperationKind::Input)
        );
        assert!(manifest.host_promises.iter().any(|record| {
            record.operation.kind == PendingHostOperationKind::CallAgent
                && matches!(record.state, HostPromiseState::Pending)
        }));
        assert!(manifest.host_promises.iter().any(|record| {
            record.operation.kind == PendingHostOperationKind::Input
                && matches!(record.state, HostPromiseState::Pending)
        }));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "done".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "result": { "child": "done" } }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "input");
        assert_eq!(stored.call_log[2].function, "call_agent");
        assert_eq!(
            stored.call_log[2].args,
            json!({ "path": child_path.display().to_string(), "input": { "value": "go" } })
        );
        assert_eq!(stored.call_log[2].result, json!({ "child": "done" }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_nested_suspending_call_agent_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-nested-suspending-call-agent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("child.ts");
        let grandchild_path = temp_dir.join("grandchild.ts");
        std::fs::write(
            &grandchild_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input(`grandchild ${input.value}?`);
                    return { grandchild: answer };
                }
            "#,
        )
        .unwrap();
        let grandchild_path_json =
            serde_json::to_string(&grandchild_path.display().to_string()).unwrap();
        std::fs::write(
            &child_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const result = await chidori.callAgent({grandchild_path_json}, {{ value: input.value }});
                    return {{ child: result }};
                }}
            "#
            ),
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const first = await chidori.input("first?");
                    const result = await chidori.callAgent({child_path_json}, {{ value: first }});
                    return {{ result }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_prompt"], json!("grandchild go?"));
        let manifest = SnapshotStore::new(run_base.join(&original_run_id))
            .load_manifest()
            .unwrap();
        assert_eq!(
            manifest.pending.as_ref().map(|pending| &pending.kind),
            Some(&PendingHostOperationKind::Input)
        );
        assert!(manifest.host_promises.iter().any(|record| {
            record.operation.kind == PendingHostOperationKind::CallAgent
                && matches!(record.state, HostPromiseState::Pending)
        }));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "done".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(
            body["output"],
            json!({ "result": { "child": { "grandchild": "done" } } })
        );
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "input");
        assert_eq!(stored.call_log[2].function, "call_agent");
        assert_eq!(
            stored.call_log[2].args,
            json!({ "path": child_path.display().to_string(), "input": { "value": "go" } })
        );
        assert_eq!(
            stored.call_log[2].result,
            json!({ "child": { "grandchild": "done" } })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_rejects_nested_suspending_call_agent_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-nested-suspending-call-agent-reject-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("child.ts");
        let grandchild_path = temp_dir.join("grandchild.ts");
        std::fs::write(
            &grandchild_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input(`grandchild ${input.value}?`);
                    throw new Error(`grandchild failed ${answer}`);
                }
            "#,
        )
        .unwrap();
        let grandchild_path_json =
            serde_json::to_string(&grandchild_path.display().to_string()).unwrap();
        std::fs::write(
            &child_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    return await chidori.callAgent({grandchild_path_json}, {{ value: input.value }});
                }}
            "#
            ),
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const first = await chidori.input("first?");
                    try {{
                        await chidori.callAgent({child_path_json}, {{ value: first }});
                    }} catch (err) {{
                        return {{ caught: String(err) }};
                    }}
                    return {{ caught: null }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_prompt"], json!("grandchild go?"));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "bad".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert!(body["output"]["caught"]
            .as_str()
            .unwrap()
            .contains("grandchild failed bad"));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "input");
        assert_eq!(stored.call_log[2].function, "call_agent");
        assert_eq!(stored.call_log[2].result, Value::Null);
        assert!(stored.call_log[2]
            .error
            .as_deref()
            .unwrap()
            .contains("grandchild failed bad"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_rejects_suspending_call_agent_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-suspending-call-agent-reject-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        let child_path = temp_dir.join("child.ts");
        std::fs::write(
            &child_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input(`child ${input.value}?`);
                    throw new Error(`child failed ${answer}`);
                }
            "#,
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &agent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const first = await chidori.input("first?");
                    try {{
                        await chidori.callAgent({child_path_json}, {{ value: first }});
                    }} catch (err) {{
                        return {{ caught: String(err) }};
                    }}
                    return {{ caught: null }};
                }}
            "#
            ),
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["pending_prompt"], json!("child go?"));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "bad".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "caught": "Error: JavaScript exception: child failed bad" })
        );
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 3);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "input");
        assert_eq!(stored.call_log[2].function, "call_agent");
        assert_eq!(stored.call_log[2].result, Value::Null);
        assert_eq!(
            stored.call_log[2].error.as_deref(),
            Some("JavaScript exception: child failed bad")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_prompt_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-prompt-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("continue?");
                    const text = await chidori.prompt(`Say ${answer}`, { model: "test-model", type: "progress" });
                    return { text };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ServerStaticProvider {
            content: "provider result".to_string(),
            input_tokens: 3,
            output_tokens: 5,
        }));
        let mut state = test_state(run_base.clone(), agent_path);
        state.providers = Arc::new(providers);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "status".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "text": "provider result" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 2);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "prompt");
        assert_eq!(
            stored.call_log[1].args,
            json!({ "text": "Say status", "model": "test-model", "type": "progress" })
        );
        assert_eq!(stored.call_log[1].result, json!("provider result"));
        let usage = stored.call_log[1].token_usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 5);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_prompt_tool_loop_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-prompt-tool-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_dir.join("tools")).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            temp_dir.join("tools").join("echo.ts"),
            r#"
                export const tool = {
                    name: "echo",
                    description: "Echo a value",
                    parameters: {
                        type: "object",
                        properties: { value: { type: "string" } },
                        required: ["value"],
                    },
                };

                export async function run(args, chidori) {
                    return { echoed: `${args.value}!` };
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.input("continue?");
                    const text = await chidori.prompt("use a tool", { model: "test-model", tools: ["echo"] });
                    return { text };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let pending = paused.paused.expect("expected input pause");
        let original_run_id = paused.run_id.clone();

        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ServerToolUseProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool_name: "echo".to_string(),
            tool_input: json!({ "value": "from-model" }),
        }));
        let mut state = test_state(run_base.clone(), agent_path);
        state.providers = Arc::new(providers);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(pending.seq),
            pending_prompt: Some(pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "text": "final answer" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 4);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "prompt");
        assert_eq!(
            stored.call_log[1].args,
            json!({
                "text": "use a tool",
                "model": "test-model",
                "type": null,
                "tools": ["echo"],
                "turn": 0,
            })
        );
        assert_eq!(stored.call_log[2].function, "tool");
        assert_eq!(
            stored.call_log[2].args,
            json!({ "name": "echo", "kwargs": { "value": "from-model" } })
        );
        assert_eq!(
            stored.call_log[2].result,
            json!({ "echoed": "from-model!" })
        );
        assert_eq!(stored.call_log[3].function, "prompt");
        assert_eq!(stored.call_log[3].result["content"], json!("final answer"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_suspending_prompt_tool_loop_from_live_vm_without_replay() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-prompt-suspending-tool-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_dir.join("tools")).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            temp_dir.join("tools").join("ask.ts"),
            r#"
                export const tool = {
                    name: "ask",
                    description: "Ask for input",
                    parameters: {
                        type: "object",
                        properties: { prompt: { type: "string" } },
                        required: ["prompt"],
                    },
                };

                export async function run(args, chidori) {
                    const answer = await chidori.input(args.prompt);
                    return { answer };
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.input("continue?");
                    const text = await chidori.prompt("use a suspending tool", { model: "test-model", tools: ["ask"] });
                    return { text };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ServerToolUseProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            tool_name: "ask".to_string(),
            tool_input: json!({ "prompt": "tool prompt?" }),
        }));
        let mut state = test_state(run_base.clone(), agent_path);
        state.providers = Arc::new(providers);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "yes".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_prompt"], json!("tool prompt?"));
        let manifest = SnapshotStore::new(run_base.join(&original_run_id))
            .load_manifest()
            .unwrap();
        assert_eq!(
            manifest.pending.as_ref().map(|pending| &pending.kind),
            Some(&PendingHostOperationKind::Input)
        );
        assert!(run_base
            .join(&original_run_id)
            .join(PROMPT_TOOL_PAUSE_FILE)
            .exists());

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "nested".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "text": "final answer" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 5);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "prompt");
        assert_eq!(
            stored.call_log[1].result["tool_calls"][0]["name"],
            json!("ask")
        );
        assert_eq!(stored.call_log[2].function, "input");
        assert_eq!(stored.call_log[3].function, "tool");
        assert_eq!(
            stored.call_log[3].args,
            json!({ "name": "ask", "kwargs": { "prompt": "tool prompt?" } })
        );
        assert_eq!(stored.call_log[3].result, json!({ "answer": "nested" }));
        assert_eq!(stored.call_log[4].function, "prompt");
        assert_eq!(stored.call_log[4].result["content"], json!("final answer"));
        assert!(!run_base
            .join(&original_run_id)
            .join(PROMPT_TOOL_PAUSE_FILE)
            .exists());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_handles_repeated_suspending_prompt_tool_loop_from_live_vm_without_replay(
    ) {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-prompt-repeated-suspending-tool-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_dir.join("tools")).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            temp_dir.join("tools").join("ask.ts"),
            r#"
                export const tool = {
                    name: "ask",
                    description: "Ask for input",
                    parameters: {
                        type: "object",
                        properties: { prompt: { type: "string" } },
                        required: ["prompt"],
                    },
                };

                export async function run(args, chidori) {
                    const answer = await chidori.input(args.prompt);
                    return { answer };
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.input("continue?");
                    const text = await chidori.prompt("use repeated tools", { model: "test-model", tools: ["ask"] });
                    return { text };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let original_run_id = paused.run_id.clone();

        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ServerRepeatedToolUseProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }));
        let mut state = test_state(run_base.clone(), agent_path);
        state.providers = Arc::new(providers);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(original_run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "start".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["pending_prompt"], json!("first tool?"));

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "first".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["pending_prompt"], json!("second tool?"));
        assert!(run_base
            .join(&original_run_id)
            .join(PROMPT_TOOL_PAUSE_FILE)
            .exists());

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "second".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(original_run_id));
        assert_eq!(body["output"], json!({ "text": "final repeated answer" }));
        let stored = state.session_store.get("session-1").unwrap().unwrap();
        assert_eq!(stored.call_log.len(), 8);
        assert_eq!(stored.call_log[0].function, "input");
        assert_eq!(stored.call_log[1].function, "prompt");
        assert_eq!(
            stored.call_log[1].result["tool_calls"][0]["id"],
            json!("toolu_1")
        );
        assert_eq!(stored.call_log[2].function, "input");
        assert_eq!(stored.call_log[3].function, "tool");
        assert_eq!(stored.call_log[3].result, json!({ "answer": "first" }));
        assert_eq!(stored.call_log[4].function, "prompt");
        assert_eq!(
            stored.call_log[4].result["tool_calls"][0]["id"],
            json!("toolu_2")
        );
        assert_eq!(stored.call_log[5].function, "input");
        assert_eq!(stored.call_log[6].function, "tool");
        assert_eq!(stored.call_log[6].result, json!({ "answer": "second" }));
        assert_eq!(stored.call_log[7].function, "prompt");
        assert_eq!(
            stored.call_log[7].result["content"],
            json!("final repeated answer")
        );
        assert!(!run_base
            .join(&original_run_id)
            .join(PROMPT_TOOL_PAUSE_FILE)
            .exists());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn resume_session_direct_live_vm_pauses_again_on_second_input() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-live-vm-resume-second-input-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(
            &agent_path,
            r#"
                export async function agent(input, chidori) {
                    const first = await chidori.input("first?");
                    const second = await chidori.input("second?");
                    return { first, second };
                }
            "#,
        )
        .unwrap();

        let paused = tokio::task::spawn_blocking({
            let temp_dir = temp_dir.clone();
            let run_base = run_base.clone();
            let agent_path = agent_path.clone();
            move || {
                let engine = Engine::new(
                    Arc::new(ProviderRegistry::new()),
                    Arc::new(TemplateEngine::new(&temp_dir)),
                    Arc::new(tokio::runtime::Runtime::new().unwrap()),
                )
                .with_persist_base(run_base);
                engine.run_pausable(&agent_path, &json!({}))
            }
        })
        .await
        .unwrap()
        .unwrap();
        let first_pending = paused.paused.expect("expected first input pause");
        let run_id = paused.run_id.clone();

        let state = test_state(run_base.clone(), agent_path);
        let session = StoredSession {
            id: "session-1".to_string(),
            run_id: Some(run_id.clone()),
            status: SessionStatus::Paused,
            input: json!({}),
            output: None,
            call_log: paused.call_log.into_records(),
            error: None,
            pending_seq: Some(first_pending.seq),
            pending_prompt: Some(first_pending.prompt),
            pending_approval: None,
            approvals: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        state.session_store.put(&session).unwrap();

        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "one".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["run_id"], json!(run_id));
        assert_eq!(body["pending_seq"], json!(2));
        assert_eq!(body["pending_prompt"], json!("second?"));

        let manifest = SnapshotStore::new(run_base.join(&run_id))
            .load_manifest()
            .unwrap();
        assert_eq!(manifest.snapshot_kind, SnapshotBlobKind::LiveQuickJsVm);
        assert_eq!(
            manifest.pending.as_ref().map(|pending| pending.id),
            Some(HostOperationId(2))
        );
        assert_eq!(manifest.host_promises.len(), 2);

        let (status, body) = response_json(
            resume_session(
                State(state),
                Path("session-1".to_string()),
                Json(ResumeRequest {
                    response: "two".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["run_id"], json!(run_id));
        assert_eq!(body["output"], json!({ "first": "one", "second": "two" }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_snapshot_validation_rejects_source_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-snapshot-source-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(&agent_path, "export async function agent() { return 1; }").unwrap();

        let store = SnapshotStore::new(run_base.join(run_id));
        let manifest = crate::runtime::snapshot::SnapshotManifest::new(
            run_id,
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default(run_id),
            SourceFingerprint::from_source(
                &agent_path,
                "export async function agent() { return 1; }",
            ),
            Vec::new(),
            None,
            0,
        );
        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();
        std::fs::write(&agent_path, "export async function agent() { return 2; }").unwrap();

        let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path)
            .unwrap_err();
        assert!(err.to_string().contains("runtime snapshot source mismatch"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_snapshot_validation_rejects_module_graph_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-snapshot-module-graph-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let agent_path = temp_dir.join("agent.ts");
        let module_path = temp_dir.join("lib.ts");
        let wrong_module_path = temp_dir.join("other.ts");
        let source = r#"
            import { value } from "./lib.ts";
            export async function agent() { return value; }
        "#;
        let module_source = "export const value = 1;";
        std::fs::write(&agent_path, source).unwrap();
        std::fs::write(&module_path, module_source).unwrap();
        std::fs::write(&wrong_module_path, "export const value = 2;").unwrap();

        let store = SnapshotStore::new(run_base.join(run_id));
        let manifest = crate::runtime::snapshot::SnapshotManifest::new(
            run_id,
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default(run_id),
            SourceFingerprint::from_source(&agent_path, source),
            vec![SourceFingerprint::from_source(&module_path, module_source)],
            None,
            0,
        )
        .with_module_graph(vec![
            crate::runtime::snapshot::SnapshotModuleGraphEntry {
                path: agent_path.clone(),
                imports: vec![crate::runtime::snapshot::SnapshotModuleImport {
                    specifier: "./lib.ts".to_string(),
                    resolved_path: Some(wrong_module_path),
                }],
            },
            crate::runtime::snapshot::SnapshotModuleGraphEntry {
                path: module_path,
                imports: Vec::new(),
            },
        ]);
        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

        let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("runtime snapshot module graph mismatch"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_snapshot_validation_rejects_abi_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-snapshot-abi-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let agent_path = temp_dir.join("agent.ts");
        let source = "export async function agent() { return 1; }";
        std::fs::write(&agent_path, source).unwrap();

        let store = SnapshotStore::new(run_base.join(run_id));
        let manifest = crate::runtime::snapshot::SnapshotManifest::new(
            run_id,
            SnapshotAbi::current("different-fork"),
            RuntimePolicy::durable_default(run_id),
            SourceFingerprint::from_source(&agent_path, source),
            Vec::new(),
            None,
            0,
        );
        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

        let err = validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path)
            .unwrap_err();
        assert!(err.to_string().contains("runtime snapshot ABI mismatch"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_snapshot_validation_accepts_live_vm_manifest() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-snapshot-live-kind-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let agent_path = temp_dir.join("agent.ts");
        let source = "export async function agent() { return 1; }";
        std::fs::write(&agent_path, source).unwrap();

        let store = SnapshotStore::new(run_base.join(run_id));
        let manifest = crate::runtime::snapshot::SnapshotManifest::new(
            run_id,
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default(run_id),
            SourceFingerprint::from_source(&agent_path, source),
            Vec::new(),
            None,
            0,
        )
        .with_snapshot_kind(crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm);
        let snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"snapshot-bytes");
        store
            .save_live_vm_snapshot(&manifest, &snapshot, &[])
            .unwrap();

        validate_snapshot_manifest_for_resume(&run_base, Some(run_id), &agent_path).unwrap();

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_resolves_persisted_pending_input_host_operation() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-resume-host-promise-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let run_dir = run_base.join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();

        let pending = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(7),
            3,
            PendingHostOperationKind::Input,
            json!({ "prompt": "Approve?" }),
        );
        let records = vec![HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Pending,
        }];
        std::fs::write(
            run_dir.join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join(HOST_PROMISE_TABLE_FILE),
            serde_json::to_vec_pretty(&records).unwrap(),
        )
        .unwrap();

        resolve_persisted_pending_host_operation(
            &run_base,
            Some(run_id),
            3,
            PendingHostOperationKind::Input,
            json!("yes"),
        )
        .unwrap();

        assert!(!run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
        let records: Vec<HostPromiseRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
                .unwrap();
        assert_eq!(records.len(), 1);
        match &records[0].state {
            HostPromiseState::Resolved { value, .. } => assert_eq!(value, &json!("yes")),
            other => panic!("expected resolved host promise, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_rejects_persisted_pending_host_operation_missing_from_table() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-resume-missing-host-promise-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let run_dir = run_base.join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();

        let pending = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(7),
            3,
            PendingHostOperationKind::Input,
            json!({ "prompt": "Approve?" }),
        );
        std::fs::write(
            run_dir.join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join(HOST_PROMISE_TABLE_FILE),
            serde_json::to_vec_pretty(&Vec::<HostPromiseRecord>::new()).unwrap(),
        )
        .unwrap();

        let err = resolve_persisted_pending_host_operation(
            &run_base,
            Some(run_id),
            3,
            PendingHostOperationKind::Input,
            json!("yes"),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("missing from persisted host promise table"));
        assert!(run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
        let records: Vec<HostPromiseRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
                .unwrap();
        assert!(records.is_empty());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resume_rejects_already_completed_persisted_pending_host_operation() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-resume-completed-host-promise-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let run_dir = run_base.join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();

        let pending = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(7),
            3,
            PendingHostOperationKind::Input,
            json!({ "prompt": "Approve?" }),
        );
        let records = vec![HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Resolved {
                value: json!("old"),
                completed_at: chrono::Utc::now(),
            },
        }];
        std::fs::write(
            run_dir.join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join(HOST_PROMISE_TABLE_FILE),
            serde_json::to_vec_pretty(&records).unwrap(),
        )
        .unwrap();

        let err = resolve_persisted_pending_host_operation(
            &run_base,
            Some(run_id),
            3,
            PendingHostOperationKind::Input,
            json!("yes"),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("already completed in persisted host promise table"));
        assert!(run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
        let records: Vec<HostPromiseRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
                .unwrap();
        match &records[0].state {
            HostPromiseState::Resolved { value, .. } => assert_eq!(value, &json!("old")),
            other => panic!("expected original resolved host promise, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn approval_allow_resolves_persisted_pending_host_operation() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-approval-allow-host-promise-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let run_dir = run_base.join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();

        let pending = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(9),
            5,
            PendingHostOperationKind::Tool,
            json!({ "name": "deploy", "kwargs": {} }),
        );
        let records = vec![HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Pending,
        }];
        std::fs::write(
            run_dir.join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join(HOST_PROMISE_TABLE_FILE),
            serde_json::to_vec_pretty(&records).unwrap(),
        )
        .unwrap();

        complete_persisted_pending_host_operation(
            &run_base,
            Some(run_id),
            None,
            HostPromiseCompletion::Resolved(json!({
                "approved": true,
                "target": "tool:deploy",
            })),
        )
        .unwrap();

        assert!(!run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
        let records: Vec<HostPromiseRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
                .unwrap();
        assert_eq!(records.len(), 1);
        match &records[0].state {
            HostPromiseState::Resolved { value, .. } => {
                assert_eq!(value["approved"], json!(true));
                assert_eq!(value["target"], json!("tool:deploy"));
            }
            other => panic!("expected resolved host promise, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn approval_deny_rejects_persisted_pending_host_operation() {
        let temp_dir = std::env::temp_dir().join(format!(
            "chidori-server-approval-deny-host-promise-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_base = temp_dir.join(".chidori").join("runs");
        let run_id = "run-1";
        let run_dir = run_base.join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();

        let pending = PendingHostOperation::new(
            crate::runtime::snapshot::HostOperationId(10),
            6,
            PendingHostOperationKind::Http,
            json!({ "url": "https://example.invalid" }),
        );
        let records = vec![HostPromiseRecord {
            operation: pending.clone(),
            state: HostPromiseState::Pending,
        }];
        std::fs::write(
            run_dir.join(PENDING_HOST_OPERATION_FILE),
            serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join(HOST_PROMISE_TABLE_FILE),
            serde_json::to_vec_pretty(&records).unwrap(),
        )
        .unwrap();

        complete_persisted_pending_host_operation(
            &run_base,
            Some(run_id),
            None,
            HostPromiseCompletion::Rejected(
                "policy: `http:https://example.invalid` denied by operator".to_string(),
            ),
        )
        .unwrap();

        assert!(!run_dir.join(PENDING_HOST_OPERATION_FILE).exists());
        let records: Vec<HostPromiseRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join(HOST_PROMISE_TABLE_FILE)).unwrap())
                .unwrap();
        assert_eq!(records.len(), 1);
        match &records[0].state {
            HostPromiseState::Rejected { error, .. } => {
                assert_eq!(
                    error,
                    "policy: `http:https://example.invalid` denied by operator"
                );
            }
            other => panic!("expected rejected host promise, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
