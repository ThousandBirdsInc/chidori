//! The supervisor half of OS isolation — the parent-side broker.
//!
//! [`run_agent_isolated`] spawns the worker (`chidori __run-worker`), ships it
//! the [`FromParent::Init`] handoff, then services every host op the child sends
//! by routing it through the *same* [`route_host_op`] the in-process host uses.
//! The durable call log, policy, MCP, providers, and OTEL all stay here in the
//! trusted parent; the child only computes JavaScript.
//!
//! Phase 2 adds the parent-side hard backstops that the child cannot evade: a
//! wall-clock **deadline-kill** (a watchdog thread that `SIGKILL`s a wedged
//! child) and **signal-aware failure mapping** so an OS kill — CPU limit, file
//! limit, OOM, deadline — surfaces as a precise error instead of an opaque
//! "worker terminated". The per-process `setrlimit` floor is applied by the
//! child itself (see [`super::limits`]).

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::runtime::rust_engine::{build_sync_native_dispatch, route_host_op, rust_engine_prelude};
use crate::runtime::typescript::bindings::HostBindingBackend;

use super::limits::ResourceLimits;
use super::protocol::{read_frame, write_frame, FromChild, FromParent, Outcome};

/// Optional parent-side wall-clock deadline, in milliseconds, from
/// `CHIDORI_ISOLATE_DEADLINE_MS`. Distinct from the in-engine
/// `CHIDORI_JS_DEADLINE_MS` (which the child enforces cooperatively): this is the
/// hard backstop that reclaims a child which has stopped cooperating entirely.
/// Off (`None`) unless set to a positive value.
fn deadline_from_env() -> Option<Duration> {
    std::env::var("CHIDORI_ISOLATE_DEADLINE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
}

/// Run `source` (the agent at `path`) in a sandboxed child process, brokering its
/// host effects back through `backend`. Returns the agent's output, or an error
/// if the worker failed, crashed, hit a resource limit, or blew the deadline.
pub(crate) fn run_agent_isolated(
    path: &Path,
    source: &str,
    input: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    let exe = std::env::current_exe().context("locating the chidori worker binary")?;
    // Sandbox degradation notes (e.g. "landlock not enforced") are a real
    // security signal, but each run spawns a fresh worker — unthrottled they
    // repeat on every run of a long-lived server. Let the first worker of this
    // parent process print them; later workers are told they've been said.
    static SANDBOX_NOTES_RELAYED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let notes_already_relayed =
        SANDBOX_NOTES_RELAYED.swap(true, std::sync::atomic::Ordering::Relaxed);
    let mut child = Command::new(&exe)
        .arg("__run-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // The child must not re-enter isolation (it runs the agent directly); make
        // that impossible regardless of how this process's env was configured.
        // Explicitly `off` (not unset) so nothing downstream can re-apply a
        // default-on posture to the worker or its descendants.
        .env("CHIDORI_ISOLATE", "off")
        .env(
            "CHIDORI_ISOLATE_SANDBOX_NOTES_QUIET",
            if notes_already_relayed { "1" } else { "0" },
        )
        .spawn()
        .with_context(|| format!("spawning isolate worker `{} __run-worker`", exe.display()))?;

    let mut to_child = child.stdin.take().expect("worker stdin was piped");
    let mut from_child = child.stdout.take().expect("worker stdout was piped");

    let init = FromParent::Init {
        entry_path: path.to_string_lossy().into_owned(),
        entry_source: source.to_string(),
        fallback_export: "agent".to_string(),
        input: input.clone(),
        prelude: backend.runtime_policy().map(|p| rust_engine_prelude(&p)),
        limits: ResourceLimits::from_env(),
    };

    // Arm the deadline watchdog (if configured) before brokering: it SIGKILLs the
    // child if the run outlasts the deadline, which unblocks the broker's read.
    let deadline = deadline_from_env();
    let watchdog = deadline.map(|d| DeadlineWatchdog::arm(child.id(), d));

    let result = broker(&mut from_child, &mut to_child, backend, init);

    // Disarm the watchdog (a no-op if it already fired), then drop our pipe ends
    // so the worker sees EOF, and reap the child to avoid a zombie. The `Done`
    // frame is authoritative for the outcome; the exit status only *enriches* an
    // error with the OS-level cause.
    let killed_by_deadline = watchdog.map(|w| w.disarm()).unwrap_or(false);
    drop(to_child);
    drop(from_child);
    let status = child.wait();

    match result {
        Ok(value) => Ok(value),
        Err(e) => {
            if killed_by_deadline {
                let ms = deadline.map(|d| d.as_millis()).unwrap_or(0);
                return Err(e.context(format!(
                    "isolate worker exceeded the {ms} ms wall-clock deadline and was killed"
                )));
            }
            match status {
                Ok(s) if !s.success() => match exit_cause(&s) {
                    Some(cause) => Err(e.context(cause)),
                    None => Err(e.context(format!("isolate worker exited with status {s}"))),
                },
                _ => Err(e),
            }
        }
    }
}

/// The broker loop: send `init`, then service `Call` frames until the worker
/// reports `Done`. Generic over the transport so tests can drive it over an
/// in-process socket pair. `pub(crate)` for that reason.
pub(crate) fn broker<R: Read, W: Write>(
    from_child: &mut R,
    to_child: &mut W,
    backend: &HostBindingBackend,
    init: FromParent,
) -> Result<Value> {
    // The captured-effect native dispatch (VFS / crypto / timers), built once and
    // shared across every brokered op — identical to what the in-process host
    // constructs, so brokered and inline runs hit the same handlers.
    let sync = match (backend.runtime_policy(), backend.runtime_ctx()) {
        (Some(policy), Some(ctx)) => Some(build_sync_native_dispatch(ctx.clone(), policy)),
        _ => None,
    };

    write_frame(to_child, &init).context("sending Init to the isolate worker")?;

    loop {
        let msg: FromChild = read_frame(from_child)
            .context("isolate worker terminated before returning a result")?;
        match msg {
            FromChild::Call { op, args } => {
                let outcome: Outcome = route_host_op(backend, sync.as_ref(), &op, &args).into();
                write_frame(to_child, &FromParent::Reply(outcome))
                    .context("replying to the isolate worker")?;
            }
            FromChild::Done { outcome } => {
                return Result::<Value, String>::from(outcome).map_err(|e| anyhow!(e));
            }
        }
    }
}

/// A background thread that `SIGKILL`s the worker if the run outlasts the
/// deadline. [`disarm`](DeadlineWatchdog::disarm) returns whether it fired, and
/// blocks until the thread has exited so no kill can land after the call returns.
struct DeadlineWatchdog {
    stop: mpsc::Sender<()>,
    fired: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl DeadlineWatchdog {
    /// Start watching `pid`. On Unix the kill is a `SIGKILL` by pid (the child is
    /// not yet reaped, so the pid is unambiguous); on other platforms the
    /// watchdog degrades to a no-op (Windows isolation is a later phase).
    fn arm(pid: u32, deadline: Duration) -> Self {
        let (stop, rx) = mpsc::channel::<()>();
        let fired = Arc::new(AtomicBool::new(false));
        let fired_thread = fired.clone();
        let handle = std::thread::spawn(move || {
            // Wake either when the run completes (a send/disconnect) or when the
            // deadline elapses (timeout) — only the timeout triggers a kill.
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(deadline) {
                fired_thread.store(true, Ordering::Release);
                kill_pid(pid);
            }
        });
        DeadlineWatchdog {
            stop,
            fired,
            handle,
        }
    }

    /// Stop the watchdog and report whether it fired. Joins the thread, so once
    /// this returns the watchdog can no longer issue a kill.
    fn disarm(self) -> bool {
        let _ = self.stop.send(());
        let _ = self.handle.join();
        self.fired.load(Ordering::Acquire)
    }
}

/// Send `SIGKILL` to `pid`. A failure (e.g. the child already exited) is ignored.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    // SAFETY: `kill` takes scalar arguments and has no memory-safety contract;
    // targeting an already-exited pid simply returns `ESRCH`, which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_pid(_pid: u32) {}

/// Describe why a non-success worker exit happened, when the OS tells us via a
/// terminating signal. `None` for a plain nonzero exit (the caller adds the
/// generic status context).
#[cfg(unix)]
fn exit_cause(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    let sig = status.signal()?;
    Some(match sig {
        libc::SIGSYS => {
            "isolate worker attempted a blocked syscall and was killed (seccomp/SIGSYS)".to_string()
        }
        libc::SIGKILL => {
            "isolate worker was killed (out of memory, or an external SIGKILL)".to_string()
        }
        libc::SIGXCPU => "isolate worker exceeded its CPU-time limit (RLIMIT_CPU)".to_string(),
        libc::SIGXFSZ => "isolate worker exceeded its file-size limit (RLIMIT_FSIZE)".to_string(),
        libc::SIGSEGV => "isolate worker crashed (SIGSEGV)".to_string(),
        other => format!("isolate worker was terminated by signal {other}"),
    })
}

#[cfg(not(unix))]
fn exit_cause(_status: &std::process::ExitStatus) -> Option<String> {
    None
}
