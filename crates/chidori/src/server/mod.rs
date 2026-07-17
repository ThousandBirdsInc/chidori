//! HTTP server exposing agents as a web service: the shared [`AppState`] and
//! its durable-run helpers, plus [`serve`] — the entry point that assembles
//! the router from the sibling modules (sessions, hardening, engine,
//! detached agents, recipes, and the event fallback).

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};

use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::acp::{self, AcpState};
use crate::mcp::{McpManager, McpServersConfig};
use crate::policy::PolicyConfig;
use crate::providers::ProviderRegistry;
use crate::recipes::Recipe;
use crate::runtime::context::RuntimeContext;
use crate::runtime::engine::RunResult;
use crate::runtime::snapshot::{
    HostOperationId, HostPromiseRecord, HostPromiseState, PendingHostOperation,
    PendingHostOperationKind, SnapshotStore, PENDING_HOST_OPERATION_FILE,
};
use crate::runtime::template::TemplateEngine;
use crate::scheduler::{self, SchedulerDeps};
use crate::storage::{build_session_store, SessionStatus, SessionStore, StoredSession};
use crate::tools::{ToolDef, ToolRegistry};

mod detached;
mod engine;
mod events;
mod hardening;
mod recipes;
mod sessions;
#[cfg(test)]
mod tests;

use detached::{
    get_detached_agent, list_detached_agents, send_detached_agent, stop_detached_agent,
};
use engine::run_agent_sync;
use events::handle_event;
use hardening::{
    allow_unauthenticated_from_env, auth_middleware, build_cors_layer, health, is_loopback_host,
};
use recipes::{list_recipes, run_recipe};
use sessions::resume::{approve_session, resume_session, signal_session};
use sessions::stream::stream_session;
use sessions::{
    agent_error_string, arm_signal_timeout, cancel_session, create_session, get_checkpoint,
    get_session, get_snapshot_manifest, list_agents, list_sessions, replay_session, session_policy,
};

// Test-only re-imports: they keep the flat namespace the test module's
// `use super::*` saw when this module was a single file.
#[cfg(test)]
use hardening::bearer_token_matches;
#[cfg(test)]
use sessions::resume::{ApproveRequest, ResumeRequest, SignalRequest};
#[cfg(test)]
use sessions::stream::stamp_attempt;
#[cfg(test)]
use sessions::{resolve_agent_override, CancelSessionRequest, CreateSessionRequest};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    #[allow(dead_code)]
    providers: Arc<ProviderRegistry>,
    template_engine: Arc<TemplateEngine>,
    agent_path: PathBuf,
    /// False when the server was started without an agent file (fleet-only
    /// mode): `agent_path` is then a sentinel and handlers that would run the
    /// default agent must reject with guidance instead.
    has_default_agent: bool,
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
    /// Per-run advisory locks serializing `signals/inbox.json` read-modify-write
    /// (enqueue and the delivery routing decision). A paused run is not a live
    /// task in Phase 1, but the endpoint can still race itself across concurrent
    /// deliveries; this in-process mutex (keyed by run id) makes each delivery's
    /// inbox mutation atomic, matching how the server already serializes per-run
    /// state. See `docs/signals.md` §11.
    signal_inbox_locks: Arc<StdMutex<HashMap<String, Arc<StdMutex<()>>>>>,
    /// Warm-parked runs by SESSION id (`docs/resume-performance.md` §5): while
    /// a run is parked at an `input()` pause its VM stays live on its blocking
    /// thread, and `/resume` delivers the response through `resolution`
    /// instead of replaying the whole history. Entries degrade gracefully —
    /// removing one (cancel, eviction, restart) drops the resolution sender,
    /// the parked bridge wakes with `Park`, and the run unwinds into the
    /// classic replay-resume artifact.
    warm_runs: Arc<StdMutex<HashMap<String, Arc<WarmRun>>>>,
    /// How long a warm-parked run waits for its resume before evicting itself
    /// back to the unwind path (freeing the thread and VM).
    warm_evict: std::time::Duration,
}

/// One session's warm run: the channel its parked engine thread listens on
/// (`Some` exactly while parked at an input pause) and the stream of leg
/// outcomes — one `RunResult` per pause plus the terminal result.
struct WarmRun {
    resolution: StdMutex<Option<std::sync::mpsc::Sender<String>>>,
    outcomes: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<anyhow::Result<RunResult>>>,
}

/// Kill switch: `CHIDORI_WARM_RESUME=0|false|off` restores unwind-and-replay
/// for every pause.
fn warm_resume_enabled() -> bool {
    !matches!(
        std::env::var("CHIDORI_WARM_RESUME").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn warm_evict_from_env() -> std::time::Duration {
    let ms = std::env::var("CHIDORI_WARM_RESUME_EVICT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600_000);
    std::time::Duration::from_millis(ms)
}

/// Register a fresh warm run for `session_id` (replacing any stale entry) and
/// build the [`WarmInputBridge`] its engine leg installs: on an `input()`
/// pause the bridge surfaces a paused `RunResult` on the outcome channel —
/// the exact shape the unwind path returns — and parks the engine thread
/// awaiting the response. The caller spawns the leg and forwards its final
/// result into the same outcome channel, so each HTTP request consumes
/// exactly one outcome.
fn install_warm_run(
    state: &AppState,
    session_id: &str,
) -> (
    Arc<WarmRun>,
    tokio::sync::mpsc::UnboundedSender<anyhow::Result<RunResult>>,
    crate::runtime::context::WarmInputBridge,
) {
    let (outcome_tx, outcome_rx) = tokio::sync::mpsc::unbounded_channel();
    let warm = Arc::new(WarmRun {
        resolution: StdMutex::new(None),
        outcomes: tokio::sync::Mutex::new(outcome_rx),
    });
    state
        .warm_runs
        .lock()
        .unwrap()
        .insert(session_id.to_string(), warm.clone());
    // The bridge holds only a WEAK reference to the entry: the resolution
    // sender lives inside `WarmRun`, so if the bridge kept a strong Arc the
    // parked thread would hold its own wake channel alive and a dropped
    // entry (cancel, restart, server shutdown, test teardown) could never
    // disconnect it. With the weak ref, removing the entry from the map
    // drops the sender and the parked thread wakes with `Park` immediately.
    let bridge_warm = Arc::downgrade(&warm);
    let bridge_outcomes = outcome_tx.clone();
    let evict = state.warm_evict;
    let bridge_run_base = state.run_base.clone();
    let bridge = crate::runtime::context::WarmInputBridge::new(move |ctx, pending| {
        use crate::runtime::context::WarmInputWait;
        // The pause becomes externally visible when the outcome surfaces, so
        // drain the durability barrier first (mirror pipelines included) —
        // the same output gate the unwind path runs at a pause.
        if ctx.flush_store().is_err() {
            return WarmInputWait::Park;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let Some(warm) = bridge_warm.upgrade() else {
                return WarmInputWait::Park;
            };
            *warm.resolution.lock().unwrap() = Some(tx);
            // The Arc drops here: while parked, this thread must NOT keep the
            // entry (and thus its own sender) alive.
        }
        let leg = Ok(RunResult {
            output: Value::Null,
            call_log: ctx.call_log(),
            replayed_calls: ctx.replay_hit_count(),
            run_id: ctx.run_id(),
            paused: Some(pending.clone()),
            paused_approval: None,
            paused_signal: None,
        });
        if bridge_outcomes.send(leg).is_err() {
            if let Some(warm) = bridge_warm.upgrade() {
                *warm.resolution.lock().unwrap() = None;
            }
            return WarmInputWait::Park;
        }
        let outcome = match rx.recv_timeout(evict) {
            Ok(response) => {
                // Reload the durable signal inbox before continuing: a signal
                // delivered while this run was parked landed in
                // `signals/inbox.json` (the server enqueues around paused
                // runs), which the live VM's in-memory inbox cannot see. The
                // file is the union — live drains re-persist it — so the
                // reload is exactly what a replay resume would have seeded.
                let run_id = ctx.run_id();
                ctx.set_signal_inbox(load_persisted_signal_inbox(&bridge_run_base, Some(&run_id)));
                WarmInputWait::Delivered(response)
            }
            // Eviction deadline, or the entry was dropped (cancel/restart/
            // shutdown — the sender disconnects): unwind into the ordinary
            // paused artifact; a later /resume replays. The engine thread
            // and VM are reclaimed.
            Err(_) => WarmInputWait::Park,
        };
        if let Some(warm) = bridge_warm.upgrade() {
            *warm.resolution.lock().unwrap() = None;
        }
        outcome
    });
    (warm, outcome_tx, bridge)
}

/// Drop a session's warm entry when its run settled terminally (or its live
/// leg is gone), so the map holds only parked/parkable runs.
fn release_warm_run_if_settled(state: &AppState, session: &StoredSession) {
    if !matches!(session.status, SessionStatus::Paused) {
        // Completed / Failed / AwaitingApproval / Cancelled: the leg ended (an
        // approval pause always unwinds), so nothing is parked to deliver to.
        state.warm_runs.lock().unwrap().remove(&session.id);
    }
}

impl AppState {
    /// Resolve (creating on first use) the per-run inbox lock for `run_id`. The
    /// caller `.lock()`s the returned `Arc` and holds the guard for the duration
    /// of an enqueue / routing decision.
    fn signal_inbox_lock(&self, run_id: &str) -> Arc<StdMutex<()>> {
        let mut locks = self.signal_inbox_locks.lock().unwrap();
        locks
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(StdMutex::new(())))
            .clone()
    }
}

#[derive(Clone)]
struct ActiveSession {
    cancelled: Arc<AtomicBool>,
    cancel_tx: tokio::sync::mpsc::UnboundedSender<String>,
    attempt_number: Option<u64>,
    /// Live in-memory signal delivery (`docs/signals.md` Phase 3). Present
    /// while a streaming worker supervises this session; the delivery endpoint
    /// enqueues straight into the live run's mailbox and wakes the worker,
    /// skipping the HTTP pause→deliver→resume round-trip.
    signals: Option<LiveSignalSession>,
}

/// The live-delivery handle a streaming worker registers in `active_sessions`.
#[derive(Clone)]
struct LiveSignalSession {
    /// The CURRENT run context of the supervised session. The endpoint
    /// enqueues into this context's in-memory mailbox (which write-through
    /// persists to `signals/inbox.json`), so a running agent sees the signal
    /// at its next listen point with no disk re-read. The worker swaps in the
    /// fresh context before each in-process resume while holding this lock,
    /// so a concurrent delivery always lands in the context that will run.
    ctx_slot: Arc<StdMutex<RuntimeContext>>,
    /// Wake channel into the worker's `select!` loop. Carries the
    /// `delivery_seq` and name of a just-enqueued signal so a worker idling on
    /// a signal pause can resolve it immediately (pending-pause-wins-with-
    /// newest: the woken worker takes *this exact entry* back out of the
    /// mailbox, leaving older queued same-name entries for later listen
    /// points).
    signal_tx: tokio::sync::mpsc::UnboundedSender<(u64, String)>,
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
        "pending_details": s.pending_details,
        "pending_signal_name": s.pending_signal_name,
        "pending_signal_names": s.pending_signal_names,
        "pending_signal_deadline": s.pending_signal_deadline,
        "pending_approval": s.pending_approval,
        "policy_profile": s.policy_profile,
    })
}

fn store_or_500(state: &AppState, session: &StoredSession) -> Option<Response> {
    if let Err(e) = state.session_store.put(session) {
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
    allow_source_change: bool,
) -> anyhow::Result<()> {
    crate::runtime::snapshot::validate_manifest_for_resume(
        run_base,
        run_id,
        agent_path,
        allow_source_change,
    )
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
    // Mutations go through the run's store so a configured durable mirror
    // stays in step with the filesystem layout (`docs/durable-storage.md`).
    let factory = crate::runtime::store::RunStoreFactory::shared(run_base);
    let _ = factory.hydrate(run_id);
    let store = factory.store_for(run_id);
    let Some(pending_bytes) = store.get_blob(PENDING_HOST_OPERATION_FILE)? else {
        return Ok(None);
    };

    let pending: PendingHostOperation = serde_json::from_slice(&pending_bytes)?;
    if let Some((seq, kind)) = expected {
        if pending.seq != seq || pending.kind != kind {
            return Ok(None);
        }
    }

    // Union the compacted table with any per-op blobs written since the last
    // compaction; a delivery is a natural compaction point, so the updated
    // table is folded back to `host_promises.json` and the blobs retired.
    let mut records = crate::runtime::snapshot::load_host_promise_records(store.as_ref())?;
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
    crate::runtime::snapshot::SnapshotStore::with_store(run_base.join(run_id), store.clone())
        .compact_host_promises(&records)?;
    store.delete_blob(PENDING_HOST_OPERATION_FILE)?;
    Ok(Some(pending))
}

// Not yet wired: the reject/timeout side of persisted host-promise completion.
// `resolve_persisted_pending_host_operation` covers today's resolve path; this
// generalizes it to arbitrary completions by operation id.
#[allow(dead_code)]
fn complete_persisted_host_promise_record(
    run_base: &FsPath,
    run_id: Option<&str>,
    id: HostOperationId,
    completion: HostPromiseCompletion,
) -> anyhow::Result<()> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let store = crate::runtime::store::RunStoreFactory::shared(run_base).store_for(run_id);
    let mut records = crate::runtime::snapshot::load_host_promise_records(store.as_ref())?;
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
    crate::runtime::snapshot::SnapshotStore::with_store(run_base.join(run_id), store.clone())
        .compact_host_promises(&records)?;
    Ok(())
}

fn load_persisted_host_promises(
    run_base: &FsPath,
    run_id: Option<&str>,
) -> anyhow::Result<Vec<HostPromiseRecord>> {
    let Some(run_id) = run_id else {
        return Ok(Vec::new());
    };
    // Materialize the run dir from the durable mirror first when this machine
    // has never seen the run (the machine-loss recovery path).
    let factory = crate::runtime::store::RunStoreFactory::shared(run_base);
    let _ = factory.hydrate(run_id);
    // Union of the compacted table and any per-op blobs written since the
    // last compaction (`docs/durable-storage.md`).
    crate::runtime::snapshot::load_host_promise_records(factory.store_for(run_id).as_ref())
}

/// Load the virtual filesystem captured in a run's snapshot manifest so a
/// resumed run sees the file state it had at suspend. Returns an empty VFS if
/// the run has no persisted manifest yet (e.g. it never reached a safepoint).
fn load_persisted_vfs(run_base: &FsPath, run_id: Option<&str>) -> crate::runtime::vfs::Vfs {
    let Some(run_id) = run_id else {
        return crate::runtime::vfs::Vfs::new();
    };
    let _ = crate::runtime::store::RunStoreFactory::shared(run_base).hydrate(run_id);
    match crate::runtime::snapshot::SnapshotStore::new(run_base.join(run_id)).load_manifest() {
        Ok(manifest) => manifest.vfs,
        Err(_) => crate::runtime::vfs::Vfs::new(),
    }
}

/// Load a run's durable signal mailbox (`signals/inbox.json`) so a resumed run
/// can drain queued signals at later listen points (doc §9, sibling of
/// `load_persisted_vfs`). Empty when the run has no inbox file yet.
fn load_persisted_signal_inbox(
    run_base: &FsPath,
    run_id: Option<&str>,
) -> Vec<crate::runtime::snapshot::QueuedSignal> {
    let Some(run_id) = run_id else {
        return Vec::new();
    };
    crate::runtime::context::load_signal_inbox(&run_base.join(run_id))
}

/// Append a signal to a run's durable mailbox under a per-run advisory lock,
/// assigning it the next `delivery_seq` (`max(existing)+1`, starting at 1) so
/// global arrival order across senders is frozen and same-name signals are
/// consumed lowest-first (doc §8.4/§10/§11). The lock is the same per-run mutex
/// the server uses to serialize inbox read-modify-write while a run is paused or
/// running (doc §11: "guard `inbox.json` read-modify-write with a per-run
/// advisory ... lock"). Creates the `signals/` directory if absent.
fn enqueue_signal_to_inbox(
    state: &AppState,
    run_id: &str,
    name: &str,
    payload: Value,
    from: Value,
) -> anyhow::Result<crate::runtime::snapshot::QueuedSignal> {
    use crate::runtime::snapshot::{QueuedSignal, SIGNAL_INBOX_FILE};

    let lock = state.signal_inbox_lock(run_id);
    let _guard = lock.lock().unwrap();
    let factory = crate::runtime::store::RunStoreFactory::shared(&state.run_base);
    let _ = factory.hydrate(run_id);
    let store = factory.store_for(run_id);
    let mut inbox: Vec<QueuedSignal> = match store.get_blob(SIGNAL_INBOX_FILE)? {
        Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        None => Vec::new(),
    };
    let next_delivery_seq = inbox
        .iter()
        .map(|s| s.delivery_seq)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let queued = QueuedSignal {
        name: name.to_string(),
        payload,
        from,
        delivery_seq: next_delivery_seq,
        enqueued_at: chrono::Utc::now(),
    };
    inbox.push(queued.clone());
    store.put_blob(SIGNAL_INBOX_FILE, &serde_json::to_vec_pretty(&inbox)?)?;
    Ok(queued)
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
    agent_path: Option<PathBuf>,
    host: String,
    port: u16,
    policy: Arc<PolicyConfig>,
    policy_posture: String,
) -> anyhow::Result<()> {
    // No agent file → a fleet-only server: it re-arms and drives the
    // detached-agent fleet under the current directory, and every session
    // request must name its agent explicitly. The sentinel path keeps the
    // base-dir/tool-dir derivations working; `has_default_agent` gates the
    // handlers that would otherwise run it.
    let has_default_agent = agent_path.is_some();
    let agent_path =
        agent_path.unwrap_or_else(|| PathBuf::from(".").join("__no_default_agent__.ts"));
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

    // Load the MCP servers, recipes, and session store up front so startup
    // errors happen before we bind the listener. The permission policy is
    // resolved by the caller (CLI flag or CHIDORI_POLICY* env vars).
    let mcp = Arc::new(McpManager::new());
    let mcp_cfg = McpServersConfig::load_from_env().unwrap_or_default();
    let mcp_tools = mcp.start_from_config(&mcp_cfg).await.unwrap_or_else(|e| {
        tracing::warn!("MCP startup: {}", e);
        Vec::new()
    });
    let mcp_tools = Arc::new(mcp_tools);

    let base_dir = agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let session_store = build_session_store(&base_dir)?;
    let run_base = base_dir.join(".chidori").join("runs");

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
            providers: providers.clone(),
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
        has_default_agent,
        run_base,
        session_store,
        policy,
        mcp,
        mcp_tools,
        recipes: recipes_arc,
        run_semaphore: Arc::new(Semaphore::new(max_concurrent)),
        acquire_timeout: std::time::Duration::from_millis(acquire_timeout_ms),
        active_sessions: Arc::new(StdMutex::new(HashMap::new())),
        signal_inbox_locks: Arc::new(StdMutex::new(HashMap::new())),
        warm_runs: Arc::new(StdMutex::new(HashMap::new())),
        warm_evict: warm_evict_from_env(),
    };

    // Re-arm signal-pause timeout timers (`timeoutMs`, `docs/signals.md`
    // Phase 2) for sessions persisted with a deadline by a previous server
    // process. Deadlines already in the past fire (resolve to the timeout
    // sentinel) immediately.
    if let Ok(sessions) = state.session_store.list() {
        for session in &sessions {
            arm_signal_timeout(&state, session);
        }
    }

    // Re-arm the detached-agent fleet (`docs/detached-agents.md`): install the
    // hub's runtime parts, then wake agents that were mid-run when the
    // previous process died and re-arm hibernating agents' alarm deadlines.
    {
        let rt = crate::scheduler::shared_tokio_runtime()?;
        // The registry holds only externally-sourced tools (MCP servers).
        // Agent tools are defined in-VM with `defineTool` and never registered.
        let mut registry = ToolRegistry::new();
        for def in state.mcp_tools.iter() {
            registry.register(def.clone());
        }
        let parts = crate::runtime::host_agent::AgentRuntimeParts {
            providers: state.providers.clone(),
            template_engine: state.template_engine.clone(),
            tokio_rt: rt,
            policy: session_policy(&state, None),
            tools: Arc::new(registry),
            mcp: state.mcp.clone(),
            run_base: state.run_base.clone(),
        };
        match crate::runtime::host_agent::hub().rearm_from_registry(parts) {
            Ok(count) if count > 0 => {
                eprintln!("  Re-armed {count} detached agent(s) from the registry");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("re-arming detached agents: {err}"),
        }
    }

    let auth_required = std::env::var("CHIDORI_API_KEY").is_ok();
    let loopback = is_loopback_host(&host);
    // Fail closed on the dangerous combination: a network-reachable bind with
    // no authentication means anyone who can route to the port can execute
    // agent code. The default bind is loopback, so this only trips when the
    // operator explicitly asked for a wider bind without setting a key.
    if !loopback && !auth_required && !allow_unauthenticated_from_env() {
        anyhow::bail!(
            "refusing to bind {host}:{port} without authentication: a non-loopback bind \
             exposes this server — which executes agent code — to the network with no \
             access control. Either set CHIDORI_API_KEY to require bearer auth, keep the \
             default loopback bind (drop --host / CHIDORI_HOST), or set \
             CHIDORI_ALLOW_UNAUTHENTICATED=1 if a reverse proxy or firewall in front of \
             this server already controls access."
        );
    }
    let cors_layer = build_cors_layer();

    // ACP router owns its own state so session lookups go through the same
    // SessionStore as the rest of the server.
    let acp_runner_state = state.clone();
    let acp_state = AcpState {
        store: state.session_store.clone(),
        run_prompt: Arc::new(move |inputs: Value| -> Result<Value, String> {
            run_agent_sync(&acp_runner_state, inputs)
                .map_err(|e| agent_error_string(&acp_runner_state.agent_path, &e))
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
        .route("/sessions/{id}/signal", post(signal_session))
        .route("/sessions/{id}/approve", post(approve_session))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/sessions/stream", post(stream_session))
        // Example agent discovery — peer directory of the server's
        // configured agent. Lets clients like util-trace-webgl pick an
        // example to run without restarting the server.
        .route("/agents", get(list_agents))
        // Detached durable agents (docs/detached-agents.md): registry
        // listing, status, mailbox delivery (which wakes a hibernating
        // agent), and cooperative stop.
        .route("/agents/detached", get(list_detached_agents))
        .route("/agents/detached/{name}", get(get_detached_agent))
        .route("/agents/detached/{name}/send", post(send_detached_agent))
        .route("/agents/detached/{name}/stop", post(stop_detached_agent))
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

    let addr = format!("{host}:{port}");
    eprintln!("Listening on http://{addr}");
    if !loopback {
        eprintln!(
            "WARNING: non-loopback bind speaks plain HTTP (no TLS). Terminate TLS at a \
             reverse proxy or platform ingress and firewall the port (docs/deployment.md)."
        );
    }
    eprintln!();
    eprintln!(
        "  Concurrency: max {} sessions, {}ms acquire timeout",
        max_concurrent, acquire_timeout_ms
    );
    eprintln!(
        "  Auth:        {}",
        if auth_required {
            "REQUIRED (Authorization: Bearer $CHIDORI_API_KEY)"
        } else if loopback {
            "disabled (loopback bind; set CHIDORI_API_KEY to enable)"
        } else {
            "DISABLED on a network-reachable bind (CHIDORI_ALLOW_UNAUTHENTICATED) — \
             anyone who can reach the port can execute agents"
        }
    );
    eprintln!("  Policy:      {}", policy_posture);
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
    eprintln!("              POST /sessions/{{id}}/signal     → deliver a signal to a run");
    eprintln!("              POST /sessions/{{id}}/cancel     → cancel running session");
    eprintln!("              POST /sessions/stream            → run with SSE events");
    eprintln!("  Health:     GET  /health");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
