//! Event-driven handler: the fallback for non-session routes. Any request is
//! folded into a JSON `event` value and handed to the agent.
//!
//! The agent receives `{event: {method, path, headers, query, body}}` (see
//! docs/running-modes.md). A run that completes responds synchronously — an
//! output of `{status, body, headers?}` is honored as the HTTP response,
//! anything else returns as `200` JSON. A run that PAUSES (a
//! `chidori.signal(...)` listen point, an `input()` call, or a policy
//! approval gate) is persisted as a real session and answered with
//! `202 Accepted` + the session view, so the caller holds the id it needs to
//! deliver signals / resume / approve — an event run must never strand a
//! durable pause behind an anonymous `null`.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::storage::{SessionStatus, StoredSession};

use super::engine::build_engine;
use super::hardening::acquire_run_slot;
use super::sessions::{agent_error_string, apply_run_outcome, arm_signal_timeout};
use super::{session_view, store_or_500, AppState};

// ---------------------------------------------------------------------------
// Event-driven handler (fallback for non-session routes)
// ---------------------------------------------------------------------------

/// Paths that are overwhelmingly automatic browser/scanner noise, not
/// deliberate events: browsers request these on their own (favicon, touch
/// icons), and crawlers/probes sweep them constantly. Kept deliberately
/// conservative — only paths a browser or well-behaved bot fetches without
/// the user asking — because a matching request is answered 404 without
/// running the agent (each event run executes the full agent and burns
/// tokens). `CHIDORI_SERVE_ALL_PATHS=1` disables the short-circuit for
/// agents that genuinely serve these paths.
pub(super) fn is_probe_noise_path(path: &str) -> bool {
    path == "/favicon.ico"
        || path == "/robots.txt"
        || path.starts_with("/apple-touch-icon")
        || path.starts_with("/.well-known/")
}

/// Escape hatch for the noise short-circuit: `CHIDORI_SERVE_ALL_PATHS=1`
/// (or `true`/`on`) routes every path to the agent, including the ones
/// [`is_probe_noise_path`] would answer 404.
fn serve_all_paths_from_env() -> bool {
    matches!(
        std::env::var("CHIDORI_SERVE_ALL_PATHS").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

/// Whether a request path should be answered 404 without invoking the agent.
/// Split from the env read so both branches are unit-testable without
/// mutating process-global environment state.
pub(super) fn noise_short_circuit(path: &str, serve_all_paths: bool) -> bool {
    !serve_all_paths && is_probe_noise_path(path)
}

pub(super) async fn handle_event(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    // Short-circuit obvious browser/scanner noise (favicon, robots.txt,
    // touch icons, /.well-known/*) with an empty 404 before the agent runs:
    // every event run executes the whole agent end-to-end, so stray probes
    // would otherwise burn tokens. See docs/running-modes.md §3;
    // CHIDORI_SERVE_ALL_PATHS=1 restores agent(event) for these paths.
    if noise_short_circuit(uri.path(), serve_all_paths_from_env()) {
        return StatusCode::NOT_FOUND.into_response();
    }

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
    if !state.has_default_agent {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(
                json!({"error": "this server was started without an agent file \
                (fleet-only mode), so there is no agent(event) handler; use the \
                /agents/detached/* endpoints or restart the server with an agent path"}),
            ),
        )
            .into_response();
    }

    // Event runs execute the full agent — gate them on the same concurrency
    // semaphore as sessions so a burst of stray requests cannot pile up
    // unbounded blocking runs.
    let permit = match acquire_run_slot(&state).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let app_state = state.clone();
    let input_for_run = input.clone();

    let result = tokio::task::spawn_blocking(move || {
        let engine = build_engine(&app_state, None);
        engine.run_pausable(&app_state.agent_path, &input_for_run)
    })
    .await
    .unwrap();
    drop(permit);

    let run_result = match result {
        Ok(run_result) => run_result,
        Err(e) => {
            eprintln!("Agent error: {e:#}");
            let error = json!({"error": agent_error_string(&state.agent_path, &e)});
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response();
        }
    };

    let mut session = StoredSession {
        id: uuid::Uuid::new_v4().to_string(),
        run_id: None,
        status: SessionStatus::Failed,
        input,
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
        policy_profile: None,
        created_at: chrono::Utc::now(),
    };
    apply_run_outcome(&mut session, run_result);

    // A paused event run is a REAL durable pause (the runtime already
    // persisted the pending op under `.chidori/runs/<run_id>`). Register it
    // as a session and answer 202 with the session view, so the webhook
    // caller (or an operator reading its logs) can deliver the signal,
    // resume the input, or approve the gated call — the run journaled work
    // and must stay reachable.
    if matches!(
        session.status,
        SessionStatus::Paused | SessionStatus::AwaitingApproval
    ) {
        if let Some(err) = store_or_500(&state, &session) {
            return err;
        }
        arm_signal_timeout(&state, &session);
        return (StatusCode::ACCEPTED, Json(session_view(&session))).into_response();
    }

    // Completed: stay stateless (no session row for every stray probe) and
    // honor the documented response mapping: an output object of
    // `{status, body, headers?}` shapes the HTTP response; anything else is
    // returned as 200 JSON.
    let output = session.output.take().unwrap_or(Value::Null);
    if let Value::Object(ref map) = output {
        let status = map.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
        let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

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
    (StatusCode::OK, Json(output)).into_response()
}
