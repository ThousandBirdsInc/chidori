//! Cron scheduler for recipes. One background task per recipe; each task
//! sleeps until the next scheduled tick, then runs the agent through the
//! engine pipeline. Results are persisted through the session store so they
//! show up under `GET /sessions` just like interactive runs.
//!
//! Cron expression semantics are delegated to the `cron` crate. A missing
//! schedule on a recipe simply means "no scheduling", and the scheduler
//! skips it.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use chrono::Utc;
use cron::Schedule;

use crate::mcp::McpManager;
use crate::policy::PolicyConfig;
use crate::providers::ProviderRegistry;
use crate::recipes::Recipe;
use crate::runtime::engine::Engine;
use crate::runtime::template::TemplateEngine;
use crate::storage::{SessionStatus, SessionStore, StoredSession};
use crate::tools::ToolRegistry;

/// Stack size for every thread that may run the JS interpreter — tokio
/// workers/blocking threads (agent runs go through `spawn_blocking`) and the
/// branch worker threads. The interpreter recurses natively with the agent's
/// JS call depth (`max_call_depth` = 2000 frames), which needs more headroom
/// than tokio's 2 MiB default; on 64-bit the extra virtual space is only
/// committed if actually touched. Sized by measurement: a debug build needs
/// more than 32 MiB for the depth guard to fire its catchable RangeError
/// before the native stack runs out (release frames are far smaller, but the
/// reservation is virtual either way, so one generous constant serves both).
pub const JS_THREAD_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Build the process's tokio runtime. Exactly `tokio::runtime::Runtime::new()`
/// plus [`JS_THREAD_STACK_BYTES`]-sized threads: agent JS executes on this
/// runtime's blocking threads, and a 2 MiB-stack thread aborts the whole
/// process on ~350 frames of JS recursion instead of letting the engine's
/// depth guard throw a catchable RangeError at 2000.
pub fn new_tokio_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(JS_THREAD_STACK_BYTES)
        .build()
}

/// The process-wide host-effect runtime, built on first use and never
/// dropped. The server and scheduler used to build (and tear down) a fresh
/// multi-thread runtime PER AGENT RUN — spawning a full worker pool with
/// [`JS_THREAD_STACK_BYTES`] stacks each time (~430 µs plus thread churn;
/// `benches/runtime.rs` per_run_setup). Engines only `block_on` this runtime
/// from blocking threads and never spawn run-scoped background tasks on it,
/// so one shared runtime serves every run. It lives in a `static` so it is
/// never dropped (dropping a runtime from async context panics); the CLI's
/// one-runtime-per-invocation paths in `main.rs` are unaffected.
pub fn shared_tokio_runtime() -> std::io::Result<Arc<tokio::runtime::Runtime>> {
    static RT: OnceLock<Arc<tokio::runtime::Runtime>> = OnceLock::new();
    if let Some(rt) = RT.get() {
        return Ok(rt.clone());
    }
    let rt = Arc::new(new_tokio_runtime()?);
    Ok(RT.get_or_init(|| rt).clone())
}

#[derive(Clone)]
pub struct SchedulerDeps {
    /// Shared provider registry — the same one the server's session handlers
    /// use, so scheduled runs see identical providers (and reuse its HTTP
    /// clients) instead of rebuilding a registry from env on every tick.
    pub providers: Arc<ProviderRegistry>,
    pub template_engine: Arc<TemplateEngine>,
    pub session_store: Arc<dyn SessionStore>,
    pub policy: Arc<PolicyConfig>,
    pub mcp: Arc<McpManager>,
    /// Extra tool defs loaded from MCP servers — injected into each scheduled
    /// run's ToolRegistry so cron-launched agents can use the same MCP tools
    /// as interactive sessions.
    pub mcp_tools: Vec<crate::tools::ToolDef>,
}

/// Spawn a background task for every recipe that has a schedule set.
pub fn spawn_all(recipes: Vec<Recipe>, deps: SchedulerDeps) {
    for recipe in recipes {
        let Some(cron_expr) = recipe.schedule.clone() else {
            continue;
        };
        let schedule = match Schedule::from_str(&cron_expr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "recipe `{}`: invalid cron `{}`: {}",
                    recipe.name,
                    cron_expr,
                    e
                );
                continue;
            }
        };
        let deps = deps.clone();
        let recipe_for_task = recipe.clone();
        tokio::spawn(async move {
            run_loop(recipe_for_task, schedule, deps).await;
        });
        tracing::info!("scheduled recipe `{}` with `{}`", recipe.name, cron_expr);
    }
}

async fn run_loop(recipe: Recipe, schedule: Schedule, deps: SchedulerDeps) {
    loop {
        let Some(next) = schedule.upcoming(Utc).next() else {
            tracing::warn!("recipe `{}`: schedule has no future ticks", recipe.name);
            return;
        };
        let wait = (next - Utc::now()).num_milliseconds().max(0) as u64;
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;

        if let Err(e) = run_once(&recipe, &deps).await {
            tracing::warn!("recipe `{}` run failed: {}", recipe.name, e);
        }
    }
}

/// One-shot invocation of a recipe. Exposed for an explicit "trigger now"
/// endpoint; the scheduler loop also funnels through here.
pub async fn run_once(recipe: &Recipe, deps: &SchedulerDeps) -> Result<String> {
    let recipe = recipe.clone();
    let agent_path = recipe.agent.clone();
    let inputs = recipe.inputs.clone();
    let deps = deps.clone();
    let recipe_name = recipe.name.clone();

    let (id, session) = tokio::task::spawn_blocking(move || -> Result<(String, StoredSession)> {
        let rt = crate::scheduler::shared_tokio_runtime()?;
        let providers = deps.providers.clone();

        // Build the tool registry: recipe-local dirs + default `<agent>/tools`
        // + any MCP tools the server handed us.
        let mut dirs: Vec<PathBuf> = recipe.tools.to_vec();
        dirs.push(
            agent_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("tools"),
        );
        let mut registry = ToolRegistry::load_from_dirs_cached(&dirs)
            .map(|r| (*r).clone())
            .unwrap_or_else(|_| ToolRegistry::new());
        for def in &deps.mcp_tools {
            registry.register(def.clone());
        }

        let engine = Engine::new(providers, deps.template_engine.clone(), rt)
            .with_tools(Arc::new(registry))
            .with_policy(deps.policy.clone())
            .with_mcp(deps.mcp.clone());

        let result = engine
            .run(&agent_path, &inputs)
            .with_context(|| format!("recipe `{}` execution", recipe_name))?;

        let id = uuid::Uuid::new_v4().to_string();
        let session = StoredSession {
            id: id.clone(),
            run_id: Some(result.run_id),
            status: SessionStatus::Completed,
            input: inputs,
            output: Some(result.output),
            call_log: result.call_log.into_records(),
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
            created_at: Utc::now(),
        };
        Ok((id, session))
    })
    .await??;

    deps.session_store.put(&session)?;
    Ok(id)
}

#[cfg(test)]
mod stack_tests {
    use super::JS_THREAD_STACK_BYTES;

    /// A JS thread sized to [`JS_THREAD_STACK_BYTES`] must let the engine's
    /// default call-depth guard fire its catchable `RangeError` on deep
    /// recursion, rather than the native stack overflowing and aborting the
    /// process (which is what a too-small stack did — see the constant's
    /// rationale). Regression for `chidori run <deeply-recursive-agent>`.
    #[test]
    fn default_depth_recursion_throws_not_aborts() {
        let outcome = std::thread::Builder::new()
            .stack_size(JS_THREAD_STACK_BYTES)
            .spawn(|| {
                let mut engine = chidori_js::Engine::new();
                // Default max_call_depth (2000); unbounded recursion must hit
                // the guard, not the stack.
                engine
                    .eval("function f(n){ return f(n + 1); } f(0)")
                    .err()
                    .unwrap_or_default()
            })
            .expect("spawn JS thread")
            .join()
            .expect("thread must return an error, not abort");
        assert!(
            outcome.contains("Maximum call stack size exceeded"),
            "expected a catchable RangeError, got: {outcome}"
        );
    }
}
