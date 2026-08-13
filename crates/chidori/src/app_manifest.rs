//! The application manifest (`chidori.app.yml`): declarative fleet assembly
//! for `chidori serve`.
//!
//! Detached agents, schedules, and webhook routes are all spawnable
//! imperatively (`chidori.agents.spawn`, recipes, the event fallback); the
//! manifest gives that composition a source-controlled definition instead of
//! runtime state, so `chidori serve` boots a whole application:
//!
//! ```yaml
//! name: support-desk
//! agents:
//!   - name: triage
//!     agent: agents/triage.ts        # entry, relative to the manifest
//!     keep_alive: true               # spawn at boot; re-arm forever after
//!     input: { queue: "inbound" }
//!     restart: resume                # never | clean | resume (default)
//!   - name: standup-scribe
//!     agent: agents/scribe.ts
//!     schedule: "0 9 * * 1-5"        # cron → runs as a scheduled session
//! routes:
//!   - path: /webhooks/github
//!     agent: triage                  # deliver into this agent's mailbox…
//!     signal: github-event           # …as this named signal
//! ```
//!
//! Semantics:
//! - `keep_alive: true` — at boot, if the name is not already live in the
//!   detached-agent registry (`docs/detached-agents.md`), spawn it; if it is
//!   live, the normal registry re-arm covers it. A settled (completed /
//!   failed / stopped) incarnation is replaced by a fresh spawn, mailbox
//!   migration included, exactly like a `chidori.agents.spawn` reusing the
//!   name.
//! - `schedule` — the entry becomes a recipe (`docs/cli.md`), scheduled by
//!   the same cron loop and listed under `GET /recipes`.
//! - `routes` — each path is served by the fleet server; a request's JSON
//!   body is delivered to the named agent's durable mailbox as the named
//!   signal (waking it if it hibernates on that name). Routes sit behind the
//!   same bearer auth as every other server route.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::recipes::Recipe;

/// Manifest file names probed (in order) in the server's base directory when
/// no explicit path is given.
pub const MANIFEST_FILE_NAMES: &[&str] = &["chidori.app.yml", "chidori.app.yaml", "chidori.app.json"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agents: Vec<ManifestAgent>,
    #[serde(default)]
    pub routes: Vec<ManifestRoute>,
    /// The directory the manifest was loaded from; every relative `agent`
    /// path resolves against it. Not part of the file format.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestAgent {
    /// Registry name (detached agents) and recipe name (schedules).
    pub name: String,
    /// Agent entry file, relative to the manifest's directory.
    pub agent: PathBuf,
    #[serde(default)]
    pub input: Value,
    /// Spawn as a detached agent at server boot and keep it re-armed.
    #[serde(default)]
    pub keep_alive: bool,
    /// Cron schedule (5-field, or 6-field with seconds); the entry runs as a
    /// scheduled recipe. Composable with `keep_alive: false` only — a
    /// detached agent is a process, not a job.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Restart strategy for `keep_alive` agents: `never` | `clean` | `resume`.
    #[serde(default)]
    pub restart: Option<String>,
    #[serde(default)]
    pub max_restarts: Option<u32>,
    #[serde(default)]
    pub backoff_ms: Option<u64>,
    /// Default model for the agent's prompts.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRoute {
    /// URL path to serve (must start with `/`).
    pub path: String,
    /// Detached-agent name the request is delivered to.
    pub agent: String,
    /// Signal name the request body arrives under.
    pub signal: String,
}

impl AppManifest {
    /// Probe `dir` for a manifest file. `None` when the directory has none —
    /// a manifest is optional; serving without one is the classic behavior.
    pub fn find_in(dir: &Path) -> Option<PathBuf> {
        MANIFEST_FILE_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
    }

    /// Load and validate a manifest. Errors are boot-time errors: a manifest
    /// that names an unreadable agent file or an invalid cron/route should
    /// stop the server before it binds, not fail at first use.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading app manifest {}", path.display()))?;
        let mut manifest: AppManifest =
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                serde_json::from_str(&text)
                    .with_context(|| format!("parsing app manifest JSON {}", path.display()))?
            } else {
                serde_yaml::from_str(&text)
                    .with_context(|| format!("parsing app manifest YAML {}", path.display()))?
            };
        manifest.base_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for agent in &self.agents {
            if agent.name.is_empty()
                || agent
                    .name
                    .chars()
                    .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
            {
                anyhow::bail!(
                    "app manifest: `{}` is not a registrable agent name \
                     (allowed: ASCII letters, digits, `-`, `_`, `.`)",
                    agent.name
                );
            }
            if !seen.insert(agent.name.clone()) {
                anyhow::bail!("app manifest: duplicate agent name `{}`", agent.name);
            }
            let entry = self.resolve_agent_path(agent);
            if !entry.is_file() {
                anyhow::bail!(
                    "app manifest: agent `{}` names a missing entry file {}",
                    agent.name,
                    entry.display()
                );
            }
            if let Some(restart) = &agent.restart {
                if !matches!(restart.as_str(), "never" | "clean" | "resume") {
                    anyhow::bail!(
                        "app manifest: agent `{}` has unknown restart strategy `{restart}` \
                         (expected \"never\", \"clean\", or \"resume\")",
                        agent.name
                    );
                }
            }
            if agent.keep_alive && agent.schedule.is_some() {
                anyhow::bail!(
                    "app manifest: agent `{}` sets both keep_alive and schedule — a \
                     detached agent is a long-lived process, a schedule is a repeated \
                     job; split them into two entries",
                    agent.name
                );
            }
            if let Some(schedule) = &agent.schedule {
                normalize_cron(schedule).with_context(|| {
                    format!(
                        "app manifest: agent `{}` has invalid cron `{schedule}`",
                        agent.name
                    )
                })?;
            }
        }
        for route in &self.routes {
            if !route.path.starts_with('/') {
                anyhow::bail!(
                    "app manifest: route path `{}` must start with `/`",
                    route.path
                );
            }
            if route.agent.is_empty() || route.signal.is_empty() {
                anyhow::bail!(
                    "app manifest: route `{}` needs both `agent` and `signal`",
                    route.path
                );
            }
            // A route usually targets a manifest-managed agent; targeting a
            // name spawned by other means is legal, so an unknown name is a
            // warning at delivery time, not a boot error.
        }
        Ok(())
    }

    /// An agent's entry file, resolved against the manifest's directory.
    pub fn resolve_agent_path(&self, agent: &ManifestAgent) -> PathBuf {
        if agent.agent.is_absolute() {
            agent.agent.clone()
        } else {
            self.base_dir.join(&agent.agent)
        }
    }

    /// The scheduled entries as recipes for the cron scheduler (and
    /// `GET /recipes`). Paths are absolute so the scheduler is independent of
    /// the process working directory.
    pub fn to_recipes(&self) -> Vec<Recipe> {
        self.agents
            .iter()
            .filter_map(|agent| {
                let schedule = agent.schedule.as_ref()?;
                let schedule = normalize_cron(schedule).ok()?;
                Some(Recipe {
                    name: agent.name.clone(),
                    agent: self.resolve_agent_path(agent),
                    schedule: Some(schedule),
                    inputs: agent.input.clone(),
                    description: agent.description.clone(),
                })
            })
            .collect()
    }

    /// The `keep_alive` entries, i.e. the detached-agent fleet this manifest
    /// declares.
    pub fn fleet(&self) -> impl Iterator<Item = &ManifestAgent> {
        self.agents.iter().filter(|a| a.keep_alive)
    }
}

/// Accept both the documented 5-field cron form and the `cron` crate's
/// 6/7-field form, returning an expression the scheduler's parser accepts
/// (5-field input gains a `0` seconds column).
fn normalize_cron(expr: &str) -> Result<String> {
    if cron::Schedule::from_str(expr).is_ok() {
        return Ok(expr.to_string());
    }
    let with_seconds = format!("0 {expr}");
    if cron::Schedule::from_str(&with_seconds).is_ok() {
        return Ok(with_seconds);
    }
    anyhow::bail!("not a valid cron expression: `{expr}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn touch_agent(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "export {}\n").unwrap();
    }

    #[test]
    fn loads_and_validates_a_full_manifest() {
        let dir = tempfile::tempdir().unwrap();
        touch_agent(dir.path(), "agents/triage.ts");
        touch_agent(dir.path(), "agents/scribe.ts");
        let path = write_manifest(
            dir.path(),
            "chidori.app.yml",
            r#"
name: support-desk
agents:
  - name: triage
    agent: agents/triage.ts
    keep_alive: true
    input: { queue: "inbound" }
  - name: scribe
    agent: agents/scribe.ts
    schedule: "0 9 * * 1-5"
routes:
  - path: /webhooks/github
    agent: triage
    signal: github-event
"#,
        );
        let manifest = AppManifest::load(&path).unwrap();
        assert_eq!(manifest.name.as_deref(), Some("support-desk"));
        assert_eq!(manifest.fleet().count(), 1);
        let recipes = manifest.to_recipes();
        assert_eq!(recipes.len(), 1);
        assert_eq!(recipes[0].name, "scribe");
        // 5-field cron normalized to the scheduler's 6-field parser.
        assert_eq!(recipes[0].schedule.as_deref(), Some("0 0 9 * * 1-5"));
        assert!(recipes[0].agent.is_absolute());
        assert_eq!(manifest.routes.len(), 1);
    }

    #[test]
    fn find_in_probes_the_standard_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(AppManifest::find_in(dir.path()), None);
        write_manifest(dir.path(), "chidori.app.yaml", "agents: []\n");
        assert_eq!(
            AppManifest::find_in(dir.path()),
            Some(dir.path().join("chidori.app.yaml"))
        );
    }

    #[test]
    fn missing_agent_file_is_a_boot_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            "chidori.app.yml",
            "agents:\n  - name: ghost\n    agent: missing.ts\n",
        );
        let err = AppManifest::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("missing entry file"));
    }

    #[test]
    fn keep_alive_plus_schedule_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        touch_agent(dir.path(), "a.ts");
        let path = write_manifest(
            dir.path(),
            "chidori.app.yml",
            "agents:\n  - name: both\n    agent: a.ts\n    keep_alive: true\n    schedule: \"0 9 * * *\"\n",
        );
        let err = AppManifest::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("both keep_alive and schedule"));
    }

    #[test]
    fn invalid_names_cron_and_route_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        touch_agent(dir.path(), "a.ts");
        for (body, needle) in [
            (
                "agents:\n  - name: \"bad name\"\n    agent: a.ts\n",
                "not a registrable agent name",
            ),
            (
                "agents:\n  - name: a\n    agent: a.ts\n    schedule: \"whenever\"\n",
                "invalid cron",
            ),
            (
                "agents: []\nroutes:\n  - path: webhooks\n    agent: a\n    signal: s\n",
                "must start with `/`",
            ),
            (
                "agents:\n  - name: a\n    agent: a.ts\n  - name: a\n    agent: a.ts\n",
                "duplicate agent name",
            ),
        ] {
            let path = write_manifest(dir.path(), "chidori.app.yml", body);
            let err = AppManifest::load(&path).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "expected `{needle}` in: {err:#}"
            );
        }
    }
}
