//! Health endpoint and hardening layers: bearer-token auth, CORS, and the
//! concurrency limit that caps simultaneous agent runs.

use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use tower_http::cors::{AllowOrigin, Any as CorsAny, CorsLayer};

use super::AppState;

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
/// `CHIDORI_API_KEY` accepts a comma-separated list of keys so a key can be
/// rotated without a hard cutover: set `new-key,old-key`, roll every client
/// to the new key, then drop the old one from the list.
///
/// When the env var is unset the middleware is a no-op, so the default
/// local-dev experience is unchanged.
pub(super) async fn auth_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
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
        .map(|v| bearer_token_matches(v, &expected))
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

/// Compare the `Authorization` header value against every configured API key
/// in constant time. A plain `==` short-circuits on the first differing byte,
/// which turns the comparison into a timing oracle an attacker can use to
/// recover the key byte-by-byte; `subtle::ConstantTimeEq` examines every byte
/// regardless of where the mismatch is. (Equal-length inputs are required for
/// `ct_eq`; a length mismatch only reveals the key's length, not its bytes.)
///
/// `raw_keys` is the comma-separated `CHIDORI_API_KEY` value; every key is
/// checked — never early-returned — so accepted and rejected requests do the
/// same amount of comparison work.
pub(super) fn bearer_token_matches(header_value: &str, raw_keys: &str) -> bool {
    use subtle::ConstantTimeEq as _;

    let mut ok = false;
    for key in raw_keys.split(',') {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let expected = format!("Bearer {key}");
        ok |= bool::from(expected.as_bytes().ct_eq(header_value.as_bytes()));
    }
    ok
}

/// A bind host counts as loopback when it is the literal name `localhost` or
/// parses to a loopback IP (`127.0.0.0/8`, `::1`). Anything else — including
/// unresolvable hostnames — is treated as network-reachable, so the
/// no-auth-on-a-reachable-bind refusal fails closed.
pub(super) fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Explicit operator opt-out of the "non-loopback bind requires
/// CHIDORI_API_KEY" refusal, for deployments where something in front of the
/// server (reverse proxy auth, network policy, firewall) controls access.
pub(super) fn allow_unauthenticated_from_env() -> bool {
    matches!(
        std::env::var("CHIDORI_ALLOW_UNAUTHENTICATED").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

/// Build a CORS layer from `CHIDORI_CORS_ORIGINS`:
///
///  * unset     → no CORS headers emitted (same-origin only)
///  * `*`       → `Access-Control-Allow-Origin: *`, `Any` methods + headers
///  * `a,b,c`   → explicit allow-list of origins
pub(super) fn build_cors_layer() -> CorsLayer {
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
pub(super) async fn acquire_run_slot(
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

pub(super) async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}
