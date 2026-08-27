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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
///
/// With the warm pool enabled (`CHIDORI_ISOLATE_WARM_POOL` > 0), the child may
/// have been spawned — and its module graph compiled — *before* this run
/// arrived (see [`WarmPool`]); it still serves exactly one run and exits.
pub(crate) fn run_agent_isolated(
    path: &Path,
    source: &str,
    input: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    let entry_key = path.to_string_lossy().into_owned();
    let init = FromParent::Init {
        entry_path: entry_key.clone(),
        entry_source: source.to_string(),
        fallback_export: "agent".to_string(),
        input: input.clone(),
        prelude: backend.runtime_policy().map(|p| rust_engine_prelude(&p)),
        limits: ResourceLimits::from_env(),
    };

    if warm_pool_target() > 0 {
        let taken = WarmPool::global().take(&entry_key);
        // Replenish regardless of hit or miss, so the NEXT run of this entry
        // finds a warm worker. (On a miss this is also what seeds the pool:
        // the first run is cold, the second is warm.)
        WarmPool::global().replenish(
            entry_key.clone(),
            source.to_string(),
            backend.runtime_policy().map(|p| rust_engine_prelude(&p)),
        );
        if let Some(worker) = taken {
            match run_on_worker(worker, backend, &init) {
                WarmRunOutcome::Finished(result) => return result,
                // The pooled worker was dead before the Init was delivered:
                // nothing ran, nothing was journaled — fall through to a
                // fresh cold spawn exactly as if the pool had missed.
                WarmRunOutcome::InitUndelivered => {}
            }
        }
    }

    let mut worker = spawn_worker()?;
    // Arm the deadline watchdog (if configured) before brokering: it SIGKILLs the
    // child if the run outlasts the deadline, which unblocks the broker's read.
    let deadline = deadline_from_env();
    let watchdog = deadline.map(|d| DeadlineWatchdog::arm(worker.child.id(), d));
    let result = broker(&mut worker.from_child, &mut worker.to_child, backend, init);
    finish_reap(worker, result, watchdog, deadline)
}

/// How a run handed to a pooled worker ended.
enum WarmRunOutcome {
    /// The `Init` reached the worker; the result — success or failure — is
    /// authoritative (effects may have been journaled, so no retry).
    Finished(Result<Value>),
    /// The pooled worker was already dead when we tried to hand it the run: no
    /// frame was delivered, so a cold spawn can safely take over.
    InitUndelivered,
}

/// Hand `init` to a prewarmed worker. Only an undelivered `Init` is retryable —
/// once the frame is written, the run may have started journaling effects and
/// the outcome (whatever it is) is final.
fn run_on_worker(
    mut worker: SpawnedWorker,
    backend: &HostBindingBackend,
    init: &FromParent,
) -> WarmRunOutcome {
    // A worker that already exited in the pool (OOM-killed, operator SIGKILL,
    // a prewarm crash after parking) is silently discarded.
    if matches!(worker.child.try_wait(), Ok(Some(_)) | Err(_)) {
        worker.discard();
        return WarmRunOutcome::InitUndelivered;
    }
    if write_frame(&mut worker.to_child, init).is_err() {
        // EPIPE on the very first frame: the worker died before reading
        // anything, so nothing ran.
        worker.discard();
        return WarmRunOutcome::InitUndelivered;
    }
    if env_verbose() {
        eprintln!(
            "isolate: prewarmed worker serving this run (spawned {} ms ago)",
            worker.spawned_at.elapsed().as_millis()
        );
    }
    let deadline = deadline_from_env();
    let watchdog = deadline.map(|d| DeadlineWatchdog::arm(worker.child.id(), d));
    let result = broker_after_init(&mut worker.from_child, &mut worker.to_child, backend);
    WarmRunOutcome::Finished(finish_reap(worker, result, watchdog, deadline))
}

/// A spawned `__run-worker` child with its pipe ends, shared by the cold path
/// and the warm pool.
struct SpawnedWorker {
    child: Child,
    to_child: ChildStdin,
    from_child: ChildStdout,
    spawned_at: Instant,
}

impl SpawnedWorker {
    /// Kill and reap a worker that will never serve a run (pool eviction, a
    /// dead-on-acquire worker). Best-effort; also removes its per-run cgroup.
    fn discard(mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        super::limits::cleanup_worker_cgroup(pid);
    }
}

/// Spawn a fresh `chidori __run-worker` child with piped stdin/stdout.
fn spawn_worker() -> Result<SpawnedWorker> {
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
    let to_child = child.stdin.take().expect("worker stdin was piped");
    let from_child = child.stdout.take().expect("worker stdout was piped");
    Ok(SpawnedWorker {
        child,
        to_child,
        from_child,
        spawned_at: Instant::now(),
    })
}

/// Shared post-broker teardown: disarm the watchdog, close the pipes, reap the
/// child, clean up its cgroup, and enrich an error with the OS-level cause.
fn finish_reap(
    worker: SpawnedWorker,
    result: Result<Value>,
    watchdog: Option<DeadlineWatchdog>,
    deadline: Option<Duration>,
) -> Result<Value> {
    // Disarm the watchdog (a no-op if it already fired), then drop our pipe ends
    // so the worker sees EOF, and reap the child to avoid a zombie. The `Done`
    // frame is authoritative for the outcome; the exit status only *enriches* an
    // error with the OS-level cause.
    let killed_by_deadline = watchdog.map(|w| w.disarm()).unwrap_or(false);
    let SpawnedWorker {
        mut child,
        to_child,
        from_child,
        ..
    } = worker;
    drop(to_child);
    drop(from_child);
    let worker_pid = child.id();
    let status = child.wait();
    // The reaped worker's per-run cgroup (memory.max ceiling) is empty now —
    // remove it so a long-lived server doesn't accrete one dir per run.
    super::limits::cleanup_worker_cgroup(worker_pid);

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
    broker_loop(from_child, to_child, backend, sync)
}

/// [`broker`] for a worker whose `Init` frame is already on the wire: builds
/// the captured-native dispatch and services calls until `Done`. Used by the
/// warm-pool run path and by tests driving a prewarmed worker directly.
pub(crate) fn broker_after_init<R: Read, W: Write>(
    from_child: &mut R,
    to_child: &mut W,
    backend: &HostBindingBackend,
) -> Result<Value> {
    let sync = match (backend.runtime_policy(), backend.runtime_ctx()) {
        (Some(policy), Some(ctx)) => Some(build_sync_native_dispatch(ctx.clone(), policy)),
        _ => None,
    };
    broker_loop(from_child, to_child, backend, sync)
}

/// The service half of [`broker`], after the `Init` frame is on the wire — the
/// warm-pool path writes its `Init` separately (a failed write there means the
/// pooled worker died and a cold spawn can retry safely).
fn broker_loop<R: Read, W: Write>(
    from_child: &mut R,
    to_child: &mut W,
    backend: &HostBindingBackend,
    sync: Option<Rc<dyn Fn(&str, &Value) -> std::result::Result<Value, String>>>,
) -> Result<Value> {
    loop {
        let msg: FromChild = read_frame(from_child)
            .context("isolate worker terminated before returning a result")?;
        match msg {
            FromChild::Call { op, args } => {
                let outcome: Outcome = route_host_op(backend, sync.as_ref(), &op, &args).into();
                write_frame(to_child, &FromParent::Reply(outcome))
                    .context("replying to the isolate worker")?;
            }
            // A stray prewarm ack (a pool worker whose `Warmed` was never
            // consumed cannot reach a run — the filler always consumes it —
            // but tolerate it rather than desync).
            FromChild::Warmed { .. } => continue,
            FromChild::Done { outcome } => {
                return Result::<Value, String>::from(outcome).map_err(|e| anyhow!(e));
            }
        }
    }
}

// =============================================================================
// Warm worker pool
// =============================================================================

/// Target number of prewarmed workers to keep parked per agent entry, from
/// `CHIDORI_ISOLATE_WARM_POOL`. 0 (the default) disables the pool entirely:
/// every run cold-spawns its worker, the historical behavior.
pub(crate) fn warm_pool_target() -> usize {
    std::env::var("CHIDORI_ISOLATE_WARM_POOL")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

/// How long a parked worker stays eligible before it is discarded, from
/// `CHIDORI_ISOLATE_WARM_TTL_MS` (default 15 minutes). A TTL bounds the cost of
/// a pool warmed for an entry that stops receiving traffic; staleness of the
/// *code* needs no TTL at all — the compile caches are keyed by full source,
/// so an edited module simply misses and recompiles.
fn warm_pool_ttl() -> Duration {
    std::env::var("CHIDORI_ISOLATE_WARM_TTL_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(15 * 60))
}

/// Hard ceiling on a single prewarm (spawn → `Warmed`): a wedge here would
/// otherwise park a filler thread forever. Generous — a prewarm is a compile,
/// not a run.
const PREWARM_DEADLINE: Duration = Duration::from_secs(60);

/// At most this many distinct entries keep warm workers at once; the
/// least-recently-used entry is evicted past the cap. Bounds a multi-tenant
/// fleet's idle-worker memory to `keys × CHIDORI_ISOLATE_WARM_POOL` processes.
const WARM_POOL_MAX_KEYS: usize = 8;

fn env_verbose() -> bool {
    let truthy = |key: &str| {
        std::env::var(key).is_ok_and(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && !matches!(v.as_str(), "0" | "off" | "false" | "no")
        })
    };
    truthy("CHIDORI_VERBOSE") || truthy("CHIDORI_ISOLATE_VERBOSE")
}

/// Per-entry pool state. `filling` counts prewarm threads in flight so a burst
/// of runs doesn't over-spawn past the target.
struct PoolEntry {
    workers: Vec<SpawnedWorker>,
    filling: usize,
    last_used: Instant,
}

/// The process-wide pool of prewarmed workers, keyed by agent entry path.
///
/// This deliberately does NOT reuse a worker across runs — the property the
/// os-isolation design refuses to give up. A pooled worker is an ordinary
/// spawn-per-run worker whose spawn (and module-graph compile, inside the full
/// sandbox) happened *before* the run arrived; it serves one `Init` and exits.
/// There is no cross-run state to reset because no worker ever sees two runs.
pub(crate) struct WarmPool {
    entries: Mutex<HashMap<String, PoolEntry>>,
}

impl WarmPool {
    pub(crate) fn global() -> &'static WarmPool {
        static POOL: OnceLock<WarmPool> = OnceLock::new();
        POOL.get_or_init(|| WarmPool {
            entries: Mutex::new(HashMap::new()),
        })
    }

    /// Take one live prewarmed worker for `key`, discarding expired ones along
    /// the way. `None` on a miss (the caller cold-spawns).
    fn take(&self, key: &str) -> Option<SpawnedWorker> {
        let mut entries = self.entries.lock().expect("warm pool poisoned");
        let ttl = warm_pool_ttl();
        // Sweep expired workers everywhere (an entry whose traffic stopped
        // would otherwise hold its processes until the next acquire of that
        // same key, which never comes).
        for entry in entries.values_mut() {
            let (keep, expired): (Vec<_>, Vec<_>) = entry
                .workers
                .drain(..)
                .partition(|w| w.spawned_at.elapsed() < ttl);
            entry.workers = keep;
            for worker in expired {
                worker.discard();
            }
        }
        entries.retain(|_, e| !e.workers.is_empty() || e.filling > 0);
        let entry = entries.get_mut(key)?;
        entry.last_used = Instant::now();
        entry.workers.pop()
    }

    /// Bring `key`'s pool up to the configured target in the background.
    /// Evicts the least-recently-used entry past [`WARM_POOL_MAX_KEYS`].
    fn replenish(&self, key: String, entry_source: String, prelude: Option<String>) {
        let target = warm_pool_target();
        let deficit = {
            let mut entries = self.entries.lock().expect("warm pool poisoned");
            if !entries.contains_key(&key) && entries.len() >= WARM_POOL_MAX_KEYS {
                if let Some(oldest) = entries
                    .iter()
                    .filter(|(_, e)| e.filling == 0)
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone())
                {
                    if let Some(evicted) = entries.remove(&oldest) {
                        for worker in evicted.workers {
                            worker.discard();
                        }
                    }
                }
            }
            let entry = entries.entry(key.clone()).or_insert_with(|| PoolEntry {
                workers: Vec::new(),
                filling: 0,
                last_used: Instant::now(),
            });
            let have = entry.workers.len() + entry.filling;
            let deficit = target.saturating_sub(have);
            entry.filling += deficit;
            deficit
        };
        for _ in 0..deficit {
            let key = key.clone();
            let entry_source = entry_source.clone();
            let prelude = prelude.clone();
            std::thread::Builder::new()
                .name("chidori-warm-pool".to_string())
                .spawn(move || {
                    let worker = prewarm_worker(&key, &entry_source, prelude.as_deref());
                    let mut entries = WarmPool::global()
                        .entries
                        .lock()
                        .expect("warm pool poisoned");
                    if let Some(entry) = entries.get_mut(&key) {
                        entry.filling = entry.filling.saturating_sub(1);
                        match worker {
                            // Park only up to the target: a concurrent config
                            // change or eviction may have shrunk the room.
                            Ok(worker) if entry.workers.len() < warm_pool_target() => {
                                entry.workers.push(worker);
                            }
                            Ok(worker) => worker.discard(),
                            Err(err) => {
                                if env_verbose() {
                                    eprintln!("isolate: warm-pool prewarm failed: {err:#}");
                                }
                            }
                        }
                    } else if let Ok(worker) = worker {
                        // The entry was evicted while we prewarmed.
                        worker.discard();
                    }
                })
                .expect("spawning warm-pool filler thread");
        }
    }
}

/// Spawn a worker and drive its prewarm to completion: send
/// [`FromParent::Prewarm`], service its brokered `__module_load` calls (the
/// only op a prewarm may make — anything else is answered with an error), and
/// consume the [`FromChild::Warmed`] ack, so the parked worker's pipe is quiet
/// and the next frame it reads is its run's `Init`.
fn prewarm_worker(
    entry_path: &str,
    entry_source: &str,
    prelude: Option<&str>,
) -> Result<SpawnedWorker> {
    let mut worker = spawn_worker()?;
    // A wedged prewarm (hostile source spinning the compiler, a deadlocked
    // pipe) must not park this filler thread forever.
    let watchdog = DeadlineWatchdog::arm(worker.child.id(), PREWARM_DEADLINE);
    let result = prewarm_exchange(
        &mut worker.from_child,
        &mut worker.to_child,
        FromParent::Prewarm {
            entry_path: entry_path.to_string(),
            entry_source: entry_source.to_string(),
            prelude: prelude.map(str::to_string),
            limits: ResourceLimits::from_env(),
        },
    );
    let killed = watchdog.disarm();
    match result {
        Ok(modules) => {
            if env_verbose() {
                eprintln!(
                    "isolate: prewarmed worker for {entry_path} ({modules} modules, {} ms)",
                    worker.spawned_at.elapsed().as_millis()
                );
            }
            Ok(worker)
        }
        Err(e) => {
            worker.discard();
            if killed {
                Err(e.context(format!(
                    "prewarm exceeded the {} s deadline and was killed",
                    PREWARM_DEADLINE.as_secs()
                )))
            } else {
                Err(e)
            }
        }
    }
}

/// The parent half of the prewarm handshake: send the `Prewarm` frame, service
/// the worker's brokered `__module_load` calls (the only op a prewarm may make
/// — anything else is answered with an error), and consume the `Warmed` ack so
/// the parked worker's pipe is quiet and the next frame it reads is its run's
/// `Init`. Generic over the transport so tests can drive it over an in-process
/// socket pair; `pub(crate)` for that reason.
pub(crate) fn prewarm_exchange<R: Read, W: Write>(
    from_child: &mut R,
    to_child: &mut W,
    prewarm: FromParent,
) -> Result<u32> {
    write_frame(to_child, &prewarm).context("sending Prewarm to the isolate worker")?;
    loop {
        let msg: FromChild =
            read_frame(from_child).context("isolate worker terminated during prewarm")?;
        match msg {
            FromChild::Call { op, args } if op == "__module_load" => {
                let outcome: Outcome = serve_module_load(&args).into();
                write_frame(to_child, &FromParent::Reply(outcome))
                    .context("replying to the prewarming worker")?;
            }
            FromChild::Call { op, .. } => {
                let outcome = Outcome::Err(format!("op `{op}` is not available during prewarm"));
                write_frame(to_child, &FromParent::Reply(outcome))
                    .context("replying to the prewarming worker")?;
            }
            FromChild::Warmed { modules } => return Ok(modules),
            FromChild::Done { outcome } => {
                let err = match outcome {
                    Outcome::Err(e) => e,
                    Outcome::Ok(_) => "unexpected Done during prewarm".to_string(),
                };
                return Err(anyhow!(err));
            }
        }
    }
}

/// The prewarm-time `__module_load` handler: the identical resolution the run
/// path's [`route_host_op`] performs, minus any backend (module loading is
/// context-free — pure filesystem + registry resolution).
fn serve_module_load(args: &Value) -> std::result::Result<Value, String> {
    let specifier = args
        .get("specifier")
        .and_then(|v| v.as_str())
        .ok_or("__module_load: missing `specifier`")?;
    let importer = args
        .get("importer")
        .and_then(|v| v.as_str())
        .ok_or("__module_load: missing `importer`")?;
    let (key, source) =
        crate::runtime::typescript::loader::load_module_source(specifier, importer)?;
    Ok(serde_json::json!({ "key": key, "source": source }))
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
