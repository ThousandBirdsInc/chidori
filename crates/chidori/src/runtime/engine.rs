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
    PendingInput, PendingSignal, RuntimeContext, RuntimeEvent,
};
use crate::runtime::snapshot::{
    HostPromiseRecord, RuntimePolicy, SnapshotAbi, SnapshotManifest, SnapshotModuleGraphEntry,
    SnapshotStore, SourceFingerprint,
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
    /// Hands out per-run persistence handles: the filesystem layout under
    /// `persist_base`, teed with a durable mirror when one is configured
    /// (`CHIDORI_RUN_STORE`, `docs/durable-storage.md`). Built alongside
    /// `persist_base`; None when persistence is disabled.
    run_store: Option<crate::runtime::store::RunStoreFactory>,
    /// Warm-resume bridge installed on every run leg's context (see
    /// [`crate::runtime::context::WarmInputBridge`]): the session server sets
    /// this so an `input()` pause parks the live VM instead of unwinding, and
    /// the resume continues in place. None (CLI, tests, embedders) keeps the
    /// classic pause-unwind behavior.
    warm_input_bridge: Option<crate::runtime::context::WarmInputBridge>,
    /// Default root for `chidori.workspace`, used when the run's context has no
    /// workspace root of its own. The CLI points this at the agent's project
    /// directory so `chidori.workspace` works out of the box; an explicit
    /// `CHIDORI_WORKSPACE_ROOT` env var still wins (it populates the context
    /// default, which takes precedence over this fallback).
    workspace_root: Option<PathBuf>,
    /// Default model applied to every run's context, overriding the
    /// environment-derived default (see [`Engine::with_default_model`]).
    default_model: Option<String>,
    /// Allow this run to persist a call log SHORTER than the durable one —
    /// the explicit opt-in for intentional history rewrites
    /// (`resume --until-seq` time travel). Off by default so an
    /// early-diverged resume attempt can never truncate a journal.
    allow_history_rewrite: bool,
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
    /// Set when the agent called `chidori.signal(name)` at a listen point with
    /// an empty mailbox in Pause mode. The caller should treat the run as
    /// suspended on the named listen point and later resume by delivering a
    /// matching signal (replaying the call log with a completed `Signal` host
    /// op at `pending.seq`). Distinct from `paused` so the server knows *which*
    /// named op is waiting. See `docs/signals.md`.
    pub paused_signal: Option<PendingSignal>,
}

/// Persist a resume scaffold for a run on the pure-Rust engine (G2).
///
/// The engine's durability is the `RuntimeContext` call log, not a VM image:
/// resume re-executes the agent from the top and feeds each host effect its
/// recorded result via the call-log replay (`try_replay`), blocking again at the
/// pending frontier. So there is no VM blob to store — we persist a manifest
/// (`InitialTypeScriptStateScaffold` kind) plus the call log, pending operation,
/// host-promise table, capabilities, and VFS, which is exactly what the server's
/// call-log replay resume path consumes.
///
/// The ABI token is `chidori-quickjs` for backward compatibility: the server's
/// resume gate validates the manifest ABI against
/// `SnapshotAbi::current("chidori-quickjs")` — the token the durable format has
/// always used (it predates the QuickJS removal in #39, and the scaffold has
/// always resumed engine-agnostically via call-log replay, never a VM image).
/// Keeping the name leaves the manifest acceptable to the unchanged resume gate
/// and lets artifacts written before the removal still load.
pub(crate) fn persist_journal_scaffold(
    base: &Path,
    run_id: &str,
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
    ctx: &RuntimeContext,
) -> Result<()> {
    ScaffoldPersister::new(base, run_id, path, source, policy)
        .persist(ctx, CheckpointWrite::Compact)
}

/// Whether a scaffold persist rewrites the full `checkpoint.json`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointWrite {
    /// Write only when the in-memory log holds records the O(1) journal
    /// appends did not cover (`RuntimeContext::call_log_checkpoint_dirty`) —
    /// the per-safepoint cadence. Live records are already durable in
    /// `records.jsonl` via `record_call`, and the loader unions the last
    /// checkpoint with the appended tail, so skipping the rewrite loses
    /// nothing.
    IfDirty,
    /// Always write — the compaction points: pause, settle, one-shot scaffold
    /// snapshots.
    Compact,
}

/// Per-run persister for the durable journal scaffold. Caches the pieces that
/// are invariant for the run's lifetime — the entry fingerprint, the module
/// fingerprints + import graph (a disk walk over every imported module), and
/// the serialized code-bundle blob — so the per-host-call safepoints pay for
/// none of them. Before this existed, every safepoint re-walked the module
/// graph from disk twice and rewrote the whole checkpoint: O(history) bytes
/// and O(modules) disk reads per host call.
pub(crate) struct ScaffoldPersister {
    base: PathBuf,
    run_id: String,
    path: PathBuf,
    source: String,
    policy: RuntimePolicy,
    entry: SourceFingerprint,
    /// `(dependency fingerprints, full module graph)` — computed on first use
    /// (set only on success, so a transient read failure retries).
    modules: std::sync::OnceLock<(Vec<SourceFingerprint>, Vec<SnapshotModuleGraphEntry>)>,
    /// The rust engine has no VM image to serialize; resume is call-log
    /// replay. We still write a non-empty blob — the durable code bundle the
    /// journal keys reference — so `SnapshotStore::load()` round-trips and
    /// the on-disk shape matches the established scaffold shape
    /// (manifest + blob + checkpoint + pending). The bytes are run-invariant,
    /// so they are serialized once here and written once per run.
    blob_bytes: Vec<u8>,
    blob_written: std::sync::atomic::AtomicBool,
    /// The longest call log ever durably written for this run — loaded once
    /// from the previous manifest (resume) and advanced with each write.
    /// A resume attempt that diverged or was denied early carries a SHORTER
    /// log than the durable one; letting it compact would destroy recorded
    /// history, so shorter-log writes are skipped (see `persist`).
    checkpoint_floor: std::sync::OnceLock<std::sync::atomic::AtomicUsize>,
}

impl ScaffoldPersister {
    pub(crate) fn new(
        base: &Path,
        run_id: &str,
        path: &Path,
        source: &str,
        policy: &RuntimePolicy,
    ) -> Self {
        let blob = chidori_js::replay::DurableBlob {
            bundle: source.to_string(),
            effects: Vec::new(),
            journal: Vec::new(),
        };
        Self {
            base: base.to_path_buf(),
            run_id: run_id.to_string(),
            path: path.to_path_buf(),
            source: source.to_string(),
            policy: policy.clone(),
            entry: SourceFingerprint::from_source(path, source),
            modules: std::sync::OnceLock::new(),
            blob_bytes: serde_json::to_vec(&blob).unwrap_or_default(),
            blob_written: std::sync::atomic::AtomicBool::new(false),
            checkpoint_floor: std::sync::OnceLock::new(),
        }
    }

    /// The journal-length floor below which this run refuses to compact
    /// (initialized from the previous manifest's `call_log_len`, 0 for a
    /// fresh run). `reset_checkpoint_floor` is the explicit opt-out for
    /// intentional history rewrites (`resume --until-seq` time travel).
    fn checkpoint_floor(&self) -> &std::sync::atomic::AtomicUsize {
        self.checkpoint_floor.get_or_init(|| {
            let previous = SnapshotStore::new(self.base.join(&self.run_id))
                .load_manifest()
                .map(|manifest| manifest.call_log_len)
                .unwrap_or(0);
            std::sync::atomic::AtomicUsize::new(previous)
        })
    }

    pub(crate) fn reset_checkpoint_floor(&self) {
        self.checkpoint_floor()
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn modules(&self) -> Result<&(Vec<SourceFingerprint>, Vec<SnapshotModuleGraphEntry>)> {
        if let Some(cached) = self.modules.get() {
            return Ok(cached);
        }
        // Sources are fixed for the run's lifetime (the engine read them once
        // at start and never re-reads), so the manifest must describe what the
        // run is actually executing — caching is more correct than re-walking,
        // not just cheaper.
        let computed = crate::runtime::typescript::module_graph::snapshot_modules(
            &self.path,
            &self.source,
            &self.policy,
        )?;
        Ok(self.modules.get_or_init(|| computed))
    }

    pub(crate) fn persist(&self, ctx: &RuntimeContext, checkpoint: CheckpointWrite) -> Result<()> {
        use std::sync::atomic::Ordering;

        // Monotonicity guard: a resume attempt that diverged, was denied, or
        // failed BEFORE reaching the durable frontier carries a shorter call
        // log than the one on disk. Persisting it would overwrite recorded
        // history with a truncation — so skip, and leave the longer journal
        // authoritative. (`resume --until-seq` resets the floor explicitly.)
        let floor = self.checkpoint_floor();
        let current_len = ctx.call_log_len();
        if current_len < floor.load(Ordering::SeqCst) {
            tracing::warn!(
                run_id = %self.run_id,
                current = current_len,
                durable = floor.load(Ordering::SeqCst),
                "skipping journal persist: this attempt's call log is shorter than the \
                 durable one (an early-diverged resume must not truncate history)"
            );
            return Ok(());
        }

        let pending = ctx.active_pending_host_operation();
        let compact = checkpoint == CheckpointWrite::Compact;
        let (modules, module_graph) = self.modules()?;
        let mut manifest = SnapshotManifest::new(
            &self.run_id,
            SnapshotAbi::current("chidori-quickjs"),
            self.policy.clone(),
            self.entry.clone(),
            modules.clone(),
            pending,
            ctx.call_log_len(),
        )
        .with_module_graph(module_graph.clone())
        .with_capabilities(ctx.capabilities())
        .with_vfs(ctx.vfs_snapshot())
        .with_default_model(Some(ctx.config().model));
        // The embedded host-promise table has the same freshness contract as
        // checkpoint.json: a compaction-time snapshot. Runtime resume never
        // reads it (deliveries and replay load `host_promises.json` ∪ the
        // per-op blobs); embedding the whole table per safepoint was the
        // other O(history)-per-call term.
        if compact {
            manifest = manifest.with_host_promises(ctx.host_promise_records());
        }
        // Write through the run's store handle when persistence is enabled, so
        // a configured durable mirror receives the scaffold too; identical
        // on-disk layout either way.
        let store = match ctx.store() {
            Some(store) => SnapshotStore::with_store(self.base.join(&self.run_id), store),
            None => SnapshotStore::new(self.base.join(&self.run_id)),
        };
        if !self.blob_written.load(Ordering::Acquire) {
            store.put_snapshot_blob(&manifest, &self.blob_bytes)?;
            self.blob_written.store(true, Ordering::Release);
        }
        store.put_manifest(&manifest)?;
        if compact || ctx.call_log_checkpoint_dirty() {
            let call_log = ctx.call_log().into_records();
            store.write_call_log(&call_log)?;
            ctx.clear_call_log_checkpoint_dirty(call_log.len());
            floor.fetch_max(call_log.len(), Ordering::SeqCst);
        }
        if compact {
            store.compact_host_promises(&manifest.host_promises)?;
        }
        store.put_pending(manifest.pending.as_ref())
    }
}

/// Install the durable-scaffold safepoints on a prepared context: the
/// manifest + pending (and, when the log is checkpoint-dirty, the full
/// checkpoint) are persisted before every live host side effect and after
/// every recorded completion, so a crash mid-run always leaves a resumable
/// artifact. Shared by the engine's run path and the detached-agent
/// supervisor (`host_agent`), which runs modules directly.
pub(crate) fn install_journal_scaffold_safepoints(
    base: &Path,
    run_id: &str,
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
    ctx: &RuntimeContext,
) {
    let persister = Arc::new(ScaffoldPersister::new(base, run_id, path, source, policy));
    install_scaffold_safepoints_with(persister, ctx);
}

/// Wire an existing per-run [`ScaffoldPersister`] into both host-operation
/// safepoints (pre-effect and post-completion), each at the `IfDirty`
/// checkpoint cadence.
pub(crate) fn install_scaffold_safepoints_with(
    persister: Arc<ScaffoldPersister>,
    ctx: &RuntimeContext,
) {
    {
        let persister = persister.clone();
        let safepoint_ctx = ctx.clone();
        ctx.set_host_operation_safepoint(HostOperationSafepoint::new(move |_operation| {
            persister.persist(&safepoint_ctx, CheckpointWrite::IfDirty)
        }));
    }
    let completion_ctx = ctx.clone();
    ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
        move |_record| persister.persist(&completion_ctx, CheckpointWrite::IfDirty),
    ));
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
            run_store: None,
            warm_input_bridge: None,
            workspace_root: None,
            default_model: None,
            allow_history_rewrite: false,
        }
    }

    pub fn with_warm_input_bridge(
        mut self,
        bridge: crate::runtime::context::WarmInputBridge,
    ) -> Self {
        self.warm_input_bridge = Some(bridge);
        self
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
        // `shared` memoizes per base, so per-request engine builds (the
        // server) reuse one factory — one SQLite connection / HTTP relay.
        self.run_store = Some(crate::runtime::store::RunStoreFactory::shared(&base));
        self.persist_base = Some(base);
        self
    }

    /// Persist runs through a pre-built [`RunStoreFactory`] — the server path,
    /// which constructs the factory once at startup (a durable mirror holds
    /// shared state like a SQLite connection) instead of once per engine.
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn with_run_store(mut self, factory: crate::runtime::store::RunStoreFactory) -> Self {
        self.persist_base = Some(factory.run_base().to_path_buf());
        self.run_store = Some(factory);
        self
    }

    /// Set the default `chidori.workspace` root, applied to each run whose
    /// context doesn't already carry one (i.e. when `CHIDORI_WORKSPACE_ROOT` is
    /// unset). Lets the CLI scope the workspace to the agent's project dir.
    /// Default model for prompts that don't set one in code — overrides the
    /// context's environment-derived default. `resume`/`branch-rerun` pass
    /// the model recorded in the run's manifest here so replays and
    /// continuations keep the run's model instead of re-deriving it from
    /// whatever environment hosts them.
    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    /// Opt this run into intentional journal truncation (`resume --until-seq`
    /// time travel). See the `allow_history_rewrite` field.
    pub fn with_history_rewrite_allowed(mut self, allowed: bool) -> Self {
        self.allow_history_rewrite = allowed;
        self
    }

    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }

    /// Resume a persisted, paused `chidori.branch` sub-run of the run at
    /// `run_dir` by answering its pending `input()` prompt. The branch replays
    /// its checkpoint with a synthetic input record and continues live to its
    /// next outcome (out-of-band — the parent run is untouched history).
    /// Returns the updated `BranchOutcome` JSON.
    pub fn resume_branch(&self, run_dir: &Path, branch_id: &str, response: &str) -> Result<Value> {
        let backend = self.branch_backend(branch_id)?;
        crate::runtime::host_branch::resume_branch(&backend, run_dir, branch_id, response)
            .map_err(|err| anyhow::anyhow!(err))
    }

    /// Re-run a persisted `chidori.branch` sub-run fresh from its parent
    /// anchor with whatever its stored `source.ts` now contains — the
    /// edit-and-rerun flow. Returns the updated `BranchOutcome` JSON.
    pub fn rerun_branch(&self, run_dir: &Path, branch_id: &str) -> Result<Value> {
        let backend = self.branch_backend(branch_id)?;
        crate::runtime::host_branch::rerun_branch(&backend, run_dir, branch_id)
            .map_err(|err| anyhow::anyhow!(err))
    }

    /// List the persisted branch stores of the run at `run_dir`.
    pub fn list_branches(run_dir: &Path) -> Result<Vec<Value>> {
        crate::runtime::host_branch::list_branches(run_dir).map_err(|err| anyhow::anyhow!(err))
    }

    /// The host backend for out-of-band branch operations: same wiring as a
    /// run's backend (providers, policy + seeded approvals, tools, MCP); the
    /// placeholder context is swapped for the rebuilt branch context inside
    /// `host_branch`.
    fn branch_backend(
        &self,
        branch_id: &str,
    ) -> Result<crate::runtime::typescript::bindings::HostBindingBackend> {
        let policy = RuntimePolicy::from_env_for_durable_run(branch_id)?;
        let mut seeded = PolicyCache::default();
        for (target, args) in &self.approvals {
            seeded.approve(target, args);
        }
        Ok(
            crate::runtime::typescript::bindings::HostBindingBackend::for_runtime(
                RuntimeContext::new(),
                self.providers.clone(),
                self.template_engine.clone(),
                self.tokio_rt.clone(),
                self.policy.clone(),
                Arc::new(StdMutex::new(seeded)),
                policy,
                self.tools.clone(),
                self.mcp.clone(),
            ),
        )
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

    /// `run`, announcing the fresh run's id on stderr before execution
    /// starts. The CLI uses this so the id `chidori resume` needs after a
    /// crash is already on record — a SIGKILLed process loses whatever stdout
    /// it had buffered, and the success-path id print never happens.
    pub fn run_announced(&self, path: &Path, inputs: &Value) -> Result<RunResult> {
        let ctx = RuntimeContext::new();
        eprintln!("Run id: {}", ctx.run_id());
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

    /// `run_with_replay` under the run's ORIGINAL id: with persistence
    /// configured, live continuation past the replay frontier journals into
    /// the same run directory, so a crash-resume that itself crashes resumes
    /// from the new frontier instead of the old one. This is the CLI
    /// `chidori resume` path.
    pub fn resume_run(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        run_id: &str,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay(replay_log);
        ctx.set_run_id(run_id.to_string());
        self.run_with_context(path, inputs, ctx)
    }

    #[allow(dead_code)] // Lib-facade entry point; the bin target compiles the module tree separately and never calls it.
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

    /// Run an agent with a replay log *and* a live event sender. Prior turns
    /// in the log replay silently (their host calls short-circuit before any
    /// provider request, so they emit no prompt stream); only calls past the
    /// end of the log execute live and stream token deltas. Used by the
    /// `chidori chat` REPL to stream just the newest turn's reply.
    pub fn run_with_replay_streaming(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        sender: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay(replay_log);
        ctx.set_event_sender(sender);
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent while forwarding live events and pausing on input or
    /// approval requests. This is the in-process equivalent of the session
    /// server's interactive execution path for embedders that already own
    /// their UI/event loop.
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
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
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
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

    #[allow(dead_code)] // Lib-facade entry point; the bin target compiles the module tree separately and never calls it.
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
    #[allow(dead_code)] // Lib-facade entry point; the bin target compiles the module tree separately and never calls it.
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
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
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

    /// As `run_replay_pausable_with_host_promises_and_vfs`, but also threads the
    /// durable signal mailbox (`signals/inbox.json`) into the resumed run so a
    /// re-run that reaches a `chidori.signal(name)`/`pollSignal(name)` listen
    /// point drains any queued entries instead of pausing. See `docs/signals.md`
    /// §9 (the resume worker loads the inbox).
    pub fn run_replay_pausable_with_host_promises_vfs_and_signals(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: crate::runtime::vfs::Vfs,
        signal_inbox: Vec<crate::runtime::snapshot::QueuedSignal>,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay_host_promises_vfs_and_signals(
            replay_log,
            host_promises,
            vfs,
            signal_inbox,
        );
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    /// As above, preserving the original run id (so the resumed run keeps its
    /// persisted run directory) AND threading the signal mailbox. This is the
    /// method the server's resume/signal-delivery paths use.
    #[allow(clippy::too_many_arguments)]
    pub fn run_replay_pausable_with_host_promises_vfs_signals_preserving_run_id(
        &self,
        path: &Path,
        inputs: &Value,
        replay_log: Vec<CallRecord>,
        host_promises: Vec<HostPromiseRecord>,
        vfs: crate::runtime::vfs::Vfs,
        signal_inbox: Vec<crate::runtime::snapshot::QueuedSignal>,
        run_id: String,
    ) -> Result<RunResult> {
        let ctx = RuntimeContext::with_replay_host_promises_vfs_and_signals(
            replay_log,
            host_promises,
            vfs,
            signal_inbox,
        );
        ctx.set_run_id(run_id);
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(path, inputs, ctx)
    }

    /// Run an agent on a context the caller prepared (event sender, input
    /// mode, replay state, run id). Used by the server's streaming supervisor,
    /// which keeps a handle on the live context so externally delivered
    /// signals can be enqueued into the running agent's mailbox in-memory
    /// (`docs/signals.md` Phase 3) and swaps in a fresh replay context for
    /// each in-process resume.
    pub fn run_with_prepared_context(
        &self,
        path: &Path,
        inputs: &Value,
        ctx: RuntimeContext,
    ) -> Result<RunResult> {
        self.run_with_context(path, inputs, ctx)
    }

    /// Resume/replay an interactive streaming run with persisted host-promise
    /// state. Used by embedders that mirror the server session interaction
    /// without routing through HTTP.
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
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
        // Fall back to the engine's default workspace root when the context
        // doesn't already have one. `CHIDORI_WORKSPACE_ROOT` populates the
        // context default (via `RuntimeContext::new`), so an explicit env var
        // still takes precedence; this only fills in the CLI's project-dir
        // default so `chidori.workspace` works without extra configuration.
        if ctx.workspace_root().is_none() {
            if let Some(ref root) = self.workspace_root {
                ctx.set_workspace_root(root.clone());
            }
        }
        if let Some(ref model) = self.default_model {
            ctx.set_default_model(model.clone());
        }
        if let Some(ref bridge) = self.warm_input_bridge {
            ctx.set_warm_input_bridge(bridge.clone());
        }

        // Enable persistence if configured: the filesystem run dir, teed with
        // the durable mirror when one is set up (`docs/durable-storage.md`).
        if let Some(ref factory) = self.run_store {
            let run_id = ctx.run_id();
            let run_dir = factory.run_base().join(&run_id);
            ctx.enable_persistence_with_store(run_dir, factory.store_for(&run_id));
            // Save the input alongside the checkpoint for later resume/trace.
            if let Some(store) = ctx.store() {
                let _ = store.put_blob(
                    "input.json",
                    serde_json::to_string_pretty(inputs)
                        .unwrap_or_default()
                        .as_bytes(),
                );
            }
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

            // Run the agent on the pure-Rust `chidori-js` engine — the only JS
            // engine in the tree (the QuickJS/C path was removed in #39).
            // Host effects flow through host_core/RuntimeContext, so the durable
            // call log and the host-call span tree behave identically; this path
            // additionally emits JS-level function spans when tracing is on.
            // Build the runtime host backend so every `chidori.*` effect routes
            // through identical durable machinery (call log, replay, policy,
            // MCP, OTEL).
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
            // artifact on disk (G2). Stores the engine's journal-shaped
            // manifest. One persister per run: the module graph, entry
            // fingerprint, and code-bundle blob are computed once and reused
            // by every safepoint. The initial persist is `IfDirty`: a fresh
            // run has nothing to checkpoint, and a resume must NOT rewrite
            // `checkpoint.json` from its still-empty in-memory log — that
            // would truncate the previous turn's journal while it is being
            // replayed.
            let persister = self.persist_base.as_ref().map(|base| {
                Arc::new(ScaffoldPersister::new(
                    base, &run_id, path, &source, &policy,
                ))
            });
            if let Some(ref persister) = persister {
                if self.allow_history_rewrite {
                    // `resume --until-seq` time travel: the caller truncated
                    // the journal on purpose; the shorter-log guard must not
                    // pin the run to its longer past.
                    persister.reset_checkpoint_floor();
                }
                install_scaffold_safepoints_with(persister.clone(), &ctx);
                persister.persist(&ctx, CheckpointWrite::IfDirty)?;
            }

            // Pause surfacing on the rust path (G1). A `chidori.input()` in
            // Pause mode or a policy-approval block sets `pending_input` /
            // `pending_approval` on the ctx and signals a pause by returning
            // the `PAUSE_MARKER` sentinel from the host effect — which the
            // rust engine throws as a JS error and bubbles up here as `Err`.
            // We surface that as a paused `RunResult` so the resume flow has
            // something to resume. The check runs
            // on `Ok` too in case the agent caught the sentinel and returned.
            let surface_pause = |ctx: &RuntimeContext| -> Option<RunResult> {
                // A pause is externally visible (the caller renders a prompt /
                // waits on a signal), so flush buffered journal writes first —
                // the output-gate point for pauses. Failures are logged: the
                // strict gate in host dispatch already stops further effects.
                if let Err(err) = ctx.flush_store() {
                    tracing_error!(run_id = %ctx.run_id(), error = %err, "flushing run store at pause");
                }
                if let Some(pending) = ctx.take_pending_input() {
                    if let Some(ref persister) = persister {
                        let _ = persister.persist(ctx, CheckpointWrite::Compact);
                    }
                    emit_otel(ctx, None);
                    return Some(RunResult {
                        output: Value::Null,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: Some(pending),
                        paused_approval: None,
                        paused_signal: None,
                    });
                }
                if let Some(approval) = ctx.take_pending_approval() {
                    if let Some(ref persister) = persister {
                        let _ = persister.persist(ctx, CheckpointWrite::Compact);
                    }
                    emit_otel(ctx, None);
                    return Some(RunResult {
                        output: Value::Null,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: None,
                        paused_approval: Some(approval),
                        paused_signal: None,
                    });
                }
                // A `chidori.signal(name)` listen point with an empty mailbox
                // sets `pending_signal` and bails with `PAUSE_MARKER`, exactly
                // like `input` — surface it as a paused run, not a failure.
                if let Some(signal) = ctx.take_pending_signal() {
                    if let Some(ref persister) = persister {
                        let _ = persister.persist(ctx, CheckpointWrite::Compact);
                    }
                    emit_otel(ctx, None);
                    return Some(RunResult {
                        output: Value::Null,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: None,
                        paused_approval: None,
                        paused_signal: Some(signal),
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
                    if let Some(ref persister) = persister {
                        if let Some(store) = ctx.store() {
                            let _ = store.put_blob(
                                "output.json",
                                serde_json::to_string_pretty(&output)
                                    .unwrap_or_default()
                                    .as_bytes(),
                            );
                        }
                        let _ = persister.persist(&ctx, CheckpointWrite::Compact);
                    }
                    // Output gate: the run's result is not surfaced until the
                    // journal is durable. A strict-mode persistence failure
                    // fails the run here instead of returning an output whose
                    // recording was lost (`docs/durable-storage.md`).
                    ctx.flush_store()?;
                    emit_otel(&ctx, None);
                    Ok(RunResult {
                        output,
                        call_log: ctx.call_log(),
                        run_id: ctx.run_id(),
                        paused: None,
                        paused_approval: None,
                        paused_signal: None,
                    })
                }
                Err(e) => {
                    if let Some(paused) = surface_pause(&ctx) {
                        return Ok(paused);
                    }
                    let err_msg = e.to_string();
                    tracing_error!(agent = %agent_name, run_id = %run_id, error = %err_msg, "rust-engine agent run failed");
                    if let Some(ref persister) = persister {
                        let _ = persister.persist(&ctx, CheckpointWrite::Compact);
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
                ..Default::default()
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
            let loaded = SnapshotStore::new(&run_dir).load()?;
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
            // The pending promise must be durable BEFORE the provider runs.
            // It lives in the per-op blob ∪ table union (the manifest embeds
            // the table only at compaction points, like checkpoint.json).
            let store = crate::runtime::store::FsRunStore::new(&run_dir);
            let promises = crate::runtime::snapshot::load_host_promise_records(&store)?;
            anyhow::ensure!(
                promises.len() == 1,
                "expected one pending host promise before provider"
            );
            anyhow::ensure!(
                matches!(
                    promises[0].state,
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
                ..Default::default()
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
                import type { Chidori } from "chidori:agent";
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
                    await fetch("https://example.invalid");
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
            default_reason: None,
            overlay: None,
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
                    await fetch("not a url");
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
    fn engine_call_agent_resolves_relative_path_against_project_root() {
        // Sub-agent paths like "agents/child.ts" must resolve against the
        // project root (the TemplateEngine base dir), not the process cwd —
        // the host (e.g. the builder's oneshot runner) may spawn the engine
        // from an unrelated working directory.
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-call-agent-relative-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::write(
            dir.join("agents").join("child.ts"),
            r#"
                export async function agent(input, chidori) {
                    return { value: input.value + 1 };
                }
            "#,
        )
        .unwrap();
        let parent_path = dir.join("parent.ts");
        std::fs::write(
            &parent_path,
            r#"
                export async function agent(input, chidori) {
                    return await chidori.callAgent("agents/child.ts", { value: input.value });
                }
            "#,
        )
        .unwrap();
        assert_ne!(
            std::env::current_dir().unwrap(),
            dir,
            "test must run with cwd outside the project for the resolution to be exercised"
        );
        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );
        let result = engine
            .run(&parent_path, &serde_json::json!({ "value": 41 }))
            .expect("relative sub-agent path should resolve against project root");
        assert_eq!(result.output, serde_json::json!({ "value": 42 }));

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
                    await chidori.mark("mid-run", { step: 1 });
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
        assert_eq!(checkpoint[0].function, "mark");
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
    /// resumed run draws the exact same bytes. Runs on the pure-Rust
    /// `chidori-js` engine (the only engine in the tree).
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

    /// An agent awaiting `chidori.signal("review")` with an empty mailbox pauses
    /// (surfaced as `RunResult.paused_signal`, not a failure). Delivering the
    /// signal — via a completed `Signal` host promise plus the synthetic
    /// `signal` CallRecord, the durable resume shape from `docs/signals.md` §9 —
    /// completes the run with the delivered payload, and a full re-run from the
    /// recorded call_log reproduces identical output.
    #[test]
    fn engine_pauses_and_resumes_typescript_signal() {
        let dir =
            std::env::temp_dir().join(format!("chidori-engine-ts-signal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const review = await chidori.signal("review");
                    return { decision: review.payload.decision, by: review.from.id };
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

        // Empty mailbox → pauses on the named listen point.
        let paused = engine()
            .run_pausable(&path, &serde_json::json!({}))
            .unwrap();
        assert!(paused.paused.is_none());
        assert!(paused.paused_approval.is_none());
        let pending = paused.paused_signal.expect("expected a signal pause");
        assert_eq!(pending.name, "review");
        assert_eq!(pending.seq, 1);
        assert!(paused.call_log.into_records().is_empty());

        // Deliver the signal: a completed Signal host promise (match key
        // {name}) carrying the {name,payload,from} result. `replay_completed_
        // host_operation` injects the synthetic `signal` CallRecord on resume.
        let delivered = serde_json::json!({
            "name": "review",
            "payload": { "decision": "approve" },
            "from": { "kind": "human", "id": "mara" },
        });
        let host_promises = vec![crate::runtime::snapshot::HostPromiseRecord {
            operation: crate::runtime::snapshot::PendingHostOperation::new(
                crate::runtime::snapshot::HostOperationId(1),
                pending.seq,
                crate::runtime::snapshot::PendingHostOperationKind::Signal,
                serde_json::json!({ "name": "review" }),
            ),
            state: crate::runtime::snapshot::HostPromiseState::Resolved {
                value: delivered.clone(),
                completed_at: chrono::Utc::now(),
            },
        }];
        let resumed = engine()
            .run_replay_pausable_with_host_promises(
                &path,
                &serde_json::json!({}),
                Vec::new(),
                host_promises,
            )
            .unwrap();
        assert!(resumed.paused_signal.is_none());
        assert_eq!(
            resumed.output,
            serde_json::json!({ "decision": "approve", "by": "mara" })
        );
        let records = resumed.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "signal");
        assert_eq!(records[0].result, delivered);

        // Full re-run from the recorded call_log reproduces identical output
        // without any mailbox or delivery.
        let rerun = engine()
            .run_with_replay(&path, &serde_json::json!({}), records.clone())
            .unwrap();
        assert_eq!(rerun.output, resumed.output);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// `chidori.step(name, fn)` — the durable value checkpoint
    /// (`docs/value-checkpoints.md`): the callback runs once and its result is
    /// journaled as a `step` CallRecord; replay returns the recorded value
    /// WITHOUT re-running the callback. Proven by replaying against an edited
    /// agent whose step body now throws — the recorded value wins.
    #[test]
    fn engine_step_memoizes_value_checkpoint_on_replay() {
        let dir =
            std::env::temp_dir().join(format!("chidori-engine-ts-step-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const plan = await chidori.step("plan", () => {
                        let total = 0;
                        for (let i = 0; i <= 1000; i++) total += i;
                        return { total, source: "computed" };
                    });
                    return { total: plan.total, source: plan.source };
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

        let recorded = engine().run(&path, &serde_json::json!({})).unwrap();
        assert_eq!(
            recorded.output,
            serde_json::json!({ "total": 500500, "source": "computed" })
        );
        let records = recorded.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "step");
        assert_eq!(records[0].args, serde_json::json!({ "name": "plan" }));
        assert_eq!(
            records[0].result,
            serde_json::json!({ "total": 500500, "source": "computed" })
        );

        // Replay against an edited body that would throw if re-executed: the
        // journaled value must win (the whole point of the value checkpoint).
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const plan = await chidori.step("plan", () => {
                        throw new Error("step callback must not re-run on replay");
                    });
                    return { total: plan.total, source: plan.source };
                }
            "#,
        )
        .unwrap();
        let replayed = engine()
            .run_with_replay(&path, &serde_json::json!({}), records)
            .unwrap();
        assert_eq!(replayed.output, recorded.output);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A step callback that throws journals the error, and replay re-throws the
    /// recorded error without re-running the callback — even when the edited
    /// callback would now succeed.
    #[test]
    fn engine_step_replays_recorded_error() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-step-err-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    try {
                        await chidori.step("parse", () => { throw new Error("bad parse"); });
                        return { ok: true };
                    } catch (e) {
                        return { caught: String(e) };
                    }
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

        let recorded = engine().run(&path, &serde_json::json!({})).unwrap();
        let caught = recorded
            .output
            .get("caught")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        assert!(caught.contains("bad parse"), "got: {caught}");
        let records = recorded.call_log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "step");
        assert!(records[0].error.as_deref().unwrap().contains("bad parse"));

        // Edited callback now succeeds — but the journaled error replays.
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    try {
                        await chidori.step("parse", () => ({ fine: true }));
                        return { ok: true };
                    } catch (e) {
                        return { caught: String(e) };
                    }
                }
            "#,
        )
        .unwrap();
        let replayed = engine()
            .run_with_replay(&path, &serde_json::json!({}), records)
            .unwrap();
        assert_eq!(replayed.output, recorded.output);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The pure-compute contract is enforced loudly: host effects, captured
    /// randomness, and async callbacks are all refused inside a step body.
    #[test]
    fn engine_step_enforces_pure_compute_contract() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-step-pure-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                import { randomBytes } from "node:crypto";
                export async function agent(input, chidori) {
                    const tryStep = async (name, fn) => {
                        try { await chidori.step(name, fn); return "ok"; }
                        catch (e) { return String(e); }
                    };
                    return {
                        effect: await tryStep("effect", () => { chidori.log("nope"); return 1; }),
                        random: await tryStep("random", () => randomBytes(4).toString("hex")),
                        timer: await tryStep("timer", () => { setTimeout(() => {}, 1); return 1; }),
                        asyncFn: await tryStep("asyncFn", async () => 1),
                    };
                }
            "#,
        )
        .unwrap();

        let engine = Engine::new(
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(&dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
        );
        let result = engine.run(&path, &serde_json::json!({})).unwrap();
        let field = |k: &str| {
            result
                .output
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string()
        };
        assert!(
            field("effect").contains("not allowed inside chidori.step(\"effect\")"),
            "got: {}",
            field("effect")
        );
        assert!(
            field("random").contains("not allowed inside chidori.step(\"random\")"),
            "got: {}",
            field("random")
        );
        assert!(
            field("timer").contains("not allowed inside chidori.step(\"timer\")"),
            "got: {}",
            field("timer")
        );
        assert!(
            field("asyncFn").contains("must return synchronously"),
            "got: {}",
            field("asyncFn")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The P6 acceptance: a resume after a pause does not re-pay the step. The
    /// agent computes a step, then pauses on `input()`; the resume replays from
    /// the journal against an edited step body that would throw — and completes
    /// with the recorded step value.
    #[test]
    fn engine_step_skips_recompute_on_input_resume() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-ts-step-resume-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const plan = await chidori.step("plan", () => ({ total: 42 }));
                    const answer = await chidori.input("Approve the plan?");
                    return { total: plan.total, approved: answer === "yes" };
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
        let pending = paused.paused.expect("expected an input pause");
        assert_eq!(pending.seq, 2, "step takes seq 1, input pauses at seq 2");
        let mut replay = paused.call_log.into_records();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].function, "step");

        // Resume = journal replay + the synthetic input record, against an
        // edited step body that must not run.
        replay.push(CallRecord {
            seq: pending.seq,
            parent_seq: None,
            function: "input".to_string(),
            args: serde_json::json!({ "prompt": pending.prompt }),
            result: serde_json::json!("yes"),
            duration_ms: 0,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        });
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const plan = await chidori.step("plan", () => {
                        throw new Error("resume must not re-pay the step");
                    });
                    const answer = await chidori.input("Approve the plan?");
                    return { total: plan.total, approved: answer === "yes" };
                }
            "#,
        )
        .unwrap();
        let resumed = engine()
            .run_replay_pausable(&path, &serde_json::json!({}), replay)
            .unwrap();
        assert!(resumed.paused.is_none());
        assert_eq!(
            resumed.output,
            serde_json::json!({ "total": 42, "approved": true })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_with_replay_streaming_streams_only_the_new_turn() {
        // The `chidori chat` streaming path: replaying a prior turn must emit no
        // prompt deltas (it short-circuits before the provider), so only the
        // newest turn's reply streams.
        let dir = std::env::temp_dir().join(format!(
            "chidori-engine-chat-stream-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                export async function agent(input, chidori) {
                    const chat = chidori.conversation({ system: "terse" });
                    for (const m of input.messages) await chat.say(m);
                    return { history: chat.history() };
                }
            "#,
        )
        .unwrap();

        let engine = |content: &str| {
            let mut providers = ProviderRegistry::new();
            providers.register(Box::new(FixedTestProvider {
                content: content.to_string(),
                input_tokens: 10,
                output_tokens: 5,
            }));
            Engine::new(
                Arc::new(providers),
                Arc::new(TemplateEngine::new(&dir)),
                Arc::new(tokio::runtime::Runtime::new().unwrap()),
            )
        };

        // Turn 1: one live message, capture the call log.
        let turn1 = engine("reply one")
            .run(&path, &serde_json::json!({ "messages": ["hi"] }))
            .unwrap();
        let call_log = turn1.call_log.into_records();

        // Turn 2: replay turn 1 and stream the new message. Collect deltas.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        let deltas = std::thread::spawn(move || {
            let mut rx = rx;
            let mut collected = String::new();
            while let Some(evt) = rx.blocking_recv() {
                if let RuntimeEvent::PromptDelta { delta, .. } = evt {
                    collected.push_str(&delta);
                }
            }
            collected
        });
        let turn2 = engine("reply two")
            .run_with_replay_streaming(
                &path,
                &serde_json::json!({ "messages": ["hi", "again"] }),
                call_log,
                tx,
            )
            .unwrap();
        let streamed = deltas.join().unwrap();

        // Both replies are in the returned history (turn 1 replayed, turn 2
        // live), but ONLY the new turn streamed deltas.
        let history = turn2.output["history"].as_array().unwrap();
        let texts: Vec<&str> = history
            .iter()
            .filter(|t| t["role"] == "assistant")
            .map(|t| t["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, vec!["reply one", "reply two"]);
        assert_eq!(streamed, "reply two");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Pins the checkpoint-write cadence: steady-state safepoints must NOT
    /// rewrite `checkpoint.json` (the O(1) `records.jsonl` append plus the
    /// loader's union already carry the log), while replay-seeded logs
    /// (checkpoint-dirty) and explicit compaction points must.
    #[test]
    fn scaffold_safepoints_skip_checkpoint_rewrite_until_dirty_or_compaction() {
        use crate::runtime::store::RunStore as _;

        let base =
            std::env::temp_dir().join(format!("chidori-scaffold-cadence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let agent_path = base.join("agent.ts");
        std::fs::write(&agent_path, "export async function agent() { return 1; }").unwrap();
        let source = std::fs::read_to_string(&agent_path).unwrap();

        let record = |seq: u64, function: &str| CallRecord {
            seq,
            parent_seq: None,
            function: function.to_string(),
            args: serde_json::Value::Null,
            result: serde_json::Value::Null,
            duration_ms: 0,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        };

        // Live run: records flow through `record_call`'s O(1) append.
        let ctx = RuntimeContext::new();
        let run_id = ctx.run_id();
        let run_dir = ctx.enable_persistence(base.clone());
        let policy = RuntimePolicy::durable_default(&run_id);
        let persister = ScaffoldPersister::new(&base, &run_id, &agent_path, &source, &policy);

        // Run-start persist: scaffold artifacts exist, no checkpoint yet.
        persister.persist(&ctx, CheckpointWrite::IfDirty).unwrap();
        assert!(run_dir
            .join(crate::runtime::snapshot::SNAPSHOT_MANIFEST_FILE)
            .is_file());
        assert!(!run_dir.join("checkpoint.json").exists());

        // A live record + its completion safepoint: still no checkpoint
        // rewrite, but the journal loads complete via the union.
        ctx.record_call(record(1, "prompt"));
        persister.persist(&ctx, CheckpointWrite::IfDirty).unwrap();
        assert!(
            !run_dir.join("checkpoint.json").exists(),
            "steady-state safepoint must not rewrite the checkpoint"
        );
        let loaded = crate::runtime::store::FsRunStore::new(&run_dir)
            .load_call_log()
            .unwrap()
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].function, "prompt");

        // A compaction point (pause/settle) writes the full checkpoint.
        persister.persist(&ctx, CheckpointWrite::Compact).unwrap();
        let checkpoint: Vec<CallRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join("checkpoint.json")).unwrap())
                .unwrap();
        assert_eq!(checkpoint.len(), 1);

        // Resume replay: replayed pushes bypass the append path, so the log
        // is checkpoint-dirty and the first safepoint persists one full
        // checkpoint covering the replayed prefix + synthetic record.
        let resumed = RuntimeContext::with_replay(vec![record(1, "prompt"), record(2, "input")]);
        resumed.enable_persistence_with_store(
            run_dir.clone(),
            Arc::new(crate::runtime::store::FsRunStore::new(&run_dir)),
        );
        assert!(resumed.try_replay(1).is_some());
        assert!(resumed.try_replay(2).is_some());
        assert!(resumed.call_log_checkpoint_dirty());
        let resumed_persister =
            ScaffoldPersister::new(&base, &run_id, &agent_path, &source, &policy);
        resumed_persister
            .persist(&resumed, CheckpointWrite::IfDirty)
            .unwrap();
        assert!(!resumed.call_log_checkpoint_dirty());
        let checkpoint: Vec<CallRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join("checkpoint.json")).unwrap())
                .unwrap();
        assert_eq!(
            checkpoint.len(),
            2,
            "first safepoint past the replay frontier must persist replayed + synthetic records"
        );

        // Once clean, further live records go back to append-only cadence.
        resumed.record_call(record(3, "tool"));
        resumed_persister
            .persist(&resumed, CheckpointWrite::IfDirty)
            .unwrap();
        let checkpoint: Vec<CallRecord> =
            serde_json::from_slice(&std::fs::read(run_dir.join("checkpoint.json")).unwrap())
                .unwrap();
        assert_eq!(
            checkpoint.len(),
            2,
            "clean safepoint must leave the checkpoint alone"
        );
        let loaded = crate::runtime::store::FsRunStore::new(&run_dir)
            .load_call_log()
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.len(),
            3,
            "the union must still see the appended tail"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
