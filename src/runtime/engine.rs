use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::mcp::McpManager;
use crate::policy::{PolicyCache, PolicyConfig};
use crate::providers::ProviderRegistry;
use crate::runtime::call_log::{CallLog, CallRecord};
use crate::runtime::context::{
    HostOperationCompletionSafepoint, HostOperationSafepoint, InputMode, PendingApproval,
    PendingInput, RuntimeContext, RuntimeEvent,
};
use crate::runtime::snapshot::{
    HostPromiseRecord, RuntimePolicy, SnapshotAbi, SnapshotManifest, SnapshotStore,
    SourceFingerprint,
};
use crate::runtime::template::TemplateEngine;
use crate::tools::ToolRegistry;
use tracing::{error as tracing_error, info};

/// Agent execution engine.
pub struct Engine {
    providers: Arc<ProviderRegistry>,
    template_engine: Arc<TemplateEngine>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    tools: Arc<ToolRegistry>,
    policy: Arc<PolicyConfig>,
    mcp: Arc<McpManager>,
    /// Pre-approved (target, args) pairs to seed the PolicyCache before the
    /// run starts. Used by the server's approval flow: on /approve, the
    /// handler adds the pending target+args here, rebuilds the engine, and
    /// replays the agent — the policy check will now hit the cache and pass.
    approvals: Vec<(String, serde_json::Value)>,
    /// If set, each run persists its call log to `<persist_base>/<run_id>/checkpoint.json`.
    persist_base: Option<PathBuf>,
}

pub struct RunResult {
    pub output: Value,
    pub call_log: CallLog,
    #[allow(dead_code)]
    pub run_id: String,
    /// Set when the agent called `input()` in Pause mode. The caller should
    /// treat the run as suspended and later resume by replaying the call log
    /// with a synthetic `input` record appended at `pending.seq`.
    pub paused: Option<PendingInput>,
    /// Set when the permission policy blocked an AskBefore call in Pause
    /// mode. The caller should render an approval UI and, on approve, re-run
    /// the agent with the approval added to `Engine::with_approvals`.
    pub paused_approval: Option<PendingApproval>,
}

/// Persist a resume scaffold for a run on the pure-Rust engine (G2).
///
/// Unlike the QuickJS path, the rust live path's durability is the
/// `RuntimeContext` call log, not a VM image: resume re-executes the agent from
/// the top and feeds each host effect its recorded result via the call-log
/// replay (`try_replay`), blocking again at the pending frontier. So there is no
/// VM blob to store — we persist a manifest (`InitialTypeScriptStateScaffold`
/// kind) plus the call log, pending operation, host-promise table, capabilities,
/// and VFS, which is exactly what the server's call-log replay resume path
/// consumes.
///
/// The ABI token is `chidori-quickjs` on purpose: the server's resume gate
/// validates the manifest ABI against `SnapshotAbi::current("chidori-quickjs")`,
/// and the existing QuickJS *scaffold* fallback uses the same token and resumes
/// the same engine-agnostic way (call-log replay, not a VM image). Reusing it
/// keeps the rust manifest acceptable to the unchanged resume gate.
fn persist_rust_journal_scaffold(
    base: &Path,
    run_id: &str,
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
    ctx: &RuntimeContext,
) -> Result<()> {
    let call_log = ctx.call_log().into_records();
    let pending = ctx.active_pending_host_operation();
    let host_promises = ctx.host_promise_records();
    let modules = crate::runtime::typescript::module_graph::snapshot_module_fingerprints(
        path, source, policy,
    )?;
    let module_graph =
        crate::runtime::typescript::module_graph::snapshot_module_graph(path, source, policy)?;
    let manifest = SnapshotManifest::new(
        run_id,
        SnapshotAbi::current("chidori-quickjs"),
        policy.clone(),
        SourceFingerprint::from_source(path, source),
        modules,
        pending.clone(),
        call_log.len(),
    )
    .with_module_graph(module_graph)
    .with_host_promises(host_promises)
    .with_capabilities(ctx.capabilities())
    .with_vfs(ctx.vfs_snapshot());
    // The rust engine has no VM image to serialize; resume is call-log replay.
    // We still write a non-empty blob — the durable code bundle the journal keys
    // reference — so `SnapshotStore::load()` round-trips and the on-disk shape
    // matches the QuickJS scaffold (manifest + blob + checkpoint + pending).
    let blob = chidori_js::replay::DurableBlob {
        bundle: source.to_string(),
        effects: Vec::new(),
        journal: Vec::new(),
    };
    let blob_bytes = serde_json::to_vec(&blob).unwrap_or_default();
    SnapshotStore::new(base.join(run_id)).save(&manifest, &blob_bytes, &call_log)
}

impl Engine {
    pub fn new(
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            providers,
            template_engine,
            tokio_rt,
            tools: Arc::new(ToolRegistry::new()),
            policy: PolicyConfig::from_env(),
            mcp: Arc::new(McpManager::new()),
            approvals: Vec::new(),
            persist_base: None,
        }
    }

    pub fn with_approvals(mut self, approvals: Vec<(String, serde_json::Value)>) -> Self {
        self.approvals = approvals;
        self
    }

    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_policy(mut self, policy: Arc<PolicyConfig>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_mcp(mut self, mcp: Arc<McpManager>) -> Self {
        self.mcp = mcp;
        self
    }

    pub fn with_persist_base(mut self, base: PathBuf) -> Self {
        self.persist_base = Some(base);
        self
    }

    /// Parse and validate an agent or tool file without executing it.
    pub fn check(&self, path: &Path) -> Result<()> {
        if path.extension().and_then(|e| e.to_str()) == Some("ts") {
            let run_id = "check";
            let policy = crate::runtime::snapshot::RuntimePolicy::from_env_for_durable_run(run_id)?;
            crate::runtime::typescript::check::check_typescript_file(path, &policy)?;
            return Ok(());
        }

        anyhow::bail!(
            "unsupported agent file {}: TypeScript `.ts` agents are required",
            path.display()
        )
    }

    /// Run a TypeScript agent file with the given JSON inputs.
    pub fn run(&self, path: &Path, inputs: &Value) -> Result<RunResult> {
        let ctx = RuntimeContext::new();
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent with a pre-loaded call log for replay.
    ///
    /// Host function calls whose sequence numbers match the replay log
    /// return cached results instantly. Calls past the end of the log
    /// execute normally. The returned call log is the full merged log.
    pub fn run_with_replay(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay(replay_log);
        self.run_with_context(path, inputs, ctx)
    }

    pub fn run_with_replay_and_host_promises(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
    ) -> Result<RunResult> {
        self.run_with_replay_host_promises_and_vfs(
            path,
            inputs,
            replay_log,
            host_promises,
            crate::runtime::vfs::Vfs::new(),
        )
    }

    /// Resume with a virtual filesystem restored from a snapshot manifest, so
    /// the resumed run observes the file state captured at suspend.
    pub fn run_with_replay_host_promises_and_vfs(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: crate::runtime::vfs::Vfs,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay_host_promises_and_vfs(replay_log, host_promises, vfs);
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent while forwarding each host function call and every
    /// streamed token delta to `sender` as a live RuntimeEvent. Used by
    /// the server's SSE endpoint to drive incremental UIs.
    pub fn run_streaming(
        &self,
        path: &Path,
        inputs: &Value,
        sender: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::new();
        ctx.set_event_sender(sender);
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent while forwarding live events and pausing on input or
    /// approval requests. This is the in-process equivalent of the session
    /// server's interactive execution path for embedders that already own
    /// their UI/event loop.
    pub fn run_streaming_pausable(
        &self,
        path: &Path,
        inputs: &Value,
        sender: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<RunResult> {
        self.run_streaming_pausable_with_model_override(path, inputs, sender, None)
    }

    /// As `run_streaming_pausable`, but installs an optional model-override hook
    /// (Pi-style save point) so a mid-run model change refreshes the model on
    /// the next provider request inside this run's tool loop.
    pub fn run_streaming_pausable_with_model_override(
        &self,
        path: &Path,
        inputs: &Value,
        sender: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
        model_override: Option<crate::runtime::context::ModelOverride>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::new();
        ctx.set_event_sender(sender);
        ctx.set_input_mode(InputMode::Pause);
        if let Some(model_override) = model_override {
            ctx.set_model_override(model_override);
        }
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent in Pause-on-input mode. When the agent calls `input()`
    /// and no cached response exists, the engine catches the pause sentinel
    /// and returns a RunResult whose `paused` field is set.
    pub fn run_pausable(&self, path: &Path, inputs: &Value) -> Result<RunResult> {
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent with replay + pause support. Used to resume a paused
    /// session: the caller has appended a synthetic `input` record to the
    /// call log at the pending seq; replay returns it to the agent, which
    /// continues until it finishes or calls `input()` again.
    pub fn run_replay_pausable(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay(replay_log);
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    pub fn run_replay_pausable_with_host_promises(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
    ) -> Result<RunResult> {
        self.run_replay_pausable_with_host_promises_and_vfs(
            path,
            inputs,
            replay_log,
            host_promises,
            crate::runtime::vfs::Vfs::new(),
        )
    }

    /// As `run_replay_pausable_with_host_promises`, restoring a snapshot
    /// manifest's virtual filesystem for the resumed run.
    pub fn run_replay_pausable_with_host_promises_and_vfs(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: crate::runtime::vfs::Vfs,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay_host_promises_and_vfs(replay_log, host_promises, vfs);
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    /// As `run_replay_pausable_with_host_promises_and_vfs`, but continues the run
    /// under `run_id` instead of minting a fresh one. The server's call-log
    /// replay resume uses this so a resumed run keeps its original id (and its
    /// persisted run directory), matching the live-VM resume path — a resumed
    /// run is the same durable run, not a new one.
    pub fn run_replay_pausable_with_host_promises_and_vfs_preserving_run_id(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: crate::runtime::vfs::Vfs,
        run_id: String,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay_host_promises_and_vfs(replay_log, host_promises, vfs);
        ctx.set_run_id(run_id);
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    /// Resume/replay an interactive streaming run with persisted host-promise
    /// state. Used by embedders that mirror the server session interaction
    /// without routing through HTTP.
    pub fn run_streaming_replay_pausable_with_host_promises(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        sender: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay_and_host_promises(replay_log, host_promises);
        ctx.set_event_sender(sender);
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    fn run_with_context(
        &self,
        path: &Path,
        inputs: &Value,
        ctx: RuntimeContext,
    ) -> Result<RunResult> {
        // Enable on-disk persistence if configured.
        if let Some(ref base) = self.persist_base {
            let run_dir = ctx.enable_persistence(base.clone());
            // Save the input alongside the checkpoint for later resume/trace.
            let _ = std::fs::write(
                run_dir.join("input.json"),
                serde_json::to_string_pretty(inputs).unwrap_or_default(),
            );
        }

        // Start a root OTEL span for this run. No-op when OTEL is disabled
        // (i.e. OTEL_EXPORTER_OTLP_ENDPOINT is unset). The `_otel_guard`
        // local finishes the parent span when this function returns — on
        // success, error, or pause — by way of Drop on the dropped Arc.
        let agent_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("agent")
            .to_string();
        let run_id = ctx.run_id();
        // The OTLP batch span processor spawns background tasks on the
        // current Tokio runtime; entering `tokio_rt` here makes both the
        // one-time init and the per-call span emissions well-formed.
        let _tokio_guard = self.tokio_rt.enter();
        info!(agent = %agent_name, run_id = %run_id, "agent run start");
        if let Some(run_span) = crate::runtime::otel::start_run_span(&agent_name, &run_id) {
            ctx.set_otel_run(run_span);
        }

        // Close out OTEL at a terminal/pause boundary. Host-call spans stream
        // out during the run (`record_call` → `RunSpan::stream_record`), and
        // only for LIVE execution — replayed calls (try_replay / absorb) don't
        // re-emit, since their spans shipped when they first ran live (this is
        // what keeps a resume from duplicating the prior turn's spans). `finish`
        // flushes any buffered stragglers and ends the run span. No-op when OTEL
        // is off (otel_run is None).
        let emit_otel = |ctx: &RuntimeContext, error: Option<&str>| {
            if let Some(otel) = ctx.otel_run() {
                otel.finish(error);
            }
        };

        if path.extension().and_then(|e| e.to_str()) == Some("ts") {
            let policy = RuntimePolicy::from_env_for_durable_run(&run_id)?;
            let source = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;

            // Route to the pure-Rust engine when selected (CHIDORI_JS_ENGINE=rust).
            // Host effects still flow through host_core/RuntimeContext, so the
            // durable call log and the host-call span tree behave identically;
            // this path additionally emits JS-level function spans when tracing
            // is on. Only compiled with `--features rust-engine`.
            // Build the same runtime host backend the QuickJS path uses, so
            // every `chidori.*` effect routes through identical durable
            // machinery (call log, replay, policy, MCP, OTEL).
            let mut seeded = PolicyCache::default();
            for (target, args) in &self.approvals {
                seeded.approve(target, args);
            }
            let backend = crate::runtime::typescript::bindings::HostBindingBackend::for_runtime(
                ctx.clone(),
                self.providers.clone(),
                self.template_engine.clone(),
                self.tokio_rt.clone(),
                self.policy.clone(),
                Arc::new(StdMutex::new(seeded)),
                policy.clone(),
                self.tools.clone(),
                self.mcp.clone(),
            );
            // Persist a durable journal scaffold before the run and at each
            // host-operation safepoint, so a pause/crash has a resumable
            // artifact on disk (G2). Mirrors the QuickJS safepoint wiring
            // below, but stores the rust engine's journal-shaped manifest.
            if let Some(ref base) = self.persist_base {
                let safepoint_base = base.clone();
                let safepoint_run_id = run_id.clone();
                let safepoint_path = path.to_path_buf();
                let safepoint_source = source.clone();
                let safepoint_policy = policy.clone();
                let safepoint_ctx = ctx.clone();
                ctx.set_host_operation_safepoint(HostOperationSafepoint::new(move |_operation| {
                    persist_rust_journal_scaffold(
                        &safepoint_base,
                        &safepoint_run_id,
                        &safepoint_path,
                        &safepoint_source,
                        &safepoint_policy,
                        &safepoint_ctx,
                    )
                }));
                let completion_base = base.clone();
                let completion_run_id = run_id.clone();
                let completion_path = path.to_path_buf();
                let completion_source = source.clone();
                let completion_policy = policy.clone();
                let completion_ctx = ctx.clone();
                ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
                    move |_record| {
                        persist_rust_journal_scaffold(
                            &completion_base,
                            &completion_run_id,
                            &completion_path,
                            &completion_source,
                            &completion_policy,
                            &completion_ctx,
                        )
                    },
                ));
                persist_rust_journal_scaffold(base, &run_id, path, &source, &policy, &ctx)?;
            }

            // Pause surfacing on the rust path (G1). A `chidori.input()` in
            // Pause mode or a policy-approval block sets `pending_input` /
            // `pending_approval` on the ctx and signals a pause by returning
            // the `PAUSE_MARKER` sentinel from the host effect — which the
            // rust engine throws as a JS error and bubbles up here as `Err`.
            // We surface that as a paused `RunResult` so the resume flow has
            // something to resume, mirroring the QuickJS arm. The check runs
            // on `Ok` too in case the agent caught the sentinel and returned.
            let surface_pause = |ctx: &RuntimeContext| -> Option<RunResult> {
                if let Some(pending) = ctx.take_pending_input() {
                    if let Some(ref base) = self.persist_base {
                        let _ = persist_rust_journal_scaffold(
                            base, &run_id, path, &source, &policy, ctx,
                        );
                    }
                    emit_otel(ctx, None);
                    return Some(RunResult {
                        output: Value::Null,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: Some(pending),
                        paused_approval: None,
                    });
                }
                if let Some(approval) = ctx.take_pending_approval() {
                    if let Some(ref base) = self.persist_base {
                        let _ = persist_rust_journal_scaffold(
                            base, &run_id, path, &source, &policy, ctx,
                        );
                    }
                    emit_otel(ctx, None);
                    return Some(RunResult {
                        output: Value::Null,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: None,
                        paused_approval: Some(approval),
                    });
                }
                None
            };

            let rust_result =
                crate::runtime::rust_engine::run_agent(path, &source, inputs, &backend);
            // Release the streaming event sender now that the run is over.
            // The chidori-js VM can leak its heap on drop (Rc cycles), which
            // would keep `backend` — and through it this ctx and the sender —
            // alive, hanging a `--stream` drain loop that waits for the
            // channel to close. Dropping our own `backend` handle plus
            // clearing the ctx-held sender closes the channel regardless.
            ctx.clear_event_sender();
            drop(backend);
            return match rust_result {
                Ok(output) => {
                    if let Some(paused) = surface_pause(&ctx) {
                        return Ok(paused);
                    }
                    info!(agent = %agent_name, run_id = %run_id, "rust-engine agent run ok");
                    if let Some(ref base) = self.persist_base {
                        let run_dir = base.join(ctx.run_id());
                        let _ = std::fs::write(
                            run_dir.join("output.json"),
                            serde_json::to_string_pretty(&output).unwrap_or_default(),
                        );
                        let _ = persist_rust_journal_scaffold(
                            base, &run_id, path, &source, &policy, &ctx,
                        );
                    }
                    emit_otel(&ctx, None);
                    Ok(RunResult {
                        output,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: None,
                        paused_approval: None,
                    })
                }
                Err(e) => {
                    if let Some(paused) = surface_pause(&ctx) {
                        return Ok(paused);
                    }
                    let err_msg = e.to_string();
                    tracing_error!(agent = %agent_name, run_id = %run_id, error = %err_msg, "rust-engine agent run failed");
                    if let Some(ref base) = self.persist_base {
                        let _ = persist_rust_journal_scaffold(
                            base, &run_id, path, &source, &policy, &ctx,
                        );
                    }
                    emit_otel(&ctx, Some(&err_msg));
                    Err(e)
                }
            };
        }

        let err_msg = format!(
            "unsupported agent file {}: TypeScript `.ts` agents are required",
            path.display()
        );
        tracing_error!(agent = %agent_name, run_id = %run_id, error = %err_msg, "agent run failed");
        emit_otel(&ctx, Some(&err_msg));
        Err(anyhow::anyhow!(err_msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        ContentBlock, LlmProvider, LlmRequest, LlmResponse, ProviderRegistry, TokenSink,
    };
    use crate::runtime::template::TemplateEngine;

    /// The pure-Rust engine is the only runtime. Its durability is call-log
    /// replay over a scaffold manifest (`InitialTypeScriptStateScaffold`), not a
    /// VM image, so persistence tests assert that shape.
    fn rust_engine_active() -> bool {
        true
    }

    struct FixedTestProvider {
        content: String,
        input_tokens: u64,
        output_tokens: u64,
    }

    struct InspectingTestProvider {
        run_base: PathBuf,
        observed_pending_prompt: Arc<std::sync::Mutex<bool>>,
        expected_snapshot_kind: Option<crate::runtime::snapshot::SnapshotBlobKind>,
        expected_blob: Option<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FixedTestProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: self.content.clone(),
                blocks: vec![ContentBlock::Text {
                    text: self.content.clone(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            })
        }

        async fn stream(
            &self,
            request: &LlmRequest,
            on_delta: &mut TokenSink,
        ) -> Result<LlmResponse> {
            let response = self.send(request).await?;
            on_delta(&response.content);
            Ok(response)
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for InspectingTestProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            let run_dir = std::fs::read_dir(&self.run_base)?
                .next()
                .ok_or_else(|| anyhow::anyhow!("expected persisted run dir before provider"))??
                .path();
            let loaded = SnapshotStore::new(run_dir).load()?;
            let pending = loaded
                .manifest
                .pending
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("expected pending prompt before provider"))?;
            anyhow::ensure!(
                pending.kind == crate::runtime::snapshot::PendingHostOperationKind::Prompt,
                "expected pending prompt before provider, got {:?}",
                pending.kind
            );
            anyhow::ensure!(
                loaded.manifest.call_log_len == 0,
                "prompt call should not be recorded before provider executes"
            );
            anyhow::ensure!(
                !loaded.blob.is_empty(),
                "pre-provider snapshot blob is empty"
            );
            if let Some(expected_kind) = self.expected_snapshot_kind {
                anyhow::ensure!(
                    loaded.manifest.snapshot_kind == expected_kind,
                    "expected pre-provider snapshot kind {:?}, got {:?}",
                    expected_kind,
                    loaded.manifest.snapshot_kind
                );
            }
            if let Some(expected_blob) = &self.expected_blob {
                anyhow::ensure!(
                    &loaded.blob == expected_blob,
                    "pre-provider snapshot blob did not match expected live VM snapshot"
                );
            }
            anyhow::ensure!(
                loaded.manifest.host_promises.len() == 1,
                "expected one pending host promise before provider"
            );
            anyhow::ensure!(
                matches!(
                    loaded.manifest.host_promises[0].state,
                    crate::runtime::snapshot::HostPromiseState::Pending
                ),
                "expected pending host promise before provider"
            );
            *self.observed_pending_prompt.lock().unwrap() = true;
            Ok(LlmResponse {
                content: "provider result".to_string(),
                blocks: vec![ContentBlock::Text {
                    text: "provider result".to_string(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 1,
                output_tokens: 2,
            })
        }
    }

    #[test]
    fn engine_runs_typescript_agent_and_records_log_call() {
        let dir = std::env::temp_dir().join(format!("chidori-engine-ts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                import type { Chidori } from "chidori";
                export async function agent(input: { name: string }, chidori: Chidori) {
                    await chidori.log("starting");
                    return { greeting: "Hello, " + input.name };
                }
            "#,
        )
        .unwrap();

        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );
        let result = engine
            .run(&path, &serde_json::json!({ "name": "TypeScript" }))
            .unwrap();

        assert_eq!(
            result.output,
            serde_json::json!({ "greeting": "Hello, TypeScript" })
        );
        let records = result.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "log");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_pauses_and_resumes_typescript_input() {
        let dir =
            std::env::temp_dir().join(format!("chidori-engine-ts-input-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const approval = await chidori.input("Approve this request?");
                    return { approved: approval.toLowerCase() === "yes" };
                }
            "#,
        )
        .unwrap();

        let engine = || {
            Engine::new(
                Arc::new(ProviderRegistry::new()),
                Arc::new(TemplateEngine::new(&dir)),
                Arc::new(tokio::runtime::Runtime::new().unwrap()),
            )
        };

        let paused = engine()
            .run_pausable(&path, &serde_json::json!({}))
            .unwrap();
        let pending = paused.paused.unwrap();
        assert_eq!(pending.seq, 1);
        assert_eq!(pending.prompt, "Approve this request?");
        assert!(paused.call_log.into_records().is_empty());

        let replay = vec![CallRecord {
            seq: pending.seq,
            parent_seq: None,
            function: "input".to_string(),
            args: serde_json::json!({ "prompt": pending.prompt }),
            result: serde_json::json!("yes"),
            duration_ms: 0,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        }];
        let resumed = engine()
            .run_replay_pausable(&path, &serde_json::json!({}), replay)
            .unwrap();

        assert_eq!(resumed.output, serde_json::json!({ "approved": true }));
        assert!(resumed.paused.is_none());
        let records = resumed.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "input");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_replays_completed_host_promise_without_live_tool_execution() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-host-promise-replay-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    return await chidori.tool("echo", { value: input.value });
                }
            "#,
        )
        .unwrap();
        let args = serde_json::json!({
            "name": "echo",
            "kwargs": { "value": 42 },
        });
        let host_promises = vec![crate::runtime::snapshot::HostPromiseRecord {
            operation: crate::runtime::snapshot::PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                1,
                crate::runtime::snapshot::PendingHostOperationKind::Tool,
                args,
            ),
            state: crate::runtime::snapshot::HostPromiseState::Resolved {
                value: serde_json::json!({ "value": 42 }),
                completed_at: chrono::Utc::now(),
            },
        }];
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );

        let result = engine
            .run_with_replay_and_host_promises(
                &path,
                &serde_json::json!({ "value": 42 }),
                Vec::new(),
                host_promises,
            )
            .unwrap();

        assert_eq!(result.output, serde_json::json!({ "value": 42 }));
        let records = result.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "tool");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_replays_completed_prompt_host_promise_without_provider_call() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-prompt-host-promise-replay-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const text = await chidori.prompt("hello", { type: "progress" });
                    return { text };
                }
            "#,
        )
        .unwrap();
        let args = serde_json::json!({
            "text": "hello",
            "model": "claude-sonnet-4-6",
            "type": "progress",
        });
        let host_promises = vec![crate::runtime::snapshot::HostPromiseRecord {
            operation: crate::runtime::snapshot::PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                1,
                crate::runtime::snapshot::PendingHostOperationKind::Prompt,
                args,
            ),
            state: crate::runtime::snapshot::HostPromiseState::Resolved {
                value: serde_json::json!("cached prompt"),
                completed_at: chrono::Utc::now(),
            },
        }];
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );

        let result = engine
            .run_with_replay_and_host_promises(
                &path,
                &serde_json::json!({}),
                Vec::new(),
                host_promises,
            )
            .unwrap();

        assert_eq!(
            result.output,
            serde_json::json!({ "text": "cached prompt" })
        );
        let records = result.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "prompt");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_replays_completed_call_agent_host_promise_without_child_execution() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-call-agent-host-promise-replay-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let child_path = dir.join("missing-child.ts");
        let child_path_string = child_path.display().to_string();
        let child_path_json = serde_json::to_string(&child_path_string).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    return await chidori.callAgent({child_path_json}, {{ value: input.value }});
                }}
                "#
            ),
        )
        .unwrap();
        let args = serde_json::json!({
            "path": child_path_string,
            "input": { "value": 41 },
        });
        let host_promises = vec![crate::runtime::snapshot::HostPromiseRecord {
            operation: crate::runtime::snapshot::PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                1,
                crate::runtime::snapshot::PendingHostOperationKind::CallAgent,
                args,
            ),
            state: crate::runtime::snapshot::HostPromiseState::Resolved {
                value: serde_json::json!({ "value": 42 }),
                completed_at: chrono::Utc::now(),
            },
        }];
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );

        let result = engine
            .run_with_replay_and_host_promises(
                &path,
                &serde_json::json!({ "value": 41 }),
                Vec::new(),
                host_promises,
            )
            .unwrap();

        assert_eq!(result.output, serde_json::json!({ "value": 42 }));
        let records = result.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "call_agent");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_persists_typescript_snapshot_blob_on_policy_approval_pause() {
        if rust_engine_active() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-approval-snapshot-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.http("https://example.invalid");
                    return { ok: true };
                }
            "#,
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: crate::policy::Decision::AskBefore,
                match_args: None,
                reason: Some("test approval".to_string()),
            }],
            default: crate::policy::Decision::AlwaysAllow,
        });

        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_policy(policy)
        .with_persist_base(run_base.clone());
        let paused = engine.run_pausable(&path, &serde_json::json!({})).unwrap();

        let approval = paused.paused_approval.unwrap();
        assert_eq!(approval.target, "http");
        let loaded = SnapshotStore::new(run_base.join(&paused.run_id))
            .load()
            .unwrap();
        let pending = loaded.manifest.pending.as_ref().unwrap();
        assert_eq!(
            pending.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Http
        );
        assert_eq!(
            pending.args,
            serde_json::json!({
                "url": "https://example.invalid",
                "method": "GET",
                "headers": null,
                "body": null,
                "params": null,
            })
        );
        assert_eq!(loaded.manifest.host_promises.len(), 1);
        assert_eq!(loaded.manifest.host_promises[0].operation.id, pending.id);
        assert_eq!(
            loaded.manifest.host_promises[0].operation.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Http
        );
        assert!(matches!(
            loaded.manifest.host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Pending
        ));
        assert!(!loaded.blob.is_empty());
        let persisted_pending: crate::runtime::snapshot::PendingHostOperation =
            serde_json::from_slice(
                &std::fs::read(
                    run_base
                        .join(&paused.run_id)
                        .join(crate::runtime::snapshot::PENDING_HOST_OPERATION_FILE),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(persisted_pending.id, pending.id);
        assert_eq!(
            persisted_pending.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Http
        );
        let persisted_host_promises: Vec<crate::runtime::snapshot::HostPromiseRecord> =
            serde_json::from_slice(
                &std::fs::read(
                    run_base
                        .join(&paused.run_id)
                        .join(crate::runtime::snapshot::HOST_PROMISE_TABLE_FILE),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(persisted_host_promises.len(), 1);
        assert_eq!(persisted_host_promises[0].operation.id, pending.id);
        assert!(matches!(
            persisted_host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Pending
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_safepoints_persist_snapshot_around_failed_host_side_effect() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-host-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.http("not a url");
                    return { ok: true };
                }
            "#,
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());

        let err = match engine.run(&path, &serde_json::json!({})) {
            Ok(_) => panic!("expected invalid URL host operation to fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("builder error"));

        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest = SnapshotStore::new(run_dir).load_manifest().unwrap();
        assert_eq!(manifest.call_log_len, 1);
        assert!(manifest.pending.is_none());
        assert_eq!(manifest.host_promises.len(), 1);
        assert!(matches!(
            manifest.host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Rejected { .. }
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_prompt_safepoint_persists_pending_snapshot_before_provider_call() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-prompt-pre-provider-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const text = await chidori.prompt("write status", { type: "progress" });
                    return { text };
                }
            "#,
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let observed_pending_prompt = Arc::new(std::sync::Mutex::new(false));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(InspectingTestProvider {
            run_base: run_base.clone(),
            observed_pending_prompt: observed_pending_prompt.clone(),
            expected_snapshot_kind: None,
            expected_blob: None,
        }));
        let engine = Engine::new(
            Arc::new(providers),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());

        let result = engine.run(&path, &serde_json::json!({})).unwrap();

        assert_eq!(
            result.output,
            serde_json::json!({ "text": "provider result" })
        );
        assert!(*observed_pending_prompt.lock().unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_completion_safepoint_persists_snapshot_after_host_result_record() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-host-completion-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.log("before-js-error");
                    throw new Error("after host result");
                }
            "#,
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());

        let err = match engine.run(&path, &serde_json::json!({})) {
            Ok(_) => panic!("expected JavaScript error after host result"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("after host result"));

        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest = SnapshotStore::new(run_dir).load_manifest().unwrap();
        assert_eq!(manifest.call_log_len, 1);
        assert!(manifest.pending.is_none());
        assert_eq!(manifest.host_promises.len(), 1);
        assert!(matches!(
            manifest.host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Resolved { .. }
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_prompt_completion_safepoint_persists_provider_result_record() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-prompt-completion-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.prompt("write status", { type: "progress" });
                    throw new Error("after provider result");
                }
            "#,
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(FixedTestProvider {
            content: "provider result".to_string(),
            input_tokens: 7,
            output_tokens: 11,
        }));
        let engine = Engine::new(
            Arc::new(providers),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());

        let err = match engine.run(&path, &serde_json::json!({})) {
            Ok(_) => panic!("expected JavaScript error after provider result"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("after provider result"));

        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let loaded = SnapshotStore::new(run_dir).load().unwrap();
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert!(loaded.manifest.pending.is_none());
        assert_eq!(loaded.manifest.host_promises.len(), 1);
        assert_eq!(
            loaded.manifest.host_promises[0].operation.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Prompt
        );
        match &loaded.manifest.host_promises[0].state {
            crate::runtime::snapshot::HostPromiseState::Resolved { value, .. } => {
                assert_eq!(value, &serde_json::json!("provider result"));
            }
            other => panic!("expected resolved prompt host promise, got {other:?}"),
        }
        assert!(!loaded.blob.is_empty());
        let checkpoint: Vec<CallRecord> = serde_json::from_slice(
            &std::fs::read(
                run_base
                    .join(&loaded.manifest.run_id)
                    .join("checkpoint.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(checkpoint.len(), 1);
        assert_eq!(checkpoint[0].function, "prompt");
        assert_eq!(checkpoint[0].result, serde_json::json!("provider result"));
        assert_eq!(checkpoint[0].args["type"], serde_json::json!("progress"));
        let usage = checkpoint[0].token_usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 11);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_tool_completion_safepoint_persists_tool_result_record() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-tool-completion-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.tool("echo", { value: input.value });
                    throw new Error("after tool result");
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &tool_path,
            r#"
                export const tool = {
                  name: "echo",
                  description: "Echo a value",
                  parameters: {
                    type: "object",
                    properties: { value: { type: "number" } },
                    required: ["value"],
                  },
                };

                export async function run(args, chidori) {
                  return { value: args.value + 1 };
                }
            "#,
        )
        .unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: Vec::new(),
            source_path: tool_path,
            source_fingerprint: None,
            backend: crate::tools::ToolBackend::TypeScript,
        });
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_tools(Arc::new(registry))
        .with_persist_base(run_base.clone());

        let err = match engine.run(&path, &serde_json::json!({ "value": 41 })) {
            Ok(_) => panic!("expected JavaScript error after tool result"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("after tool result"),
            "unexpected error: {err}"
        );

        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let loaded = SnapshotStore::new(run_dir).load().unwrap();
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert!(loaded.manifest.pending.is_none());
        assert_eq!(loaded.manifest.host_promises.len(), 1);
        assert_eq!(
            loaded.manifest.host_promises[0].operation.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Tool
        );
        match &loaded.manifest.host_promises[0].state {
            crate::runtime::snapshot::HostPromiseState::Resolved { value, .. } => {
                assert_eq!(value, &serde_json::json!({ "value": 42 }));
            }
            other => panic!("expected resolved tool host promise, got {other:?}"),
        }
        assert!(!loaded.blob.is_empty());
        let checkpoint: Vec<CallRecord> = serde_json::from_slice(
            &std::fs::read(
                run_base
                    .join(&loaded.manifest.run_id)
                    .join("checkpoint.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(checkpoint.len(), 1);
        assert_eq!(checkpoint[0].function, "tool");
        assert_eq!(checkpoint[0].args["name"], serde_json::json!("echo"));
        assert_eq!(
            checkpoint[0].args["kwargs"],
            serde_json::json!({ "value": 41 })
        );
        assert_eq!(checkpoint[0].result, serde_json::json!({ "value": 42 }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_tool_pause_persists_inner_pending_operation_and_outer_tool_promise() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-tool-pause-snapshot-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let tool_path = dir.join("ask.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    return await chidori.tool("ask", { prompt: input.prompt });
                }
            "#,
        )
        .unwrap();
        std::fs::write(
            &tool_path,
            r#"
                export const tool = {
                  name: "ask",
                  description: "Ask for input",
                  parameters: {
                    type: "object",
                    properties: { prompt: { type: "string" } },
                    required: ["prompt"],
                  },
                };

                export async function run(args, chidori) {
                  const answer = await chidori.input(args.prompt);
                  return { answer };
                }
            "#,
        )
        .unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "ask".to_string(),
            description: "Ask for input".to_string(),
            params: Vec::new(),
            source_path: tool_path,
            source_fingerprint: None,
            backend: crate::tools::ToolBackend::TypeScript,
        });
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_tools(Arc::new(registry))
        .with_persist_base(run_base.clone());

        let paused = engine
            .run_pausable(&path, &serde_json::json!({ "prompt": "Continue?" }))
            .unwrap();
        let pending_input = paused.paused.expect("expected tool input pause");
        assert_eq!(pending_input.prompt, "Continue?");

        let loaded = SnapshotStore::new(run_base.join(&paused.run_id))
            .load()
            .unwrap();
        let pending = loaded.manifest.pending.as_ref().unwrap();
        assert_eq!(
            pending.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Input
        );
        assert_eq!(pending.args, serde_json::json!({ "prompt": "Continue?" }));
        assert_eq!(loaded.manifest.host_promises.len(), 2);
        assert!(loaded.manifest.host_promises.iter().any(|record| {
            record.operation.kind == crate::runtime::snapshot::PendingHostOperationKind::Tool
                && matches!(
                    record.state,
                    crate::runtime::snapshot::HostPromiseState::Pending
                )
        }));
        assert!(loaded.manifest.host_promises.iter().any(|record| {
            record.operation.kind == crate::runtime::snapshot::PendingHostOperationKind::Input
                && matches!(
                    record.state,
                    crate::runtime::snapshot::HostPromiseState::Pending
                )
        }));
        let persisted_pending: crate::runtime::snapshot::PendingHostOperation =
            serde_json::from_slice(
                &std::fs::read(
                    run_base
                        .join(&paused.run_id)
                        .join(crate::runtime::snapshot::PENDING_HOST_OPERATION_FILE),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            persisted_pending.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Input
        );

        let mut host_promises = loaded.manifest.host_promises.clone();
        let input_record = host_promises
            .iter_mut()
            .find(|record| {
                record.operation.kind == crate::runtime::snapshot::PendingHostOperationKind::Input
            })
            .expect("expected persisted input host promise");
        input_record.state = crate::runtime::snapshot::HostPromiseState::Resolved {
            value: serde_json::json!("yes"),
            completed_at: chrono::Utc::now(),
        };

        let resumed = engine
            .run_replay_pausable_with_host_promises(
                &path,
                &serde_json::json!({ "prompt": "Continue?" }),
                Vec::new(),
                host_promises,
            )
            .unwrap();
        assert!(resumed.paused.is_none());
        assert_eq!(resumed.output, serde_json::json!({ "answer": "yes" }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_call_agent_completion_safepoint_persists_sub_agent_result_record() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-call-agent-completion-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let child_path = dir.join("child.ts");
        let parent_path = dir.join("parent.ts");
        std::fs::write(
            &child_path,
            r#"
                export async function agent(input, chidori) {
                    return { value: input.value + 1 };
                }
            "#,
        )
        .unwrap();
        let child_path_string = child_path.display().to_string();
        let child_path_json = serde_json::to_string(&child_path_string).unwrap();
        std::fs::write(
            &parent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    await chidori.callAgent({child_path_json}, {{ value: input.value }});
                    throw new Error("after sub-agent result");
                }}
                "#
            ),
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());

        let err = match engine.run(&parent_path, &serde_json::json!({ "value": 41 })) {
            Ok(_) => panic!("expected JavaScript error after sub-agent result"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("after sub-agent result"));

        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let loaded = SnapshotStore::new(run_dir).load().unwrap();
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert!(loaded.manifest.pending.is_none());
        assert_eq!(loaded.manifest.host_promises.len(), 1);
        assert_eq!(
            loaded.manifest.host_promises[0].operation.kind,
            crate::runtime::snapshot::PendingHostOperationKind::CallAgent
        );
        match &loaded.manifest.host_promises[0].state {
            crate::runtime::snapshot::HostPromiseState::Resolved { value, .. } => {
                assert_eq!(value, &serde_json::json!({ "value": 42 }));
            }
            other => panic!("expected resolved sub-agent host promise, got {other:?}"),
        }
        assert!(!loaded.blob.is_empty());
        let checkpoint: Vec<CallRecord> = serde_json::from_slice(
            &std::fs::read(
                run_base
                    .join(&loaded.manifest.run_id)
                    .join("checkpoint.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(checkpoint.len(), 1);
        assert_eq!(checkpoint[0].function, "call_agent");
        assert_eq!(
            checkpoint[0].args["path"],
            serde_json::json!(child_path_string)
        );
        assert_eq!(
            checkpoint[0].args["input"],
            serde_json::json!({ "value": 41 })
        );
        assert_eq!(checkpoint[0].result, serde_json::json!({ "value": 42 }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_checkpoint_safepoint_persists_snapshot_after_marker_record() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-checkpoint-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.checkpoint("mid-run", { step: 1 });
                    throw new Error("after checkpoint");
                }
            "#,
        )
        .unwrap();
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());

        let err = match engine.run(&path, &serde_json::json!({})) {
            Ok(_) => panic!("expected JavaScript error after checkpoint"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("after checkpoint"));

        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let loaded = SnapshotStore::new(run_dir).load().unwrap();
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert!(loaded.manifest.pending.is_none());
        assert_eq!(loaded.manifest.host_promises.len(), 1);
        assert_eq!(
            loaded.manifest.host_promises[0].operation.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Checkpoint
        );
        assert!(matches!(
            loaded.manifest.host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Resolved { .. }
        ));
        assert!(!loaded.blob.is_empty());
        let checkpoint: Vec<CallRecord> = serde_json::from_slice(
            &std::fs::read(
                run_base
                    .join(&loaded.manifest.run_id)
                    .join("checkpoint.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(checkpoint.len(), 1);
        assert_eq!(checkpoint[0].function, "checkpoint");
        assert_eq!(checkpoint[0].args["label"], serde_json::json!("mid-run"));
        assert_eq!(checkpoint[0].args["data"], serde_json::json!({ "step": 1 }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_runs_typescript_sub_agent_with_shared_call_log() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-call-agent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let child_path = dir.join("child.ts");
        let parent_path = dir.join("parent.ts");
        std::fs::write(
            &child_path,
            r#"
                export async function agent(input, chidori) {
                    await chidori.log("child-start");
                    return { child: input.value + 1 };
                }
            "#,
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &parent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const result = await chidori.callAgent({child_path_json}, {{ value: input.value }});
                    return {{ parent: result.child + 1 }};
                }}
                "#
            ),
        )
        .unwrap();

        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );
        let result = engine
            .run(&parent_path, &serde_json::json!({ "value": 40 }))
            .unwrap();

        assert_eq!(result.output, serde_json::json!({ "parent": 42 }));
        let records = result.call_log.into_records();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| record.function == "log"));
        assert!(records.iter().any(|record| record.function == "call_agent"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_rejects_starlark_sub_agent_from_typescript() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-star-call-agent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let child_path = dir.join("child.star");
        let parent_path = dir.join("parent.ts");
        std::fs::write(
            &child_path,
            r#"
def agent(value):
    return {"child": value + 1}
"#,
        )
        .unwrap();
        let child_path_json = serde_json::to_string(&child_path.display().to_string()).unwrap();
        std::fs::write(
            &parent_path,
            format!(
                r#"
                export async function agent(input, chidori) {{
                    const result = await chidori.callAgent({child_path_json}, {{ value: input.value }});
                    return {{ parent: result.child + 1 }};
                }}
                "#
            ),
        )
        .unwrap();

        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );
        let err = match engine.run(&parent_path, &serde_json::json!({ "value": 40 })) {
            Ok(_) => panic!("expected Starlark sub-agent to be rejected"),
            Err(err) => err,
        };

        assert!(err
            .to_string()
            .contains("chidori.callAgent supports .ts agents"));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// `node:` captured effects (crypto + fs) run on the active engine and a
    /// record→replay round-trip reproduces identical output — including the
    /// captured randomness, which is journaled as a `crypto.random` call so a
    /// resumed run draws the exact same bytes. Runs under whichever engine
    /// `CHIDORI_JS_ENGINE` selects, so it guards both the QuickJS and rust paths.
    #[test]
    fn engine_node_builtins_crypto_fs_record_replay_parity() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-node-builtins-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                import { createHash, randomBytes } from "node:crypto";
                import { writeFileSync, readFileSync } from "node:fs";
                export async function agent(input: { msg: string }, chidori) {
                    const hash = createHash("sha256").update(input.msg).digest("hex");
                    const rnd = randomBytes(8).toString("hex");
                    writeFileSync("/work.txt", input.msg + ":" + rnd);
                    const back = readFileSync("/work.txt", "utf8");
                    return { hash, rnd, back };
                }
            "#,
        )
        .unwrap();

        let engine = || {
            Engine::new(
                Arc::new(ProviderRegistry::new()),
                Arc::new(TemplateEngine::new(&dir)),
                Arc::new(tokio::runtime::Runtime::new().unwrap()),
            )
        };

        let recorded = engine()
            .run(&path, &serde_json::json!({ "msg": "hello" }))
            .unwrap();
        let out = recorded.output.clone();
        // sha256("hello") is fixed; the captured random and the file contents are
        // present in the output and must survive replay unchanged.
        assert_eq!(
            out.get("hash").and_then(|v| v.as_str()).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let rnd = out.get("rnd").and_then(|v| v.as_str()).unwrap().to_string();
        assert_eq!(rnd.len(), 16);

        // The captured `crypto.random` is in the log; replaying re-derives the
        // same bytes, so the whole output is byte-identical.
        let replay_log = recorded.call_log.into_records();
        assert!(
            replay_log.iter().any(|r| r.function == "crypto.random"),
            "captured randomness should be journaled as crypto.random"
        );
        let replayed = engine()
            .run_with_replay(&path, &serde_json::json!({ "msg": "hello" }), replay_log)
            .unwrap();
        assert_eq!(replayed.output, out);

        let _ = std::fs::remove_dir_all(dir);
    }
}
