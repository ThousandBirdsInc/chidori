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
