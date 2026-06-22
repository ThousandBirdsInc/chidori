//! Linux seccomp-bpf syscall confinement for the isolation worker (phase 3).
//!
//! This is defense-in-depth *behind* the primary boundary. The engine already
//! has no ambient authority — there is no socket, no `exec`, no real filesystem
//! wired into it, and every effect is brokered to the parent — so the realistic
//! threat seccomp addresses is a hypothetical interpreter RCE that tries to issue
//! raw syscalls directly. Against that, this filter slams the door on the
//! highest-value targets: network egress, new-program execution, debugging,
//! namespace/privilege escalation, and kernel surface.
//!
//! **Denylist, not allowlist (for now).** The filter defaults to *allow* and
//! kills the process on a curated set of dangerous syscalls. A near-empty
//! allowlist is the stronger end state (and the documented goal — see
//! `docs/os-isolation-plan.md`), but it risks false-positive kills of the engine
//! on an unanticipated syscall and is fragile across libc/kernel versions. A
//! denylist cannot break a healthy run, ships real confinement today, and is a
//! clean base to tighten from. `fork`/`clone` are intentionally *not* denied —
//! the engine's watchdog thread needs them and a fork that cannot `exec` gains no
//! new code — so the `exec*` denial is what actually forecloses code execution.

/// What each best-effort confinement layer achieved for a worker. Layers that
/// could not be applied (older kernel, rootless container, …) leave their flag
/// `false` and append a human-readable reason to `notes`; the worker logs the
/// notes and, under `CHIDORI_ISOLATE_REQUIRE_SANDBOX`, fails closed if the
/// portable core (seccomp) did not apply.
#[derive(Debug, Default)]
pub struct SandboxOutcome {
    /// The worker runs in its own (empty) network namespace (Linux).
    pub network_isolated: bool,
    /// Landlock is enforcing a read-only view of the filesystem (Linux).
    pub landlock_enforced: bool,
    /// The seccomp denylist is installed (Linux).
    pub seccomp_applied: bool,
    /// A Seatbelt profile is confining the worker (macOS). Only ever set/read on
    /// macOS, so it is dead on other targets.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub seatbelt_applied: bool,
    /// Human-readable reasons for any layer that was skipped.
    pub notes: Vec<String>,
}

impl SandboxOutcome {
    /// Whether the platform's *primary* confinement is active — the gate for
    /// `CHIDORI_ISOLATE_REQUIRE_SANDBOX` (seccomp on Linux, Seatbelt on macOS).
    /// The namespace/Landlock layers are defense-in-depth on top of this.
    pub fn core_confined(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.seccomp_applied
        }
        #[cfg(target_os = "macos")]
        {
            self.seatbelt_applied
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            false
        }
    }
}

/// Apply every available confinement layer to the current process, best-effort;
/// the returned [`SandboxOutcome`] records what stuck. The per-OS work lives in
/// [`apply_linux`] / [`apply_macos`]; other platforms get nothing (yet).
///
/// Sound only when the caller *is* the dedicated worker process, since each layer
/// mutates the current process irreversibly.
pub fn apply() -> SandboxOutcome {
    let mut outcome = SandboxOutcome::default();

    #[cfg(target_os = "linux")]
    apply_linux(&mut outcome);
    #[cfg(target_os = "macos")]
    apply_macos(&mut outcome);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    outcome
        .notes
        .push("no OS sandbox layer is available on this platform".to_string());

    outcome
}

/// Linux confinement, ordered so each layer is still legal when the next runs:
/// network namespace + Landlock first (they need `unshare` / `landlock_*`, which
/// seccomp then denies), and the seccomp denylist last.
#[cfg(target_os = "linux")]
fn apply_linux(outcome: &mut SandboxOutcome) {
    match apply_network_namespace() {
        Ok(()) => outcome.network_isolated = true,
        Err(e) => outcome
            .notes
            .push(format!("network namespace not isolated: {e}")),
    }
    match apply_landlock_readonly() {
        Ok(true) => outcome.landlock_enforced = true,
        Ok(false) => outcome
            .notes
            .push("landlock not enforced: no kernel support".to_string()),
        Err(e) => outcome.notes.push(format!("landlock not enforced: {e}")),
    }
    match install_seccomp() {
        Ok(()) => outcome.seccomp_applied = true,
        Err(e) => outcome.notes.push(format!("seccomp not applied: {e}")),
    }
}

/// Move the worker into a fresh, empty network namespace (`unshare(CLONE_NEWNET)`)
/// — only loopback, and that down — so network egress is impossible at the kernel
/// level, belt-and-suspenders with the seccomp socket block. Needs `CAP_SYS_ADMIN`
/// (root or a privileged container); rootless callers fail with `EPERM` and the
/// layer is skipped. (Rootless support via an intermediate user namespace is a
/// future enhancement — see `docs/os-isolation-plan.md`.)
#[cfg(target_os = "linux")]
fn apply_network_namespace() -> Result<(), String> {
    // SAFETY: `unshare` takes a scalar flag and affects only this process.
    let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// Enforce a **read-only** view of the filesystem via Landlock: every write-class
/// access (create / write / truncate / rename / delete / mkdir / …) is denied,
/// while reads are left untouched so the C runtime can still load what it needs.
/// The worker brokers all of its I/O, so it never legitimately writes to disk —
/// this closes the filesystem-tamper surface the seccomp denylist deliberately
/// leaves open (it does not block `openat`, to avoid false-positive kills).
///
/// Best-effort (`CompatLevel::BestEffort`): on a kernel without Landlock the
/// ruleset reports `NotEnforced` (returns `Ok(false)`) rather than erroring.
/// Returns `Ok(true)` when Landlock is enforcing (fully or partially).
#[cfg(target_os = "linux")]
fn apply_landlock_readonly() -> Result<bool, String> {
    use landlock::{AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetStatus, ABI};

    // V5 (Linux 6.10) is a modern baseline; BestEffort downgrades the handled
    // write-access set to whatever the running kernel actually supports.
    let abi = ABI::V5;
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_write(abi))
        .map_err(|e| format!("handle_access: {e}"))?
        .create()
        .map_err(|e| format!("create: {e}"))?
        // No path rules are granted, so every handled (write) access is denied.
        .restrict_self()
        .map_err(|e| format!("restrict_self: {e}"))?;
    Ok(!matches!(status.ruleset, RulesetStatus::NotEnforced))
}

/// Install the worker's seccomp filter on the current thread; every thread or
/// child it later spawns inherits it. Returns `Ok(())` on success, or a
/// human-readable reason it could not be applied (a denied/absent `seccomp`
/// syscall, an unsupported arch, …) so the caller can decide between degrading
/// and failing closed.
#[cfg(target_os = "linux")]
pub fn install_seccomp() -> Result<(), String> {
    use std::collections::BTreeMap;

    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

    let arch = match std::env::consts::ARCH {
        "x86_64" => TargetArch::x86_64,
        "aarch64" => TargetArch::aarch64,
        "riscv64" => TargetArch::riscv64,
        other => return Err(format!("seccomp: unsupported architecture `{other}`")),
    };

    // An empty rule vec means "match this syscall unconditionally" → `match_action`.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for sysno in denied_syscalls() {
        rules.insert(sysno, vec![]);
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,       // mismatch (everything not listed): allow
        SeccompAction::KillProcess, // match (a denied syscall): SIGSYS, kill process
        arch,
    )
    .map_err(|e| format!("seccomp: building filter: {e}"))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| format!("seccomp: compiling filter: {e}"))?;
    // `apply_filter` sets `PR_SET_NO_NEW_PRIVS` first, so this works unprivileged.
    seccompiler::apply_filter(&program).map_err(|e| format!("seccomp: applying filter: {e}"))?;
    Ok(())
}

/// The denied syscalls. Restricted to numbers that exist on every Linux release
/// target (x86_64 and aarch64) so the table compiles on either.
#[cfg(target_os = "linux")]
// `c_long as i64` is load-bearing on targets where `c_long` is 32-bit, even
// though it is a no-op on the 64-bit targets we ship.
#[allow(clippy::unnecessary_cast)]
fn denied_syscalls() -> Vec<i64> {
    let denied: &[libc::c_long] = &[
        // Network: the worker never legitimately touches a socket — `http` is a
        // brokered effect performed by the parent.
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_shutdown,
        // New-program execution (what actually denies "run arbitrary code").
        libc::SYS_execve,
        libc::SYS_execveat,
        // Debugging / cross-process memory.
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        // Namespace / mount manipulation (sandbox-escape surface).
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        // Privilege changes.
        libc::SYS_setuid,
        libc::SYS_setgid,
        libc::SYS_setreuid,
        libc::SYS_setregid,
        libc::SYS_setresuid,
        libc::SYS_setresgid,
        libc::SYS_setfsuid,
        libc::SYS_setfsgid,
        libc::SYS_setgroups,
        // Kernel surface / modules / power.
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        // Keyrings.
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
    ];
    denied.iter().map(|n| *n as i64).collect()
}

// ---------------------------------------------------------------------------
// macOS (Seatbelt) — parity with the Linux posture: no network, no filesystem
// writes. Implemented behind the same best-effort contract as the Linux layers.
// ---------------------------------------------------------------------------

/// macOS confinement: apply a Seatbelt profile via `sandbox_init`.
#[cfg(target_os = "macos")]
fn apply_macos(outcome: &mut SandboxOutcome) {
    match apply_seatbelt() {
        Ok(()) => outcome.seatbelt_applied = true,
        Err(e) => outcome.notes.push(format!("seatbelt not applied: {e}")),
    }
}

/// The Seatbelt profile (SBPL). Allow-default with targeted denies — the same
/// philosophy as the Linux seccomp denylist + Landlock read-only posture, and
/// low-risk: a brokered compute worker still reads files and allocates freely,
/// it just can't reach the network or write to disk. Later rules win in SBPL, so
/// the denies override `(allow default)` for their operations.
#[cfg(target_os = "macos")]
const SEATBELT_PROFILE: &str = "\
(version 1)
(allow default)
(deny network*)
(deny file-write*)
";

/// Confine the current process with [`SEATBELT_PROFILE`]. Best-effort: a failure
/// is returned as a reason (the worker logs it and degrades), never a panic.
#[cfg(target_os = "macos")]
fn apply_seatbelt() -> Result<(), String> {
    use std::ffi::{CStr, CString};

    // `sandbox_init`/`sandbox_free_error` live in libSystem (auto-linked). The API
    // is deprecated-but-present and stable — it is what Chromium's renderer uses.
    extern "C" {
        fn sandbox_init(
            profile: *const libc::c_char,
            flags: u64,
            errorbuf: *mut *mut libc::c_char,
        ) -> libc::c_int;
        fn sandbox_free_error(errorbuf: *mut libc::c_char);
    }

    let profile = CString::new(SEATBELT_PROFILE).map_err(|e| format!("profile CString: {e}"))?;
    let mut err_ptr: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `profile` is a valid NUL-terminated SBPL string; `flags = 0` selects
    // an inline (non-named) profile. On failure `sandbox_init` allocates an owned C
    // string into `err_ptr`, which we read and then free with `sandbox_free_error`.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err_ptr) };
    if rc == 0 {
        return Ok(());
    }
    let reason = if err_ptr.is_null() {
        format!("sandbox_init returned {rc}")
    } else {
        // SAFETY: `err_ptr` is a valid C string owned by libSystem; copy it out,
        // then hand it back to be freed.
        let msg = unsafe { CStr::from_ptr(err_ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { sandbox_free_error(err_ptr) };
        msg
    };
    Err(reason)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn filter_compiles_for_this_arch() {
        // Building + compiling the filter must succeed on the host arch; we do not
        // *apply* it (that would confine the test process for the rest of its run).
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};
        use std::collections::BTreeMap;

        let arch = match std::env::consts::ARCH {
            "x86_64" => TargetArch::x86_64,
            "aarch64" => TargetArch::aarch64,
            "riscv64" => TargetArch::riscv64,
            other => panic!("unexpected test arch {other}"),
        };
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for sysno in denied_syscalls() {
            rules.insert(sysno, vec![]);
        }
        assert!(!rules.is_empty());
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::KillProcess,
            arch,
        )
        .expect("filter builds");
        let program: BpfProgram = filter.try_into().expect("filter compiles");
        assert!(!program.is_empty());
    }
}
