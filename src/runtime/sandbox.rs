//! WebAssembly sandbox backing the `exec()` host function.
//!
//! Uses wasmer with the Cranelift compiler and the metering middleware to
//! execute untrusted WASM modules with a hard fuel limit (bounded instruction
//! count) and a hard memory-page cap. Input can be either a `.wasm` binary
//! blob or a WAT (WebAssembly Text) source string — wasmer's `Module::new`
//! accepts both.
//!
//! This is the low-level primitive. Higher-level sandboxing for LLM-generated
//! JavaScript / Python source code lives on top of it, by bundling a
//! language-interpreter-compiled-to-WASM binary (e.g. QuickJS-WASM) and
//! marshalling source code in, result JSON out. That is tracked as follow-up.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use wasmer::sys::{BaseTunables, Cranelift, NativeEngineExt};
use wasmer::vm::{VMConfig, VMMemory, VMTable};
use wasmer::{
    imports, wasmparser::Operator, CompilerConfig, Engine, Function, FunctionEnv,
    FunctionEnvMut, Instance, Memory, MemoryType, Module, Pages, Store, TableType, Target,
    Tunables, Value as WasmValue,
};
use wasmer_middlewares::metering::{get_remaining_points, set_remaining_points, MeteringPoints};
use wasmer_middlewares::Metering;

pub struct ExecRequest {
    /// Either raw WASM bytes (`.wasm`) or WAT source (`.wat` / inline text).
    /// Wasmer's `Module::new` auto-detects which one based on the leading bytes.
    pub wasm_source: Vec<u8>,
    /// Name of the exported function to call.
    pub function: String,
    /// Arguments to the function. Only i32/i64/f32/f64 are supported today.
    pub args: Vec<WasmArg>,
    /// Maximum instructions the sandboxed code may execute before being
    /// aborted with a fuel-exhaustion error.
    pub fuel: u64,
    /// Upper bound on linear memory in WebAssembly pages (64 KiB each).
    /// The default wasmer limit is very high; this caps it.
    pub memory_pages: u32,
    /// Optional callback invoked when the guest calls `host.log(ptr, len)`.
    /// The host reads `len` bytes from the guest's exported `memory` starting
    /// at `ptr` and passes the decoded string to this closure.
    pub log_callback: Option<LogFn>,
}

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum WasmArg {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
}

impl WasmArg {
    #[allow(dead_code)]
    fn to_wasm_value(&self) -> WasmValue {
        match self {
            WasmArg::I32(v) => WasmValue::I32(*v),
            WasmArg::I64(v) => WasmValue::I64(*v),
            WasmArg::F32(v) => WasmValue::F32(*v),
            WasmArg::F64(v) => WasmValue::F64(*v),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    /// Return values from the function, serialized as JSON for the call log.
    pub returns: Vec<JsonValue>,
    /// Remaining fuel after the call completed (None if exhausted).
    pub fuel_remaining: Option<u64>,
}

/// Parse args from a Starlark-friendly JSON array into typed WASM args.
/// Integers become i64, floats become f64 — the WASM function signature is
/// used later to coerce where needed.
pub fn parse_args(args_json: &JsonValue) -> Result<Vec<WasmArg>> {
    let arr = args_json
        .as_array()
        .ok_or_else(|| anyhow!("exec() args must be a list"))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let arg = match v {
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                        WasmArg::I32(i as i32)
                    } else {
                        WasmArg::I64(i)
                    }
                } else if let Some(f) = n.as_f64() {
                    WasmArg::F64(f)
                } else {
                    bail!("unsupported numeric arg: {}", v);
                }
            }
            _ => bail!("exec() only supports numeric args today, got {}", v),
        };
        out.push(arg);
    }
    Ok(out)
}

fn wasm_value_to_json(v: &WasmValue) -> JsonValue {
    match v {
        WasmValue::I32(i) => JsonValue::from(*i),
        WasmValue::I64(i) => JsonValue::from(*i),
        WasmValue::F32(f) => JsonValue::from(*f),
        WasmValue::F64(f) => JsonValue::from(*f),
        _ => JsonValue::Null,
    }
}

/// Bounded-memory Tunables that caps any memory declaration at `max_pages`.
struct CappedTunables {
    base: BaseTunables,
    max_pages: Pages,
}

impl CappedTunables {
    fn new(max_pages: u32) -> Self {
        Self {
            base: BaseTunables::for_target(&Target::default()),
            max_pages: Pages(max_pages),
        }
    }

    fn cap(&self, mut ty: MemoryType) -> MemoryType {
        let capped_max = Some(self.max_pages);
        ty.maximum = match ty.maximum {
            Some(m) if m < self.max_pages => Some(m),
            _ => capped_max,
        };
        if ty.minimum > self.max_pages {
            ty.minimum = self.max_pages;
        }
        ty
    }
}

impl Tunables for CappedTunables {
    fn memory_style(&self, memory: &MemoryType) -> wasmer::vm::MemoryStyle {
        self.base.memory_style(&self.cap(*memory))
    }

    fn table_style(&self, table: &TableType) -> wasmer::vm::TableStyle {
        self.base.table_style(table)
    }

    fn create_host_memory(
        &self,
        ty: &MemoryType,
        style: &wasmer::vm::MemoryStyle,
    ) -> std::result::Result<VMMemory, wasmer::vm::MemoryError> {
        self.base.create_host_memory(&self.cap(*ty), style)
    }

    unsafe fn create_vm_memory(
        &self,
        ty: &MemoryType,
        style: &wasmer::vm::MemoryStyle,
        vm_definition_location: std::ptr::NonNull<wasmer::vm::VMMemoryDefinition>,
    ) -> std::result::Result<VMMemory, wasmer::vm::MemoryError> {
        self.base
            .create_vm_memory(&self.cap(*ty), style, vm_definition_location)
    }

    fn create_host_table(
        &self,
        ty: &TableType,
        style: &wasmer::vm::TableStyle,
    ) -> std::result::Result<VMTable, String> {
        self.base.create_host_table(ty, style)
    }

    unsafe fn create_vm_table(
        &self,
        ty: &TableType,
        style: &wasmer::vm::TableStyle,
        vm_definition_location: std::ptr::NonNull<wasmer::vm::VMTableDefinition>,
    ) -> std::result::Result<VMTable, String> {
        self.base.create_vm_table(ty, style, vm_definition_location)
    }

    fn vmconfig(&self) -> &VMConfig {
        self.base.vmconfig()
    }
}

/// Flat cost function for the metering middleware — every operator costs 1.
/// Gives us an instruction-count budget via the `fuel` field on ExecRequest.
fn cost(_: &Operator) -> u64 {
    1
}

/// Cache of compiled WASM artifacts keyed by (source_hash, memory_pages).
///
/// The metering middleware bakes an initial fuel budget into the engine at
/// compile time, but we reset it per call via `set_remaining_points` so one
/// compiled module works across calls with different fuel budgets. Memory
/// caps, however, live on the engine's tunables — so we keep a separate
/// artifact per (source, memory_pages) pair to ensure the requested cap is
/// actually what gets enforced.
///
/// wasmer's `Engine` and `Module` are both `Send + Sync + Clone` (internally
/// Arc'd), so this cache can live in a global Mutex without copying on hit.
/// Compiled artifacts are additionally persisted to disk under
/// `.chidori/wasm-cache/` so the first call after a fresh `cargo run`
/// doesn't re-pay the ~30s Cranelift compile cost for the RustPython
/// binary. Cache files are loaded via `Module::deserialize`, which is
/// `unsafe` because the bytes are executed directly — we trust them only
/// because we wrote them into our own cache directory ourselves.
struct CachedArtifact {
    engine: Engine,
    module: Module,
}

fn artifact_cache() -> &'static Mutex<HashMap<u64, CachedArtifact>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, CachedArtifact>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(source: &[u8], memory_pages: u32) -> u64 {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    memory_pages.hash(&mut h);
    h.finish()
}

/// Disk-cache version tag. Bump this whenever the compile pipeline changes
/// in a way that invalidates previously-serialized bytes (compiler options,
/// metering cost function, wasmer version bump, etc.).
const DISK_CACHE_VERSION: u32 = 1;

/// Resolve the on-disk cache path for a given key, ensuring the parent
/// directory exists. Returns `None` if we can't materialize the directory
/// (e.g. home dir missing), in which case the in-memory cache still works.
fn disk_cache_path(key: u64) -> Option<std::path::PathBuf> {
    let base = std::path::PathBuf::from(".chidori").join("wasm-cache");
    if std::fs::create_dir_all(&base).is_err() {
        return None;
    }
    Some(base.join(format!("v{:02}-{:016x}.cwasm", DISK_CACHE_VERSION, key)))
}

/// Build a compilation engine with our standard cranelift + metering +
/// bounded-memory tunables config. Used by both the fresh-compile path and
/// the disk-cache deserialize path (which still needs a matching engine).
fn make_engine(memory_pages: u32) -> Engine {
    let mut compiler = Cranelift::new();
    let metering = Arc::new(Metering::new(u64::MAX, cost));
    compiler.push_middleware(metering);

    let mut engine: Engine = <Engine as NativeEngineExt>::new(
        Box::new(compiler),
        Target::default(),
        Default::default(),
    );
    engine.set_tunables(CappedTunables::new(memory_pages.max(1)));
    engine
}

/// Try to rehydrate a compiled module from the on-disk cache.
fn try_load_from_disk(key: u64, memory_pages: u32) -> Option<CachedArtifact> {
    let path = disk_cache_path(key)?;
    if !path.exists() {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let engine = make_engine(memory_pages);
    // SAFETY: We only read files we wrote into our own cache directory.
    // If a user manually tampers with `.chidori/wasm-cache/`, they are
    // opting into running that code. The version tag in the filename
    // guards against cross-wasmer-version collisions; a corrupt file
    // causes deserialize to fail and we fall through to a fresh compile.
    let module = match unsafe { Module::deserialize(&engine, bytes) } {
        Ok(m) => m,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    Some(CachedArtifact { engine, module })
}

/// Persist a freshly-compiled artifact to disk so the next process start
/// can skip the Cranelift compile. Best-effort — if the serialize or write
/// fails, we log and keep going.
fn try_save_to_disk(key: u64, artifact: &CachedArtifact) {
    let Some(path) = disk_cache_path(key) else {
        return;
    };
    match artifact.module.serialize() {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, &bytes) {
                tracing::warn!(
                    "wasm disk cache: failed to write {}: {}",
                    path.display(),
                    e
                );
            }
        }
        Err(e) => {
            tracing::warn!("wasm disk cache: serialize failed: {}", e);
        }
    }
}

/// Compile a fresh (engine, module) pair with metering + bounded-memory
/// tunables. This is the slow path; callers should try the disk cache first.
fn compile_artifact(wasm_source: &[u8], memory_pages: u32) -> Result<CachedArtifact> {
    let engine = make_engine(memory_pages);
    let store = Store::new(engine.clone());
    let module = Module::new(&store, wasm_source).context("Failed to compile WASM module")?;
    Ok(CachedArtifact { engine, module })
}

/// Resolve an artifact for (source, memory_pages): in-memory cache → disk
/// cache → fresh compile. Populates both caches on the way through.
fn load_or_compile(wasm_source: &[u8], memory_pages: u32) -> Result<(Engine, Module)> {
    let key = cache_key(wasm_source, memory_pages);
    {
        let cache = artifact_cache().lock().unwrap();
        if let Some(hit) = cache.get(&key) {
            return Ok((hit.engine.clone(), hit.module.clone()));
        }
    }

    if let Some(artifact) = try_load_from_disk(key, memory_pages) {
        let pair = (artifact.engine.clone(), artifact.module.clone());
        artifact_cache().lock().unwrap().insert(key, artifact);
        return Ok(pair);
    }

    let artifact = compile_artifact(wasm_source, memory_pages)?;
    try_save_to_disk(key, &artifact);
    let pair = (artifact.engine.clone(), artifact.module.clone());
    artifact_cache().lock().unwrap().insert(key, artifact);
    Ok(pair)
}

/// Host-side env state threaded into the `host.log` import so it can read
/// from the guest's linear memory and forward to the user's log callback.
struct HostEnv {
    /// The guest module's exported `memory`, populated after instantiation.
    /// `None` until we can look it up (and `None` forever for modules that
    /// don't export memory — `host.log` silently drops in that case).
    memory: Option<Memory>,
    log_callback: Option<LogFn>,
}

/// Implementation of the `host.log(ptr: i32, len: i32)` import.
///
/// Reads `len` bytes from the guest's exported `memory` starting at `ptr`
/// and passes the decoded UTF-8 string to the env's log callback. Also emits
/// a `tracing::info!` event targeted at `wasm_sandbox` so `--verbose` runs
/// show guest logs even without a caller-supplied callback.
fn host_log_impl(mut env: FunctionEnvMut<HostEnv>, ptr: i32, len: i32) {
    let (data, store) = env.data_and_store_mut();
    let Some(ref memory) = data.memory else { return };
    if len < 0 || ptr < 0 {
        return;
    }
    let view = memory.view(&store);
    let mut buf = vec![0u8; len as usize];
    if view.read(ptr as u64, &mut buf).is_err() {
        return;
    }
    let Ok(msg) = std::str::from_utf8(&buf) else { return };
    tracing::info!(target: "wasm_sandbox", "{}", msg);
    if let Some(cb) = data.log_callback.clone() {
        cb(msg);
    }
}

/// Execute a WASM module inside a bounded sandbox and return the function's results.
pub fn exec_wasm(req: ExecRequest) -> Result<ExecResult> {
    let (engine, module) = load_or_compile(&req.wasm_source, req.memory_pages)?;

    let mut store = Store::new(engine);

    // Build the host env that the `host.log` import reads from. `memory` is
    // filled in after instantiation, once we can resolve the module's export.
    let env = FunctionEnv::new(
        &mut store,
        HostEnv {
            memory: None,
            log_callback: req.log_callback.clone(),
        },
    );
    let host_log = Function::new_typed_with_env(&mut store, &env, host_log_impl);
    let host_imports = imports! {
        "host" => {
            "log" => host_log,
        },
    };

    let instance = Instance::new(&mut store, &module, &host_imports)
        .map_err(|e| anyhow!("Failed to instantiate WASM module: {}", e))?;

    // If the module exported its memory, wire it into the env so `host.log`
    // can read guest strings. Modules that don't export memory work fine —
    // they just can't call `host.log`.
    if let Ok(memory) = instance.exports.get_memory("memory") {
        env.as_mut(&mut store).memory = Some(memory.clone());
    }

    // Reset the fuel budget to what this specific call asked for.
    set_remaining_points(&mut store, &instance, req.fuel);

    // Locate the exported function.
    let func = instance
        .exports
        .get_function(&req.function)
        .with_context(|| format!("No exported function named `{}`", req.function))?;

    // Coerce our ExecRequest args into the exact types the function expects.
    let sig = func.ty(&store);
    let params = sig.params();
    if params.len() != req.args.len() {
        bail!(
            "arity mismatch: `{}` takes {} args, got {}",
            req.function,
            params.len(),
            req.args.len()
        );
    }
    let coerced: Vec<WasmValue> = params
        .iter()
        .zip(req.args.iter())
        .map(|(expected, arg)| coerce_arg(expected, arg))
        .collect::<Result<_>>()?;

    // Call the function. Fuel exhaustion surfaces as a trap from inside
    // wasmer — we translate it into a clean error for the agent.
    let call_result = func.call(&mut store, &coerced);
    let returns = match call_result {
        Ok(vals) => vals.iter().map(wasm_value_to_json).collect(),
        Err(e) => {
            return match get_remaining_points(&mut store, &instance) {
                MeteringPoints::Exhausted => Err(anyhow!(
                    "WASM fuel exhausted after {} instructions",
                    req.fuel
                )),
                MeteringPoints::Remaining(_) => Err(anyhow!("WASM trap: {}", e)),
            };
        }
    };

    let fuel_remaining = match get_remaining_points(&mut store, &instance) {
        MeteringPoints::Remaining(n) => Some(n),
        MeteringPoints::Exhausted => None,
    };

    Ok(ExecResult {
        returns,
        fuel_remaining,
    })
}

/// Prebuilt expression-runtime WASM binary. Built from `sandbox-runtime/` at
/// dev time via `cargo build --target wasm32-unknown-unknown --release -p
/// sandbox-runtime` and embedded here. See that crate for the supported DSL.
pub const EXPR_RUNTIME_WASM: &[u8] = include_bytes!(
    "../../sandbox-runtime/target/wasm32-unknown-unknown/release/sandbox_runtime.wasm"
);

/// Evaluate a miniscript expression using the embedded WASM runtime.
///
/// The runtime is a small recursive-descent interpreter (let/in, if/then/else,
/// integers + booleans, `+ - * / %`, comparisons, `&& || !`) cross-compiled
/// to wasm32 and shipped inside the host binary. Host-supplied `vars` are
/// passed by prepending `let name = value in …` chains to the user source
/// before it enters the sandbox — the interpreter itself doesn't need a
/// separate environment concept.
///
/// Returns the result as a string (decimal for ints, `"true"`/`"false"` for
/// bools). Full JS/Python sandboxing would layer a heavyweight precompiled
/// interpreter the same way.
pub fn exec_expr(source: &str, vars: &serde_json::Map<String, JsonValue>, fuel: u64) -> Result<String> {
    let mut prelude = String::new();
    for (name, value) in vars {
        if !is_valid_ident(name) {
            bail!("invalid var name `{}` (must be identifier)", name);
        }
        let value_lit = match value {
            JsonValue::Number(n) if n.is_i64() => n.to_string(),
            JsonValue::Bool(b) => b.to_string(),
            other => bail!("var `{}` has unsupported type: {}", name, other),
        };
        prelude.push_str("let ");
        prelude.push_str(name);
        prelude.push_str(" = ");
        prelude.push_str(&value_lit);
        prelude.push_str(" in ");
    }
    let full_source = format!("{}{}", prelude, source);

    if full_source.len() > 16 * 1024 {
        bail!("exec_expr source + vars exceeds 16 KiB scratch buffer");
    }

    // Rust-compiled wasm needs more headroom than a hand-written WAT module:
    // it reserves a stack + bump heap + data section. 17 pages = 1 MiB is
    // the minimum that lets our sandbox-runtime instantiate cleanly.
    // Rust-compiled wasm needs significant headroom: rustc reserves a stack,
    // our crate's 256 KiB bump heap lives in .bss, and the data/rodata
    // sections add on top. 32 pages = 2 MiB is comfortable; dropping below
    // ~24 traps with "out of bounds memory access" at eval time as the
    // bump allocator walks past the declared cap.
    let memory_pages: u32 = 32;
    let (engine, module) = load_or_compile(EXPR_RUNTIME_WASM, memory_pages)?;

    let mut store = Store::new(engine);
    let env = FunctionEnv::new(
        &mut store,
        HostEnv {
            memory: None,
            log_callback: None,
        },
    );
    let host_log = Function::new_typed_with_env(&mut store, &env, host_log_impl);
    let host_imports = imports! {
        "host" => { "log" => host_log },
    };

    let instance = Instance::new(&mut store, &module, &host_imports)
        .map_err(|e| anyhow!("Failed to instantiate expr runtime: {}", e))?;
    set_remaining_points(&mut store, &instance, fuel);

    let memory = instance
        .exports
        .get_memory("memory")
        .context("expr runtime didn't export memory")?;
    env.as_mut(&mut store).memory = Some(memory.clone());

    // Locate the exported scratch buffer inside the guest and copy the source
    // bytes into it so `eval()` can decode them.
    let scratch_ptr_fn = instance
        .exports
        .get_typed_function::<(), i32>(&store, "scratch_ptr")?;
    let scratch_ptr = scratch_ptr_fn.call(&mut store)?;
    let view = memory.view(&store);
    view.write(scratch_ptr as u64, full_source.as_bytes())
        .context("failed to write source into guest memory")?;

    let eval_fn = instance
        .exports
        .get_typed_function::<i32, i64>(&store, "eval")?;
    let encoded = match eval_fn.call(&mut store, full_source.len() as i32) {
        Ok(v) => v,
        Err(e) => {
            return match get_remaining_points(&mut store, &instance) {
                MeteringPoints::Exhausted => {
                    Err(anyhow!("exec_expr fuel exhausted after {} instructions", fuel))
                }
                MeteringPoints::Remaining(_) => Err(anyhow!("expr runtime trap: {}", e)),
            };
        }
    };

    let result_ptr = ((encoded >> 32) & 0xFFFF_FFFF) as u32;
    let result_len = (encoded & 0xFFFF_FFFF) as u32;
    let mut buf = vec![0u8; result_len as usize];
    let view = memory.view(&store);
    view.read(result_ptr as u64, &mut buf)
        .context("failed to read result from guest memory")?;
    let out = String::from_utf8(buf).context("expr runtime returned non-utf8 result")?;

    if let Some(err) = out.strip_prefix("ERR:") {
        bail!("expr runtime error: {}", err);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Python language layer (RustPython compiled to wasm32-wasip1)
// ---------------------------------------------------------------------------

/// Prebuilt Python-runtime WASM binary. Built from `sandbox-python/` via
/// `cargo build --release --target wasm32-wasip1` and embedded here.
pub const PYTHON_RUNTIME_WASM: &[u8] = include_bytes!(
    "../../sandbox-python/target/wasm32-wasip1/release/sandbox-python.wasm"
);

/// Shared state for our hand-rolled WASI preview 1 shim. Instead of pulling
/// in `wasmer-wasix` (which drags in reqwest, tokio networking, virtual-fs,
/// webc, and a dozen more crates), we implement just the 18 preview-1
/// functions the RustPython binary actually imports. Stdin is pre-populated
/// with the user's Python source; stdout writes are captured into a buffer
/// the host reads back after `_start` returns.
struct WasiShim {
    memory: Option<Memory>,
    stdin: Vec<u8>,
    stdin_pos: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Deterministic wall clock value in nanoseconds. Replayable runs get
    /// the same time, unlike real wall clock.
    clock_ns: u64,
    /// Counter-based pseudorandom for `random_get`. Not cryptographic; the
    /// point is determinism.
    rng_state: u64,
    /// Set by `proc_exit` to unwind the call cleanly.
    exit_code: Option<i32>,
}

impl WasiShim {
    fn read_mem(&self, store: &impl wasmer::AsStoreRef, ptr: u32, len: u32) -> Option<Vec<u8>> {
        let memory = self.memory.as_ref()?;
        let view = memory.view(store);
        let mut buf = vec![0u8; len as usize];
        view.read(ptr as u64, &mut buf).ok()?;
        Some(buf)
    }

    fn write_mem(&self, store: &impl wasmer::AsStoreRef, ptr: u32, bytes: &[u8]) -> bool {
        let Some(memory) = self.memory.as_ref() else {
            return false;
        };
        memory.view(store).write(ptr as u64, bytes).is_ok()
    }

    fn next_rand(&mut self) -> u8 {
        // xorshift64
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        (x & 0xFF) as u8
    }
}

const WASI_ERRNO_SUCCESS: i32 = 0;
const WASI_ERRNO_BADF: i32 = 8;
const WASI_ERRNO_NOENT: i32 = 44;
const WASI_ERRNO_NOTSUP: i32 = 58;

// -- WASI host functions ----------------------------------------------------
// Each of these matches the wasi_snapshot_preview1 signature exactly. `i32`
// parameters are pointers into guest linear memory; return `i32` is errno.
// We keep them small and obvious — Python-in-sandbox doesn't need the full
// POSIX emulation wasmer-wasix ships.

fn wasi_args_get(_env: FunctionEnvMut<WasiShim>, _argv: i32, _argv_buf: i32) -> i32 {
    WASI_ERRNO_SUCCESS
}

fn wasi_args_sizes_get(
    mut env: FunctionEnvMut<WasiShim>,
    argc_ptr: i32,
    argv_buf_size_ptr: i32,
) -> i32 {
    let (data, store) = env.data_and_store_mut();
    let _ = data.write_mem(&store, argc_ptr as u32, &0u32.to_le_bytes());
    let _ = data.write_mem(&store, argv_buf_size_ptr as u32, &0u32.to_le_bytes());
    WASI_ERRNO_SUCCESS
}

fn wasi_environ_get(_env: FunctionEnvMut<WasiShim>, _environ: i32, _environ_buf: i32) -> i32 {
    WASI_ERRNO_SUCCESS
}

fn wasi_environ_sizes_get(
    mut env: FunctionEnvMut<WasiShim>,
    count_ptr: i32,
    buf_size_ptr: i32,
) -> i32 {
    let (data, store) = env.data_and_store_mut();
    let _ = data.write_mem(&store, count_ptr as u32, &0u32.to_le_bytes());
    let _ = data.write_mem(&store, buf_size_ptr as u32, &0u32.to_le_bytes());
    WASI_ERRNO_SUCCESS
}

fn wasi_clock_time_get(
    mut env: FunctionEnvMut<WasiShim>,
    _clock_id: i32,
    _precision: i64,
    time_ptr: i32,
) -> i32 {
    let (data, store) = env.data_and_store_mut();
    let now = data.clock_ns;
    let _ = data.write_mem(&store, time_ptr as u32, &now.to_le_bytes());
    WASI_ERRNO_SUCCESS
}

fn wasi_fd_close(_env: FunctionEnvMut<WasiShim>, _fd: i32) -> i32 {
    WASI_ERRNO_SUCCESS
}

fn wasi_fd_fdstat_get(
    mut env: FunctionEnvMut<WasiShim>,
    _fd: i32,
    stat_ptr: i32,
) -> i32 {
    // 24-byte fdstat: fs_filetype(u8) + pad + fs_flags(u16) + pad +
    // fs_rights_base(u64) + fs_rights_inheriting(u64). Zero-filled is fine;
    // RustPython only checks a couple of fields.
    let (data, store) = env.data_and_store_mut();
    let zeros = [0u8; 24];
    let _ = data.write_mem(&store, stat_ptr as u32, &zeros);
    WASI_ERRNO_SUCCESS
}

fn wasi_fd_filestat_get(_env: FunctionEnvMut<WasiShim>, _fd: i32, _stat_ptr: i32) -> i32 {
    WASI_ERRNO_BADF
}

fn wasi_fd_prestat_get(_env: FunctionEnvMut<WasiShim>, _fd: i32, _prestat_ptr: i32) -> i32 {
    // No preopens: always return BADF so the guest's preopen walker stops.
    WASI_ERRNO_BADF
}

fn wasi_fd_prestat_dir_name(
    _env: FunctionEnvMut<WasiShim>,
    _fd: i32,
    _path: i32,
    _path_len: i32,
) -> i32 {
    WASI_ERRNO_BADF
}

/// Read from stdin (fd 0) by copying out of the preloaded buffer. Any other
/// fd returns BADF. The guest passes an iovec array at `iovs_ptr` with
/// `iovs_len` entries; we walk them and fill each until stdin is exhausted.
fn wasi_fd_read(
    mut env: FunctionEnvMut<WasiShim>,
    fd: i32,
    iovs_ptr: i32,
    iovs_len: i32,
    nread_ptr: i32,
) -> i32 {
    if fd != 0 {
        return WASI_ERRNO_BADF;
    }
    let (data, store) = env.data_and_store_mut();
    let mut total = 0usize;
    for i in 0..iovs_len {
        let iov_off = iovs_ptr as u32 + (i as u32) * 8;
        let Some(iov_bytes) = data.read_mem(&store, iov_off, 8) else {
            return WASI_ERRNO_BADF;
        };
        let buf_ptr = u32::from_le_bytes(iov_bytes[0..4].try_into().unwrap());
        let buf_len = u32::from_le_bytes(iov_bytes[4..8].try_into().unwrap()) as usize;
        if data.stdin_pos >= data.stdin.len() {
            break;
        }
        let avail = data.stdin.len() - data.stdin_pos;
        let n = core::cmp::min(avail, buf_len);
        let slice = &data.stdin[data.stdin_pos..data.stdin_pos + n].to_vec();
        if !data.write_mem(&store, buf_ptr, slice) {
            return WASI_ERRNO_BADF;
        }
        data.stdin_pos += n;
        total += n;
    }
    let _ = data.write_mem(&store, nread_ptr as u32, &(total as u32).to_le_bytes());
    WASI_ERRNO_SUCCESS
}

/// Write to stdout (fd 1) or stderr (fd 2); anything else is BADF.
fn wasi_fd_write(
    mut env: FunctionEnvMut<WasiShim>,
    fd: i32,
    iovs_ptr: i32,
    iovs_len: i32,
    nwritten_ptr: i32,
) -> i32 {
    if fd != 1 && fd != 2 {
        return WASI_ERRNO_BADF;
    }
    let (data, store) = env.data_and_store_mut();
    let mut total = 0usize;
    for i in 0..iovs_len {
        let iov_off = iovs_ptr as u32 + (i as u32) * 8;
        let Some(iov_bytes) = data.read_mem(&store, iov_off, 8) else {
            return WASI_ERRNO_BADF;
        };
        let buf_ptr = u32::from_le_bytes(iov_bytes[0..4].try_into().unwrap());
        let buf_len = u32::from_le_bytes(iov_bytes[4..8].try_into().unwrap());
        let Some(bytes) = data.read_mem(&store, buf_ptr, buf_len) else {
            return WASI_ERRNO_BADF;
        };
        if fd == 1 {
            data.stdout.extend_from_slice(&bytes);
        } else {
            data.stderr.extend_from_slice(&bytes);
        }
        total += buf_len as usize;
    }
    let _ = data.write_mem(&store, nwritten_ptr as u32, &(total as u32).to_le_bytes());
    WASI_ERRNO_SUCCESS
}

fn wasi_path_filestat_get(
    _env: FunctionEnvMut<WasiShim>,
    _fd: i32,
    _flags: i32,
    _path: i32,
    _path_len: i32,
    _stat_ptr: i32,
) -> i32 {
    WASI_ERRNO_NOENT
}

fn wasi_path_open(
    _env: FunctionEnvMut<WasiShim>,
    _fd: i32,
    _dirflags: i32,
    _path: i32,
    _path_len: i32,
    _oflags: i32,
    _rights_base: i64,
    _rights_inheriting: i64,
    _fdflags: i32,
    _opened_fd_ptr: i32,
) -> i32 {
    WASI_ERRNO_NOTSUP
}

fn wasi_poll_oneoff(
    _env: FunctionEnvMut<WasiShim>,
    _in_ptr: i32,
    _out_ptr: i32,
    _nsubscriptions: i32,
    _nevents_ptr: i32,
) -> i32 {
    WASI_ERRNO_NOTSUP
}

/// proc_exit is defined to never return; we trap the guest by returning a
/// user-level RuntimeError. The orchestrator below distinguishes a clean
/// proc_exit from a real crash by checking `exit_code` after the trap.
///
/// Wasmer's typed host function API requires us to actually return the
/// declared result type, so we declare this as returning `i32` (even
/// though the real WASI signature is `()`) and panic via the error path;
/// `call()` converts it to a trap. In practice `raise` would be cleaner,
/// but the 4.x API surfaces traps via `Err` returns from the caller
/// rather than a panic-inside-host hook, so we do this from the caller
/// side instead: mark `exit_code` and let the guest hit an unreachable.
fn wasi_proc_exit(mut env: FunctionEnvMut<WasiShim>, code: i32) -> Result<(), wasmer::RuntimeError> {
    env.data_mut().exit_code = Some(code);
    Err(wasmer::RuntimeError::user(Box::new(ProcExit(code))))
}

#[derive(Debug)]
struct ProcExit(i32);
impl std::fmt::Display for ProcExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proc_exit({})", self.0)
    }
}
impl std::error::Error for ProcExit {}

fn wasi_random_get(mut env: FunctionEnvMut<WasiShim>, buf_ptr: i32, buf_len: i32) -> i32 {
    let (data, store) = env.data_and_store_mut();
    let mut bytes = Vec::with_capacity(buf_len as usize);
    for _ in 0..buf_len {
        bytes.push(data.next_rand());
    }
    let _ = data.write_mem(&store, buf_ptr as u32, &bytes);
    WASI_ERRNO_SUCCESS
}

fn wasi_sched_yield(_env: FunctionEnvMut<WasiShim>) -> i32 {
    WASI_ERRNO_SUCCESS
}

/// Run a WASI preview 1 binary (the stdin-in / stdout-out contract our
/// language-layer guests use) and return captured stdout.
///
/// `label` is only used for error messages ("Python", "JavaScript", …).
/// The function handles everything the exec_python / exec_js wrappers had
/// in common: artifact cache lookup, fresh store, WasiShim env, all 18
/// WASI imports, memory wiring, fuel setup, `_start` invocation, and the
/// `proc_exit` / fuel / trap / ERR-prefix demultiplexing.
fn run_wasi_guest(
    wasm: &[u8],
    source: &str,
    memory_pages: u32,
    fuel: u64,
    label: &'static str,
) -> Result<String> {
    let (engine, module) = load_or_compile(wasm, memory_pages)?;
    let mut store = Store::new(engine);

    let env = FunctionEnv::new(
        &mut store,
        WasiShim {
            memory: None,
            stdin: source.as_bytes().to_vec(),
            stdin_pos: 0,
            stdout: Vec::with_capacity(256),
            stderr: Vec::with_capacity(64),
            clock_ns: 1_700_000_000_000_000_000, // fixed "now" for determinism
            rng_state: 0xDEAD_BEEF_CAFE_BABE,
            exit_code: None,
        },
    );

    let wasi_imports = imports! {
        "wasi_snapshot_preview1" => {
            "args_get" => Function::new_typed_with_env(&mut store, &env, wasi_args_get),
            "args_sizes_get" => Function::new_typed_with_env(&mut store, &env, wasi_args_sizes_get),
            "environ_get" => Function::new_typed_with_env(&mut store, &env, wasi_environ_get),
            "environ_sizes_get" => Function::new_typed_with_env(&mut store, &env, wasi_environ_sizes_get),
            "clock_time_get" => Function::new_typed_with_env(&mut store, &env, wasi_clock_time_get),
            "fd_close" => Function::new_typed_with_env(&mut store, &env, wasi_fd_close),
            "fd_fdstat_get" => Function::new_typed_with_env(&mut store, &env, wasi_fd_fdstat_get),
            "fd_filestat_get" => Function::new_typed_with_env(&mut store, &env, wasi_fd_filestat_get),
            "fd_prestat_get" => Function::new_typed_with_env(&mut store, &env, wasi_fd_prestat_get),
            "fd_prestat_dir_name" => Function::new_typed_with_env(&mut store, &env, wasi_fd_prestat_dir_name),
            "fd_read" => Function::new_typed_with_env(&mut store, &env, wasi_fd_read),
            "fd_write" => Function::new_typed_with_env(&mut store, &env, wasi_fd_write),
            "path_filestat_get" => Function::new_typed_with_env(&mut store, &env, wasi_path_filestat_get),
            "path_open" => Function::new_typed_with_env(&mut store, &env, wasi_path_open),
            "poll_oneoff" => Function::new_typed_with_env(&mut store, &env, wasi_poll_oneoff),
            "proc_exit" => Function::new_typed_with_env(&mut store, &env, wasi_proc_exit),
            "random_get" => Function::new_typed_with_env(&mut store, &env, wasi_random_get),
            "sched_yield" => Function::new_typed_with_env(&mut store, &env, wasi_sched_yield),
        },
    };

    let instance = Instance::new(&mut store, &module, &wasi_imports)
        .map_err(|e| anyhow!("Failed to instantiate {} sandbox: {}", label, e))?;

    if let Ok(memory) = instance.exports.get_memory("memory") {
        env.as_mut(&mut store).memory = Some(memory.clone());
    }

    set_remaining_points(&mut store, &instance, fuel);

    let start_fn = instance
        .exports
        .get_typed_function::<(), ()>(&store, "_start")
        .with_context(|| format!("{} sandbox is missing `_start`", label))?;

    let run_result = start_fn.call(&mut store);
    let guest = env.as_ref(&store);
    let captured = String::from_utf8_lossy(&guest.stdout).to_string();
    let exit_code = guest.exit_code;

    match run_result {
        Ok(()) => {}
        Err(e) => {
            if let Some(code) = exit_code {
                if code != 0 {
                    bail!("{} sandbox exited with code {}", label, code);
                }
            } else {
                return match get_remaining_points(&mut store, &instance) {
                    MeteringPoints::Exhausted => Err(anyhow!(
                        "exec_{} fuel exhausted after {} instructions",
                        label.to_lowercase(),
                        fuel
                    )),
                    MeteringPoints::Remaining(_) => {
                        Err(anyhow!("{} sandbox trap: {}", label, e))
                    }
                };
            }
        }
    }

    if let Some(err) = captured.strip_prefix("ERR:") {
        bail!("{} error: {}", label, err);
    }
    Ok(captured)
}

/// Evaluate a Python program inside the WASM sandbox and return the
/// `repr()` of a top-level `result` variable (or `"None"` if the program
/// didn't bind one). Errors surface with `ERR:` prefixed by the Python
/// exception type name.
///
/// Uses the embedded `sandbox-python` WASI binary — a small Rust program
/// that links against `rustpython-vm` and runs the guest's stdin as Python
/// source. The host side implements just enough of WASI preview 1 (18
/// functions, ~200 LOC) to get stdin, stdout, and the clock wired up, and
/// fuel metering still enforces the per-call instruction budget.
pub fn exec_python(source: &str, fuel: u64) -> Result<String> {
    if source.len() > 64 * 1024 {
        bail!("exec_python source exceeds 64 KiB limit");
    }
    // RustPython needs a lot of linear memory (stack + heap + data + stdlib
    // bytes). 128 pages = 8 MiB is the smallest value I've seen the VM
    // boot cleanly at.
    run_wasi_guest(PYTHON_RUNTIME_WASM, source, 128, fuel, "Python")
}

// ---------------------------------------------------------------------------
// JavaScript language layer (Boa compiled to wasm32-wasip1)
// ---------------------------------------------------------------------------

/// Prebuilt JavaScript-runtime WASM binary. Built from `sandbox-js/` via
/// `cargo build --release --target wasm32-wasip1` and embedded here.
pub const JS_RUNTIME_WASM: &[u8] = include_bytes!(
    "../../sandbox-js/target/wasm32-wasip1/release/sandbox-js.wasm"
);

/// Evaluate a JavaScript program inside the WASM sandbox and return the
/// `String(value)` of the final expression.
///
/// Uses the embedded `sandbox-js` WASI binary, which links against
/// `boa_engine` (default features off — no float16, xsum, intl, temporal,
/// or wasm-bindgen). Reuses the same hand-rolled WASI preview 1 shim as
/// the Python sandbox, since Boa's WASI-std crate set is a strict subset.
pub fn exec_js(source: &str, fuel: u64) -> Result<String> {
    if source.len() > 64 * 1024 {
        bail!("exec_js source exceeds 64 KiB limit");
    }
    // Boa is lighter than RustPython — 3.4 MB vs 7.6 MB — so it needs less
    // headroom. 64 pages = 4 MiB is comfortable in practice.
    run_wasi_guest(JS_RUNTIME_WASM, source, 64, fuel, "JavaScript")
}

fn is_valid_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn coerce_arg(expected: &wasmer::Type, arg: &WasmArg) -> Result<WasmValue> {
    use wasmer::Type::*;
    Ok(match (expected, arg) {
        (I32, WasmArg::I32(v)) => WasmValue::I32(*v),
        (I32, WasmArg::I64(v)) => WasmValue::I32(*v as i32),
        (I64, WasmArg::I64(v)) => WasmValue::I64(*v),
        (I64, WasmArg::I32(v)) => WasmValue::I64(*v as i64),
        (F32, WasmArg::F32(v)) => WasmValue::F32(*v),
        (F32, WasmArg::F64(v)) => WasmValue::F32(*v as f32),
        (F64, WasmArg::F64(v)) => WasmValue::F64(*v),
        (F64, WasmArg::F32(v)) => WasmValue::F64(*v as f64),
        (other, _) => bail!("unsupported WASM parameter type: {:?}", other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADD_WAT: &str = r#"
        (module
            (func $add (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add)
        )
    "#;

    const INFINITE_WAT: &str = r#"
        (module
            (func $loop_forever (export "loop") (result i32)
                (loop $lp
                    br $lp
                )
                i32.const 0
            )
        )
    "#;

    #[test]
    fn test_add() {
        let result = exec_wasm(ExecRequest {
            wasm_source: ADD_WAT.as_bytes().to_vec(),
            function: "add".into(),
            args: vec![WasmArg::I32(2), WasmArg::I32(3)],
            fuel: 1_000_000,
            memory_pages: 1,
            log_callback: None,
        })
        .unwrap();
        assert_eq!(result.returns, vec![JsonValue::from(5)]);
        assert!(result.fuel_remaining.unwrap() < 1_000_000);
    }

    #[test]
    fn test_cache_reuse() {
        // Run the same module twice with different fuel budgets. The second
        // call should hit the compiled-artifact cache yet still get its own
        // fuel budget applied (i.e. set_remaining_points takes effect).
        let make = |fuel| ExecRequest {
            wasm_source: ADD_WAT.as_bytes().to_vec(),
            function: "add".into(),
            args: vec![WasmArg::I32(10), WasmArg::I32(20)],
            fuel,
            memory_pages: 1,
            log_callback: None,
        };
        let first = exec_wasm(make(1_000_000)).unwrap();
        let second = exec_wasm(make(500)).unwrap();
        assert_eq!(first.returns, vec![JsonValue::from(30)]);
        assert_eq!(second.returns, vec![JsonValue::from(30)]);
        // Fuel budgets are applied per-call, not cached. The first call
        // started with 1M and used ~4 ops; the second started with 500
        // and also used ~4 ops.
        assert!(first.fuel_remaining.unwrap() > 900_000);
        assert!(second.fuel_remaining.unwrap() < 500);
    }

    /// Module that imports `host.log`, embeds the string "hi from wasm" at
    /// offset 0 in memory via a data segment, and calls the import on entry.
    const HOST_LOG_WAT: &str = r#"
        (module
            (import "host" "log" (func $log (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "hi from wasm")
            (func (export "run")
                i32.const 0
                i32.const 12
                call $log)
        )
    "#;

    #[test]
    fn test_exec_expr_infix() {
        let empty = serde_json::Map::new();
        // Arithmetic + precedence.
        assert_eq!(exec_expr("2 + 3", &empty, 1_000_000).unwrap(), "5");
        assert_eq!(exec_expr("2 + 3 * 4", &empty, 1_000_000).unwrap(), "14");
        assert_eq!(exec_expr("(2 + 3) * 4", &empty, 1_000_000).unwrap(), "20");
        // Let bindings chain.
        assert_eq!(
            exec_expr("let x = 5 in let y = x * 2 in x + y", &empty, 1_000_000).unwrap(),
            "15"
        );
        // If/then/else returns the right branch.
        assert_eq!(
            exec_expr("if 10 > 3 then 100 else 200", &empty, 1_000_000).unwrap(),
            "100"
        );
        // Booleans + logical ops with short-circuit.
        assert_eq!(
            exec_expr("true && (1 < 2)", &empty, 1_000_000).unwrap(),
            "true"
        );
        assert_eq!(
            exec_expr("false || !false", &empty, 1_000_000).unwrap(),
            "true"
        );
    }

    #[test]
    fn test_exec_expr_with_vars() {
        let mut vars = serde_json::Map::new();
        vars.insert("a".into(), serde_json::json!(7));
        vars.insert("b".into(), serde_json::json!(6));
        // Host prepends `let a = 7 in let b = 6 in …` to the source.
        assert_eq!(exec_expr("a * b + 1", &vars, 1_000_000).unwrap(), "43");
        assert_eq!(
            exec_expr("if a > b then a - b else b - a", &vars, 1_000_000).unwrap(),
            "1"
        );
    }

    #[test]
    fn test_exec_python_basic() {
        // Top-level `result` variable is the contract for pulling a value
        // back out of the sandbox — matches Jupyter's final-expression
        // convention without needing the VM to track statement-exprs.
        let out = exec_python("result = 6 * 7", 200_000_000).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn test_exec_python_multistatement() {
        let out = exec_python(
            "\
def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)
result = fact(5)
",
            200_000_000,
        )
        .unwrap();
        assert_eq!(out, "120");
    }

    #[test]
    fn test_exec_python_error() {
        let err = exec_python("result = 1 / 0", 200_000_000).unwrap_err();
        assert!(
            err.to_string().contains("ZeroDivisionError"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_exec_js_basic() {
        let out = exec_js("6 * 7", 200_000_000).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn test_exec_js_function() {
        let out = exec_js(
            "\
function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); }
fact(6)
",
            200_000_000,
        )
        .unwrap();
        assert_eq!(out, "720");
    }

    #[test]
    fn test_exec_js_objects() {
        // Final expression: join a derived array.
        let out = exec_js(
            "\
const xs = [1, 2, 3, 4, 5];
const doubled = xs.map(x => x * 2);
doubled.reduce((a, b) => a + b, 0)
",
            200_000_000,
        )
        .unwrap();
        assert_eq!(out, "30");
    }

    #[test]
    fn test_exec_js_error() {
        let err = exec_js("throw new Error('boom')", 200_000_000).unwrap_err();
        assert!(
            err.to_string().contains("boom") || err.to_string().contains("Error"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_exec_expr_errors() {
        let empty = serde_json::Map::new();
        assert!(exec_expr("1 / 0", &empty, 1_000_000)
            .unwrap_err()
            .to_string()
            .contains("div by zero"));
        assert!(exec_expr("missing_var + 1", &empty, 1_000_000)
            .unwrap_err()
            .to_string()
            .contains("unbound variable"));
        assert!(exec_expr("1 + true", &empty, 1_000_000)
            .unwrap_err()
            .to_string()
            .contains("type error"));
    }

    #[test]
    fn test_host_log_bridge() {
        use std::sync::Mutex;
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let cb: LogFn = Arc::new(move |msg: &str| sink.lock().unwrap().push(msg.to_string()));

        exec_wasm(ExecRequest {
            wasm_source: HOST_LOG_WAT.as_bytes().to_vec(),
            function: "run".into(),
            args: vec![],
            fuel: 1_000_000,
            memory_pages: 1,
            log_callback: Some(cb),
        })
        .unwrap();

        let msgs = captured.lock().unwrap().clone();
        assert_eq!(msgs, vec!["hi from wasm".to_string()]);
    }

    #[test]
    fn test_fuel_exhaustion() {
        let err = exec_wasm(ExecRequest {
            wasm_source: INFINITE_WAT.as_bytes().to_vec(),
            function: "loop".into(),
            args: vec![],
            fuel: 10_000,
            memory_pages: 1,
            log_callback: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("fuel exhausted"));
    }
}
