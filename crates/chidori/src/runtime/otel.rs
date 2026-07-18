//! OpenTelemetry (OTLP/gRPC) span export for agent runs.
//!
//! Every host function call already produces a [`CallRecord`] with a start
//! timestamp and duration. This module converts those records into OTEL
//! spans and ships them to any OTLP-compatible backend (tael, Jaeger, Tempo,
//! Honeycomb, Datadog, etc.) via the standard OTLP gRPC protocol on port
//! 4317 by default.
//!
//! Activation is env-driven: set `OTEL_EXPORTER_OTLP_ENDPOINT` to turn
//! tracing on; leave it unset to keep the runtime silent. No CLI flags.
//!
//! Each agent run produces one parent span (`agent.run`) with one child span
//! per host function call. Call spans STREAM out during the run (see
//! [`RunSpan::stream_record`], driven from `record_call`): each span ships as
//! its call completes, buffered only until its parent span exists so it can be
//! nested by `CallRecord::parent_seq` (the only hierarchy signal app-tael
//! reads). Spans are emitted for LIVE execution only — replayed calls don't
//! re-emit, so a resume doesn't duplicate a prior turn's spans. Attributes
//! include agent name, run id, call sequence, model, token counts, and duration.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use opentelemetry::global;
use opentelemetry::trace::{
    Span, SpanBuilder, SpanKind, Status as SpanStatus, TraceContextExt, Tracer,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::TracerProvider as SdkTracerProvider;
use opentelemetry_sdk::Resource;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::runtime::call_log::CallRecord;

/// Tracer instance name under which every agent span is emitted.
const TRACER_NAME: &str = "chidori";

/// Process-wide OTEL handle. Initialized lazily on first call to
/// [`init_from_env`]; re-initialization is a no-op.
static HANDLE: OnceLock<Option<OtelHandle>> = OnceLock::new();

/// Owns the OTLP tracer provider for the process lifetime.
pub struct OtelHandle {
    provider: SdkTracerProvider,
}

impl OtelHandle {
    /// Flush any buffered spans while keeping the exporter alive for later runs.
    pub fn force_flush(&self) {
        let debug = std::env::var("CHIDORI_OTEL_DEBUG").as_deref() == Ok("1");
        for r in self.provider.force_flush() {
            if let Err(e) = r {
                if debug {
                    eprintln!("otel: flush error: {e:?}");
                }
            }
        }
    }

    /// Flush any buffered spans and shut the exporter down cleanly.
    ///
    /// Errors are printed to stderr only when `CHIDORI_OTEL_DEBUG=1`; the
    /// normal unreachable-endpoint case doesn't need to alarm users whose
    /// agents ran fine.
    pub fn shutdown(&self) {
        let debug = std::env::var("CHIDORI_OTEL_DEBUG").as_deref() == Ok("1");
        self.force_flush();
        if let Err(e) = self.provider.shutdown() {
            if debug {
                eprintln!("otel: shutdown error: {e:?}");
            }
        }
    }
}

/// Read `OTEL_EXPORTER_OTLP_ENDPOINT` and, if present, install a
/// process-wide OTLP/gRPC span exporter. Returns `Some(&handle)` when
/// tracing is active, `None` when the env var was unset or the exporter
/// failed to start.
///
/// Must be called from inside a running Tokio runtime — the batch span
/// processor spawns background tasks on the `Tokio` runtime channel.
pub fn init_from_env() -> Option<&'static OtelHandle> {
    HANDLE.get_or_init(try_init).as_ref()
}

/// Flush any pending OTLP spans and shut the provider down. Call this
/// right before a short-lived CLI command exits — otherwise the batch
/// span processor's background task is torn down with the Tokio runtime
/// before it gets a chance to ship the final batch.
///
/// Safe to call even when OTEL was never initialized.
pub fn shutdown_on_exit() {
    if let Some(Some(h)) = HANDLE.get() {
        h.shutdown();
    }
}

/// Flush any currently buffered spans without shutting down the exporter.
///
/// The engine calls this at every run-end boundary (while its Tokio runtime is
/// still alive — see `emit_otel` in `engine.rs`); long-lived embedders can also
/// call it after an agent turn when traces should be visible immediately in an
/// OTLP backend such as Tael.
pub fn force_flush() {
    if let Some(Some(h)) = HANDLE.get() {
        h.force_flush();
    }
}

fn try_init() -> Option<OtelHandle> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "chidori".to_string());

    let exporter = match SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("otel: failed to build OTLP exporter for {endpoint}: {e}");
            return None;
        }
    };

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter, runtime::Tokio)
        .with_resource(Resource::new(vec![KeyValue::new(
            "service.name",
            service_name,
        )]))
        .build();

    global::set_tracer_provider(provider.clone());
    Some(OtelHandle { provider })
}

/// Branch attribution for a call record: which `chidori.branch` variant the
/// call executed inside. Stamped as `chidori.branch_id` / `chidori.branch_label`
/// span attributes so a branch fan-out's subtrees are filterable per variant
/// in an OTLP backend (`tael experiment compare`, `tael query traces
/// --attribute chidori.branch_label=...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTag {
    pub branch_id: String,
    pub label: String,
}

/// Incremental span-emission state for a run. Records stream in as calls
/// complete; a record whose parent span isn't emitted yet waits in `pending`
/// until the parent arrives (a child is recorded before its parent during live
/// runs), so spans ship during the run instead of all at once at the end.
#[derive(Debug, Default)]
struct EmitState {
    /// seq -> the Context to parent that seq's children under.
    ctx_by_seq: HashMap<u64, Context>,
    /// seqs already emitted as spans (idempotency + parent-ready checks).
    emitted: HashSet<u64>,
    /// Records whose parent span hasn't been emitted yet, awaiting it,
    /// each with its branch attribution (None outside `chidori.branch`).
    pending: Vec<(CallRecord, Option<BranchTag>)>,
}

/// A live parent span for one agent run. [`stream_record`](RunSpan::stream_record)
/// emits each host call's span as it completes (nested by `parent_seq`), and
/// [`finish`](RunSpan::finish) flushes any stragglers and ends the parent.
#[derive(Debug)]
pub struct RunSpan {
    parent_cx: Context,
    agent_name: String,
    run_id: String,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
    emit: Mutex<EmitState>,
}

/// Start a root span for an agent run if OTEL is active. Returns None when
/// tracing is disabled so callers can skip the per-call cost entirely.
///
/// `checkpoint_path` is the persisted run directory (`.chidori/runs/<run id>/`)
/// when persistence is on; it's stamped as `chidori.checkpoint_path` so tooling
/// can go from a trace straight to the replayable artifact ($0 `chidori resume`,
/// `chidori branch-rerun` from the anchored state).
pub fn start_run_span(
    agent_name: &str,
    run_id: &str,
    checkpoint_path: Option<&std::path::Path>,
) -> Option<Arc<RunSpan>> {
    init_from_env()?;

    let mut attrs = vec![
        KeyValue::new("agent.name", agent_name.to_string()),
        KeyValue::new("agent.run_id", run_id.to_string()),
        // `chidori.run_id` is the stable, product-named join key between a
        // tael trace and the replayable run (`chidori resume <file> <run_id>`).
        KeyValue::new("chidori.run_id", run_id.to_string()),
    ];
    if let Some(path) = checkpoint_path {
        attrs.push(KeyValue::new(
            "chidori.checkpoint_path",
            path.display().to_string(),
        ));
    }

    let tracer = global::tracer(TRACER_NAME);
    let span = tracer
        .span_builder(format!("agent.run {agent_name}"))
        .with_kind(SpanKind::Internal)
        .with_attributes(attrs)
        .start(&tracer);

    let parent_cx = Context::current_with_span(span);
    Some(Arc::new(RunSpan {
        parent_cx,
        agent_name: agent_name.to_string(),
        run_id: run_id.to_string(),
        total_input_tokens: AtomicU64::new(0),
        total_output_tokens: AtomicU64::new(0),
        emit: Mutex::new(EmitState::default()),
    }))
}

impl RunSpan {
    /// Stamp a capability flag on the run span as a boolean attribute, e.g.
    /// `chidori.capability.crypto_random=true`. Called once per capability the
    /// first time the agent touches that surface.
    pub fn record_capability(&self, cap: crate::runtime::capability::Capability) {
        self.parent_cx.span().set_attribute(KeyValue::new(
            format!("chidori.capability.{}", cap.as_str()),
            true,
        ));
    }

    /// Stream one completed call record as a span, emitting it as soon as its
    /// parent span exists. A record whose `parent_seq` hasn't been emitted yet
    /// is buffered and flushed when the parent arrives (children are recorded
    /// before their parent during live runs) — so spans ship incrementally
    /// during the run, with the OTLP batch processor handling the wire batching,
    /// instead of all at once at the end. Idempotent per seq; safe to call from
    /// `record_call`.
    pub fn stream_record(&self, record: CallRecord) {
        self.stream_record_tagged(record, None);
    }

    /// [`stream_record`](Self::stream_record) with branch attribution: calls
    /// made inside a `chidori.branch` variant stamp `chidori.branch_id` /
    /// `chidori.branch_label` so each variant renders as a filterable subtree.
    pub fn stream_record_tagged(&self, record: CallRecord, branch: Option<BranchTag>) {
        let mut state = self.emit.lock().unwrap();
        if state.emitted.contains(&record.seq) {
            return;
        }
        state.pending.push((record, branch));
        self.drain_ready(&mut state);
    }

    /// Emit every pending record whose parent is already emitted (or is a root),
    /// to a fixpoint. Records still waiting on a parent stay buffered.
    fn drain_ready(&self, state: &mut EmitState) {
        while let Some(pos) = state
            .pending
            .iter()
            .position(|(r, _)| r.parent_seq.map_or(true, |p| state.emitted.contains(&p)))
        {
            let (record, branch) = state.pending.remove(pos);
            self.emit_one(state, &record, branch.as_ref());
        }
    }

    /// Flush ALL buffered records, treating any still-missing parent as a root
    /// (parented to the run span) so nothing is dropped at run end.
    fn drain_all(&self, state: &mut EmitState) {
        while !state.pending.is_empty() {
            // Prefer a record whose parent is ready (keeps nesting correct);
            // otherwise take the first orphan and parent it to the run span.
            let pos = state
                .pending
                .iter()
                .position(|(r, _)| r.parent_seq.map_or(true, |p| state.emitted.contains(&p)))
                .unwrap_or(0);
            let (record, branch) = state.pending.remove(pos);
            self.emit_one(state, &record, branch.as_ref());
        }
    }

    /// Build and emit `record`'s span under its parent's context (or the run
    /// span), recording its context for descendants.
    fn emit_one(&self, state: &mut EmitState, record: &CallRecord, branch: Option<&BranchTag>) {
        let span_ctx = {
            let parent_cx = record
                .parent_seq
                .and_then(|p| state.ctx_by_seq.get(&p))
                .unwrap_or(&self.parent_cx);
            self.build_call_span(record, parent_cx, branch)
        };
        state.ctx_by_seq.insert(
            record.seq,
            Context::new().with_remote_span_context(span_ctx),
        );
        state.emitted.insert(record.seq);
    }

    /// Build one child span for `record` under `parent_cx`, end it at its
    /// explicit end time, and return its `SpanContext` so descendants can
    /// parent to it without keeping the `Span` object alive. Token totals are
    /// accumulated here so [`finish`](Self::finish) sees them regardless of
    /// emission path.
    fn build_call_span(
        &self,
        record: &CallRecord,
        parent_cx: &Context,
        branch: Option<&BranchTag>,
    ) -> opentelemetry::trace::SpanContext {
        let tracer = global::tracer(TRACER_NAME);

        let start_time: SystemTime = record.timestamp.into();
        let end_time = start_time + Duration::from_millis(record.duration_ms);

        let mut attrs = vec![
            KeyValue::new("agent.name", self.agent_name.clone()),
            KeyValue::new("agent.run_id", self.run_id.clone()),
            KeyValue::new("chidori.run_id", self.run_id.clone()),
            KeyValue::new("call.seq", record.seq as i64),
            KeyValue::new("call.function", record.function.clone()),
            KeyValue::new("call.duration_ms", record.duration_ms as i64),
        ];

        // Branch attribution: which `chidori.branch` variant this call ran
        // inside, so an A/B fan-out is comparable per variant in the backend.
        if let Some(tag) = branch {
            attrs.push(KeyValue::new("chidori.branch_id", tag.branch_id.clone()));
            attrs.push(KeyValue::new("chidori.branch_label", tag.label.clone()));
        }

        // Surface the LLM model if present — the most commonly filtered-on
        // attribute for cost and latency debugging. Uses the OTEL semantic
        // convention for GenAI.
        if let Some(model) = record.args.get("model").and_then(|v| v.as_str()) {
            attrs.push(KeyValue::new("gen_ai.request.model", model.to_string()));
        }
        // Mirror the prompt's content-addressed request digest (already in the
        // call-log args) onto the span: a stable join key for "the same prompt
        // across runs" in a backend's attribute/SQL layer.
        if let Some(digest) = record.args.get("request_digest").and_then(|v| v.as_str()) {
            attrs.push(KeyValue::new(
                "chidori.prompt.request_digest",
                digest.to_string(),
            ));
        }
        if let Some(usage) = &record.token_usage {
            attrs.push(KeyValue::new(
                "gen_ai.usage.input_tokens",
                usage.input_tokens as i64,
            ));
            attrs.push(KeyValue::new(
                "gen_ai.usage.output_tokens",
                usage.output_tokens as i64,
            ));
            // Prompt-cache effectiveness, visible per prompt span: creation =
            // the prefix was (re)written this call, read = it was served from
            // cache at the discounted rate.
            if let Some(creation) = usage.cache_creation_tokens {
                attrs.push(KeyValue::new(
                    "gen_ai.usage.cache_creation_tokens",
                    creation as i64,
                ));
            }
            if let Some(read) = usage.cache_read_tokens {
                attrs.push(KeyValue::new("gen_ai.usage.cache_read_tokens", read as i64));
            }
            self.total_input_tokens
                .fetch_add(usage.input_tokens, Ordering::Relaxed);
            self.total_output_tokens
                .fetch_add(usage.output_tokens, Ordering::Relaxed);
        }

        if record.function == "tool" {
            append_tool_attributes(&mut attrs, record);
        }

        if matches!(
            record.function.as_str(),
            "signal" | "poll_signal" | "signal_any"
        ) {
            append_signal_attributes(&mut attrs, record);
        }

        let builder = SpanBuilder::from_name(span_name_for(record))
            .with_kind(span_kind_for(&record.function))
            .with_start_time(start_time)
            .with_end_time(end_time)
            .with_attributes(attrs);

        let mut span = tracer.build_with_context(builder, parent_cx);
        if let Some(err) = &record.error {
            span.set_attribute(KeyValue::new("error.type", record.function.clone()));
            span.set_attribute(KeyValue::new("exception.message", err.clone()));
            span.add_event(
                "exception",
                vec![
                    KeyValue::new("exception.type", record.function.clone()),
                    KeyValue::new("exception.message", err.clone()),
                ],
            );
            span.set_status(SpanStatus::error(err.clone()));
        } else {
            span.set_status(SpanStatus::Ok);
        }
        let span_ctx = span.span_context().clone();
        // End at the recorded end time (a bare `end()` would stamp *now* and
        // discard the faithful duration). Capturing the SpanContext first lets
        // children reference it after the span is gone.
        span.end_with_timestamp(end_time);
        span_ctx
    }

    /// Flush any remaining buffered records, then close the parent span. Sets
    /// overall status and releases resources.
    pub fn finish(&self, error: Option<&str>) {
        {
            let mut state = self.emit.lock().unwrap();
            self.drain_all(&mut state);
        }
        let span = self.parent_cx.span();
        let input = self.total_input_tokens.load(Ordering::Relaxed);
        let output = self.total_output_tokens.load(Ordering::Relaxed);
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.total_input_tokens",
            input as i64,
        ));
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.total_output_tokens",
            output as i64,
        ));
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.total_tokens",
            (input + output) as i64,
        ));
        if let Some(err) = error {
            span.set_status(SpanStatus::error(err.to_string()));
        } else {
            span.set_status(SpanStatus::Ok);
        }
        span.end();
    }
}

/// JS-level trace observer: turns chidori-js function activations into a nested
/// OTEL span tree under a run span. Spans open live on enter and close on exit;
/// the active stack decides each new span's parent, and suspend/resume move an
/// activation in and out of "current" WITHOUT closing its (still-open) span — so
/// a function that awaits/yields keeps the right parentage when it resumes.
///
/// Only the pure-Rust `chidori-js` engine (the sole JS engine) can be observed
/// at this granularity. Install on `ReplayRuntime.vm.trace_sink` for a run that
/// has a [`RunSpan`]. A pure consumer of trace events — never affects execution.
pub struct JsTraceObserver {
    run_cx: Context,
    agent_name: String,
    /// Byte offsets of each `\n` in the module source, for offset→line mapping.
    newline_offsets: Vec<u32>,
    next: u64,
    /// Currently-executing activations (token → its span's child context). The
    /// last entry is the parent for the next `on_enter`.
    active: Vec<(u64, Context)>,
    /// Open spans by token, ended on exit (kept open across suspension).
    open: std::collections::HashMap<u64, opentelemetry::global::BoxedSpan>,
    /// Suspended activations: removed from `active` but span still open.
    parked: std::collections::HashMap<u64, Context>,
    /// Tokens whose span was skipped (depth cap) — exit/suspend/resume no-op.
    skipped: std::collections::HashSet<u64>,
    /// Maximum nesting depth that gets spans; deeper calls are dropped to bound
    /// span volume on deep recursion.
    max_depth: usize,
}

impl JsTraceObserver {
    /// 1-based source line for a byte offset.
    fn line_of(&self, offset: u32) -> u32 {
        self.newline_offsets.partition_point(|&n| n < offset) as u32 + 1
    }
}

impl chidori_js::TraceObserver for JsTraceObserver {
    fn on_enter(&mut self, info: chidori_js::TraceEnter<'_>) -> u64 {
        let token = self.next;
        self.next += 1;
        if self.active.len() >= self.max_depth {
            self.skipped.insert(token);
            return token;
        }
        let name = if info.name.is_empty() {
            "<anonymous>"
        } else {
            info.name
        };
        let parent_cx = self.active.last().map(|(_, c)| c).unwrap_or(&self.run_cx);
        let mut attrs = vec![
            KeyValue::new("agent.name", self.agent_name.clone()),
            KeyValue::new("js.function", name.to_string()),
            KeyValue::new("code.lineno", self.line_of(info.source_start) as i64),
        ];
        if info.is_async {
            attrs.push(KeyValue::new("js.async", true));
        }
        if info.is_generator {
            attrs.push(KeyValue::new("js.generator", true));
        }
        let tracer = global::tracer(TRACER_NAME);
        let builder = SpanBuilder::from_name(name.to_string())
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs);
        let span = tracer.build_with_context(builder, parent_cx);
        let child_cx = Context::new().with_remote_span_context(span.span_context().clone());
        self.open.insert(token, span);
        self.active.push((token, child_cx));
        token
    }

    fn on_exit(&mut self, token: u64, threw: bool) {
        if self.skipped.remove(&token) {
            return;
        }
        if let Some(mut span) = self.open.remove(&token) {
            span.set_status(if threw {
                SpanStatus::error("threw")
            } else {
                SpanStatus::Ok
            });
            span.end();
        }
        self.active.retain(|(t, _)| *t != token);
        self.parked.remove(&token);
    }

    fn on_suspend(&mut self, token: u64) {
        if let Some(pos) = self.active.iter().position(|(t, _)| *t == token) {
            let (_, cx) = self.active.remove(pos);
            self.parked.insert(token, cx);
        }
    }

    fn on_resume(&mut self, token: u64) {
        if let Some(cx) = self.parked.remove(&token) {
            self.active.push((token, cx));
        }
    }
}

impl RunSpan {
    /// Build a JS-level trace observer that nests function spans under this run
    /// span. `source` is the module text (for line resolution); `max_depth`
    /// bounds span volume on deep recursion. Install the result on the
    /// chidori-js `Vm.trace_sink`.
    pub fn js_trace_observer(&self, source: &str, max_depth: usize) -> JsTraceObserver {
        JsTraceObserver {
            run_cx: self.parent_cx.clone(),
            agent_name: self.agent_name.clone(),
            newline_offsets: source
                .bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i as u32)
                .collect(),
            next: 0,
            active: Vec::new(),
            open: std::collections::HashMap::new(),
            parked: std::collections::HashMap::new(),
            skipped: std::collections::HashSet::new(),
            max_depth,
        }
    }
}

/// Map known host function names to semantic span kinds so backends can
/// filter/visualize by category. CLIENT is used for calls that reach
/// external systems (LLM providers, HTTP, sub-agents, tools, the WASM
/// sandbox, the memory store); INTERNAL is the default.
fn span_kind_for(function: &str) -> SpanKind {
    match function {
        // External-system calls. The live recorded `CallRecord::function`
        // strings (see `host_core.rs::host_operation_kind`) are `prompt`,
        // `http`, `tool`, `call_agent`, and `memory`. The `exec`/`exec_js`/
        // `exec_python`/`exec_expr` names are reserved sandbox effects that the
        // host does not currently record (the JS stubs are inert); they're kept
        // here so they classify as CLIENT if a sandbox is ever wired up.
        "prompt" | "http" | "tool" | "call_agent" | "exec" | "exec_js" | "exec_python"
        | "exec_expr" | "memory" => SpanKind::Client,
        _ => SpanKind::Internal,
    }
}

fn span_name_for(record: &CallRecord) -> String {
    if record.function == "tool" {
        "tool.call".to_string()
    } else {
        format!("host.{}", record.function)
    }
}

/// Stamp signal provenance on a `signal` / `poll_signal` / `signal_any` span
/// (`docs/signals.md` Phase 2): the consumed signal's name and the sender
/// (`from = {kind, id, runId?}`), so a multiplayer trace is filterable by
/// participant. A `poll_signal` that found nothing stamps only the polled name;
/// a timed-out listen point stamps `signal.timed_out`.
fn append_signal_attributes(attrs: &mut Vec<KeyValue>, record: &CallRecord) {
    // The fired name lives in the result; fall back to the listen-point args
    // (`{name}` / `{names}`) when the result has none (poll miss, timeout).
    let name = record
        .result
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| record.args.get("name").and_then(Value::as_str));
    if let Some(name) = name {
        attrs.push(KeyValue::new("signal.name", name.to_string()));
    }
    if let Some(names) = record.args.get("names").and_then(Value::as_array) {
        let listen_set = names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(",");
        attrs.push(KeyValue::new("signal.listen_names", listen_set));
    }
    if record
        .result
        .get("timedOut")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        attrs.push(KeyValue::new("signal.timed_out", true));
    }
    let Some(from) = record.result.get("from").filter(|v| !v.is_null()) else {
        return;
    };
    if let Some(kind) = from.get("kind").and_then(Value::as_str) {
        attrs.push(KeyValue::new("signal.from.kind", kind.to_string()));
    }
    if let Some(id) = from.get("id").and_then(Value::as_str) {
        attrs.push(KeyValue::new("signal.from.id", id.to_string()));
    }
    if let Some(run_id) = from.get("runId").and_then(Value::as_str) {
        attrs.push(KeyValue::new("signal.from.run_id", run_id.to_string()));
    }
}

fn append_tool_attributes(attrs: &mut Vec<KeyValue>, record: &CallRecord) {
    let tool_name = record
        .args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let kwargs = record.args.get("kwargs").unwrap_or(&Value::Null);
    let arguments_json = serde_json::to_string(kwargs).unwrap_or_else(|_| "null".to_string());
    let result_count = infer_result_count(&record.result);
    let source = extract_tool_source(&record.result).unwrap_or_else(|| tool_name.clone());
    let status = if record.error.is_some() {
        "error"
    } else if result_count == 0 {
        "empty"
    } else {
        "ok"
    };

    attrs.push(KeyValue::new("tool.name", tool_name.clone()));
    attrs.push(KeyValue::new("tool.integration.name", tool_name));
    attrs.push(KeyValue::new("tool.arguments_json", arguments_json.clone()));
    attrs.push(KeyValue::new("tool.args_json", arguments_json.clone()));
    attrs.push(KeyValue::new(
        "tool.arguments_hash",
        sha256_hex(arguments_json.as_bytes()),
    ));
    attrs.push(KeyValue::new("tool.status", status));
    attrs.push(KeyValue::new("tool.result_count", result_count as i64));
    attrs.push(KeyValue::new("tool.source", source));
    attrs.push(KeyValue::new("tool.latency_ms", record.duration_ms as i64));
    attrs.push(KeyValue::new("tool.cache_hit", false));
    if record.error.is_some() {
        attrs.push(KeyValue::new("tool.error_code", "tool_error"));
    }
}

fn extract_tool_source(result: &Value) -> Option<String> {
    result
        .get("source")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn infer_result_count(result: &Value) -> usize {
    match result {
        Value::Null => 0,
        Value::Array(values) => values.len(),
        Value::Object(map) if map.is_empty() => 0,
        Value::Object(map) => {
            for key in [
                "results",
                "matches",
                "items",
                "recalls",
                "trials",
                "providers",
                "plans",
                "labels",
                "drugs",
            ] {
                if let Some(Value::Array(values)) = map.get(key) {
                    return values.len();
                }
            }
            for key in ["result_count", "count", "total"] {
                if let Some(count) = map.get(key).and_then(Value::as_u64) {
                    return count as usize;
                }
            }
            1
        }
        _ => 1,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::call_log::CallRecord;
    use chrono::Utc;
    use serde_json::json;

    #[test]
    fn tool_span_name_uses_tael_tool_call_name() {
        let record = CallRecord {
            seq: 1,
            parent_seq: None,
            function: "tool".to_string(),
            args: json!({"name": "search_medlineplus_health_topics", "kwargs": {"query": "asthma"}}),
            result: json!({"source": "MedlinePlus", "results": [{"title": "Asthma"}]}),
            duration_ms: 12,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        };

        assert_eq!(span_name_for(&record), "tool.call");
    }

    #[test]
    fn tool_attributes_include_invoked_name_args_and_source() {
        let record = CallRecord {
            seq: 1,
            parent_seq: None,
            function: "tool".to_string(),
            args: json!({"name": "search_medlineplus_health_topics", "kwargs": {"query": "asthma", "limit": 2}}),
            result: json!({"source": "MedlinePlus", "results": [{"title": "Asthma"}]}),
            duration_ms: 12,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        };
        let mut attrs = Vec::new();

        append_tool_attributes(&mut attrs, &record);
        let rendered = attrs
            .iter()
            .map(|attr| format!("{}={}", attr.key, attr.value))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("tool.name=search_medlineplus_health_topics"));
        assert!(rendered.contains("tool.arguments_json={\"limit\":2,\"query\":\"asthma\"}"));
        assert!(rendered.contains("tool.source=MedlinePlus"));
        assert!(rendered.contains("tool.result_count=1"));
        assert!(rendered.contains("tool.status=ok"));
    }

    #[test]
    fn signal_attributes_stamp_name_and_sender_provenance() {
        // A consumed signal: name + from ride in the result.
        let record = CallRecord {
            seq: 3,
            parent_seq: None,
            function: "signal".to_string(),
            args: json!({"name": "review"}),
            result: json!({
                "name": "review",
                "payload": {"decision": "approve"},
                "from": {"kind": "agent", "id": "compliance-bot", "runId": "run-9"},
            }),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        };
        let mut attrs = Vec::new();
        append_signal_attributes(&mut attrs, &record);
        let rendered = attrs
            .iter()
            .map(|attr| format!("{}={}", attr.key, attr.value))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("signal.name=review"));
        assert!(rendered.contains("signal.from.kind=agent"));
        assert!(rendered.contains("signal.from.id=compliance-bot"));
        assert!(rendered.contains("signal.from.run_id=run-9"));

        // A timed-out signalAny: listen set from args, timed_out flag, no from.
        let timeout = CallRecord {
            seq: 4,
            parent_seq: None,
            function: "signal_any".to_string(),
            args: json!({"names": ["review", "steer"]}),
            result: json!({"name": null, "payload": null, "from": null, "timedOut": true}),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        };
        let mut attrs = Vec::new();
        append_signal_attributes(&mut attrs, &timeout);
        let rendered = attrs
            .iter()
            .map(|attr| format!("{}={}", attr.key, attr.value))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("signal.listen_names=review,steer"));
        assert!(rendered.contains("signal.timed_out=true"));
        assert!(!rendered.contains("signal.from"));
    }

    #[test]
    fn span_kind_matches_recorded_function_names() {
        // CLIENT for calls that reach external systems. The live host_core
        // function strings are `prompt`/`http`/`tool`/`call_agent`/`memory`;
        // the `exec`/`exec_js`/`exec_python`/`exec_expr` names are reserved
        // (inert) sandbox effects also mapped to CLIENT. Regression guard
        // against the old `agent`/`exec`-only mapping that missed `call_agent`.
        for f in [
            "prompt",
            "http",
            "tool",
            "call_agent",
            "exec",
            "exec_js",
            "exec_python",
            "exec_expr",
            "memory",
        ] {
            assert!(
                matches!(span_kind_for(f), SpanKind::Client),
                "{f} should map to SpanKind::Client"
            );
        }
        for f in ["input", "template", "log", "checkpoint", "workspace"] {
            assert!(
                matches!(span_kind_for(f), SpanKind::Internal),
                "{f} should map to SpanKind::Internal"
            );
        }
    }

    use opentelemetry::trace::SpanId;
    use opentelemetry_sdk::export::trace::SpanData;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::SimpleSpanProcessor;
    use std::sync::Mutex as StdMutex;

    // Swapping the process-global tracer provider is not thread-safe across
    // tests, so serialize every provider-based test through this lock.
    static PROVIDER_LOCK: StdMutex<()> = StdMutex::new(());

    /// Build a run span against the current (test) global tracer, bypassing the
    /// OTLP-endpoint gate that `start_run_span` enforces.
    fn run_span_for_test(agent: &str, run_id: &str) -> RunSpan {
        let tracer = global::tracer(TRACER_NAME);
        let span = tracer
            .span_builder(format!("agent.run {agent}"))
            .with_kind(SpanKind::Internal)
            .with_attributes(vec![
                KeyValue::new("agent.name", agent.to_string()),
                KeyValue::new("agent.run_id", run_id.to_string()),
            ])
            .start(&tracer);
        RunSpan {
            parent_cx: Context::current_with_span(span),
            agent_name: agent.to_string(),
            run_id: run_id.to_string(),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            emit: Mutex::new(EmitState::default()),
        }
    }

    /// Stream `records` one at a time (the real per-call path), flush at finish,
    /// and return the finished spans (the run span plus one per record).
    fn emit_and_collect(records: &[CallRecord]) -> Vec<SpanData> {
        let _guard = PROVIDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        global::set_tracer_provider(provider.clone());
        let run = run_span_for_test("agent", "r1");
        for r in records {
            run.stream_record(r.clone());
        }
        run.finish(None);
        let _ = provider.force_flush();
        exporter.get_finished_spans().unwrap()
    }

    fn span_by_seq(spans: &[SpanData], seq: i64) -> &SpanData {
        spans
            .iter()
            .find(|s| {
                s.attributes.iter().any(|a| {
                    a.key.as_str() == "call.seq"
                        && matches!(&a.value, opentelemetry::Value::I64(v) if *v == seq)
                })
            })
            .unwrap_or_else(|| panic!("no span for call.seq={seq}"))
    }

    fn run_root(spans: &[SpanData]) -> &SpanData {
        spans
            .iter()
            .find(|s| s.name.as_ref() == "agent.run agent")
            .expect("run span")
    }

    fn rec(seq: u64, parent: Option<u64>, function: &str) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: parent,
            function: function.to_string(),
            args: json!({}),
            result: Value::Null,
            duration_ms: 5,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }
    }

    #[test]
    fn stream_record_ships_span_before_run_finishes() {
        // The core streaming property: a completed call's span is exported while
        // the run is still going — before `finish` and before the run span
        // itself ships. (SimpleSpanProcessor exports synchronously on span end.)
        let _guard = PROVIDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        global::set_tracer_provider(provider.clone());

        let run = run_span_for_test("agent", "r1");
        run.stream_record(rec(1, None, "prompt"));
        let _ = provider.force_flush();

        let mid = exporter.get_finished_spans().unwrap();
        // The host call's span already shipped, mid-run.
        assert!(mid.iter().any(|s| s.name.as_ref() == "host.prompt"));
        // The run span has NOT shipped yet (it ends at finish).
        assert!(!mid.iter().any(|s| s.name.as_ref() == "agent.run agent"));

        run.finish(None);
    }

    #[test]
    fn stream_records_nest_by_parent_seq_under_one_trace() {
        // call_agent(seq1) → nested log(seq2, parent 1); sibling prompt(seq3).
        // Feed child-before-parent (the order live runs record in).
        let records = vec![
            rec(2, Some(1), "log"),
            rec(1, None, "call_agent"),
            rec(3, None, "prompt"),
        ];
        let spans = emit_and_collect(&records);

        let root = run_root(&spans);
        let agent_call = span_by_seq(&spans, 1);
        let log = span_by_seq(&spans, 2);
        let prompt = span_by_seq(&spans, 3);

        // Hierarchy via real OTEL parent_span_id (the only signal tael reads).
        assert_eq!(agent_call.parent_span_id, root.span_context.span_id());
        assert_eq!(log.parent_span_id, agent_call.span_context.span_id());
        assert_eq!(prompt.parent_span_id, root.span_context.span_id());
        assert_eq!(root.parent_span_id, SpanId::INVALID);

        // One connected trace across the whole tree.
        let tid = root.span_context.trace_id();
        for s in [agent_call, log, prompt] {
            assert_eq!(s.span_context.trace_id(), tid);
        }
    }

    #[test]
    fn stream_records_order_independent() {
        // Any input order yields the same parentage — which is exactly what
        // makes live (child-before-parent) and replay produce the same tree.
        let chain = |records: &[CallRecord]| {
            let spans = emit_and_collect(records);
            let s1 = span_by_seq(&spans, 1).span_context.span_id();
            let s2 = span_by_seq(&spans, 2).span_context.span_id();
            (
                span_by_seq(&spans, 2).parent_span_id == s1,
                span_by_seq(&spans, 3).parent_span_id == s2,
            )
        };
        let forward = vec![
            rec(1, None, "call_agent"),
            rec(2, Some(1), "log"),
            rec(3, Some(2), "http"),
        ];
        let reversed = vec![
            rec(3, Some(2), "http"),
            rec(2, Some(1), "log"),
            rec(1, None, "call_agent"),
        ];
        assert_eq!(chain(&forward), (true, true));
        assert_eq!(chain(&reversed), (true, true));
    }

    #[test]
    fn stream_records_preserve_timing_and_token_totals() {
        let mut r = rec(1, None, "prompt");
        let ts = Utc::now();
        r.timestamp = ts;
        r.duration_ms = 1234;
        r.args = json!({ "model": "claude" });
        r.token_usage = Some(crate::runtime::call_log::TokenUsage {
            input_tokens: 10,
            output_tokens: 7,
            cache_creation_tokens: None,
            cache_read_tokens: None,
        });

        let spans = emit_and_collect(std::slice::from_ref(&r));
        let call = span_by_seq(&spans, 1);
        // Span carries the record's explicit start, and end == start + duration,
        // so wall-clock timing survives the (now batch, post-hoc) emission.
        let start: SystemTime = r.timestamp.into();
        assert_eq!(call.start_time, start);
        assert_eq!(
            call.end_time.duration_since(call.start_time).unwrap(),
            Duration::from_millis(1234)
        );

        // Token totals roll up onto the run span at finish, regardless of the
        // (now batch) emission path.
        let root = run_root(&spans);
        let total = root
            .attributes
            .iter()
            .find(|a| a.key.as_str() == "gen_ai.usage.total_tokens");
        assert!(matches!(
            total.map(|a| &a.value),
            Some(opentelemetry::Value::I64(17))
        ));
    }

    #[test]
    fn call_spans_stamp_chidori_run_id_and_prompt_digest() {
        let mut r = rec(1, None, "prompt");
        r.args = json!({ "model": "claude", "request_digest": "abc123" });
        let spans = emit_and_collect(std::slice::from_ref(&r));
        let call = span_by_seq(&spans, 1);
        let attr = |key: &str| {
            call.attributes
                .iter()
                .find(|a| a.key.as_str() == key)
                .map(|a| a.value.as_str().into_owned())
        };
        assert_eq!(attr("chidori.run_id").as_deref(), Some("r1"));
        assert_eq!(
            attr("chidori.prompt.request_digest").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn branch_tagged_records_stamp_branch_id_and_label() {
        let _guard = PROVIDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        global::set_tracer_provider(provider.clone());

        let run = run_span_for_test("agent", "r1");
        // A branch op (seq 1) with one tagged call inside variant "retry" and
        // one untagged sibling outside the branch.
        run.stream_record(rec(1, None, "branch"));
        run.stream_record_tagged(
            rec(2, Some(1), "prompt"),
            Some(BranchTag {
                branch_id: "r1-op1-branch-0".to_string(),
                label: "retry".to_string(),
            }),
        );
        run.stream_record(rec(3, None, "log"));
        run.finish(None);
        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();

        let tagged = span_by_seq(&spans, 2);
        let attr = |s: &SpanData, key: &str| {
            s.attributes
                .iter()
                .find(|a| a.key.as_str() == key)
                .map(|a| a.value.as_str().into_owned())
        };
        assert_eq!(
            attr(tagged, "chidori.branch_id").as_deref(),
            Some("r1-op1-branch-0")
        );
        assert_eq!(
            attr(tagged, "chidori.branch_label").as_deref(),
            Some("retry")
        );
        // Untagged sibling carries no branch attribution.
        let untagged = span_by_seq(&spans, 3);
        assert_eq!(attr(untagged, "chidori.branch_label"), None);
    }

    #[test]
    fn js_trace_observer_emits_nested_spans_through_engine() {
        let _guard = PROVIDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        global::set_tracer_provider(provider.clone());

        let run = run_span_for_test("agent", "r1");
        let src = "function b(){ return 1; } function a(){ return b(); } a();";
        let observer = run.js_trace_observer(src, 64);
        let mut engine = chidori_js::Engine::new();
        engine.vm.trace_sink = Some(Box::new(observer));
        engine.eval(src).expect("eval ok");
        run.finish(None);
        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();

        let by_name = |n: &str| {
            spans
                .iter()
                .find(|s| s.name.as_ref() == n)
                .unwrap_or_else(|| panic!("no span named {n}"))
        };
        // a() is called by the top-level <script> frame; b() by a(). JS-level
        // spans nest by real OTEL parent_span_id, rooted at the run span.
        let run_span = by_name("agent.run agent");
        let script = by_name("<script>");
        let a = by_name("a");
        let b = by_name("b");
        assert_eq!(b.parent_span_id, a.span_context.span_id());
        assert_eq!(a.parent_span_id, script.span_context.span_id());
        assert_eq!(script.parent_span_id, run_span.span_context.span_id());

        let tid = run_span.span_context.trace_id();
        for s in [script, a, b] {
            assert_eq!(s.span_context.trace_id(), tid);
        }
    }
}
