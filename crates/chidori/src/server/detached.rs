//! Detached durable agents (`docs/detached-agents.md`): registry listing,
//! status, mailbox delivery (which wakes a hibernating agent), and
//! cooperative stop.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Detached durable agents (docs/detached-agents.md)
// ---------------------------------------------------------------------------

pub(super) async fn list_detached_agents() -> Response {
    match tokio::task::spawn_blocking(|| {
        let hub = crate::runtime::host_agent::hub();
        hub.installed_parts().and_then(|parts| hub.list(&parts))
    })
    .await
    {
        Ok(Ok(agents)) => Json(json!({ "agents": agents })).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

pub(super) async fn get_detached_agent(Path(name): Path<String>) -> Response {
    match tokio::task::spawn_blocking(move || {
        let hub = crate::runtime::host_agent::hub();
        hub.installed_parts()
            .and_then(|parts| hub.status(&parts, &name))
    })
    .await
    {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(err)) => (StatusCode::NOT_FOUND, Json(json!({"error": err}))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct SendDetachedAgentBody {
    name: String,
    #[serde(default)]
    payload: Value,
}

/// Deliver a named message into a detached agent's durable mailbox. A
/// hibernating agent whose listen set matches is woken (resume-by-replay).
pub(super) async fn send_detached_agent(
    Path(agent): Path<String>,
    Json(body): Json<SendDetachedAgentBody>,
) -> Response {
    let from = json!({ "kind": "external", "id": "http" });
    match tokio::task::spawn_blocking(move || {
        let hub = crate::runtime::host_agent::hub();
        hub.installed_parts()
            .and_then(|parts| hub.send(&parts, &agent, &body.name, body.payload, from))
    })
    .await
    {
        Ok(Ok(receipt)) => Json(receipt).into_response(),
        Ok(Err(err)) => (StatusCode::NOT_FOUND, Json(json!({"error": err}))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

pub(super) async fn stop_detached_agent(Path(name): Path<String>) -> Response {
    match tokio::task::spawn_blocking(move || {
        let hub = crate::runtime::host_agent::hub();
        hub.installed_parts()
            .and_then(|parts| hub.stop(&parts, &name))
    })
    .await
    {
        Ok(Ok(outcome)) => Json(outcome).into_response(),
        Ok(Err(err)) => (StatusCode::NOT_FOUND, Json(json!({"error": err}))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}
