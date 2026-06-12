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

impl Decision {
    /// Total order by restrictiveness: AlwaysAllow < AskBefore < NeverAllow.
    fn strictness(self) -> u8 {
        match self {
            Decision::AlwaysAllow => 0,
            Decision::AskBefore => 1,
            Decision::NeverAllow => 2,
        }
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
    /// Optional reason attached to the fallback decision, surfaced in the
    /// denial/approval message so an operator hitting a deny-by-default
    /// posture is told how to relax it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reason: Option<String>,
    /// An additional policy layered on top of this one: the effective decision
    /// for a call is the *stricter* of the two. Set via [`restricted_by`]
    /// (e.g. a per-session profile on the server) so a caller-selected policy
    /// can only tighten, never relax, the operator's policy. Never serialized:
    /// the overlay is reconstructed from its profile name on every run.
    #[serde(skip)]
    pub overlay: Option<Arc<PolicyConfig>>,
}

impl PolicyConfig {
    pub fn from_env() -> Arc<Self> {
        Self::from_env_configured().unwrap_or_else(|| Arc::new(PolicyConfig::default()))
    }

    /// Env-driven policy resolution that distinguishes "the operator
    /// configured a policy" from "nothing was configured". Returns `Some`
    /// only when a `CHIDORI_POLICY*` source resolved successfully; a
    /// malformed source warns and falls through to the next, exactly as
    /// [`from_env`] always has. Callers that want a deny-by-default posture
    /// when nothing (valid) is configured — `chidori serve` — map `None` to
    /// [`serve_default_profile`] so misconfiguration fails closed instead of
    /// silently running allow-all.
    pub fn from_env_configured() -> Option<Arc<Self>> {
        if let Ok(path) = std::env::var("CHIDORI_POLICY_FILE") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<PolicyConfig>(&text) {
                    Ok(cfg) => return Some(Arc::new(cfg)),
                    Err(e) => tracing::warn!("CHIDORI_POLICY_FILE parse error: {}", e),
                }
            }
        }
        if let Ok(inline) = std::env::var("CHIDORI_POLICY") {
            match serde_json::from_str::<PolicyConfig>(&inline) {
                Ok(cfg) => return Some(Arc::new(cfg)),
                Err(e) => tracing::warn!("CHIDORI_POLICY parse error: {}", e),
            }
        }
        if let Ok(profile) = std::env::var("CHIDORI_POLICY_PROFILE") {
            let profile = profile.trim();
            if !profile.is_empty() {
                match builtin_profile(profile) {
                    Some(cfg) => return Some(Arc::new(cfg)),
                    None => tracing::warn!(
                        "CHIDORI_POLICY_PROFILE: unknown profile `{}` (known: {})",
                        profile,
                        BUILTIN_PROFILES.join(", ")
                    ),
                }
            }
        }
        None
    }

    /// Resolve a call against the policy. `target` is the normalized target
    /// string (see PolicyRule.target). Rules are tried in order; the first
    /// matching rule wins. Wildcard "*" always matches target. When an
    /// overlay is present, the stricter of the two resolutions wins.
    pub fn decide(&self, target: &str, args: &Value) -> (Decision, Option<String>) {
        let base = self.decide_local(target, args);
        match &self.overlay {
            None => base,
            Some(overlay) => {
                let layered = overlay.decide(target, args);
                if layered.0.strictness() > base.0.strictness() {
                    layered
                } else {
                    base
                }
            }
        }
    }

    fn decide_local(&self, target: &str, args: &Value) -> (Decision, Option<String>) {
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
        (self.default, self.default_reason.clone())
    }

    /// Layer `profile` on top of this policy: every decision becomes the
    /// stricter of the two. This is the per-session selection mechanism — a
    /// caller-picked profile can deny or gate what the operator's policy
    /// allows, but can never allow what the operator denies.
    pub fn restricted_by(&self, profile: Arc<PolicyConfig>) -> PolicyConfig {
        let mut base = self.clone();
        base.overlay = Some(match base.overlay.take() {
            None => profile,
            Some(existing) => Arc::new(existing.restricted_by(profile)),
        });
        base
    }
}

/// Names of the built-in profiles selectable via `CHIDORI_POLICY_PROFILE`,
/// the `--untrusted` CLI flag, or a session's `policy_profile` field.
pub const BUILTIN_PROFILES: &[&str] = &["untrusted", "supervised"];

/// Build a ready-made [`PolicyConfig`] by name, or `None` for an unknown name.
///
/// * `"untrusted"` — deny-by-default: gated effects are hard-refused unless
///   on the read-only workspace allowlist.
/// * `"supervised"` — ask-by-default: the same allowlist, but unmatched gated
///   effects pause the run for operator approval (the server's
///   `awaiting_approval` / `/approve` flow) instead of failing outright.
///
/// Both are opt-in and never affect the default profile, which keeps the
/// historical `AlwaysAllow` fallback.
pub fn builtin_profile(name: &str) -> Option<PolicyConfig> {
    match name {
        "untrusted" => Some(untrusted_profile()),
        "supervised" => Some(supervised_profile()),
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
    PolicyConfig {
        rules: read_only_workspace_allowlist(),
        default: Decision::NeverAllow,
        default_reason: None,
        overlay: None,
    }
}

/// The policy `chidori serve` runs under when the operator has not configured
/// one. The server is the surface untrusted callers reach, so sessions there
/// are deny-by-default out of the box: the `untrusted` profile, with a
/// fallback reason telling the operator how to relax it. `chidori run` keeps
/// the permissive default — the primary model for local CLI runs is trusted,
/// developer-authored agent code.
pub fn serve_default_profile() -> PolicyConfig {
    let mut cfg = untrusted_profile();
    cfg.default_reason = Some(
        "chidori serve is deny-by-default: configure CHIDORI_POLICY / CHIDORI_POLICY_FILE / \
         CHIDORI_POLICY_PROFILE, or start the server with --trusted, to allow this effect"
            .to_string(),
    );
    cfg
}

/// Ask-by-default profile: identical allowlist to `untrusted`, but unmatched
/// gated effects resolve to `AskBefore` — under the server's pause flow the
/// run suspends as `awaiting_approval` and the operator decides per call
/// (approvals are remembered per (target, args) for the session). On the bare
/// CLI, where nothing can answer the prompt, the call errors instead.
fn supervised_profile() -> PolicyConfig {
    PolicyConfig {
        rules: read_only_workspace_allowlist(),
        default: Decision::AskBefore,
        default_reason: None,
        overlay: None,
    }
}

fn read_only_workspace_allowlist() -> Vec<PolicyRule> {
    let allow_read_only = |target: &str| PolicyRule {
        target: target.to_string(),
        decision: Decision::AlwaysAllow,
        match_args: None,
        reason: Some("read-only workspace introspection".to_string()),
    };
    vec![
        allow_read_only("workspace:list"),
        allow_read_only("workspace:read"),
        allow_read_only("workspace:manifest"),
    ]
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
    fn supervised_profile_asks_by_default_and_allows_read_only() {
        let cfg = builtin_profile("supervised").expect("supervised profile exists");
        assert_eq!(cfg.default, Decision::AskBefore);

        let (decision, _) = cfg.decide("http", &json!({ "url": "https://example.com" }));
        assert_eq!(decision, Decision::AskBefore);
        let (decision, _) = cfg.decide("workspace:write", &json!({ "path": "a.txt" }));
        assert_eq!(decision, Decision::AskBefore);
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
    fn overlay_tightens_a_permissive_base() {
        let base = PolicyConfig::default(); // AlwaysAllow fallback
        let layered = base.restricted_by(Arc::new(builtin_profile("untrusted").unwrap()));

        let (decision, _) = layered.decide("http", &json!({}));
        assert_eq!(decision, Decision::NeverAllow);
        let (decision, _) = layered.decide("workspace:write", &json!({}));
        assert_eq!(decision, Decision::NeverAllow);
        // Allowed on both sides stays allowed.
        let (decision, _) = layered.decide("workspace:read", &json!({}));
        assert_eq!(decision, Decision::AlwaysAllow);
    }

    #[test]
    fn overlay_cannot_relax_a_restrictive_base() {
        // Operator policy denies http outright; a session-selected supervised
        // profile (AskBefore) must not downgrade that to a mere prompt.
        let base = PolicyConfig {
            rules: vec![PolicyRule {
                target: "http".to_string(),
                decision: Decision::NeverAllow,
                match_args: None,
                reason: Some("operator denies egress".to_string()),
            }],
            default: Decision::AlwaysAllow,
            default_reason: None,
            overlay: None,
        };
        let layered = base.restricted_by(Arc::new(builtin_profile("supervised").unwrap()));

        let (decision, reason) = layered.decide("http", &json!({}));
        assert_eq!(decision, Decision::NeverAllow);
        assert_eq!(reason.as_deref(), Some("operator denies egress"));
        // Targets the base allows fall to the overlay's AskBefore.
        let (decision, _) = layered.decide("workspace:write", &json!({}));
        assert_eq!(decision, Decision::AskBefore);
    }

    #[test]
    fn overlay_stacks_recursively() {
        let base = PolicyConfig::default();
        let layered = base
            .restricted_by(Arc::new(builtin_profile("supervised").unwrap()))
            .restricted_by(Arc::new(builtin_profile("untrusted").unwrap()));
        // The strictest of all layers wins.
        let (decision, _) = layered.decide("http", &json!({}));
        assert_eq!(decision, Decision::NeverAllow);
        let (decision, _) = layered.decide("workspace:read", &json!({}));
        assert_eq!(decision, Decision::AlwaysAllow);
    }

    #[test]
    fn serve_default_profile_denies_with_actionable_reason() {
        let cfg = serve_default_profile();
        assert_eq!(cfg.default, Decision::NeverAllow);

        let (decision, reason) = cfg.decide("http", &json!({ "url": "https://example.com" }));
        assert_eq!(decision, Decision::NeverAllow);
        let reason = reason.expect("serve default denial carries a how-to-relax reason");
        assert!(
            reason.contains("--trusted"),
            "reason should name the opt-out: {reason}"
        );
        assert!(
            reason.contains("CHIDORI_POLICY"),
            "reason should name the env config: {reason}"
        );

        let (decision, reason) = cfg.decide("workspace:write", &json!({ "path": "a.txt" }));
        assert_eq!(decision, Decision::NeverAllow);
        assert!(reason.is_some());
    }

    #[test]
    fn serve_default_profile_keeps_read_only_workspace_allowlist() {
        let cfg = serve_default_profile();
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
