//! End-to-end tests for the OS-isolation resource floor (phase 2), driven
//! through the real `chidori` binary so the actual worker subprocess, its
//! `setrlimit` floor, and the parent's deadline-kill / signal mapping are all
//! exercised. Unix-only: the limits and the kill path are Unix primitives.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn chidori_bin() -> &'static str {
    env!("CARGO_BIN_EXE_chidori")
}

fn write_agent(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chidori-isolate-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.ts");
    fs::write(&path, src).unwrap();
    path
}

/// Run `chidori run <agent> --isolate` with extra env, returning the output.
fn run_isolated(agent: &PathBuf, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(chidori_bin());
    cmd.arg("run").arg(agent).arg("--isolate");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn isolated_run_succeeds_under_the_default_resource_floor() {
    // The default rlimits (no-core, fsize=0, nofile=256) must not break a normal
    // run — they only close doors the agent never uses.
    let agent = write_agent(
        "ok",
        r#"
        import { chidori, run } from "chidori:agent";
        run(async () => {
            await chidori.log("isolated and limited");
            return { ok: true };
        });
        "#,
    );
    let out = run_isolated(&agent, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success; stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("\"ok\""), "stdout missing result: {stdout}");
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

#[test]
fn parent_deadline_kills_a_wedged_worker() {
    // A busy loop with the in-engine opcode budget disabled never self-terminates,
    // so only the parent's wall-clock deadline can reclaim it. (No CPU limit set,
    // so the deadline — not RLIMIT_CPU — is unambiguously the cause.)
    let agent = write_agent(
        "deadline",
        r#"
        import { run } from "chidori:agent";
        run(async () => { while (true) {} });
        "#,
    );
    let out = run_isolated(
        &agent,
        &[
            ("CHIDORI_JS_OP_BUDGET", "0"),   // disable the in-engine compute bound
            ("CHIDORI_JS_DEADLINE_MS", "0"), // disable the in-engine deadline
            ("CHIDORI_ISOLATE_DEADLINE_MS", "500"), // parent hard backstop
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "wedged worker should fail the run");
    assert!(
        stderr.contains("deadline"),
        "error should name the deadline; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

#[test]
fn seccomp_blocks_a_denied_syscall() {
    // A normal agent, but the worker is told to probe `socket()` once the seccomp
    // filter is installed. With the filter active that syscall raises SIGSYS and
    // kills the worker, which the parent maps to a seccomp error. If seccomp can't
    // be installed in this environment, the worker says so and we skip rather than
    // report a false failure.
    let agent = write_agent(
        "seccomp",
        r#"
        import { chidori, run } from "chidori:agent";
        run(async () => { await chidori.log("unreachable: killed before running"); return {}; });
        "#,
    );
    let out = run_isolated(&agent, &[("CHIDORI_ISOLATE_SELFTEST", "socket")]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if stderr.contains("seccomp-unavailable") {
        eprintln!("skipping seccomp test: seccomp could not be applied in this environment");
        let _ = fs::remove_dir_all(agent.parent().unwrap());
        return;
    }
    assert!(
        !stderr.contains("socket-not-blocked"),
        "socket() was NOT blocked by the seccomp filter; stderr={stderr}"
    );
    assert!(
        !out.status.success(),
        "worker probing a denied syscall should fail the run; stderr={stderr}"
    );
    assert!(
        stderr.contains("seccomp"),
        "error should name the seccomp violation; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

#[test]
fn filesystem_writes_are_blocked_when_confined() {
    // Probe a file create once the sandbox is in place. The OS filesystem-write
    // confinement — Landlock read-only on Linux, the Seatbelt `(deny file-write*)`
    // rule on macOS — must deny the `open(O_CREAT)`. This is the cross-platform
    // proof the sandbox actually loaded and enforces; in particular it is how the
    // macOS Seatbelt path is verified at runtime in CI. If no such layer is
    // active in this environment (older Linux kernel without Landlock, etc.) the
    // worker says so and we skip rather than fail.
    let agent = write_agent(
        "fs-write",
        r#"
        import { run } from "chidori:agent";
        run(async () => ({}));
        "#,
    );
    let out = run_isolated(&agent, &[("CHIDORI_ISOLATE_SELFTEST", "fs-write")]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if stderr.contains("fs-write-confinement-unavailable") {
        eprintln!("skipping fs-write test: no filesystem-write confinement in this environment");
        let _ = fs::remove_dir_all(agent.parent().unwrap());
        return;
    }
    assert!(
        !stderr.contains("fs-write-not-blocked"),
        "file creation was NOT blocked by the OS sandbox; stderr={stderr}"
    );
    assert!(
        stderr.contains("fs-write-blocked"),
        "expected the sandbox-blocked marker; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

/// macOS-only: the runtime verification of the phase-4 Seatbelt FFI that a Linux
/// dev/CI host cannot perform. Unlike the skip-aware test above, this one *fails*
/// (not skips) if Seatbelt does not load and enforce, so the macOS CI job catches
/// a regression in `sandbox_init` / the SBPL profile. It asserts: the worker did
/// not report the profile as unapplied, the normal isolated run still works
/// (i.e. `(deny file-write*)` did not also wedge the broker pipe), and a file
/// create is denied.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_loads_and_enforces_on_macos() {
    let agent = write_agent(
        "seatbelt",
        r#"
        import { chidori, run } from "chidori:agent";
        run(async (input: { value: number }) => {
            await chidori.log("seatbelt smoke");
            return { value: (input?.value ?? 0) + 1 };
        });
        "#,
    );

    // 1) A normal isolated run must still succeed — proves the Seatbelt profile
    //    didn't also block the worker's stdout broker pipe.
    let ok = run_isolated(&agent, &[]);
    let ok_err = String::from_utf8_lossy(&ok.stderr);
    assert!(
        ok.status.success(),
        "isolated run failed under Seatbelt; stderr={ok_err}"
    );
    assert!(
        !ok_err.contains("seatbelt not applied"),
        "Seatbelt profile failed to load; stderr={ok_err}"
    );

    // 2) The fs-write probe must be blocked (Seatbelt is actually enforcing).
    let probe = run_isolated(&agent, &[("CHIDORI_ISOLATE_SELFTEST", "fs-write")]);
    let probe_err = String::from_utf8_lossy(&probe.stderr);
    assert!(
        !probe_err.contains("fs-write-confinement-unavailable"),
        "Seatbelt reported no filesystem-write confinement; stderr={probe_err}"
    );
    assert!(
        probe_err.contains("fs-write-blocked"),
        "Seatbelt did not block a file create; stderr={probe_err}"
    );

    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

#[test]
fn missing_core_sandbox_fails_closed_by_default() {
    // If the platform's core confinement (seccomp on Linux, Seatbelt on macOS)
    // cannot be applied, an isolated run must refuse — not quietly execute with
    // process separation only. The test hook makes the worker report the core
    // layer as unapplied so the gate is exercised on hosts where the sandbox
    // genuinely installs.
    let agent = write_agent(
        "fail-closed",
        r#"
        import { run } from "chidori:agent";
        run(async () => ({ ok: true }));
        "#,
    );
    let out = run_isolated(&agent, &[("CHIDORI_ISOLATE_TEST_FORCE_UNCONFINED", "1")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "run without core confinement should fail closed by default; stderr={stderr}"
    );
    assert!(
        stderr.contains("fail closed") || stderr.contains("could not be applied"),
        "error should explain the fail-closed refusal; stderr={stderr}"
    );
    assert!(
        stderr.contains("CHIDORI_ISOLATE_REQUIRE_SANDBOX"),
        "error should name the explicit opt-out; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

#[test]
fn degraded_run_needs_an_explicit_opt_out_and_stays_loud() {
    // With CHIDORI_ISOLATE_REQUIRE_SANDBOX=0 the operator accepts a degraded
    // run — it must succeed, but the downgrade must be announced on stderr.
    let agent = write_agent(
        "degraded",
        r#"
        import { run } from "chidori:agent";
        run(async () => ({ ok: true }));
        "#,
    );
    let out = run_isolated(
        &agent,
        &[
            ("CHIDORI_ISOLATE_TEST_FORCE_UNCONFINED", "1"),
            ("CHIDORI_ISOLATE_REQUIRE_SANDBOX", "0"),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "opted-out degraded run should succeed; stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("\"ok\""), "stdout missing result: {stdout}");
    assert!(
        stderr.contains("WARNING") && stderr.contains("WITHOUT"),
        "degraded run must announce the downgrade loudly; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

#[test]
fn cpu_limit_terminates_a_busy_worker() {
    // With compute bounds disabled, a busy loop burns CPU until RLIMIT_CPU fires
    // (SIGXCPU), which the parent maps to a CPU-time error. No deadline set, so
    // the CPU limit is the sole cause.
    let agent = write_agent(
        "cpu",
        r#"
        import { run } from "chidori:agent";
        run(async () => { while (true) {} });
        "#,
    );
    let out = run_isolated(
        &agent,
        &[
            ("CHIDORI_JS_OP_BUDGET", "0"),
            ("CHIDORI_JS_DEADLINE_MS", "0"),
            ("CHIDORI_ISOLATE_CPU_SECS", "1"),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "CPU-bound worker should fail the run"
    );
    assert!(
        stderr.contains("CPU"),
        "error should name the CPU limit; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}
