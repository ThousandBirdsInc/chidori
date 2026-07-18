//! POST /sessions/stream — the SSE streaming runner: forwards runtime events
//! as Server-Sent Events and supervises live signal pauses so a delivery (or
//! timeout) resumes the run in-process without an HTTP round-trip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::runtime::context::{InputMode, RuntimeContext, RuntimeEvent};
use crate::runtime::engine::RunResult;
use crate::runtime::host_core::signal_timeout_sentinel;
use crate::runtime::snapshot::PendingHostOperationKind;
use crate::storage::{SessionStatus, StoredSession};

use super::super::engine::build_engine;
use super::super::hardening::acquire_run_slot;
use super::super::{
    complete_persisted_pending_host_operation, load_persisted_host_promises, load_persisted_vfs,
    ActiveSession, AppState, HostPromiseCompletion, LiveSignalSession,
};
use super::resume::signal_resolution_record;
use super::{agent_error_string, apply_run_outcome, validate_policy_profile, CreateSessionRequest};

/// POST /sessions/stream — run the agent and stream each host-function call
/// as a Server-Sent Event while it executes. Final event has `event: done`
/// carrying the session id and output.
pub(in crate::server) fn stamp_attempt(mut value: Value, attempt_number: Option<u64>) -> Value {
    if let Some(attempt_number) = attempt_number {
        if let Some(object) = value.as_object_mut() {
            object.insert("attempt_number".to_string(), json!(attempt_number));
        }
    }
    value
}

/// Serialize a runtime event into its SSE (event name, data) pair — the
/// canonical wire shape shared by the live POST stream, the re-attach log,
/// and the GET replay path.
fn runtime_event_to_sse_parts(
    evt: RuntimeEvent,
    attempt_number: Option<u64>,
) -> (&'static str, String) {
    match evt {
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
    }
}

// ---------------------------------------------------------------------------
// SSE event log: catch-up + live re-attach (GET /sessions/{id}/stream)
// ---------------------------------------------------------------------------

/// The in-memory SSE journal of one live streaming session: every event the
/// POST stream emits is appended here and fanned out on a broadcast channel,
/// so a client whose SSE connection dropped can `GET /sessions/{id}/stream`,
/// replay everything it missed, and follow live from exactly where the
/// replay ends. Appends and subscriptions serialize on one lock — an append
/// broadcasts while holding it and a subscriber snapshots + subscribes while
/// holding it — so no event can be both absent from a snapshot and sent
/// before that subscriber's receiver existed.
pub(in crate::server) struct SessionEventLog {
    inner: StdMutex<SessionEventLogInner>,
    broadcast: tokio::sync::broadcast::Sender<(String, String)>,
}

struct SessionEventLogInner {
    entries: Vec<(String, String)>,
    /// Set once a `done` event lands; later appends are dropped so the
    /// disconnect-cleanup guard can never write past a real terminal event.
    closed: bool,
}

impl SessionEventLog {
    fn new() -> Self {
        let (broadcast, _) = tokio::sync::broadcast::channel(1024);
        Self {
            inner: StdMutex::new(SessionEventLogInner {
                entries: Vec::new(),
                closed: false,
            }),
            broadcast,
        }
    }

    /// Append an event and fan it out to live subscribers. A `done` event
    /// closes the log; appends after close are dropped.
    fn append(&self, name: &str, data: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        if name == "done" {
            inner.closed = true;
        }
        inner.entries.push((name.to_string(), data.to_string()));
        let _ = self.broadcast.send((name.to_string(), data.to_string()));
    }

    /// Everything logged so far, whether the log is closed, and a receiver
    /// positioned exactly after the snapshot (atomic with it, see above).
    fn snapshot_and_subscribe(
        &self,
    ) -> (
        Vec<(String, String)>,
        bool,
        tokio::sync::broadcast::Receiver<(String, String)>,
    ) {
        let inner = self.inner.lock().unwrap();
        (
            inner.entries.clone(),
            inner.closed,
            self.broadcast.subscribe(),
        )
    }
}

/// Dropped when the POST stream's supervisor future ends. On the normal
/// paths a real `done` event has already closed the log and this append is a
/// no-op; if the originating client disconnected mid-run (the SSE body
/// future dropped without reaching a terminal event), it closes the log with
/// a synthetic `done` so re-attached clients don't hang on a feed nobody
/// supervises anymore.
struct EventLogCloseGuard {
    log: Arc<SessionEventLog>,
    session_id: String,
}

impl Drop for EventLogCloseGuard {
    fn drop(&mut self) {
        let data = json!({
            "id": self.session_id,
            "status": "failed",
            "error": "the originating stream disconnected before the run settled; \
                      check GET /sessions/{id} for the session's current state",
        });
        self.log.append("done", &data.to_string());
    }
}

/// The terminal SSE `done` payload for a session's current state — one
/// mapping shared by the live POST stream's final event and the GET
/// re-attach replay of a settled session. `Running` only occurs on the GET
/// path (a session with no live event log to follow, e.g. created via POST
/// /sessions): the stream reports the status honestly and closes.
fn final_session_event(session: &StoredSession) -> Value {
    match session.status {
        SessionStatus::Completed => json!({
            "id": session.id,
            "status": "completed",
            "output": session.output,
        }),
        SessionStatus::Paused if !session.pending_signal_names.is_empty() => json!({
            "id": session.id,
            "status": "paused",
            "pending_seq": session.pending_seq,
            "pending_signal_name": session.pending_signal_name,
            "pending_signal_names": session.pending_signal_names,
            "pending_signal_deadline": session.pending_signal_deadline,
        }),
        SessionStatus::Paused => json!({
            "id": session.id,
            "status": "paused",
            "pending_seq": session.pending_seq,
            "pending_prompt": session.pending_prompt,
            "pending_details": session.pending_details,
        }),
        SessionStatus::AwaitingApproval => json!({
            "id": session.id,
            "status": "awaiting_approval",
            "pending_approval": session.pending_approval,
        }),
        SessionStatus::Cancelled => json!({
            "id": session.id,
            "status": "cancelled",
            "error": session.error,
        }),
        SessionStatus::Running => json!({
            "id": session.id,
            "status": "running",
        }),
        SessionStatus::Failed => json!({
            "id": session.id,
            "status": "failed",
            "error": session.error,
        }),
    }
}

/// GET /sessions/{id}/stream — (re-)attach to a session's SSE event stream.
///
/// For a session with a live streaming supervisor (started via POST
/// /sessions/stream and not yet settled), replays every event already logged
/// — so a client whose connection dropped catches up — then follows the live
/// broadcast until the run settles (`done`). For any other stored session,
/// replays the logged call records as `call` events and closes with a `done`
/// event carrying the session's current state. Same SSE event format as the
/// POST stream; same bearer auth as every session route (applied by the
/// router-wide middleware).
pub(in crate::server) async fn attach_session_stream(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    // Live re-attach: snapshot + subscribe atomically, then stream.
    let live_log = state
        .active_sessions
        .lock()
        .unwrap()
        .get(&id)
        .and_then(|active| active.event_log.clone());
    if let Some(log) = live_log {
        let (snapshot, closed, mut rx) = log.snapshot_and_subscribe();
        let stream = async_stream::stream! {
            for (name, data) in snapshot {
                yield Ok::<_, std::convert::Infallible>(Event::default().event(name).data(data));
            }
            if !closed {
                loop {
                    match rx.recv().await {
                        Ok((name, data)) => {
                            let is_done = name == "done";
                            yield Ok::<_, std::convert::Infallible>(
                                Event::default().event(name).data(data),
                            );
                            if is_done {
                                break;
                            }
                        }
                        // Lagged: this attacher fell >capacity events behind;
                        // skip the overwritten ones and keep following.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        };
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }

    // No live supervisor: replay the stored session's journal, then close
    // with its current state.
    let session = match state.session_store.get(&id) {
        Ok(Some(session)) => session,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let stream = async_stream::stream! {
        for record in &session.call_log {
            let data = serde_json::to_string(&json!(record)).unwrap_or_else(|_| "{}".into());
            yield Ok::<_, std::convert::Infallible>(Event::default().event("call").data(data));
        }
        let data = serde_json::to_string(&final_session_event(&session))
            .unwrap_or_else(|_| "{}".into());
        yield Ok::<_, std::convert::Infallible>(Event::default().event("done").data(data));
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Spawn one blocking agent run for the streaming supervisor, reporting the
/// engine result back on `result_tx`. Holds a clone of the shared run permit
/// for the duration of the run.
fn spawn_streaming_run(
    state: &AppState,
    policy_profile: Option<String>,
    ctx: RuntimeContext,
    input: Value,
    result_tx: tokio::sync::mpsc::UnboundedSender<anyhow::Result<RunResult>>,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) {
    let app_state = state.clone();
    let agent_path = state.agent_path.clone();
    tokio::task::spawn_blocking(move || {
        let _run_permit = permit;
        let engine = build_engine(&app_state, policy_profile.as_deref());
        let result = engine.run_with_prepared_context(&agent_path, &input, ctx);
        let _ = result_tx.send(result);
    });
}

/// Resolve a supervised signal pause in-process and kick off the resumed run
/// (`docs/signals.md` Phase 3 — the fast resume trigger that skips the HTTP
/// `/resume` round-trip). Completes the persisted pending `Signal` op with
/// `value` (a delivered `{name,payload,from}` or the timeout sentinel),
/// appends the synthetic resolution record, swaps a fresh replay context into
/// the live slot — carrying over the in-memory mailbox so a delivery racing
/// this resume is not lost — persists the session as Running, and spawns the
/// blocking re-run, which reports back on `result_tx`. Returns false (leaving
/// the pause supervised) when no matching pending op exists on disk.
#[allow(clippy::too_many_arguments)]
fn resume_signal_pause_in_process(
    state: &AppState,
    session: &mut StoredSession,
    ctx_slot: &Arc<StdMutex<RuntimeContext>>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    result_tx: &tokio::sync::mpsc::UnboundedSender<anyhow::Result<RunResult>>,
    permit: &Arc<tokio::sync::OwnedSemaphorePermit>,
    policy_profile: Option<String>,
    seq: u64,
    value: Value,
) -> bool {
    let run_id = session.run_id.clone().unwrap_or_else(|| session.id.clone());
    let completed = {
        let lock = state.signal_inbox_lock(&run_id);
        let _guard = lock.lock().unwrap();
        complete_persisted_pending_host_operation(
            &state.run_base,
            session.run_id.as_deref(),
            Some((seq, PendingHostOperationKind::Signal)),
            HostPromiseCompletion::Resolved(value.clone()),
        )
    };
    let Ok(Some(pending)) = completed else {
        return false;
    };
    let resolution = signal_resolution_record(&pending, seq, value);
    // Emit the consumed signal on the live stream. The resumed run replays
    // the log (replayed records deliberately re-emit nothing), so without
    // this the one record that carries `{name, payload, from}` — who steered
    // the run — would never reach a dashboard watching the SSE stream.
    let _ = event_tx.send(RuntimeEvent::Call(resolution.clone()));
    session.call_log.push(resolution);

    let host_promises = load_persisted_host_promises(&state.run_base, session.run_id.as_deref())
        .unwrap_or_default();
    let vfs = load_persisted_vfs(&state.run_base, session.run_id.as_deref());

    // Swap the resumed run's context into the live slot while holding it: the
    // delivery endpoint enqueues into whatever context the slot currently
    // names, so carrying the old context's in-memory mailbox into the new one
    // under the lock means no delivery can fall between the two.
    let ctx = {
        let mut slot = ctx_slot.lock().unwrap();
        let inbox = slot.signal_inbox();
        let ctx = RuntimeContext::with_replay_host_promises_vfs_and_signals(
            session.call_log.clone(),
            host_promises,
            vfs,
            inbox,
        );
        ctx.set_run_id(run_id);
        ctx.set_input_mode(InputMode::Pause);
        ctx.set_event_sender(event_tx.clone());
        *slot = ctx.clone();
        ctx
    };

    session.status = SessionStatus::Running;
    session.pending_seq = None;
    session.pending_signal_name = None;
    session.pending_signal_names = Vec::new();
    session.pending_signal_deadline = None;
    let _ = state.session_store.put(session);

    spawn_streaming_run(
        state,
        policy_profile,
        ctx,
        session.input.clone(),
        result_tx.clone(),
        permit.clone(),
    );
    true
}

pub(in crate::server) async fn stream_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    use tokio::sync::mpsc;

    // Gate on the concurrency semaphore. If we can't get a permit within
    // the acquire deadline, 503 before any streaming response headers are
    // committed so clients see the overflow cleanly. The permit is shared
    // (Arc) between the supervisor stream and each blocking run, so the slot
    // stays held across in-process signal resumes and is released when the
    // last holder drops.
    let permit = match acquire_run_slot(&state).await {
        Ok(p) => Arc::new(p),
        Err(resp) => return resp,
    };

    if let Err((status, msg)) = validate_policy_profile(body.policy_profile.as_deref()) {
        return (status, Json(json!({"error": msg}))).into_response();
    }
    if !state.has_default_agent {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "this server was started without an agent file \
                (fleet-only mode); streaming sessions need a default agent — restart the \
                server with an agent path"}),
            ),
        )
            .into_response();
    }
    let policy_profile = body.policy_profile.clone();
    // Warning-only static effect preflight under the session's tightened
    // policy — mirrors create_session (see server/preflight.rs).
    if let Some(profile) = policy_profile.as_deref() {
        super::super::preflight::warn_denied_static_effects(
            &state.agent_path,
            &super::session_policy(&state, Some(profile)),
            &format!("session policy profile '{profile}'"),
        );
    }

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

    let (event_tx, mut event_rx) =
        mpsc::unbounded_channel::<crate::runtime::context::RuntimeEvent>();
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<anyhow::Result<RunResult>>();
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<String>();
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<(u64, String)>();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Build the first run's context up front so the run id is known before the
    // agent starts and the delivery endpoint can enqueue into the live
    // in-memory mailbox from the first instant (`docs/signals.md` Phase 3).
    // Pause mode: an `input()` or approval gate surfaces as a paused session
    // (handed to the durable HTTP endpoints) instead of blocking on stdin.
    let ctx = RuntimeContext::new();
    ctx.set_event_sender(event_tx.clone());
    ctx.set_input_mode(InputMode::Pause);
    let run_id = ctx.run_id();
    let ctx_slot = Arc::new(StdMutex::new(ctx.clone()));

    // Journal every SSE event this stream emits so a dropped client can
    // re-attach via GET /sessions/{id}/stream, catch up, and follow live.
    let event_log = Arc::new(SessionEventLog::new());

    state.active_sessions.lock().unwrap().insert(
        session_id.clone(),
        ActiveSession {
            cancelled: cancelled.clone(),
            cancel_tx,
            attempt_number,
            signals: Some(LiveSignalSession {
                ctx_slot: ctx_slot.clone(),
                signal_tx,
            }),
            event_log: Some(event_log.clone()),
        },
    );
    let mut session = StoredSession {
        id: session_id.clone(),
        run_id: Some(run_id),
        status: SessionStatus::Running,
        input: input.clone(),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_details: None,
        pending_signal_name: None,
        pending_signal_names: Vec::new(),
        pending_signal_deadline: None,
        pending_approval: None,
        approvals: Vec::new(),
        policy_profile: policy_profile.clone(),
        created_at: chrono::Utc::now(),
    };
    let _ = state.session_store.put(&session);

    spawn_streaming_run(
        &state,
        policy_profile.clone(),
        ctx,
        input.clone(),
        result_tx.clone(),
        permit.clone(),
    );

    let state_for_stream = state.clone();
    let stream = async_stream::stream! {
        // The supervisor's share of the run slot (see acquire above).
        let permit = permit;
        // Closes the re-attach log with a synthetic `done` if this future is
        // dropped (client disconnect) before a real terminal event lands.
        let _close_guard = EventLogCloseGuard {
            log: event_log.clone(),
            session_id: session.id.clone(),
        };
        // The signal pause currently supervised: (pending seq, listen set).
        // While set, the run idles and a matching delivery (or the timeout
        // deadline) resumes it in-process without an HTTP round-trip.
        let mut supervising: Option<(u64, Vec<String>)> = None;
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                Some(evt) = event_rx.recv() => {
                    let (name, data) = runtime_event_to_sse_parts(evt, attempt_number);
                    event_log.append(name, &data);
                    yield Ok::<_, std::convert::Infallible>(Event::default().event(name).data(data));
                }
                Some(reason) = cancel_rx.recv() => {
                    cancelled.store(true, Ordering::SeqCst);
                    state_for_stream.active_sessions.lock().unwrap().remove(&session.id);
                    // A run idling on a supervised signal pause has no blocking
                    // task left to notice the flag — persist the cancellation
                    // here. A still-executing run persists it when it returns.
                    if supervising.is_some() {
                        session.status = SessionStatus::Cancelled;
                        session.error = Some(reason.clone());
                        let _ = state_for_stream.session_store.put(&session);
                    }
                    let final_event = stamp_attempt(json!({
                        "id": session.id,
                        "status": "cancelled",
                        "error": reason,
                    }), attempt_number);
                    let data = serde_json::to_string(&final_event).unwrap_or_else(|_| "{}".into());
                    event_log.append("done", &data);
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("done").data(data));
                    break;
                }
                Some((delivery_seq, name)) = signal_rx.recv() => {
                    // A delivery landed while we supervise. If it matches the
                    // pause we're idling on, apply the pinned tie-break
                    // (pending-pause-wins-with-newest): take THIS exact entry
                    // back out of the mailbox and resolve the pause with it,
                    // leaving older queued entries for later listen points.
                    // Otherwise it stays durably queued for a future drain.
                    if let Some((seq, names)) = supervising.clone() {
                        if names.iter().any(|n| n == &name) {
                            let entry = ctx_slot.lock().unwrap()
                                .take_queued_signal_by_delivery_seq(delivery_seq);
                            if let Some(entry) = entry {
                                let value = json!({
                                    "name": entry.name,
                                    "payload": entry.payload,
                                    "from": entry.from,
                                });
                                if resume_signal_pause_in_process(
                                    &state_for_stream, &mut session, &ctx_slot,
                                    &event_tx, &result_tx, &permit,
                                    policy_profile.clone(), seq, value,
                                ) {
                                    supervising = None;
                                    deadline = None;
                                }
                            }
                        }
                    }
                }
                _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                    // `timeoutMs` deadline passed with no matching delivery:
                    // resolve the supervised pause with the timeout sentinel.
                    if let Some((seq, names)) = supervising.clone() {
                        let sentinel = signal_timeout_sentinel(&names);
                        if resume_signal_pause_in_process(
                            &state_for_stream, &mut session, &ctx_slot,
                            &event_tx, &result_tx, &permit,
                            policy_profile.clone(), seq, sentinel,
                        ) {
                            supervising = None;
                        }
                    }
                    deadline = None;
                }
                Some(result) = result_rx.recv() => {
                    let was_cancelled = cancelled.load(Ordering::SeqCst);
                    match result {
                        Ok(run_result) => apply_run_outcome(&mut session, run_result),
                        Err(e) => {
                            session.status = SessionStatus::Failed;
                            session.output = None;
                            session.error =
                                Some(agent_error_string(&state_for_stream.agent_path, &e));
                        }
                    }
                    if was_cancelled {
                        session.status = SessionStatus::Cancelled;
                        session.output = None;
                        session.error = Some("session cancelled".to_string());
                    }
                    let _ = state_for_stream.session_store.put(&session);

                    if session.status == SessionStatus::Paused
                        && !session.pending_signal_names.is_empty()
                    {
                        // A signal listen point: stay live. Announce the pause,
                        // then either drain a signal that arrived while the run
                        // was unwinding (mailbox order: lowest delivery_seq
                        // first) or idle until a delivery/timeout resumes us.
                        let names = session.pending_signal_names.clone();
                        let seq = session.pending_seq.unwrap_or_default();
                        let paused_event = stamp_attempt(json!({
                            "id": session.id,
                            "status": "paused",
                            "pending_seq": seq,
                            "pending_signal_name": session.pending_signal_name,
                            "pending_signal_names": names,
                            "pending_signal_deadline": session.pending_signal_deadline,
                        }), attempt_number);
                        let data = serde_json::to_string(&paused_event).unwrap_or_else(|_| "{}".into());
                        event_log.append("paused", &data);
                        yield Ok::<_, std::convert::Infallible>(Event::default().event("paused").data(data));

                        let queued = ctx_slot.lock().unwrap().take_queued_signal_any(&names);
                        let mut resumed = false;
                        if let Some(entry) = queued {
                            let value = json!({
                                "name": entry.name,
                                "payload": entry.payload,
                                "from": entry.from,
                            });
                            resumed = resume_signal_pause_in_process(
                                &state_for_stream, &mut session, &ctx_slot,
                                &event_tx, &result_tx, &permit,
                                policy_profile.clone(), seq, value,
                            );
                        }
                        if resumed {
                            supervising = None;
                            deadline = None;
                        } else {
                            supervising = Some((seq, names));
                            deadline = session.pending_signal_deadline.map(|d| {
                                let wait = (d - chrono::Utc::now()).to_std().unwrap_or_default();
                                tokio::time::Instant::now() + wait
                            });
                        }
                        continue;
                    }

                    // Anything else ends live supervision: terminal states
                    // close the stream, and input/approval pauses hand off to
                    // the durable HTTP resume/approve endpoints.
                    state_for_stream.active_sessions.lock().unwrap().remove(&session.id);
                    let final_event = final_session_event(&session);
                    let data = serde_json::to_string(&stamp_attempt(final_event, attempt_number)).unwrap_or_else(|_| "{}".into());
                    event_log.append("done", &data);
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("done").data(data));
                    break;
                }
                else => {
                    state_for_stream.active_sessions.lock().unwrap().remove(&session.id);
                    break;
                },
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
