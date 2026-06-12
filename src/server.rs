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
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, Any as CorsAny, CorsLayer};

use crate::acp::{self, AcpState};
use crate::mcp::{McpManager, McpServersConfig};
use crate::policy::PolicyConfig;
use crate::providers::ProviderRegistry;
use crate::recipes::Recipe;
use crate::runtime::call_log::CallRecord;
use crate::runtime::context::{InputMode, RuntimeContext, RuntimeEvent};
use crate::runtime::engine::{Engine, RunResult};
use crate::runtime::host_core::signal_timeout_sentinel;
use crate::runtime::snapshot::{
    HostOperationId, HostPromiseRecord, HostPromiseState, PendingHostOperation,
    PendingHostOperationKind, RuntimePolicy, SnapshotAbi, SnapshotStore, SourceFingerprint,
    HOST_PROMISE_TABLE_FILE, PENDING_HOST_OPERATION_FILE,
};
use crate::runtime::template::TemplateEngine;
use crate::scheduler::{self, SchedulerDeps};
use crate::storage::{build_session_store, SessionStatus, SessionStore, StoredSession};
use crate::tools::{ToolDef, ToolRegistry};

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
    /// Per-run advisory locks serializing `signals/inbox.json` read-modify-write
    /// (enqueue and the delivery routing decision). A paused run is not a live
    /// task in Phase 1, but the endpoint can still race itself across concurrent
    /// deliveries; this in-process mutex (keyed by run id) makes each delivery's
    /// inbox mutation atomic, matching how the server already serializes per-run
    /// state. See `docs/signals.md` §11.
    signal_inbox_locks: Arc<StdMutex<HashMap<String, Arc<StdMutex<()>>>>>,
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

const PROMPT_TOOL_PAUSE_FILE: &str = "prompt_tool_pause.json";

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
        crate::runtime::typescript::module_graph::snapshot_module_graph(
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

/// Load the virtual filesystem captured in a run's snapshot manifest so a
/// resumed run sees the file state it had at suspend. Returns an empty VFS if
/// the run has no persisted manifest yet (e.g. it never reached a safepoint).
fn load_persisted_vfs(run_base: &FsPath, run_id: Option<&str>) -> crate::runtime::vfs::Vfs {
    let Some(run_id) = run_id else {
        return crate::runtime::vfs::Vfs::new();
    };
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
    let run_dir = state.run_base.join(run_id);
    let inbox_path = run_dir.join(SIGNAL_INBOX_FILE);
    let mut inbox: Vec<QueuedSignal> = if inbox_path.exists() {
        serde_json::from_slice(&std::fs::read(&inbox_path)?).unwrap_or_default()
    } else {
        Vec::new()
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
    if let Some(parent) = inbox_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&inbox_path, serde_json::to_vec_pretty(&inbox)?)?;
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
    agent_path: PathBuf,
    port: u16,
    policy: Arc<PolicyConfig>,
    policy_posture: String,
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
        signal_inbox_locks: Arc::new(StdMutex::new(HashMap::new())),
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
        .route("/sessions/{id}/signal", post(signal_session))
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
/// Build an engine for one run of a session. `policy_profile` is the
/// session's stored profile (if any), layered on the server policy — passing
/// it here (rather than only at create time) keeps the tightened policy in
/// force across resume/approve/replay re-runs of the same session.
fn build_engine(app: &AppState, policy_profile: Option<&str>) -> Engine {
    let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
    // Reuse the app's provider registry so a replay-based resume sees the same
    // providers as the live-VM resume path (which drives `state.providers`
    // directly). In production this is the env-derived registry passed to
    // `serve`; re-deriving it here would drop any test-injected providers and
    // break resume parity between the two paths.
    let providers = app.providers.clone();
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
        .with_policy(session_policy(app, policy_profile))
        .with_mcp(app.mcp.clone())
        .with_persist_base(app.run_base.clone())
}

/// Synchronous one-shot runner used by the ACP endpoint. Runs the agent on
/// the current thread (already inside spawn_blocking) and returns the output
/// JSON. Any error is bubbled as an anyhow::Error.
fn run_agent_sync(app: &AppState, inputs: Value) -> anyhow::Result<Value> {
    let engine = build_engine(app, None);
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
    /// Optional: a built-in policy profile name ("untrusted" or "supervised")
    /// applied to every run of this session, layered on the server policy
    /// with stricter-wins semantics — it can only tighten, never relax, what
    /// the operator configured. Lets a multi-tenant front-end mix trusted
    /// and untrusted callers on one server.
    #[serde(default, alias = "policyProfile")]
    policy_profile: Option<String>,
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
fn session_policy(app: &AppState, profile: Option<&str>) -> Arc<PolicyConfig> {
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

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state, body.policy_profile.as_deref());
        match replay_from {
            Some(log) => engine.run_replay_pausable(&effective_agent_path, &body.input, log),
            None => engine.run_pausable(&effective_agent_path, &body.input),
        }
    })
    .await
    .unwrap();

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
        Err(e) => session.error = Some(e.to_string()),
    }

    if let Some(err) = store_or_500(&state, &session) {
        return err;
    }
    arm_signal_timeout(&state, &session);
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
    session
        .call_log
        .push(signal_resolution_record(&pending, seq, value));

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

async fn stream_session(
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
    let policy_profile = body.policy_profile.clone();

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
        // The signal pause currently supervised: (pending seq, listen set).
        // While set, the run idles and a matching delivery (or the timeout
        // deadline) resumes it in-process without an HTTP round-trip.
        let mut supervising: Option<(u64, Vec<String>)> = None;
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                Some(evt) = event_rx.recv() => {
                    yield Ok::<_, std::convert::Infallible>(runtime_event_to_sse_event(evt, attempt_number));
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
                            session.error = Some(e.to_string());
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
                    let final_event = match session.status {
                        SessionStatus::Completed => json!({
                            "id": session.id,
                            "status": "completed",
                            "output": session.output,
                        }),
                        SessionStatus::Paused => json!({
                            "id": session.id,
                            "status": "paused",
                            "pending_seq": session.pending_seq,
                            "pending_prompt": session.pending_prompt,
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
                        _ => json!({
                            "id": session.id,
                            "status": "failed",
                            "error": session.error,
                        }),
                    };
                    let data = serde_json::to_string(&stamp_attempt(final_event, attempt_number)).unwrap_or_else(|_| "{}".into());
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
fn arm_signal_timeout(state: &AppState, session: &StoredSession) {
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

/// The synthetic CallRecord a server-side signal resolution injects at the
/// pending seq, so the replaying engine returns the delivered value (or the
/// timeout sentinel) to the agent's listen call. Uses the persisted pending
/// op's function name and match-key args, so a `signal_any` pause replays as
/// `signal_any` with its `{names}` key and a `signal` pause as `signal` with
/// `{name}`.
fn signal_resolution_record(pending: &PendingHostOperation, seq: u64, value: Value) -> CallRecord {
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
async fn complete_pending_and_resume(
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

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state, policy_profile.as_deref()).with_approvals(approvals);
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
    })
    .await
    .unwrap();

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
struct SignalRequest {
    name: String,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    from: Value,
}

async fn signal_session(
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

    // Allow path: record the approval. The live-VM resume below handles the
    // QuickJS path by resuming the frozen VM; if that doesn't apply (e.g. the
    // rust engine, whose durability is call-log replay), the fallback further
    // down replays the recorded log with the approval seeded so prior host
    // calls return their results and only the blocked call re-executes.
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
        let engine = build_engine(&app_state, None);
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
            signal_inbox_locks: Arc::new(StdMutex::new(HashMap::new())),
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
                signals: None,
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
                pending_signal_name: None,
                pending_signal_names: Vec::new(),
                pending_signal_deadline: None,
                pending_approval: None,
                approvals: Vec::new(),
                policy_profile: None,
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
                ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
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

    fn policy_test_project(name: &str, agent_source: &str) -> (PathBuf, AppState) {
        let temp_dir = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(&agent_path, agent_source).unwrap();
        let run_base = temp_dir.join(".chidori").join("runs");
        let state = test_state(run_base, agent_path);
        (temp_dir, state)
    }

    fn create_request(policy_profile: Option<&str>) -> CreateSessionRequest {
        CreateSessionRequest {
            input: json!({}),
            session_id: None,
            attempt_number: None,
            replay_from: None,
            agent: None,
            policy_profile: policy_profile.map(ToOwned::to_owned),
        }
    }

    const HTTP_AGENT: &str = r#"
        export async function agent(input, chidori) {
            const res = await chidori.http({ url: "https://example.invalid/" });
            return { res };
        }
    "#;

    #[tokio::test]
    async fn create_session_rejects_unknown_policy_profile() {
        let (temp_dir, state) = policy_test_project("chidori-server-policy-unknown", HTTP_AGENT);

        let (status, body) = response_json(
            create_session(State(state), Json(create_request(Some("nonsense")))).await,
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("unknown policy profile 'nonsense'") && error.contains("untrusted"),
            "expected an unknown-profile error listing the builtins, got: {error}"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn serve_default_profile_denies_sessions_and_explains_the_opt_out() {
        // The policy `chidori serve` resolves when the operator configured
        // nothing: sessions are deny-by-default and the denial tells the
        // operator how to relax the posture.
        let (temp_dir, mut state) =
            policy_test_project("chidori-server-policy-serve-default", HTTP_AGENT);
        state.policy = Arc::new(crate::policy::serve_default_profile());

        let (status, body) =
            response_json(create_session(State(state), Json(create_request(None))).await).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["status"], json!("failed"));
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("policy: `http` denied") && error.contains("--trusted"),
            "expected an actionable deny-by-default error, got: {error}"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn untrusted_session_denies_gated_effects_despite_permissive_server_policy() {
        let (temp_dir, state) = policy_test_project("chidori-server-policy-untrusted", HTTP_AGENT);
        // The server policy is the permissive default (AlwaysAllow); the
        // session profile must tighten it, not the other way around.

        let (status, body) = response_json(
            create_session(State(state), Json(create_request(Some("untrusted")))).await,
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["status"], json!("failed"));
        assert_eq!(body["policy_profile"], json!("untrusted"));
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("policy: `http` denied"),
            "expected the http call to be denied, got: {error}"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn supervised_session_pauses_for_approval_then_operator_denies() {
        let (temp_dir, state) = policy_test_project("chidori-server-policy-supervised", HTTP_AGENT);

        let (status, body) = response_json(
            create_session(
                State(state.clone()),
                Json(create_request(Some("supervised"))),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["status"], json!("awaitingapproval"));
        assert_eq!(body["policy_profile"], json!("supervised"));
        assert_eq!(body["pending_approval"]["target"], json!("http"));
        let id = body["id"].as_str().unwrap().to_string();

        // Operator denies: the session fails without the call executing.
        let (status, body) = response_json(
            approve_session(
                State(state),
                Path(id),
                Json(ApproveRequest {
                    decision: "deny".to_string(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("failed"));
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("denied by operator"),
            "expected an operator-denied error, got: {error}"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    // -----------------------------------------------------------------------
    // Signal delivery (`docs/signals.md` §9–§11) — Stage 2.
    // -----------------------------------------------------------------------

    /// Build a state whose run_base is the agent's `.chidori/runs` dir (so a run
    /// pausing on a signal persists into the same tree the delivery endpoint
    /// reads), with a generous acquire timeout for the real engine runs.
    fn signal_test_state(temp_dir: &FsPath, agent_path: PathBuf) -> AppState {
        let run_base = temp_dir.join(".chidori").join("runs");
        std::fs::create_dir_all(&run_base).unwrap();
        let mut state = test_state(run_base, agent_path);
        state.run_semaphore = Arc::new(Semaphore::new(4));
        state.acquire_timeout = std::time::Duration::from_secs(30);
        state
    }

    fn write_agent(temp_dir: &FsPath, source: &str) -> PathBuf {
        std::fs::create_dir_all(temp_dir).unwrap();
        let agent_path = temp_dir.join("agent.ts");
        std::fs::write(&agent_path, source).unwrap();
        agent_path
    }

    async fn create_paused_session(state: &AppState, id: &str, input: Value) -> Value {
        let (_status, body) = response_json(
            create_session(
                State(state.clone()),
                Json(
                    serde_json::from_value(json!({
                        "input": input,
                        "session_id": id,
                    }))
                    .unwrap(),
                ),
            )
            .await,
        )
        .await;
        body
    }

    /// Signal delivered to a run paused waiting on THIS name resolves the pause
    /// and resumes to completion; the final output and the session view reflect
    /// the delivered payload.
    #[tokio::test]
    async fn signal_to_paused_waiting_this_name_resolves_and_resumes() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-resolve-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const review = await chidori.signal("review");
                    return { decision: review.payload.decision, by: review.from.id };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "s-signal-1", json!({})).await;
        assert_eq!(created["status"], json!("paused"));
        assert_eq!(created["pending_signal_name"], json!("review"));

        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("s-signal-1".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "approve" }),
                    from: json!({ "kind": "human", "id": "mara" }),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "decision": "approve", "by": "mara" })
        );

        let stored = state.session_store.get("s-signal-1").unwrap().unwrap();
        assert_eq!(stored.status, SessionStatus::Completed);
        assert_eq!(stored.pending_signal_name, None);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// Signal delivered to a run paused on `input()` (a different pause) is
    /// enqueued; the run stays paused; after the input is resumed, the agent's
    /// later `signal(name)` drains the queued entry without pausing again.
    #[tokio::test]
    async fn signal_to_input_paused_enqueues_then_drains_after_resume() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-enqueue-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const name = await chidori.input("who?");
                    const review = await chidori.signal("review");
                    return { name, decision: review.payload.decision };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "s-signal-2", json!({})).await;
        assert_eq!(created["status"], json!("paused"));
        // This is an input() pause, not a signal pause.
        assert_eq!(created["pending_signal_name"], Value::Null);
        let run_id = created["run_id"].as_str().unwrap().to_string();

        // Deliver a "review" signal while paused on input → must enqueue.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("s-signal-2".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "changes" }),
                    from: json!({ "id": "bot" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["status"], json!("queued"));
        assert_eq!(body["delivery_seq"], json!(1));

        // inbox.json exists with the entry, and the run is still paused.
        let inbox = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].name, "review");
        let still_paused = state.session_store.get("s-signal-2").unwrap().unwrap();
        assert_eq!(still_paused.status, SessionStatus::Paused);

        // Resume the input(); the later signal() drains the queued entry.
        let (status, body) = response_json(
            resume_session(
                State(state.clone()),
                Path("s-signal-2".to_string()),
                Json(ResumeRequest {
                    response: "ada".to_string(),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "name": "ada", "decision": "changes" })
        );

        // Inbox was drained to empty by consumption.
        let drained = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
        assert!(
            drained.is_empty(),
            "inbox should be drained, got {drained:?}"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// Signal delivered to a completed run → 409 Conflict and NO inbox file.
    #[tokio::test]
    async fn signal_to_completed_run_conflicts_with_no_inbox() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-409-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent() { return { ok: true }; }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "s-signal-3", json!({})).await;
        assert_eq!(created["status"], json!("completed"));
        let run_id = created["run_id"].as_str().unwrap().to_string();

        let (status, _body) = response_json(
            signal_session(
                State(state.clone()),
                Path("s-signal-3".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: Value::Null,
                    from: Value::Null,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let inbox_path = state
            .run_base
            .join(&run_id)
            .join(crate::runtime::snapshot::SIGNAL_INBOX_FILE);
        assert!(
            !inbox_path.exists(),
            "completed run must not have an inbox written"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// Determinism: enqueue-before-listen and pause-then-deliver produce the
    /// identical recorded `signal` CallRecord and final output. Both paths use
    /// the same agent body and deliver the same `{name,payload,from}`; only the
    /// arrival timing differs (queued-then-drained vs pending-pause-resolved).
    #[tokio::test]
    async fn signal_enqueue_before_listen_matches_pause_then_deliver() {
        let payload = json!({ "decision": "approve" });
        let from = json!({ "kind": "human", "id": "mara" });

        // Path A: pause-then-deliver. The agent reaches `signal("review")` with
        // an empty mailbox, pauses, and the delivery resolves the pending pause.
        let source_a = r#"
            export async function agent(input, chidori) {
                const review = await chidori.signal("review");
                return { decision: review.payload.decision, by: review.from.id };
            }
        "#;
        let dir_a =
            std::env::temp_dir().join(format!("chidori-signal-det-a-{}", uuid::Uuid::new_v4()));
        let agent_a = write_agent(&dir_a, source_a);
        let state_a = signal_test_state(&dir_a, agent_a);
        create_paused_session(&state_a, "a", json!({})).await;
        response_json(
            signal_session(
                State(state_a.clone()),
                Path("a".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: payload.clone(),
                    from: from.clone(),
                }),
            )
            .await,
        )
        .await;
        let stored_a = state_a.session_store.get("a").unwrap().unwrap();

        // Path B: enqueue-before-listen. The agent first pauses on `input()`; we
        // deliver "review" while it is paused (so the signal is ENQUEUED, never a
        // pending-pause), then resume the input(). When the agent reaches
        // `signal("review")` it drains the queued entry WITHOUT pausing. The
        // recorded signal value must be identical to Path A.
        let source_b = r#"
            export async function agent(input, chidori) {
                await chidori.input("gate");
                const review = await chidori.signal("review");
                return { decision: review.payload.decision, by: review.from.id };
            }
        "#;
        let dir_b =
            std::env::temp_dir().join(format!("chidori-signal-det-b-{}", uuid::Uuid::new_v4()));
        let agent_b = write_agent(&dir_b, source_b);
        let state_b = signal_test_state(&dir_b, agent_b);
        create_paused_session(&state_b, "b", json!({})).await;
        // Deliver while paused on input → enqueued.
        let (qstatus, _) = response_json(
            signal_session(
                State(state_b.clone()),
                Path("b".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: payload.clone(),
                    from: from.clone(),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(qstatus, StatusCode::ACCEPTED);
        // Resume the input(); the later signal() drains the queued entry.
        response_json(
            resume_session(
                State(state_b.clone()),
                Path("b".to_string()),
                Json(ResumeRequest {
                    response: "go".to_string(),
                }),
            )
            .await,
        )
        .await;
        let stored_b = state_b.session_store.get("b").unwrap().unwrap();

        // Identical recorded signal CallRecord (result + match-key args) and
        // identical final output, regardless of arrival timing.
        let sig_a = stored_a
            .call_log
            .iter()
            .find(|r| r.function == "signal")
            .unwrap();
        let sig_b = stored_b
            .call_log
            .iter()
            .find(|r| r.function == "signal")
            .unwrap();
        assert_eq!(sig_a.result, sig_b.result);
        assert_eq!(sig_a.args, sig_b.args);
        assert_eq!(stored_a.output, stored_b.output);

        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
    }

    /// Tie-break (`docs/signals.md` §11, pinned "pending-pause-wins-with-newest"):
    /// a queued same-name entry already in the inbox PLUS a pending pause on that
    /// name PLUS a new delivery → the pause resolves with the NEW payload, and
    /// the older queued entry survives for a later listen point.
    #[tokio::test]
    async fn signal_tie_break_pending_pause_wins_with_newest() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-tiebreak-{}", uuid::Uuid::new_v4()));
        // Two sequential review listen points: the first consumes the new
        // delivery (pause wins), the second drains the older queued entry.
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const first = await chidori.signal("review");
                    const second = await chidori.signal("review");
                    return { first: first.payload.tag, second: second.payload.tag };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "tie", json!({})).await;
        let run_id = created["run_id"].as_str().unwrap().to_string();
        assert_eq!(created["pending_signal_name"], json!("review"));

        // Pre-seed an OLDER queued same-name entry directly into the inbox while
        // the run is paused on the first `review`.
        enqueue_signal_to_inbox(
            &state,
            &run_id,
            "review",
            json!({ "tag": "old-queued" }),
            json!({ "id": "queued-sender" }),
        )
        .unwrap();

        // Deliver a NEW review: the pending pause must resolve with THIS one.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("tie".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "tag": "new-delivered" }),
                    from: json!({ "id": "live-sender" }),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        // First listen point got the NEW payload; second drained the OLD queued.
        assert_eq!(
            body["output"],
            json!({ "first": "new-delivered", "second": "old-queued" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// End-to-end: an agent with two sequential `signal("review")` calls; two
    /// deliveries; both recorded in order with their delivered payloads.
    #[tokio::test]
    async fn signal_two_sequential_listen_points_record_in_order() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-two-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const a = await chidori.signal("review");
                    const b = await chidori.signal("review");
                    return { a: a.payload.round, b: b.payload.round };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        create_paused_session(&state, "two", json!({})).await;

        // First delivery resolves the first listen point; the run re-pauses on
        // the second `review`.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("two".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "round": 1 }),
                    from: json!({ "id": "r1" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("paused"));
        assert_eq!(body["pending_signal_name"], json!("review"));

        // Second delivery resolves the second listen point → completion.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("two".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "round": 2 }),
                    from: json!({ "id": "r2" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["output"], json!({ "a": 1, "b": 2 }));

        // Both signal records present, in seq order, with their payloads.
        let stored = state.session_store.get("two").unwrap().unwrap();
        let signals: Vec<_> = stored
            .call_log
            .iter()
            .filter(|r| r.function == "signal")
            .collect();
        assert_eq!(signals.len(), 2);
        assert!(signals[0].seq < signals[1].seq);
        assert_eq!(signals[0].result["payload"], json!({ "round": 1 }));
        assert_eq!(signals[1].result["payload"], json!({ "round": 2 }));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// Pure replay (`/sessions/{id}/replay`) must NOT consume the inbox: a replay
    /// with a non-empty inbox leaves the inbox file unchanged and reproduces the
    /// identical output (the recorded `signal` call short-circuits before the
    /// mailbox drain — the determinism contract, `docs/signals.md` §10).
    #[tokio::test]
    async fn replay_does_not_consume_inbox() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-replay-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const review = await chidori.signal("review");
                    return { decision: review.payload.decision };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        // Drive a run to completion via a delivered signal.
        let created = create_paused_session(&state, "replay-src", json!({})).await;
        let run_id = created["run_id"].as_str().unwrap().to_string();
        response_json(
            signal_session(
                State(state.clone()),
                Path("replay-src".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "approve" }),
                    from: json!({ "id": "x" }),
                }),
            )
            .await,
        )
        .await;
        let completed = state.session_store.get("replay-src").unwrap().unwrap();
        assert_eq!(completed.status, SessionStatus::Completed);

        // Seed a non-empty inbox under the run dir; replay must ignore it.
        enqueue_signal_to_inbox(
            &state,
            &run_id,
            "review",
            json!({ "decision": "SHOULD-NOT-BE-USED" }),
            json!({ "id": "ghost" }),
        )
        .unwrap();
        let inbox_before = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
        assert_eq!(inbox_before.len(), 1);

        let (status, body) = response_json(
            replay_session(State(state.clone()), Path("replay-src".to_string())).await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        // Replay reproduces the recorded decision, not the ghost inbox entry.
        assert_eq!(body["output"], json!({ "decision": "approve" }));

        // Inbox file is untouched by replay.
        let inbox_after = load_persisted_signal_inbox(&state.run_base, Some(&run_id));
        assert_eq!(inbox_after, inbox_before);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    // -----------------------------------------------------------------------
    // Phase 2: signalAny + timeoutMs (`docs/signals.md` §14 Phase 2).
    // -----------------------------------------------------------------------

    /// Poll the session store until `id` reaches `status` (the timeout timers
    /// and streaming supervisor advance sessions asynchronously).
    async fn wait_for_status(state: &AppState, id: &str, status: SessionStatus) -> StoredSession {
        for _ in 0..400 {
            if let Ok(Some(s)) = state.session_store.get(id) {
                if s.status == status {
                    return s;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("session {id} never reached {status:?}");
    }

    /// `chidori.signalAny([..])` pauses on the whole listen set, advertises it
    /// in the session view, and a delivery matching ANY listed name resolves
    /// the pause; the recorded call replays as `signal_any` with its `{names}`
    /// match key and the bare fired signal as result.
    #[tokio::test]
    async fn signal_any_pauses_on_set_and_resolves_on_any_listed_name() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-any-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const fired = await chidori.signalAny(["review", "steer"]);
                    return { fired: fired.name, payload: fired.payload, by: fired.from.id };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "any-1", json!({})).await;
        assert_eq!(created["status"], json!("paused"));
        assert_eq!(created["pending_signal_name"], json!("review"));
        assert_eq!(created["pending_signal_names"], json!(["review", "steer"]));

        // Deliver the SECOND listed name — it must resolve the pause.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("any-1".to_string()),
                Json(SignalRequest {
                    name: "steer".to_string(),
                    payload: json!({ "dir": "left" }),
                    from: json!({ "kind": "human", "id": "sam" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(
            body["output"],
            json!({ "fired": "steer", "payload": { "dir": "left" }, "by": "sam" })
        );

        // The synthetic record uses the persisted op's function + match key.
        let stored = state.session_store.get("any-1").unwrap().unwrap();
        let record = stored
            .call_log
            .iter()
            .find(|r| r.function == "signal_any")
            .expect("signal_any record");
        assert_eq!(record.args, json!({ "names": ["review", "steer"] }));
        assert_eq!(record.result["name"], json!("steer"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A `timeoutMs` signal pause persists its deadline, and the armed server
    /// timer resolves it with the `{timedOut: true}` sentinel; a replay of the
    /// timed-out session reproduces the sentinel from the recorded call.
    #[tokio::test]
    async fn signal_timeout_resolves_with_sentinel_and_replays() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-timeout-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const result = await chidori.signal("review", { timeoutMs: 150 });
                    if (result.timedOut) {
                        return { timedOut: true, name: result.name };
                    }
                    return { timedOut: false, decision: result.payload.decision };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "to-1", json!({})).await;
        assert_eq!(created["status"], json!("paused"));
        assert!(
            !created["pending_signal_deadline"].is_null(),
            "timeoutMs pause must persist its deadline: {created}"
        );

        // No delivery: the armed timer fires and resolves the sentinel.
        let stored = wait_for_status(&state, "to-1", SessionStatus::Completed).await;
        assert_eq!(
            stored.output,
            Some(json!({ "timedOut": true, "name": "review" }))
        );
        let record = stored
            .call_log
            .iter()
            .find(|r| r.function == "signal")
            .expect("signal record");
        assert_eq!(record.result["timedOut"], json!(true));

        // Replay reproduces the sentinel deterministically.
        let (status, body) =
            response_json(replay_session(State(state.clone()), Path("to-1".to_string())).await)
                .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            body["output"],
            json!({ "timedOut": true, "name": "review" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A delivery that lands before the `timeoutMs` deadline wins; the timer
    /// later fires as a no-op (the pause is already resolved).
    #[tokio::test]
    async fn signal_delivery_before_timeout_wins() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-race-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const result = await chidori.signal("review", { timeoutMs: 60000 });
                    if (result.timedOut) {
                        return { timedOut: true };
                    }
                    return { timedOut: false, decision: result.payload.decision };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        create_paused_session(&state, "race-1", json!({})).await;
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("race-1".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "approve" }),
                    from: json!({ "id": "mara" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["output"],
            json!({ "timedOut": false, "decision": "approve" })
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A timed-out multi-name `signalAny` resolves to the sentinel with a null
    /// `name` (no name fired).
    #[tokio::test]
    async fn signal_any_timeout_sentinel_has_null_name() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-signal-any-to-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const result = await chidori.signalAny(["a", "b"], { timeoutMs: 150 });
                    return { timedOut: result.timedOut === true, name: result.name };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let created = create_paused_session(&state, "any-to", json!({})).await;
        assert_eq!(created["pending_signal_names"], json!(["a", "b"]));

        let stored = wait_for_status(&state, "any-to", SessionStatus::Completed).await;
        assert_eq!(
            stored.output,
            Some(json!({ "timedOut": true, "name": null }))
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    // -----------------------------------------------------------------------
    // Phase 3: live in-memory delivery to streaming runs (`docs/signals.md`).
    // -----------------------------------------------------------------------

    /// Drive a streaming session through `stream_session` and collect its full
    /// SSE body in a background task (the body future drives the supervisor).
    async fn start_stream(
        state: &AppState,
        session_id: &str,
        input: Value,
    ) -> tokio::task::JoinHandle<String> {
        let response = stream_session(
            State(state.clone()),
            Json(
                serde_json::from_value(json!({
                    "input": input,
                    "session_id": session_id,
                }))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        tokio::spawn(async move {
            let bytes = body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8_lossy(&bytes).to_string()
        })
    }

    /// A streaming run that pauses on `signal()` stays live: the delivery
    /// endpoint reports `delivered_live` and the supervisor resolves the pause
    /// in-process (no `/resume` round-trip), the stream carrying a `paused`
    /// event and then `done`. The recorded signal call matches the durable
    /// persist-resume path byte-for-byte on its match key and result.
    #[tokio::test]
    async fn stream_session_resolves_signal_pause_in_process() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-stream-signal-{}", uuid::Uuid::new_v4()));
        let source = r#"
            export async function agent(input, chidori) {
                const review = await chidori.signal("review");
                return { decision: review.payload.decision, by: review.from.id };
            }
        "#;
        let agent_path = write_agent(&temp_dir, source);
        let state = signal_test_state(&temp_dir, agent_path);

        let sse = start_stream(&state, "live-1", json!({})).await;

        // Wait for the worker to persist the supervised signal pause; the
        // session must STAY in active_sessions (live supervision continues).
        let paused = wait_for_status(&state, "live-1", SessionStatus::Paused).await;
        assert_eq!(paused.pending_signal_name.as_deref(), Some("review"));
        assert!(state.active_sessions.lock().unwrap().contains_key("live-1"));

        // Deliver: routed to the live worker, not the durable resume path.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("live-1".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "approve" }),
                    from: json!({ "kind": "human", "id": "mara" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["status"], json!("delivered_live"));

        // The supervisor resumes in-process and completes the session.
        let stored = wait_for_status(&state, "live-1", SessionStatus::Completed).await;
        assert_eq!(
            stored.output,
            Some(json!({ "decision": "approve", "by": "mara" }))
        );
        assert!(!state.active_sessions.lock().unwrap().contains_key("live-1"));

        // The stream announced the pause and finished with done/completed.
        let sse_text = sse.await.unwrap();
        assert!(sse_text.contains("event: paused"), "sse: {sse_text}");
        assert!(sse_text.contains("event: done"), "sse: {sse_text}");
        assert!(
            sse_text.contains("\"status\":\"completed\""),
            "sse: {sse_text}"
        );

        // Determinism: the same agent driven through the durable
        // create→deliver path records the identical signal call.
        let dir_b =
            std::env::temp_dir().join(format!("chidori-stream-signal-b-{}", uuid::Uuid::new_v4()));
        let agent_b = write_agent(&dir_b, source);
        let state_b = signal_test_state(&dir_b, agent_b);
        create_paused_session(&state_b, "durable-1", json!({})).await;
        response_json(
            signal_session(
                State(state_b.clone()),
                Path("durable-1".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "approve" }),
                    from: json!({ "kind": "human", "id": "mara" }),
                }),
            )
            .await,
        )
        .await;
        let durable = state_b.session_store.get("durable-1").unwrap().unwrap();
        let sig_live = stored
            .call_log
            .iter()
            .find(|r| r.function == "signal")
            .unwrap();
        let sig_durable = durable
            .call_log
            .iter()
            .find(|r| r.function == "signal")
            .unwrap();
        assert_eq!(sig_live.args, sig_durable.args);
        assert_eq!(sig_live.result, sig_durable.result);
        assert_eq!(sig_live.seq, sig_durable.seq);
        assert_eq!(stored.output, durable.output);

        let _ = std::fs::remove_dir_all(temp_dir);
        let _ = std::fs::remove_dir_all(dir_b);
    }

    /// A signal delivered live for a name the run is NOT waiting on lands in
    /// the live in-memory mailbox (write-through persisted) and survives the
    /// in-process resume: the resumed run drains it at a later `pollSignal`.
    #[tokio::test]
    async fn stream_session_live_mailbox_carries_over_in_process_resume() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-stream-mailbox-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const review = await chidori.signal("review");
                    const steer = await chidori.pollSignal("steer");
                    return {
                        decision: review.payload.decision,
                        steer: steer ? steer.payload.dir : null,
                    };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let sse = start_stream(&state, "live-2", json!({})).await;
        wait_for_status(&state, "live-2", SessionStatus::Paused).await;

        // Deliver a NON-matching name first: enqueued live, no resume.
        let (status, body) = response_json(
            signal_session(
                State(state.clone()),
                Path("live-2".to_string()),
                Json(SignalRequest {
                    name: "steer".to_string(),
                    payload: json!({ "dir": "left" }),
                    from: json!({ "id": "sam" }),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["status"], json!("delivered_live"));

        // Now resolve the supervised pause; the resumed run must still see the
        // queued "steer" at its pollSignal.
        response_json(
            signal_session(
                State(state.clone()),
                Path("live-2".to_string()),
                Json(SignalRequest {
                    name: "review".to_string(),
                    payload: json!({ "decision": "approve" }),
                    from: json!({ "id": "mara" }),
                }),
            )
            .await,
        )
        .await;

        let stored = wait_for_status(&state, "live-2", SessionStatus::Completed).await;
        assert_eq!(
            stored.output,
            Some(json!({ "decision": "approve", "steer": "left" }))
        );
        let _ = sse.await.unwrap();

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A streaming run paused with `timeoutMs` resolves to the sentinel from
    /// the supervisor's own deadline (no external delivery, stream stays one
    /// continuous session through to done).
    #[tokio::test]
    async fn stream_session_signal_timeout_resolves_in_process() {
        let temp_dir =
            std::env::temp_dir().join(format!("chidori-stream-timeout-{}", uuid::Uuid::new_v4()));
        let agent_path = write_agent(
            &temp_dir,
            r#"
                export async function agent(input, chidori) {
                    const result = await chidori.signal("review", { timeoutMs: 150 });
                    return { timedOut: result.timedOut === true };
                }
            "#,
        );
        let state = signal_test_state(&temp_dir, agent_path);

        let sse = start_stream(&state, "live-3", json!({})).await;
        let stored = wait_for_status(&state, "live-3", SessionStatus::Completed).await;
        assert_eq!(stored.output, Some(json!({ "timedOut": true })));
        let sse_text = sse.await.unwrap();
        assert!(sse_text.contains("event: paused"), "sse: {sse_text}");
        assert!(
            sse_text.contains("\"status\":\"completed\""),
            "sse: {sse_text}"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
