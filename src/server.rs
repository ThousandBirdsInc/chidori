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
use crate::runtime::context::RuntimeEvent;
use crate::runtime::engine::Engine;
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

    if let Some(err) = store_or_500(&state, &session) {
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
    let vfs = load_persisted_vfs(&state.run_base, original.run_id.as_deref());
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
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
                pending_approval: None,
                approvals: original.approvals.clone(),
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
                let completed_session = StoredSession {
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
                };
                let _ = app_state.session_store.put(&completed_session);
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
    let vfs = load_persisted_vfs(&state.run_base, original.run_id.as_deref());
    let resume_run_id = original.run_id.clone();
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
        // Continue under the original run id (when known) so the resumed run
        // keeps its persisted run directory and stays a single durable run,
        // matching the live-VM resume path. Falls back to a fresh id only when
        // the session never recorded one.
        match resume_run_id {
            Some(run_id) => engine
                .run_replay_pausable_with_host_promises_and_vfs_preserving_run_id(
                    &app_state.agent_path,
                    &input_clone,
                    call_log,
                    host_promises,
                    vfs,
                    run_id,
                ),
            None => engine.run_replay_pausable_with_host_promises_and_vfs(
                &app_state.agent_path,
                &input_clone,
                call_log,
                host_promises,
                vfs,
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
    let app_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
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
                .run_replay_pausable_with_host_promises_and_vfs_preserving_run_id(
                    &app_state.agent_path,
                    &input,
                    call_log,
                    Vec::new(),
                    vfs,
                    run_id,
                ),
            None => engine.run_replay_pausable_with_host_promises_and_vfs(
                &app_state.agent_path,
                &input,
                call_log,
                Vec::new(),
                vfs,
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
}
