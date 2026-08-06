//! No-op stand-in for [`otel`](super::otel) when the `otel` feature is off.
//!
//! Mirrors the real module's public surface exactly, so every call site
//! compiles unchanged: `init_from_env`/`start_run_span` return `None`, which
//! makes all span-emission paths dead — [`RunSpan`] is never constructed and
//! its methods exist only to satisfy the type checker. Building without
//! `otel` drops the OTLP/gRPC dependency tree (tonic, prost, hyper, tower),
//! which is the point: materially faster builds where tael/OTLP export isn't
//! needed. There is no runtime behavior difference for a process that never
//! set `OTEL_EXPORTER_OTLP_ENDPOINT` — the real module is inert without it.

use std::sync::Arc;

use crate::runtime::call_log::CallRecord;
use crate::runtime::capability::Capability;

/// Feature-off placeholder for the OTLP pipeline handle.
pub struct OtelHandle {}

impl OtelHandle {
    pub fn force_flush(&self) {}
    pub fn shutdown(&self) {}
}

/// Always `None`: with the feature off there is no exporter to initialize,
/// regardless of `OTEL_EXPORTER_OTLP_ENDPOINT`.
pub fn init_from_env() -> Option<&'static OtelHandle> {
    None
}

pub fn shutdown_on_exit() {}

pub fn force_flush() {}

/// Branch identity tag; carried through contexts but never emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTag {
    pub branch_id: String,
    pub label: String,
}

/// Feature-off run span. Never constructed (`start_run_span` returns `None`);
/// the methods exist so call sites that hold `Option<Arc<RunSpan>>` typecheck.
#[derive(Debug)]
pub struct RunSpan {}

pub fn start_run_span(
    _agent_name: &str,
    _run_id: &str,
    _checkpoint_path: Option<&std::path::Path>,
) -> Option<Arc<RunSpan>> {
    None
}

impl RunSpan {
    pub fn record_capability(&self, _cap: Capability) {}
    pub fn stream_record(&self, _record: CallRecord) {}
    pub fn stream_record_tagged(&self, _record: CallRecord, _branch: Option<BranchTag>) {}
    pub fn finish(&self, _error: Option<&str>) {}
    pub fn js_trace_observer(&self, _source: &str, _max_depth: usize) -> JsTraceObserver {
        JsTraceObserver {}
    }
}

/// Feature-off trace observer: satisfies `chidori_js::TraceObserver` but is
/// unreachable in practice (it can only be built from a `RunSpan`).
pub struct JsTraceObserver {}

impl chidori_js::TraceObserver for JsTraceObserver {
    fn on_enter(&mut self, _info: chidori_js::TraceEnter<'_>) -> u64 {
        0
    }
    fn on_exit(&mut self, _token: u64, _threw: bool) {}
    fn on_suspend(&mut self, _token: u64) {}
    fn on_resume(&mut self, _token: u64) {}
}
