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
use std::sync::Arc;

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

#[derive(Clone)]
pub struct SchedulerDeps {
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
        let rt = Arc::new(tokio::runtime::Runtime::new()?);
        let providers = Arc::new(ProviderRegistry::from_env());

        // Build the tool registry: recipe-local dirs + default `<agent>/tools`
        // + any MCP tools the server handed us.
        let mut dirs: Vec<PathBuf> = recipe
            .tools
            .iter()
            .cloned()
            .collect();
        dirs.push(
            agent_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("tools"),
        );
        let mut registry = ToolRegistry::load_from_dirs(&dirs)
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
            status: SessionStatus::Completed,
            input: inputs,
            output: Some(result.output),
            call_log: result.call_log.into_records(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_approval: None,
            approvals: Vec::new(),
            created_at: Utc::now(),
        };
        Ok((id, session))
    })
    .await??;

    deps.session_store.put(&session)?;
    Ok(id)
}
