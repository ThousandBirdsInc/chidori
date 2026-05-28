use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::call_log::{CallLog, CallRecord};
use crate::runtime::otel::RunSpan;
use crate::runtime::snapshot::{
    HostOperationId, HostPromiseRecord, HostPromiseTable, PendingHostOperation,
    PendingHostOperationKind, HOST_PROMISE_TABLE_FILE, PENDING_HOST_OPERATION_FILE,
};

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

#[derive(Clone)]
pub struct LiveVmSnapshotter(
    Arc<dyn Fn() -> anyhow::Result<chidori_quickjs::RuntimeSnapshot> + Send + Sync>,
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

impl std::fmt::Debug for LiveVmSnapshotter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveVmSnapshotter").finish_non_exhaustive()
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

impl LiveVmSnapshotter {
    #[allow(dead_code)]
    pub fn new(
        callback: impl Fn() -> anyhow::Result<chidori_quickjs::RuntimeSnapshot> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(callback))
    }

    fn capture(&self) -> anyhow::Result<chidori_quickjs::RuntimeSnapshot> {
        (self.0)()
    }
}

#[derive(Debug)]
struct RuntimeContextInner {
    /// Agent-level defaults set via config().
    pub config: AgentConfig,
    /// Accumulated call log for checkpointing / tracing.
    pub call_log: CallLog,
    /// Sequence counter for call log entries.
    pub seq: u64,
    /// Pre-loaded call log for replay mode. When set, host functions
    /// return cached results instead of executing for matching sequence numbers.
    pub replay_log: Option<Vec<CallRecord>>,
    /// Unique identifier for this run. Used as the subdirectory name
    /// under `.chidori/runs/` when persistence is enabled.
    pub run_id: String,
    /// Directory into which the checkpoint file is written after each call.
    /// None disables on-disk persistence.
    pub persist_dir: Option<PathBuf>,
    /// How `input()` should behave when there is no cached response:
    /// read from stdin, or pause the run and surface the prompt to the caller.
    pub input_mode: InputMode,
    /// Set by `input()` when pausing. The engine reads this after eval
    /// unwinds to distinguish a pause from a real error.
    pub pending_input: Option<PendingInput>,
    /// Set by the permission policy when an AskBefore rule needs approval.
    pub pending_approval: Option<PendingApproval>,
    /// Durable host-promise bookkeeping. This is snapshot-serializable Rust
    /// state; the QuickJS fork will bind these ids to real JS promises.
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
    /// Optional live VM snapshotter registered by snapshot-capable runtimes.
    /// When present, durable safepoints persist its live continuation bytes
    /// instead of the initial TypeScript state scaffold.
    pub live_vm_snapshotter: Option<LiveVmSnapshotter>,
    /// Optional scoped workspace root exposed through `chidori.workspace`.
    pub workspace_root: Option<PathBuf>,
    /// Seqs of host calls currently executing (their `live()` is on the
    /// stack). The top is the parent of any call recorded while it runs —
    /// this is how sub-agent calls (made inside `call_agent`'s execution)
    /// get stamped with their enclosing `call_agent`'s seq.
    pub call_stack: Vec<u64>,
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
/// distinguish it from a genuine failure.
pub const PAUSE_MARKER: &str = "__CHIDORI_PAUSED_FOR_INPUT__";

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

impl RuntimeContext {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: AgentConfig::default(),
                call_log: CallLog::new(),
                seq: 0,
                replay_log: None,
                run_id: uuid::Uuid::new_v4().to_string(),
                persist_dir: None,
                input_mode: InputMode::Stdin,
                pending_input: None,
                pending_approval: None,
                host_promises: HostPromiseTable::new(),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                live_vm_snapshotter: None,
                workspace_root: default_workspace_root(),
                call_stack: Vec::new(),
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
        Self {
            inner: Arc::new(Mutex::new(RuntimeContextInner {
                config: AgentConfig::default(),
                call_log: CallLog::new(),
                seq: 0,
                replay_log: Some(replay_log),
                run_id: uuid::Uuid::new_v4().to_string(),
                persist_dir: None,
                input_mode: InputMode::Stdin,
                pending_input: None,
                pending_approval: None,
                host_promises: HostPromiseTable::from_records(host_promises),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                live_vm_snapshotter: None,
                workspace_root: default_workspace_root(),
                call_stack: Vec::new(),
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
                seq,
                replay_log: None,
                run_id,
                persist_dir: None,
                input_mode: InputMode::Stdin,
                pending_input: None,
                pending_approval: None,
                host_promises: HostPromiseTable::new(),
                event_sender: None,
                emit_call_events: true,
                otel_run: None,
                host_operation_safepoint: None,
                host_operation_completion_safepoint: None,
                live_vm_snapshotter: None,
                workspace_root: default_workspace_root(),
                call_stack: Vec::new(),
            })),
        }
    }

    /// Enable on-disk persistence. Each `record_call` will rewrite
    /// `<base_dir>/<run_id>/checkpoint.json` with the full log.
    /// Returns the run directory path.
    pub fn enable_persistence(&self, base_dir: PathBuf) -> PathBuf {
        let mut inner = self.inner.lock().unwrap();
        let run_dir = base_dir.join(&inner.run_id);
        let _ = std::fs::create_dir_all(&run_dir);
        inner.persist_dir = Some(run_dir.clone());
        run_dir
    }

    pub fn run_id(&self) -> String {
        self.inner.lock().unwrap().run_id.clone()
    }

    pub fn config(&self) -> AgentConfig {
        self.inner.lock().unwrap().config.clone()
    }

    pub fn next_seq(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        inner.seq
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
    pub fn try_replay(&self, seq: u64) -> Option<CallRecord> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ref replay_log) = inner.replay_log {
            if let Some(record) = replay_log.iter().find(|r| r.seq == seq) {
                let record = record.clone();
                // Record the replayed call in the new call log.
                inner.call_log.push(record.clone());
                return Some(record);
            }
        }
        None
    }

    /// Replay-cache lookup with divergence check. Returns:
    ///   Ok(Some(record)) — cached and the cached record's function name
    ///                      matches `expected_fn`. Safe to use.
    ///   Ok(None)         — no cache hit; caller should execute live.
    ///   Err(msg)         — cached, but the recorded function differs from
    ///                      what the agent is calling now. The agent code
    ///                      changed since the checkpoint was saved. The
    ///                      engine should abort the replay with a clear error.
    pub fn try_replay_checked(
        &self,
        seq: u64,
        expected_fn: &str,
    ) -> Result<Option<CallRecord>, String> {
        match self.try_replay(seq) {
            None => Ok(None),
            Some(record) if record.function == expected_fn => Ok(Some(record)),
            Some(record) => Err(format!(
                "Replay divergence at seq {}: checkpoint has `{}` but agent called `{}`. \
                 The agent code changed since the checkpoint was saved — \
                 re-run without replay to regenerate.",
                seq, record.function, expected_fn
            )),
        }
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
        inner.call_log.push(record.clone());
        if let Some(ref dir) = inner.persist_dir {
            if let Ok(json) = inner.call_log.to_json() {
                let _ = std::fs::write(dir.join("checkpoint.json"), json);
            }
        }
        if let Some(ref otel) = inner.otel_run {
            otel.record_call_span(&record);
        }
        if inner.emit_call_events {
            if let Some(ref tx) = inner.event_sender {
                let _ = tx.send(RuntimeEvent::Call(record));
            }
        }
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
    pub fn set_live_vm_snapshotter(&self, snapshotter: LiveVmSnapshotter) {
        self.inner.lock().unwrap().live_vm_snapshotter = Some(snapshotter);
    }

    #[allow(dead_code)]
    pub fn set_workspace_root(&self, root: impl Into<PathBuf>) {
        self.inner.lock().unwrap().workspace_root = Some(root.into());
    }

    pub fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().workspace_root.clone()
    }

    pub fn capture_live_vm_snapshot(
        &self,
    ) -> Option<anyhow::Result<chidori_quickjs::RuntimeSnapshot>> {
        let snapshotter = self.inner.lock().unwrap().live_vm_snapshotter.clone()?;
        Some(snapshotter.capture())
    }

    pub fn call_log(&self) -> CallLog {
        self.inner.lock().unwrap().call_log.clone()
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
        persist_host_promises(&inner);
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
        persist_host_promises(&inner);
        Ok(())
    }

    pub fn reject_host_operation(
        &self,
        id: HostOperationId,
        error: impl Into<String>,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.host_promises.reject(id, error)?;
        persist_host_promises(&inner);
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
        args: &serde_json::Value,
    ) -> Option<HostPromiseRecord> {
        self.inner
            .lock()
            .unwrap()
            .host_promises
            .completed_operation(seq, kind, args)
    }

    #[allow(dead_code)]
    pub fn host_promise_records(&self) -> Vec<HostPromiseRecord> {
        self.inner.lock().unwrap().host_promises.records()
    }
}

fn persist_host_promises(inner: &RuntimeContextInner) {
    let Some(dir) = inner.persist_dir.as_ref() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let records = inner.host_promises.records();
    if let Ok(json) = serde_json::to_vec_pretty(&records) {
        let _ = std::fs::write(dir.join(HOST_PROMISE_TABLE_FILE), json);
    }

    let pending = inner.host_promises.active_pending_operation();
    let pending_path = dir.join(PENDING_HOST_OPERATION_FILE);
    match pending {
        Some(pending) => {
            if let Ok(json) = serde_json::to_vec_pretty(&pending) {
                let _ = std::fs::write(pending_path, json);
            }
        }
        None => {
            if pending_path.exists() {
                let _ = std::fs::remove_file(pending_path);
            }
        }
    }
}

fn default_workspace_root() -> Option<PathBuf> {
    std::env::var_os("CHIDORI_WORKSPACE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::snapshot::{
        HostPromiseState, HOST_PROMISE_TABLE_FILE, PENDING_HOST_OPERATION_FILE,
    };

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
        let table_path = run_dir.join(HOST_PROMISE_TABLE_FILE);
        assert!(pending_path.exists());
        assert!(table_path.exists());
        let pending: PendingHostOperation =
            serde_json::from_slice(&std::fs::read(&pending_path).unwrap()).unwrap();
        assert_eq!(pending.id, id);
        assert_eq!(pending.kind, PendingHostOperationKind::Prompt);

        ctx.resolve_host_operation(id, serde_json::json!("done"))
            .unwrap();

        assert!(!pending_path.exists());
        let records: Vec<HostPromiseRecord> =
            serde_json::from_slice(&std::fs::read(&table_path).unwrap()).unwrap();
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
            PendingHostOperationKind::Sandbox,
            Some("exec_js".to_string()),
            serde_json::json!({ "source": "1 + 1" }),
        );

        let pending = ctx.pending_host_operation(id).unwrap();
        assert_eq!(pending.kind, PendingHostOperationKind::Sandbox);
        assert_eq!(pending.function.as_deref(), Some("exec_js"));

        let records = ctx.host_promise_records();
        assert_eq!(records[0].operation.function.as_deref(), Some("exec_js"));
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
