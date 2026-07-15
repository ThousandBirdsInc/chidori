//! OS-level isolation: run an agent in a child process and broker its host
//! effects back over a pipe.
//!
//! The design and rationale live in `docs/os-isolation-plan.md`. The short
//! version: the host-call boundary (`run_module`'s single `(op, args) -> JSON`
//! seam) doubles as a serialization boundary, so the JavaScript engine can be
//! moved into a disposable child process while every powerful effect stays in
//! the trusted parent. Phases 1-5 are implemented: the worker, the broker, and
//! the wire protocol (`worker`/`supervisor`/`protocol`); rlimits and a
//! deadline-kill (`limits`); and the per-OS sandbox (`sandbox`) — seccomp,
//! network namespaces, and Landlock on Linux; Seatbelt on macOS — so the child
//! runs with brokered effects *and* syscall/filesystem/network confinement.

pub mod limits;
pub mod protocol;
pub mod sandbox;
pub mod supervisor;
pub mod worker;

pub(crate) use supervisor::run_agent_isolated;

/// Whether OS isolation is enabled for this process, controlled by the
/// `CHIDORI_ISOLATE` environment variable. Any value other than the unset /
/// empty / explicitly-falsey forms (`0`, `off`, `false`, `no`) turns it on;
/// `process` is the canonical value. The worker child always has this
/// explicitly off (the supervisor sets `off`), so a worker never recursively
/// re-isolates.
///
/// Embedders (and the test harness) see the historical opt-in behavior: unset
/// means off. The `chidori` CLI flips the default at startup via
/// [`default_on_if_unset`], so `run`/`serve` isolate out of the box on
/// platforms with a worker sandbox and `--no-isolate` / `CHIDORI_ISOLATE=off`
/// are the opt-outs.
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

/// Explicitly turn OS isolation off (`--no-isolate`). Sets the falsey value
/// rather than unsetting so the choice survives [`default_on_if_unset`] and is
/// inherited by child processes.
pub fn disable() {
    std::env::set_var("CHIDORI_ISOLATE", "off");
}

/// Default-on isolation for the CLI: when the operator has expressed no
/// preference (`CHIDORI_ISOLATE` unset), turn isolation on wherever the
/// process-worker mechanism is supported (Unix; Linux and macOS additionally
/// get an OS sandbox layer). Called once at `chidori` startup — an explicit
/// env value, `--isolate`, or `--no-isolate` always wins.
pub fn default_on_if_unset() {
    if cfg!(unix) && std::env::var_os("CHIDORI_ISOLATE").is_none() {
        enable();
    }
}

/// A one-line, human-readable description of the isolation posture, for startup
/// banners. Describes *intent*: the worker logs to stderr what actually stuck
/// on this host. The platform's core layer (seccomp / Seatbelt) fails closed by
/// default; the auxiliary layers are best-effort.
pub fn describe() -> String {
    if !enabled() {
        return "off (agents run in-process; pass --isolate or unset CHIDORI_ISOLATE to sandbox)"
            .to_string();
    }
    let layers = if cfg!(target_os = "linux") {
        "Linux: network namespace + Landlock + seccomp; fails closed if seccomp can't apply"
    } else if cfg!(target_os = "macos") {
        "macOS: Seatbelt profile; fails closed if it can't apply"
    } else {
        "no OS sandbox layer on this platform — process separation only"
    };
    format!("on — process-per-run worker ({layers})")
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
