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
    /// The worker runs in its own (empty) network namespace.
    pub network_isolated: bool,
    /// Landlock is enforcing a read-only view of the filesystem.
    pub landlock_enforced: bool,
    /// The seccomp denylist is installed.
    pub seccomp_applied: bool,
    /// Human-readable reasons for any layer that was skipped.
    pub notes: Vec<String>,
}

/// Apply every confinement layer to the current process, in the order that keeps
/// each one legal: namespace + Landlock first (they need syscalls — `unshare`,
/// `landlock_*` — that seccomp then denies), and the seccomp denylist last. Every
/// layer is best-effort; the returned [`SandboxOutcome`] records what stuck.
///
/// Sound only when the caller *is* the dedicated worker process, since each layer
/// mutates the current process irreversibly.
pub fn apply() -> SandboxOutcome {
    let mut outcome = SandboxOutcome::default();

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
    outcome
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

#[cfg(not(target_os = "linux"))]
fn apply_network_namespace() -> Result<(), String> {
    Err("network namespaces are only available on Linux".to_string())
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

#[cfg(not(target_os = "linux"))]
fn apply_landlock_readonly() -> Result<bool, String> {
    Err("landlock is only available on Linux".to_string())
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

#[cfg(not(target_os = "linux"))]
pub fn install_seccomp() -> Result<(), String> {
    Err("seccomp confinement is only available on Linux".to_string())
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
