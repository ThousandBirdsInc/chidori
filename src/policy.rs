//! Permission policy for dangerous host functions.
//!
//! Goose-style tool confirmation. Each rule targets a host function + optional
//! argument matcher and resolves to one of three decisions:
//!   * AlwaysAllow — run without confirmation
//!   * AskBefore   — pause the run and surface a prompt (via input())
//!   * NeverAllow  — hard refusal, the call errors out
//!
//! Policy is loaded at engine startup from:
//!   1. CHIDORI_POLICY_FILE — path to a JSON file
//!   2. CHIDORI_POLICY — inline JSON string
//!   3. default (AlwaysAllow for everything except shell, which keeps the
//!      existing CHIDORI_SHELL_ALLOW semantics)

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AlwaysAllow,
    AskBefore,
    NeverAllow,
}

impl Default for Decision {
    fn default() -> Self {
        Decision::AlwaysAllow
    }
}

/// A single rule. `target` is "tool:<name>" / "shell" / "http" / "exec" /
/// "write_file" / "*". `match_args` is an optional JSON subset that must be
/// contained in the call args for the rule to apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    pub target: String,
    #[serde(default)]
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
    /// Fallback when no rule matches.
    #[serde(default)]
    pub default: Decision,
}

impl PolicyConfig {
    pub fn from_env() -> Arc<Self> {
        if let Ok(path) = std::env::var("CHIDORI_POLICY_FILE") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<PolicyConfig>(&text) {
                    Ok(cfg) => return Arc::new(cfg),
                    Err(e) => tracing::warn!("CHIDORI_POLICY_FILE parse error: {}", e),
                }
            }
        }
        if let Ok(inline) = std::env::var("CHIDORI_POLICY") {
            match serde_json::from_str::<PolicyConfig>(&inline) {
                Ok(cfg) => return Arc::new(cfg),
                Err(e) => tracing::warn!("CHIDORI_POLICY parse error: {}", e),
            }
        }
        Arc::new(PolicyConfig::default())
    }

    /// Resolve a call against the policy. `target` is the normalized target
    /// string (see PolicyRule.target). Rules are tried in order; the first
    /// matching rule wins. Wildcard "*" always matches target.
    pub fn decide(&self, target: &str, args: &Value) -> (Decision, Option<String>) {
        for rule in &self.rules {
            if rule.target != target && rule.target != "*" {
                continue;
            }
            if let Some(ref pat) = rule.match_args {
                if !value_contains(args, pat) {
                    continue;
                }
            }
            return (rule.decision, rule.reason.clone());
        }
        (self.default, None)
    }
}

/// True when `args` contains all keys/values in `pattern` (shallow for scalars,
/// recursive for objects). Lists require equality.
fn value_contains(args: &Value, pattern: &Value) -> bool {
    match (args, pattern) {
        (Value::Object(a), Value::Object(p)) => p
            .iter()
            .all(|(k, pv)| a.get(k).map(|av| value_contains(av, pv)).unwrap_or(false)),
        (Value::String(a), Value::String(p)) => a.contains(p.as_str()),
        (a, p) => a == p,
    }
}

/// A remembered user decision for the remainder of this run. After the user
/// approves an AskBefore call once, we cache (target, canonical_args) → Allow
/// so repeated calls in the same agent pass through.
#[derive(Debug, Default)]
pub struct PolicyCache {
    inner: HashMap<String, bool>,
}

impl PolicyCache {
    pub fn is_approved(&self, target: &str, args: &Value) -> bool {
        self.inner
            .get(&cache_key(target, args))
            .copied()
            .unwrap_or(false)
    }
    pub fn approve(&mut self, target: &str, args: &Value) {
        self.inner.insert(cache_key(target, args), true);
    }
}

fn cache_key(target: &str, args: &Value) -> String {
    format!("{}::{}", target, serde_json::to_string(args).unwrap_or_default())
}
