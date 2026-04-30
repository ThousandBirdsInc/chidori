use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::call_log::{CallLog, CallRecord};
use crate::runtime::otel::RunSpan;

/// A streaming event the runtime emits while an agent runs. CallRecord is
/// the original per-call event; TokenDelta carries partial LLM output as the
/// provider streams it back. A single SSE endpoint can multiplex both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Call(CallRecord),
    TokenDelta { seq: u64, delta: String },
}

/// Shared runtime context passed into Starlark host functions.
///
/// Holds the LLM provider registry, call log, template engine,
/// and agent-level configuration. Wrapped in Arc<Mutex<>> so
/// Starlark's synchronous host functions can mutate it.
#[derive(Debug, Clone)]
pub struct RuntimeContext {
    inner: Arc<Mutex<RuntimeContextInner>>,
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
    /// Optional live-event sink. When set, every `record_call` is also
    /// forwarded here so the server can stream host-function calls to
    /// connected clients (e.g. over SSE). Token deltas emitted by streaming
    /// providers flow through the same channel as RuntimeEvent::TokenDelta.
    pub event_sender: Option<UnboundedSender<RuntimeEvent>>,
    /// Optional OpenTelemetry parent span for this run. When set, every
    /// `record_call` also emits a child OTLP span with the call's timing
    /// and attributes — shipping automatically to any OTLP backend (tael,
    /// Jaeger, Honeycomb, Datadog, ...). None disables OTEL export.
    pub otel_run: Option<Arc<RunSpan>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Read one line from stdin and return it. Used by the CLI `run` command.
    Stdin,
    /// Record the prompt and raise a pause sentinel; the engine returns a
    /// Paused RunResult so the caller can collect the response out-of-band.
    Pause,
}

#[derive(Debug, Clone)]
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
    pub timeout: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            max_turns: 10,
            timeout: 300,
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
                event_sender: None,
                otel_run: None,
            })),
        }
    }

    /// Create a context in replay mode with a pre-loaded call log.
    /// Host functions will return cached results for matching calls.
    pub fn with_replay(replay_log: Vec<CallRecord>) -> Self {
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
                event_sender: None,
                otel_run: None,
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

    pub fn update_config<F: FnOnce(&mut AgentConfig)>(&self, f: F) {
        let mut inner = self.inner.lock().unwrap();
        f(&mut inner.config);
    }

    pub fn next_seq(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        inner.seq
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

    pub fn record_call(&self, record: CallRecord) {
        let mut inner = self.inner.lock().unwrap();
        inner.call_log.push(record.clone());
        if let Some(ref dir) = inner.persist_dir {
            if let Ok(json) = inner.call_log.to_json() {
                let _ = std::fs::write(dir.join("checkpoint.json"), json);
            }
        }
        if let Some(ref otel) = inner.otel_run {
            otel.record_call_span(&record);
        }
        if let Some(ref tx) = inner.event_sender {
            let _ = tx.send(RuntimeEvent::Call(record));
        }
    }

    /// Emit a streaming token fragment for seq. Used by prompt() when the
    /// provider supports incremental decoding. Ignored if no event sender
    /// is attached to the context.
    pub fn emit_token_delta(&self, seq: u64, delta: String) {
        let inner = self.inner.lock().unwrap();
        if let Some(ref tx) = inner.event_sender {
            let _ = tx.send(RuntimeEvent::TokenDelta { seq, delta });
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
}
