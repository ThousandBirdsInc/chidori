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
//! The hard *memory* ceiling is cgroup v2 `memory.max`
//! (`CHIDORI_ISOLATE_MEMORY_MAX_MB`, Linux only): the worker enters its own
//! leaf cgroup — under `CHIDORI_ISOLATE_CGROUP_DIR` when delegated, else as a
//! sibling of its own cgroup — with `memory.max`, `memory.swap.max=0`, and
//! `memory.oom.group=1` set before agent code runs, so an OOM kills the whole
//! worker atomically and the fleet can bin-pack on a kernel-enforced number
//! instead of the watchdog's soft one. Best-effort like everything here: no
//! delegation means an audible fallback to the watchdog.
//!
//! Deliberately *not* set here: `RLIMIT_AS` (address-space caps are too blunt —
//! a multi-threaded VM reserves far more virtual memory than it resides, so an
//! AS cap kills healthy runs; the heap watchdog and the cgroup `memory.max`
//! above are the right tools) and `RLIMIT_NPROC` (counts every process of
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
    /// Hard, kernel-enforced memory ceiling via cgroup v2 `memory.max`
    /// (Linux only). `None` (the default) leaves the polled heap watchdog as
    /// the only memory bound — a *soft* limit a run can overshoot by
    /// whatever it allocates within one poll interval, which is the wrong
    /// thing to bin-pack a fleet on. With a cap set, the worker enters its
    /// own leaf cgroup with `memory.max` (and `memory.oom.group`, so an OOM
    /// kills the whole worker atomically, never a random thread) before any
    /// agent code runs. Env: `CHIDORI_ISOLATE_MEMORY_MAX_MB` (0 disables).
    #[serde(default)]
    pub memory_max_bytes: Option<u64>,
    /// Where to create the per-worker cgroup: a delegated cgroup v2
    /// directory the chidori user may write (e.g. what systemd `Delegate=`
    /// hands a service, or a directory the operator chowned under
    /// `/sys/fs/cgroup`). Unset, the worker tries the parent of its own
    /// cgroup — right wherever the service runs as a leaf under a slice
    /// with the memory controller enabled. Env: `CHIDORI_ISOLATE_CGROUP_DIR`.
    #[serde(default)]
    pub cgroup_dir: Option<String>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            cpu_secs: None,
            fsize_bytes: None,
            nofile: Some(256),
            no_core: true,
            memory_max_bytes: None,
            cgroup_dir: None,
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
        if let Some(mb) = env_u64("CHIDORI_ISOLATE_MEMORY_MAX_MB") {
            limits.memory_max_bytes = (mb > 0).then(|| mb.saturating_mul(1024 * 1024));
        }
        if let Ok(dir) = std::env::var("CHIDORI_ISOLATE_CGROUP_DIR") {
            let dir = dir.trim();
            if !dir.is_empty() {
                limits.cgroup_dir = Some(dir.to_string());
            }
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
        #[cfg(target_os = "linux")]
        if let Some(bytes) = self.memory_max_bytes {
            match enter_memory_cgroup(bytes, self.cgroup_dir.as_deref(), std::process::id()) {
                Ok(path) => {
                    tracing::debug!("isolate worker: memory.max={bytes} at {}", path.display());
                }
                // Best-effort like every limit here: the polled heap watchdog
                // is still in force, so degrade audibly, never fatally.
                Err(err) => eprintln!(
                    "isolate worker: cgroup memory.max unavailable ({err}); the polled heap \
                     watchdog remains the only memory bound — for a hard ceiling run the \
                     server under a delegated cgroup (systemd Delegate=yes) or point \
                     CHIDORI_ISOLATE_CGROUP_DIR at a writable cgroup v2 directory"
                ),
            }
        }
    }

    #[cfg(not(unix))]
    pub fn apply_to_self(&self) {}
}

/// Where the per-worker cgroup for `pid` lives: `chidori-worker-<pid>` under
/// the configured delegated directory, or under the parent of `base_cgroup`
/// (the cgroup the server and its workers share) so a service running as a
/// leaf under a slice with the memory controller enabled needs no
/// configuration at all. One derivation shared by the worker (which enters
/// it) and the supervisor (which removes it after reaping the child).
#[cfg(target_os = "linux")]
fn worker_cgroup_dir(dir_override: Option<&str>, pid: u32) -> Result<std::path::PathBuf, String> {
    let base = match dir_override {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let own = own_cgroup_dir()?;
            own.parent()
                .map(|p| p.to_path_buf())
                .ok_or_else(|| "own cgroup has no parent".to_string())?
        }
    };
    Ok(base.join(format!("chidori-worker-{pid}")))
}

/// This process's cgroup v2 directory, from `/proc/self/cgroup` (the
/// unified-hierarchy `0::` line) joined onto `/sys/fs/cgroup`.
#[cfg(target_os = "linux")]
fn own_cgroup_dir() -> Result<std::path::PathBuf, String> {
    let text = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("reading /proc/self/cgroup: {e}"))?;
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| "no cgroup v2 (unified hierarchy) entry".to_string())?
        .trim();
    Ok(std::path::PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// Create the worker's leaf cgroup, set its limits, and move this process in.
/// Limits are written BEFORE the move so there is no unbounded window. The
/// `memory.max` write is the make-or-break step: it fails cleanly when the
/// memory controller is not enabled for the directory (no delegation), which
/// is the signal to fall back to the watchdog.
#[cfg(target_os = "linux")]
fn enter_memory_cgroup(
    bytes: u64,
    dir_override: Option<&str>,
    pid: u32,
) -> Result<std::path::PathBuf, String> {
    let dir = worker_cgroup_dir(dir_override, pid)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::write(dir.join("memory.max"), bytes.to_string())
        .map_err(|e| format!("writing memory.max: {e}"))?;
    // Best-effort companions: no swap escape hatch past the cap, and an OOM
    // kills the whole worker atomically (never one random thread, which
    // would leave a wedged half-dead VM behind the broker pipe).
    let _ = std::fs::write(dir.join("memory.swap.max"), "0");
    let _ = std::fs::write(dir.join("memory.oom.group"), "1");
    std::fs::write(dir.join("cgroup.procs"), pid.to_string())
        .map_err(|e| format!("entering {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Supervisor-side cleanup: remove the reaped worker's cgroup directory (an
/// empty cgroup rmdirs; a non-empty or never-created one is left alone).
/// Derives the same path the worker entered, from the same environment.
#[cfg(target_os = "linux")]
pub fn cleanup_worker_cgroup(pid: u32) {
    let limits = ResourceLimits::from_env();
    if limits.memory_max_bytes.is_none() {
        return;
    }
    if let Ok(dir) = worker_cgroup_dir(limits.cgroup_dir.as_deref(), pid) {
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn cleanup_worker_cgroup(_pid: u32) {}

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
            nofile: Some(hard.saturating_add(1_000_000)),
            no_core: false,
            ..ResourceLimits::default()
        };
        // Should clamp to `hard` rather than EPERM-fail; no panic, no change past
        // the hard cap.
        limits.apply_to_self();
        assert_eq!(current_hard(libc::RLIMIT_NOFILE), Some(hard));
    }

    /// The delegated-directory path (`enter_memory_cgroup` against
    /// CHIDORI_ISOLATE_CGROUP_DIR): limits land in the interface files
    /// BEFORE the pid enters, and the supervisor-side derivation names the
    /// same directory so cleanup can rmdir it after the reap. A tempdir
    /// stands in for the delegated cgroupfs directory — the fs writes are
    /// identical, only the kernel side effects differ.
    #[cfg(target_os = "linux")]
    #[test]
    fn enter_memory_cgroup_writes_limits_then_membership() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().into_owned();
        let entered = enter_memory_cgroup(64 * 1024 * 1024, Some(&base), 4242).unwrap();
        assert_eq!(entered, worker_cgroup_dir(Some(&base), 4242).unwrap());
        let read = |name: &str| std::fs::read_to_string(entered.join(name)).unwrap();
        assert_eq!(read("memory.max"), (64 * 1024 * 1024u64).to_string());
        assert_eq!(read("memory.swap.max"), "0");
        assert_eq!(read("memory.oom.group"), "1");
        assert_eq!(read("cgroup.procs"), "4242");
    }

    /// Without a delegated dir, the worker targets a SIBLING of its own
    /// cgroup — entering its own cgroup's subdirectory would trip cgroup
    /// v2's no-internal-process rule wherever the parent holds processes.
    #[cfg(target_os = "linux")]
    #[test]
    fn default_worker_cgroup_is_a_sibling_of_our_own() {
        match (own_cgroup_dir(), worker_cgroup_dir(None, 7)) {
            (Ok(own), Ok(worker)) => {
                assert_eq!(worker.parent(), own.parent());
                assert_eq!(
                    worker.file_name().and_then(|n| n.to_str()),
                    Some("chidori-worker-7")
                );
            }
            // Hosts without cgroup v2 report an error rather than a wrong path.
            (Err(_), result) => assert!(result.is_err()),
            (Ok(_), Err(_)) => panic!("own cgroup resolved but worker path did not"),
        }
    }
}
