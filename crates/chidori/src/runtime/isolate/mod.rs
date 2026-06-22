//! OS-level isolation: run an agent in a child process and broker its host
//! effects back over a pipe.
//!
//! The design and rationale live in `docs/os-isolation-plan.md`. The short
//! version: the host-call boundary (`run_module`'s single `(op, args) -> JSON`
//! seam) doubles as a serialization boundary, so the JavaScript engine can be
//! moved into a disposable child process while every powerful effect stays in
//! the trusted parent. This module is **phase 1** — the worker, the broker, and
//! the wire protocol. The per-OS sandbox (seccomp / namespaces on Linux,
//! Seatbelt on macOS) lands in later phases; until then the child is a separate
//! process with brokered effects but no syscall confinement yet.

pub mod limits;
pub mod protocol;
pub mod sandbox;
pub mod supervisor;
pub mod worker;

pub(crate) use supervisor::run_agent_isolated;

/// Whether OS isolation is enabled for this process, controlled by the
/// `CHIDORI_ISOLATE` environment variable. Any value other than the unset /
/// empty / explicitly-falsey forms (`0`, `off`, `false`, `no`) turns it on;
/// `process` is the canonical value. The worker child always has this unset (the
/// supervisor strips it), so a worker never recursively re-isolates.
pub fn enabled() -> bool {
    match std::env::var("CHIDORI_ISOLATE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        Err(_) => false,
    }
}

/// Turn OS isolation on for this process and the workers it spawns, as if
/// `CHIDORI_ISOLATE=process` were set. Centralizes the env-var name so the CLI
/// flags (`run --isolate`, `serve --isolate`) and the env path agree.
pub fn enable() {
    std::env::set_var("CHIDORI_ISOLATE", "process");
}

/// A one-line, human-readable description of the isolation posture, for startup
/// banners. Describes *intent*: the worker applies each layer best-effort and
/// logs to stderr what actually stuck on this host.
pub fn describe() -> String {
    if !enabled() {
        return "off (agents run in-process)".to_string();
    }
    let layers = if cfg!(target_os = "linux") {
        "Linux: network namespace + Landlock + seccomp"
    } else if cfg!(target_os = "macos") {
        "macOS: Seatbelt profile"
    } else {
        "no OS sandbox layer on this platform"
    };
    format!("on — process-per-run worker ({layers}; best-effort)")
}

/// If untrusted code is being run without OS isolation, nudge the operator that
/// `--isolate` is available. No-op when isolation is already on. Per the
/// orthogonal-but-composable design, this never *enables* isolation itself.
pub fn warn_if_untrusted_without_isolation(untrusted: bool) {
    if untrusted && !enabled() {
        eprintln!(
            "note: running under the untrusted policy without OS isolation. Pass --isolate \
             (or set CHIDORI_ISOLATE=process) to also sandbox each run in a confined child process."
        );
    }
}
