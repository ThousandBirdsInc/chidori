//! Agent recipes: YAML-defined wrappers that bundle an agent file path with
//! default inputs, a cron schedule, and optional metadata. Recipes are the
//! unit the scheduler operates on.
//!
//! Example `recipes/daily-digest.yaml`:
//!
//! ```yaml
//! name: daily-digest
//! agent: agents/digest.star
//! schedule: "0 9 * * *"        # every day at 09:00
//! inputs:
//!   channel: "#general"
//!   lookback_hours: 24
//! description: "Summarize yesterday's activity and post to Slack"
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// serde_yaml is used via its full path (`serde_yaml::from_str`) so no top-level
// import is needed.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub name: String,
    pub agent: PathBuf,
    /// Cron expression. Standard 5-field form.
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub inputs: Value,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tools: Vec<PathBuf>,
}

impl Recipe {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading recipe {}", path.display()))?;
        // JSON and YAML both deserialize via serde; pick the parser from the
        // file extension. YAML is the documented format but JSON stays
        // supported for tooling that prefers it.
        let recipe: Recipe = if path.extension().and_then(|e| e.to_str()) == Some("json") {
            serde_json::from_str(&text)
                .with_context(|| format!("parsing recipe JSON {}", path.display()))?
        } else {
            serde_yaml::from_str(&text)
                .with_context(|| format!("parsing recipe YAML {}", path.display()))?
        };
        Ok(recipe)
    }

    pub fn load_dir(dir: &Path) -> Result<Vec<Recipe>> {
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "yaml" | "yml" | "json") {
                match Recipe::load(&path) {
                    Ok(r) => out.push(r),
                    Err(e) => tracing::warn!("recipe {}: {}", path.display(), e),
                }
            }
        }
        Ok(out)
    }
}

