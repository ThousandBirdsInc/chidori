//! Session API: session create/list/get, checkpoint and snapshot inspection,
//! replay, cancellation, and the pause/outcome plumbing shared by every run
//! path. The durable resume/signal/approve endpoints live in [`resume`]; the
//! SSE streaming runner lives in [`stream`].

pub(super) mod resume;
pub(super) mod stream;

use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy::PolicyConfig;
use crate::runtime::call_log::CallRecord;
use crate::runtime::engine::RunResult;
use crate::runtime::host_core::signal_timeout_sentinel;
use crate::runtime::snapshot::PendingHostOperationKind;
use crate::storage::{SessionStatus, StoredSession};

use self::resume::{complete_pending_and_resume, signal_resolution_record};
use super::engine::build_engine;
use super::hardening::acquire_run_slot;
use super::{
    complete_persisted_pending_host_operation, install_warm_run, is_supported_agent_filename,
    is_supported_agent_path, load_persisted_host_promises, load_persisted_vfs,
    release_warm_run_if_settled, session_view, snapshot_manifest_for_session, store_or_500,
    warm_resume_enabled, AppState, HostPromiseCompletion,
};

// ---------------------------------------------------------------------------
// Session API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct CreateSessionRequest {
    pub(super) input: Value,
    /// Optional client-selected id. Useful for cancelling a streaming session
    /// before its final `done` event reports the generated id.
    #[serde(default, alias = "sessionId")]
    pub(super) session_id: Option<String>,
    /// Optional generation attempt number stamped onto streaming events.
    #[serde(default, alias = "attemptNumber")]
    pub(super) attempt_number: Option<u64>,
    /// Optional: provide a checkpoint (call log) to replay from.
    #[serde(default)]
    pub(super) replay_from: Option<Vec<CallRecord>>,
    /// Optional: override the server's default agent for this session.
    /// Must be a bare filename (e.g. "hello.ts") resolved against
    /// the parent directory of the server's configured agent_path.
    /// Path traversal is rejected. When unset, the server's default
    /// agent is used.
    #[serde(default)]
    pub(super) agent: Option<String>,
    /// Optional: a built-in policy profile name ("untrusted" or "supervised")
    /// applied to every run of this session, layered on the server policy
    /// with stricter-wins semantics — it can only tighten, never relax, what
    /// the operator configured. Lets a multi-tenant front-end mix trusted
    /// and untrusted callers on one server.
    #[serde(default, alias = "policyProfile")]
    pub(super) policy_profile: Option<String>,
}

/// Validate a client-supplied policy profile name at session creation.
fn validate_policy_profile(requested: Option<&str>) -> Result<(), (StatusCode, String)> {
    match requested {
        None => Ok(()),
        Some(name) if crate::policy::builtin_profile(name).is_some() => Ok(()),
        Some(name) => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "unknown policy profile '{}' (known: {})",
                name,
                crate::policy::BUILTIN_PROFILES.join(", ")
            ),
        )),
    }
}

/// Resolve the effective policy for a session: the server policy, optionally
/// tightened by the session's profile. A stored profile name that no longer
/// resolves (e.g. after a downgrade) fails closed to `untrusted` rather than
/// silently running under the looser server policy.
pub(super) fn session_policy(app: &AppState, profile: Option<&str>) -> Arc<PolicyConfig> {
    let Some(name) = profile else {
        return app.policy.clone();
    };
    let profile_cfg = crate::policy::builtin_profile(name).unwrap_or_else(|| {
        tracing::warn!(
            "session policy profile '{}' is unknown; failing closed to 'untrusted'",
            name
        );
        crate::policy::builtin_profile("untrusted").expect("untrusted profile exists")
    });
    Arc::new(app.policy.restricted_by(Arc::new(profile_cfg)))
}

/// Resolve an optional per-session agent override against the server's
/// configured `agent_path`. Accepts only a bare agent filename in the
/// peer directory — no subdirectories, no `..`, no absolute paths.
/// Returns a `(StatusCode, message)` error suitable for short-circuit
/// rejection when the client passes something invalid.
pub(super) fn resolve_agent_override(
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
pub(super) async fn list_agents(State(state): State<AppState>) -> Response {
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
pub(super) async fn create_session(
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
    if let Err((status, msg)) = validate_policy_profile(body.policy_profile.as_deref()) {
        return (status, Json(json!({"error": msg}))).into_response();
    }
    let policy_profile = body.policy_profile.clone();
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

    let result = if warm_resume_enabled() {
        // Warm mode: run the leg under a supervisor task and consume ONE
        // outcome — either an input pause surfaced by the bridge (the VM
        // stays parked on its thread for `/resume` to continue in place) or
        // the leg's final result.
        let (warm, outcome_tx, bridge) = install_warm_run(&state, &id);
        let leg_input = body.input.clone();
        let leg_profile = body.policy_profile.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let engine =
                    build_engine(&app_state, leg_profile.as_deref()).with_warm_input_bridge(bridge);
                match replay_from {
                    Some(log) => engine.run_replay_pausable(&effective_agent_path, &leg_input, log),
                    None => engine.run_pausable(&effective_agent_path, &leg_input),
                }
            })
            .await
            .unwrap_or_else(|join_err| Err(anyhow::anyhow!("agent run panicked: {join_err}")));
            let _ = outcome_tx.send(result);
        });
        let outcome = {
            let mut outcomes = warm.outcomes.lock().await;
            outcomes.recv().await
        };
        match outcome {
            Some(result) => result,
            None => Err(anyhow::anyhow!("agent run ended without an outcome")),
        }
    } else {
        tokio::task::spawn_blocking(move || {
            let engine = build_engine(&app_state, body.policy_profile.as_deref());
            match replay_from {
                Some(log) => engine.run_replay_pausable(&effective_agent_path, &body.input, log),
                None => engine.run_pausable(&effective_agent_path, &body.input),
            }
        })
        .await
        .unwrap()
    };

    let mut session = StoredSession {
        id: id.clone(),
        run_id: None,
        status: SessionStatus::Failed,
        input,
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile,
        created_at: chrono::Utc::now(),
    };
    match result {
        Ok(run_result) => apply_run_outcome(&mut session, run_result),
        Err(e) => session.error = Some(agent_error_string(&state.agent_path, &e)),
    }

    if let Some(err) = store_or_500(&state, &session) {
        return err;
    }
    arm_signal_timeout(&state, &session);
    release_warm_run_if_settled(&state, &session);
    drop(permit);
    (StatusCode::CREATED, Json(session_view(&session))).into_response()
}

/// GET /sessions — list all sessions.
pub(super) async fn list_sessions(State(state): State<AppState>) -> Response {
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
pub(super) async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
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
pub(super) async fn get_checkpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
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
pub(super) async fn get_snapshot_manifest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
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
pub(super) async fn replay_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
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
    let vfs = load_persisted_vfs(&state.run_base, original.run_id.as_deref());
    let policy_profile = original.policy_profile.clone();
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state, policy_profile.as_deref()).with_approvals(approvals);
        engine.run_with_replay_host_promises_and_vfs(
            &app_state.agent_path,
            &input_clone,
            call_log,
            host_promises,
            vfs,
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
                pending_signal_name: None,
                pending_signal_names: Vec::new(),
                pending_signal_deadline: None,
                pending_approval: None,
                approvals: original.approvals.clone(),
                policy_profile: original.policy_profile.clone(),
                created_at: chrono::Utc::now(),
            };
            if let Some(err) = store_or_500(&state, &session) {
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

#[derive(Deserialize)]
pub(super) struct CancelSessionRequest {
    #[serde(default)]
    pub(super) reason: Option<String>,
}

/// POST /sessions/:id/cancel — mark a session cancelled and notify a live
/// streaming run if this server is still supervising it.
pub(super) async fn cancel_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<CancelSessionRequest>>,
) -> Response {
    let reason = body
        .and_then(|Json(body)| body.reason)
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "session cancelled".to_string());

    let active = state.active_sessions.lock().unwrap().remove(&id);
    // Dropping a warm entry drops its resolution sender: a parked engine
    // thread wakes with `Park` and unwinds, reclaiming the thread and VM.
    state.warm_runs.lock().unwrap().remove(&id);
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
    if let Some(resp) = store_or_500(&state, &session) {
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

/// Render an agent-run error for a session's stored/returned `error` field.
/// Uncaught-exception stack frames arrive from the engine in transpiled
/// coordinates; remap them to positions in the original TypeScript against
/// the served agent's workspace root — the same display-boundary remap the
/// CLI applies in `main::report_cli_error` (which resolves its root from a
/// thread-local these tokio handlers never set). Frames that can't be read
/// or remapped pass through unchanged, as does any error without frames.
pub(super) fn agent_error_string(agent_path: &FsPath, e: &anyhow::Error) -> String {
    let root = crate::runtime::typescript::transpile::find_workspace_root(agent_path);
    crate::runtime::rust_engine::remap_stack_frames_within(&root, &e.to_string())
}

/// Map a finished engine run onto a stored session: status, output, call log,
/// and the pending-pause fields (input prompt / signal listen set + timeout
/// deadline / approval). Shared by session creation, the durable resume tail,
/// and the streaming supervisor so every path surfaces pauses identically.
fn apply_run_outcome(session: &mut StoredSession, run_result: RunResult) {
    session.run_id = Some(run_result.run_id);
    session.call_log = run_result.call_log.into_records();
    session.output = None;
    session.pending_seq = None;
    session.pending_prompt = None;
    session.pending_signal_name = None;
    session.pending_signal_names = Vec::new();
    session.pending_signal_deadline = None;
    session.pending_approval = None;
    if let Some(pending) = run_result.paused {
        session.status = SessionStatus::Paused;
        session.pending_seq = Some(pending.seq);
        session.pending_prompt = Some(pending.prompt);
    } else if let Some(signal) = run_result.paused_signal {
        // A signal listen point with an empty mailbox. Reuse `Paused`;
        // `pending_signal_name(s)` (not the status) marks it as a signal pause
        // so the delivery endpoint can match the name. A `timeoutMs` pause
        // persists its absolute deadline so a timer (or restarted server) can
        // resolve it with the timeout sentinel.
        session.status = SessionStatus::Paused;
        session.pending_seq = Some(signal.seq);
        session.pending_signal_name = Some(signal.name.clone());
        session.pending_signal_names = signal.listen_names();
        session.pending_signal_deadline = signal
            .timeout_ms
            .map(|ms| chrono::Utc::now() + chrono::Duration::milliseconds(ms as i64));
    } else if let Some(appr) = run_result.paused_approval {
        session.status = SessionStatus::AwaitingApproval;
        session.pending_approval = Some(appr);
    } else {
        session.status = SessionStatus::Completed;
        session.output = Some(run_result.output);
    }
}

/// The awaited listen set of a signal-paused session, tolerating sessions
/// persisted before `pending_signal_names` existed (fall back to the single
/// `pending_signal_name`). Empty when the session is not paused on a signal.
fn pending_listen_names(session: &StoredSession) -> Vec<String> {
    if !session.pending_signal_names.is_empty() {
        session.pending_signal_names.clone()
    } else {
        session.pending_signal_name.clone().into_iter().collect()
    }
}

/// Arm the in-process timer for a session just persisted with a signal-pause
/// deadline (`timeoutMs`, `docs/signals.md` Phase 2). No-op when the session
/// has no deadline. The timer re-validates against the stored session before
/// firing, so a delivery that resolves the pause first wins and the timer
/// becomes a no-op.
pub(super) fn arm_signal_timeout(state: &AppState, session: &StoredSession) {
    let Some(deadline) = session.pending_signal_deadline else {
        return;
    };
    if session.status != SessionStatus::Paused {
        return;
    }
    let Some(seq) = session.pending_seq else {
        return;
    };
    let state = state.clone();
    let id = session.id.clone();
    tokio::spawn(async move {
        let wait = (deadline - chrono::Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(wait).await;
        fire_signal_timeout(&state, &id, seq).await;
    });
}

/// Resolve an expired signal pause with the `{ timedOut: true }` sentinel and
/// resume the run — the timer-side twin of `signal_session`'s resolve+resume
/// branch. Validates that the session is still paused on the SAME listen point
/// (a delivery may have already resolved it) and that no live streaming worker
/// owns the session (the Phase 3 supervisor runs its own deadline).
async fn fire_signal_timeout(state: &AppState, id: &str, seq: u64) {
    let Ok(Some(session)) = state.session_store.get(id) else {
        return;
    };
    let names = pending_listen_names(&session);
    if session.status != SessionStatus::Paused
        || session.pending_seq != Some(seq)
        || names.is_empty()
    {
        return;
    }
    if state.active_sessions.lock().unwrap().contains_key(id) {
        return;
    }

    let sentinel = signal_timeout_sentinel(&names);
    let lock = state.signal_inbox_lock(session.run_id.as_deref().unwrap_or(id));
    let completed = {
        let _guard = lock.lock().unwrap();
        complete_persisted_pending_host_operation(
            &state.run_base,
            session.run_id.as_deref(),
            Some((seq, PendingHostOperationKind::Signal)),
            HostPromiseCompletion::Resolved(sentinel.clone()),
        )
    };
    // No matching pending op on disk means the pause was already resolved (or
    // never persisted); either way there is nothing to time out.
    let Ok(Some(pending)) = completed else {
        return;
    };
    let mut call_log = session.call_log.clone();
    call_log.push(signal_resolution_record(&pending, seq, sentinel));
    let _ = complete_pending_and_resume(state, session, call_log).await;
}
