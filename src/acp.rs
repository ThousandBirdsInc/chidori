//! Minimal Agent Client Protocol (ACP) endpoint.
//!
//! ACP is Anthropic's standardized protocol for driving an agent from an
//! external client. The full spec covers streaming, tool-approval flows, and
//! session thread management. This implementation ships a small HTTP-shaped
//! subset so ACP clients can at least:
//!
//!   * create a thread (POST /acp/threads)
//!   * send a user message and read the assistant's response
//!     (POST /acp/threads/{id}/prompts)
//!   * list existing threads (GET /acp/threads)
//!
//! Internally each ACP thread maps to a framework Session (persisted through
//! `SessionStore`) so the same storage, permission, and replay infrastructure
//! applies. Agents drive ACP prompts by reading `inputs.acp_prompt` and
//! returning an `acp_response` string.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::storage::{SessionStatus, SessionStore, StoredSession};

#[derive(Clone)]
pub struct AcpState {
    pub store: Arc<dyn SessionStore>,
    /// Callback to actually run an agent prompt. Injected so the ACP module
    /// doesn't need to depend on the whole server AppState surface.
    pub run_prompt: Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>,
}

pub fn router(state: AcpState) -> Router {
    Router::new()
        .route("/acp/threads", post(create_thread).get(list_threads))
        .route("/acp/threads/{id}", get(get_thread))
        .route("/acp/threads/{id}/prompts", post(send_prompt))
        .with_state(state)
}

#[derive(Deserialize)]
struct CreateThreadRequest {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Serialize)]
struct Thread {
    id: String,
    title: Option<String>,
    status: String,
}

async fn create_thread(
    State(state): State<AcpState>,
    Json(body): Json<CreateThreadRequest>,
) -> Response {
    let id = uuid::Uuid::new_v4().to_string();
    let session = StoredSession {
        id: id.clone(),
        status: SessionStatus::Running,
        input: json!({ "acp_title": body.title }),
        output: None,
        call_log: Vec::new(),
        error: None,
        pending_seq: None,
        pending_prompt: None,
        pending_approval: None,
        approvals: Vec::new(),
        created_at: chrono::Utc::now(),
    };
    if let Err(e) = state.store.put(&session) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        Json(Thread {
            id,
            title: body.title,
            status: "running".into(),
        }),
    )
        .into_response()
}

async fn list_threads(State(state): State<AcpState>) -> Response {
    let sessions = state.store.list().unwrap_or_default();
    let threads: Vec<Value> = sessions
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "status": s.status,
                "title": s.input.get("acp_title"),
            })
        })
        .collect();
    Json(json!({ "threads": threads })).into_response()
}

async fn get_thread(State(state): State<AcpState>, Path(id): Path<String>) -> Response {
    match state.store.get(&id) {
        Ok(Some(s)) => Json(json!({
            "id": s.id,
            "status": s.status,
            "title": s.input.get("acp_title"),
            "output": s.output,
        }))
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "thread not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct PromptRequest {
    prompt: String,
}

async fn send_prompt(
    State(state): State<AcpState>,
    Path(id): Path<String>,
    Json(body): Json<PromptRequest>,
) -> Response {
    // Look up the thread and hand the prompt off to the agent runner. The
    // agent is expected to return `{"response": "..."}` — matching the
    // shape `inputs.acp_prompt` is paired with.
    let Ok(Some(mut session)) = state.store.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "thread not found"})),
        )
            .into_response();
    };

    let inputs = json!({
        "acp_prompt": body.prompt,
        "thread_id": id,
    });

    let runner = state.run_prompt.clone();
    let result = tokio::task::spawn_blocking(move || runner(inputs))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));

    match result {
        Ok(output) => {
            session.output = Some(output.clone());
            session.status = SessionStatus::Completed;
            let _ = state.store.put(&session);
            Json(json!({
                "id": id,
                "response": output.get("response").cloned().unwrap_or(output),
            }))
            .into_response()
        }
        Err(e) => {
            session.status = SessionStatus::Failed;
            session.error = Some(e.clone());
            let _ = state.store.put(&session);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e})),
            )
                .into_response()
        }
    }
}
