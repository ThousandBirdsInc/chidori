//! Per-process resource limits for the OS-isolation worker (phase 2).
//!
//! These are the cross-Unix *floor* of the isolation story: cheap, unprivileged
//! limits the worker applies to itself right after the [`Init`] handoff, before
//! a single line of agent code runs. They are backstops, not the primary guard —
//! the opcode budget still bounds compute gracefully in-engine and the
//! counting-allocator watchdog still bounds the heap. What these add is a *hard*,
//! kernel-enforced ceiling that does not depend on the engine cooperating:
//!
//! * `RLIMIT_CPU` — a hard CPU-seconds cap. Unlike a wall-clock deadline it does
//!   **not** count time the child spends blocked waiting on a brokered host
//!   effect (that is not CPU time), so it bounds runaway *compute* without
//!   penalising a legitimately slow agent. The natural hard backstop to the
//!   in-engine opcode budget.
//! * `RLIMIT_FSIZE` — max bytes the child may write to a regular file. The child
//!   has no filesystem (every `node:fs` op is brokered), so the default of `0`
//!   costs nothing and slams the door on any stray write. Pipes/sockets are
//!   exempt, so the stdout protocol channel is unaffected.
//! * `RLIMIT_CORE` — no core dumps (a crash must not splatter process memory to
//!   disk).
//! * `RLIMIT_NOFILE` — a small open-file ceiling.
//!
//! Deliberately *not* set here: `RLIMIT_AS` (address-space caps are too blunt —
//! a multi-threaded VM reserves far more virtual memory than it resides, so an
//! AS cap kills healthy runs; the heap watchdog and, later, a cgroup
//! `memory.max` are the right tools) and `RLIMIT_NPROC` (counts every process of
//! the real uid, so a low cap fails unpredictably under concurrency; blocking
//! `fork` belongs to the seccomp phase). Both are tracked in
//! `docs/os-isolation-plan.md`.

use serde::{Deserialize, Serialize};

/// The resource limits a worker applies to itself. Computed in the parent from
/// the environment and shipped in [`super::protocol::FromParent::Init`] so the
/// policy lives in one place and the child just enforces what it is told.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Hard CPU-seconds ceiling (`RLIMIT_CPU`). `None` leaves it unset (the
    /// default — a hard CPU kill is opt-in, since the opcode budget already
    /// bounds compute gracefully). Env: `CHIDORI_ISOLATE_CPU_SECS`.
    pub cpu_secs: Option<u64>,
    /// Max bytes the child may write to any regular file (`RLIMIT_FSIZE`).
    /// **Off by default** (`None`): a `0` cap is too blunt — it also kills writes
    /// to an inherited `stderr` that happens to be a *regular file* (redirected
    /// logs), which the worker legitimately uses for diagnostics. File-write
    /// confinement is Landlock's job (it blocks *opening* new files while leaving
    /// inherited fds alone); see [`super::sandbox`]. Opt in via
    /// `CHIDORI_ISOLATE_FSIZE_BYTES` for workloads with no such stderr.
    pub fsize_bytes: Option<u64>,
    /// Max open file descriptors (`RLIMIT_NOFILE`), clamped to the inherited hard
    /// limit. Defaults to `Some(256)`. Env: `CHIDORI_ISOLATE_NOFILE`.
    pub nofile: Option<u64>,
    /// Disable core dumps (`RLIMIT_CORE = 0`). Defaults to `true`.
    pub no_core: bool,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            cpu_secs: None,
            fsize_bytes: None,
            nofile: Some(256),
            no_core: true,
        }
    }
}

impl ResourceLimits {
    /// Resolve the limits to apply from the environment, layering over
    /// [`Default`]. Parsing is forgiving: a malformed value falls back to the
    /// default for that field rather than failing the run.
    pub fn from_env() -> Self {
        fn env_u64(key: &str) -> Option<u64> {
            std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
        }
        let mut limits = ResourceLimits::default();
        if let Some(secs) = env_u64("CHIDORI_ISOLATE_CPU_SECS") {
            limits.cpu_secs = (secs > 0).then_some(secs);
        }
        if let Some(bytes) = env_u64("CHIDORI_ISOLATE_FSIZE_BYTES") {
            limits.fsize_bytes = Some(bytes);
        }
        if let Some(n) = env_u64("CHIDORI_ISOLATE_NOFILE") {
            limits.nofile = (n > 0).then_some(n);
        }
        if let Ok(v) = std::env::var("CHIDORI_ISOLATE_NO_CORE") {
            let v = v.trim().to_ascii_lowercase();
            limits.no_core = !matches!(v.as_str(), "0" | "off" | "false" | "no");
        }
        limits
    }

    /// Apply every configured limit to the *current* process. Best-effort: a
    /// failure to set one limit is reported to stderr and skipped, never fatal —
    /// a missing backstop should degrade isolation, not break the run. No-op on
    /// non-Unix platforms (the per-OS story there is a later phase).
    #[cfg(unix)]
    pub fn apply_to_self(&self) {
        if let Some(secs) = self.cpu_secs {
            // Give the *hard* limit one second of headroom over the soft limit so
            // the soft `RLIMIT_CPU` fires `SIGXCPU` first (whose default action
            // terminates the process). With soft == hard the kernel jumps
            // straight to `SIGKILL`, which is indistinguishable from an OOM kill.
            set_rlimit(libc::RLIMIT_CPU, secs, secs.saturating_add(1), "RLIMIT_CPU");
        }
        if let Some(bytes) = self.fsize_bytes {
            set_rlimit(libc::RLIMIT_FSIZE, bytes, bytes, "RLIMIT_FSIZE");
        }
        if self.no_core {
            set_rlimit(libc::RLIMIT_CORE, 0, 0, "RLIMIT_CORE");
        }
        if let Some(n) = self.nofile {
            // Never try to *raise* NOFILE above the inherited hard limit — that
            // fails with EPERM for an unprivileged process. Clamp instead.
            let target = current_hard(libc::RLIMIT_NOFILE).map_or(n, |hard| n.min(hard));
            set_rlimit(libc::RLIMIT_NOFILE, target, target, "RLIMIT_NOFILE");
        }
    }

    #[cfg(not(unix))]
    pub fn apply_to_self(&self) {}
}

/// The integer type `setrlimit`/`getrlimit` take for the resource selector:
/// `__rlimit_resource_t` on glibc/Linux, plain `c_int` everywhere else
/// (macOS/BSD). Matches the type of the `libc::RLIMIT_*` constants per platform,
/// so callers pass `libc::RLIMIT_CPU` etc. unchanged.
#[cfg(target_os = "linux")]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(target_os = "linux")))]
type RlimitResource = libc::c_int;

/// Set the soft (`cur`) and hard (`max`) limit of `resource`. The kernel applies
/// `RLIM_INFINITY` semantics; we only translate `u64`. The `as rlim_t` casts are
/// load-bearing for portability — `rlim_t` is not `u64` on every Unix — even
/// where this target makes them a no-op.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn set_rlimit(resource: RlimitResource, soft: u64, hard: u64, name: &str) {
    let rlim = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: `setrlimit` reads a single well-formed `rlimit` we own; it only
    // affects this process and cannot violate Rust's memory model.
    let rc = unsafe { libc::setrlimit(resource, &rlim) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("isolate worker: failed to set {name} (soft={soft}, hard={hard}): {err}");
    }
}

/// The inherited hard limit for `resource`, if it can be read.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // `rlim_t` width is platform-dependent
fn current_hard(resource: RlimitResource) -> Option<u64> {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into a single `rlimit` we own and borrow mutably.
    let rc = unsafe { libc::getrlimit(resource, &mut rlim) };
    (rc == 0).then_some(rlim.rlim_max as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let l = ResourceLimits::default();
        // FSIZE is off by default: a 0 cap also kills writes to a redirected
        // (regular-file) stderr, which the worker uses for diagnostics.
        assert_eq!(l.fsize_bytes, None);
        assert_eq!(l.nofile, Some(256));
        assert!(l.no_core);
        assert_eq!(l.cpu_secs, None);
    }

    #[test]
    fn from_env_is_forgiving_and_serializes() {
        // Round-trips through the wire format (it rides the Init frame).
        let l = ResourceLimits::from_env();
        let json = serde_json::to_string(&l).unwrap();
        let back: ResourceLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(back.no_core, l.no_core);
        assert_eq!(back.fsize_bytes, l.fsize_bytes);
    }

    // Applying limits mutates the test process's own rlimits, so keep it to a
    // raise-the-floor no-op: setting NOFILE to its current hard limit must
    // succeed and not panic.
    #[cfg(unix)]
    #[test]
    fn apply_nofile_clamps_to_hard_limit() {
        let hard = current_hard(libc::RLIMIT_NOFILE).unwrap();
        let limits = ResourceLimits {
            cpu_secs: None,
            fsize_bytes: None,
            nofile: Some(hard.saturating_add(1_000_000)),
            no_core: false,
        };
        // Should clamp to `hard` rather than EPERM-fail; no panic, no change past
        // the hard cap.
        limits.apply_to_self();
        assert_eq!(current_hard(libc::RLIMIT_NOFILE), Some(hard));
    }
}
