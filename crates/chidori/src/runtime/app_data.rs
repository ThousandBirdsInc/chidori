//! Host-only app-data broker — the agent-run write tool for generative UI.
//!
//! A generated agent and the UI it drives can share one live-synced dataset:
//! the agent writes a row to its app cluster and every subscribed client gets
//! the diff. A guest agent running in the JS sandbox cannot speak the
//! Postgres wire protocol and must never hold the cluster's read-write
//! credential, so the write goes through a host function (`chidori.appData.*`)
//! that crosses the host boundary, journaled like any other effect.
//!
//! The credential and endpoint arrive host-side in `CHIDORI_APP_DATA` — never
//! visible to the guest — mirroring `CHIDORI_SECRET_ENV` / `CHIDORI_MCP_CONFIG`:
//!
//! ```json
//! { "endpoint": "http://127.0.0.1:8090/internal/app-data/write",
//!   "token": "__CHIDORI_SECRET__<id>__" }
//! ```
//!
//! `token` is a *placeholder*, not a raw token: the host function sets it as a
//! `Authorization: Bearer` header and the existing secret broker
//! ([`crate::runtime::secret_env`]) substitutes the real per-run token, locked
//! to the endpoint's host. The write itself is performed by agent-builder
//! (Option A in docs/design/chidori-handoff.md §3.2.2): the host issues a
//! loopback HTTP POST that wraps `AppDataPlane::execute_write`, so chidori never
//! holds a libpq credential and reuses the proven write path. See
//! app-agent-builder docs/design/chidori-handoff.md §3.2.

use serde::Deserialize;
use serde_json::{json, Value};

/// Host-only env var carrying the app-data endpoint + token placeholder. Absent
/// when no cluster is bound to the run, so a clusterless agent gets a clear
/// `no_cluster` error rather than a silent no-op.
pub const APP_DATA_ENV: &str = "CHIDORI_APP_DATA";

#[derive(Debug, Clone, Deserialize)]
pub struct AppDataConfig {
    /// The loopback agent-builder endpoint that performs the write.
    pub endpoint: String,
    /// A `__CHIDORI_SECRET__<id>__` placeholder; the secret broker substitutes
    /// the real per-run token host-side, audience-locked to `endpoint`'s host.
    pub token: String,
}

impl AppDataConfig {
    /// Parse the host-only blob. A missing var means "no cluster bound" (the
    /// caller maps it to a structured `no_cluster` error); malformed JSON is
    /// logged because it means a binding the harness intended to provide is
    /// unusable.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(APP_DATA_ENV).ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<Self>(&raw) {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                tracing::error!("invalid {APP_DATA_ENV}, app data unavailable: {err}");
                None
            }
        }
    }
}

/// The structured error value the guest sees for a failed app-data call. Mirrors
/// the `mcpError` shape (mcp-http-transport-chidori.md §3.3): a value, not a
/// thrown exception, so the generation loop / run inspector can surface it and
/// replay stays byte-identical. `kind` is one of `no_cluster | sql | transport`.
pub fn app_data_error(kind: &str, message: impl Into<String>) -> Value {
    json!({ "appDataError": { "kind": kind, "message": message.into() } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_and_token() {
        let cfg: AppDataConfig = serde_json::from_str(
            r#"{"endpoint":"http://127.0.0.1:8090/x","token":"__CHIDORI_SECRET__abc__"}"#,
        )
        .unwrap();
        assert_eq!(cfg.endpoint, "http://127.0.0.1:8090/x");
        assert_eq!(cfg.token, "__CHIDORI_SECRET__abc__");
    }

    #[test]
    fn error_value_has_kind_and_message() {
        let e = app_data_error("no_cluster", "no app data cluster bound to this run");
        assert_eq!(e["appDataError"]["kind"], "no_cluster");
        assert_eq!(
            e["appDataError"]["message"],
            "no app data cluster bound to this run"
        );
    }
}
