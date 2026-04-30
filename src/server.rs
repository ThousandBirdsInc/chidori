use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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
use crate::runtime::engine::Engine;
use crate::runtime::template::TemplateEngine;
use crate::scheduler::{self, SchedulerDeps};
use crate::storage::{
    build_session_store, SessionStatus, SessionStore, StoredSession,
};
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
    session_store: Arc<dyn SessionStore>,
    policy: Arc<PolicyConfig>,
    mcp: Arc<McpManager>,
    mcp_tools: Arc<Vec<ToolDef>>,
    recipes: Arc<Vec<Recipe>>,
    /// Caps the number of agent runs executing concurrently.
    run_semaphore: Arc<Semaphore>,
    acquire_timeout: std::time::Duration,
}

/// Render a StoredSession into the JSON shape historical clients expect.
fn session_view(s: &StoredSession) -> Value {
    json!({
        "id": s.id,
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
    let mcp_tools = mcp
        .start_from_config(&mcp_cfg)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("MCP startup: {}", e);
            Vec::new()
        });
    let mcp_tools = Arc::new(mcp_tools);

    let session_store = build_session_store();

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
        session_store,
        policy,
        mcp,
        mcp_tools,
        recipes: recipes_arc,
        run_semaphore: Arc::new(Semaphore::new(max_concurrent)),
        acquire_timeout: std::time::Duration::from_millis(acquire_timeout_ms),
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
        .route("/sessions/{id}/replay", post(replay_session))
        .route("/sessions/{id}/resume", post(resume_session))
        .route("/sessions/{id}/approve", post(approve_session))
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
    eprintln!("  Concurrency: max {} sessions, {}ms acquire timeout", max_concurrent, acquire_timeout_ms);
    eprintln!("  Auth:        {}", if auth_required {
        "REQUIRED (Authorization: Bearer $CHIDORI_API_KEY)"
    } else {
        "disabled (set CHIDORI_API_KEY to enable)"
    });
    eprintln!("  CORS:        {}", match std::env::var("CHIDORI_CORS_ORIGINS").ok() {
        Some(v) if v.trim() == "*" => "open (Any)".to_string(),
        Some(v) => format!("allow: {}", v),
        None => "disabled (set CHIDORI_CORS_ORIGINS to enable)".to_string(),
    });
    eprintln!();
    eprintln!("  Events:     ANY /*           → agent(event)");
    eprintln!("  Sessions:   POST /sessions   → create & run");
    eprintln!("              GET  /sessions   → list all");
    eprintln!("              GET  /sessions/{{id}}  → get result");
    eprintln!("              GET  /sessions/{{id}}/checkpoint → call log");
    eprintln!("              POST /sessions/{{id}}/replay     → replay from checkpoint");
    eprintln!("              POST /sessions/{{id}}/resume     → resume paused input() call");
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

async fn run_recipe(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
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
    /// Optional: provide a checkpoint (call log) to replay from.
    #[serde(default)]
    replay_from: Option<Vec<CallRecord>>,
    /// Optional: override the server's default agent for this session.
    /// Must be a bare filename (e.g. "hello.star") resolved against
    /// the parent directory of the server's configured agent_path.
    /// Path traversal is rejected. When unset, the server's default
    /// agent is used.
    #[serde(default)]
    agent: Option<String>,
}

/// Resolve an optional per-session agent override against the server's
/// configured `agent_path`. Accepts only a bare .star filename in the
/// peer directory — no subdirectories, no `..`, no absolute paths.
/// Returns a `(StatusCode, message)` error suitable for short-circuit
/// rejection when the client passes something invalid.
fn resolve_agent_override(
    default_path: &std::path::Path,
    requested: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    // Reject anything that's not a plain filename. The allow-list keeps
    // the validation simple and audit-friendly: letters, digits, dashes,
    // dots, underscores only.
    let ok = !requested.is_empty()
        && requested.len() < 128
        && requested.ends_with(".star")
        && requested
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid agent name '{}': must be a bare `.star` filename",
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

/// GET /agents — list the `.star` files in the peer directory of the
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
            if path.extension().and_then(|s| s.to_str()) != Some("star") {
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

    let id = uuid::Uuid::new_v4().to_string();
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
                    (SessionStatus::Completed, None, None, None, Some(run_result.output))
                };
            StoredSession {
                id: id.clone(),
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
            "status": s.status,
            "call_log": s.call_log,
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
    let input_clone = input.clone();
    let approvals = original.approvals.clone();
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
        engine.run_with_replay(&app_state.agent_path, &input_clone, call_log)
    })
    .await
    .unwrap();

    match result {
        Ok(run_result) => {
            let new_id = uuid::Uuid::new_v4().to_string();
            let session = StoredSession {
                id: new_id.clone(),
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
async fn stream_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    use futures::stream::StreamExt;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    // Gate on the concurrency semaphore. If we can't get a permit within
    // the acquire deadline, 503 before any streaming response headers are
    // committed so clients see the overflow cleanly.
    let permit = match acquire_run_slot(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    let input = body.input.clone();
    let app_state = state.clone();

    let (event_tx, event_rx) =
        mpsc::unbounded_channel::<crate::runtime::context::RuntimeEvent>();
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    let agent_path = app_state.agent_path.clone();
    let session_id_for_task = session_id.clone();

    // Move the permit into the blocking task so it's held for the entire
    // agent run. Dropping it at the end of the closure releases the slot.
    tokio::task::spawn_blocking(move || {
        let _run_permit = permit;
        let engine = build_engine(&app_state);

        let result = engine.run_streaming(&agent_path, &input, event_tx);
        let final_event = match result {
            Ok(run_result) => json!({
                "id": session_id_for_task,
                "status": "completed",
                "output": run_result.output,
            }),
            Err(e) => json!({
                "id": session_id_for_task,
                "status": "failed",
                "error": e.to_string(),
            }),
        };
        let _ = result_tx.send(final_event);
    });

    let call_stream = UnboundedReceiverStream::new(event_rx).map(|evt| {
        use crate::runtime::context::RuntimeEvent;
        let (name, data) = match &evt {
            RuntimeEvent::Call(record) => (
                "call",
                serde_json::to_string(record).unwrap_or_else(|_| "{}".into()),
            ),
            RuntimeEvent::TokenDelta { seq, delta } => (
                "token",
                serde_json::to_string(&json!({ "seq": seq, "delta": delta }))
                    .unwrap_or_else(|_| "{}".into()),
            ),
        };
        Ok(Event::default().event(name).data(data))
    });

    let done_stream = async_stream::stream! {
        if let Some(final_event) = result_rx.recv().await {
            let data = serde_json::to_string(&final_event).unwrap_or_else(|_| "{}".into());
            yield Ok::<_, std::convert::Infallible>(Event::default().event("done").data(data));
        }
    };

    Sse::new(call_stream.chain(done_stream))
        .keep_alive(KeepAlive::default())
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

    // Inject a synthetic `input` record at the pending seq so the replaying
    // engine returns the user's response to the agent's input() call.
    let mut call_log = original.call_log.clone();
    call_log.push(CallRecord {
        seq,
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
    let approvals = original.approvals.clone();
    let app_state = state.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state).with_approvals(approvals);
        engine.run_replay_pausable(&app_state.agent_path, &input_clone, call_log)
    })
    .await
    .unwrap();

    let mut session = original;
    match result {
        Ok(run_result) => {
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
            (
                StatusCode::OK,
                Json(session_view(&session)),
            )
                .into_response()
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

    if body.decision != "allow" {
        original.status = SessionStatus::Failed;
        original.error = Some(format!(
            "policy: `{}` denied by operator",
            pending.target
        ));
        original.pending_approval = None;
        let _ = state.session_store.put(&original);
        return (StatusCode::OK, Json(session_view(&original))).into_response();
    }

    // Allow path: record the approval, then re-run the agent fresh (no
    // replay log) so the policy cache is seeded and the blocked call runs.
    // We deliberately re-run from scratch rather than replay: the paused
    // run didn't record the blocked call, and replay would expect the same
    // seq to now contain a tool record, which it doesn't.
    original.approvals.push((pending.target.clone(), pending.args.clone()));
    original.pending_approval = None;

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
                let status = map
                    .get("status")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(200) as u16;
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
