use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::call_log::{CallLog, CallRecord};
use crate::runtime::capability::{Capability, CapabilityLedger};
use crate::runtime::otel::RunSpan;
use crate::runtime::snapshot::{
    HostOperationId, HostPromiseRecord, HostPromiseTable, PendingHostOperation,
    PendingHostOperationKind, QueuedSignal, PENDING_HOST_OPERATION_FILE, SIGNAL_INBOX_FILE,
};
use crate::runtime::store::{FsRunStore, RunStore};
use crate::runtime::vfs::Vfs;

/// A streaming event the runtime emits while an agent runs. CallRecord is
/// the original per-call event; prompt stream events carry LLM output as the
/// provider streams it back. A single SSE endpoint can multiplex both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Call(CallRecord),
    PromptStart {
        stream_id: String,
        seq: u64,
        prompt_type: Option<String>,
        model: String,
    },
    PromptDelta {
        stream_id: String,
        seq: u64,
        prompt_type: Option<String>,
        delta: String,
    },
    PromptEnd {
        stream_id: String,
        seq: u64,
        prompt_type: Option<String>,
        error: Option<String>,
    },
}

/// Shared runtime context passed into TypeScript host bindings.
///
/// Holds the LLM provider registry, call log, template engine,
/// and agent-level configuration. Wrapped in Arc<Mutex<>> so
/// synchronous TypeScript host bindings can mutate it.
#[derive(Debug, Clone)]
pub struct RuntimeContext {
    inner: Arc<Mutex<RuntimeContextInner>>,
}

#[derive(Clone)]
pub struct HostOperationSafepoint(
    Arc<dyn Fn(&PendingHostOperation) -> anyhow::Result<()> + Send + Sync>,
);

#[derive(Clone)]
pub struct HostOperationCompletionSafepoint(
    Arc<dyn Fn(&HostPromiseRecord) -> anyhow::Result<()> + Send + Sync>,
);

impl std::fmt::Debug for HostOperationSafepoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostOperationSafepoint")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for HostOperationCompletionSafepoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostOperationCompletionSafepoint")
            .finish_non_exhaustive()
    }
}

impl HostOperationSafepoint {
    #[allow(dead_code)]
    pub fn new(
        callback: impl Fn(&PendingHostOperation) -> anyhow::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(callback))
    }

    fn call(&self, operation: &PendingHostOperation) -> anyhow::Result<()> {
        (self.0)(operation)
    }
}

impl HostOperationCompletionSafepoint {
    #[allow(dead_code)]
    pub fn new(
        callback: impl Fn(&HostPromiseRecord) -> anyhow::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(callback))
    }

    fn call(&self, record: &HostPromiseRecord) -> anyhow::Result<()> {
        (self.0)(record)
    }
}

/// A pre-loaded replay journal with lookup indexes built once at
/// construction. A resume calls `try_replay` (and, on every hit,
/// `absorb_replayed_subtree`) once per recorded effect, so without the
/// indexes a full resume sweep scans — and the absorb path deep-cloned —
/// the entire journal per effect: O(N²) in run history. The journal is
/// immutable after construction, so the indexes never need maintenance.
#[derive(Debug)]
struct ReplayJournal {
    records: Vec<CallRecord>,
    /// seq → index of its first record (first occurrence wins, mirroring the
    /// linear scan this replaces).
    by_seq: HashMap<u64, usize>,
    /// parent seq → indices of its direct children, in journal order.
    children: HashMap<u64, Vec<usize>>,
}

impl ReplayJournal {
    fn new(records: Vec<CallRecord>) -> Self {
        let mut by_seq = HashMap::with_capacity(records.len());
        let mut children: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, r) in records.iter().enumerate() {
            by_seq.entry(r.seq).or_insert(i);
            if let Some(parent) = r.parent_seq {
                children.entry(parent).or_default().push(i);
            }
        }
        Self {
            records,
            by_seq,
            children,
        }
    }
}

/// Outcome of a warm input pause ([`WarmInputBridge`]).
pub enum WarmInputWait {
    /// The `input()` response arrived while the VM stayed live: continue with
    /// it in place.
    Delivered(String),
    /// Park (no supervisor waiting, eviction deadline, capacity): unwind via
    /// the ordinary PAUSE_MARKER path and resume by replay later.
    Park,
}

/// Warm-resume bridge installed by the session server: when an `input()`
/// listen point in Pause mode has no cached response, the bridge surfaces the
/// pause to the server (which answers the HTTP request) and BLOCKS the engine
/// thread until the response is delivered — the continuation then costs O(1)
/// instead of an O(history) replay re-execution. The pending operation is
/// durable on disk BEFORE the bridge is consulted, so a crash while parked
/// resumes by replay exactly as an unwound pause would; `Park` (eviction,
/// capacity, shutdown) degrades to that same path at any time.
#[derive(Clone)]
pub struct WarmInputBridge(
    Arc<dyn Fn(&RuntimeContext, &PendingInput) -> WarmInputWait + Send + Sync>,
);

impl std::fmt::Debug for WarmInputBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WarmInputBridge").finish_non_exhaustive()
    }
}

impl WarmInputBridge {
    pub fn new(
        callback: impl Fn(&RuntimeContext, &PendingInput) -> WarmInputWait + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(callback))
    }

    pub fn wait(&self, ctx: &RuntimeContext, pending: &PendingInput) -> WarmInputWait {
        (self.0)(ctx, pending)
    }
}

/// Outcome of an actor's inline listen-point wait ([`ActorSignalWaiter`]).
pub enum ActorSignalWait {
    /// A matching message reached the actor's shared mailbox; the caller
    /// should pump the mailbox into the run-level inbox and retry the drain.
    Delivered,
    /// The listen point's own `timeoutMs` deadline elapsed: resolve with the
    /// timeout sentinel in place.
    TimedOut,
    /// Park the actor (stop requested or idle cap reached): fall back to the
    /// pause/hibernate path.
    Park,
}

/// Inline mailbox wait installed by the actor supervisor
/// (`host_actor::supervise`). When present, a `chidori.signal`-family listen
/// point with an empty inbox BLOCKS here for the next matching delivery and
/// resumes in place — the actor's history is not re-executed per message,
/// which is what made a signal-driven actor O(messages²) over its lifetime.
/// Stop and idle still park through the ordinary pause path (`Park`), so
/// hibernation and supervision semantics are unchanged. Args: the listen
/// names and the listen point's `timeoutMs`.
#[derive(Clone)]
pub struct ActorSignalWaiter(Arc<dyn Fn(&[String], Option<u64>) -> ActorSignalWait + Send + Sync>);

impl std::fmt::Debug for ActorSignalWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSignalWaiter").finish_non_exhaustive()
    }
}

impl ActorSignalWaiter {
    pub fn new(
        callback: impl Fn(&[String], Option<u64>) -> ActorSignalWait + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(callback))
    }

    pub fn wait(&self, names: &[String], timeout_ms: Option<u64>) -> ActorSignalWait {
        (self.0)(names, timeout_ms)
    }
}

#[derive(Debug)]
struct RuntimeContextInner {
    /// Agent-level defaults set via config().
    pub config: AgentConfig,
    /// Accumulated call log for checkpointing / tracing.
    pub call_log: CallLog,
    /// True when `call_log` holds records that never went through
    /// `RunStore::append_record` — records pushed while replaying a resume
    /// (`try_replay` / `absorb_replayed_subtree`), including the synthetic
    /// resume record. The durable safepoints write the full `checkpoint.json`
    /// only while this is set (or at explicit compaction points); live
    /// records are already durable via the O(1) journal append in
    /// `record_call`, so steady-state safepoints skip the O(history) rewrite.
    pub call_log_dirty: bool,
    /// Sequence counter for call log entries.
    pub seq: u64,
    /// Pre-loaded call log for replay mode. When set, host functions
    /// return cached results instead of executing for matching sequence numbers.
    pub replay_log: Option<ReplayJournal>,
    /// Unique identifier for this run. Used as the subdirectory name
    /// under `.chidori/runs/` when persistence is enabled.
    pub run_id: String,
    /// Directory into which the run's durable artifacts are written.
    /// None disables persistence. Kept alongside `store` because branch
    /// stores and out-of-band tooling still address artifacts by path.
    pub persist_dir: Option<PathBuf>,
    /// The persistence handle every durable write goes through. Filesystem by
    /// default (`enable_persistence`); a configured durable mirror rides along
    /// via `enable_persistence_with_store` (`docs/durable-storage.md`).
    pub store: Option<Arc<dyn RunStore>>,
    /// First persistence failure observed on this run, when the durability
    /// policy is strict. Checked before the next live host effect executes so
    /// a run cannot keep taking side effects its journal isn't recording.
    pub persist_failure: Option<String>,
    /// Whether persistence failures poison the run (`CHIDORI_DURABILITY=strict`)
    /// or are logged and tolerated (default, the pre-store behavior).
    pub strict_durability: bool,
    /// How `input()` should behave when there is no cached response:
    /// read from stdin, or pause the run and surface the prompt to the caller.
    pub input_mode: InputMode,
    /// Set by `input()` when pausing. The engine reads this after eval
    /// unwinds to distinguish a pause from a real error.
    pub pending_input: Option<PendingInput>,
    /// Set by the permission policy when an AskBefore rule needs approval.
    pub pending_approval: Option<PendingApproval>,
    /// Set by `signal()` when pausing at a listen point with an empty mailbox.
    /// The engine reads this after eval unwinds to surface a signal pause.
    pub pending_signal: Option<PendingSignal>,
    /// The `chidori.step(name, fn)` callback currently live-executing, if any.
    /// While set, all other host effects are refused (step callbacks must be
    /// pure compute — `docs/value-checkpoints.md`).
    pub active_step: Option<ActiveStep>,
    /// Durable per-run signal mailbox, loaded at run/resume start (threaded the
    /// same way `vfs` is). `take_queued_signal(name)` drains the lowest-
    /// `delivery_seq` matching entry and immediately re-persists the shrunken
    /// inbox so a crash can't double-deliver (see `docs/signals.md` §8.4/§10).
    pub signal_inbox: Vec<QueuedSignal>,
    /// Durable host-promise bookkeeping: snapshot-serializable Rust state that
    /// pairs each pending host operation id with its eventual result for the
    /// deterministic-replay journal.
    #[allow(dead_code)]
    pub host_promises: HostPromiseTable,
    /// Optional live-event sink. When set, every `record_call` is also
    /// forwarded here so the server can stream host-function calls to
    /// connected clients (e.g. over SSE). Token deltas emitted by streaming
    /// providers flow through the same channel as prompt stream events.
    pub event_sender: Option<UnboundedSender<RuntimeEvent>>,
    /// Whether `record_call` should forward Call events to `event_sender`.
    /// Parallel branch contexts disable this because their local sequence
    /// numbers are remapped when branch logs are merged into the parent.
    pub emit_call_events: bool,
    /// Optional OpenTelemetry parent span for this run. When set, every
    /// `record_call` also emits a child OTLP span with the call's timing
    /// and attributes — shipping automatically to any OTLP backend (tael,
    /// Jaeger, Honeycomb, Datadog, ...). None disables OTEL export.
    pub otel_run: Option<Arc<RunSpan>>,
    /// Optional durable safepoint invoked after a pending host operation is
    /// persisted and before the corresponding live side effect executes.
    pub host_operation_safepoint: Option<HostOperationSafepoint>,
    /// Optional durable safepoint invoked after a host operation result is
    /// persisted and recorded, before control returns to JavaScript.
    pub host_operation_completion_safepoint: Option<HostOperationCompletionSafepoint>,
    /// Optional inline mailbox wait for actor listen points (see
    /// [`ActorSignalWaiter`]); installed per supervision iteration.
    pub actor_signal_waiter: Option<ActorSignalWaiter>,
    /// Optional warm-resume bridge for `input()` pauses (see
    /// [`WarmInputBridge`]); installed by the session server's run legs.
    pub warm_input_bridge: Option<WarmInputBridge>,
    /// Optional scoped workspace root exposed through `chidori.workspace`.
    pub workspace_root: Option<PathBuf>,
    /// Seqs of host calls currently executing (their `live()` is on the
    /// stack). The top is the parent of any call recorded while it runs —
    /// this is how sub-agent calls (made inside `call_agent`'s execution)
    /// get stamped with their enclosing `call_agent`'s seq.
    pub call_stack: Vec<u64>,
    /// Accumulated capability flags raised when agent code touches a
    /// captured-effect surface (filesystem, crypto, timers). Surfaced on the
    /// snapshot manifest and as OTEL span attributes; recomputed and checked
    /// against the stored set on replay.
    pub capabilities: CapabilityLedger,
    /// In-memory, snapshot-resident virtual filesystem backing `node:fs`.
    /// Reads/writes never touch the host disk; the tree rides the snapshot
    /// manifest so it survives suspend → restore identically.
    pub vfs: Vfs,
    /// Whether this context belongs to a `chidori.branch` sub-run. Branch
    /// contexts may not fork again: a nested branch would allocate sequence
    /// ranges outside the parent branch's reserved range and break the
    /// disjointness invariant, so `run_branches` rejects it up front.
    pub is_branch: bool,
    /// Optional host-supplied model override (Pi-style save point). When set,
    /// every prompt host call (`execute_prompt_text` / `execute_prompt_response`)
    /// consults it just before sending and, if it yields `Some(model)`, swaps
    /// the request's model. This refreshes the model between provider requests
    /// for *every* execution path that runs through the prompt bindings — the
    /// native agent loop and the TypeScript interactive engine alike.
    pub model_override: Option<ModelOverride>,
    /// The run's actor hub (spawned actor sub-runs, their mailboxes, and the
    /// name registry — `docs/actors.md`). Created lazily by the first
    /// `chidori.actors.spawn`; actor sub-run contexts share the spawning run's
    /// hub so sends, receives, and lookups all address one table.
    pub actor_hub: Option<Arc<crate::runtime::host_actor::ActorHub>>,
    /// Set on actor sub-run contexts: this context's pid in the hub. Routes
    /// `chidori.receive` to the actor's own mailbox, stamps outgoing message
    /// senders, and scopes join/stop to the owning spawner.
    pub actor_id: Option<String>,
}

/// Host-supplied callback returning the current model override, or `None` to
/// leave the request's model unchanged. Wrapped like the safepoint callbacks so
/// `RuntimeContextInner` can keep deriving `Debug`.
#[derive(Clone)]
pub struct ModelOverride(Arc<dyn Fn() -> Option<String> + Send + Sync>);

impl std::fmt::Debug for ModelOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelOverride").finish_non_exhaustive()
    }
}

impl ModelOverride {
    #[allow(dead_code)] // Exercised only by tests today; the lib target sees it as dead.
    pub fn new(callback: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }

    /// Invoke the override, returning the current model (or `None`).
    pub fn resolve(&self) -> Option<String> {
        (self.0)()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Read one line from stdin and return it. Used by the CLI `run` command.
    Stdin,
    /// Record the prompt and raise a pause sentinel; the engine returns a
    /// Paused RunResult so the caller can collect the response out-of-band.
    Pause,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingInput {
    pub seq: u64,
    pub prompt: String,
    /// The artifact under review (`input()`'s `opts.details`) — surfaced to
    /// whoever answers (CLI render, server `pending_details`) so an approval
    /// gate can show what it is approving. Never part of the durable record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Set by `execute_signal` / `execute_signal_any` when a listen point has no
/// matching mailbox entry and must pause. The engine reads this after eval
/// unwinds to distinguish a signal pause (surfaced as `RunResult.paused_signal`)
/// from a real error, mirroring [`PendingInput`]. The pending host operation's
/// match key is `{ "name": name }` (or `{ "names": [...] }` for `signalAny`);
/// `id` is its host-promise id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingSignal {
    pub seq: u64,
    /// The awaited name (`signal`) or the first of the awaited names
    /// (`signalAny`). Kept as the primary name for views and messages.
    pub name: String,
    /// The full awaited name set. `[name]` for `chidori.signal(name)`; the
    /// listen set for the fan-in `chidori.signal(names[])`. A delivery matching ANY of
    /// these resolves the pause.
    #[serde(default)]
    pub names: Vec<String>,
    /// `timeoutMs` from the listen call's options, when given. The supervising
    /// server arms a timer and, on expiry, resolves the pause with the
    /// `{ timedOut: true }` sentinel instead of a delivered signal.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    pub id: HostOperationId,
}

impl PendingSignal {
    /// The awaited name set, falling back to `[name]` for values deserialized
    /// from before `names` existed.
    pub fn listen_names(&self) -> Vec<String> {
        if self.names.is_empty() {
            vec![self.name.clone()]
        } else {
            self.names.clone()
        }
    }
}

/// Set by `execute_step_begin` while a `chidori.step(name, fn)` callback is
/// live-executing in the VM. The step's call-log record is written at `seq`
/// when `execute_step_end` takes this back; while it is set, every other host
/// effect refuses to run — a step callback must be pure, synchronous
/// computation, or skipping it on replay would desynchronize the journal
/// (see `docs/value-checkpoints.md`).
#[derive(Debug, Clone)]
pub struct ActiveStep {
    pub seq: u64,
    pub name: String,
    pub started: chrono::DateTime<chrono::Utc>,
}

/// Set by the policy enforcer when a call needs user approval but the
/// engine is running in Pause mode (server context). The engine catches the
/// pause sentinel, takes this value, and returns it in `RunResult` so the
/// HTTP layer can render an approval UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingApproval {
    pub target: String,
    pub args: serde_json::Value,
    pub reason: Option<String>,
}

/// Marker text used to tag the pause sentinel error so the engine can
/// distinguish it from a genuine failure. Lives in [`crate::runtime::errors`]
/// with the typed [`crate::runtime::errors::RunInterrupt`] taxonomy;
/// re-exported here for the historical import path.
#[allow(unused_imports)]
// Historical-path re-export; the bin target compiles the module tree separately and never uses it.
pub use crate::runtime::errors::PAUSE_MARKER;

/// True when `CHIDORI_REPLAY_LAX=1`: argument-level replay divergences are
/// downgraded to warnings that restore the historical best-effort behavior
/// (serve the cached result on the sync path, silently re-execute live on the
/// async host-operation path). Function-name and operation-kind mismatches
/// remain fatal regardless.
pub(crate) fn replay_lax() -> bool {
    std::env::var("CHIDORI_REPLAY_LAX").ok().as_deref() == Some("1")
}

/// Render call args for a divergence message without flooding the error with
/// a multi-kilobyte prompt payload.
/// Name the top-level keys that actually differ between a recorded call's
/// args and the args the agent is calling with now — "the agent code changed"
/// is the wrong diagnosis when the only drift is configuration (e.g. resuming
/// under a different default model), so the error should say which knob moved.
pub(crate) fn describe_args_divergence(
    recorded: &serde_json::Value,
    current: &serde_json::Value,
) -> String {
    let (Some(recorded), Some(current)) = (recorded.as_object(), current.as_object()) else {
        return String::new();
    };
    let mut keys: Vec<&String> = recorded
        .keys()
        .chain(current.keys())
        .filter(|k| *k != "request_digest" && recorded.get(*k) != current.get(*k))
        .collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return String::new();
    }
    let mut description = format!(
        " Differing field(s): {}.",
        keys.iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if keys.iter().any(|k| *k == "model") {
        let recorded_model = recorded
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unset>");
        let current_model = current
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unset>");
        description.push_str(&format!(
            " The run was recorded with model `{recorded_model}` but is replaying under \
             `{current_model}` — a configuration mismatch, not a code change; pass \
             `--model {recorded_model}` (or leave `--model`/CHIDORI_MODEL unset so the \
             model recorded in the run's manifest applies)."
        ));
    }
    description
}

pub(crate) fn truncate_json_for_error(value: &serde_json::Value) -> String {
    let s = serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string());
    if s.chars().count() > 300 {
        let mut t: String = s.chars().take(300).collect();
        t.push('…');
        t
    } else {
        s
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub temperature: f64,
    pub max_tokens: u64,
    pub max_turns: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        let model = std::env::var("CHIDORI_MODEL")
            .or_else(|_| std::env::var("ANTHROPIC_MODEL"))
            .unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
        Self {
            model,
            temperature: 0.7,
            max_tokens: 4096,
            max_turns: 10,
        }
    }
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeContext {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: AgentConfig::default(),
                call_log: CallLog::new(),
                call_log_dirty: false,
                seq: 0,
                replay_log: None,
                run_id: uuid::Uuid::new_v4().to_string(),
                persist_dir: None,
                store: None,
                persist_failure: None,
                strict_durability: crate::runtime::store::strict_durability(),
                input_mode: InputMode::Stdin,
                pending_input: None,
                pending_approval: None,
                pending_signal: None,
                active_step: None,
                signal_inbox: Vec::new(),
                host_promises: HostPromiseTable::new(),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                actor_signal_waiter: None,
                warm_input_bridge: None,
                workspace_root: default_workspace_root(),
                call_stack: Vec::new(),
                capabilities: CapabilityLedger::new(),
                vfs: vfs_from_seed_env(),
                is_branch: false,
                model_override: None,
                actor_hub: None,
                actor_id: None,
            })),
        }
    }

    /// Create a context in replay mode with a pre-loaded call log.
    /// Host functions will return cached results for matching calls.
    pub fn with_replay(replay_log: Vec<CallRecord>) -> Self {
        Self::with_replay_and_host_promises(replay_log, Vec::new())
    }

    pub fn with_replay_and_host_promises(
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
    ) -> Self {
        Self::with_replay_host_promises_and_vfs(replay_log, host_promises, vfs_from_seed_env())
    }

    /// As `with_replay_and_host_promises`, but restores a virtual filesystem
    /// captured in a snapshot manifest so a resumed run sees the exact file
    /// state it had at suspend (rather than re-seeding from the environment).
    pub fn with_replay_host_promises_and_vfs(
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: Vfs,
    ) -> Self {
        Self::with_replay_host_promises_vfs_and_signals(replay_log, host_promises, vfs, Vec::new())
    }

    /// As `with_replay_host_promises_and_vfs`, but also threads an initial signal
    /// mailbox (`docs/signals.md` §8.4) loaded from the run's durable
    /// `signals/inbox.json`, the same way `vfs` is restored. A signal that was
    /// enqueued before the agent reached a matching `chidori.signal(name)` listen
    /// point is drained from this inbox on (re)run. The existing constructor
    /// variants delegate here with an empty inbox so their signatures stay
    /// unchanged.
    pub fn with_replay_host_promises_vfs_and_signals(
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: Vfs,
        signal_inbox: Vec<QueuedSignal>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: AgentConfig::default(),
                call_log: CallLog::new(),
                call_log_dirty: false,
                seq: 0,
                replay_log: Some(ReplayJournal::new(replay_log)),
                run_id: uuid::Uuid::new_v4().to_string(),
                persist_dir: None,
                store: None,
                persist_failure: None,
                strict_durability: crate::runtime::store::strict_durability(),
                input_mode: InputMode::Stdin,
                pending_input: None,
                pending_approval: None,
                pending_signal: None,
                active_step: None,
                signal_inbox,
                host_promises: HostPromiseTable::from_records(host_promises),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                actor_signal_waiter: None,
                warm_input_bridge: None,
                workspace_root: default_workspace_root(),
                call_stack: Vec::new(),
                capabilities: CapabilityLedger::new(),
                vfs,
                is_branch: false,
                model_override: None,
                actor_hub: None,
                actor_id: None,
            })),
        }
    }

    #[allow(dead_code)]
    pub fn with_existing_call_log(run_id: String, records: Vec<CallRecord>) -> Self {
        let seq = records.iter().map(|record| record.seq).max().unwrap_or(0);
        let mut call_log = CallLog::new();
        for record in records {
            call_log.push(record);
        }
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: AgentConfig::default(),
                call_log,
                // Seeded records never went through this context's store
                // handle; the first checkpoint write must include them.
                call_log_dirty: true,
                seq,
                replay_log: None,
                run_id,
                persist_dir: None,
                store: None,
                persist_failure: None,
                strict_durability: crate::runtime::store::strict_durability(),
                input_mode: InputMode::Stdin,
                pending_input: None,
                pending_approval: None,
                pending_signal: None,
                active_step: None,
                signal_inbox: Vec::new(),
                host_promises: HostPromiseTable::new(),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                actor_signal_waiter: None,
                warm_input_bridge: None,
                workspace_root: default_workspace_root(),
                call_stack: Vec::new(),
                capabilities: CapabilityLedger::new(),
                vfs: vfs_from_seed_env(),
                is_branch: false,
                model_override: None,
                actor_hub: None,
                actor_id: None,
            })),
        }
    }

    /// Build the context for one `chidori.branch` sub-run, anchored to the
    /// parent's state (`docs/branching-execution.md` §8.3): the parent's config,
    /// VFS snapshot, input mode, workspace root, model override, streaming event
    /// sink, and OTEL run span are inherited; the call log is fresh and the
    /// sequence counter starts at `base_seq` (the branch's reserved
    /// `CallLogSequenceRange` start minus one, so its records stay disjoint from
    /// the parent and sibling branches). The call stack is seeded with
    /// `parent_branch_seq` — the parent's `branch` call — so every top-level
    /// record the branch makes carries `parent_seq = branch seq` and the shared
    /// OTEL span tree nests the branch's subtree under the `branch` span.
    pub fn for_branch(
        parent: &RuntimeContext,
        run_id: String,
        base_seq: u64,
        parent_branch_seq: u64,
    ) -> Self {
        let parent_inner = parent.inner.lock().unwrap();
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: parent_inner.config.clone(),
                call_log: CallLog::new(),
                call_log_dirty: false,
                seq: base_seq,
                replay_log: None,
                run_id,
                persist_dir: None,
                store: None,
                persist_failure: None,
                strict_durability: crate::runtime::store::strict_durability(),
                input_mode: parent_inner.input_mode,
                pending_input: None,
                pending_approval: None,
                pending_signal: None,
                active_step: None,
                signal_inbox: Vec::new(),
                host_promises: HostPromiseTable::new(),
                event_sender: parent_inner.event_sender.clone(),
                emit_call_events: parent_inner.emit_call_events,
                otel_run: parent_inner.otel_run.clone(),
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                actor_signal_waiter: None,
                warm_input_bridge: None,
                workspace_root: parent_inner.workspace_root.clone(),
                call_stack: vec![parent_branch_seq],
                capabilities: CapabilityLedger::new(),
                vfs: parent_inner.vfs.clone(),
                is_branch: true,
                model_override: parent_inner.model_override.clone(),
                actor_hub: None,
                actor_id: None,
            })),
        }
    }

    /// Rebuild the context for one persisted `chidori.branch` sub-run resumed
    /// or re-run **out-of-band** — after the parent run has moved on (or its
    /// process exited), so there is no live parent context to inherit from.
    /// The anchor state comes from the branch store instead: the fork-time VFS
    /// snapshot, the branch's reserved base sequence, and the parent `branch`
    /// call's seq for call-stack seeding (so re-recorded records keep the same
    /// parentage the original run stamped). `replay_log` carries the branch's
    /// recorded records (plus a synthetic `input` record when resuming a
    /// pause); pass an empty log for a fresh edit-and-rerun from the anchor.
    /// Input mode is `Pause` so an unanswered later `input()` pauses again
    /// rather than reading stdin.
    pub fn for_branch_resume(
        replay_log: Vec<CallRecord>,
        vfs: Vfs,
        base_seq: u64,
        parent_branch_seq: u64,
        run_id: String,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: AgentConfig::default(),
                call_log: CallLog::new(),
                call_log_dirty: false,
                seq: base_seq,
                replay_log: Some(ReplayJournal::new(replay_log)),
                run_id,
                persist_dir: None,
                store: None,
                persist_failure: None,
                strict_durability: crate::runtime::store::strict_durability(),
                input_mode: InputMode::Pause,
                pending_input: None,
                pending_approval: None,
                pending_signal: None,
                active_step: None,
                signal_inbox: Vec::new(),
                host_promises: HostPromiseTable::new(),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                actor_signal_waiter: None,
                warm_input_bridge: None,
                workspace_root: default_workspace_root(),
                call_stack: vec![parent_branch_seq],
                capabilities: CapabilityLedger::new(),
                vfs,
                is_branch: true,
                model_override: None,
                actor_hub: None,
                actor_id: None,
            })),
        }
    }

    /// Build the context for one iteration of a spawned actor sub-run
    /// (`docs/actors.md`). Like [`for_branch`](Self::for_branch), the parent's
    /// config, workspace root, model override, streaming sink, and OTEL run are
    /// inherited and the sequence counter starts at `base_seq` (the actor's
    /// reserved range start minus one). Unlike a branch:
    /// - `replay_log` carries the actor's own accumulated records, because the
    ///   actor's supervision loop re-enters the source module on every message
    ///   wait and restart (the standard resume-by-replay model);
    /// - `signal_inbox` carries the actor mailbox entries left unconsumed by
    ///   the previous iteration;
    /// - the parent's `actor_hub` is shared, so the actor can send to siblings
    ///   and to the parent;
    /// - the call stack starts empty: top-level records are stamped with the
    ///   parent's `join_actor` seq when they merge, not at record time (the
    ///   join seq is unknown while the actor runs).
    ///
    /// Input mode is `Pause` so a `chidori.input()` inside an actor surfaces as
    /// a paused outcome instead of reading stdin on a background thread.
    #[allow(clippy::too_many_arguments)]
    pub fn for_actor(
        parent: &RuntimeContext,
        actor_id: String,
        base_seq: u64,
        replay_log: Vec<CallRecord>,
        vfs: Vfs,
        signal_inbox: Vec<QueuedSignal>,
        hub: Arc<crate::runtime::host_actor::ActorHub>,
    ) -> Self {
        let parent_inner = parent.inner.lock().unwrap();
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: parent_inner.config.clone(),
                call_log: CallLog::new(),
                call_log_dirty: false,
                seq: base_seq,
                replay_log: Some(ReplayJournal::new(replay_log)),
                run_id: actor_id.clone(),
                persist_dir: None,
                store: None,
                persist_failure: None,
                strict_durability: crate::runtime::store::strict_durability(),
                input_mode: InputMode::Pause,
                pending_input: None,
                pending_approval: None,
                pending_signal: None,
                active_step: None,
                signal_inbox,
                host_promises: HostPromiseTable::new(),
                event_sender: parent_inner.event_sender.clone(),
                emit_call_events: parent_inner.emit_call_events,
                otel_run: parent_inner.otel_run.clone(),
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                actor_signal_waiter: None,
                warm_input_bridge: None,
                workspace_root: parent_inner.workspace_root.clone(),
                call_stack: Vec::new(),
                capabilities: CapabilityLedger::new(),
                vfs,
                is_branch: false,
                model_override: parent_inner.model_override.clone(),
                actor_hub: Some(hub),
                actor_id: Some(actor_id),
            })),
        }
    }

    /// Whether this context belongs to a `chidori.branch` sub-run.
    pub fn is_branch(&self) -> bool {
        self.inner.lock().unwrap().is_branch
    }

    /// The pid of the actor sub-run this context belongs to, if any.
    pub fn actor_id(&self) -> Option<String> {
        self.inner.lock().unwrap().actor_id.clone()
    }

    /// The run's actor hub, if one has been created.
    pub fn actor_hub(&self) -> Option<Arc<crate::runtime::host_actor::ActorHub>> {
        self.inner.lock().unwrap().actor_hub.clone()
    }

    /// The run's actor hub, created on first use (the first
    /// `chidori.actors.spawn` in the run).
    pub fn ensure_actor_hub(&self) -> Arc<crate::runtime::host_actor::ActorHub> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .actor_hub
            .get_or_insert_with(|| Arc::new(crate::runtime::host_actor::ActorHub::new()))
            .clone()
    }

    /// The run's persistence directory, when persistence is enabled. Branch
    /// sub-runs persist their stores under `<persist_dir>/branches/`.
    pub fn persist_dir(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().persist_dir.clone()
    }

    /// Fold a completed branch sub-run's records into this (parent) log without
    /// re-emitting stream events or OTEL spans — the branch context already
    /// emitted them live. Advances the sequence counter past every merged seq,
    /// mirroring what `absorb_replayed_subtree` does for the same subtree on
    /// replay, so live and replayed runs agree on the next sequence number after
    /// a `branch` op. Persists the checkpoint once at the end.
    pub fn merge_branch_records(&self, records: Vec<CallRecord>) {
        let mut inner = self.inner.lock().unwrap();
        for record in records {
            inner.seq = inner.seq.max(record.seq);
            inner.call_log.push(record);
        }
        // Merged branch records bypass `record_call`'s journal append, so the
        // log is checkpoint-dirty until the full write below (or, when this
        // context has no store, a later safepoint) covers them.
        inner.call_log_dirty = true;
        if let Some(store) = inner.store.clone() {
            let result = store.write_call_log(inner.call_log.records());
            if result.is_ok() {
                inner.call_log_dirty = false;
            }
            note_persist_result(&mut inner, result);
        }
    }

    /// Enable persistence with the default filesystem layout. Each
    /// `record_call` appends the record to `<base_dir>/<run_id>/records.jsonl`;
    /// the full `checkpoint.json` is rewritten only at compaction points (run
    /// start, pause, settle) or when the log is checkpoint-dirty. Returns the
    /// run directory path.
    #[allow(dead_code)] // Exercised only by tests today; the lib target sees it as dead.
    pub fn enable_persistence(&self, base_dir: PathBuf) -> PathBuf {
        let run_dir = base_dir.join(self.run_id());
        let _ = std::fs::create_dir_all(&run_dir);
        self.enable_persistence_with_store(run_dir.clone(), Arc::new(FsRunStore::new(&run_dir)));
        run_dir
    }

    /// Enable persistence through an explicit [`RunStore`] handle (e.g. the
    /// filesystem teed with a durable SQLite/HTTP mirror). `run_dir` remains
    /// the filesystem address of the run's artifacts for path-based consumers.
    pub fn enable_persistence_with_store(&self, run_dir: PathBuf, store: Arc<dyn RunStore>) {
        let _ = std::fs::create_dir_all(&run_dir);
        let mut inner = self.inner.lock().unwrap();
        inner.persist_dir = Some(run_dir);
        inner.store = Some(store);
    }

    /// The run's persistence handle, when persistence is enabled.
    pub fn store(&self) -> Option<Arc<dyn RunStore>> {
        self.inner.lock().unwrap().store.clone()
    }

    /// The first persistence failure recorded under strict durability, if any.
    /// Host dispatch checks this before executing a live effect so a run whose
    /// journal writes are failing stops taking side effects.
    pub fn persist_failure(&self) -> Option<String> {
        self.inner.lock().unwrap().persist_failure.clone()
    }

    /// Durability barrier for the run's store: every buffered write is durable
    /// when this returns. Called by the engine before a run settles or pauses
    /// (the output-gate point). Surfaces a strict-mode persistence failure.
    pub fn flush_store(&self) -> anyhow::Result<()> {
        let (store, failure) = {
            let inner = self.inner.lock().unwrap();
            (inner.store.clone(), inner.persist_failure.clone())
        };
        if let Some(failure) = failure {
            anyhow::bail!("durable journal write failed: {failure}");
        }
        if let Some(store) = store {
            store.flush()?;
        }
        Ok(())
    }

    /// Override the run id. Used by the server's replay-based resume to keep the
    /// resumed run under the original run id (so persisted checkpoint/manifest
    /// files stay in the same run directory and the durable run is continuous),
    /// matching the live-VM resume path. Must be called before persistence is
    /// enabled, since `enable_persistence` derives the run directory from it.
    pub fn set_run_id(&self, run_id: String) {
        self.inner.lock().unwrap().run_id = run_id;
    }

    pub fn run_id(&self) -> String {
        self.inner.lock().unwrap().run_id.clone()
    }

    pub fn config(&self) -> AgentConfig {
        self.inner.lock().unwrap().config.clone()
    }

    /// Override the default model for prompts that don't set one in code —
    /// used to make a run's recorded model travel with it (manifest on
    /// resume, descriptor on a detached-agent wake) instead of being
    /// re-derived from whatever environment happens to host the wake.
    pub fn set_default_model(&self, model: String) {
        self.inner.lock().unwrap().config.model = model;
    }

    pub fn next_seq(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        inner.seq
    }

    /// The current sequence number without advancing it. Used to stamp a
    /// capability flag's first-touch seq for inline (non-call-logged) effects
    /// like hashing, which must not consume a sequence slot.
    pub fn current_seq(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    /// Mark `seq`'s `live()` as executing: any call recorded until the
    /// matching [`exit_call`](Self::exit_call) nests under it. Paired around
    /// the execution of host calls that can contain other calls (`call_agent`).
    pub fn enter_call(&self, seq: u64) {
        self.inner.lock().unwrap().call_stack.push(seq);
    }

    /// Pop the innermost executing call. Pops `seq` defensively in case an
    /// inner call unwound without its own `exit_call`.
    pub fn exit_call(&self, seq: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(pos) = inner.call_stack.iter().rposition(|&s| s == seq) {
            inner.call_stack.truncate(pos);
        }
    }

    /// Check if there is a cached result for the given sequence number.
    /// If so, return it (and record the replayed call in the new log).
    /// If not, return None — the host function should execute normally.
    /// Whether the NEXT durable call will be served from the replay journal.
    /// A replayed call executes no side effect — its recorded result returns
    /// from cache (or the divergence check refuses loudly) — so policy gates,
    /// which exist to guard live effects, skip asking for it.
    pub fn next_call_is_replayed(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        let next = inner.seq + 1;
        inner
            .replay_log
            .as_ref()
            .is_some_and(|journal| journal.by_seq.contains_key(&next))
    }

    pub fn try_replay(&self, seq: u64) -> Option<CallRecord> {
        let mut inner = self.inner.lock().unwrap();
        let mut record = {
            let journal = inner.replay_log.as_ref()?;
            let &i = journal.by_seq.get(&seq)?;
            journal.records[i].clone()
        };
        // Nest a replayed call under the call currently executing, exactly
        // as `record_call` does for live calls. Without this a replayed
        // *nested* host call — e.g. a tool's inner `input`, injected as a
        // synthetic resume record with no parent — would record at top
        // level, so a later resume's `absorb_replayed_subtree` wouldn't
        // recognize it as part of the container's subtree and the
        // sequence counter would collide with the next call (replay
        // divergence on a second suspension).
        if record.parent_seq.is_none() {
            record.parent_seq = inner.call_stack.last().copied();
        }
        // Record the replayed call in the new call log. Replay bypasses
        // `record_call`'s journal append (the record is already durable from
        // the turn that ran it live), so mark the log checkpoint-dirty: the
        // first safepoint past the replay frontier writes one full checkpoint
        // covering the replayed prefix plus any synthetic resume records.
        inner.call_log.push(record.clone());
        inner.call_log_dirty = true;
        Some(record)
    }

    /// Replay-cache lookup with divergence check. Returns:
    ///   Ok(Some(record)) — cached, and the cached record's function name AND
    ///                      arguments match the call the agent is making now.
    ///                      Safe to use.
    ///   Ok(None)         — no cache hit; caller should execute live.
    ///   Err(msg)         — cached, but the recorded call differs from what
    ///                      the agent is calling now. The agent code (or its
    ///                      inputs) changed since the checkpoint was saved.
    ///                      The engine should abort the replay with a clear
    ///                      error rather than pair cached results with
    ///                      different code.
    ///
    /// A name mismatch is always fatal. An argument mismatch (same function,
    /// different args — compared with the derived `request_digest` field
    /// stripped, like the async host-operation path) is fatal by default and
    /// downgraded to a warning under `CHIDORI_REPLAY_LAX=1`, which restores
    /// the historical best-effort behavior of serving the cached result.
    pub fn try_replay_checked(
        &self,
        seq: u64,
        expected_fn: &str,
        expected_args: &serde_json::Value,
    ) -> Result<Option<CallRecord>, String> {
        match self.try_replay(seq) {
            None => Ok(None),
            Some(record) if record.function != expected_fn => Err(format!(
                "Replay divergence at seq {}: checkpoint has `{}` but agent called `{}`. \
                 The agent code changed since the checkpoint was saved — \
                 re-run without replay to regenerate.",
                seq, record.function, expected_fn
            )),
            Some(record)
                if !crate::runtime::snapshot::completed_args_match(&record.args, expected_args) =>
            {
                if replay_lax() {
                    tracing::warn!(
                        "replay divergence at seq {seq} tolerated (CHIDORI_REPLAY_LAX=1): \
                         `{expected_fn}` was recorded with different arguments; \
                         serving the cached result"
                    );
                    return Ok(Some(record));
                }
                Err(format!(
                    "Replay divergence at seq {}: `{}` was recorded with arguments {} but the \
                     agent now calls it with {}.{} The agent code (or its inputs/configuration) \
                     changed since the checkpoint was saved — re-run without replay to \
                     regenerate, or set CHIDORI_REPLAY_LAX=1 to tolerate argument drift and \
                     serve cached results.",
                    seq,
                    expected_fn,
                    truncate_json_for_error(&record.args),
                    truncate_json_for_error(expected_args),
                    describe_args_divergence(&record.args, expected_args)
                ))
            }
            Some(record) => Ok(Some(record)),
        }
    }

    /// Reconcile the sequence counter (and active log) with the nested host
    /// calls a just-replayed call made during recording.
    ///
    /// Replay short-circuits a cached call by returning its stored result
    /// WITHOUT re-running `live()`. For a leaf call that's exactly right. But a
    /// *container* call — a `tool` or `call_agent` whose body itself made host
    /// calls — consumed extra sequence numbers for those nested calls when it
    /// was first recorded. Because `live()` is skipped on replay, those numbers
    /// are never re-consumed, so without this the next outer call would collide
    /// with a nested record's seq and the replay would diverge.
    ///
    /// We walk the `parent_seq` tree to find every descendant of `root_seq`,
    /// preserve those nested records in the active log (so the replayed trace
    /// keeps the same shape), and advance the counter past the maximum seq the
    /// subtree used. A leaf call has no descendants, making this a no-op — so it
    /// is safe to call unconditionally on every replay hit.
    pub fn absorb_replayed_subtree(&self, root_seq: u64) {
        let mut inner = self.inner.lock().unwrap();
        // Borrow the journal in place (field-disjoint from the fields
        // mutated below): this runs on EVERY replay hit, and a deep clone of
        // the whole history made each resume re-execution O(history²).
        let inner = &mut *inner;
        let Some(journal) = &inner.replay_log else {
            return;
        };
        // Transitive descendants of `root_seq` via the prebuilt children map.
        let mut subtree: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        subtree.insert(root_seq);
        let mut queue: Vec<u64> = vec![root_seq];
        while let Some(parent) = queue.pop() {
            for &i in journal.children.get(&parent).into_iter().flatten() {
                let seq = journal.records[i].seq;
                if subtree.insert(seq) {
                    queue.push(seq);
                }
            }
        }
        let mut max_seq = root_seq;
        if subtree.len() > 1 {
            // A container call: keep its nested records in the replayed trace,
            // in journal order, and advance the counter past them so the next
            // outer call can't reuse their seqs. Same checkpoint-dirty
            // contract as `try_replay`: these pushes bypass the journal
            // append.
            for r in &journal.records {
                if r.seq != root_seq && subtree.contains(&r.seq) {
                    inner.call_log.push(r.clone());
                    inner.call_log_dirty = true;
                    max_seq = max_seq.max(r.seq);
                }
            }
        }
        inner.seq = inner.seq.max(max_seq);
    }

    pub fn record_call(&self, mut record: CallRecord) {
        let mut inner = self.inner.lock().unwrap();
        // Stamp the enclosing call (the live-call stack top) as the parent,
        // unless the record already carries one — replayed records keep the
        // parentage serialized in their checkpoint. The call being recorded
        // has already popped itself off the stack, so the top is its parent.
        if record.parent_seq.is_none() {
            record.parent_seq = inner.call_stack.last().copied();
        }
        inner.seq = inner.seq.max(record.seq);
        let store = inner.store.clone();
        let otel = inner.otel_run.clone();
        let event_tx = if inner.emit_call_events {
            inner.event_sender.clone()
        } else {
            None
        };
        // A record's args/result can be large (a whole LLM response), so the
        // record MOVES into the call log; only a consumer outside the lock —
        // the journal append, the event stream, OTEL — forces one copy.
        let record = if store.is_some() || otel.is_some() || event_tx.is_some() {
            let copy = record.clone();
            inner.call_log.push(record);
            copy
        } else {
            inner.call_log.push(record);
            return;
        };
        drop(inner);
        // O(1) append to the journal (`records.jsonl` + any durable mirror),
        // OUTSIDE the context lock: under strict durability this write fsyncs,
        // and a configured mirror adds a network round-trip — holding the lock
        // across it would stall every other runtime-context access for the
        // duration of the I/O. Concurrent appends may land out of seq order in
        // the file; the loader's union sorts the tail by seq, so order on disk
        // is not load-bearing. The append still happens BEFORE the record is
        // surfaced on the event stream below, preserving the
        // durable-before-visible ordering.
        if let Some(store) = store {
            let result = store.append_record(&record);
            if result.is_err() {
                note_persist_result(&mut self.inner.lock().unwrap(), result);
            }
        }
        // Stream this call's OTEL span now (buffered until its parent span
        // exists), so spans ship incrementally during the run rather than as one
        // tree at the end. Only live-executed calls reach `record_call`; replayed
        // calls (try_replay / absorb_replayed_subtree) don't re-emit. The
        // `RuntimeEvent::Call` stream below is the other real-time surface.
        if let Some(tx) = event_tx {
            let _ = tx.send(RuntimeEvent::Call(record.clone()));
        }
        if let Some(otel) = otel {
            otel.stream_record(record);
        }
    }

    /// Raise a capability flag for a captured-effect surface the agent touched.
    /// Idempotent per capability — only the first touch records its `seq`. When
    /// a flag is newly raised and OTEL is active, it's also stamped on the run
    /// span so traces advertise the surface.
    pub fn note_capability(&self, cap: Capability, seq: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.capabilities.note(cap, seq) {
            if let Some(ref otel) = inner.otel_run {
                otel.record_capability(cap);
            }
        }
    }

    /// Snapshot of the capabilities touched so far, for the manifest / status.
    pub fn capabilities(&self) -> CapabilityLedger {
        self.inner.lock().unwrap().capabilities.clone()
    }

    /// A clone of the current virtual filesystem, for persisting into the
    /// snapshot manifest. Restoration on resume happens via
    /// [`RuntimeContext::with_replay_host_promises_and_vfs`].
    pub fn vfs_snapshot(&self) -> Vfs {
        self.inner.lock().unwrap().vfs.clone()
    }

    // --- Virtual filesystem operations -------------------------------------
    //
    // VFS state rides the snapshot, so these are *not* call-logged: a restore
    // reconstructs the tree directly. Each operation raises its capability flag
    // for visibility. The logical mtime stamped on writes is the current
    // sequence number, keeping `stat` times deterministic without consuming a
    // sequence slot (which would risk replay misalignment with the call log).

    /// Read a file's raw bytes from the VFS. Raises `FsRead`.
    pub fn vfs_read(&self, path: &str) -> Result<Vec<u8>, String> {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.read(path);
        note_cap(&mut inner, Capability::FsRead);
        res
    }

    /// Write bytes to the VFS (create or truncate). Raises `FsWrite`.
    pub fn vfs_write(&self, path: &str, bytes: Vec<u8>) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.seq;
        let res = inner.vfs.write(path, bytes, seq);
        note_cap(&mut inner, Capability::FsWrite);
        res
    }

    /// Append bytes to a VFS file (create if absent). Raises `FsWrite`.
    pub fn vfs_append(&self, path: &str, extra: &[u8]) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.seq;
        let res = inner.vfs.append(path, extra, seq);
        note_cap(&mut inner, Capability::FsWrite);
        res
    }

    /// Create a directory in the VFS. Raises `FsWrite`.
    pub fn vfs_mkdir(&self, path: &str, recursive: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.mkdir(path, recursive);
        note_cap(&mut inner, Capability::FsWrite);
        res
    }

    /// List a directory's immediate children. Raises `FsRead`.
    pub fn vfs_readdir(&self, path: &str) -> Result<Vec<String>, String> {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.readdir(path);
        note_cap(&mut inner, Capability::FsRead);
        res
    }

    /// Remove a path from the VFS. Raises `FsDelete`.
    pub fn vfs_remove(&self, path: &str, recursive: bool, force: bool) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.remove(path, recursive, force);
        note_cap(&mut inner, Capability::FsDelete);
        res
    }

    /// Rename/move a VFS path. Raises `FsWrite`.
    pub fn vfs_rename(&self, from: &str, to: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.rename(from, to);
        note_cap(&mut inner, Capability::FsWrite);
        res
    }

    /// Whether a path exists in the VFS. Raises `FsRead`.
    pub fn vfs_exists(&self, path: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.exists(path);
        note_cap(&mut inner, Capability::FsRead);
        res
    }

    /// `stat`-style metadata for a VFS path. Raises `FsRead`.
    pub fn vfs_stat(&self, path: &str) -> Result<serde_json::Value, String> {
        let mut inner = self.inner.lock().unwrap();
        let res = inner.vfs.stat(path);
        note_cap(&mut inner, Capability::FsRead);
        res
    }

    pub fn begin_prompt_stream(
        &self,
        seq: u64,
        prompt_type: Option<String>,
        model: String,
    ) -> Option<String> {
        let tx = self.inner.lock().unwrap().event_sender.clone()?;
        let stream_id = uuid::Uuid::new_v4().to_string();
        let _ = tx.send(RuntimeEvent::PromptStart {
            stream_id: stream_id.clone(),
            seq,
            prompt_type,
            model,
        });
        Some(stream_id)
    }

    /// Emit a streaming token fragment for a prompt stream. Used by prompt()
    /// when the provider supports incremental decoding. Ignored if no event
    /// sender is attached to the context.
    pub fn emit_prompt_delta(
        &self,
        stream_id: String,
        seq: u64,
        prompt_type: Option<String>,
        delta: String,
    ) {
        let inner = self.inner.lock().unwrap();
        if let Some(ref tx) = inner.event_sender {
            let _ = tx.send(RuntimeEvent::PromptDelta {
                stream_id,
                seq,
                prompt_type,
                delta,
            });
        }
    }

    pub fn end_prompt_stream(
        &self,
        stream_id: String,
        seq: u64,
        prompt_type: Option<String>,
        error: Option<String>,
    ) {
        let inner = self.inner.lock().unwrap();
        if let Some(ref tx) = inner.event_sender {
            let _ = tx.send(RuntimeEvent::PromptEnd {
                stream_id,
                seq,
                prompt_type,
                error,
            });
        }
    }

    pub fn has_event_sender(&self) -> bool {
        self.inner.lock().unwrap().event_sender.is_some()
    }

    pub fn set_event_sender(&self, tx: UnboundedSender<RuntimeEvent>) {
        self.inner.lock().unwrap().event_sender = Some(tx);
    }

    /// Drop the streaming event sender, closing the channel once all other
    /// clones are gone. Used by the rust-engine path after a run completes:
    /// the chidori-js VM can leak its heap on drop (Rc cycles, no cycle
    /// collector), which would otherwise keep the dispatch closure — and through
    /// it this context and the sender — alive, hanging a drain loop that waits
    /// for the channel to close. No-op when streaming isn't in use.
    pub fn clear_event_sender(&self) {
        self.inner.lock().unwrap().event_sender = None;
    }

    /// Install a model-override hook consulted before every prompt host call,
    /// so a mid-run model change refreshes the model on the next provider
    /// request across all execution paths.
    #[allow(dead_code)] // Exercised only by tests today; the lib target sees it as dead.
    pub fn set_model_override(&self, model_override: ModelOverride) {
        self.inner.lock().unwrap().model_override = Some(model_override);
    }

    /// Resolve the current model override, if a hook is installed and yields one.
    pub fn resolve_model_override(&self) -> Option<String> {
        let hook = self.inner.lock().unwrap().model_override.clone();
        hook.and_then(|hook| hook.resolve())
    }

    pub fn set_otel_run(&self, run: Arc<RunSpan>) {
        self.inner.lock().unwrap().otel_run = Some(run);
    }

    pub fn otel_run(&self) -> Option<Arc<RunSpan>> {
        self.inner.lock().unwrap().otel_run.clone()
    }

    #[allow(dead_code)]
    pub fn set_host_operation_safepoint(&self, safepoint: HostOperationSafepoint) {
        self.inner.lock().unwrap().host_operation_safepoint = Some(safepoint);
    }

    #[allow(dead_code)]
    pub fn set_warm_input_bridge(&self, bridge: WarmInputBridge) {
        self.inner.lock().unwrap().warm_input_bridge = Some(bridge);
    }

    pub fn warm_input_bridge(&self) -> Option<WarmInputBridge> {
        self.inner.lock().unwrap().warm_input_bridge.clone()
    }

    pub fn set_actor_signal_waiter(&self, waiter: ActorSignalWaiter) {
        self.inner.lock().unwrap().actor_signal_waiter = Some(waiter);
    }

    pub fn actor_signal_waiter(&self) -> Option<ActorSignalWaiter> {
        self.inner.lock().unwrap().actor_signal_waiter.clone()
    }

    pub fn set_host_operation_completion_safepoint(
        &self,
        safepoint: HostOperationCompletionSafepoint,
    ) {
        self.inner
            .lock()
            .unwrap()
            .host_operation_completion_safepoint = Some(safepoint);
    }

    #[allow(dead_code)]
    pub fn set_workspace_root(&self, root: impl Into<PathBuf>) {
        self.inner.lock().unwrap().workspace_root = Some(root.into());
    }

    pub fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().workspace_root.clone()
    }

    pub fn call_log(&self) -> CallLog {
        self.inner.lock().unwrap().call_log.clone()
    }

    /// Number of records in the accumulated call log, without cloning it.
    pub fn call_log_len(&self) -> usize {
        self.inner.lock().unwrap().call_log.records().len()
    }

    /// Whether the call log holds records the O(1) journal appends did not
    /// cover (replayed/synthetic records pushed during resume). While set,
    /// the durable safepoints must write the full checkpoint; once cleared,
    /// they can skip the O(history) rewrite because `record_call` keeps the
    /// append-only journal complete on its own.
    pub fn call_log_checkpoint_dirty(&self) -> bool {
        self.inner.lock().unwrap().call_log_dirty
    }

    /// Clear the checkpoint-dirty flag after a successful full-checkpoint
    /// write of a log snapshot taken at `persisted_len` records. Guarded by
    /// length so records pushed between the snapshot and this call keep the
    /// log dirty.
    pub(crate) fn clear_call_log_checkpoint_dirty(&self, persisted_len: usize) {
        let mut inner = self.inner.lock().unwrap();
        if inner.call_log.records().len() == persisted_len {
            inner.call_log_dirty = false;
        }
    }

    pub fn set_input_mode(&self, mode: InputMode) {
        self.inner.lock().unwrap().input_mode = mode;
    }

    pub fn input_mode(&self) -> InputMode {
        self.inner.lock().unwrap().input_mode
    }

    pub fn set_pending_input(&self, pending: PendingInput) {
        self.inner.lock().unwrap().pending_input = Some(pending);
    }

    pub fn take_pending_input(&self) -> Option<PendingInput> {
        self.inner.lock().unwrap().pending_input.take()
    }

    pub fn set_pending_approval(&self, pending: PendingApproval) {
        self.inner.lock().unwrap().pending_approval = Some(pending);
    }

    pub fn take_pending_approval(&self) -> Option<PendingApproval> {
        self.inner.lock().unwrap().pending_approval.take()
    }

    pub fn set_pending_signal(&self, pending: PendingSignal) {
        self.inner.lock().unwrap().pending_signal = Some(pending);
    }

    pub fn take_pending_signal(&self) -> Option<PendingSignal> {
        self.inner.lock().unwrap().pending_signal.take()
    }

    /// Mark a `chidori.step(name, fn)` callback as live-executing at `seq`.
    /// While set, every other host effect refuses to run (the callback must be
    /// pure compute), so the step's record at `seq` is always the next record —
    /// skipping the callback on replay can never desynchronize the journal.
    pub fn begin_step(&self, seq: u64, name: &str) {
        self.inner.lock().unwrap().active_step = Some(ActiveStep {
            seq,
            name: name.to_string(),
            started: chrono::Utc::now(),
        });
    }

    /// Take back the live-executing step marker (set by [`begin_step`](Self::begin_step)).
    pub fn take_active_step(&self) -> Option<ActiveStep> {
        self.inner.lock().unwrap().active_step.take()
    }

    /// The name of the step callback currently live-executing, if any. Host
    /// effect dispatchers consult this to refuse effects inside step bodies.
    pub fn active_step_name(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .active_step
            .as_ref()
            .map(|s| s.name.clone())
    }

    /// Replace the in-memory signal mailbox. Used by the
    /// `with_replay_..._and_signals` constructors and resume paths that load the
    /// durable inbox from disk before agent code runs.
    pub fn set_signal_inbox(&self, inbox: Vec<QueuedSignal>) {
        self.inner.lock().unwrap().signal_inbox = inbox;
    }

    /// A clone of the current signal mailbox, for inspection/tests.
    pub fn signal_inbox(&self) -> Vec<QueuedSignal> {
        self.inner.lock().unwrap().signal_inbox.clone()
    }

    /// Drain the lowest-`delivery_seq` queued signal whose name matches `name`,
    /// removing it from the in-memory mailbox and — if persistence is enabled —
    /// immediately re-persisting the shrunken inbox to `SIGNAL_INBOX_FILE` in the
    /// SAME critical section as consumption. This is the determinism guarantee
    /// from `docs/signals.md` §8.4/§10: a crash between consumption and the
    /// recorded `CallRecord` must not leave the signal in the inbox for a second
    /// delivery — on restart the recorded result wins and the inbox is never
    /// re-drained for that seq. Returns `None` when no matching entry exists.
    pub fn take_queued_signal(&self, name: &str) -> Option<QueuedSignal> {
        self.take_queued_signal_any(std::slice::from_ref(&name.to_string()))
    }

    /// As [`take_queued_signal`](Self::take_queued_signal), but matching ANY of
    /// `names` (the `chidori.signal(names[])` fan-in drain). The lowest-`delivery_seq`
    /// entry across the whole set wins, so two queued candidates with different
    /// names are consumed in arrival order — and that choice is frozen into the
    /// recorded result.
    pub fn take_queued_signal_any(&self, names: &[String]) -> Option<QueuedSignal> {
        let mut inner = self.inner.lock().unwrap();
        let idx = inner
            .signal_inbox
            .iter()
            .enumerate()
            .filter(|(_, s)| names.iter().any(|n| n == &s.name))
            .min_by_key(|(_, s)| s.delivery_seq)
            .map(|(i, _)| i)?;
        let signal = inner.signal_inbox.remove(idx);
        persist_signal_inbox(&mut inner);
        Some(signal)
    }

    /// Remove a specific queued signal by its `delivery_seq`, re-persisting the
    /// shrunken inbox in the same critical section. Used by the live streaming
    /// supervisor (Phase 3, `docs/signals.md`) to apply the pinned
    /// "pending-pause-wins-with-newest" tie-break: a just-delivered signal is
    /// write-through enqueued for durability, then *that exact entry* is taken
    /// back out to resolve the pending pause, leaving older queued same-name
    /// entries for later listen points.
    pub fn take_queued_signal_by_delivery_seq(&self, delivery_seq: u64) -> Option<QueuedSignal> {
        let mut inner = self.inner.lock().unwrap();
        let idx = inner
            .signal_inbox
            .iter()
            .position(|s| s.delivery_seq == delivery_seq)?;
        let signal = inner.signal_inbox.remove(idx);
        persist_signal_inbox(&mut inner);
        Some(signal)
    }

    /// Append a signal delivered to a LIVE run into its in-memory mailbox,
    /// write-through persisting the grown inbox (`docs/signals.md` Phase 3).
    /// The in-memory and on-disk inboxes mutate in one critical section, so the
    /// running agent sees the entry at its next listen point and a crash after
    /// the enqueue cannot lose an acknowledged delivery. Returns the stored
    /// entry with its assigned `delivery_seq` (monotonic above every entry
    /// currently queued).
    pub fn enqueue_live_signal(
        &self,
        name: &str,
        payload: serde_json::Value,
        from: serde_json::Value,
    ) -> QueuedSignal {
        let mut inner = self.inner.lock().unwrap();
        let delivery_seq = inner
            .signal_inbox
            .iter()
            .map(|s| s.delivery_seq)
            .max()
            .unwrap_or(0)
            + 1;
        let queued = QueuedSignal {
            name: name.to_string(),
            payload,
            from,
            delivery_seq,
            enqueued_at: chrono::Utc::now(),
        };
        inner.signal_inbox.push(queued.clone());
        persist_signal_inbox(&mut inner);
        queued
    }

    #[allow(dead_code)]
    pub fn create_host_promise(
        &self,
        seq: u64,
        kind: PendingHostOperationKind,
        args: serde_json::Value,
    ) -> HostOperationId {
        self.inner
            .lock()
            .unwrap()
            .host_promises
            .create(seq, kind, args)
    }

    #[allow(dead_code)]
    pub fn begin_host_operation(
        &self,
        seq: u64,
        kind: PendingHostOperationKind,
        args: serde_json::Value,
    ) -> HostOperationId {
        self.begin_host_operation_with_function(seq, kind, None, args)
    }

    pub fn begin_host_operation_with_function(
        &self,
        seq: u64,
        kind: PendingHostOperationKind,
        function: Option<String>,
        args: serde_json::Value,
    ) -> HostOperationId {
        let mut inner = self.inner.lock().unwrap();
        let id = inner
            .host_promises
            .create_with_function(seq, kind, function, args);
        persist_host_promise_change(&mut inner, id);
        id
    }

    pub fn run_host_operation_safepoint(&self, id: HostOperationId) -> anyhow::Result<()> {
        let (safepoint, operation) = {
            let inner = self.inner.lock().unwrap();
            let safepoint = inner.host_operation_safepoint.clone();
            let operation = inner
                .host_promises
                .pending_operation(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown pending host operation id {}", id.0))?;
            (safepoint, operation)
        };
        if let Some(safepoint) = safepoint {
            safepoint.call(&operation)?;
        }
        Ok(())
    }

    pub fn run_host_operation_completion_safepoint(
        &self,
        id: HostOperationId,
    ) -> anyhow::Result<()> {
        let (safepoint, record) = {
            let inner = self.inner.lock().unwrap();
            let safepoint = inner.host_operation_completion_safepoint.clone();
            let record = inner
                .host_promises
                .record(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown host operation id {}", id.0))?;
            (safepoint, record)
        };
        if let Some(safepoint) = safepoint {
            safepoint.call(&record)?;
        }
        Ok(())
    }

    pub fn resolve_host_operation(
        &self,
        id: HostOperationId,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.host_promises.resolve(id, value)?;
        persist_host_promise_change(&mut inner, id);
        Ok(())
    }

    pub fn reject_host_operation(
        &self,
        id: HostOperationId,
        error: impl Into<String>,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.host_promises.reject(id, error)?;
        persist_host_promise_change(&mut inner, id);
        Ok(())
    }

    #[allow(dead_code)]
    pub fn resolve_host_promise(
        &self,
        id: HostOperationId,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.inner.lock().unwrap().host_promises.resolve(id, value)
    }

    #[allow(dead_code)]
    pub fn reject_host_promise(
        &self,
        id: HostOperationId,
        error: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.inner.lock().unwrap().host_promises.reject(id, error)
    }

    #[allow(dead_code)]
    pub fn pending_host_operation(&self, id: HostOperationId) -> Option<PendingHostOperation> {
        self.inner
            .lock()
            .unwrap()
            .host_promises
            .pending_operation(id)
            .cloned()
    }

    #[allow(dead_code)]
    pub fn pending_host_operations(&self) -> Vec<PendingHostOperation> {
        self.inner
            .lock()
            .unwrap()
            .host_promises
            .pending_operations()
    }

    #[allow(dead_code)]
    pub fn active_pending_host_operation(&self) -> Option<PendingHostOperation> {
        self.inner
            .lock()
            .unwrap()
            .host_promises
            .active_pending_operation()
    }

    #[allow(dead_code)]
    pub fn completed_host_operation(
        &self,
        seq: u64,
        kind: PendingHostOperationKind,
    ) -> Option<HostPromiseRecord> {
        self.inner
            .lock()
            .unwrap()
            .host_promises
            .completed_operation(seq, kind)
    }

    #[allow(dead_code)]
    pub fn host_promise_records(&self) -> Vec<HostPromiseRecord> {
        self.inner.lock().unwrap().host_promises.records()
    }
}

/// Record a persistence outcome on the context: strict durability latches the
/// first failure (host dispatch refuses further live effects); the default
/// policy logs and continues, preserving the pre-store behavior.
fn note_persist_result(inner: &mut RuntimeContextInner, result: anyhow::Result<()>) {
    if let Err(err) = result {
        if inner.strict_durability {
            if inner.persist_failure.is_none() {
                inner.persist_failure = Some(err.to_string());
            }
        } else {
            tracing::warn!(run_id = %inner.run_id, error = %err, "run persistence write failed");
        }
    }
}

fn persist_host_promise_change(inner: &mut RuntimeContextInner, id: HostOperationId) {
    let Some(store) = inner.store.clone() else {
        return;
    };
    // One O(1) blob per state change (`host_promises/<id>.json`) instead of
    // rewriting the whole table — which made every host call O(history) and
    // every run O(history²). Compaction points fold the blobs back into
    // `host_promises.json`; readers union both (`load_host_promise_records`).
    if let Some(record) = inner.host_promises.record(id).cloned() {
        let result = serde_json::to_vec_pretty(&record)
            .map_err(anyhow::Error::from)
            .and_then(|json| {
                store.put_blob(&crate::runtime::snapshot::host_promise_blob_key(id), &json)
            });
        note_persist_result(inner, result);
    }

    let pending = inner.host_promises.active_pending_operation();
    let result = match pending {
        Some(pending) => serde_json::to_vec_pretty(&pending)
            .map_err(anyhow::Error::from)
            .and_then(|json| store.put_blob(PENDING_HOST_OPERATION_FILE, &json)),
        None => store.delete_blob(PENDING_HOST_OPERATION_FILE),
    };
    note_persist_result(inner, result);
}

/// Persist the in-memory signal mailbox to `SIGNAL_INBOX_FILE` through the
/// run's store. No-op when persistence is disabled.
fn persist_signal_inbox(inner: &mut RuntimeContextInner) {
    let Some(store) = inner.store.clone() else {
        return;
    };
    let result = serde_json::to_vec_pretty(&inner.signal_inbox)
        .map_err(anyhow::Error::from)
        .and_then(|json| store.put_blob(SIGNAL_INBOX_FILE, &json));
    note_persist_result(inner, result);
}

/// Load the durable signal mailbox from a run directory. Returns an empty vec
/// when the file is absent or unreadable (a fresh run with no queued signals).
/// Used by resume/run paths to thread the inbox into a context the same way the
/// VFS is restored.
pub fn load_signal_inbox(run_dir: &std::path::Path) -> Vec<QueuedSignal> {
    let path = run_dir.join(SIGNAL_INBOX_FILE);
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn default_workspace_root() -> Option<PathBuf> {
    std::env::var_os("CHIDORI_WORKSPACE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Raise a capability flag on `inner`, stamping the current sequence number as
/// the first-touch seq and mirroring it onto the OTEL run span. Idempotent per
/// capability. Split out so the VFS/crypto/timer methods stay one-liners while
/// holding the inner lock exactly once.
fn note_cap(inner: &mut RuntimeContextInner, cap: Capability) {
    let seq = inner.seq;
    if inner.capabilities.note(cap, seq) {
        if let Some(ref otel) = inner.otel_run {
            otel.record_capability(cap);
        }
    }
}

/// Construct the initial virtual filesystem, pre-populated from the
/// `CHIDORI_VFS_SEED` channel if present. The seed is a JSON object mapping an
/// absolute path to its contents — either a UTF-8 string, or an object
/// `{ "base64": "..." }` for binary. This is the only host-disk-adjacent input
/// to the VFS and it is read once, before agent code runs, so the seeded tree
/// is identical on every (re)construction and therefore deterministic.
fn vfs_from_seed_env() -> Vfs {
    let mut vfs = Vfs::new();
    let Ok(raw) = std::env::var("CHIDORI_VFS_SEED") else {
        return vfs;
    };
    if raw.trim().is_empty() {
        return vfs;
    }
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw) else {
        tracing::warn!("CHIDORI_VFS_SEED is not a JSON object; ignoring");
        return vfs;
    };
    for (path, value) in map {
        let bytes = match &value {
            serde_json::Value::String(s) => s.clone().into_bytes(),
            serde_json::Value::Object(obj) => match obj.get("base64").and_then(|v| v.as_str()) {
                Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        tracing::warn!(path = %path, error = %err, "skipping VFS seed entry with invalid base64");
                        continue;
                    }
                },
                None => {
                    tracing::warn!(path = %path, "skipping VFS seed entry: object without `base64`");
                    continue;
                }
            },
            _ => {
                tracing::warn!(path = %path, "skipping VFS seed entry: unsupported value type");
                continue;
            }
        };
        vfs.seed_file(&path, bytes);
    }
    vfs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::snapshot::{HostPromiseState, PENDING_HOST_OPERATION_FILE};

    #[test]
    fn runtime_context_tracks_host_promise_lifecycle() {
        let ctx = RuntimeContext::new();
        let id = ctx.create_host_promise(
            1,
            PendingHostOperationKind::Prompt,
            serde_json::json!({ "text": "hello" }),
        );

        assert_eq!(id, HostOperationId(1));
        assert_eq!(ctx.pending_host_operations().len(), 1);
        assert_eq!(ctx.pending_host_operation(id).unwrap().seq, 1);

        ctx.resolve_host_promise(id, serde_json::json!("done"))
            .unwrap();

        assert!(ctx.pending_host_operation(id).is_none());
        let records = ctx.host_promise_records();
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
    }

    #[test]
    fn runtime_context_rejects_completed_host_promise_twice() {
        let ctx = RuntimeContext::new();
        let id = ctx.create_host_promise(
            1,
            PendingHostOperationKind::Http,
            serde_json::json!({ "url": "https://example.com" }),
        );
        ctx.reject_host_promise(id, "failed").unwrap();

        let err = ctx
            .resolve_host_promise(id, serde_json::json!(null))
            .unwrap_err();
        assert!(err.to_string().contains("already completed"));
    }

    #[test]
    fn runtime_context_persists_pending_and_completed_host_operations() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-promise-persist-test-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());

        let id = ctx.begin_host_operation(
            1,
            PendingHostOperationKind::Prompt,
            serde_json::json!({ "text": "hello" }),
        );

        let pending_path = run_dir.join(PENDING_HOST_OPERATION_FILE);
        assert!(pending_path.exists());
        let pending: PendingHostOperation =
            serde_json::from_slice(&std::fs::read(&pending_path).unwrap()).unwrap();
        assert_eq!(pending.id, id);
        assert_eq!(pending.kind, PendingHostOperationKind::Prompt);
        // The pending state is durable BEFORE the effect runs, via the O(1)
        // per-op blob; the union loader is the read surface for the compacted
        // table + blobs.
        let store = crate::runtime::store::FsRunStore::new(&run_dir);
        let records = crate::runtime::snapshot::load_host_promise_records(&store).unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].state, HostPromiseState::Pending));

        ctx.resolve_host_operation(id, serde_json::json!("done"))
            .unwrap();

        assert!(!pending_path.exists());
        let records = crate::runtime::snapshot::load_host_promise_records(&store).unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn runtime_context_persists_concrete_host_function_name() {
        let ctx = RuntimeContext::new();
        let id = ctx.begin_host_operation_with_function(
            1,
            PendingHostOperationKind::Tool,
            Some("tool".to_string()),
            serde_json::json!({ "name": "do_thing" }),
        );

        let pending = ctx.pending_host_operation(id).unwrap();
        assert_eq!(pending.kind, PendingHostOperationKind::Tool);
        assert_eq!(pending.function.as_deref(), Some("tool"));

        let records = ctx.host_promise_records();
        assert_eq!(records[0].operation.function.as_deref(), Some("tool"));
    }

    #[test]
    fn runtime_context_persists_latest_pending_operation_for_nested_pause() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-promise-nested-pending-test-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());

        let tool_id = ctx.begin_host_operation(
            1,
            PendingHostOperationKind::Tool,
            serde_json::json!({ "name": "ask", "kwargs": {} }),
        );
        let input_id = ctx.begin_host_operation(
            2,
            PendingHostOperationKind::Input,
            serde_json::json!({ "prompt": "Continue?" }),
        );

        let active = ctx.active_pending_host_operation().unwrap();
        assert_eq!(active.id, input_id);
        assert_eq!(active.kind, PendingHostOperationKind::Input);

        let pending: PendingHostOperation = serde_json::from_slice(
            &std::fs::read(run_dir.join(PENDING_HOST_OPERATION_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(pending.id, input_id);
        assert_eq!(pending.kind, PendingHostOperationKind::Input);
        assert_eq!(ctx.pending_host_operation(tool_id).unwrap().id, tool_id);

        let _ = std::fs::remove_dir_all(base);
    }
}
