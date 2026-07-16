//! Shared engine builder: every server handler that spawns an agent goes
//! through [`build_engine`] so the config surface stays in one place.

use std::sync::Arc;

use serde_json::Value;

use crate::runtime::engine::Engine;
use crate::tools::ToolRegistry;

use super::sessions::session_policy;
use super::AppState;

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
pub(super) fn build_engine(app: &AppState, policy_profile: Option<&str>) -> Engine {
    let rt = crate::scheduler::shared_tokio_runtime().unwrap();
    // Reuse the app's provider registry so a replay-based resume sees the same
    // providers as the live-VM resume path (which drives `state.providers`
    // directly). In production this is the env-derived registry passed to
    // `serve`; re-deriving it here would drop any test-injected providers and
    // break resume parity between the two paths.
    let providers = app.providers.clone();
    // Tools come from the implicit `<agent dir>/tools/` convention plus any
    // `--tools` dirs passed to `chidori serve` — the same discovery rule as
    // `chidori run`.
    let mut tool_dirs = vec![app
        .agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("tools")];
    tool_dirs.extend(app.extra_tool_dirs.iter().cloned());
    let mut registry = ToolRegistry::load_from_dirs_cached(&tool_dirs)
        .map(|r| (*r).clone())
        .unwrap_or_else(|_| ToolRegistry::new());
    for def in app.mcp_tools.iter() {
        registry.register(def.clone());
    }
    // Default `chidori.workspace` to the served agent's project directory,
    // matching `chidori run` — an explicit CHIDORI_WORKSPACE_ROOT still wins
    // (it populates the context default, which the engine never overrides).
    let workspace_root = app
        .agent_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let workspace_root = std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
    Engine::new(providers, app.template_engine.clone(), rt)
        .with_tools(Arc::new(registry))
        .with_policy(session_policy(app, policy_profile))
        .with_mcp(app.mcp.clone())
        .with_persist_base(app.run_base.clone())
        .with_workspace_root(workspace_root)
}

/// Synchronous one-shot runner used by the ACP endpoint. Runs the agent on
/// the current thread (already inside spawn_blocking) and returns the output
/// JSON. Any error is bubbled as an anyhow::Error.
pub(super) fn run_agent_sync(app: &AppState, inputs: Value) -> anyhow::Result<Value> {
    let engine = build_engine(app, None);
    let result = engine.run(&app.agent_path, &inputs)?;
    Ok(result.output)
}
