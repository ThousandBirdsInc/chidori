//! Durable pause completion: the resume/signal/approve endpoints and their
//! shared tail, `complete_pending_and_resume`, which resolves a persisted
//! pending host operation and replay-resumes the run.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::runtime::call_log::CallRecord;
use crate::runtime::snapshot::{PendingHostOperation, PendingHostOperationKind};
use crate::storage::{SessionStatus, StoredSession};

use super::super::engine::build_engine;
use super::super::{
    complete_persisted_pending_host_operation, enqueue_signal_to_inbox, install_warm_run,
    load_persisted_host_promises, load_persisted_signal_inbox, load_persisted_vfs,
    release_warm_run_if_settled, session_view, store_or_500, validate_snapshot_manifest_for_resume,
    warm_resume_enabled, AppState, HostPromiseCompletion,
};
use super::{agent_error_string, apply_run_outcome, arm_signal_timeout, pending_listen_names};

/// The synthetic CallRecord a server-side signal resolution injects at the
/// pending seq, so the replaying engine returns the delivered value (or the
/// timeout sentinel) to the agent's listen call. Uses the persisted pending
/// op's function name and match-key args, so a `signal_any` pause replays as
/// `signal_any` with its `{names}` key and a `signal` pause as `signal` with
/// `{name}`.
pub(super) fn signal_resolution_record(
    pending: &PendingHostOperation,
    seq: u64,
    value: Value,
) -> CallRecord {
    CallRecord {
        seq,
        parent_seq: None,
        function: pending
            .function
            .clone()
            .unwrap_or_else(|| "signal".to_string()),
        args: pending.args.clone(),
        result: value,
        duration_ms: 0,
        token_usage: None,
        timestamp: chrono::Utc::now(),
        error: None,
    }
}

/// Shared tail of `resume_session` and `signal_session` (doc §9: "factor its
/// shared tail into `complete_pending_and_resume(...)`"). The caller has already
/// (1) resolved the persisted pending host operation and (2) appended the
/// synthetic resume `CallRecord` (an `input` record for resume, a `signal`
/// record for signal delivery) at the pending seq into `call_log`. This helper
/// performs the common re-run: load the host-promise table, VFS, and signal
/// mailbox; replay-run the agent (preserving the run id and per-session policy
/// profile + approvals); and map the outcome back onto `original`, surfacing a
/// fresh input/signal/approval pause or completion. Returns the HTTP `Response`.
///
/// Threading the signal inbox here is what lets a resumed run that reaches a
/// *second* `signal(name)`/`pollSignal(name)` listen point drain a queued entry
/// instead of pausing (doc §9, §3 of the stage spec).
pub(super) async fn complete_pending_and_resume(
    state: &AppState,
    original: StoredSession,
    call_log: Vec<CallRecord>,
) -> Response {
    let input = original.input.clone();
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
    let vfs = load_persisted_vfs(&state.run_base, original.run_id.as_deref());
    let signal_inbox = load_persisted_signal_inbox(&state.run_base, original.run_id.as_deref());
    let resume_run_id = original.run_id.clone();
    let policy_profile = original.policy_profile.clone();
    let app_state = state.clone();

    // The resumed leg gets its own warm bridge (when enabled), so a LATER
    // `input()` pause in the continuation parks the live VM for the next
    // /resume instead of unwinding — every replay-based resume upgrades the
    // session back onto the warm path.
    let warm = if warm_resume_enabled() {
        Some(install_warm_run(state, &original.id))
    } else {
        None
    };
    let run_leg = move |bridge: Option<crate::runtime::context::WarmInputBridge>| {
        let mut engine =
            build_engine(&app_state, policy_profile.as_deref()).with_approvals(approvals);
        if let Some(bridge) = bridge {
            engine = engine.with_warm_input_bridge(bridge);
        }
        // Continue under the original run id (when known) so the resumed run
        // keeps its persisted run directory and stays a single durable run,
        // matching the live-VM resume path. Falls back to a fresh id only when
        // the session never recorded one.
        match resume_run_id {
            Some(run_id) => engine
                .run_replay_pausable_with_host_promises_vfs_signals_preserving_run_id(
                    &app_state.agent_path,
                    &input,
                    call_log,
                    host_promises,
                    vfs,
                    signal_inbox,
                    run_id,
                ),
            None => engine.run_replay_pausable_with_host_promises_vfs_and_signals(
                &app_state.agent_path,
                &input,
                call_log,
                host_promises,
                vfs,
                signal_inbox,
            ),
        }
    };

    let result = match warm {
        Some((warm, outcome_tx, bridge)) => {
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || run_leg(Some(bridge)))
                    .await
                    .unwrap_or_else(|join_err| {
                        Err(anyhow::anyhow!("agent run panicked: {join_err}"))
                    });
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
        }
        None => tokio::task::spawn_blocking(move || run_leg(None))
            .await
            .unwrap(),
    };

    let mut session = original;
    match result {
        Ok(run_result) => {
            // A re-run that reached a NEW pause persists it exactly like the
            // initial run does (the pending op + host promise table + shrunken
            // inbox were already written to disk by the runtime safepoints).
            apply_run_outcome(&mut session, run_result);
            if let Some(err) = store_or_500(state, &session) {
                return err;
            }
            arm_signal_timeout(state, &session);
            release_warm_run_if_settled(state, &session);
            (StatusCode::OK, Json(session_view(&session))).into_response()
        }
        Err(e) => {
            let error = agent_error_string(&state.agent_path, &e);
            session.status = SessionStatus::Failed;
            session.error = Some(error.clone());
            let _ = state.session_store.put(&session);
            state.warm_runs.lock().unwrap().remove(&session.id);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    }
}

/// POST /sessions/:id/resume — supply a response to the agent's pending
/// `input()` call and continue the run. Body: `{"response": "<string>"}`,
/// plus optional `"allow_source_change": true` — the edit-and-resume opt-in
/// that lets the resume proceed when the agent source changed since the run
/// was recorded (replay's divergence checks still guard the journaled calls).
#[derive(Deserialize)]
pub(in crate::server) struct ResumeRequest {
    pub(in crate::server) response: String,
    #[serde(default)]
    pub(in crate::server) allow_source_change: bool,
}

pub(in crate::server) async fn resume_session(
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

    // Warm fast path: this session's VM is parked on its thread awaiting
    // exactly this response — deliver it and await the leg's next outcome
    // (the next pause or the terminal result). The parked engine records the
    // same journal entry the synthetic replay injection below produces, so
    // the durable artifacts are identical either way. Falls back to the
    // replay path when nothing is parked (server restart, eviction, kill
    // switch) — that path re-derives everything from the journal.
    let warm_entry = state.warm_runs.lock().unwrap().get(&id).cloned();
    if let Some(warm) = warm_entry {
        let parked = warm.resolution.lock().unwrap().take();
        match parked {
            Some(tx) if tx.send(body.response.clone()).is_ok() => {
                let result = {
                    let mut outcomes = warm.outcomes.lock().await;
                    outcomes.recv().await
                };
                let mut session = original;
                match result {
                    Some(Ok(run_result)) => apply_run_outcome(&mut session, run_result),
                    Some(Err(e)) => {
                        session.status = SessionStatus::Failed;
                        session.error = Some(agent_error_string(&state.agent_path, &e));
                    }
                    None => {
                        session.status = SessionStatus::Failed;
                        session.error = Some("agent run ended without an outcome".to_string());
                    }
                }
                if let Some(err) = store_or_500(&state, &session) {
                    return err;
                }
                arm_signal_timeout(&state, &session);
                release_warm_run_if_settled(&state, &session);
                return (StatusCode::OK, Json(session_view(&session))).into_response();
            }
            _ => {
                // Not parked (evicted, unwound, or a racing resume won):
                // retire the stale entry and take the replay path.
                state.warm_runs.lock().unwrap().remove(&id);
            }
        }
    }

    if let Err(err) = validate_snapshot_manifest_for_resume(
        &state.run_base,
        original.run_id.as_deref(),
        &state.agent_path,
        body.allow_source_change,
    ) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }

    let _completed_pending = match complete_persisted_pending_host_operation(
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

    complete_pending_and_resume(&state, original, call_log).await
}

/// POST /sessions/:id/signal — deliver a signal `{ name, payload, from }` to a
/// run (`docs/signals.md` §9). `name` is a required string; `payload` is any
/// JSON (default null); `from` is an optional provenance object (default null).
///
/// Routing by run state (doc §9 table):
///   * Paused waiting on THIS name → resolve the pending `Signal` op with
///     `{name,payload,from}`, inject a synthetic `signal` CallRecord, and resume
///     via `complete_pending_and_resume` (the same machinery `/resume` uses).
///   * Paused on a different name / on input / on approval, or Running → enqueue
///     into `signals/inbox.json` (drained at the next matching listen point),
///     202 Accepted.
///   * Completed / Failed / Cancelled → 409 Conflict, NO inbox write (an orphan
///     inbox would mislead a later replay).
#[derive(Deserialize)]
pub(in crate::server) struct SignalRequest {
    pub(in crate::server) name: String,
    #[serde(default)]
    pub(in crate::server) payload: Value,
    #[serde(default)]
    pub(in crate::server) from: Value,
    /// Edit-and-resume opt-in: allow the signal-triggered resume even though
    /// the agent source changed since the run was recorded.
    #[serde(default)]
    pub(in crate::server) allow_source_change: bool,
}

pub(in crate::server) async fn signal_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SignalRequest>,
) -> Response {
    if body.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "signal requires a non-empty `name`"})),
        )
            .into_response();
    }

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

    // Terminal runs reject delivery with no inbox write.
    if matches!(
        original.status,
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
    ) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("session is {:?}; cannot accept signals", original.status),
            })),
        )
            .into_response();
    }

    // Phase 3 (`docs/signals.md`): a live streaming worker supervises this
    // session — deliver in-memory. The signal is write-through enqueued into
    // the live run's mailbox (durably mirrored to `signals/inbox.json` in the
    // same critical section) and the worker is woken; a run mid-execution
    // drains it at its next listen point, and a run idling on a matching
    // listen point is resolved+resumed in-process by the worker, skipping the
    // HTTP pause→resume round-trip.
    let live = state
        .active_sessions
        .lock()
        .unwrap()
        .get(&id)
        .and_then(|active| active.signals.clone());
    if let Some(live) = live {
        let queued = {
            let ctx = live.ctx_slot.lock().unwrap();
            ctx.enqueue_live_signal(&body.name, body.payload.clone(), body.from.clone())
        };
        let _ = live
            .signal_tx
            .send((queued.delivery_seq, queued.name.clone()));
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "id": id,
                "status": "delivered_live",
                "name": queued.name,
                "delivery_seq": queued.delivery_seq,
            })),
        )
            .into_response();
    }

    // Paused waiting on THIS name (or a `signalAny` listen set containing it):
    // resolve the pending pause with the newly arrived signal and resume.
    // Tie-break (doc §11, pinned decision): "pending-pause-wins-with-newest" —
    // when the run is paused on name X and a same-name entry is ALSO already
    // queued in the inbox, the pending pause resolves with THIS just-delivered
    // signal; the older queued entry stays in the inbox (threaded into the
    // resumed run by `complete_pending_and_resume`) for the next listen point.
    let waiting_on_this_name = original.status == SessionStatus::Paused
        && pending_listen_names(&original)
            .iter()
            .any(|n| n == &body.name);

    if waiting_on_this_name {
        let Some(seq) = original.pending_seq else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Signal-paused session has no pending seq"})),
            )
                .into_response();
        };

        if let Err(err) = validate_snapshot_manifest_for_resume(
            &state.run_base,
            original.run_id.as_deref(),
            &state.agent_path,
            body.allow_source_change,
        ) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }

        // The recorded signal result freezes `{name,payload,from}` (doc §8.3) —
        // the match key on disk is `{name}` only.
        let value = json!({
            "name": body.name,
            "payload": body.payload,
            "from": body.from,
        });

        // Serialize against concurrent deliveries to the same run while we
        // resolve the pending op + mutate the inbox-adjacent durable state.
        let lock = state.signal_inbox_lock(original.run_id.as_deref().unwrap_or(&id));
        let _guard = lock.lock().unwrap();

        let completed = complete_persisted_pending_host_operation(
            &state.run_base,
            original.run_id.as_deref(),
            Some((seq, PendingHostOperationKind::Signal)),
            HostPromiseCompletion::Resolved(value.clone()),
        );
        match completed {
            // The pending op matched a `Signal` at this seq: inject the
            // synthetic resolution record (a `signal` or `signal_any` record,
            // taken from the persisted op) and resume (reusing the resume tail).
            Ok(Some(pending)) => {
                drop(_guard);
                let mut call_log = original.call_log.clone();
                call_log.push(signal_resolution_record(&pending, seq, value));
                complete_pending_and_resume(&state, original, call_log).await
            }
            // No matching pending op on disk (e.g. nothing persisted). Fall back
            // to enqueueing so the signal is not lost.
            Ok(None) => {
                drop(_guard);
                enqueue_and_respond(&state, &original, &id, body)
            }
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err.to_string()})),
            )
                .into_response(),
        }
    } else {
        // Paused on a different name / input / approval, or Running: enqueue.
        enqueue_and_respond(&state, &original, &id, body)
    }
}

/// Enqueue a delivered signal into the run's durable mailbox and return a
/// 202 Accepted body describing the assigned `delivery_seq`. Shared by the
/// "paused-on-other / running" branch and the pending-op-missing fallback.
fn enqueue_and_respond(
    state: &AppState,
    original: &StoredSession,
    id: &str,
    body: SignalRequest,
) -> Response {
    let run_id = match original.run_id.as_deref() {
        Some(run_id) => run_id,
        None => {
            // No run directory yet (e.g. a Running session that hasn't recorded
            // a run id). Key the mailbox by session id so it is still durable and
            // is picked up once the run threads its inbox.
            id
        }
    };
    match enqueue_signal_to_inbox(state, run_id, &body.name, body.payload, body.from) {
        Ok(queued) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "id": id,
                "status": "queued",
                "name": queued.name,
                "delivery_seq": queued.delivery_seq,
            })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// POST /sessions/:id/approve — approve (or deny) a policy-gated call that
/// paused the run. On approve, the (target, args) is appended to the session's
/// approvals list and the agent is replayed; the pre-seeded PolicyCache makes
/// the previously-blocked call pass through. On deny, the session transitions
/// to failed.
#[derive(Deserialize)]
pub(in crate::server) struct ApproveRequest {
    /// "allow" or "deny". Defaults to "allow" for convenience.
    #[serde(default = "default_decision")]
    pub(in crate::server) decision: String,
    /// Edit-and-resume opt-in: allow the post-approval resume even though the
    /// agent source changed since the run was recorded.
    #[serde(default)]
    pub(in crate::server) allow_source_change: bool,
}

fn default_decision() -> String {
    "allow".to_string()
}

pub(in crate::server) async fn approve_session(
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
        body.allow_source_change,
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

    // Allow path: record the approval. Durability is call-log replay, so the
    // resume below replays the recorded log with the approval seeded — prior
    // host calls return their recorded results and only the blocked call
    // re-executes.
    original
        .approvals
        .push((pending.target.clone(), pending.args.clone()));
    original.pending_approval = None;

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
    let call_log = original.call_log.clone();
    let resume_run_id = original.run_id.clone();
    let vfs = load_persisted_vfs(&state.run_base, original.run_id.as_deref());
    // Thread the signal mailbox so an approved run that reaches a `signal(name)`
    // listen point drains a queued entry instead of pausing (doc §9, §3).
    let signal_inbox = load_persisted_signal_inbox(&state.run_base, original.run_id.as_deref());
    let policy_profile = original.policy_profile.clone();
    let app_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state, policy_profile.as_deref()).with_approvals(approvals);
        // Replay the recorded call log (so any host calls the agent made before
        // the policy block — e.g. a prior `input()` — return their recorded
        // results instead of pausing again) with the approval seeded in the
        // policy cache. The blocked call itself was never recorded, so its seq
        // is past the log and it executes live, now passing the seeded policy.
        // Host promises are intentionally empty so the gated call runs for real
        // rather than replaying a placeholder resolution. Preserve the run id so
        // the resumed run keeps its persisted directory.
        match resume_run_id {
            Some(run_id) => engine
                .run_replay_pausable_with_host_promises_vfs_signals_preserving_run_id(
                    &app_state.agent_path,
                    &input,
                    call_log,
                    Vec::new(),
                    vfs,
                    signal_inbox,
                    run_id,
                ),
            None => engine.run_replay_pausable_with_host_promises_vfs_and_signals(
                &app_state.agent_path,
                &input,
                call_log,
                Vec::new(),
                vfs,
                signal_inbox,
            ),
        }
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
                session.pending_signal_name = None;
            } else if let Some(signal) = run_result.paused_signal {
                session.status = SessionStatus::Paused;
                session.pending_seq = Some(signal.seq);
                session.pending_prompt = None;
                session.pending_signal_name = Some(signal.name);
            } else if let Some(appr) = run_result.paused_approval {
                session.status = SessionStatus::AwaitingApproval;
                session.pending_approval = Some(appr);
            } else {
                session.status = SessionStatus::Completed;
                session.output = Some(run_result.output);
            }
            session.call_log = run_result.call_log.into_records();
            if let Some(err) = store_or_500(&state, &session) {
                return err;
            }
            (StatusCode::OK, Json(session_view(&session))).into_response()
        }
        Err(e) => {
            let error = agent_error_string(&state.agent_path, &e);
            session.status = SessionStatus::Failed;
            session.error = Some(error.clone());
            let _ = state.session_store.put(&session);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            )
                .into_response()
        }
    }
}
