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
//! Each agent run produces one parent span (`agent.run`) with one child
//! span per host function call. Attributes include agent name, run id,
//! call sequence, model, token counts, and duration.

use std::sync::{Arc, OnceLock};
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
    /// Flush any buffered spans and shut the exporter down cleanly.
    ///
    /// Errors are printed to stderr only when `CHIDORI_OTEL_DEBUG=1`; the
    /// normal unreachable-endpoint case doesn't need to alarm users whose
    /// agents ran fine.
    pub fn shutdown(&self) {
        let debug = std::env::var("CHIDORI_OTEL_DEBUG").as_deref() == Ok("1");
        for r in self.provider.force_flush() {
            if let Err(e) = r {
                if debug {
                    eprintln!("otel: flush error: {e:?}");
                }
            }
        }
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

fn try_init() -> Option<OtelHandle> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "chidori".to_string());

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

/// A live parent span for one agent run. Each call to
/// [`record_call_span`](RunSpan::record_call_span) emits a child span
/// anchored under the parent; [`finish`](RunSpan::finish) ends the parent.
#[derive(Debug)]
pub struct RunSpan {
    parent_cx: Context,
    agent_name: String,
    run_id: String,
}

/// Start a root span for an agent run if OTEL is active. Returns None when
/// tracing is disabled so callers can skip the per-call cost entirely.
pub fn start_run_span(agent_name: &str, run_id: &str) -> Option<Arc<RunSpan>> {
    init_from_env()?;

    let tracer = global::tracer(TRACER_NAME);
    let span = tracer
        .span_builder(format!("agent.run {agent_name}"))
        .with_kind(SpanKind::Internal)
        .with_attributes(vec![
            KeyValue::new("agent.name", agent_name.to_string()),
            KeyValue::new("agent.run_id", run_id.to_string()),
        ])
        .start(&tracer);

    let parent_cx = Context::current_with_span(span);
    Some(Arc::new(RunSpan {
        parent_cx,
        agent_name: agent_name.to_string(),
        run_id: run_id.to_string(),
    }))
}

impl RunSpan {
    /// Emit a child span for a completed host function call. The span is
    /// opened with the recorded start time and closed with start+duration,
    /// so replay won't skew wall-clock timing.
    pub fn record_call_span(&self, record: &CallRecord) {
        let tracer = global::tracer(TRACER_NAME);

        let start_time: SystemTime = record.timestamp.into();
        let end_time = start_time + Duration::from_millis(record.duration_ms);

        let mut attrs = vec![
            KeyValue::new("agent.name", self.agent_name.clone()),
            KeyValue::new("agent.run_id", self.run_id.clone()),
            KeyValue::new("call.seq", record.seq as i64),
            KeyValue::new("call.function", record.function.clone()),
            KeyValue::new("call.duration_ms", record.duration_ms as i64),
        ];

        // Surface the LLM model if present — the most commonly filtered-on
        // attribute for cost and latency debugging. Uses the OTEL semantic
        // convention for GenAI.
        if let Some(model) = record.args.get("model").and_then(|v| v.as_str()) {
            attrs.push(KeyValue::new("gen_ai.request.model", model.to_string()));
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
        }

        let builder = SpanBuilder::from_name(format!("host.{}", record.function))
            .with_kind(span_kind_for(&record.function))
            .with_start_time(start_time)
            .with_end_time(end_time)
            .with_attributes(attrs);

        let mut span = tracer.build_with_context(builder, &self.parent_cx);
        if let Some(err) = &record.error {
            span.set_status(SpanStatus::error(err.clone()));
        } else {
            span.set_status(SpanStatus::Ok);
        }
        // Drop closes the span at the explicit end_time we set above, so
        // recorded wall-clock duration survives even when replay fires the
        // span emission long after the original call happened.
    }

    /// Close the parent span. Sets overall status and releases resources.
    pub fn finish(&self, error: Option<&str>) {
        let span = self.parent_cx.span();
        if let Some(err) = error {
            span.set_status(SpanStatus::error(err.to_string()));
        } else {
            span.set_status(SpanStatus::Ok);
        }
        span.end();
    }
}

/// Map known host function names to semantic span kinds so backends can
/// filter/visualize by category. CLIENT is used for calls that reach
/// external systems (LLM providers, HTTP, sub-agents, tools, the WASM
/// sandbox, the memory store); INTERNAL is the default.
fn span_kind_for(function: &str) -> SpanKind {
    match function {
        "prompt" | "http" | "tool" | "agent" | "exec" | "memory" => SpanKind::Client,
        _ => SpanKind::Internal,
    }
}
