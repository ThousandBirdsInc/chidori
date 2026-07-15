//! The worker half of OS isolation — the code that runs in the sandboxed child.
//!
//! The child holds *only* the JavaScript engine. It reads an [`FromParent::Init`]
//! handoff, runs the agent through the ordinary [`run_module`] path, and routes
//! every host op (`chidori.*` effect, captured native, DOM flush, module load)
//! back to the parent over the pipe via [`BrokeredHost`]. It never touches the
//! filesystem, the network, or a clock of its own — those live behind the seam.
//!
//! Before running the agent the worker seals itself in: the `setrlimit` floor
//! ([`super::limits`]) then the confinement layers — network namespace,
//! Landlock, and the seccomp denylist ([`super::sandbox`]). The platform's core
//! layer (seccomp / Seatbelt) fails closed by default; the auxiliary layers are
//! best-effort. See `docs/os-isolation-plan.md`.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::rc::Rc;

use serde_json::Value;

use crate::runtime::rust_engine::{run_module, RunHost};

use super::protocol::{read_frame, write_frame, FromChild, FromParent, Outcome};

/// The duplex the worker speaks over: replies/Init arrive on `reader`, calls/Done
/// go out on `writer`. Wrapped in a single cell so the run thread can borrow both
/// for one request/response exchange.
struct WorkerIo<R: Read, W: Write> {
    reader: R,
    writer: W,
}

/// A [`RunHost`] that satisfies every op by a blocking round trip to the parent.
/// Mirrors the engine's existing synchronous dispatch, so to the VM this is
/// indistinguishable from the in-process host.
struct BrokeredHost<R: Read, W: Write> {
    io: Rc<RefCell<WorkerIo<R, W>>>,
    prelude: Option<String>,
}

impl<R: Read, W: Write> RunHost for BrokeredHost<R, W> {
    fn call(&self, op: &str, args: &Value) -> Result<Value, String> {
        let mut io = self.io.borrow_mut();
        let io = &mut *io;
        write_frame(
            &mut io.writer,
            &FromChild::Call {
                op: op.to_string(),
                args: args.clone(),
            },
        )
        .map_err(|e| format!("isolate worker: writing host call `{op}`: {e}"))?;
        let reply: FromParent = read_frame(&mut io.reader)
            .map_err(|e| format!("isolate worker: reading reply for `{op}`: {e}"))?;
        match reply {
            FromParent::Reply(outcome) => outcome.into(),
            FromParent::Init { .. } => {
                Err("isolate worker: unexpected Init while awaiting a reply".to_string())
            }
        }
    }

    fn prelude(&self) -> Option<String> {
        self.prelude.clone()
    }
}

/// Entry point for the hidden `chidori __run-worker` subcommand: drive the
/// protocol over this process's stdin/stdout. stderr is left untouched for
/// diagnostics — nothing but frames may go to stdout or the stream desyncs.
///
/// This is the only caller that applies the `setrlimit` floor, because the
/// limits are applied to the *current process* — sound only when that process is
/// a dedicated worker. The in-process [`serve`] path (used by tests) must never
/// self-limit, or it would mutate the limits of whatever process is hosting it.
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_inner(stdin.lock(), stdout.lock(), true)
}

/// Run one agent to completion over an arbitrary reader/writer pair *without*
/// applying any per-process resource limits. Factored out of [`run`] so tests
/// can drive the worker over an in-process socket without a subprocess — and
/// without the worker's `setrlimit` floor leaking onto the test process.
#[allow(dead_code)] // Exercised only by tests today; the lib target sees it as dead.
pub fn serve<R: Read + 'static, W: Write + 'static>(reader: R, writer: W) -> io::Result<()> {
    serve_inner(reader, writer, false)
}

/// Shared worker body. `apply_limits` gates the per-process `setrlimit` floor —
/// see [`run`] vs [`serve`].
fn serve_inner<R: Read + 'static, W: Write + 'static>(
    reader: R,
    writer: W,
    apply_limits: bool,
) -> io::Result<()> {
    let io = Rc::new(RefCell::new(WorkerIo { reader, writer }));

    let init: FromParent = {
        let mut guard = io.borrow_mut();
        read_frame(&mut guard.reader)?
    };
    let (entry_path, entry_source, fallback_export, input, prelude, limits) = match init {
        FromParent::Init {
            entry_path,
            entry_source,
            fallback_export,
            input,
            prelude,
            limits,
        } => (
            entry_path,
            entry_source,
            fallback_export,
            input,
            prelude,
            limits,
        ),
        FromParent::Reply(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "isolate worker: expected Init as the first frame",
            ));
        }
    };

    // Slam the resource floor shut, then the confinement layers — before any agent
    // code runs, and only in a real worker process (see `serve_inner`'s
    // `apply_limits`; these mutate the *current* process, which is sound only when
    // it is a dedicated worker). The defense-in-depth layers (network namespace,
    // Landlock) are best-effort: one that can't be installed degrades isolation
    // with a logged note. The platform's *core* confinement (seccomp on Linux,
    // Seatbelt on macOS) fails **closed** by default — a run that advertises
    // isolation must not quietly execute with process separation only. The
    // operator can accept that degraded posture explicitly with
    // `CHIDORI_ISOLATE_REQUIRE_SANDBOX=0`, in which case the downgrade is still
    // announced loudly on stderr.
    let mut sandbox = if apply_limits {
        limits.apply_to_self();
        super::sandbox::apply()
    } else {
        let _ = &limits;
        super::sandbox::SandboxOutcome::default()
    };
    // Test-only: pretend the platform's core layer failed to apply, so the
    // fail-closed gate below can be exercised end-to-end on hosts where
    // seccomp/Seatbelt install fine (see `isolate_limits`). The real filter (if
    // any) stays active — only the reported outcome is falsified. Inert unless
    // the env var is set.
    if apply_limits && std::env::var_os("CHIDORI_ISOLATE_TEST_FORCE_UNCONFINED").is_some() {
        sandbox.seccomp_applied = false;
        sandbox.seatbelt_applied = false;
        sandbox.notes.push(
            "core confinement reported as unapplied by CHIDORI_ISOLATE_TEST_FORCE_UNCONFINED \
             (test hook)"
                .to_string(),
        );
    }
    let sandbox = sandbox;
    for note in &sandbox.notes {
        eprintln!("isolate worker: sandbox: {note}");
    }

    // Test-only probes: once the sandbox is in place, attempt an operation a given
    // layer must forbid, to prove it does. Gated behind an env var so it is inert
    // in normal operation. Runs before the fail-closed gate so a probe can report
    // its layer as unavailable (the skip-aware tests depend on that marker).
    #[cfg(unix)]
    if apply_limits {
        if let Some(mode) = std::env::var_os("CHIDORI_ISOLATE_SELFTEST") {
            run_selftest(&mode.to_string_lossy(), &sandbox);
        }
    }

    if apply_limits && !sandbox.core_confined() {
        if sandbox_required() {
            let mut guard = io.borrow_mut();
            let reasons = if sandbox.notes.is_empty() {
                String::new()
            } else {
                format!(": {}", sandbox.notes.join("; "))
            };
            return write_frame(
                &mut guard.writer,
                &FromChild::Done {
                    outcome: Outcome::Err(format!(
                        "the platform's core sandbox confinement (seccomp on Linux, \
                         Seatbelt on macOS) could not be applied, and isolated runs \
                         fail closed without it{reasons}. Set \
                         CHIDORI_ISOLATE_REQUIRE_SANDBOX=0 to explicitly accept a \
                         degraded run with process separation and brokered effects \
                         but no syscall/filesystem/network confinement."
                    )),
                },
            );
        }
        eprintln!(
            "isolate worker: WARNING: running WITHOUT the platform's core sandbox \
             confinement (CHIDORI_ISOLATE_REQUIRE_SANDBOX is disabled or this \
             platform has no sandbox layer): this run has process separation and \
             brokered effects only — no syscall/filesystem/network confinement."
        );
    }

    let host: Rc<dyn RunHost> = Rc::new(BrokeredHost {
        io: io.clone(),
        prelude,
    });
    // `run_module` already contains the opcode-budget guard and a `catch_unwind`
    // boundary, so an interpreter panic comes back here as `Err`, not an unwind.
    let outcome: Outcome = match run_module(
        Path::new(&entry_path),
        &entry_source,
        &fallback_export,
        &input,
        host,
    ) {
        Ok(value) => Outcome::Ok(value),
        Err(e) => Outcome::Err(e.to_string()),
    };

    let mut guard = io.borrow_mut();
    write_frame(&mut guard.writer, &FromChild::Done { outcome })
}

/// Whether a missing core sandbox must fail the run, per
/// `CHIDORI_ISOLATE_REQUIRE_SANDBOX`.
fn sandbox_required() -> bool {
    sandbox_required_from(
        std::env::var("CHIDORI_ISOLATE_REQUIRE_SANDBOX")
            .ok()
            .as_deref(),
    )
}

/// The `CHIDORI_ISOLATE_REQUIRE_SANDBOX` policy, factored over the raw env value
/// so it is unit-testable. Unset (or empty) means **fail closed by default** on
/// platforms that implement a core layer (seccomp on Linux, Seatbelt on macOS);
/// on platforms with no sandbox implementation at all the startup banner already
/// announces "no OS sandbox layer", so the default there is the loud downgrade
/// rather than an unconditional refusal. An explicit falsy value
/// (`0`/`off`/`false`/`no`) opts into degraded runs anywhere; any other explicit
/// value demands confinement even on platforms without a sandbox layer.
fn sandbox_required_from(value: Option<&str>) -> bool {
    let platform_default = cfg!(any(target_os = "linux", target_os = "macos"));
    match value {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            if v.is_empty() {
                platform_default
            } else {
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
        }
        None => platform_default,
    }
}

/// Dispatch a sandbox self-test probe (driven by `isolate_limits` integration
/// tests via `CHIDORI_ISOLATE_SELFTEST`). Each probe attempts an operation the
/// named layer must forbid and reports the result on stderr; if the relevant
/// layer wasn't applied in this environment it prints an `*-unavailable` marker
/// so the test can *skip* rather than fail. Always terminates the process.
#[cfg(unix)]
fn run_selftest(mode: &str, sandbox: &crate::runtime::isolate::sandbox::SandboxOutcome) -> ! {
    match mode {
        // seccomp: `socket()` must raise SIGSYS and kill us before it returns.
        "socket" => {
            if !sandbox.seccomp_applied {
                eprintln!("isolate-selftest: seccomp-unavailable");
                std::process::exit(0);
            }
            // SAFETY: `socket` takes scalar args. With the filter active the call
            // never returns (the kernel raises SIGSYS); if it does, the filter
            // failed to block it — a real test failure.
            let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            eprintln!("isolate-selftest: socket-not-blocked (fd={fd})");
            std::process::exit(97);
        }
        // Filesystem-write confinement: creating a file must be denied — by
        // Landlock's read-only policy on Linux, or the Seatbelt `(deny
        // file-write*)` rule on macOS. Distinct from RLIMIT_FSIZE, which blocks
        // the *write*, not the *open*. This is the cross-platform proof that the
        // OS sandbox actually loaded and is enforcing.
        "fs-write" => {
            if !(sandbox.landlock_enforced || sandbox.seatbelt_applied) {
                eprintln!("isolate-selftest: fs-write-confinement-unavailable");
                std::process::exit(0);
            }
            let path = c"/tmp/chidori-fs-write-selftest";
            // SAFETY: `open` takes a valid NUL-terminated path and scalar flags.
            let fd = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
                    0o600,
                )
            };
            if fd >= 0 {
                // SAFETY: closing an fd we just opened.
                unsafe { libc::close(fd) };
                eprintln!("isolate-selftest: fs-write-not-blocked");
                std::process::exit(96);
            }
            let err = std::io::Error::last_os_error();
            eprintln!("isolate-selftest: fs-write-blocked ({err})");
            std::process::exit(0);
        }
        other => {
            eprintln!("isolate-selftest: unknown mode `{other}`");
            std::process::exit(95);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sandbox_required_from;

    #[test]
    fn sandbox_is_required_by_default_where_a_core_layer_exists() {
        let platform_has_core = cfg!(any(target_os = "linux", target_os = "macos"));
        assert_eq!(sandbox_required_from(None), platform_has_core);
        assert_eq!(sandbox_required_from(Some("")), platform_has_core);
        assert_eq!(sandbox_required_from(Some("  ")), platform_has_core);
    }

    #[test]
    fn explicit_falsy_value_opts_into_degraded_runs() {
        for v in ["0", "off", "false", "no", " OFF ", "False"] {
            assert!(!sandbox_required_from(Some(v)), "value {v:?}");
        }
    }

    #[test]
    fn explicit_truthy_value_always_requires_the_sandbox() {
        for v in ["1", "on", "true", "yes", "require"] {
            assert!(sandbox_required_from(Some(v)), "value {v:?}");
        }
    }
}
