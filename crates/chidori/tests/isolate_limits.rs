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

/// With the warm pool enabled, a one-shot `chidori run` behaves exactly as
/// without it: the root run is correct, and the workers the pool prewarmed in
/// the background (which never get a run — the process exits first) shut down
/// silently on EOF instead of reporting protocol errors.
#[test]
fn warm_pool_enabled_run_is_correct_and_parked_workers_exit_quietly() {
    let agent = write_agent(
        "pool-quiet",
        r#"
        import { run } from "chidori:agent";
        import { bump } from "./helper.ts";
        run(async (input: { n: number }) => ({ out: bump(input.n ?? 1) }));
        "#,
    );
    fs::write(
        agent.parent().unwrap().join("helper.ts"),
        "export function bump(n: number): number { return n + 1; }\n",
    )
    .unwrap();
    let out = run_isolated(
        &agent,
        &[
            ("CHIDORI_ISOLATE_WARM_POOL", "2"),
            ("CHIDORI_ISOLATE_VERBOSE", "1"),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("\"out\""),
        "stdout missing result: {stdout}"
    );
    // The parked prewarmed workers see EOF at process exit — that is their
    // normal end-of-life, not an error to report.
    assert!(
        !stderr.contains("isolate worker error"),
        "parked workers must exit quietly at parent exit; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}

/// Minimal raw-HTTP client for the serve E2E below (no HTTP dev-dependency).
fn http_request(port: u16, request: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    // No write-shutdown: hyper aborts on a half-closed connection. The
    // `Connection: close` header makes the server close after the response,
    // which ends the read.
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

/// End-to-end warm-pool proof through `chidori serve` (the pool's audience —
/// one long-lived parent process serving many root runs): the first session
/// cold-spawns its worker and seeds the pool; a later session draws a
/// prewarmed one.
#[test]
fn warm_pool_serves_a_later_server_run_prewarmed() {
    let agent = write_agent(
        "serve",
        r#"
        import { run } from "chidori:agent";
        import { bump } from "./helper.ts";
        run(async (input: { n: number }) => ({ out: bump(input.n) }));
        "#,
    );
    fs::write(
        agent.parent().unwrap().join("helper.ts"),
        "export function bump(n: number): number { return n + 1; }\n",
    )
    .unwrap();

    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut server = Command::new(chidori_bin())
        .arg("serve")
        .arg(&agent)
        .arg("--port")
        .arg(port.to_string())
        .arg("--isolate")
        .env("CHIDORI_ISOLATE_WARM_POOL", "2")
        .env("CHIDORI_ISOLATE_VERBOSE", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Everything below must kill the server even on panic, so run it in a
    // closure and reap afterwards.
    let result = std::panic::catch_unwind(|| {
        // Readiness: /health answers once the server is up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("server did not come up on port {port}");
            }
            let health = std::panic::catch_unwind(|| {
                http_request(
                    port,
                    &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
                )
            });
            if health.is_ok_and(|h| h.starts_with("HTTP/1.1 200")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let post_session = |n: u32| {
            let body = format!("{{\"input\":{{\"n\":{n}}}}}");
            http_request(
                port,
                &format!(
                    "POST /sessions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
            )
        };

        // First run: cold worker, seeds the pool.
        let first = post_session(5);
        assert!(
            first.contains("\"out\":6") || first.contains("\"out\": 6"),
            "first session response: {first}"
        );
        // Give the background prewarm ample time (measured ~70 ms in debug).
        std::thread::sleep(std::time::Duration::from_secs(2));
        // Second run: must draw a prewarmed worker.
        let second = post_session(7);
        assert!(
            second.contains("\"out\":8") || second.contains("\"out\": 8"),
            "second session response: {second}"
        );
    });

    let _ = server.kill();
    let output = server.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Err(panic) = result {
        panic!(
            "serve E2E failed: {}; server stderr={stderr}",
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        );
    }
    assert!(
        stderr.contains("prewarmed worker serving"),
        "the second run should have drawn a prewarmed worker; stderr={stderr}"
    );
    let _ = fs::remove_dir_all(agent.parent().unwrap());
}
