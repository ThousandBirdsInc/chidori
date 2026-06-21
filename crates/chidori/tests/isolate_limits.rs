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
fn landlock_blocks_file_creation() {
    // Probe a file create once the sandbox is in place. Under the Landlock
    // read-only policy the `open(O_CREAT)` is denied with EACCES; if Landlock
    // isn't enforced in this environment (older kernel, or the LSM isn't in the
    // active set) the worker says so and we skip rather than fail.
    let agent = write_agent(
        "landlock",
        r#"
        import { run } from "chidori:agent";
        run(async () => ({}));
        "#,
    );
    let out = run_isolated(&agent, &[("CHIDORI_ISOLATE_SELFTEST", "fs-write")]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if stderr.contains("landlock-unavailable") {
        eprintln!("skipping landlock test: not enforced in this environment");
        let _ = fs::remove_dir_all(agent.parent().unwrap());
        return;
    }
    assert!(
        !stderr.contains("fs-write-not-blocked"),
        "file creation was NOT blocked by Landlock; stderr={stderr}"
    );
    assert!(
        stderr.contains("fs-write-blocked"),
        "expected the Landlock-blocked marker; stderr={stderr}"
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
