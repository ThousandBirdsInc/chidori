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

fn persist_ts_snapshot_manifest_scaffold(
    base: &Path,
    run_id: &str,
    path: &Path,
    source: &str,
    inputs: &Value,
    policy: &RuntimePolicy,
    ctx: &RuntimeContext,
) -> Result<()> {
    let call_log = ctx.call_log().into_records();
    let pending = ctx.active_pending_host_operation();
    let host_promises = ctx.host_promise_records();
    let modules =
        crate::runtime::typescript::snapshot::snapshot_module_fingerprints(path, source, policy)?;
    let module_graph =
        crate::runtime::typescript::snapshot::snapshot_module_graph(path, source, policy)?;
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
    .with_host_promises(host_promises.clone());

    if let Some(snapshot) = ctx.capture_live_vm_snapshot() {
        let snapshot = snapshot.with_context(|| {
            format!(
                "capturing live TypeScript VM snapshot for persisted run {}",
                run_id
            )
        })?;
        return SnapshotStore::new(base.join(run_id))
            .save_live_vm_snapshot(&manifest, &snapshot, &call_log);
    }

    if !host_promises.is_empty() {
        match crate::runtime::typescript::snapshot::snapshot_live_agent_state(
            path,
            source,
            inputs.clone(),
            policy.clone(),
            &host_promises,
            pending.as_ref(),
        ) {
            Ok(snapshot) => {
                return SnapshotStore::new(base.join(run_id))
                    .save_live_vm_snapshot(&manifest, &snapshot, &call_log);
            }
            Err(err) => {
                tracing::warn!(
                    run_id = %run_id,
                    error = %err,
                    "failed to persist live TypeScript VM snapshot from host promise records; falling back to initial snapshot scaffold"
                );
            }
        }
    }

    match crate::runtime::typescript::snapshot::snapshot_initial_agent_state(
        path,
        source,
        policy.clone(),
    ) {
        Ok(snapshot_blob) => {
            SnapshotStore::new(base.join(run_id)).save(&manifest, &snapshot_blob, &call_log)
        }
        Err(err) => {
            tracing::warn!(
                run_id = %run_id,
                error = %err,
                "failed to persist initial TypeScript VM snapshot; saving manifest only"
            );
            SnapshotStore::new(base.join(run_id)).save_manifest_only(&manifest, &call_log)
        }
    }
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
        let ctx = RuntimeContext::with_replay_and_host_promises(replay_log, host_promises);
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
        let ctx = RuntimeContext::new();
        ctx.set_event_sender(sender);
        ctx.set_input_mode(InputMode::Pause);
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
        let ctx = RuntimeContext::with_replay_and_host_promises(replay_log, host_promises);
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

        if path.extension().and_then(|e| e.to_str()) == Some("ts") {
            let policy = RuntimePolicy::from_env_for_durable_run(&run_id)?;
            let source = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            if let Some(ref base) = self.persist_base {
                let safepoint_base = base.clone();
                let safepoint_run_id = run_id.clone();
                let safepoint_path = path.to_path_buf();
                let safepoint_source = source.clone();
                let safepoint_inputs = inputs.clone();
                let safepoint_policy = policy.clone();
                let safepoint_ctx = ctx.clone();
                ctx.set_host_operation_safepoint(HostOperationSafepoint::new(move |_operation| {
                    persist_ts_snapshot_manifest_scaffold(
                        &safepoint_base,
                        &safepoint_run_id,
                        &safepoint_path,
                        &safepoint_source,
                        &safepoint_inputs,
                        &safepoint_policy,
                        &safepoint_ctx,
                    )
                }));
                let completion_base = base.clone();
                let completion_run_id = run_id.clone();
                let completion_path = path.to_path_buf();
                let completion_source = source.clone();
                let completion_inputs = inputs.clone();
                let completion_policy = policy.clone();
                let completion_ctx = ctx.clone();
                ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
                    move |_record| {
                        persist_ts_snapshot_manifest_scaffold(
                            &completion_base,
                            &completion_run_id,
                            &completion_path,
                            &completion_source,
                            &completion_inputs,
                            &completion_policy,
                            &completion_ctx,
                        )
                    },
                ));
                persist_ts_snapshot_manifest_scaffold(
                    base, &run_id, path, &source, inputs, &policy, &ctx,
                )?;
            }
            let runtime =
                crate::runtime::typescript::engine::TypeScriptVmRuntime::new(policy.clone())?;
            let mut seeded = PolicyCache::default();
            for (target, args) in &self.approvals {
                seeded.approve(target, args);
            }
            let result = runtime.run_agent_source_with_context(
                path,
                &source,
                inputs,
                ctx.clone(),
                self.providers.clone(),
                self.template_engine.clone(),
                self.tokio_rt.clone(),
                self.policy.clone(),
                Arc::new(StdMutex::new(seeded)),
                self.tools.clone(),
                self.mcp.clone(),
            );

            return match result {
                Ok(output) => {
                    if let Some(pending) = ctx.take_pending_input() {
                        if let Some(ref base) = self.persist_base {
                            let _ = persist_ts_snapshot_manifest_scaffold(
                                base, &run_id, path, &source, inputs, &policy, &ctx,
                            );
                        }
                        if let Some(otel) = ctx.otel_run() {
                            otel.finish(None);
                        }
                        return Ok(RunResult {
                            output: Value::Null,
                            call_log: ctx.call_log(),
                            run_id: ctx.run_id(),
                            paused: Some(pending),
                            paused_approval: None,
                        });
                    }
                    if let Some(approval) = ctx.take_pending_approval() {
                        if let Some(ref base) = self.persist_base {
                            let _ = persist_ts_snapshot_manifest_scaffold(
                                base, &run_id, path, &source, inputs, &policy, &ctx,
                            );
                        }
                        if let Some(otel) = ctx.otel_run() {
                            otel.finish(None);
                        }
                        return Ok(RunResult {
                            output: Value::Null,
                            call_log: ctx.call_log(),
                            run_id: ctx.run_id(),
                            paused: None,
                            paused_approval: Some(approval),
                        });
                    }
                    if let Some(ref base) = self.persist_base {
                        let run_dir = base.join(ctx.run_id());
                        let _ = std::fs::write(
                            run_dir.join("output.json"),
                            serde_json::to_string_pretty(&output).unwrap_or_default(),
                        );
                        let _ = persist_ts_snapshot_manifest_scaffold(
                            base, &run_id, path, &source, inputs, &policy, &ctx,
                        );
                    }

                    info!(agent = %agent_name, run_id = %run_id, "typescript agent run ok");
                    if let Some(otel) = ctx.otel_run() {
                        otel.finish(None);
                    }
                    Ok(RunResult {
                        output,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: None,
                        paused_approval: None,
                    })
                }
                Err(e) => {
                    if let Some(pending) = ctx.take_pending_input() {
                        if let Some(ref base) = self.persist_base {
                            let _ = persist_ts_snapshot_manifest_scaffold(
                                base, &run_id, path, &source, inputs, &policy, &ctx,
                            );
                        }
                        if let Some(otel) = ctx.otel_run() {
                            otel.finish(None);
                        }
                        return Ok(RunResult {
                            output: Value::Null,
                            call_log: ctx.call_log(),
                            run_id: ctx.run_id(),
                            paused: Some(pending),
                            paused_approval: None,
                        });
                    }
                    if let Some(approval) = ctx.take_pending_approval() {
                        if let Some(ref base) = self.persist_base {
                            let _ = persist_ts_snapshot_manifest_scaffold(
                                base, &run_id, path, &source, inputs, &policy, &ctx,
                            );
                        }
                        if let Some(otel) = ctx.otel_run() {
                            otel.finish(None);
                        }
                        return Ok(RunResult {
                            output: Value::Null,
                            call_log: ctx.call_log(),
                            run_id: ctx.run_id(),
                            paused: None,
                            paused_approval: Some(approval),
                        });
                    }
                    let err_msg = e.to_string();
                    tracing_error!(agent = %agent_name, run_id = %run_id, error = %err_msg, "typescript agent run failed");
                    if let Some(otel) = ctx.otel_run() {
                        otel.finish(Some(&err_msg));
                    }
                    Err(e)
                }
            };
        }

        let err_msg = format!(
            "unsupported agent file {}: TypeScript `.ts` agents are required",
            path.display()
        );
        tracing_error!(agent = %agent_name, run_id = %run_id, error = %err_msg, "agent run failed");
        if let Some(otel) = ctx.otel_run() {
            otel.finish(Some(&err_msg));
        }
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
    fn engine_persists_typescript_snapshot_manifest_on_pause() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-snapshot-manifest-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("Continue?");
                    return { answer };
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
        let paused = engine.run_pausable(&path, &serde_json::json!({})).unwrap();

        let loaded = SnapshotStore::new(run_base.join(&paused.run_id))
            .load()
            .unwrap();
        let manifest = &loaded.manifest;
        assert_eq!(manifest.run_id, paused.run_id);
        assert_eq!(manifest.entry.path, path);
        assert_eq!(
            manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(manifest.call_log_len, 0);
        let pending = manifest.pending.as_ref().unwrap();
        assert_eq!(
            pending.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Input
        );
        assert_eq!(pending.args, serde_json::json!({ "prompt": "Continue?" }));
        assert_eq!(manifest.host_promises.len(), 1);
        assert_eq!(manifest.host_promises[0].operation.id, pending.id);
        assert_eq!(
            manifest.host_promises[0].operation.kind,
            crate::runtime::snapshot::PendingHostOperationKind::Input
        );
        assert!(matches!(
            manifest.host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Pending
        ));
        assert!(!loaded.blob.is_empty());
        chidori_quickjs::RuntimeSnapshot(loaded.blob.clone())
            .ensure_restorable()
            .unwrap();
        let mut live_runtime = chidori_quickjs::SnapshotRuntime::restore(&loaded.blob).unwrap();
        live_runtime
            .resolve_host_promise(
                chidori_quickjs::HostPromiseId(pending.id.0),
                serde_json::json!("yes"),
            )
            .unwrap();
        assert_eq!(
            live_runtime.run_jobs_until_blocked().unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "answer": "yes" }))
        );
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
            crate::runtime::snapshot::PendingHostOperationKind::Input
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
    fn engine_live_vm_snapshotter_persists_live_blob_on_input_pause() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-live-input-pause-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const answer = await chidori.input("Continue?");
                    return { answer };
                }
            "#,
        )
        .unwrap();
        let live_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"live-input-vm");
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);
        ctx.set_live_vm_snapshotter(crate::runtime::context::LiveVmSnapshotter::new({
            let live_snapshot = live_snapshot.clone();
            move || Ok(live_snapshot.clone())
        }));

        let paused = engine
            .run_with_context(&path, &serde_json::json!({}), ctx)
            .unwrap();

        assert!(paused.paused.is_some());
        let loaded = SnapshotStore::new(run_base.join(&paused.run_id))
            .load()
            .unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(
            loaded
                .manifest
                .pending
                .as_ref()
                .map(|pending| pending.kind.clone()),
            Some(crate::runtime::snapshot::PendingHostOperationKind::Input)
        );
        assert_eq!(loaded.blob, live_snapshot.0);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_persists_typescript_snapshot_blob_on_policy_approval_pause() {
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
    fn engine_live_vm_snapshotter_persists_live_blob_on_policy_approval_pause() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-live-approval-pause-{}",
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
        let live_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"live-approval-vm");
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
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);
        ctx.set_live_vm_snapshotter(crate::runtime::context::LiveVmSnapshotter::new({
            let live_snapshot = live_snapshot.clone();
            move || Ok(live_snapshot.clone())
        }));

        let paused = engine
            .run_with_context(&path, &serde_json::json!({}), ctx)
            .unwrap();

        assert!(paused.paused_approval.is_some());
        let loaded = SnapshotStore::new(run_base.join(&paused.run_id))
            .load()
            .unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(
            loaded
                .manifest
                .pending
                .as_ref()
                .map(|pending| pending.kind.clone()),
            Some(crate::runtime::snapshot::PendingHostOperationKind::Http)
        );
        assert_eq!(loaded.blob, live_snapshot.0);

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
    fn engine_live_vm_snapshotter_persists_live_blob_around_failed_host_side_effect() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-live-host-safepoint-{}",
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
        let live_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"live-http-vm");
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());
        let ctx = RuntimeContext::new();
        ctx.set_live_vm_snapshotter(crate::runtime::context::LiveVmSnapshotter::new({
            let live_snapshot = live_snapshot.clone();
            move || Ok(live_snapshot.clone())
        }));

        let err = match engine.run_with_context(&path, &serde_json::json!({}), ctx) {
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
        let loaded = SnapshotStore::new(run_dir).load().unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert!(loaded.manifest.pending.is_none());
        assert_eq!(loaded.manifest.host_promises.len(), 1);
        assert!(matches!(
            loaded.manifest.host_promises[0].state,
            crate::runtime::snapshot::HostPromiseState::Rejected { .. }
        ));
        assert_eq!(loaded.blob, live_snapshot.0);

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
    fn engine_live_vm_snapshotter_persists_live_blob_around_prompt_provider_call() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-live-prompt-safepoint-{}",
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
        let live_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"live-prompt-vm");
        let run_base = dir.join(".chidori").join("runs");
        let observed_pending_prompt = Arc::new(std::sync::Mutex::new(false));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(InspectingTestProvider {
            run_base: run_base.clone(),
            observed_pending_prompt: observed_pending_prompt.clone(),
            expected_snapshot_kind: Some(crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm),
            expected_blob: Some(live_snapshot.0.clone()),
        }));
        let engine = Engine::new(
            Arc::new(providers),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base.clone());
        let ctx = RuntimeContext::new();
        ctx.set_live_vm_snapshotter(crate::runtime::context::LiveVmSnapshotter::new({
            let live_snapshot = live_snapshot.clone();
            move || Ok(live_snapshot.clone())
        }));

        let result = engine
            .run_with_context(&path, &serde_json::json!({}), ctx)
            .unwrap();

        assert_eq!(
            result.output,
            serde_json::json!({ "text": "provider result" })
        );
        assert!(*observed_pending_prompt.lock().unwrap());
        let run_dir = std::fs::read_dir(&run_base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let loaded = SnapshotStore::new(run_dir).load().unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert!(loaded.manifest.pending.is_none());
        assert_eq!(loaded.blob, live_snapshot.0);

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
    fn typescript_snapshot_manifest_records_local_module_fingerprints() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-module-manifest-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let module_path = dir.join("lib.ts");
        let source = r#"
            import { value } from "./lib.ts";
            export async function agent() { return value; }
        "#;
        let module_source = "export const value = 1;";
        std::fs::write(&path, source).unwrap();
        std::fs::write(&module_path, module_source).unwrap();
        let run_id = "run-with-module";
        let run_base = dir.join(".chidori").join("runs");
        let ctx = RuntimeContext::new();
        ctx.enable_persistence(run_base.clone());
        let policy = RuntimePolicy::durable_default(run_id);

        persist_ts_snapshot_manifest_scaffold(
            &run_base,
            run_id,
            &path,
            source,
            &serde_json::Value::Null,
            &policy,
            &ctx,
        )
        .unwrap();

        let manifest = SnapshotStore::new(run_base.join(run_id))
            .load_manifest()
            .unwrap();
        assert_eq!(
            manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::InitialTypeScriptStateScaffold
        );
        assert_eq!(manifest.modules.len(), 1);
        assert_eq!(
            manifest.modules[0],
            SourceFingerprint::from_source(&module_path, module_source)
        );
        assert_eq!(manifest.module_graph.len(), 2);
        let entry = manifest
            .module_graph
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        assert_eq!(entry.imports[0].specifier, "./lib.ts");
        assert_eq!(entry.imports[0].resolved_path, Some(module_path));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn typescript_persistence_prefers_registered_live_vm_snapshotter() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-live-vm-snapshotter-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let source = "export async function agent() { return { ok: true }; }";
        std::fs::write(&path, source).unwrap();
        let run_id = "run-with-live-vm";
        let run_base = dir.join(".chidori").join("runs");
        let ctx = RuntimeContext::new();
        ctx.enable_persistence(run_base.clone());
        ctx.set_live_vm_snapshotter(crate::runtime::context::LiveVmSnapshotter::new(|| {
            Ok(chidori_quickjs::RuntimeSnapshot::from_payload(
                b"live-vm-payload",
            ))
        }));
        let policy = RuntimePolicy::durable_default(run_id);

        persist_ts_snapshot_manifest_scaffold(
            &run_base,
            run_id,
            &path,
            source,
            &serde_json::Value::Null,
            &policy,
            &ctx,
        )
        .unwrap();

        let loaded = SnapshotStore::new(run_base.join(run_id)).load().unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            crate::runtime::snapshot::SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(
            loaded.blob,
            chidori_quickjs::RuntimeSnapshot::from_payload(b"live-vm-payload").0
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn typescript_live_vm_snapshotter_failure_blocks_persisted_run() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-live-vm-snapshotter-fail-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            "export async function agent() { return { should_not_run: true }; }",
        )
        .unwrap();
        let ctx = RuntimeContext::new();
        ctx.set_live_vm_snapshotter(crate::runtime::context::LiveVmSnapshotter::new(|| {
            anyhow::bail!("snapshot capture failed")
        }));
        let run_base = dir.join(".chidori").join("runs");
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(dir.clone())),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        )
        .with_persist_base(run_base);

        let err = match engine.run_with_context(&path, &serde_json::json!({}), ctx) {
            Ok(_) => panic!("expected live snapshotter failure"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("capturing live TypeScript VM snapshot"));

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
}
