//! Pure-Rust JS engine integration — the only JavaScript engine.
//!
//! This adapter drives the in-tree `chidori-js` engine for all TypeScript
//! agent, tool, and sub-agent execution (`engine.rs`, `server.rs`, `bindings.rs`).
//!
//! Durability here is the deterministic-replay journal (see
//! `docs/pure-rust-js-engine-plan.md`), not a VM-image snapshot. Because the
//! journal references the code bundle by content hash, `snapshot`/`restore`
//! round-trip a self-describing blob of `{bundle, effects, journal}` rather than
//! threading the bundle through the trait signature.

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use crate::runtime::context::RuntimeContext;
use crate::runtime::snapshot::{
    CryptoPolicy, FsPolicy, HostOperationId, JsRunState, RuntimePolicy, SnapshotCapableJsEngine,
    TimerPolicy, TypeScriptImportPolicy,
};
use crate::runtime::typescript::bindings::HostBindingBackend;
use crate::runtime::typescript::transpile::{transpile_module, TranspileOptions};

pub use chidori_js::replay::ReplayRuntime;

/// A durable Rust-engine instance behind the `SnapshotCapableJsEngine` trait.
pub struct RustReplayEngine {
    rt: ReplayRuntime,
    effects: Vec<String>,
}

impl RustReplayEngine {
    /// Begin a fresh durable execution of `bundle`, exposing the named host
    /// effects as global async functions.
    pub fn start(bundle: &str, effects: &[&str]) -> Self {
        RustReplayEngine {
            rt: ReplayRuntime::record(bundle, effects),
            effects: effects.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The effect name + JSON args for a host op the engine is blocked on.
    pub fn pending_op(&self, id: HostOperationId) -> Option<(String, Value)> {
        self.rt.pending_op(id.0)
    }

    pub fn console(&self) -> &[String] {
        self.rt.console()
    }

    /// Install a JS-level trace observer on the underlying VM so function
    /// enter/exit/suspend/resume become a nested OTEL span tree. Build the
    /// observer with [`crate::runtime::otel::RunSpan::js_trace_observer`] from
    /// the run's span. Off unless installed; gate the call on
    /// [`js_tracing_enabled`].
    pub fn install_trace_sink(&mut self, sink: Box<dyn chidori_js::TraceObserver>) {
        self.rt.vm.trace_sink = Some(sink);
    }
}

/// Whether JS-level tracing should be turned on: opt-in via `CHIDORI_TRACE_JS=1`
/// AND an OTLP endpoint configured (no endpoint ⇒ nowhere to send spans, so stay
/// zero-cost). Default off keeps the engine's hot path untouched.
pub fn js_tracing_enabled() -> bool {
    std::env::var("CHIDORI_TRACE_JS").as_deref() == Ok("1")
        && std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
}

/// Maximum JS-level span nesting depth (bounds span volume on deep recursion).
const JS_TRACE_MAX_DEPTH: usize = 64;

/// Run a TypeScript agent through the pure-Rust `chidori-js` engine end-to-end,
/// returning the agent's output.
///
/// Host effects are routed through `host_core` / `RuntimeContext`, so the
/// durable call log, replay, and the host-call OTEL span tree (including nesting
/// under `tool` calls) all behave consistently. When [`js_tracing_enabled`], a
/// JS-level trace observer is installed so function enter/exit becomes spans
/// nested under the run's host-call tree.
///
/// Agents export an async/sync `agent(input)` (or call `run(handler)`) and use
/// the global `chidori` object. The full `chidori.*` effect surface (`log`,
/// `input`, `prompt`, `tool`, `callAgent`, `http`, `memory`, `template`,
/// `checkpoint`, `workspace.*`) is wired through the shared
/// [`HostBindingBackend`], with policy enforcement and MCP. Relative and `node:`
/// multi-file imports are resolved and linked by the engine; nested TypeScript
/// tools and sub-agents run natively on this same engine.
pub fn run_agent(
    path: &Path,
    source: &str,
    inputs: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    // Agents define their entrypoint with `run(handler)`; fall back to a legacy
    // `agent` export if `run(...)` wasn't called.
    run_module(path, source, "agent", inputs, backend)
}

/// Reframe a chidori-js entrypoint error so an uncaught JS exception surfaces
/// as `JavaScript exception: <message>` — the shape the durable format, the
/// host-call span tree, and the SDKs expect. chidori-js stringifies a thrown
/// `Error` as `"<Name>: <message>"`; we strip the standard error-class prefix
/// to recover the bare message and apply the host framing. Pause sentinels pass through untouched — they are control
/// flow, not exceptions, and `engine.rs` / `host_core` detect them by substring.
fn js_exception_message(err: &str) -> String {
    if err.contains(crate::runtime::context::PAUSE_MARKER) {
        return err.to_string();
    }
    const ERROR_NAMES: &[&str] = &[
        "Error",
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
        "AggregateError",
    ];
    for name in ERROR_NAMES {
        if let Some(rest) = err.strip_prefix(&format!("{name}: ")) {
            return format!("JavaScript exception: {rest}");
        }
    }
    format!("JavaScript exception: {err}")
}

/// Run a nested TypeScript **tool** file natively on the rust engine (G4).
///
/// Re-enters [`run_module`] with the tool's `run(args)` entrypoint. The
/// same `backend` (hence the same `RuntimeContext`) is threaded through, so the
/// tool's host effects nest under the parent tool call (`parent_seq`) and share
/// the durable call log, policy, MCP, and OTEL span tree. A suspension inside
/// the tool (e.g. `chidori.input()` in Pause mode) surfaces as the usual
/// `PAUSE_MARKER` error and propagates to the parent run.
pub(crate) fn run_tool_file(
    path: &Path,
    kwargs: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading tool {}: {e}", path.display()))?;
    run_module(path, &source, "run", kwargs, backend)
}

/// Run a nested TypeScript **sub-agent** file natively on the rust engine (G4).
/// Mirrors [`run_tool_file`] but invokes the `agent(input)` entrypoint.
pub(crate) fn run_agent_file(
    path: &Path,
    input: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading sub-agent {}: {e}", path.display()))?;
    run_module(path, &source, "agent", input, backend)
}

/// Resource limits applied to every rust-engine agent run, read from the
/// environment so a deployment can tune (or disable) each without a rebuild.
struct ExecutionLimits {
    /// Opcode budget — bounds *pure-JS compute* and is latency-independent (time
    /// blocked in a synchronous host effect does not consume it), so a runaway
    /// `while (true) {}` terminates with a `RangeError`. `None` disables.
    /// Env `CHIDORI_JS_OP_BUDGET` (default 5e9; `0` disables).
    op_budget: Option<u64>,
    /// Live heap growth ceiling in bytes for this run, enforced by the watchdog
    /// via the counting allocator's per-run meter (allocations made on the run's
    /// own thread — see [`crate::mem_guard`]). `None` disables. Env
    /// `CHIDORI_JS_MEM_CAP_MB` (default 4096; `0` disables).
    mem_cap: Option<usize>,
    /// Watchdog sampling interval for the memory cap and deadline checks.
    /// Env `CHIDORI_JS_MEM_POLL_MS` (default 10; clamped to at least 1).
    /// A run can overshoot the cap by what it allocates within one interval,
    /// so tighten this together with the cap when confining untrusted code.
    poll_interval: Duration,
    /// Optional wall-clock deadline. `None` disables (the default).
    /// Env `CHIDORI_JS_DEADLINE_MS`.
    ///
    /// CAUTION: wall-clock time includes time blocked in *synchronous host
    /// effects* (LLM / tool / http calls run inline on this thread), so a tight
    /// deadline can abort an agent that is merely waiting on a slow tool. It is
    /// off by default for that reason — prefer `op_budget` to bound compute.
    /// Enable it only where host effects are known-fast (e.g. confining untrusted
    /// code with a short hard limit).
    deadline: Option<Duration>,
}

impl ExecutionLimits {
    fn from_env() -> Self {
        fn env_u64(key: &str) -> Option<u64> {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        }
        let op_budget = match env_u64("CHIDORI_JS_OP_BUDGET").unwrap_or(5_000_000_000) {
            0 => None,
            n => Some(n),
        };
        let mem_cap = match env_u64("CHIDORI_JS_MEM_CAP_MB").unwrap_or(4096) {
            0 => None,
            mb => Some((mb as usize).saturating_mul(1024 * 1024)),
        };
        let deadline = match env_u64("CHIDORI_JS_DEADLINE_MS").unwrap_or(0) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        };
        let poll_interval =
            Duration::from_millis(env_u64("CHIDORI_JS_MEM_POLL_MS").unwrap_or(10).max(1));
        ExecutionLimits {
            op_budget,
            mem_cap,
            poll_interval,
            deadline,
        }
    }
}

/// RAII guard that installs the execution limits on a VM and, when a memory cap
/// or deadline is configured, runs a background watchdog that trips the VM's
/// cooperative-cancellation flag once either is exceeded. The watchdog is always
/// joined on drop — including the panic-unwind path — so it never outlives the
/// run or leaks a thread.
struct ExecutionGuard {
    done: Arc<AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
    /// Keeps the per-run allocation meter registered on the run thread for the
    /// lifetime of the run. Declared after `watchdog` is irrelevant — `Drop`
    /// joins the watchdog explicitly before this guard unregisters.
    _meter: Option<crate::mem_guard::RunMeterGuard>,
}

impl ExecutionGuard {
    fn install(vm: &mut chidori_js::Vm) -> Self {
        let limits = ExecutionLimits::from_env();
        if let Some(budget) = limits.op_budget {
            vm.op_budget = Some(budget);
        }
        let interrupt = Arc::new(AtomicBool::new(false));
        vm.interrupt = Some(interrupt.clone());

        // Per-run accounting: register a meter on this thread (the thread the
        // VM runs on) so the cap measures this run's own allocations rather
        // than process-wide growth — concurrent runs no longer trip each
        // other's caps.
        let meter_guard = limits
            .mem_cap
            .map(|_| crate::mem_guard::RunMeterGuard::install());

        let done = Arc::new(AtomicBool::new(false));
        // Only spend a thread when there is something time- or memory-based to
        // watch; the opcode budget is enforced inline by the VM and needs none.
        let watchdog = if limits.mem_cap.is_some() || limits.deadline.is_some() {
            let done_w = done.clone();
            let deadline_at = limits.deadline.map(|d| Instant::now() + d);
            let mem_cap = limits.mem_cap;
            let meter = meter_guard.as_ref().map(|g| g.handle());
            let poll_interval = limits.poll_interval;
            Some(std::thread::spawn(move || loop {
                if done_w.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(at) = deadline_at {
                    if Instant::now() >= at {
                        interrupt.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                if let (Some(cap), Some(meter)) = (mem_cap, meter.as_ref()) {
                    if crate::mem_guard::run_meter_bytes(meter) > cap {
                        interrupt.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                std::thread::sleep(poll_interval);
            }))
        } else {
            None
        };
        ExecutionGuard {
            done,
            watchdog,
            _meter: meter_guard,
        }
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.watchdog.take() {
            let _ = handle.join();
        }
    }
}

/// Recover a human-readable message from a caught panic payload.
fn panic_payload_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Transpile `source`, run it as a module on a fresh `chidori-js` engine, and
/// invoke its entrypoint with `input`. The entrypoint is whatever the module
/// passed to `run(handler)`, or `fallback_export` (e.g. `agent` for agents). The
/// VM's `chidori.*` effects are forwarded to `backend.dispatch`, the durable
/// host machinery in `host_core`.
fn run_module(
    path: &Path,
    source: &str,
    fallback_export: &str,
    input: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    // `Node` accepts relative `./foo` imports *and* allowlisted `node:` builtins
    // (the special-cased `chidori` SDK import is stripped by transpilation). This
    // is the durable default so `node:fs`/`crypto`/`timers` reach the
    // captured-effect natives installed below.
    let opts = TranspileOptions {
        import_policy: TypeScriptImportPolicy::Node,
    };
    let js = transpile_module(path, source, &opts)?;

    let mut engine = chidori_js::Engine::new();
    if js_tracing_enabled() {
        if let Some(ctx) = backend.runtime_ctx() {
            if let Some(run) = ctx.otel_run() {
                engine.vm.trace_sink =
                    Some(Box::new(run.js_trace_observer(&js, JS_TRACE_MAX_DEPTH)));
            }
        }
    }
    // Captured-effect natives (`node:` crypto/fs) + the determinism prelude
    // (process env, TextEncoder/atob, Web Crypto, virtual timers). Installed only
    // for the runtime backend — the recorder/metadata backend has no policy or
    // call log to capture into, and `node:`-using agent code doesn't run there.
    if let (Some(policy), Some(ctx)) = (backend.runtime_policy(), backend.runtime_ctx()) {
        let sync = build_sync_native_dispatch(ctx.clone(), policy.clone());
        engine.install_sync_natives(SYNC_NATIVE_NAMES, sync);
        let prelude = rust_engine_prelude(&policy);
        engine
            .eval(&prelude)
            .map_err(|e| anyhow::anyhow!("installing node: builtin prelude: {e}"))?;
    }
    let backend = backend.clone();
    let dispatch: Rc<dyn Fn(&str, &Value) -> std::result::Result<Value, String>> =
        Rc::new(move |effect: &str, args: &Value| backend.dispatch(effect, args));
    engine.install_chidori_effects(dispatch);
    // Install the JS-level `chidori` SDK sugar (tryCall/retry/parallel + the
    // memory.set/get/delete/clear wrappers).
    // These are pure-JS helpers layered on top of the native host object, so they
    // must run *after* `install_chidori_effects` (the memory sugar wraps the
    // native `chidori.memory`, and the script's guarded workspace shim no-ops
    // because the rust engine already exposes a native `chidori.workspace`).
    // Without this the meta-agent's `chidori.retry(...)`/`chidori.parallel(...)`
    // calls hit `undefined is not a function`.
    engine
        .eval(crate::runtime::typescript::helpers::CHIDORI_JS_HELPERS_SCRIPT)
        .map_err(|e| anyhow::anyhow!("installing chidori JS SDK helpers: {e}"))?;
    // Install the base networking surface (`globalThis.fetch` + Headers/Request/
    // Response) over the captured `__chidori_http` host op that
    // `install_chidori_effects` just defined. This replaces the platform's
    // networking APIs, so every network call — including ones made inside a
    // dependency — is policy-gated, pausable, and recorded. The `node:http`/
    // `node:https` client shims route through the same host op.
    engine
        .eval(crate::runtime::typescript::helpers::FETCH_POLYFILL)
        .map_err(|e| anyhow::anyhow!("installing fetch polyfill: {e}"))?;
    let slot = engine.install_entrypoint();

    let entry_key = path.to_string_lossy().to_string();
    // Resolve each `(specifier, importer)` to a sibling `.ts`/`.js` file (or, for
    // `node:` specifiers, the synthetic builtin shim) and hand the linker its
    // transpiled ES module source.
    let mut load =
        |specifier: &str, importer_key: &str| -> std::result::Result<(String, String), String> {
            if let Some(name) = specifier.strip_prefix("node:") {
                // Serve the shim by name under a stable synthetic key. The shim's own
                // `node:` imports (e.g. `node:buffer`) recurse through this same
                // branch; its body is plain JS, so it needs no transpilation.
                let src = crate::runtime::typescript::builtins::shim_source(name)
                    .ok_or_else(|| format!("unsupported node: builtin '{specifier}'"))?;
                return Ok((format!("node:{name}"), src.to_string()));
            }
            load_module_source(specifier, importer_key)
        };

    // Install per-run resource limits (opcode budget + memory/deadline watchdog)
    // before any agent code runs, and isolate the host from an engine panic: a
    // bug in the interpreter must surface as an error, not unwind into the server.
    let _guard = ExecutionGuard::install(&mut engine.vm);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.run_entrypoint_graph(&entry_key, &js, input, &slot, fallback_export, &mut load)
    }));
    // Break the heap's Rc cycles before the engine drops: the result is already
    // a host `serde_json::Value`, and without this every agent run leaks its
    // realm + agent object graph in a long-lived server process.
    engine.vm.dispose();
    match outcome {
        Ok(result) => result.map_err(|e| anyhow::anyhow!(js_exception_message(&e))),
        Err(panic) => Err(anyhow::anyhow!(
            "rust engine panicked: {}",
            panic_payload_message(panic.as_ref())
        )),
    }
}

/// Resolve `specifier` relative to `importer_key`'s directory, read the file, and
/// transpile it to ES module source — the host half of the rust engine's module
/// loader (the linker lives in `chidori-js`).
fn load_module_source(
    specifier: &str,
    importer_key: &str,
) -> std::result::Result<(String, String), String> {
    let importer = Path::new(importer_key);
    let dir = importer.parent().unwrap_or_else(|| Path::new("."));
    let resolved =
        crate::runtime::typescript::transpile::resolve_relative_import(importer, dir, specifier, 0)
            .map_err(|e| e.to_string())?;
    let key = resolved.to_string_lossy().to_string();
    let src = std::fs::read_to_string(&resolved)
        .map_err(|e| format!("reading module {}: {e}", resolved.display()))?;
    let js = transpile_module(
        &resolved,
        &src,
        &TranspileOptions {
            import_policy: TypeScriptImportPolicy::Node,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok((key, js))
}

/// The synchronous `__chidori_*` natives the `node:` builtin shims and the
/// determinism prelude call inline. Paired with their declared arity.
const SYNC_NATIVE_NAMES: &[(&str, u32)] = &[
    ("__chidori_crypto_hash", 2),
    ("__chidori_crypto_hmac", 3),
    ("__chidori_crypto_random", 1),
    ("__chidori_fs_read", 1),
    ("__chidori_fs_write", 2),
    ("__chidori_fs_append", 2),
    ("__chidori_fs_exists", 1),
    ("__chidori_fs_readdir", 1),
    ("__chidori_fs_mkdir", 2),
    ("__chidori_fs_rm", 3),
    ("__chidori_fs_rename", 2),
    ("__chidori_fs_stat", 1),
    ("__chidori_note_capability", 1),
];

fn b64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(s: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("invalid base64: {e}"))
}

fn crypto_policy_guard(policy: &RuntimePolicy) -> std::result::Result<(), String> {
    if policy.crypto == CryptoPolicy::Disabled {
        return Err("node:crypto is disabled by Chidori runtime policy (crypto=disabled)".into());
    }
    Ok(())
}

fn fs_policy_guard(policy: &RuntimePolicy) -> std::result::Result<(), String> {
    match policy.fs {
        FsPolicy::Captured => Ok(()),
        FsPolicy::Disabled => {
            Err("node:fs is disabled by Chidori runtime policy (fs=disabled)".into())
        }
        FsPolicy::Host => {
            Err("node:fs host-disk mode (fs=host) is not implemented in this runtime".into())
        }
    }
}

/// Captured randomness: the result is keyed by the call-log sequence and
/// recorded as a `crypto.random` `CallRecord` (unless `crypto=host`), so a
/// resumed run draws the identical bytes byte-for-byte on replay.
fn execute_captured_random(
    ctx: &RuntimeContext,
    policy: &RuntimePolicy,
    n: usize,
) -> std::result::Result<Vec<u8>, String> {
    use crate::runtime::capability::Capability;
    let seq = ctx.next_seq();
    match ctx.try_replay_checked(seq, "crypto.random") {
        Ok(Some(record)) => {
            ctx.note_capability(Capability::CryptoRandom, seq);
            let b64 = record
                .result
                .get("bytes")
                .and_then(|v| v.as_str())
                .ok_or("crypto replay record is missing bytes")?;
            return b64_decode(b64);
        }
        Ok(None) => {}
        Err(message) => return Err(message),
    }
    let bytes = match policy.crypto {
        CryptoPolicy::Seeded => {
            crate::runtime::crypto::seeded_bytes(&policy.deterministic_seed, seq, n)
        }
        CryptoPolicy::Captured | CryptoPolicy::Host => crate::runtime::crypto::random_bytes(n),
        CryptoPolicy::Disabled => return Err("node:crypto is disabled".into()),
    };
    if policy.crypto != CryptoPolicy::Host {
        ctx.record_call(crate::runtime::call_log::CallRecord {
            seq,
            parent_seq: None,
            function: "crypto.random".to_string(),
            args: serde_json::json!({ "n": n }),
            result: serde_json::json!({ "bytes": b64_encode(&bytes) }),
            duration_ms: 0,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        });
    }
    ctx.note_capability(Capability::CryptoRandom, seq);
    Ok(bytes)
}

/// Build the `__chidori_*` sync-native dispatcher. Crypto hashing/HMAC are pure
/// and inline; randomness is captured through the call log; the VFS ops operate
/// on the snapshot-resident `RuntimeContext` filesystem — so a
/// `node:fs`/`node:crypto` agent records and replays deterministically.
fn build_sync_native_dispatch(
    ctx: RuntimeContext,
    policy: RuntimePolicy,
) -> Rc<dyn Fn(&str, &Value) -> std::result::Result<Value, String>> {
    use crate::runtime::capability::Capability;
    Rc::new(
        move |name: &str, args: &Value| -> std::result::Result<Value, String> {
            // Inside a live `chidori.step` callback (pure-compute contract,
            // docs/value-checkpoints.md), refuse the captured-effect natives
            // whose work would be skipped on replay: recorded randomness, VFS
            // mutation, and timer/microtask scheduling (the polyfills note a
            // capability at schedule time, so blocking that blocks them).
            // Hashing and VFS reads stay allowed — they record nothing and
            // mutate nothing, and the step's memoized result keeps replay
            // deterministic regardless.
            if matches!(
                name,
                "__chidori_crypto_random"
                    | "__chidori_fs_write"
                    | "__chidori_fs_append"
                    | "__chidori_fs_mkdir"
                    | "__chidori_fs_rm"
                    | "__chidori_fs_rename"
                    | "__chidori_note_capability"
            ) {
                if let Some(step) = ctx.active_step_name() {
                    return Err(format!(
                        "captured effects (randomness, fs writes, timers) are not allowed \
                         inside chidori.step(\"{step}\"): step callbacks must be pure, \
                         synchronous computation"
                    ));
                }
            }
            let str_arg = |i: usize| -> std::result::Result<String, String> {
                args.get(i)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| format!("{name}: argument {i} must be a string"))
            };
            let bool_arg = |i: usize| args.get(i).and_then(|v| v.as_bool()).unwrap_or(false);
            match name {
                "__chidori_crypto_hash" => {
                    crypto_policy_guard(&policy)?;
                    let alg = str_arg(0)?;
                    let data = b64_decode(&str_arg(1)?)?;
                    let digest =
                        crate::runtime::crypto::hash(&alg, &data).map_err(|e| e.to_string())?;
                    ctx.note_capability(Capability::CryptoHash, ctx.current_seq());
                    Ok(Value::String(b64_encode(&digest)))
                }
                "__chidori_crypto_hmac" => {
                    crypto_policy_guard(&policy)?;
                    let alg = str_arg(0)?;
                    let key = b64_decode(&str_arg(1)?)?;
                    let data = b64_decode(&str_arg(2)?)?;
                    let digest = crate::runtime::crypto::hmac(&alg, &key, &data)
                        .map_err(|e| e.to_string())?;
                    ctx.note_capability(Capability::CryptoHash, ctx.current_seq());
                    Ok(Value::String(b64_encode(&digest)))
                }
                "__chidori_crypto_random" => {
                    crypto_policy_guard(&policy)?;
                    let n = args
                        .get(0)
                        .and_then(|v| v.as_u64())
                        .ok_or("crypto random length must be a non-negative integer")?
                        as usize;
                    if n > 1_048_576 {
                        return Err(format!("crypto random length {n} exceeds the 1MiB cap"));
                    }
                    let bytes = execute_captured_random(&ctx, &policy, n)?;
                    Ok(Value::String(b64_encode(&bytes)))
                }
                "__chidori_fs_read" => {
                    fs_policy_guard(&policy)?;
                    let bytes = ctx.vfs_read(&str_arg(0)?)?;
                    Ok(Value::String(b64_encode(&bytes)))
                }
                "__chidori_fs_write" => {
                    fs_policy_guard(&policy)?;
                    let bytes = b64_decode(&str_arg(1)?)?;
                    ctx.vfs_write(&str_arg(0)?, bytes)?;
                    Ok(Value::Null)
                }
                "__chidori_fs_append" => {
                    fs_policy_guard(&policy)?;
                    let bytes = b64_decode(&str_arg(1)?)?;
                    ctx.vfs_append(&str_arg(0)?, &bytes)?;
                    Ok(Value::Null)
                }
                "__chidori_fs_exists" => {
                    fs_policy_guard(&policy)?;
                    Ok(Value::Bool(ctx.vfs_exists(&str_arg(0)?)))
                }
                "__chidori_fs_readdir" => {
                    fs_policy_guard(&policy)?;
                    let entries = ctx.vfs_readdir(&str_arg(0)?)?;
                    Ok(Value::Array(
                        entries.into_iter().map(Value::String).collect(),
                    ))
                }
                "__chidori_fs_mkdir" => {
                    fs_policy_guard(&policy)?;
                    ctx.vfs_mkdir(&str_arg(0)?, bool_arg(1))?;
                    Ok(Value::Null)
                }
                "__chidori_fs_rm" => {
                    fs_policy_guard(&policy)?;
                    ctx.vfs_remove(&str_arg(0)?, bool_arg(1), bool_arg(2))?;
                    Ok(Value::Null)
                }
                "__chidori_fs_rename" => {
                    fs_policy_guard(&policy)?;
                    ctx.vfs_rename(&str_arg(0)?, &str_arg(1)?)?;
                    Ok(Value::Null)
                }
                "__chidori_fs_stat" => {
                    fs_policy_guard(&policy)?;
                    ctx.vfs_stat(&str_arg(0)?)
                }
                "__chidori_note_capability" => {
                    let cap = match str_arg(0)?.as_str() {
                        "timer" => Capability::Timer,
                        "microtask" => Capability::Microtask,
                        _ => return Ok(Value::Null),
                    };
                    ctx.note_capability(cap, ctx.current_seq());
                    Ok(Value::Null)
                }
                _ => Err(format!("unknown captured-effect native `{name}`")),
            }
        },
    )
}

/// The determinism prelude installed on the rust engine before an agent runs:
/// the logical clock, `process.env`, UTF-8/base64 text primitives, the Web
/// Crypto subset, and the virtual timer queue. Date and `Math.random`
/// determinism are already native to `chidori-js`, so this installs no
/// Date/random shims.
fn rust_engine_prelude(policy: &RuntimePolicy) -> String {
    use crate::runtime::typescript::helpers::{
        chidori_agent_env_json, TEXT_ENCODING_POLYFILL, TIMER_DISABLED_POLYFILL,
        TIMER_VIRTUAL_POLYFILL, WEB_CRYPTO_POLYFILL,
    };
    let mut out = String::new();
    out.push_str(
        "if (typeof globalThis.__chidori_now !== \"number\") globalThis.__chidori_now = 0;\n",
    );
    let env_json = chidori_agent_env_json();
    out.push_str(&format!(
        "globalThis.process = Object.freeze({{ env: Object.freeze({env_json}) }});\n"
    ));
    out.push_str(TEXT_ENCODING_POLYFILL);
    out.push_str(WEB_CRYPTO_POLYFILL);
    match policy.timers {
        TimerPolicy::Disabled => out.push_str(TIMER_DISABLED_POLYFILL),
        TimerPolicy::Virtual | TimerPolicy::Host => out.push_str(TIMER_VIRTUAL_POLYFILL),
    }
    out
}

impl SnapshotCapableJsEngine for RustReplayEngine {
    fn snapshot(&mut self) -> Result<Vec<u8>> {
        let refs: Vec<&str> = self.effects.iter().map(|s| s.as_str()).collect();
        Ok(self.rt.to_blob(&refs))
    }

    fn restore(snapshot: &[u8]) -> Result<Self> {
        // Decode the self-describing blob to recover the effect names for
        // re-snapshotting, then rebuild the runtime (replays to the frontier).
        let blob: chidori_js::replay::DurableBlob =
            serde_json::from_slice(snapshot).map_err(|e| anyhow::anyhow!(e))?;
        let effects = blob.effects.clone();
        let rt = ReplayRuntime::from_blob(snapshot).map_err(|e| anyhow::anyhow!(e))?;
        Ok(RustReplayEngine { rt, effects })
    }

    fn resolve_host_promise(&mut self, id: HostOperationId, value: Value) -> Result<()> {
        self.rt
            .resolve_op(id.0, Ok(value))
            .map_err(|e| anyhow::anyhow!(e))
    }

    fn reject_host_promise(&mut self, id: HostOperationId, error: String) -> Result<()> {
        self.rt
            .resolve_op(id.0, Err(error))
            .map_err(|e| anyhow::anyhow!(e))
    }

    fn run_jobs_until_blocked(&mut self) -> Result<JsRunState> {
        match self
            .rt
            .run_until_blocked()
            .map_err(|e| anyhow::anyhow!(e))?
        {
            chidori_js::RunOutcome::Completed => Ok(JsRunState::Completed),
            chidori_js::RunOutcome::BlockedOnHost(id) => {
                Ok(JsRunState::BlockedOnHostOperation(HostOperationId(id)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use crate::mcp::McpManager;
    use crate::policy::{PolicyCache, PolicyConfig};
    use crate::providers::ProviderRegistry;
    use crate::runtime::context::RuntimeContext;
    use crate::runtime::snapshot::RuntimePolicy;
    use crate::runtime::template::TemplateEngine;
    use crate::tools::{ToolBackend, ToolRegistry};

    /// A fully-wired runtime backend over `ctx`/`tools` with default providers,
    /// template engine, tokio runtime, policy, and MCP — enough to exercise the
    /// full effect dispatch in tests.
    fn test_backend(ctx: RuntimeContext, tools: Arc<ToolRegistry>) -> HostBindingBackend {
        HostBindingBackend::for_runtime(
            ctx,
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(".")),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            PolicyConfig::from_env(),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("rust-engine-test"),
            tools,
            Arc::new(McpManager::new()),
        )
    }

    #[test]
    fn run_agent_executes_ts_through_rust_engine_and_records_host_calls() {
        // A single-file TS agent that does JS work (a nested function call) and a
        // chidori.log host effect, then returns a value derived from its input.
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-rust-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent(input: { value: number }) {
                function double(x: number) { return x * 2; }
                chidori.log("hello from the rust engine");
                return { value: double(input.value) };
            }
        "#;
        std::fs::write(&path, src).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx.clone(), tools);
        let output = run_agent(&path, src, &serde_json::json!({ "value": 21 }), &backend).unwrap();
        assert_eq!(output, serde_json::json!({ "value": 42 }));

        // The host effect flowed through host_core → the RuntimeContext call log,
        // so durability + host-call spans work.
        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "log");
        assert_eq!(
            records[0].args,
            serde_json::json!({ "message": "hello from the rust engine" })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_agent_supports_run_entrypoint_and_import_convention() {
        // The current convention: `import { chidori, run } from "chidori"` (the
        // import is stripped, both resolve to globals) and `run(handler)` as the
        // entrypoint — no second `chidori` param, no magic `agent` export.
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-rust-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            import { chidori, run } from "chidori";
            run(async (input: { value: number }) => {
                await chidori.log("via run() entrypoint");
                return { value: input.value + 1 };
            });
        "#;
        std::fs::write(&path, src).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx.clone(), tools);
        let output = run_agent(&path, src, &serde_json::json!({ "value": 41 }), &backend).unwrap();
        assert_eq!(output, serde_json::json!({ "value": 42 }));

        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "log");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_agent_nests_tool_internal_calls_under_the_tool_on_rust_engine() {
        // A TypeScript tool whose `run` makes its own chidori.log calls: those
        // must be recorded as CHILDREN of the tool call (parent_seq = tool seq),
        // so the trace nests correctly.
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-rust-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("tools")).unwrap();

        let tool_path = dir.join("tools").join("echo2.ts");
        std::fs::write(
            &tool_path,
            r#"
            export const tool = { name: "echo2", description: "doubles and logs", parameters: {} };
            export async function run(args: { value: number }) {
                chidori.log("tool: doubling " + args.value);
                return { value: args.value * 2 };
            }
            "#,
        )
        .unwrap();

        let agent_path = dir.join("agent.ts");
        let agent_src = r#"
            export async function agent(input: { value: number }) {
                chidori.log("agent: before tool");
                const r = await chidori.tool("echo2", { value: input.value });
                return r;
            }
        "#;
        std::fs::write(&agent_path, agent_src).unwrap();

        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo2".to_string(),
            description: "doubles and logs".to_string(),
            params: Vec::new(),
            source_path: tool_path,
            source_fingerprint: None,
            backend: ToolBackend::TypeScript,
        });
        let tools = Arc::new(registry);

        let backend = test_backend(ctx.clone(), tools);
        let output = run_agent(
            &agent_path,
            agent_src,
            &serde_json::json!({ "value": 5 }),
            &backend,
        )
        .unwrap();
        assert_eq!(output, serde_json::json!({ "value": 10 }));

        let records = ctx.call_log().into_records();
        // agent log (top-level), tool's internal log (nested), tool call.
        let tool = records.iter().find(|r| r.function == "tool").unwrap();
        let tool_log = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "tool: doubling 5")
            .unwrap();
        let agent_log = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "agent: before tool")
            .unwrap();
        // The tool's log nests under the tool; the agent's log is top-level.
        assert_eq!(tool_log.parent_seq, Some(tool.seq));
        assert_eq!(agent_log.parent_seq, None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_agent_wires_template_checkpoint_and_memory_effects() {
        // Effects beyond log/input/tool now route through the shared host backend
        // on the rust engine: minijinja templates, durable checkpoints, and the
        // memory store all flow through the same `host_core` machinery + call log.
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-rust-fx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let ns = format!("rust-fx-{}", uuid::Uuid::new_v4());
        // `__NS__` is substituted (not format!) so the `{{ name }}` minijinja
        // braces and TS object braces stay literal.
        let src = r#"
            export async function agent(input: { name: string }) {
                await chidori.checkpoint("start", { n: 1 });
                const greeting = await chidori.template("Hello {{ name }}", { name: input.name });
                await chidori.memory("set", "greeting", greeting, { namespace: "__NS__" });
                const back = await chidori.memory("get", "greeting", null, { namespace: "__NS__" });
                return { greeting, back };
            }
        "#
        .replace("__NS__", &ns);
        std::fs::write(&path, &src).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx.clone(), tools);
        let output = run_agent(
            &path,
            &src,
            &serde_json::json!({ "name": "world" }),
            &backend,
        )
        .unwrap();
        assert_eq!(
            output,
            serde_json::json!({ "greeting": "Hello world", "back": "Hello world" })
        );

        let records = ctx.call_log().into_records();
        let fns: Vec<&str> = records.iter().map(|r| r.function.as_str()).collect();
        assert!(fns.contains(&"checkpoint"), "missing checkpoint: {fns:?}");
        assert!(fns.contains(&"template"), "missing template: {fns:?}");
        assert_eq!(
            fns.iter().filter(|f| **f == "memory").count(),
            2,
            "expected two memory calls: {fns:?}"
        );

        let _ = std::fs::remove_file(format!(".chidori/memory/{ns}.json"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_agent_exposes_chidori_js_sdk_helpers() {
        // The JS-level SDK sugar (tryCall/retry/parallel + memory.set/get/delete/
        // clear) is loaded from CHIDORI_JS_HELPERS_SCRIPT and installed after
        // the native host object. The agent below never defines
        // these itself, so it only passes if the engine layered them on — a
        // regression guard for the meta-agent's `chidori.retry`/`chidori.parallel`
        // calls, which otherwise hit "undefined is not a function".
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-rust-sdk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                let attempts = 0;
                const value = await chidori.retry(async () => {
                    attempts += 1;
                    if (attempts < 2) throw new Error("flaky");
                    return 42;
                }, { attempts: 3 });
                const par = await chidori.parallel([
                    async () => "a",
                    async () => "b",
                ], { concurrency: 2 });
                const caught = await chidori.tryCall(async () => { throw new Error("boom"); });
                return {
                    value,
                    attempts,
                    par,
                    caughtOk: caught.ok,
                    memorySet: typeof chidori.memory.set,
                };
            }
        "#;
        std::fs::write(&path, src).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx.clone(), tools);
        let output = run_agent(&path, src, &serde_json::json!({}), &backend).unwrap();
        assert_eq!(
            output,
            serde_json::json!({
                "value": 42,
                "attempts": 2,
                "par": ["a", "b"],
                "caughtOk": false,
                "memorySet": "function",
            })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A provider that answers each call with the next canned response and
    /// captures every request, so context tests can assert both the answers
    /// and the cache layout on the wire.
    struct SequenceProvider {
        responses: Vec<String>,
        calls: std::sync::atomic::AtomicUsize,
        requests: Arc<StdMutex<Vec<crate::providers::LlmRequest>>>,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for SequenceProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(
            &self,
            request: &crate::providers::LlmRequest,
        ) -> anyhow::Result<crate::providers::LlmResponse> {
            self.requests.lock().unwrap().push(request.clone());
            let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let content = self
                .responses
                .get(index)
                .cloned()
                .unwrap_or_else(|| "out of responses".to_string());
            Ok(crate::providers::LlmResponse {
                content: content.clone(),
                blocks: vec![crate::providers::ContentBlock::Text { text: content }],
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: if index == 0 { 100 } else { 0 },
                cache_read_tokens: if index == 0 { 0 } else { 100 },
                ..crate::providers::LlmResponse::default()
            })
        }
    }

    /// A provider that returns one fixed response and counts how many live
    /// calls it received — used to prove replay short-circuits prior turns.
    struct CountingProvider {
        response: String,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for CountingProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(
            &self,
            _request: &crate::providers::LlmRequest,
        ) -> anyhow::Result<crate::providers::LlmResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::providers::LlmResponse {
                content: self.response.clone(),
                blocks: vec![crate::providers::ContentBlock::Text {
                    text: self.response.clone(),
                }],
                ..crate::providers::LlmResponse::default()
            })
        }
    }

    /// Serializes the prompt-issuing context tests against the local
    /// prompt-cache test: `CHIDORI_PROMPT_CACHE_DIR` is process-global, and
    /// the two CONTEXT_AGENT_SRC tests send byte-identical requests — run
    /// concurrently with the cache enabled, one could be served from entries
    /// the other stored and break its provider call-count assertions.
    static PROMPT_ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn context_test_backend(
        ctx: RuntimeContext,
        providers: crate::providers::ProviderRegistry,
    ) -> HostBindingBackend {
        HostBindingBackend::for_runtime(
            ctx,
            Arc::new(providers),
            Arc::new(TemplateEngine::new(".")),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            PolicyConfig::from_env(),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("rust-engine-test"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        )
    }

    const CONTEXT_AGENT_SRC: &str = r#"
        export async function agent(input: { questions: string[] }) {
            const base = chidori.context()
                .system("You are a policy analyst.")
                .doc("corpus", "Section 1: chidori agents are durable.")
                .cacheBreakpoint("5m");
            const baseDigest = base.digest();
            const forkA = base.user("a");
            const forkB = base.user("b");
            let ctx = base;
            const answers: string[] = [];
            for (const q of input.questions) {
                ctx = ctx.user(q);
                const r = await ctx.prompt({ model: "test-model" });
                ctx = r.context;
                answers.push(r.text);
            }
            return {
                answers,
                digestLen: baseDigest.length,
                baseDigestStable: base.digest() === baseDigest,
                forkDigestsDiffer: forkA.digest() !== forkB.digest(),
                hasTokenEstimate: ctx.estimateTokens() > 0,
            };
        }
    "#;

    #[test]
    fn context_builder_composes_multi_turn_prompts_with_cache_layout() {
        let _env = PROMPT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-rust-context-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, CONTEXT_AGENT_SRC).unwrap();

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut providers = crate::providers::ProviderRegistry::new();
        providers.register(Box::new(SequenceProvider {
            responses: vec!["answer one".to_string(), "answer two".to_string()],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        }));
        let backend = context_test_backend(ctx.clone(), providers);
        let output = run_agent(
            &path,
            CONTEXT_AGENT_SRC,
            &serde_json::json!({ "questions": ["q1", "q2"] }),
            &backend,
        )
        .unwrap();

        assert_eq!(
            output["answers"],
            serde_json::json!(["answer one", "answer two"])
        );
        assert_eq!(output["digestLen"], serde_json::json!(64));
        assert_eq!(output["baseDigestStable"], serde_json::json!(true));
        assert_eq!(output["forkDigestsDiffer"], serde_json::json!(true));
        assert_eq!(output["hasTokenEstimate"], serde_json::json!(true));

        // The wire requests: turn 1 sends [doc, q1]; turn 2 extends the same
        // prefix with the assistant turn and q2. The explicit breakpoint marks
        // the doc; auto-marking covers system and the conversation head.
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages.len(), 2);
        assert_eq!(requests[1].messages.len(), 4);
        for request in requests.iter() {
            assert!(request
                .system
                .as_deref()
                .unwrap()
                .contains("policy analyst"));
            assert!(request.cache.system.is_some(), "system head must be marked");
            assert!(
                request.messages[0].cache_control.is_some(),
                "explicit doc breakpoint must survive"
            );
            assert!(
                request.messages.last().unwrap().cache_control.is_some(),
                "conversation head must be auto-marked"
            );
        }
        // Turn 2's prefix content is identical to turn 1's request — the
        // property provider caches key on. (The rolling head *marker* moves
        // turn to turn; markers are placement metadata, not content.)
        let content =
            |m: &crate::providers::Message| serde_json::to_string(&(&m.role, &m.content)).unwrap();
        for i in 0..2 {
            assert_eq!(
                content(&requests[1].messages[i]),
                content(&requests[0].messages[i])
            );
        }

        // Durable records: two prompt calls, each carrying the assembled
        // request digest and the observed cache token split.
        let records = ctx.call_log().into_records();
        let prompts: Vec<_> = records.iter().filter(|r| r.function == "prompt").collect();
        assert_eq!(prompts.len(), 2);
        for record in &prompts {
            assert_eq!(record.args["request_digest"].as_str().unwrap().len(), 64);
        }
        let usage_turn1 = prompts[0].token_usage.as_ref().unwrap();
        assert_eq!(usage_turn1.cache_creation_tokens, Some(100));
        assert_eq!(usage_turn1.cache_read_tokens, None);
        let usage_turn2 = prompts[1].token_usage.as_ref().unwrap();
        assert_eq!(usage_turn2.cache_read_tokens, Some(100));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn context_conversation_replays_without_provider_calls() {
        let _env = PROMPT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "chidori-rust-context-replay-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, CONTEXT_AGENT_SRC).unwrap();
        let input = serde_json::json!({ "questions": ["q1", "q2"] });

        // Record a live run.
        let live_ctx = RuntimeContext::new();
        let mut providers = crate::providers::ProviderRegistry::new();
        providers.register(Box::new(SequenceProvider {
            responses: vec!["answer one".to_string(), "answer two".to_string()],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::new(StdMutex::new(Vec::new())),
        }));
        let live_backend = context_test_backend(live_ctx.clone(), providers);
        let live_output = run_agent(&path, CONTEXT_AGENT_SRC, &input, &live_backend).unwrap();
        let records = live_ctx.call_log().into_records();

        // Replay against an EMPTY provider registry: any live LLM call would
        // fail, so identical output proves the conversation came from the log.
        let replay_ctx = RuntimeContext::with_replay(records);
        let replay_backend =
            context_test_backend(replay_ctx, crate::providers::ProviderRegistry::new());
        let replay_output = run_agent(&path, CONTEXT_AGENT_SRC, &input, &replay_backend).unwrap();
        assert_eq!(live_output, replay_output);

        let _ = std::fs::remove_dir_all(dir);
    }

    const CONVERSATION_AGENT_SRC: &str = r#"
        export async function agent(input: { messages: string[] }) {
            const chat = chidori.conversation({
                system: "You are a terse test assistant.",
                model: "test-model",
            });
            const replies: string[] = [];
            for (const message of input.messages) {
                replies.push(await chat.say(message));
            }
            return {
                replies,
                length: chat.length,
                history: chat.history(),
                turnCount: chat.history().length,
            };
        }
    "#;

    #[test]
    fn conversation_helper_threads_turns_and_replays() {
        let _env = PROMPT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "chidori-rust-conversation-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, CONVERSATION_AGENT_SRC).unwrap();
        let input = serde_json::json!({ "messages": ["hi", "again"] });

        // Record a live run: each say() is one durable prompt host call, and the
        // assistant turn threads back into the context for the next message.
        let live_ctx = RuntimeContext::new();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut providers = crate::providers::ProviderRegistry::new();
        providers.register(Box::new(SequenceProvider {
            responses: vec!["reply one".to_string(), "reply two".to_string()],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        }));
        let live_backend = context_test_backend(live_ctx.clone(), providers);
        let live_output = run_agent(&path, CONVERSATION_AGENT_SRC, &input, &live_backend).unwrap();

        assert_eq!(
            live_output["replies"],
            serde_json::json!(["reply one", "reply two"])
        );
        assert_eq!(live_output["length"], serde_json::json!(2));
        assert_eq!(live_output["turnCount"], serde_json::json!(4));

        // Turn 2 extends turn 1's prefix: [user hi] then [user hi, assistant,
        // user again]. The shared head is what the provider cache keys on.
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].messages.len(), 1);
            assert_eq!(requests[1].messages.len(), 3);
            for request in requests.iter() {
                assert!(request
                    .system
                    .as_deref()
                    .unwrap()
                    .contains("terse test assistant"));
                assert!(request.cache.system.is_some(), "system head must be marked");
            }
        }

        // Replay against an EMPTY provider registry: any live LLM call would
        // fail, so identical output proves the dialogue came from the call log.
        let records = live_ctx.call_log().into_records();
        let replay_ctx = RuntimeContext::with_replay(records);
        let replay_backend =
            context_test_backend(replay_ctx, crate::providers::ProviderRegistry::new());
        let replay_output =
            run_agent(&path, CONVERSATION_AGENT_SRC, &input, &replay_backend).unwrap();
        assert_eq!(live_output, replay_output);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn chat_turn_loop_replays_prior_turns_and_only_calls_provider_for_the_new_message() {
        // Mirrors `chidori chat`: each turn re-runs the conversational agent
        // with the prior call log replayed and one more message appended, so
        // only the newest message reaches the provider.
        let _env = PROMPT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("chidori-rust-chat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, CONVERSATION_AGENT_SRC).unwrap();

        // Turn 1: messages = [hi]. One live provider call.
        let ctx1 = RuntimeContext::new();
        let mut providers1 = crate::providers::ProviderRegistry::new();
        providers1.register(Box::new(SequenceProvider {
            responses: vec!["reply one".to_string()],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::new(StdMutex::new(Vec::new())),
        }));
        let backend1 = context_test_backend(ctx1.clone(), providers1);
        let out1 = run_agent(
            &path,
            CONVERSATION_AGENT_SRC,
            &serde_json::json!({ "messages": ["hi"] }),
            &backend1,
        )
        .unwrap();
        assert_eq!(out1["replies"], serde_json::json!(["reply one"]));
        let call_log = ctx1.call_log().into_records();

        // Turn 2: messages = [hi, again], replaying turn 1's log. The first
        // say() replays "reply one" for free; only the second say() is live, so
        // the provider — which has just ONE response — is called exactly once.
        let calls2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx2 = RuntimeContext::with_replay(call_log);
        let mut providers2 = crate::providers::ProviderRegistry::new();
        providers2.register(Box::new(CountingProvider {
            response: "reply two".to_string(),
            calls: Arc::clone(&calls2),
        }));
        let backend2 = context_test_backend(ctx2, providers2);
        let out2 = run_agent(
            &path,
            CONVERSATION_AGENT_SRC,
            &serde_json::json!({ "messages": ["hi", "again"] }),
            &backend2,
        )
        .unwrap();
        assert_eq!(
            out2["replies"],
            serde_json::json!(["reply one", "reply two"])
        );
        assert_eq!(
            calls2.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the new message should reach the provider"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    const COMPACT_AGENT_SRC: &str = r#"
        export async function agent(input: { questions: string[] }) {
            let ctx = chidori.context()
                .system("You are a terse assistant for the compaction test.");
            for (const q of input.questions) {
                ctx = ctx.user(q);
                const r = await ctx.prompt({ model: "test-model" });
                ctx = r.context;
            }
            const beforeTokens = ctx.estimateTokens();
            const underBudget = await ctx.compact({ budgetTokens: 1000000 });
            const compacted = await ctx.compact({ keepTurns: 2 });
            const next = compacted.user("compact-final-question?");
            const r = await next.prompt({ model: "test-model" });
            return {
                finalText: r.text,
                noopUnderBudget: underBudget === ctx,
                compactedIsNew: compacted !== ctx,
                shrank: compacted.estimateTokens() < beforeTokens,
            };
        }
    "#;

    #[test]
    fn context_compact_summarizes_old_turns_into_recorded_segment() {
        let _env = PROMPT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-rust-compact-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, COMPACT_AGENT_SRC).unwrap();

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut providers = crate::providers::ProviderRegistry::new();
        providers.register(Box::new(SequenceProvider {
            responses: vec![
                "a long first answer about compaction alpha".to_string(),
                "a long second answer about compaction beta".to_string(),
                "a long third answer about compaction gamma".to_string(),
                "brief summary".to_string(),
                "final answer".to_string(),
            ],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        }));
        let backend = context_test_backend(ctx.clone(), providers);
        let input = serde_json::json!({
            "questions": ["compact-q-alpha?", "compact-q-beta?", "compact-q-gamma?"]
        });
        let output = run_agent(&path, COMPACT_AGENT_SRC, &input, &backend).unwrap();

        assert_eq!(output["finalText"], serde_json::json!("final answer"));
        // Under budget and "nothing old enough" paths are pure no-ops that
        // return the SAME context value without a host call.
        assert_eq!(output["noopUnderBudget"], serde_json::json!(true));
        assert_eq!(output["compactedIsNew"], serde_json::json!(true));
        assert_eq!(output["shrank"], serde_json::json!(true));

        let requests = requests.lock().unwrap();
        // 3 conversation turns + 1 summarization call + 1 post-compact turn;
        // the under-budget compact made NO provider call.
        assert_eq!(requests.len(), 5);

        // The summarization request: transcript of the old turns as one user
        // message under the summarizer system instructions.
        let summarize = &requests[3];
        assert!(summarize
            .system
            .as_deref()
            .unwrap()
            .contains("compact conversation history"));
        assert_eq!(summarize.messages.len(), 1);
        let transcript = match &summarize.messages[0].content[0] {
            crate::providers::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text transcript, got {other:?}"),
        };
        assert!(transcript.contains("User: compact-q-alpha?"));
        assert!(transcript.contains("Assistant: a long first answer about compaction alpha"));
        assert!(transcript.contains("User: compact-q-beta?"));
        // The kept turns are NOT summarized.
        assert!(!transcript.contains("compact-q-gamma?"));

        // The post-compact request: summary segment + the two kept turns +
        // the new question, with a fresh cache breakpoint on the summary.
        let after = &requests[4];
        assert_eq!(after.messages.len(), 4);
        let summary_text = match &after.messages[0].content[0] {
            crate::providers::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected summary text, got {other:?}"),
        };
        assert!(summary_text.contains("<conversation-summary>"));
        assert!(summary_text.contains("brief summary"));
        assert!(
            after.messages[0].cache_control.is_some(),
            "summary segment must carry a fresh cache breakpoint"
        );
        let kept_q = match &after.messages[1].content[0] {
            crate::providers::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected kept user turn, got {other:?}"),
        };
        assert_eq!(kept_q, "compact-q-gamma?");

        // The summarization is a normal recorded prompt: 5 prompt records.
        let records = ctx.call_log().into_records();
        let prompts: Vec<_> = records.iter().filter(|r| r.function == "prompt").collect();
        assert_eq!(prompts.len(), 5);

        // Replay against an empty provider registry reproduces the whole run,
        // compaction included, from the call log alone.
        let replay_ctx = RuntimeContext::with_replay(records);
        let replay_backend =
            context_test_backend(replay_ctx, crate::providers::ProviderRegistry::new());
        let replay_output = run_agent(&path, COMPACT_AGENT_SRC, &input, &replay_backend).unwrap();
        assert_eq!(output, replay_output);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Removes the prompt-cache env flag even if the test panics, so a failure
    /// can't leave the process-global cache enabled for unrelated tests.
    struct PromptCacheEnvGuard;
    impl Drop for PromptCacheEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("CHIDORI_PROMPT_CACHE_DIR");
        }
    }

    #[test]
    fn local_prompt_cache_serves_identical_request_without_provider() {
        let _env = PROMPT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "chidori-rust-prompt-cache-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent(input: {}) {
                const text = await chidori.prompt(
                    "local-prompt-cache-unique-question?",
                    { model: "test-model" },
                );
                return { text };
            }
        "#;
        std::fs::write(&path, src).unwrap();
        std::env::set_var(
            "CHIDORI_PROMPT_CACHE_DIR",
            dir.join("prompt-cache").to_str().unwrap(),
        );
        let _cache_env = PromptCacheEnvGuard;

        // First run pays the provider and populates the cache.
        let first_ctx = RuntimeContext::new();
        let first_calls = Arc::new(StdMutex::new(Vec::new()));
        let mut providers = crate::providers::ProviderRegistry::new();
        providers.register(Box::new(SequenceProvider {
            responses: vec!["the locally cached answer".to_string()],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::clone(&first_calls),
        }));
        let first_backend = context_test_backend(first_ctx.clone(), providers);
        let first_output = run_agent(&path, src, &serde_json::json!({}), &first_backend).unwrap();
        assert_eq!(
            first_output,
            serde_json::json!({ "text": "the locally cached answer" })
        );
        assert_eq!(first_calls.lock().unwrap().len(), 1);

        // A FRESH run (empty call log, so no replay) issuing the identical
        // request is served from the local cache: zero provider calls, yet it
        // records an identical result as a normal CallRecord.
        let second_ctx = RuntimeContext::new();
        let second_calls = Arc::new(StdMutex::new(Vec::new()));
        let mut providers = crate::providers::ProviderRegistry::new();
        providers.register(Box::new(SequenceProvider {
            responses: vec!["WRONG: provider must not be consulted".to_string()],
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::clone(&second_calls),
        }));
        let second_backend = context_test_backend(second_ctx.clone(), providers);
        let second_output = run_agent(&path, src, &serde_json::json!({}), &second_backend).unwrap();
        assert_eq!(second_output, first_output);
        assert_eq!(second_calls.lock().unwrap().len(), 0);

        let first_records = first_ctx.call_log().into_records();
        let second_records = second_ctx.call_log().into_records();
        let first_prompt = first_records
            .iter()
            .find(|r| r.function == "prompt")
            .unwrap();
        let second_prompt = second_records
            .iter()
            .find(|r| r.function == "prompt")
            .unwrap();
        assert_eq!(second_prompt.result, first_prompt.result);
        assert_eq!(second_prompt.args, first_prompt.args);
        // The first run paid tokens; the cache-served run paid none.
        assert!(first_prompt.token_usage.is_some());
        assert!(second_prompt.token_usage.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_agent_resolves_relative_module_imports() {
        // A multi-file agent: the entry imports a helper from a sibling `.ts`
        // module. The rust engine resolves + transpiles + links the graph.
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-rust-mods-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();

        std::fs::write(
            dir.join("lib").join("math.ts"),
            r#"
            export function triple(x: number): number { return x * 3; }
            export const BONUS = 1;
            "#,
        )
        .unwrap();

        let agent_path = dir.join("agent.ts");
        let agent_src = r#"
            import { chidori } from "chidori";
            import { triple, BONUS } from "./lib/math";
            export async function agent(input: { value: number }) {
                chidori.log("computing");
                return { value: triple(input.value) + BONUS };
            }
        "#;
        std::fs::write(&agent_path, agent_src).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx.clone(), tools);
        let output = run_agent(
            &agent_path,
            agent_src,
            &serde_json::json!({ "value": 10 }),
            &backend,
        )
        .unwrap();
        assert_eq!(output, serde_json::json!({ "value": 31 }));

        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "log");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_agent_opcode_budget_terminates_infinite_loop() {
        // The opcode budget wired into `run_module` must bound pure-JS compute so
        // a runaway loop terminates with an error instead of hanging the host. We
        // set a modest budget via env (well above any legitimate test agent's op
        // count, so concurrent tests are unaffected) and run an infinite loop.
        std::env::set_var("CHIDORI_JS_OP_BUDGET", "5000000");
        let ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-rust-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                let n = 0;
                while (true) { n = n + 1; }
                return { n };
            }
        "#;
        std::fs::write(&path, src).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx, tools);
        let result = run_agent(&path, src, &serde_json::json!({}), &backend);
        std::env::remove_var("CHIDORI_JS_OP_BUDGET");

        let err = result.expect_err("infinite loop should exhaust the opcode budget");
        let msg = err.to_string();
        assert!(
            msg.contains("budget") || msg.contains("RangeError"),
            "expected an opcode-budget error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rust_engine_round_trips_through_the_trait_seam() {
        // A durable program that blocks on a host effect, is snapshotted, then
        // restored and resumed via the SnapshotCapableJsEngine trait.
        let bundle = r#"
            async function main() {
                const a = await fetchValue('a');
                const b = await fetchValue('b');
                report(a + b);
            }
            main();
        "#;
        let mut eng = RustReplayEngine::start(bundle, &["fetchValue", "report"]);

        // Run to the first host block.
        let state = eng.run_jobs_until_blocked().unwrap();
        let id = match state {
            JsRunState::BlockedOnHostOperation(id) => id,
            JsRunState::Completed => panic!("expected to block on fetchValue"),
        };
        let (name, _args) = eng.pending_op(id).unwrap();
        assert_eq!(name, "fetchValue");

        // Resolve it and snapshot mid-flight.
        eng.resolve_host_promise(id, serde_json::json!(10)).unwrap();
        let blob = eng.snapshot().unwrap();

        // Restore in a fresh engine (re-evaluates bundle, replays the journal).
        let mut eng2 = RustReplayEngine::restore(&blob).unwrap();
        let state2 = eng2.run_jobs_until_blocked().unwrap();
        let id2 = match state2 {
            JsRunState::BlockedOnHostOperation(id) => id,
            JsRunState::Completed => panic!("expected to block on the second fetchValue"),
        };
        eng2.resolve_host_promise(id2, serde_json::json!(32))
            .unwrap();
        // The report effect now blocks; resolve it and finish.
        if let JsRunState::BlockedOnHostOperation(id3) = eng2.run_jobs_until_blocked().unwrap() {
            let (n, args) = eng2.pending_op(id3).unwrap();
            assert_eq!(n, "report");
            assert_eq!(args[0], serde_json::json!(42));
            eng2.resolve_host_promise(id3, serde_json::json!(null))
                .unwrap();
        } else {
            panic!("expected report effect");
        }
        assert!(matches!(
            eng2.run_jobs_until_blocked().unwrap(),
            JsRunState::Completed
        ));
    }

    /// Run a pure-compute agent (no host effects) through the full engine path
    /// (transpile + module graph + `node:` shims) and return its completed value.
    fn run_compute_agent(name: &str, source: &str) -> serde_json::Value {
        let ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-rust-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, source).unwrap();
        let tools = Arc::new(ToolRegistry::new());
        let backend = test_backend(ctx, tools);
        let output = run_agent(&path, source, &serde_json::json!({}), &backend)
            .unwrap_or_else(|e| panic!("{name} agent errored: {e:?}"));
        let _ = std::fs::remove_dir_all(dir);
        output
    }

    #[test]
    fn run_agent_node_path_posix_surface() {
        // Covers the default + named import forms and the `path.posix`
        // self-alias. (Only the `node:`-prefixed specifier is accepted; bare
        // builtin specifiers are not — the resolver treats a bare `path` as a
        // package lookup, matching the other shims.)
        let out = run_compute_agent(
            "node-path",
            r#"
            import path from "node:path";
            import { join, basename } from "node:path";
            export async function agent() {
                return {
                    join: path.join("/a", "b", "..", "c.txt"),
                    resolve: path.resolve("/a/b", "../c"),
                    dirname: path.dirname("/a/b/c.txt"),
                    basename: basename("/a/b/c.txt", ".txt"),
                    extname: path.extname("index.test.ts"),
                    normalize: path.normalize("/a/./b/../c/"),
                    isAbsolute: path.isAbsolute("/x"),
                    notAbsolute: path.isAbsolute("x/y"),
                    relative: path.relative("/a/b/c", "/a/b/d/e"),
                    parsed: path.parse("/a/b/c.txt"),
                    format: path.format({ dir: "/a/b", name: "c", ext: ".txt" }),
                    sep: path.sep,
                    delimiter: path.delimiter,
                    posixIsSelf: path.posix === path.posix.posix,
                    namedJoin: join("a", "b"),
                };
            }
            "#,
        );
        assert_eq!(out["join"], serde_json::json!("/a/c.txt"));
        assert_eq!(out["resolve"], serde_json::json!("/a/c"));
        assert_eq!(out["dirname"], serde_json::json!("/a/b"));
        assert_eq!(out["basename"], serde_json::json!("c"));
        assert_eq!(out["extname"], serde_json::json!(".ts"));
        assert_eq!(out["normalize"], serde_json::json!("/a/c/"));
        assert_eq!(out["isAbsolute"], serde_json::json!(true));
        assert_eq!(out["notAbsolute"], serde_json::json!(false));
        assert_eq!(out["relative"], serde_json::json!("../d/e"));
        assert_eq!(out["parsed"]["base"], serde_json::json!("c.txt"));
        assert_eq!(out["parsed"]["ext"], serde_json::json!(".txt"));
        assert_eq!(out["parsed"]["name"], serde_json::json!("c"));
        assert_eq!(out["parsed"]["dir"], serde_json::json!("/a/b"));
        assert_eq!(out["format"], serde_json::json!("/a/b/c.txt"));
        assert_eq!(out["sep"], serde_json::json!("/"));
        assert_eq!(out["delimiter"], serde_json::json!(":"));
        assert_eq!(out["posixIsSelf"], serde_json::json!(true));
        assert_eq!(out["namedJoin"], serde_json::json!("a/b"));
    }

    #[test]
    fn run_agent_node_path_posix_subpath_reexports() {
        // The `node:path/posix` subpath shim must re-export node:path's surface.
        let out = run_compute_agent(
            "node-path-posix",
            r#"
            import posix, { join } from "node:path/posix";
            export async function agent() {
                return { default: posix.join("/a", "b"), named: join("x", "y") };
            }
            "#,
        );
        assert_eq!(out["default"], serde_json::json!("/a/b"));
        assert_eq!(out["named"], serde_json::json!("x/y"));
    }

    #[test]
    fn run_agent_node_events_emitter_surface() {
        let out = run_compute_agent(
            "node-events",
            r#"
            import EventEmitter, { once } from "node:events";
            export async function agent() {
                const ee = new EventEmitter();
                const seen = [];
                const onData = (x) => seen.push("on:" + x);
                ee.on("data", onData);
                ee.once("data", (x) => seen.push("once:" + x));
                ee.emit("data", 1);
                ee.emit("data", 2);
                const countBefore = ee.listenerCount("data");
                ee.off("data", onData);
                const countAfter = ee.listenerCount("data");
                ee.on("other", () => {});
                const names = ee.eventNames();
                const p = once(ee, "ready");
                ee.emit("ready", "go");
                const readyArgs = await p;
                return { seen, countBefore, countAfter, names, readyArgs };
            }
            "#,
        );
        assert_eq!(out["seen"], serde_json::json!(["on:1", "once:1", "on:2"]));
        assert_eq!(out["countBefore"], serde_json::json!(1));
        assert_eq!(out["countAfter"], serde_json::json!(0));
        assert_eq!(out["names"], serde_json::json!(["other"]));
        assert_eq!(out["readyArgs"], serde_json::json!(["go"]));
    }

    #[test]
    fn run_agent_fetch_routes_through_captured_http() {
        use std::io::{Read, Write};

        // A one-shot local HTTP server so the request goes through the real
        // captured networking host op (`__chidori_http`) end to end.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            let body = r#"{"ok":true,"n":7}"#;
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });

        let source = format!(
            r#"
            export async function agent() {{
                const res = await fetch("http://{addr}/data", {{
                    method: "POST",
                    headers: {{ "x-test": "1" }},
                    body: JSON.stringify({{ hello: "world" }}),
                }});
                const json = await res.json();
                return {{
                    status: res.status,
                    ok: res.ok,
                    contentType: res.headers.get("content-type"),
                    n: json.n,
                    isResponse: res instanceof Response,
                    hasFetch: typeof fetch === "function",
                }};
            }}
            "#
        );
        let out = run_compute_agent("fetch-capture", &source);
        server.join().unwrap();
        assert_eq!(out["status"], serde_json::json!(201));
        assert_eq!(out["ok"], serde_json::json!(true));
        assert_eq!(out["contentType"], serde_json::json!("application/json"));
        assert_eq!(out["n"], serde_json::json!(7));
        assert_eq!(out["isResponse"], serde_json::json!(true));
        assert_eq!(out["hasFetch"], serde_json::json!(true));
    }

    #[test]
    fn run_agent_node_http_and_fetch_share_captured_http_op() {
        // `node:http` and `fetch` must use the SAME capture point, so a library
        // reaching for either inherits identical policy + record/replay. Here we
        // only assert the surfaces coexist and route through `__chidori_http`
        // (no public `chidori.http`): the actual transport is covered above.
        let out = run_compute_agent(
            "fetch-no-public-http",
            r#"
            import http from "node:http";
            export async function agent(input, chidori) {
                return {
                    fetchInstalled: typeof fetch === "function",
                    headersInstalled: typeof Headers === "function",
                    nodeHttpRequest: typeof http.request === "function",
                    chidoriHttpRemoved: typeof (chidori && chidori.http),
                    captureNative: typeof globalThis.__chidori_http,
                };
            }
            "#,
        );
        assert_eq!(out["fetchInstalled"], serde_json::json!(true));
        assert_eq!(out["headersInstalled"], serde_json::json!(true));
        assert_eq!(out["nodeHttpRequest"], serde_json::json!(true));
        assert_eq!(out["chidoriHttpRemoved"], serde_json::json!("undefined"));
        assert_eq!(out["captureNative"], serde_json::json!("function"));
    }

    #[test]
    fn run_agent_node_url_whatwg_surface() {
        let out = run_compute_agent(
            "node-url",
            r#"
            import { URL, URLSearchParams } from "node:url";
            export async function agent() {
                const u = new URL("https://user:pw@example.com:8443/p/q?a=1&b=2#frag");
                u.searchParams.append("c", "3");
                const rel = new URL("../sibling?x=1", "https://example.com/a/b/c");
                const sp = new URLSearchParams("k=1&k=2&j=hello world");
                return {
                    protocol: u.protocol,
                    hostname: u.hostname,
                    port: u.port,
                    host: u.host,
                    pathname: u.pathname,
                    hash: u.hash,
                    origin: u.origin,
                    username: u.username,
                    getA: u.searchParams.get("a"),
                    search: u.search,
                    href: u.toString(),
                    relHref: rel.toString(),
                    spGetAll: sp.getAll("k"),
                    spHas: sp.has("j"),
                    spToString: sp.toString(),
                };
            }
            "#,
        );
        assert_eq!(out["protocol"], serde_json::json!("https:"));
        assert_eq!(out["hostname"], serde_json::json!("example.com"));
        assert_eq!(out["port"], serde_json::json!("8443"));
        assert_eq!(out["host"], serde_json::json!("example.com:8443"));
        assert_eq!(out["pathname"], serde_json::json!("/p/q"));
        assert_eq!(out["hash"], serde_json::json!("#frag"));
        assert_eq!(out["origin"], serde_json::json!("https://example.com:8443"));
        assert_eq!(out["username"], serde_json::json!("user"));
        assert_eq!(out["getA"], serde_json::json!("1"));
        assert_eq!(out["search"], serde_json::json!("?a=1&b=2&c=3"));
        assert_eq!(
            out["relHref"],
            serde_json::json!("https://example.com/a/sibling?x=1")
        );
        assert_eq!(out["spGetAll"], serde_json::json!(["1", "2"]));
        assert_eq!(out["spHas"], serde_json::json!(true));
        assert_eq!(
            out["spToString"],
            serde_json::json!("k=1&k=2&j=hello+world")
        );
    }

    #[test]
    fn run_agent_node_assert_surface() {
        let out = run_compute_agent(
            "node-assert",
            r#"
            import assert from "node:assert";
            import strict, { strictEqual as strictEqualNamed } from "node:assert/strict";
            export async function agent() {
                strictEqualNamed(2, 2);
                assert.ok(true);
                assert.equal(1, "1");
                assert.strictEqual(2, 2);
                assert.notStrictEqual(2, 3);
                assert.deepStrictEqual({ a: [1, 2], b: { c: 3 } }, { a: [1, 2], b: { c: 3 } });
                strict.strictEqual(strict, assert.strict);
                assert.throws(() => { throw new TypeError("boom"); }, TypeError);
                let threwOnEqual = false;
                try { assert.strictEqual(1, 2); } catch (e) { threwOnEqual = e.code === "ERR_ASSERTION"; }
                let rejected = false;
                try {
                    await assert.rejects(Promise.reject(new Error("nope")), /nope/);
                    rejected = true;
                } catch { rejected = false; }
                return { threwOnEqual, rejected, name: assert.AssertionError.name };
            }
            "#,
        );
        assert_eq!(out["threwOnEqual"], serde_json::json!(true));
        assert_eq!(out["rejected"], serde_json::json!(true));
        assert_eq!(out["name"], serde_json::json!("AssertionError"));
    }

    #[test]
    fn run_agent_node_os_returns_virtualized_constants() {
        // os values are fixed/virtualized (like process.platform) so runs and
        // record/replay agree byte-for-byte regardless of the host machine.
        let out = run_compute_agent(
            "node-os",
            r#"
            import os from "node:os";
            import { platform, EOL } from "node:os";
            export async function agent() {
                return {
                    platform: os.platform(),
                    namedPlatform: platform(),
                    arch: os.arch(),
                    eol: EOL,
                    tmpdir: os.tmpdir(),
                    totalmem: os.totalmem(),
                    cpus: os.cpus().length,
                };
            }
            "#,
        );
        assert_eq!(out["platform"], serde_json::json!("chidori"));
        assert_eq!(out["namedPlatform"], serde_json::json!("chidori"));
        assert_eq!(out["arch"], serde_json::json!("wasm32"));
        assert_eq!(out["eol"], serde_json::json!("\n"));
        assert_eq!(out["tmpdir"], serde_json::json!("/tmp"));
        assert_eq!(out["totalmem"], serde_json::json!(0));
        assert_eq!(out["cpus"], serde_json::json!(0));
    }
}
