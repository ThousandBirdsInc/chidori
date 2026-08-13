//! Webhook routes declared by the application manifest (`chidori.app.yml`):
//! each route delivers its request body into a detached agent's durable
//! mailbox as a named signal, waking the agent if it hibernates on that name.
//! Routes sit behind the same bearer auth as every other server route.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

/// One manifest route's target, captured as the per-route router state.
pub(crate) struct AppRouteTarget {
    pub path: String,
    pub agent: String,
    pub signal: String,
}

/// Deliver a request to the route's agent. Any JSON body becomes the signal
/// payload verbatim; a non-JSON body arrives as a string; an empty body is
/// `null`. 202 on delivery (the agent consumes asynchronously), 404 for an
/// unknown agent name, 503 when the fleet runtime isn't installed.
pub(crate) async fn deliver_app_route(
    State(target): State<Arc<AppRouteTarget>>,
    body: axum::body::Bytes,
) -> Response {
    let payload: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };

    let route = target.clone();
    // The hub does registry I/O and takes process-wide locks — keep it off
    // the async worker threads.
    let delivered = tokio::task::spawn_blocking(move || {
        let hub = crate::runtime::host_agent::hub();
        let parts = hub.installed_parts()?;
        hub.send(
            &parts,
            &route.agent,
            &route.signal,
            payload,
            json!({ "route": route.path }),
        )
    })
    .await;

    match delivered {
        Ok(Ok(receipt)) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "delivered": true,
                "agent": target.agent,
                "signal": target.signal,
                "receipt": receipt,
            })),
        )
            .into_response(),
        Ok(Err(err)) => {
            let status = if err.contains("unknown agent") {
                StatusCode::NOT_FOUND
            } else if err.contains("no runtime parts") {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "delivered": false, "error": err }))).into_response()
        }
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "delivered": false, "error": join_err.to_string() })),
        )
            .into_response(),
    }
}
