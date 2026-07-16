//! Recipes endpoints: list the configured recipes and run one off-schedule.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::scheduler::{self, SchedulerDeps};

use super::AppState;

// ---------------------------------------------------------------------------
// Recipes
// ---------------------------------------------------------------------------

pub(super) async fn list_recipes(State(state): State<AppState>) -> impl IntoResponse {
    let recipes: Vec<Value> = state
        .recipes
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "agent": r.agent,
                "schedule": r.schedule,
                "description": r.description,
            })
        })
        .collect();
    Json(json!({ "recipes": recipes }))
}

pub(super) async fn run_recipe(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let Some(recipe) = state.recipes.iter().find(|r| r.name == name).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "recipe not found"})),
        )
            .into_response();
    };
    let deps = SchedulerDeps {
        providers: state.providers.clone(),
        template_engine: state.template_engine.clone(),
        session_store: state.session_store.clone(),
        policy: state.policy.clone(),
        mcp: state.mcp.clone(),
        mcp_tools: (*state.mcp_tools).clone(),
    };
    match scheduler::run_once(&recipe, &deps).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({"session_id": id}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
