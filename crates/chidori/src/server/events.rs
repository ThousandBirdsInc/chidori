//! Event-driven handler: the fallback for non-session routes. Any request is
//! folded into a JSON `event` value and handed to the agent.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Json};
use serde_json::{json, Value};

use super::engine::build_engine;
use super::sessions::agent_error_string;
use super::AppState;

// ---------------------------------------------------------------------------
// Event-driven handler (fallback for non-session routes)
// ---------------------------------------------------------------------------

pub(super) async fn handle_event(
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
            let error = json!({"error": agent_error_string(&state.agent_path, &e)});
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
        }
    }
}
