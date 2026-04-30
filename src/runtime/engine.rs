use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use serde_json::Value;
use starlark::environment::{Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::syntax::AstModule;

use crate::runtime::dialect::studio_dialect;
use starlark::values::Value as StarlarkValue;

use crate::mcp::McpManager;
use crate::policy::{PolicyCache, PolicyConfig};
use crate::providers::ProviderRegistry;
use crate::runtime::call_log::{CallLog, CallRecord};
use crate::runtime::context::{
    InputMode, PendingApproval, PendingInput, RuntimeContext, RuntimeEvent,
};
use crate::runtime::host_functions::{self, json_to_starlark, HostState};
use crate::runtime::template::TemplateEngine;
use crate::tools::ToolRegistry;

/// The Starlark execution engine.
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

    fn globals() -> Globals {
        let mut builder = GlobalsBuilder::standard();
        host_functions::host_functions(&mut builder);
        builder.build()
    }

    /// Parse and validate a .star file without executing it.
    pub fn check(&self, path: &Path) -> Result<()> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let _ast = AstModule::parse(
            &path.display().to_string(),
            source,
            &studio_dialect(),
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    /// Run an agent .star file with the given JSON inputs.
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
        if let Some(run_span) = crate::runtime::otel::start_run_span(&agent_name, &run_id) {
            ctx.set_otel_run(run_span);
        }

        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;

        let ast = AstModule::parse(
            &path.display().to_string(),
            source,
            &studio_dialect(),
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?;

        let globals = Self::globals();
        let module = Module::new();

        // Seed the per-run policy cache with any pre-approved targets the
        // caller has accumulated (see /sessions/{id}/approve).
        let mut seeded = PolicyCache::default();
        for (target, args) in &self.approvals {
            seeded.approve(target, args);
        }
        let host_state = HostState {
            ctx: ctx.clone(),
            providers: self.providers.clone(),
            template_engine: self.template_engine.clone(),
            tokio_rt: self.tokio_rt.clone(),
            tools: self.tools.clone(),
            policy: self.policy.clone(),
            policy_cache: Arc::new(StdMutex::new(seeded)),
            mcp: self.mcp.clone(),
        };

        // First pass: evaluate the module.
        {
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&host_state);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }

        let frozen = module
            .freeze()
            .map_err(|e| anyhow::anyhow!("{:?}", e))?;

        let agent_fn = frozen.get("agent").map_err(|_| {
            anyhow::anyhow!(
                "No `agent` function found in {}. Define: def agent(...):",
                path.display()
            )
        })?;

        let module2 = Module::new();
        let mut eval2 = Evaluator::new(&module2);
        eval2.extra = Some(&host_state);

        let kwargs = build_kwargs(eval2.heap(), inputs);
        let kwargs_refs: Vec<(&str, StarlarkValue)> =
            kwargs.iter().map(|(k, v)| (k.as_str(), *v)).collect();

        let result = eval2.eval_function(agent_fn.value(), &[], &kwargs_refs);

        match result {
            Ok(value) => {
                let output = starlark_value_to_json(value);

                if let Some(ref base) = self.persist_base {
                    let run_dir = base.join(ctx.run_id());
                    let _ = std::fs::write(
                        run_dir.join("output.json"),
                        serde_json::to_string_pretty(&output).unwrap_or_default(),
                    );
                }

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
                // If the agent is paused on input() or on a policy approval
                // gate, the host function stored the pending state before
                // returning an error. Treat either as a clean suspension.
                if let Some(pending) = ctx.take_pending_input() {
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
                let err_msg = format!("{}", e);
                if let Some(otel) = ctx.otel_run() {
                    otel.finish(Some(&err_msg));
                }
                Err(anyhow::anyhow!("{}", err_msg))
            }
        }
    }
}

fn build_kwargs<'v>(
    heap: &'v starlark::values::Heap,
    inputs: &Value,
) -> Vec<(String, StarlarkValue<'v>)> {
    match inputs {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_starlark(heap, v)))
            .collect(),
        _ => Vec::new(),
    }
}

fn starlark_value_to_json(v: StarlarkValue) -> Value {
    use starlark::values::dict::DictRef;
    use starlark::values::list::ListRef;

    if v.is_none() {
        Value::Null
    } else if let Some(b) = v.unpack_bool() {
        Value::Bool(b)
    } else if let Some(i) = v.unpack_i32() {
        Value::Number(i.into())
    } else if let Some(s) = v.unpack_str() {
        Value::String(s.to_string())
    } else if let Some(dict) = DictRef::from_value(v) {
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            let key = k
                .unpack_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| k.to_repr());
            map.insert(key, starlark_value_to_json(v));
        }
        Value::Object(map)
    } else if let Some(list) = ListRef::from_value(v) {
        let items: Vec<Value> = list.iter().map(starlark_value_to_json).collect();
        Value::Array(items)
    } else {
        let repr = v.to_repr();
        let json_str = repr
            .replace("True", "true")
            .replace("False", "false")
            .replace("None", "null")
            .replace('\'', "\"");
        serde_json::from_str(&json_str).unwrap_or(Value::String(repr))
    }
}
