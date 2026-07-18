//! Startup / session-creation preflight for policy-vs-agent mismatches.
//!
//! Chidori agents do NOT statically declare their effects anywhere — there is
//! no manifest field like `effects: ["workspace:write"]`; effects are whatever
//! host calls the agent's code makes at runtime (`fetch(...)`,
//! `chidori.workspace.write(...)`, ...). The closest honest preflight is
//! therefore a *static source scan*: grep the agent's TypeScript source for
//! the well-known spellings of the policy-gated effect surfaces and check each
//! referenced target against the active policy. When a referenced target is
//! *unconditionally* denied — no argument shape could ever get it past the
//! policy — the server prints a startup warning instead of letting the first
//! real run fail mid-flight.
//!
//! This is deliberately a warning, never a refusal: the scan is a heuristic
//! (an agent may mention `fetch` in dead code, or reach an effect through a
//! spelling the scan does not know), so the server always starts. False
//! negatives are fine (the run fails later with the same policy error the
//! warning would have quoted); false positives are kept rare by only warning
//! when the policy denies the target for EVERY possible argument.

use std::path::Path;

use serde_json::json;

use crate::policy::{Decision, PolicyConfig};

/// One statically-scanned reference to a policy-gated effect surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StaticEffectRef {
    /// The policy target the reference resolves to (e.g. `workspace:write`).
    pub(super) target: &'static str,
    /// The source spelling that evidenced it (e.g. `chidori.workspace.write`).
    pub(super) evidence: &'static str,
}

/// The gated effect surfaces the scanner knows, as (needle, target, evidence).
/// `needle` is matched as a substring of the agent source with a cheap
/// identifier-boundary check on the preceding character, so `myFetch(` does
/// not count as `fetch(`. Only the *powerful* (policy-gated) surfaces are
/// listed — pure effects (`log`, `prompt`, ...) never reach the policy gate.
const EFFECT_MARKERS: &[(&str, &str, &str)] = &[
    ("fetch(", "http", "fetch(...)"),
    ("node:http", "http", "node:http import"),
    ("node:https", "http", "node:https import"),
    (
        "workspace.write(",
        "workspace:write",
        "chidori.workspace.write",
    ),
    (
        "workspace.delete(",
        "workspace:delete",
        "chidori.workspace.delete",
    ),
    (
        "workspace.remove(",
        "workspace:delete",
        "chidori.workspace.remove",
    ),
    (
        "workspace.read(",
        "workspace:read",
        "chidori.workspace.read",
    ),
    (
        "workspace.list(",
        "workspace:list",
        "chidori.workspace.list",
    ),
    (
        "workspace.manifest(",
        "workspace:manifest",
        "chidori.workspace.manifest",
    ),
];

/// True when `source` contains `needle` at an identifier boundary: the
/// character before the match must not be part of an identifier, so
/// `prefetch(` / `myFetch(` do not evidence the `http` effect while
/// `fetch(`, `globalThis.fetch(`, `await fetch(` all do.
fn contains_at_identifier_boundary(source: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = source[from..].find(needle) {
        let abs = from + pos;
        let boundary = abs == 0
            || !source[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if boundary {
            return true;
        }
        from = abs + needle.len();
    }
    false
}

/// Scan agent source for references to policy-gated effect surfaces.
/// Best-effort and entry-file-only: imports are not followed and commented-out
/// code counts — acceptable for a warning-only preflight.
pub(super) fn static_effect_refs(source: &str) -> Vec<StaticEffectRef> {
    let mut refs: Vec<StaticEffectRef> = Vec::new();
    for (needle, target, evidence) in EFFECT_MARKERS {
        if contains_at_identifier_boundary(source, needle)
            && !refs.iter().any(|r| r.target == *target)
        {
            refs.push(StaticEffectRef { target, evidence });
        }
    }
    refs
}

/// True when SOME argument shape could get a call to `target` past this
/// config layer (i.e. resolve to a decision other than `NeverAllow`).
/// Rules are first-match-wins: a rule with `match_args` only covers the args
/// it matches, so scanning continues past it; a rule without `match_args`
/// covers every remaining argument shape and ends the scan.
fn local_may_allow(cfg: &PolicyConfig, target: &str) -> bool {
    for rule in &cfg.rules {
        if rule.target != target && rule.target != "*" {
            continue;
        }
        if rule.match_args.is_some() {
            if rule.decision != Decision::NeverAllow {
                return true;
            }
        } else {
            return rule.decision != Decision::NeverAllow;
        }
    }
    cfg.default != Decision::NeverAllow
}

/// True when some argument shape could get `target` past the FULL layered
/// policy (base plus overlays; overlays only tighten, so every layer must
/// leave a path open). Approximate in exactly one direction: it may report
/// `true` when the layers' allowed argument sets do not actually intersect,
/// which suppresses a warning — never fabricates one.
fn may_allow(cfg: &PolicyConfig, target: &str) -> bool {
    local_may_allow(cfg, target)
        && cfg
            .overlay
            .as_ref()
            .is_none_or(|overlay| may_allow(overlay, target))
}

/// The effect targets referenced by `source` that the active policy denies
/// for every possible argument, each with the denial reason the run would
/// have surfaced. Empty when nothing is unconditionally denied.
pub(super) fn denied_static_effects(
    source: &str,
    policy: &PolicyConfig,
) -> Vec<(StaticEffectRef, Option<String>)> {
    static_effect_refs(source)
        .into_iter()
        .filter(|r| !may_allow(policy, r.target))
        .map(|r| {
            let (_, reason) = policy.decide(r.target, &json!({}));
            (r, reason)
        })
        .collect()
}

/// Print a stderr warning for every effect surface the agent's source
/// statically references that `policy` unconditionally denies. `posture` is
/// the human-readable name of the active policy (the startup banner's policy
/// posture, or a session's profile name). Warning only — never refuses.
/// Unreadable agent files are silently skipped (the run itself will surface
/// the real error).
pub(super) fn warn_denied_static_effects(agent_path: &Path, policy: &PolicyConfig, posture: &str) {
    let Ok(source) = std::fs::read_to_string(agent_path) else {
        return;
    };
    for (r, reason) in denied_static_effects(&source, policy) {
        eprintln!(
            "WARNING: the agent references {} but the active policy ({posture}) \
             unconditionally denies `{}`; runs that attempt it will fail{}",
            r.evidence,
            r.target,
            reason
                .map(|reason| format!(" — {reason}"))
                .unwrap_or_default(),
        );
    }
}
