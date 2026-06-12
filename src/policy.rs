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
//!   3. CHIDORI_POLICY_PROFILE — name of a built-in profile (e.g. "untrusted")
//!   4. default (AlwaysAllow for everything except shell, which keeps the
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

/// A single rule. `target` is "tool:<name>" / "http" / "workspace:<action>"
/// (where `<action>` is `list` / `read` / `write` / `delete` / `manifest`) /
/// "*". `match_args` is an optional JSON subset that must be contained in the
/// call args for the rule to apply.
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
        if let Ok(profile) = std::env::var("CHIDORI_POLICY_PROFILE") {
            let profile = profile.trim();
            if !profile.is_empty() {
                match builtin_profile(profile) {
                    Some(cfg) => return Arc::new(cfg),
                    None => tracing::warn!(
                        "CHIDORI_POLICY_PROFILE: unknown profile `{}` (known: {})",
                        profile,
                        BUILTIN_PROFILES.join(", ")
                    ),
                }
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

/// Names of the built-in profiles selectable via `CHIDORI_POLICY_PROFILE`.
pub const BUILTIN_PROFILES: &[&str] = &["untrusted"];

/// Build a ready-made [`PolicyConfig`] by name, or `None` for an unknown name.
///
/// The only profile today is `"untrusted"`: deny-by-default with a minimal
/// allowlist of pure, side-effect-free host effects. It is opt-in (via
/// `CHIDORI_POLICY_PROFILE=untrusted`) and never affects the default profile,
/// which keeps the historical `AlwaysAllow` fallback.
pub fn builtin_profile(name: &str) -> Option<PolicyConfig> {
    match name {
        "untrusted" => Some(untrusted_profile()),
        _ => None,
    }
}

/// Deny-by-default profile for running code you do not trust.
///
/// Rationale: `enforce_policy` only gates the *powerful* effects — `http` and
/// the `workspace:*` actions. Pure effects (`log`, `template`, `memory`,
/// `prompt`, ...) never reach the gate, so they always run regardless of the
/// fallback. The fallback therefore governs exactly the powerful surface, and
/// here we set it to `NeverAllow` so every gated effect is denied unless a rule
/// below opts it back in.
///
/// Allowed: `workspace:list`, `workspace:read`, `workspace:manifest` — read-only
/// introspection of the sanitized workspace root, which leaks nothing outside it
/// and mutates nothing.
///
/// Denied (by the fallback): `http` (network egress) and `workspace:write` /
/// `workspace:delete` (disk mutation within the root).
fn untrusted_profile() -> PolicyConfig {
    let allow_read_only = |target: &str| PolicyRule {
        target: target.to_string(),
        decision: Decision::AlwaysAllow,
        match_args: None,
        reason: Some("read-only workspace introspection".to_string()),
    };
    PolicyConfig {
        rules: vec![
            allow_read_only("workspace:list"),
            allow_read_only("workspace:read"),
            allow_read_only("workspace:manifest"),
        ],
        default: Decision::NeverAllow,
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
    format!(
        "{}::{}",
        target,
        serde_json::to_string(args).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_profile_is_none() {
        assert!(builtin_profile("does-not-exist").is_none());
    }

    #[test]
    fn untrusted_profile_denies_by_default() {
        let cfg = builtin_profile("untrusted").expect("untrusted profile exists");
        assert_eq!(cfg.default, Decision::NeverAllow);
    }

    #[test]
    fn untrusted_profile_denies_powerful_effects() {
        let cfg = builtin_profile("untrusted").unwrap();

        // http: no rule matches → falls through to the deny-by-default.
        let (decision, _) = cfg.decide("http", &json!({ "url": "https://example.com" }));
        assert_eq!(decision, Decision::NeverAllow);

        // workspace mutations are denied by the fallback.
        let (decision, _) = cfg.decide("workspace:write", &json!({ "path": "a.txt" }));
        assert_eq!(decision, Decision::NeverAllow);
        let (decision, _) = cfg.decide("workspace:delete", &json!({ "path": "a.txt" }));
        assert_eq!(decision, Decision::NeverAllow);
    }

    #[test]
    fn untrusted_profile_allows_read_only_workspace() {
        let cfg = builtin_profile("untrusted").unwrap();
        for target in ["workspace:list", "workspace:read", "workspace:manifest"] {
            let (decision, _) = cfg.decide(target, &json!({}));
            assert_eq!(
                decision,
                Decision::AlwaysAllow,
                "{target} should be allowed"
            );
        }
    }

    #[test]
    fn default_profile_allows_everything() {
        // The historical default must stay unchanged: no rules, AlwaysAllow fallback.
        let cfg = PolicyConfig::default();
        assert_eq!(cfg.default, Decision::AlwaysAllow);
        let (decision, _) = cfg.decide("http", &json!({ "url": "https://example.com" }));
        assert_eq!(decision, Decision::AlwaysAllow);
        let (decision, _) = cfg.decide("workspace:write", &json!({ "path": "a.txt" }));
        assert_eq!(decision, Decision::AlwaysAllow);
    }
}
