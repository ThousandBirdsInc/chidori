//! `chidori.actors.spawn` — supervised, message-passing actor sub-runs.
//!
//! An actor is a detached sibling of a `chidori.branch` sub-run: the agent
//! spawns another agent module as a long-lived concurrent process with its own
//! durable mailbox, talks to it with named messages, and collects its final
//! outcome with `joinActor` (`docs/actors.md`). Where a branch is a bounded
//! fork-compare fan-out, an actor is addressable while it runs: it can wait on
//! messages (`chidori.receive`, or the existing `chidori.signal` family), send
//! to its siblings and to the spawning run, and be restarted by the runtime
//! when it fails.
//!
//! ## Execution model
//!
//! Each actor runs on its own OS thread inside a supervision loop. One
//! iteration = one pass of the actor's source module on a fresh VM, driven by
//! the standard resume-by-replay model: the actor's accumulated call log is
//! replayed from the top, recorded effects return from cache, and execution
//! goes live at the frontier.
//!
//! In the steady state an actor stays LIVE across messages: a
//! `chidori.signal`-family listen point with an empty mailbox blocks in
//! place on the shared-mailbox condvar (the [`ActorSignalWaiter`] installed
//! per iteration) and continues when the next matching message (or the
//! listen point's own timeout) arrives — the module is NOT re-executed per
//! message. The loop re-enters the module only when the actor parks — the
//! idle cap elapses or a stop is requested — and when it fails (a restart,
//! if the spawn's supervision options allow one). `chidori.receive` blocks
//! in place as it always has.
//!
//! [`ActorSignalWaiter`]: crate::runtime::context::ActorSignalWaiter
//!
//! ## Durability
//!
//! Every actor primitive is an ordinary durable host call on the calling
//! run's log (`spawn_actor`, `send_actor`, `receive`, `join_actor`,
//! `stop_actor`, `actor_status`, `whereis`), so a replay of the parent
//! returns every recorded result from cache and never re-runs the actors.
//! The actor's own records live in a reserved [`CallLogSequenceRange`]
//! (disjoint from the parent and from sibling actors, exactly like branch
//! ranges) and fold into the parent's log when the actor is joined, stamped
//! with the `join_actor` call's seq as their parent — so a parent replay
//! absorbs the whole actor subtree at the join, keeping the sequence counter
//! aligned. If the parent crashes before a join, the recorded `spawn_actor`
//! and `send_actor` calls are enough to re-create the actor live on resume
//! (fresh run, mailbox re-seeded from the recorded sends): actor work that
//! was never joined re-executes rather than being lost.
//!
//! ## Supervision
//!
//! `spawnActor` options select what the runtime does when an iteration fails:
//! - `restart: "never"` (default) — the failure is the actor's final outcome;
//! - `restart: "clean"` — re-run the module from scratch (fresh log, the
//!   spawn-time VFS anchor, the original input);
//! - `restart: "resume"` — replay the accumulated log with the trailing
//!   failed host records stripped, so completed work is kept and the failing
//!   call itself re-executes live (a retry-from-the-frontier, which a
//!   from-scratch restart cannot express).
//!
//! `maxRestarts` bounds the loop and `backoffMs` spaces attempts (doubling
//! per attempt). Restarts never re-fire recorded effects: on `resume` the
//! replayed prefix returns from cache.
//!
//! ## Supervision trees
//!
//! Actors can spawn actors: each entry records its `owner`, a child's
//! reserved range is carved *out of* its owner's range (subdividing by
//! [`ACTOR_RANGE_SUBDIVISION`] per level, so a whole subtree stays inside the
//! top-level actor's range and merges upward join by join), `"parent"`
//! addresses the owner's mailbox, and only the owner may join or stop a
//! child. Supervisors reap their children: when an actor settles — or
//! discards its log on a `clean` restart — its still-live children are
//! cooperatively stopped first ([`ActorHub::stop_owned_subtree`]), so
//! children never outlive their supervisor and registered names are released
//! for the restarted attempt to re-claim.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};

use crate::runtime::call_log::CallRecord;
use crate::runtime::context::{RuntimeContext, PAUSE_MARKER};
use crate::runtime::host_core;
use crate::runtime::snapshot::{CallLogSequenceRange, QueuedSignal};
use crate::runtime::typescript::bindings::HostBindingBackend;
use crate::runtime::vfs::Vfs;

/// Hard cap on actors spawned by one run (the whole tree, restarts included):
/// every live actor is an OS thread making real host calls (LLM/tool spend),
/// so an unbounded spawn loop is a cost hazard before it is a correctness one.
const MAX_ACTORS: usize = 128;

/// Stack size for actor threads — the same headroom branch workers get.
const ACTOR_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Width of a top-level actor's reserved call-log sequence range. Wide on
/// purpose: supervision trees carve child ranges *out of* their parent's
/// range (that is what keeps a whole subtree inside the confinement check at
/// the top-level join), dividing by [`ACTOR_RANGE_SUBDIVISION`] per level.
/// 10^12 → children get 10^9, grandchildren 10^6, great-grandchildren 10^3.
/// Sequence numbers stay far below 2^53, so recorded seqs remain exact when
/// they cross into JavaScript or JSON tooling.
const ACTOR_TOP_RANGE_WIDTH: u64 = 1_000_000_000_000;

/// How many ways each level's range is subdivided for the next level down.
const ACTOR_RANGE_SUBDIVISION: u64 = 1000;

/// The smallest range an actor can be given (and therefore the depth floor:
/// an actor whose children would get less than this cannot spawn).
const MIN_ACTOR_RANGE_WIDTH: u64 = 1000;

/// How long an actor may sit waiting on an empty mailbox with no explicit
/// `timeoutMs` before the runtime parks it as a `paused` outcome instead of
/// holding its thread forever. Overridable per spawn with `idleTimeoutMs`.
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Default restart intensity when a restart strategy is selected.
const DEFAULT_MAX_RESTARTS: u32 = 3;

/// The reserved `sendActor` target / message `from` address for the sender's
/// spawner: the owning actor for a child in a supervision tree, or the
/// spawning run for a top-level actor.
const PARENT_ADDRESS: &str = "parent";

// --- Supervision options -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartStrategy {
    /// A failure is final (the default): the actor settles as `failed`.
    Never,
    /// Re-run the module from scratch: fresh call log, the spawn-time VFS
    /// anchor, the original input. Messages consumed by the failed attempt are
    /// gone (they were delivered); unconsumed ones stay queued.
    Clean,
    /// Replay the accumulated call log with the trailing failed records
    /// stripped, so finished work returns from cache and the failing call
    /// re-executes live.
    Resume,
}

#[derive(Debug, Clone)]
struct SupervisionOptions {
    name: Option<String>,
    restart: RestartStrategy,
    max_restarts: u32,
    backoff_ms: u64,
    idle_timeout_ms: u64,
}

impl SupervisionOptions {
    fn parse(options: &Value) -> Result<Self, String> {
        let name = options
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref name) = name {
            if name.is_empty() || name == PARENT_ADDRESS || name.starts_with("actor-") {
                return Err(format!(
                    "chidori.actors.spawn: `{name}` is not a registrable actor name (reserved)"
                ));
            }
        }
        let restart = match options.get("restart").and_then(Value::as_str) {
            None | Some("never") => RestartStrategy::Never,
            Some("clean") => RestartStrategy::Clean,
            Some("resume") => RestartStrategy::Resume,
            Some(other) => {
                return Err(format!(
                    "chidori.actors.spawn: unknown restart strategy `{other}` \
                     (expected \"never\", \"clean\", or \"resume\")"
                ))
            }
        };
        Ok(Self {
            name,
            restart,
            max_restarts: options
                .get("maxRestarts")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(DEFAULT_MAX_RESTARTS),
            backoff_ms: options
                .get("backoffMs")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            idle_timeout_ms: options
                .get("idleTimeoutMs")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_MS),
        })
    }

    /// The normalized options object stored in the durable `spawn_actor` args.
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "restart": match self.restart {
                RestartStrategy::Never => "never",
                RestartStrategy::Clean => "clean",
                RestartStrategy::Resume => "resume",
            },
            "maxRestarts": self.max_restarts,
            "backoffMs": self.backoff_ms,
            "idleTimeoutMs": self.idle_timeout_ms,
        })
    }
}

// --- The hub ------------------------------------------------------------------

/// Where an actor is in its lifecycle, as observed from outside its thread.
#[derive(Debug, Clone)]
enum Lifecycle {
    Running,
    /// Parked on an empty mailbox at a listen point for these names.
    Waiting(Vec<String>),
    /// The supervision loop has settled: `completed`, `failed`, `paused`, or
    /// `stopped`.
    Terminal(String),
}

impl Lifecycle {
    fn status_str(&self) -> &str {
        match self {
            Lifecycle::Running => "running",
            Lifecycle::Waiting(_) => "waiting",
            Lifecycle::Terminal(status) => status,
        }
    }
}

/// The state one actor shares between its thread and the runs addressing it.
#[derive(Debug)]
struct SharedState {
    mailbox: Vec<QueuedSignal>,
    next_delivery_seq: u64,
    lifecycle: Lifecycle,
    stop_requested: bool,
    restarts: u32,
}

/// One actor's rendezvous point: mailbox + lifecycle behind a mutex, and a
/// condvar the actor waits on (for messages / stop / backoff) that senders and
/// the parent's join/stop notify.
#[derive(Debug)]
struct ActorShared {
    state: Mutex<SharedState>,
    signal: Condvar,
}

impl ActorShared {
    fn new() -> Self {
        Self {
            state: Mutex::new(SharedState {
                mailbox: Vec::new(),
                next_delivery_seq: 0,
                lifecycle: Lifecycle::Running,
                stop_requested: false,
                restarts: 0,
            }),
            signal: Condvar::new(),
        }
    }

    fn deliver(&self, name: &str, payload: Value, from: Value) {
        let mut state = self.state.lock().unwrap();
        state.next_delivery_seq += 1;
        let delivery_seq = state.next_delivery_seq;
        state.mailbox.push(QueuedSignal {
            name: name.to_string(),
            payload,
            from,
            delivery_seq,
            enqueued_at: Utc::now(),
        });
        drop(state);
        self.signal.notify_all();
    }

    fn request_stop(&self) {
        self.state.lock().unwrap().stop_requested = true;
        self.signal.notify_all();
    }

    fn set_lifecycle(&self, lifecycle: Lifecycle) {
        self.state.lock().unwrap().lifecycle = lifecycle;
        self.signal.notify_all();
    }
}

/// The final settlement of an actor's supervision loop, carried back to the
/// parent through the thread's join handle. `records` is the final
/// iteration's complete call log (replayed prefix + live continuation), which
/// the parent folds into its own log at `join_actor`.
struct ActorOutcome {
    status: &'static str,
    output: Option<Value>,
    error: Option<String>,
    pending_prompt: Option<String>,
    restarts: u32,
    records: Vec<CallRecord>,
}

struct ActorEntry {
    shared: Arc<ActorShared>,
    handle: Option<JoinHandle<ActorOutcome>>,
    sequence_range: CallLogSequenceRange,
    /// The pid of the actor that spawned this one, or `None` for a top-level
    /// actor spawned by the run. Only the owner may join or stop it; its
    /// `"parent"`-addressed messages deliver to the owner's mailbox; and the
    /// owner's settle/clean-restart reaps it.
    owner: Option<String>,
    /// The registered name, kept for unregistration when the owner reaps this
    /// actor (see [`ActorHub::stop_owned_subtree`]).
    name: Option<String>,
    /// Width of the range blocks this actor's own children are carved from
    /// (this actor's range width / [`ACTOR_RANGE_SUBDIVISION`]).
    child_width: u64,
    /// Allocation high-water mark for child blocks, relative to this actor's
    /// range start.
    child_cursor: u64,
    /// The durable `join_actor`/`stop_actor` outcome once the actor has been
    /// joined and its records merged. A second join returns this instead of
    /// merging twice.
    settled_outcome: Option<Value>,
}

#[derive(Default)]
struct HubInner {
    actors: HashMap<String, ActorEntry>,
    /// Registered name → pid.
    names: HashMap<String, String>,
    /// The spawning run's own mailbox: messages actors send to `"parent"`.
    parent_mailbox: Vec<QueuedSignal>,
    parent_next_delivery_seq: u64,
    next_index: u64,
    /// High-water mark for reserved sequence-range allocation.
    range_cursor: u64,
    spawned_total: usize,
}

/// The per-run actor table: spawned actors, the name registry, and the
/// parent-addressed mailbox. Shared (via `Arc`) between the spawning run's
/// context and every actor context it creates, so all of them address one
/// table. Lock discipline: never call into a `RuntimeContext` or an
/// [`ActorShared`] while holding the hub lock.
pub struct ActorHub {
    inner: Mutex<HubInner>,
    /// Notified on parent-addressed deliveries and actor terminations, so a
    /// parent blocked in `chidori.receive` wakes up.
    parent_notify: Condvar,
}

impl std::fmt::Debug for ActorHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorHub").finish_non_exhaustive()
    }
}

impl ActorHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HubInner::default()),
            parent_notify: Condvar::new(),
        }
    }

    fn shared_of(&self, pid: &str) -> Option<Arc<ActorShared>> {
        self.inner
            .lock()
            .unwrap()
            .actors
            .get(pid)
            .map(|entry| entry.shared.clone())
    }

    /// Resolve a `sendActor`/`joinActor` target to a pid: a pid passes
    /// through (when known), a registered name maps through the registry.
    fn resolve_pid(&self, target: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        if inner.actors.contains_key(target) {
            return Some(target.to_string());
        }
        inner.names.get(target).cloned()
    }

    /// Whether any actor is still live (not yet terminal). A parent
    /// `receive` with no timeout refuses to block when nothing can ever send.
    fn has_live_actors(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.actors.values().any(|entry| {
            !matches!(
                entry.shared.state.lock().unwrap().lifecycle,
                Lifecycle::Terminal(_)
            )
        })
    }

    fn deliver_to_parent(&self, name: &str, payload: Value, from: Value) {
        let mut inner = self.inner.lock().unwrap();
        inner.parent_next_delivery_seq += 1;
        let delivery_seq = inner.parent_next_delivery_seq;
        inner.parent_mailbox.push(QueuedSignal {
            name: name.to_string(),
            payload,
            from,
            delivery_seq,
            enqueued_at: Utc::now(),
        });
        drop(inner);
        self.parent_notify.notify_all();
    }

    fn take_parent_message(&self, names: &[String]) -> Option<QueuedSignal> {
        let mut inner = self.inner.lock().unwrap();
        let idx = inner
            .parent_mailbox
            .iter()
            .enumerate()
            .filter(|(_, m)| names.iter().any(|n| n == &m.name))
            .min_by_key(|(_, m)| m.delivery_seq)
            .map(|(i, _)| i)?;
        Some(inner.parent_mailbox.remove(idx))
    }

    /// The pid of the actor that spawned `pid` (`None` = the run).
    fn owner_of(&self, pid: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .actors
            .get(pid)
            .and_then(|entry| entry.owner.clone())
    }

    /// Reap `owner`'s live children: request a cooperative stop on each, wait
    /// for its supervision loop to settle, join its thread, unregister its
    /// name, and mark it settled as `stopped` (records discarded — they were
    /// never joined). Grandchildren are reaped transitively: each child's own
    /// thread runs this for *its* children on the way out. Called when an
    /// actor settles (children must not outlive their supervisor) and on a
    /// `clean` restart (the discarded log's spawns are about to re-run live,
    /// so the previous attempt's children must go first).
    fn stop_owned_subtree(&self, owner: &str) {
        let children: Vec<(String, Arc<ActorShared>)> = {
            let inner = self.inner.lock().unwrap();
            inner
                .actors
                .iter()
                .filter(|(_, entry)| {
                    entry.owner.as_deref() == Some(owner) && entry.settled_outcome.is_none()
                })
                .map(|(pid, entry)| (pid.clone(), entry.shared.clone()))
                .collect()
        };
        for (pid, shared) in children {
            shared.request_stop();
            {
                let mut state = shared.state.lock().unwrap();
                while !matches!(state.lifecycle, Lifecycle::Terminal(_)) {
                    state = shared.signal.wait(state).unwrap();
                }
            }
            let (handle, name) = {
                let mut inner = self.inner.lock().unwrap();
                let Some(entry) = inner.actors.get_mut(&pid) else {
                    continue;
                };
                (entry.handle.take(), entry.name.clone())
            };
            let restarts = shared.state.lock().unwrap().restarts;
            if let Some(handle) = handle {
                let _ = handle.join();
            }
            let mut inner = self.inner.lock().unwrap();
            if let Some(name) = name {
                if inner.names.get(&name).map(String::as_str) == Some(pid.as_str()) {
                    inner.names.remove(&name);
                }
            }
            if let Some(entry) = inner.actors.get_mut(&pid) {
                if entry.settled_outcome.is_none() {
                    entry.settled_outcome = Some(json!({
                        "pid": pid,
                        "status": "stopped",
                        "restarts": restarts,
                    }));
                }
            }
        }
    }
}

impl Default for ActorHub {
    fn default() -> Self {
        Self::new()
    }
}

// --- Host ops -----------------------------------------------------------------

fn runtime_ctx<'a>(
    backend: &'a HostBindingBackend,
    what: &str,
) -> Result<&'a RuntimeContext, String> {
    backend
        .runtime_ctx()
        .ok_or_else(|| format!("chidori.{what} requires the runtime host backend"))
}

/// Anchor a relative actor source path to the project root (the entrypoint's
/// directory, the same anchor `callAgent` and templates use) so
/// `chidori.actors.spawn("actors/x.ts")` works regardless of the host process's
/// working directory.
fn resolve_source(backend: &HostBindingBackend, source: &str) -> PathBuf {
    let path = std::path::Path::new(source);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        backend.template_engine().base_dir().join(path)
    }
}

/// `chidori.actors.spawn(source, input, options)` — start a supervised actor
/// sub-run and return `{ pid, name }`. One durable `spawn_actor` record: a
/// parent replay returns the pid from cache without starting a thread (the
/// actor is re-created lazily by the next live call that addresses it — see
/// [`ensure_live_actor`]).
pub(crate) fn spawn_actor(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "actors.spawn")?;
    if ctx.is_branch() {
        return Err(
            "chidori.actors.spawn is not supported inside a chidori.branch sub-run \
                    (a branch's records must stay inside its reserved sequence range)"
                .to_string(),
        );
    }

    let source = a
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.actors.spawn requires a source module path")?
        .to_string();
    let input = a
        .get("input")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let options_value = a
        .get("options")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let options = SupervisionOptions::parse(&options_value)?;
    // Fail a typo'd path before recording anything, like branch validation.
    // The durable args keep the original (possibly relative) path so replay
    // keys are stable across hosts; resolution happens again wherever the
    // actor is (re)started.
    if !resolve_source(backend, &source).is_file() {
        return Err(format!(
            "chidori.actors.spawn: source module not found: {source}"
        ));
    }

    let call_args = json!({
        "source": source,
        "input": input,
        "options": options.to_json(),
    });
    host_core::execute_durable_json_call(ctx, "spawn_actor", call_args, || {
        start_actor(
            backend,
            ctx,
            &source,
            &input,
            &options,
            ctx.actor_id(),
            Vec::new(),
        )
        .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// Allocate a pid and a reserved sequence range, register the optional name,
/// and start the supervision thread. Shared by the live `spawn_actor` path and
/// the crash-resume re-creation path (which seeds the mailbox from recorded
/// sends). `owner` is the spawning actor's pid (`None` when the run spawns):
/// a top-level actor's range comes from the run-level arena, a child's range
/// is carved out of its owner's range — that containment is what lets a whole
/// supervision tree merge upward through its owner's join while every record
/// still lands inside the top-level actor's reserved range.
fn start_actor(
    backend: &HostBindingBackend,
    ctx: &RuntimeContext,
    source: &str,
    input: &Value,
    options: &SupervisionOptions,
    owner: Option<String>,
    seed_mailbox: Vec<QueuedSignal>,
) -> Result<Value, String> {
    let hub = ctx.ensure_actor_hub();
    let shared = Arc::new(ActorShared::new());
    let (pid, range, child_width) = {
        let mut inner = hub.inner.lock().unwrap();
        if inner.spawned_total >= MAX_ACTORS {
            return Err(format!(
                "chidori.actors.spawn: a run may spawn at most {MAX_ACTORS} actors \
                 (restarted children count)"
            ));
        }
        if let Some(ref name) = options.name {
            if inner.names.contains_key(name) {
                return Err(format!(
                    "chidori.actors.spawn: actor name `{name}` is already registered"
                ));
            }
        }

        // Reserve the next disjoint block above every sequence number used so
        // far — the spawner's records, merged child ranges (the counter
        // advances past them at each join), and previously reserved blocks
        // (the cursor). For the run the arena is the global sequence space;
        // for an actor it is the actor's own reserved range, subdivided.
        let (origin, arena_end, width, cursor) = match owner {
            None => (0, u64::MAX, ACTOR_TOP_RANGE_WIDTH, inner.range_cursor),
            Some(ref owner_pid) => {
                let entry = inner.actors.get(owner_pid).ok_or_else(|| {
                    format!("chidori.actors.spawn: unknown owning actor `{owner_pid}`")
                })?;
                if entry.child_width < MIN_ACTOR_RANGE_WIDTH {
                    return Err(format!(
                        "chidori.actors.spawn: supervision tree too deep — actor `{owner_pid}` \
                         has no sequence-range headroom left to subdivide for children \
                         (each level divides its range by {ACTOR_RANGE_SUBDIVISION})"
                    ));
                }
                (
                    entry.sequence_range.start - 1,
                    entry.sequence_range.end_exclusive - 1,
                    entry.child_width,
                    entry.child_cursor,
                )
            }
        };
        let floor = ctx.current_seq().saturating_sub(origin).max(cursor).max(1);
        let base = floor.div_ceil(width) * width;
        if origin + base + width > arena_end {
            return Err(format!(
                "chidori.actors.spawn: the spawner's reserved sequence range is exhausted \
                 (cannot fit another child block of width {width})"
            ));
        }
        match owner {
            None => inner.range_cursor = base + width,
            Some(ref owner_pid) => {
                if let Some(entry) = inner.actors.get_mut(owner_pid) {
                    entry.child_cursor = base + width;
                }
            }
        }
        let range = CallLogSequenceRange {
            start: origin + base + 1,
            end_exclusive: origin + base + 1 + width,
        };

        inner.spawned_total += 1;
        inner.next_index += 1;
        let pid = format!("actor-{}", inner.next_index);
        if let Some(ref name) = options.name {
            inner.names.insert(name.clone(), pid.clone());
        }
        (pid, range, width / ACTOR_RANGE_SUBDIVISION)
    };

    {
        let mut state = shared.state.lock().unwrap();
        for message in seed_mailbox {
            state.next_delivery_seq += 1;
            let delivery_seq = state.next_delivery_seq;
            state.mailbox.push(QueuedSignal {
                delivery_seq,
                ..message
            });
        }
    }

    let thread = {
        let backend = backend.clone();
        let parent_ctx = ctx.clone();
        let hub = hub.clone();
        let shared = shared.clone();
        let pid = pid.clone();
        let source = resolve_source(&backend, source);
        let input = input.clone();
        let options = options.clone();
        let range = range.clone();
        let anchor_vfs = ctx.vfs_snapshot();
        std::thread::Builder::new()
            .name(format!("chidori-{pid}"))
            .stack_size(ACTOR_THREAD_STACK_BYTES)
            .spawn(move || {
                let outcome = supervise(
                    &backend,
                    &parent_ctx,
                    &hub,
                    &shared,
                    &pid,
                    &source,
                    &input,
                    &options,
                    &range,
                    &anchor_vfs,
                );
                // Children must not outlive their supervisor: reap any this
                // actor spawned and never settled, BEFORE going terminal, so
                // whoever joins this actor observes a fully-settled subtree.
                hub.stop_owned_subtree(&pid);
                shared.set_lifecycle(Lifecycle::Terminal(outcome.status.to_string()));
                // Wake a parent blocked in `receive` so it can re-check
                // whether anything can still send to it.
                hub.parent_notify.notify_all();
                outcome
            })
            .map_err(|err| format!("chidori.actors.spawn: spawning actor thread: {err}"))?
    };

    hub.inner.lock().unwrap().actors.insert(
        pid.clone(),
        ActorEntry {
            shared,
            handle: Some(thread),
            sequence_range: range,
            owner,
            name: options.name.clone(),
            child_width,
            child_cursor: 0,
            settled_outcome: None,
        },
    );

    let mut spawned = json!({ "pid": pid });
    if let Some(ref name) = options.name {
        spawned["name"] = json!(name);
    }
    Ok(spawned)
}

/// `chidori.actors.send(to, name, payload)` — deliver a named message to an
/// actor's mailbox (`to` = pid or registered name) or to the spawning run
/// (`to = "parent"`). Durable: one `send_actor` record per delivery; replay
/// returns the recorded receipt without re-delivering.
pub(crate) fn send_actor(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "actors.send")?;
    let to = a
        .get("to")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.actors.send requires a target (a pid, a registered name, or \"parent\")")?
        .to_string();
    let name = a
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.actors.send requires a string message name")?
        .to_string();
    let payload = a.get("payload").cloned().unwrap_or(Value::Null);
    // Sender identity, in the same shape external signal senders use
    // (`SignalSender`): `id` is the sending actor's pid, or `"run"` when the
    // spawning run itself sends.
    let from = json!({
        "kind": "agent",
        "id": ctx.actor_id().unwrap_or_else(|| "run".to_string()),
    });

    let call_args = json!({ "to": to, "name": name, "payload": payload, "from": from });
    host_core::execute_durable_json_call(ctx, "send_actor", call_args, || {
        if to == PARENT_ADDRESS {
            let hub = ctx
                .actor_hub()
                .ok_or_else(|| anyhow::anyhow!("chidori.actors.send: no actor hub in this run"))?;
            // "parent" is the sender's spawner: the owning actor for a child
            // in a supervision tree, the run's own mailbox for a top-level
            // actor (or the run itself — a recorded self-send).
            let owner = ctx.actor_id().and_then(|sender| hub.owner_of(&sender));
            match owner {
                Some(owner_pid) => {
                    let shared = hub.shared_of(&owner_pid).ok_or_else(|| {
                        anyhow::anyhow!("chidori.actors.send: unknown owning actor `{owner_pid}`")
                    })?;
                    shared.deliver(&name, payload.clone(), from.clone());
                }
                None => hub.deliver_to_parent(&name, payload.clone(), from.clone()),
            }
            return Ok(json!({ "delivered": true }));
        }
        let (hub, pid) =
            ensure_live_actor(backend, ctx, &to).map_err(|err| anyhow::anyhow!(err))?;
        let shared = hub
            .shared_of(&pid)
            .ok_or_else(|| anyhow::anyhow!("chidori.actors.send: unknown actor `{to}`"))?;
        let live = !matches!(
            shared.state.lock().unwrap().lifecycle,
            Lifecycle::Terminal(_)
        );
        shared.deliver(&name, payload.clone(), from.clone());
        Ok(json!({ "delivered": live }))
    })
    .map_err(|err| err.to_string())
}

/// `chidori.receive(names, opts)` — blocking, in-place message consumption.
/// Inside an actor it drains the actor's own mailbox; in the spawning run it
/// drains the parent-addressed mailbox (and any pre-queued external signals).
/// Unlike `chidori.signal` — which pauses the whole run and unwinds the VM so
/// an *external* delivery can resume it later — `receive` parks the calling
/// thread and is woken directly by in-process senders, so it never tears down
/// the VM. Resolves to the consumed `{name, payload, from}` message, or to the
/// `{ timedOut: true }` sentinel when `opts.timeoutMs` passes first.
pub(crate) fn receive(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "receive")?;
    let names: Vec<String> = match a.get("names") {
        Some(Value::String(name)) if !name.is_empty() => vec![name.clone()],
        Some(Value::Array(values)) => {
            let names: Option<Vec<String>> = values
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect();
            match names {
                Some(names) if !names.is_empty() => names,
                _ => return Err("chidori.receive requires a name or an array of names".into()),
            }
        }
        _ => return Err("chidori.receive requires a name or an array of names".into()),
    };
    let timeout_ms = a
        .get("opts")
        .and_then(|opts| opts.get("timeoutMs"))
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0);

    let call_args = json!({ "names": names, "opts": { "timeoutMs": timeout_ms } });
    host_core::execute_durable_json_call(ctx, "receive", call_args, || {
        receive_live(ctx, &names, timeout_ms).map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

fn message_json(message: &QueuedSignal) -> Value {
    json!({
        "name": message.name,
        "payload": message.payload,
        "from": message.from,
    })
}

fn receive_live(
    ctx: &RuntimeContext,
    names: &[String],
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    if let Some(pid) = ctx.actor_id() {
        // Inside an actor. Anything already pumped into the run-level inbox
        // (by an earlier listen-point wait) is consumed first, in delivery
        // order; then the live shared mailbox.
        let hub = ctx
            .actor_hub()
            .ok_or("chidori.receive: actor context has no hub")?;
        let shared = hub
            .shared_of(&pid)
            .ok_or_else(|| format!("chidori.receive: unknown actor `{pid}`"))?;
        if let Some(queued) = ctx.take_queued_signal_any(names) {
            return Ok(message_json(&queued));
        }
        let mut state = shared.state.lock().unwrap();
        loop {
            if state.stop_requested {
                return Err(format!("chidori.receive: actor `{pid}` was stopped"));
            }
            if let Some(idx) = state
                .mailbox
                .iter()
                .enumerate()
                .filter(|(_, m)| names.iter().any(|n| n == &m.name))
                .min_by_key(|(_, m)| m.delivery_seq)
                .map(|(i, _)| i)
            {
                let message = state.mailbox.remove(idx);
                return Ok(message_json(&message));
            }
            let now = Instant::now();
            if let Some(deadline) = deadline {
                if now >= deadline {
                    return Ok(host_core::signal_timeout_sentinel(names));
                }
            }
            let wait = deadline
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(Duration::from_millis(200))
                .min(Duration::from_millis(200));
            state = shared.signal.wait_timeout(state, wait).unwrap().0;
        }
    }

    // The spawning run. External signals already queued in the run inbox are
    // eligible too, then the parent-addressed actor mailbox. The wait is a
    // short-interval condvar loop so externally delivered signals (which
    // don't notify the hub) are still picked up promptly.
    let hub = ctx.actor_hub();
    loop {
        if let Some(queued) = ctx.take_queued_signal_any(names) {
            return Ok(message_json(&queued));
        }
        if let Some(ref hub) = hub {
            if let Some(message) = hub.take_parent_message(names) {
                return Ok(message_json(&message));
            }
        }
        let now = Instant::now();
        if let Some(deadline) = deadline {
            if now >= deadline {
                return Ok(host_core::signal_timeout_sentinel(names));
            }
        } else {
            let can_be_sent_to = hub.as_ref().is_some_and(|hub| hub.has_live_actors());
            if !can_be_sent_to {
                return Err(
                    "chidori.receive would block forever: no live actors to send a message \
                     and no timeoutMs given"
                        .to_string(),
                );
            }
        }
        let wait = deadline
            .map(|d| d.saturating_duration_since(now))
            .unwrap_or(Duration::from_millis(200))
            .min(Duration::from_millis(200));
        match hub {
            Some(ref hub) => {
                let inner = hub.inner.lock().unwrap();
                let _unused = hub.parent_notify.wait_timeout(inner, wait).unwrap();
            }
            None => std::thread::sleep(wait),
        }
    }
}

/// `chidori.actors.join(pid, opts)` — wait for an actor's supervision loop to
/// settle, fold its records into this run's log (stamped with this call's seq
/// so a replay absorbs the subtree here), and return the outcome
/// `{ pid, status, output?, error?, pendingPrompt?, restarts }`. With
/// `opts.timeoutMs`, a still-running actor yields `{ status: "running" }`
/// without merging — join again later for the terminal outcome.
pub(crate) fn join_actor(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    settle_actor_call(backend, a, "join_actor", false)
}

/// `chidori.actors.stop(pid)` — request a cooperative stop (honored between
/// iterations, at mailbox waits, and during backoff — a live LLM/tool call
/// finishes first), then join and merge exactly like `joinActor`.
pub(crate) fn stop_actor(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    settle_actor_call(backend, a, "stop_actor", true)
}

fn settle_actor_call(
    backend: &HostBindingBackend,
    a: &Value,
    function: &str,
    request_stop: bool,
) -> Result<Value, String> {
    // The JS-facing name for error messages; `function` is the journal name.
    let display = if function == "stop_actor" {
        "actors.stop"
    } else {
        "actors.join"
    };
    let ctx = runtime_ctx(backend, display)?;
    let target = a
        .get("pid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("chidori.{display} requires an actor pid or registered name"))?
        .to_string();
    let timeout_ms = a
        .get("opts")
        .and_then(|opts| opts.get("timeoutMs"))
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0);

    let call_args = json!({ "pid": target, "opts": { "timeoutMs": timeout_ms } });
    let seq = ctx.next_seq();
    host_core::execute_durable_json_call_at_seq(ctx, seq, function, call_args, || {
        let (hub, pid) =
            ensure_live_actor(backend, ctx, &target).map_err(|err| anyhow::anyhow!(err))?;
        // Ownership gate: settling folds the target's records into THIS log,
        // so only the spawner may do it — the run for top-level actors, the
        // owning actor for its children in a supervision tree.
        let caller = ctx.actor_id();
        let owner = hub.owner_of(&pid);
        if owner != caller {
            return Err(anyhow::anyhow!(
                "chidori.{display}: `{target}` was spawned by {}, not {} — actors are \
                 settled by their spawner",
                owner.as_deref().unwrap_or("the run"),
                caller.as_deref().unwrap_or("the run"),
            ));
        }
        if let Some(settled) = hub
            .inner
            .lock()
            .unwrap()
            .actors
            .get(&pid)
            .and_then(|entry| entry.settled_outcome.clone())
        {
            // Already joined and merged: the stored outcome is the answer.
            return Ok(settled);
        }
        let shared = hub
            .shared_of(&pid)
            .ok_or_else(|| anyhow::anyhow!("chidori.{display}: unknown actor `{target}`"))?;
        if request_stop {
            shared.request_stop();
        }

        // Wait (bounded when a timeout was given) for the supervision loop to
        // settle. The hub lock is NOT held here — only this actor's state.
        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        {
            let mut state = shared.state.lock().unwrap();
            loop {
                if matches!(state.lifecycle, Lifecycle::Terminal(_)) {
                    break;
                }
                if let Some(deadline) = deadline {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(json!({
                            "pid": pid,
                            "status": "running",
                            "restarts": state.restarts,
                        }));
                    }
                    let wait = deadline.saturating_duration_since(now);
                    state = shared.signal.wait_timeout(state, wait).unwrap().0;
                } else {
                    state = shared.signal.wait(state).unwrap();
                }
            }
        }

        let (handle, range) = {
            let mut inner = hub.inner.lock().unwrap();
            let entry = inner
                .actors
                .get_mut(&pid)
                .ok_or_else(|| anyhow::anyhow!("chidori.{display}: unknown actor `{target}`"))?;
            (entry.handle.take(), entry.sequence_range.clone())
        };
        let handle =
            handle.ok_or_else(|| anyhow::anyhow!("chidori.{display}: actor `{pid}` join raced"))?;
        let outcome = handle
            .join()
            .map_err(|_| anyhow::anyhow!("actor `{pid}` thread panicked"))?;

        // Range confinement is the determinism guarantee, exactly as for
        // branches: every actor record must sit inside the reserved range
        // before it may join the durable log.
        let mut records = outcome.records;
        for record in &mut records {
            if !range.contains(record.seq) {
                return Err(anyhow::anyhow!(
                    "actor `{pid}` emitted call seq {} outside its reserved range {}..{}",
                    record.seq,
                    range.start,
                    range.end_exclusive
                ));
            }
            // Top-level actor records hang off this join call, so a parent
            // replay absorbs the whole subtree when the join replays.
            if record.parent_seq.is_none() {
                record.parent_seq = Some(seq);
            }
        }
        ctx.merge_branch_records(records);

        let mut result = json!({
            "pid": pid,
            "status": outcome.status,
            "restarts": outcome.restarts,
        });
        if let Some(output) = outcome.output {
            result["output"] = output;
        }
        if let Some(error) = outcome.error {
            result["error"] = json!(error);
        }
        if let Some(prompt) = outcome.pending_prompt {
            result["pendingPrompt"] = json!(prompt);
        }
        if let Some(entry) = hub.inner.lock().unwrap().actors.get_mut(&pid) {
            entry.settled_outcome = Some(result.clone());
        }
        Ok(result)
    })
    .map_err(|err| err.to_string())
}

/// `chidori.actors.status(pid)` — a durable snapshot of an actor's lifecycle:
/// `{ pid, status, restarts, mailbox }`.
pub(crate) fn actor_status(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "actors.status")?;
    let target = a
        .get("pid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.actors.status requires an actor pid or registered name")?
        .to_string();
    let call_args = json!({ "pid": target });
    host_core::execute_durable_json_call(ctx, "actor_status", call_args, || {
        let Some(hub) = ctx.actor_hub() else {
            return Ok(json!({ "pid": target, "status": "unknown" }));
        };
        let Some(pid) = hub.resolve_pid(&target) else {
            return Ok(json!({ "pid": target, "status": "unknown" }));
        };
        let Some(shared) = hub.shared_of(&pid) else {
            return Ok(json!({ "pid": target, "status": "unknown" }));
        };
        let state = shared.state.lock().unwrap();
        let mut status = json!({
            "pid": pid,
            "status": state.lifecycle.status_str(),
            "restarts": state.restarts,
            "mailbox": state.mailbox.len(),
        });
        if let Lifecycle::Waiting(ref names) = state.lifecycle {
            status["waitingFor"] = json!(names);
        }
        Ok(status)
    })
    .map_err(|err| err.to_string())
}

/// `chidori.actors.lookup(name)` — durable registry lookup: `{ pid }` (null when
/// the name is unregistered).
pub(crate) fn whereis(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "actors.lookup")?;
    let name = a
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.actors.lookup requires a registered actor name")?
        .to_string();
    let call_args = json!({ "name": name });
    host_core::execute_durable_json_call(ctx, "whereis", call_args, || {
        let pid = ctx
            .actor_hub()
            .and_then(|hub| hub.inner.lock().unwrap().names.get(&name).cloned());
        Ok(json!({ "pid": pid }))
    })
    .map_err(|err| err.to_string())
}

/// Resolve `target` to a live actor, re-creating it when this run was resumed
/// past a replayed `spawn_actor` (which returns its pid from cache without
/// starting a thread). The recorded spawn args re-create the actor fresh and
/// the recorded `send_actor` calls re-seed its mailbox, so unjoined actor work
/// re-executes on resume instead of being lost.
fn ensure_live_actor(
    backend: &HostBindingBackend,
    ctx: &RuntimeContext,
    target: &str,
) -> Result<(Arc<crate::runtime::host_actor::ActorHub>, String), String> {
    let hub = ctx.ensure_actor_hub();
    if let Some(pid) = hub.resolve_pid(target) {
        return Ok((hub, pid));
    }

    // Find the recorded spawn for this pid (or registered name).
    let records = ctx.call_log().into_records();
    let spawn = records
        .iter()
        .find(|r| {
            r.function == "spawn_actor"
                && r.error.is_none()
                && (r.result.get("pid").and_then(Value::as_str) == Some(target)
                    || r.result.get("name").and_then(Value::as_str) == Some(target))
        })
        .ok_or_else(|| format!("unknown actor `{target}` (no live actor and no recorded spawn)"))?;
    let pid = spawn
        .result
        .get("pid")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("recorded spawn for `{target}` has no pid"))?
        .to_string();
    let source = spawn
        .args
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("recorded spawn for `{target}` has no source"))?
        .to_string();
    let input = spawn.args.get("input").cloned().unwrap_or(json!({}));
    let options = SupervisionOptions::parse(spawn.args.get("options").unwrap_or(&Value::Null))?;

    // Re-seed the mailbox from every recorded send addressed to this actor
    // (by pid or by its registered name), in recorded order. The re-created
    // actor runs fresh, so it re-consumes them the way the original did.
    let name = spawn.result.get("name").and_then(Value::as_str);
    let seed: Vec<QueuedSignal> = records
        .iter()
        .filter(|r| r.function == "send_actor" && r.error.is_none())
        .filter(|r| {
            let to = r.args.get("to").and_then(Value::as_str);
            to == Some(pid.as_str()) || (name.is_some() && to == name)
        })
        .map(|r| QueuedSignal {
            name: r
                .args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            payload: r.args.get("payload").cloned().unwrap_or(Value::Null),
            from: r.args.get("from").cloned().unwrap_or(Value::Null),
            delivery_seq: 0, // reassigned by the seeding loop
            enqueued_at: Utc::now(),
        })
        .collect();

    let spawned = start_actor(
        backend,
        ctx,
        &source,
        &input,
        &options,
        ctx.actor_id(),
        seed,
    )?;
    let live_pid = spawned
        .get("pid")
        .and_then(Value::as_str)
        .unwrap_or(&pid)
        .to_string();
    // The re-created actor gets a fresh pid from the hub counter; alias the
    // recorded pid to it so recorded-pid addressing keeps working.
    {
        let mut inner = hub.inner.lock().unwrap();
        if live_pid != pid {
            inner.names.insert(pid.clone(), live_pid.clone());
        }
    }
    Ok((hub, live_pid))
}

// --- The supervision loop -----------------------------------------------------

/// How one supervision-loop iteration ended.
enum IterationEnd {
    Completed(Value),
    Failed(String),
    /// Paused on something an actor cannot answer in-process (interactive
    /// input or a policy approval) — a terminal `paused` outcome.
    Parked(Option<String>),
    /// Paused at a `chidori.signal`/`signalAny` listen point with an empty
    /// mailbox: wait for a matching delivery, then re-enter.
    WaitSignal(crate::runtime::context::PendingSignal),
}

fn settle_iteration(result: anyhow::Result<Value>, ctx: &RuntimeContext) -> IterationEnd {
    match result {
        Ok(output) => IterationEnd::Completed(output),
        Err(err) if err.to_string().contains(PAUSE_MARKER) => {
            if let Some(pending) = ctx.take_pending_signal() {
                IterationEnd::WaitSignal(pending)
            } else if let Some(pending) = ctx.take_pending_input() {
                IterationEnd::Parked(Some(pending.prompt))
            } else if let Some(pending) = ctx.take_pending_approval() {
                IterationEnd::Parked(Some(format!("approval required: {}", pending.target)))
            } else {
                IterationEnd::Parked(None)
            }
        }
        Err(err) => IterationEnd::Failed(err.to_string()),
    }
}

/// Strip the trailing failed host records off a crashed iteration's log — the
/// crash frontier — so a `resume` restart replays every completed call from
/// cache and re-executes the failing call live. Failed records *before* the
/// frontier (errors the agent caught and handled) are preserved: their
/// consumption shaped the control flow that followed them.
fn strip_crash_frontier(mut records: Vec<CallRecord>) -> Vec<CallRecord> {
    while records.last().is_some_and(|r| r.error.is_some()) {
        records.pop();
    }
    records
}

/// How a mailbox wait ended.
enum WaitOutcome {
    Matched,
    TimedOut,
    Stopped,
    Idle,
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    backend: &HostBindingBackend,
    parent_ctx: &RuntimeContext,
    hub: &Arc<ActorHub>,
    shared: &Arc<ActorShared>,
    pid: &str,
    source: &std::path::Path,
    input: &Value,
    options: &SupervisionOptions,
    range: &CallLogSequenceRange,
    anchor_vfs: &Vfs,
) -> ActorOutcome {
    let mut replay: Vec<CallRecord> = Vec::new();
    let mut carried_inbox: Vec<QueuedSignal> = Vec::new();
    let mut restarts: u32 = 0;
    // Set when the inline listen-point wait already sat out the idle cap, so
    // the pause-path fallback below parks immediately instead of waiting the
    // idle window a second time.
    let inline_idled = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let outcome = |status: &'static str,
                   output: Option<Value>,
                   error: Option<String>,
                   pending_prompt: Option<String>,
                   restarts: u32,
                   records: Vec<CallRecord>| ActorOutcome {
        status,
        output,
        error,
        pending_prompt,
        restarts,
        records,
    };

    loop {
        if shared.state.lock().unwrap().stop_requested {
            return outcome("stopped", None, None, None, restarts, replay);
        }

        // One iteration: fresh VM + context, the accumulated log replayed from
        // the top, the mailbox leftovers carried in, new deliveries pumped.
        let ctx = RuntimeContext::for_actor(
            parent_ctx,
            pid.to_string(),
            range.start - 1,
            replay.clone(),
            anchor_vfs.clone(),
            std::mem::take(&mut carried_inbox),
            hub.clone(),
        );
        pump_mailbox(shared, &ctx);
        // Inline listen-point wait: a `chidori.signal` with an empty inbox
        // blocks HERE for the next matching delivery (or its own timeout) and
        // continues in place — the actor's history is not re-executed per
        // message. Stop and idle return `Park`, falling back to the pause
        // path this loop has always handled.
        {
            let waiter_shared = shared.clone();
            let waiter_idled = inline_idled.clone();
            let idle_timeout_ms = options.idle_timeout_ms;
            ctx.set_actor_signal_waiter(crate::runtime::context::ActorSignalWaiter::new(
                move |names, timeout_ms| {
                    waiter_shared.set_lifecycle(Lifecycle::Waiting(names.to_vec()));
                    let waited =
                        wait_for_message(&waiter_shared, names, timeout_ms, idle_timeout_ms);
                    waiter_shared.set_lifecycle(Lifecycle::Running);
                    match waited {
                        WaitOutcome::Matched => crate::runtime::context::ActorSignalWait::Delivered,
                        WaitOutcome::TimedOut => crate::runtime::context::ActorSignalWait::TimedOut,
                        WaitOutcome::Idle => {
                            waiter_idled.store(true, std::sync::atomic::Ordering::SeqCst);
                            crate::runtime::context::ActorSignalWait::Park
                        }
                        WaitOutcome::Stopped => crate::runtime::context::ActorSignalWait::Park,
                    }
                },
            ));
        }
        let Some(iter_backend) = backend.with_runtime_ctx(ctx.clone()) else {
            return outcome(
                "failed",
                None,
                Some("actor requires the runtime host backend".to_string()),
                None,
                restarts,
                replay,
            );
        };
        let result = crate::runtime::rust_engine::run_agent_file(source, input, &iter_backend);

        match settle_iteration(result, &ctx) {
            IterationEnd::Completed(output) => {
                return outcome(
                    "completed",
                    Some(output),
                    None,
                    None,
                    restarts,
                    ctx.call_log().into_records(),
                );
            }
            IterationEnd::Parked(prompt) => {
                return outcome(
                    "paused",
                    None,
                    None,
                    prompt,
                    restarts,
                    ctx.call_log().into_records(),
                );
            }
            IterationEnd::WaitSignal(pending) => {
                let names = pending.listen_names();
                // The listen call's function name, for the timeout sentinel's
                // synthetic record (replay matches on function name).
                let listen_fn = ctx
                    .pending_host_operation(pending.id)
                    .and_then(|op| op.function)
                    .unwrap_or_else(|| {
                        if names.len() > 1 {
                            "signal_any"
                        } else {
                            "signal"
                        }
                        .to_string()
                    });
                shared.set_lifecycle(Lifecycle::Waiting(names.clone()));
                let waited = if inline_idled.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    // The inline wait already exhausted the idle cap for this
                    // listen point; park now rather than waiting it again.
                    WaitOutcome::Idle
                } else {
                    wait_for_message(shared, &names, pending.timeout_ms, options.idle_timeout_ms)
                };
                shared.set_lifecycle(Lifecycle::Running);

                let records = ctx.call_log().into_records();
                carried_inbox = ctx.signal_inbox();
                match waited {
                    WaitOutcome::Stopped => {
                        return outcome("stopped", None, None, None, restarts, records);
                    }
                    WaitOutcome::Idle => {
                        return outcome(
                            "paused",
                            None,
                            None,
                            Some(format!("waiting on signal: {}", names.join(", "))),
                            restarts,
                            records,
                        );
                    }
                    WaitOutcome::Matched => {
                        replay = records;
                    }
                    WaitOutcome::TimedOut => {
                        // Resolve the listen point with the timeout sentinel
                        // by injecting a synthetic record at the pending seq —
                        // the same mechanism the server uses for signal
                        // timeouts on paused runs.
                        replay = records;
                        replay.push(CallRecord {
                            seq: pending.seq,
                            parent_seq: None,
                            function: listen_fn.clone(),
                            args: if listen_fn == "signal_any" {
                                json!({ "names": names })
                            } else {
                                json!({ "name": names[0] })
                            },
                            result: host_core::signal_timeout_sentinel(&names),
                            duration_ms: 0,
                            token_usage: None,
                            timestamp: Utc::now(),
                            error: None,
                        });
                    }
                }
            }
            IterationEnd::Failed(message) => {
                let records = ctx.call_log().into_records();
                // A stop request surfaces inside the iteration as an error
                // (e.g. an interrupted `receive`): that is a stop, not a
                // failure to retry.
                if shared.state.lock().unwrap().stop_requested {
                    return outcome("stopped", None, None, None, restarts, records);
                }
                let attempts_left =
                    options.restart != RestartStrategy::Never && restarts < options.max_restarts;
                if !attempts_left {
                    return outcome("failed", None, Some(message), None, restarts, records);
                }
                restarts += 1;
                shared.state.lock().unwrap().restarts = restarts;
                if options.backoff_ms > 0 {
                    // Exponential spacing, interruptible by a stop request.
                    let backoff = options
                        .backoff_ms
                        .saturating_mul(1u64 << (restarts - 1).min(16));
                    let deadline = Instant::now() + Duration::from_millis(backoff);
                    let mut state = shared.state.lock().unwrap();
                    while !state.stop_requested {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        state = shared
                            .signal
                            .wait_timeout(state, deadline.saturating_duration_since(now))
                            .unwrap()
                            .0;
                    }
                }
                carried_inbox = ctx.signal_inbox();
                replay = match options.restart {
                    RestartStrategy::Clean => {
                        // The discarded log's `spawn_actor` calls are about to
                        // re-run live, so the failed attempt's children must
                        // be reaped first — otherwise they would leak and
                        // squat on their registered names.
                        hub.stop_owned_subtree(pid);
                        Vec::new()
                    }
                    RestartStrategy::Resume => strip_crash_frontier(records),
                    RestartStrategy::Never => unreachable!("guarded by attempts_left"),
                };
            }
        }
    }
}

/// Drain everything delivered to the actor's shared mailbox into the run-level
/// signal inbox, in delivery order, so the standard listen points
/// (`chidori.signal` / `pollSignal`) and `chidori.receive` see
/// them. Called at the start of every iteration and (via
/// [`pump_own_mailbox`]) right before each live listen-family host call.
fn pump_mailbox(shared: &ActorShared, ctx: &RuntimeContext) {
    let drained: Vec<QueuedSignal> = {
        let mut state = shared.state.lock().unwrap();
        std::mem::take(&mut state.mailbox)
    };
    for message in drained {
        ctx.enqueue_live_signal(&message.name, message.payload, message.from);
    }
}

/// If `ctx` is an actor context, pump its shared mailbox into the run-level
/// inbox so an imminent listen-point drain sees every delivery to date. Called
/// by the dispatch layer before the signal-family effects.
pub(crate) fn pump_own_mailbox(ctx: &RuntimeContext) {
    let Some(pid) = ctx.actor_id() else {
        return;
    };
    let Some(hub) = ctx.actor_hub() else {
        return;
    };
    if let Some(shared) = hub.shared_of(&pid) {
        pump_mailbox(&shared, ctx);
    }
}

/// Park the actor thread until a matching message is delivered, a stop is
/// requested, the listen point's own `timeoutMs` deadline passes, or the
/// idle cap is reached.
fn wait_for_message(
    shared: &ActorShared,
    names: &[String],
    timeout_ms: Option<u64>,
    idle_timeout_ms: u64,
) -> WaitOutcome {
    let start = Instant::now();
    let listen_deadline = timeout_ms.map(|ms| start + Duration::from_millis(ms));
    let idle_deadline = start + Duration::from_millis(idle_timeout_ms);
    let mut state = shared.state.lock().unwrap();
    loop {
        if state.stop_requested {
            return WaitOutcome::Stopped;
        }
        if state
            .mailbox
            .iter()
            .any(|m| names.iter().any(|n| n == &m.name))
        {
            return WaitOutcome::Matched;
        }
        let now = Instant::now();
        if let Some(deadline) = listen_deadline {
            if now >= deadline {
                return WaitOutcome::TimedOut;
            }
        }
        if now >= idle_deadline {
            return WaitOutcome::Idle;
        }
        let mut wake = idle_deadline;
        if let Some(deadline) = listen_deadline {
            wake = wake.min(deadline);
        }
        state = shared
            .signal
            .wait_timeout(state, wake.saturating_duration_since(now))
            .unwrap()
            .0;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;

    use crate::mcp::McpManager;
    use crate::policy::{PolicyCache, PolicyConfig};
    use crate::providers::ProviderRegistry;
    use crate::runtime::context::RuntimeContext;
    use crate::runtime::rust_engine::run_agent;
    use crate::runtime::snapshot::RuntimePolicy;
    use crate::runtime::template::TemplateEngine;
    use crate::runtime::typescript::bindings::HostBindingBackend;
    use crate::tools::ToolRegistry;

    /// A fully-wired runtime backend over `ctx`/`tools`, mirroring the
    /// host_branch test harness.
    fn test_backend(ctx: RuntimeContext, tools: Arc<ToolRegistry>) -> HostBindingBackend {
        HostBindingBackend::for_runtime(
            ctx,
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(".")),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            PolicyConfig::from_env(),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("actor-test"),
            tools,
            Arc::new(McpManager::new()),
        )
    }

    fn test_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chidori-actor-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("actors")).unwrap();
        dir
    }

    fn write_agent(dir: &std::path::Path, name: &str, source: &str) -> String {
        let path = dir.join("actors").join(name);
        std::fs::write(&path, source).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn actor_spawns_runs_and_joins_with_merged_records() {
        let dir = test_dir("basic");
        let worker = write_agent(
            &dir,
            "worker.ts",
            r#"
            export async function agent(input: { base: number }) {
                await chidori.log("worker running");
                return { doubled: input.base * 2 };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__", { base: 21 });
                const outcome = await chidori.actors.join(pid);
                return { pid, outcome };
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["pid"], json!("actor-1"));
        assert_eq!(output["outcome"]["status"], json!("completed"));
        assert_eq!(output["outcome"]["output"], json!({ "doubled": 42 }));
        assert_eq!(output["outcome"]["restarts"], json!(0));

        // Parent log: spawn at seq 1, join at seq 2; the actor's own record
        // (its log call) sits in the reserved range and nests under the join.
        let records = ctx.call_log().into_records();
        let spawn = records
            .iter()
            .find(|r| r.function == "spawn_actor")
            .unwrap();
        let join = records.iter().find(|r| r.function == "join_actor").unwrap();
        assert_eq!(spawn.seq, 1);
        assert_eq!(join.seq, 2);
        let worker_log = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "worker running")
            .unwrap();
        assert_eq!(worker_log.parent_seq, Some(join.seq));
        assert!(
            (1_000_000_000_001..2_000_000_000_001u64).contains(&worker_log.seq),
            "{}",
            worker_log.seq
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn messages_resume_an_actor_waiting_at_a_signal_listen_point() {
        // The actor listens with the EXISTING signal API — the mailbox pump
        // makes actor messages consumable at ordinary listen points.
        let dir = test_dir("signal");
        let worker = write_agent(
            &dir,
            "listener.ts",
            r#"
            export async function agent() {
                await chidori.log("before listen");
                const msg = await chidori.signal("go");
                return { got: msg.payload, from: msg.from };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__");
                await chidori.actors.send(pid, "go", { speed: "fast" });
                const outcome = await chidori.actors.join(pid);
                return outcome.output;
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(
            output,
            json!({ "got": { "speed": "fast" }, "from": { "kind": "agent", "id": "run" } })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn receive_round_trip_between_parent_and_actor() {
        // Full message loop: parent sends work with `sendActor`, the actor
        // consumes it with the blocking in-place `receive`, replies to
        // "parent", and the parent's own `receive` picks the reply up.
        let dir = test_dir("receive");
        let worker = write_agent(
            &dir,
            "replier.ts",
            r#"
            export async function agent() {
                const task = await chidori.receive("task");
                await chidori.actors.send("parent", "done", { answer: task.payload.n * 2 });
                return { handled: 1 };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__");
                await chidori.actors.send(pid, "task", { n: 21 });
                const reply = await chidori.receive("done");
                const outcome = await chidori.actors.join(pid);
                return { answer: reply.payload.answer, from: reply.from.id, status: outcome.status };
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(
            output,
            json!({ "answer": 42, "from": "actor-1", "status": "completed" })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resume_past_a_replayed_spawn_recreates_the_actor_at_the_join() {
        // A run that crashed between a spawn and its join resumes with the
        // spawn/send records cached but no live actor thread. The first live
        // call that addresses the actor (the join) must re-create it fresh
        // and re-seed its mailbox from the recorded sends, so the unjoined
        // actor work re-executes instead of being lost.
        let counter = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_counter = counter.clone();
        registry.register_native("count", "counts live calls", Vec::new(), move |_args| {
            Ok(json!({ "count": tool_counter.fetch_add(1, Ordering::SeqCst) + 1 }))
        });

        let dir = test_dir("respawn");
        let worker = write_agent(
            &dir,
            "respawn.ts",
            r#"
            export async function agent() {
                const msg = await chidori.signal("go");
                const { count } = await chidori.tool("count", {});
                return { count, speed: msg.payload.speed };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__");
                await chidori.actors.send(pid, "go", { speed: "fast" });
                const outcome = await chidori.actors.join(pid);
                return outcome;
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let live_ctx = RuntimeContext::new();
        let registry = Arc::new(registry);
        let live_backend = test_backend(live_ctx.clone(), registry.clone());
        let live_output = run_agent(&path, &src, &json!({}), &live_backend).unwrap();
        assert_eq!(live_output["output"]["count"], json!(1));

        // Simulate the crash-time checkpoint: everything up to and including
        // the send survives; the join and the merged actor records do not.
        let records = live_ctx.call_log().into_records();
        let send_seq = records
            .iter()
            .find(|r| r.function == "send_actor")
            .unwrap()
            .seq;
        let truncated: Vec<_> = records.into_iter().filter(|r| r.seq <= send_seq).collect();
        assert_eq!(truncated.len(), 2, "spawn + send survive the crash");

        let resume_ctx = RuntimeContext::with_replay(truncated);
        let resume_backend = test_backend(resume_ctx, registry);
        let resumed = run_agent(&path, &src, &json!({}), &resume_backend).unwrap();

        // The actor re-ran (fresh execution → the live counter advanced), the
        // re-seeded mailbox answered its listen point, and the join settled.
        assert_eq!(resumed["status"], json!("completed"));
        assert_eq!(resumed["output"], json!({ "count": 2, "speed": "fast" }));
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_actor_restarts_clean_until_success() {
        // The `attempt` tool returns 1, 2, ... across LIVE invocations. The
        // actor throws while n < 2; with restart "clean" the runtime re-runs
        // it from scratch and the second attempt completes.
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_attempts = attempts.clone();
        registry.register_native("attempt", "counts attempts", Vec::new(), move |_args| {
            Ok(json!({ "n": tool_attempts.fetch_add(1, Ordering::SeqCst) + 1 }))
        });

        let dir = test_dir("restart-clean");
        let worker = write_agent(
            &dir,
            "flaky.ts",
            r#"
            export async function agent() {
                const { n } = await chidori.tool("attempt", {});
                if (n < 2) throw new Error("transient failure " + n);
                return { succeededOn: n };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__", {}, {
                    restart: "clean",
                    maxRestarts: 3,
                });
                return await chidori.actors.join(pid);
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(registry));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["status"], json!("completed"));
        assert_eq!(output["output"], json!({ "succeededOn": 2 }));
        assert_eq!(output["restarts"], json!(1));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resume_restart_replays_completed_work_and_retries_the_failing_call() {
        // `mark` succeeds and counts; `flaky` fails on its first live call.
        // With restart "resume" the runtime strips the crash frontier (the
        // failed `flaky` record) and replays the rest: `mark` must NOT run a
        // second time, `flaky` retries live and succeeds.
        let marks = Arc::new(AtomicUsize::new(0));
        let flaky_calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_marks = marks.clone();
        registry.register_native("mark", "counts completed work", Vec::new(), move |_args| {
            Ok(json!({ "mark": tool_marks.fetch_add(1, Ordering::SeqCst) + 1 }))
        });
        let tool_flaky = flaky_calls.clone();
        registry.register_native("flaky", "fails once", Vec::new(), move |_args| {
            let call = tool_flaky.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                anyhow::bail!("transient tool failure");
            }
            Ok(json!({ "call": call }))
        });

        let dir = test_dir("restart-resume");
        let worker = write_agent(
            &dir,
            "resume.ts",
            r#"
            export async function agent() {
                const { mark } = await chidori.tool("mark", {});
                const { call } = await chidori.tool("flaky", {});
                return { mark, call };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__", {}, {
                    restart: "resume",
                    maxRestarts: 2,
                });
                return await chidori.actors.join(pid);
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(registry));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["status"], json!("completed"));
        assert_eq!(output["output"], json!({ "mark": 1, "call": 2 }));
        assert_eq!(output["restarts"], json!(1));
        assert_eq!(
            marks.load(Ordering::SeqCst),
            1,
            "resume must not re-run completed work"
        );
        assert_eq!(flaky_calls.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exhausted_restarts_settle_as_failed() {
        let dir = test_dir("restart-exhausted");
        let worker = write_agent(
            &dir,
            "doomed.ts",
            r#"
            export async function agent() {
                throw new Error("always fails");
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__", {}, {
                    restart: "clean",
                    maxRestarts: 2,
                });
                return await chidori.actors.join(pid);
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["status"], json!("failed"));
        assert_eq!(output["restarts"], json!(2));
        assert!(
            output["error"].as_str().unwrap().contains("always fails"),
            "{}",
            output["error"]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn actor_outcomes_replay_from_cache_without_rerunning_actors() {
        // The whole spawn/send/join conversation is durable: a replay of the
        // parent returns every recorded result from cache, re-runs nothing
        // (the live counter stays put), and produces identical output.
        let counter = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_counter = counter.clone();
        registry.register_native("count", "counts live calls", Vec::new(), move |_args| {
            Ok(json!({ "count": tool_counter.fetch_add(1, Ordering::SeqCst) + 1 }))
        });

        let dir = test_dir("replay");
        let worker = write_agent(
            &dir,
            "counting.ts",
            r#"
            export async function agent() {
                const msg = await chidori.signal("go");
                const { count } = await chidori.tool("count", {});
                return { count, speed: msg.payload.speed };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__");
                await chidori.actors.send(pid, "go", { speed: "fast" });
                const outcome = await chidori.actors.join(pid);
                await chidori.log("after join");
                return outcome;
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let live_ctx = RuntimeContext::new();
        let registry = Arc::new(registry);
        let live_backend = test_backend(live_ctx.clone(), registry.clone());
        let live_output = run_agent(&path, &src, &json!({}), &live_backend).unwrap();
        assert_eq!(live_output["status"], json!("completed"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let records = live_ctx.call_log().into_records();
        let replay_ctx = RuntimeContext::with_replay(records);
        let replay_backend = test_backend(replay_ctx, registry);
        let replay_output = run_agent(&path, &src, &json!({}), &replay_backend).unwrap();

        assert_eq!(live_output, replay_output);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "replay must not re-run actors"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registered_names_route_sends_and_whereis() {
        let dir = test_dir("registry");
        let worker = write_agent(
            &dir,
            "named.ts",
            r#"
            export async function agent() {
                const msg = await chidori.receive("ping");
                return { got: msg.name };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const spawned = await chidori.actors.spawn("__WORKER__", {}, { name: "greeter" });
                const found = await chidori.actors.lookup("greeter");
                const missing = await chidori.actors.lookup("nobody");
                await found.send("ping", null);
                const outcome = await chidori.actors.join("greeter");
                return {
                    samePid: spawned.pid === found.pid,
                    missing: missing === null,
                    status: outcome.status,
                    output: outcome.output,
                };
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(
            output,
            json!({
                "samePid": true,
                "missing": true,
                "status": "completed",
                "output": { "got": "ping" },
            })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn supervision_tree_merges_upward_and_routes_parent_messages_to_the_owner() {
        // A three-level tree: the run spawns a supervisor actor, which spawns
        // a child, sends it work, receives the child's "parent"-addressed
        // reply (proving "parent" routes to the OWNING ACTOR, not the run),
        // joins it, and returns. The child's records must sit inside a range
        // carved out of the supervisor's range, and the parent_seq chain must
        // step child-record → supervisor's join → run's join, so a run replay
        // absorbs the whole tree at one join.
        let dir = test_dir("tree");
        let child = write_agent(
            &dir,
            "leaf.ts",
            r#"
            export async function agent() {
                const task = await chidori.receive("task");
                await chidori.actors.send("parent", "leaf-done", { doubled: task.payload.n * 2 });
                return { ok: true };
            }
            "#,
        );
        let supervisor = write_agent(
            &dir,
            "mid.ts",
            &r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__CHILD__", {});
                await chidori.actors.send(pid, "task", { n: 21 });
                const reply = await chidori.receive("leaf-done");
                const outcome = await chidori.actors.join(pid);
                return { fromLeaf: reply.payload.doubled, leafStatus: outcome.status };
            }
            "#
            .replace("__CHILD__", &child),
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__MID__");
                const outcome = await chidori.actors.join(pid);
                return outcome.output;
            }
        "#
        .replace("__MID__", &supervisor);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(output, json!({ "fromLeaf": 42, "leafStatus": "completed" }));

        // Structure: the supervisor's range is a top-level block; the child's
        // records live in a 10^9-wide block carved out of it.
        let records = ctx.call_log().into_records();
        let run_join = records
            .iter()
            .find(|r| r.function == "join_actor" && r.parent_seq.is_none())
            .unwrap();
        let mid_join = records
            .iter()
            .find(|r| r.function == "join_actor" && r.parent_seq == Some(run_join.seq))
            .unwrap();
        let leaf_send = records
            .iter()
            .find(|r| {
                r.function == "send_actor"
                    && r.args["to"] == json!("parent")
                    && r.args["name"] == json!("leaf-done")
            })
            .unwrap();
        assert_eq!(leaf_send.parent_seq, Some(mid_join.seq));
        let top = 1_000_000_000_001..2_000_000_000_001u64;
        assert!(top.contains(&mid_join.seq), "{}", mid_join.seq);
        assert!(top.contains(&leaf_send.seq), "{}", leaf_send.seq);
        // The child block starts at the first 10^9 boundary inside the
        // supervisor's range.
        assert!(
            leaf_send.seq > 1_000_000_000_000 + 1_000_000_000,
            "{}",
            leaf_send.seq
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn supervision_tree_replays_from_cache() {
        // The whole tree — including the grandchild's live tool call — must
        // come back from the journal on a run replay.
        let counter = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_counter = counter.clone();
        registry.register_native("count", "counts live calls", Vec::new(), move |_args| {
            Ok(json!({ "count": tool_counter.fetch_add(1, Ordering::SeqCst) + 1 }))
        });

        let dir = test_dir("tree-replay");
        let child = write_agent(
            &dir,
            "leaf.ts",
            r#"
            export async function agent() {
                const { count } = await chidori.tool("count", {});
                return { count };
            }
            "#,
        );
        let supervisor = write_agent(
            &dir,
            "mid.ts",
            &r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__CHILD__");
                const outcome = await chidori.actors.join(pid);
                return outcome.output;
            }
            "#
            .replace("__CHILD__", &child),
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__MID__");
                const outcome = await chidori.actors.join(pid);
                return outcome.output;
            }
        "#
        .replace("__MID__", &supervisor);
        std::fs::write(&path, &src).unwrap();

        let live_ctx = RuntimeContext::new();
        let registry = Arc::new(registry);
        let live_backend = test_backend(live_ctx.clone(), registry.clone());
        let live_output = run_agent(&path, &src, &json!({}), &live_backend).unwrap();
        assert_eq!(live_output, json!({ "count": 1 }));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let records = live_ctx.call_log().into_records();
        let replay_ctx = RuntimeContext::with_replay(records);
        let replay_backend = test_backend(replay_ctx, registry);
        let replay_output = run_agent(&path, &src, &json!({}), &replay_backend).unwrap();
        assert_eq!(live_output, replay_output);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "a run replay must not re-run any level of the tree"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clean_restart_reaps_children_and_releases_their_names() {
        // A supervisor that spawns a NAMED child and then fails on its first
        // attempt. The clean restart discards its log, so the retry re-runs
        // the spawn live — which only works if the failed attempt's child was
        // reaped and its registered name released. The child runs once per
        // supervisor attempt.
        let attempts = Arc::new(AtomicUsize::new(0));
        let child_runs = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_attempts = attempts.clone();
        registry.register_native("attempt", "counts attempts", Vec::new(), move |_args| {
            Ok(json!({ "n": tool_attempts.fetch_add(1, Ordering::SeqCst) + 1 }))
        });
        let tool_child_runs = child_runs.clone();
        registry.register_native("child_ran", "counts child runs", Vec::new(), move |_args| {
            Ok(json!({ "n": tool_child_runs.fetch_add(1, Ordering::SeqCst) + 1 }))
        });

        let dir = test_dir("tree-clean");
        let child = write_agent(
            &dir,
            "kid.ts",
            r#"
            export async function agent() {
                await chidori.tool("child_ran", {});
                return { ok: true };
            }
            "#,
        );
        let supervisor = write_agent(
            &dir,
            "sup.ts",
            &r#"
            export async function agent() {
                const kid = await chidori.actors.spawn("__CHILD__", {}, { name: "kid" });
                const { n } = await chidori.tool("attempt", {});
                if (n < 2) throw new Error("supervisor transient failure " + n);
                const outcome = await chidori.actors.join(kid.pid);
                return { kid: outcome.status, attempt: n };
            }
            "#
            .replace("__CHILD__", &child),
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__SUP__", {}, {
                    restart: "clean",
                    maxRestarts: 2,
                });
                return await chidori.actors.join(pid);
            }
        "#
        .replace("__SUP__", &supervisor);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(registry));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["status"], json!("completed"));
        assert_eq!(
            output["output"],
            json!({ "kid": "completed", "attempt": 2 })
        );
        assert_eq!(output["restarts"], json!(1));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            child_runs.load(Ordering::SeqCst),
            2,
            "the clean restart re-spawns the child (one run per attempt)"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settling_an_actor_spawned_by_someone_else_is_rejected() {
        // The run spawns a worker X and a meddler actor, hands the meddler
        // X's pid, and the meddler tries to join it: settling folds records
        // into the caller's log, so only the spawner may do it.
        let dir = test_dir("ownership");
        let worker = write_agent(
            &dir,
            "victim.ts",
            r#"
            export async function agent() {
                await chidori.receive("finish");
                return {};
            }
            "#,
        );
        let meddler = write_agent(
            &dir,
            "meddler.ts",
            r#"
            export async function agent(input: { victim: string }) {
                return await chidori.actors.join(input.victim);
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const victim = await chidori.actors.spawn("__VICTIM__");
                const meddler = await chidori.actors.spawn("__MEDDLER__", { victim: victim.pid });
                const meddled = await chidori.actors.join(meddler.pid);
                await chidori.actors.send(victim.pid, "finish", null);
                const victimOutcome = await chidori.actors.join(victim.pid);
                return { meddler: meddled.status, error: meddled.error, victim: victimOutcome.status };
            }
        "#
        .replace("__VICTIM__", &worker)
        .replace("__MEDDLER__", &meddler);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(output["meddler"], json!("failed"));
        assert!(
            output["error"]
                .as_str()
                .unwrap()
                .contains("settled by their spawner"),
            "{}",
            output["error"]
        );
        assert_eq!(output["victim"], json!("completed"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn supervision_tree_depth_is_bounded_by_range_subdivision() {
        // Each level's range is its parent's divided by 1000; below the
        // minimum width there is nothing left to subdivide, so a
        // fourth-generation actor cannot spawn. The self-recursive module
        // stops when its spawn is refused, and the refusal bubbles up through
        // the joined outcomes.
        let dir = test_dir("tree-depth");
        let deep_path = dir.join("actors").join("deep.ts");
        let deep_src = r#"
            export async function agent(input: { depth: number }) {
                const child = await chidori.actors.spawn("__SELF__", { depth: input.depth + 1 });
                const outcome = await chidori.actors.join(child.pid);
                return { depth: input.depth, childOutcome: outcome };
            }
        "#
        .replace("__SELF__", &deep_path.to_string_lossy());
        std::fs::write(&deep_path, &deep_src).unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__DEEP__", { depth: 0 });
                const outcome = await chidori.actors.join(pid);
                return outcome.output;
            }
        "#
        .replace("__DEEP__", &deep_path.to_string_lossy());
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        // depth 0 (width 10^12) → 1 (10^9) → 2 (10^6) → 3 (10^3, cannot
        // subdivide further): depth 3's spawn fails, and the refusal bubbles
        // up through the nested joined outcomes.
        assert_eq!(output["depth"], json!(0));
        let depth1 = &output["childOutcome"];
        assert_eq!(depth1["status"], json!("completed"));
        let depth2 = &depth1["output"]["childOutcome"];
        assert_eq!(depth2["status"], json!("completed"));
        let depth3 = &depth2["output"]["childOutcome"];
        assert_eq!(depth3["status"], json!("failed"));
        assert!(
            depth3["error"]
                .as_str()
                .unwrap()
                .contains("supervision tree too deep"),
            "{}",
            depth3["error"]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn receive_timeout_resolves_to_the_sentinel() {
        let dir = test_dir("receive-timeout");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const result = await chidori.receive("never", { timeoutMs: 50 });
                return { timedOut: result.timedOut === true, name: result.name };
            }
        "#;
        std::fs::write(&path, src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, src, &json!({}), &backend).unwrap();
        assert_eq!(output, json!({ "timedOut": true, "name": "never" }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn receive_refuses_to_block_forever_with_no_live_actors() {
        let dir = test_dir("receive-forever");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                await chidori.receive("never");
                return {};
            }
        "#;
        std::fs::write(&path, src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let err = run_agent(&path, src, &json!({}), &backend).unwrap_err();
        assert!(err.to_string().contains("would block forever"), "{err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_actor_settles_a_waiting_actor_as_stopped() {
        let dir = test_dir("stop");
        let worker = write_agent(
            &dir,
            "waiter.ts",
            r#"
            export async function agent() {
                await chidori.signal("never-sent");
                return {};
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__");
                const status = await chidori.actors.status(pid);
                const outcome = await chidori.actors.stop(pid);
                return { status: outcome.status, observed: status.status };
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(output["status"], json!("stopped"));
        // The observed pre-stop status depends on how far the actor got:
        // freshly running or already parked at the listen point.
        let observed = output["observed"].as_str().unwrap();
        assert!(observed == "running" || observed == "waiting", "{observed}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn join_timeout_reports_running_then_settles() {
        let dir = test_dir("join-timeout");
        let worker = write_agent(
            &dir,
            "slow.ts",
            r#"
            export async function agent() {
                const msg = await chidori.signal("go");
                return { got: msg.name };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const { pid } = await chidori.actors.spawn("__WORKER__");
                const first = await chidori.actors.join(pid, { timeoutMs: 100 });
                await chidori.actors.send(pid, "go", null);
                const second = await chidori.actors.join(pid);
                return { first: first.status, second: second.status, output: second.output };
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(
            output,
            json!({ "first": "running", "second": "completed", "output": { "got": "go" } })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn actors_run_concurrently_with_the_parent_and_each_other() {
        // Two actors and the parent all rendezvous through real message
        // passing: each actor announces itself, waits for "go", then replies.
        // This only completes if actors run concurrently with the parent
        // (the parent is blocked in `receive` while both actors execute).
        let dir = test_dir("concurrent");
        let worker = write_agent(
            &dir,
            "peer.ts",
            r#"
            export async function agent(input: { id: number }) {
                await chidori.actors.send("parent", "ready", { id: input.id });
                const go = await chidori.receive("go");
                await chidori.actors.send("parent", "done", { id: input.id, sum: input.id + go.payload.add });
                return { id: input.id };
            }
            "#,
        );
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const a = await chidori.actors.spawn("__WORKER__", { id: 1 });
                const b = await chidori.actors.spawn("__WORKER__", { id: 2 });
                await chidori.receive("ready");
                await chidori.receive("ready");
                await chidori.actors.send(a.pid, "go", { add: 10 });
                await chidori.actors.send(b.pid, "go", { add: 20 });
                const first = await chidori.receive("done");
                const second = await chidori.receive("done");
                await chidori.actors.join(a.pid);
                await chidori.actors.join(b.pid);
                const sums = [first.payload.sum, second.payload.sum].sort((x, y) => x - y);
                return { sums };
            }
        "#
        .replace("__WORKER__", &worker);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(output, json!({ "sums": [11, 22] }));

        // Both actors' records merged into disjoint reserved ranges.
        let records = ctx.call_log().into_records();
        let sends_to_parent: Vec<u64> = records
            .iter()
            .filter(|r| r.function == "send_actor" && r.args["to"] == json!("parent"))
            .map(|r| r.seq)
            .collect();
        assert_eq!(sends_to_parent.len(), 4);
        assert!(sends_to_parent
            .iter()
            .any(|seq| (1_000_000_000_001..2_000_000_000_001u64).contains(seq)));
        assert!(sends_to_parent
            .iter()
            .any(|seq| (2_000_000_000_001..3_000_000_000_001u64).contains(seq)));

        let _ = std::fs::remove_dir_all(dir);
    }
}
