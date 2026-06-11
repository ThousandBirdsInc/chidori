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

/// Reframe a chidori-js entrypoint error the way the QuickJS host does, so an
/// uncaught JS exception surfaces identically on both engines: as
/// `JavaScript exception: <message>` (see `snapshot_export_error` on the QuickJS
/// path). chidori-js stringifies a thrown `Error` as `"<Name>: <message>"`; we
/// strip the standard error-class prefix to recover the bare message and apply
/// the host framing. Pause sentinels pass through untouched — they are control
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
/// Re-enters [`run_module`] with the tool's `run(args)` entrypoint instead of
/// bouncing the nested call back into QuickJS via `TypeScriptVmRuntime`. The
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
    /// Live process-heap growth ceiling in bytes, enforced by the watchdog via the
    /// counting allocator. `None` disables. Env `CHIDORI_JS_MEM_CAP_MB` (default
    /// 4096; `0` disables). A coarse process-wide backstop, not a precise per-agent
    /// quota — see [`crate::mem_guard`].
    mem_cap: Option<usize>,
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
        ExecutionLimits {
            op_budget,
            mem_cap,
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
}

impl ExecutionGuard {
    fn install(vm: &mut chidori_js::Vm) -> Self {
        let limits = ExecutionLimits::from_env();
        if let Some(budget) = limits.op_budget {
            vm.op_budget = Some(budget);
        }
        let interrupt = Arc::new(AtomicBool::new(false));
        vm.interrupt = Some(interrupt.clone());

        let done = Arc::new(AtomicBool::new(false));
        // Only spend a thread when there is something time- or memory-based to
        // watch; the opcode budget is enforced inline by the VM and needs none.
        let watchdog = if limits.mem_cap.is_some() || limits.deadline.is_some() {
            let done_w = done.clone();
            let baseline = crate::mem_guard::current_allocated_bytes();
            let deadline_at = limits.deadline.map(|d| Instant::now() + d);
            let mem_cap = limits.mem_cap;
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
                if let Some(cap) = mem_cap {
                    let used = crate::mem_guard::current_allocated_bytes().saturating_sub(baseline);
                    if used > cap {
                        interrupt.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }))
        } else {
            None
        };
        ExecutionGuard { done, watchdog }
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
/// VM's `chidori.*` effects are forwarded to `backend.dispatch`, the same
/// durable host machinery the QuickJS bindings use.
fn run_module(
    path: &Path,
    source: &str,
    fallback_export: &str,
    input: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    // `Node` accepts relative `./foo` imports *and* allowlisted `node:` builtins
    // (the special-cased `chidori` SDK import is stripped by transpilation). This
    // matches the QuickJS path's durable default so `node:fs`/`crypto`/`timers`
    // reach the captured-effect natives installed below.
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
    // memory.set/get/delete/clear wrappers) that the QuickJS path also installs.
    // These are pure-JS helpers layered on top of the native host object, so they
    // must run *after* `install_chidori_effects` (the memory sugar wraps the
    // native `chidori.memory`, and the script's guarded workspace shim no-ops
    // because the rust engine already exposes a native `chidori.workspace`).
    // Without this the meta-agent's `chidori.retry(...)`/`chidori.parallel(...)`
    // calls hit `undefined is not a function`.
    engine
        .eval(crate::runtime::typescript::helpers::CHIDORI_JS_HELPERS_SCRIPT)
        .map_err(|e| anyhow::anyhow!("installing chidori JS SDK helpers: {e}"))?;
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

/// Captured randomness, replaying byte-for-byte with the QuickJS path: the
/// result is keyed by the shared call-log sequence and recorded as a
/// `crypto.random` `CallRecord` (unless `crypto=host`), so a resumed run draws
/// the identical bytes. Mirrors `execute_captured_random` in the QuickJS
/// snapshot host.
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
/// on the same snapshot-resident `RuntimeContext` filesystem the QuickJS path
/// uses — so a `node:fs`/`node:crypto` agent replays identically on either engine.
fn build_sync_native_dispatch(
    ctx: RuntimeContext,
    policy: RuntimePolicy,
) -> Rc<dyn Fn(&str, &Value) -> std::result::Result<Value, String>> {
    use crate::runtime::capability::Capability;
    Rc::new(
        move |name: &str, args: &Value| -> std::result::Result<Value, String> {
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
/// determinism are already native to `chidori-js`, so (unlike the QuickJS
/// prelude) this installs no Date/random shims. The polyfill sources are shared
/// verbatim with the QuickJS path.
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
        // exactly like the QuickJS path (so durability + host-call spans work).
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
        // exactly like the QuickJS path — so the trace nests on the rust engine.
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
        // clear) is shared with the QuickJS path via CHIDORI_JS_HELPERS_SCRIPT and
        // installed after the native host object. The agent below never defines
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
}
