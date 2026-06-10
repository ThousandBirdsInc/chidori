//! Test262 conformance runner for chidori's embedded QuickJS runtime.
//!
//! Test262 is the official TC39 ECMAScript conformance suite. It is the one
//! test corpus that both Bun (JavaScriptCore) and Node (V8) publish numbers
//! against, which makes it the apples-to-apples yardstick for "is chidori's JS
//! runtime at parity with bun/node". This binary drives the *bare* ECMAScript
//! context — no `chidori` host object, no captured-effect prelude — so the
//! number it reports is pure language conformance, directly comparable to the
//! numbers Bun and Node report.
//!
//! Usage:
//!   test262-runner [--test262 <dir>] [--filter <substr>] [--max <n>]
//!                  [--json <out>] [--verbose] [--no-modules] [--intl] [paths...]
//!
//! `paths` are file or directory paths relative to the Test262 root (default:
//! `test/language` and `test/built-ins`). The suite is located via `--test262`,
//! then `$TEST262_DIR`, then `./vendor/test262`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::ptr;

use chidori_quickjs::{sys, EvalMode, JsThrow, RuntimeLimits, SnapshotRuntime};
use serde::Deserialize;

thread_local! {
    /// Directory that `import(specifier)` and module imports resolve against —
    /// the directory of the test file currently executing. Set per run.
    static CURRENT_MODULE_DIR: RefCell<PathBuf> = RefCell::new(PathBuf::new());
}

const USAGE: &str = "\
test262-runner — run Test262 against chidori's embedded QuickJS runtime

usage:
  test262-runner [--test262 <dir>] [--filter <substr>] [--max <n>]
                 [--json <out>] [--verbose] [--no-modules] [--intl] [paths...]

options:
  --test262 <dir>   Test262 root (else $TEST262_DIR, else vendor/test262)
  paths...          files/dirs under the root (default: test/language test/built-ins)
  --filter <substr> only run paths containing the substring
  --max <n>         stop after n test files (smoke runs)
  --json <out>      write a per-file JSON report
  --state <file>    persist per-test results; a run updates only the tests it
                    executes, then prints the whole-suite total from the store
                    (so targeted re-runs refresh global stats without a full run)
  --verbose, -v     print each failure with the thrown message
  --no-modules      skip module-flag tests (they run by default)
  --intl            also run intl402 tests
  --help, -h        show this help";

const JS_TAG_EXCEPTION: i64 = 6;
const JS_EVAL_TYPE_MODULE: i32 = 1;
const JS_EVAL_FLAG_COMPILE_ONLY: i32 = 1 << 5;

/// QuickJS module loader: resolves a (already-normalized) specifier against the
/// current test's directory, reads it, and compiles it as a module. This is
/// what makes `import()` and module-flag tests load their `_FIXTURE` files —
/// the engine already parses `import()`, it just had no loader registered.
/// Returns null (engine then throws) when the file is missing or fails to
/// compile, mirroring the reference qjs loader.
unsafe extern "C" fn module_loader(
    ctx: *mut sys::JSContext,
    module_name: *const c_char,
    _opaque: *mut c_void,
) -> *mut sys::JSModuleDef {
    let name = match CStr::from_ptr(module_name).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ptr::null_mut(),
    };
    let path = CURRENT_MODULE_DIR.with(|d| d.borrow().join(&name));
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(_) => return ptr::null_mut(),
    };
    let (Ok(csrc), Ok(cname)) = (CString::new(bytes), CString::new(name)) else {
        return ptr::null_mut();
    };
    let val = sys::JS_Eval(
        ctx,
        csrc.as_ptr(),
        csrc.as_bytes().len(),
        cname.as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
    );
    if val.tag == JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    // On success the value carries the JSModuleDef*; free the wrapper (the
    // module stays registered in the context) and hand back the pointer.
    let module = val.u.ptr as *mut sys::JSModuleDef;
    sys::JS_FreeValue(ctx, val);
    module
}

/// Per-test metadata parsed from the `/*--- ... ---*/` YAML frontmatter.
#[derive(Debug, Default, Deserialize)]
struct Meta {
    #[serde(default)]
    negative: Option<Negative>,
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Negative {
    phase: String,
    #[serde(rename = "type")]
    type_: String,
}

impl Meta {
    fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

/// Which source-text form a single test execution uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Sloppy,
    Strict,
    Module,
    Raw,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Variant::Sloppy => "sloppy",
            Variant::Strict => "strict",
            Variant::Module => "module",
            Variant::Raw => "raw",
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

/// Features QuickJS does not implement (or that this runner cannot host).
/// Tests requiring any of these are skipped rather than counted as failures,
/// so the conformance number reflects the implemented surface honestly. Bun and
/// Node likewise skip features their engines lack.
const UNSUPPORTED_FEATURES: &[&str] = &[
    // Concurrency / shared memory — not in the embedded runtime.
    "Atomics",
    "SharedArrayBuffer",
    "Atomics.waitAsync",
    // Intl — QuickJS ships no ICU/Intl.
    "Intl.Locale-info",
    // Engine sugar QuickJS does not implement.
    "decorators",
    "tail-call-optimization",
    "IsHTMLDDA",
    "Temporal",
    "Array.fromAsync",
    "import-assertions",
    "import-attributes",
    "iterator-helpers",
    "regexp-modifiers",
    "regexp-duplicate-named-groups",
    "regexp-v-flag",
    "uint8array-base64",
    "source-phase-imports",
    "FinalizationRegistry",
    "WeakRef",
    // Host capabilities this runner does not provide. `cross-realm` needs
    // `$262.createRealm` (a second realm with cross-realm marshaling), which the
    // bare context cannot host; `ShadowRealm` is unimplemented in QuickJS. Bun
    // and Node likewise skip what their host/engine lacks.
    "cross-realm",
    "ShadowRealm",
    // Stage-2/3 proposals not implemented by this QuickJS build. Verified absent
    // (no implemented-surface passes to hide), so counting them as failures
    // would understate conformance of what IS implemented.
    "joint-iteration",         // Iterator.zip / Iterator.zipKeyed
    "iterator-sequencing",     // Iterator.concat
    "import-defer",            // import defer
    "upsert",                  // Map/WeakMap.prototype.getOrInsert
    "immutable-arraybuffer",   // ArrayBuffer immutable / transfer-to-immutable
    "error-stack-accessor",    // Error.prototype.stack accessor semantics
    "await-dictionary",        // Promise.{all,allSettled,...}Keyed
    "json-parse-with-source",  // JSON.parse source / rawJSON
];

#[derive(Default)]
struct Tally {
    pass: u64,
    fail: u64,
    skip: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EngineKind {
    QuickJs,
    Rust,
}

struct Args {
    root: PathBuf,
    paths: Vec<String>,
    filter: Option<String>,
    max: Option<u64>,
    json: Option<PathBuf>,
    verbose: bool,
    modules: bool,
    intl: bool,
    engine: EngineKind,
    /// Persistent per-test result store. When set, this run UPDATES only the
    /// entries for the tests it executes, then recomputes and prints the
    /// whole-suite total from the merged store — so a targeted re-run (e.g. one
    /// directory) refreshes the global stats without re-running everything.
    state: Option<PathBuf>,
}

fn main() -> ExitCode {
    // The pure-Rust engine's regex matcher and deep recursion can need a large
    // native stack on pathological inputs; run on a big-stack thread so a single
    // deep test can't abort the whole conformance run.
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap()
}

fn run() -> ExitCode {
    // Caught panics are reported as failures; suppress their stderr spam.
    std::panic::set_hook(Box::new(|_| {}));
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("error: {msg}\n");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let harness_dir = args.root.join("harness");
    if !harness_dir.is_dir() {
        eprintln!(
            "error: {} does not look like a Test262 checkout (missing harness/).\n\
             Run scripts/test262.sh to vendor it, or pass --test262 <dir>.",
            args.root.display()
        );
        return ExitCode::from(2);
    }

    let mut harness = HarnessCache::new(harness_dir);

    // Resolve the set of test files to run.
    let mut files = Vec::new();
    let roots: Vec<PathBuf> = if args.paths.is_empty() {
        vec![
            args.root.join("test/language"),
            args.root.join("test/built-ins"),
        ]
    } else {
        args.paths.iter().map(|p| resolve_path(&args.root, p)).collect()
    };
    for r in &roots {
        collect_tests(r, &mut files);
    }
    files.sort();

    let mut tally = Tally::default();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut report = Vec::new();

    // Persistent state: the merged per-test result store (loaded if it exists).
    // This run overwrites only the entries for the tests it executes.
    let mut state: Option<std::collections::BTreeMap<String, String>> =
        args.state.as_ref().map(|p| load_state(p));

    for file in &files {
        let rel = file
            .strip_prefix(&args.root)
            .unwrap_or(file)
            .to_string_lossy()
            .to_string();

        if let Some(filter) = &args.filter {
            if !rel.contains(filter.as_str()) {
                continue;
            }
        }
        if let Some(max) = args.max {
            if tally.pass + tally.fail + tally.skip >= max {
                break;
            }
        }

        let source = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                tally.fail += 1;
                failures.push((rel.clone(), format!("read error: {e}")));
                continue;
            }
        };

        let test_dir = file.parent().unwrap_or(Path::new("."));
        if std::env::var("T262_TRACE").is_ok() {
            eprintln!("RUNNING {}", rel);
        }
        let outcomes = run_test(&rel, &source, test_dir, &mut harness, &args);
        // A test passes only if every variant passes; skips collapse to skip.
        let mut variant_results = Vec::new();
        let mut any_fail = None;
        let mut all_skip = true;
        for (variant, outcome) in &outcomes {
            match outcome {
                Outcome::Pass => {
                    all_skip = false;
                    variant_results.push((variant.label(), "pass", String::new()));
                }
                Outcome::Skip(why) => {
                    variant_results.push((variant.label(), "skip", why.clone()));
                }
                Outcome::Fail(why) => {
                    all_skip = false;
                    if any_fail.is_none() {
                        any_fail = Some(format!("[{}] {}", variant.label(), why));
                    }
                    variant_results.push((variant.label(), "fail", why.clone()));
                }
            }
        }

        let status = if let Some(why) = any_fail {
            tally.fail += 1;
            failures.push((rel.clone(), why));
            "fail"
        } else if all_skip {
            tally.skip += 1;
            "skip"
        } else {
            tally.pass += 1;
            "pass"
        };

        if let Some(state) = state.as_mut() {
            state.insert(rel.clone(), status.to_string());
        }

        if args.json.is_some() {
            report.push(serde_json::json!({
                "file": rel,
                "status": status,
                "variants": variant_results
                    .iter()
                    .map(|(v, s, m)| serde_json::json!({"variant": v, "status": s, "message": m}))
                    .collect::<Vec<_>>(),
            }));
        }
    }

    if args.verbose {
        for (file, why) in &failures {
            println!("FAIL {file}\n      {why}");
        }
    }

    let total = tally.pass + tally.fail;
    let pct = if total > 0 {
        (tally.pass as f64) * 100.0 / (total as f64)
    } else {
        0.0
    };
    println!(
        "\nTest262 (chidori/QuickJS bare context)\n  pass {}  fail {}  skip {}  =>  {:.2}% of executed",
        tally.pass, tally.fail, tally.skip, pct
    );

    // Persist the merged state and report the WHOLE-SUITE total from it, so a
    // targeted re-run refreshes the global stats without re-running everything.
    if let (Some(path), Some(state)) = (&args.state, &state) {
        let (sp, sf, sk) = state.values().fold((0u64, 0u64, 0u64), |(p, f, k), v| {
            match v.as_str() {
                "pass" => (p + 1, f, k),
                "fail" => (p, f + 1, k),
                _ => (p, f, k + 1),
            }
        });
        let stotal = sp + sf;
        let spct = if stotal > 0 {
            (sp as f64) * 100.0 / (stotal as f64)
        } else {
            0.0
        };
        save_state(path, state, spct);
        let ran = tally.pass + tally.fail + tally.skip;
        println!(
            "\nPersisted total ({} tests, this run updated {}):\n  pass {}  fail {}  skip {}  =>  {:.2}% of executed\n  state: {}",
            state.len(),
            ran,
            sp,
            sf,
            sk,
            spct,
            path.display()
        );
    }

    if let Some(path) = &args.json {
        let doc = serde_json::json!({
            "summary": {"pass": tally.pass, "fail": tally.fail, "skip": tally.skip, "pass_pct": pct},
            "results": report,
        });
        if let Err(e) = fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
            eprintln!("warning: could not write report to {}: {e}", path.display());
        } else {
            println!("  report: {}", path.display());
        }
    }

    if tally.fail > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Run every applicable variant of a single test file and return their outcomes.
fn run_test(
    rel: &str,
    source: &str,
    test_dir: &Path,
    harness: &mut HarnessCache,
    args: &Args,
) -> Vec<(Variant, Outcome)> {
    let meta = match parse_meta(source) {
        Ok(m) => m,
        Err(e) => return vec![(Variant::Sloppy, Outcome::Fail(format!("bad metadata: {e}")))],
    };

    // Feature / category gating.
    if rel.contains("intl402") && !args.intl {
        return vec![(Variant::Sloppy, Outcome::Skip("intl402".into()))];
    }
    if let Some(feat) = meta
        .features
        .iter()
        .find(|f| UNSUPPORTED_FEATURES.contains(&f.as_str()))
    {
        return vec![(Variant::Sloppy, Outcome::Skip(format!("feature:{feat}")))];
    }
    if meta.has_flag("CanBlockIsFalse") || meta.has_flag("CanBlockIsTrue") {
        return vec![(Variant::Sloppy, Outcome::Skip("agent".into()))];
    }

    let variants = select_variants(&meta, args);
    variants
        .into_iter()
        .map(|v| {
            let outcome = match args.engine {
                EngineKind::QuickJs => run_variant(source, &meta, v, test_dir, harness),
                EngineKind::Rust => run_variant_rust(source, &meta, v, test_dir, rel, harness),
            };
            (v, outcome)
        })
        .collect()
}

/// Decide which execution variants apply, per the Test262 `flags` rules.
fn select_variants(meta: &Meta, args: &Args) -> Vec<Variant> {
    if meta.has_flag("raw") {
        return vec![Variant::Raw];
    }
    if meta.has_flag("module") {
        if args.modules {
            return vec![Variant::Module];
        }
        return vec![]; // skipped only when --no-modules is passed
    }
    let mut out = Vec::new();
    if !meta.has_flag("onlyStrict") {
        out.push(Variant::Sloppy);
    }
    if !meta.has_flag("noStrict") {
        out.push(Variant::Strict);
    }
    out
}

/// Bootstrap installed before the harness includes in every (non-raw) context:
/// a capturing `print`, and a minimal `$262` host hook. Tests that need `$262`
/// features we cannot provide (e.g. `detachArrayBuffer`) fail loudly rather
/// than silently passing.
const BOOTSTRAP: &str = r#"
globalThis.__t262_print = [];
globalThis.print = function (msg) { globalThis.__t262_print.push(String(msg)); };
globalThis.$262 = {
  global: globalThis,
  evalScript: function (src) { return (0, eval)(src); },
  gc: function () {},
  // Backed by the real QuickJS JS_DetachArrayBuffer, installed natively as
  // __t262_detachArrayBuffer before this bootstrap runs.
  detachArrayBuffer: globalThis.__t262_detachArrayBuffer,
  agent: undefined,
};
"#;

/// `$262.detachArrayBuffer(buffer)` — detaches `argv[0]` via the engine's real
/// `JS_DetachArrayBuffer`, so detached-buffer conformance tests exercise actual
/// engine behavior instead of a throwing stub. Returns `undefined`.
unsafe extern "C" fn t262_detach_array_buffer(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    if argc >= 1 && !argv.is_null() {
        chidori_quickjs::sys::JS_DetachArrayBuffer(ctx, *argv);
    }
    chidori_quickjs::sys::JSValue {
        u: chidori_quickjs::sys::JSValueUnion { int32: 0 },
        tag: 3, // JS_TAG_UNDEFINED
    }
}

fn run_variant(
    source: &str,
    meta: &Meta,
    variant: Variant,
    test_dir: &Path,
    harness: &mut HarnessCache,
) -> Outcome {
    let limits = RuntimeLimits {
        memory_limit_bytes: 256 * 1024 * 1024,
        interrupt_budget: 2_000_000_000,
    };
    let runtime = match SnapshotRuntime::new(limits) {
        Ok(r) => r,
        Err(e) => return Outcome::Fail(format!("runtime init: {e}")),
    };
    // Resolve `import()` / module imports against this test's directory.
    CURRENT_MODULE_DIR.with(|d| *d.borrow_mut() = test_dir.to_path_buf());
    unsafe {
        sys::JS_SetModuleLoaderFunc(
            runtime.raw_runtime(),
            None,
            Some(module_loader),
            ptr::null_mut(),
        );
    }
    let mut ctx = match runtime.new_context() {
        Ok(c) => c,
        Err(e) => return Outcome::Fail(format!("context init: {e}")),
    };

    let is_async = meta.has_flag("async");

    // 1. Install harness, unless this is a raw test (which runs alone).
    if variant != Variant::Raw {
        if let Err(e) = ctx.install_global_native_function(
            "__t262_detachArrayBuffer",
            Some(t262_detach_array_buffer),
            1,
        ) {
            return Outcome::Fail(format!("install detachArrayBuffer: {e}"));
        }
        if let Err(e) = ctx.eval_for_conformance("<bootstrap>", BOOTSTRAP, EvalMode::Script) {
            return Outcome::Fail(format!("bootstrap threw: {e}"));
        }
        let mut includes = vec!["assert.js".to_string(), "sta.js".to_string()];
        if is_async {
            includes.push("doneprintHandle.js".to_string());
        }
        includes.extend(meta.includes.iter().cloned());
        for inc in &includes {
            let body = match harness.load(inc) {
                Ok(b) => b,
                Err(e) => return Outcome::Fail(format!("harness {inc}: {e}")),
            };
            if let Err(e) = ctx.eval_for_conformance(inc, &body, EvalMode::Script) {
                return Outcome::Fail(format!("harness {inc} threw: {e}"));
            }
        }
    }

    // 2. Execute the test body in the right mode.
    let negative = meta.negative.as_ref();
    let run_mode = match variant {
        Variant::Raw | Variant::Sloppy => EvalMode::Script,
        Variant::Strict => EvalMode::StrictScript,
        Variant::Module => EvalMode::Module,
    };

    if let Some(neg) = negative {
        // Negative test: a throw is required, with a matching constructor name.
        // Only parse/early errors are catchable by compile-only; `resolution`
        // (module link) errors surface when the module is actually instantiated,
        // so they run the full pipeline like runtime errors.
        let is_parse = neg.phase == "parse" || neg.phase == "early";
        let mode = if is_parse {
            match variant {
                Variant::Strict => EvalMode::CompileStrictScript,
                Variant::Module => EvalMode::CompileModule,
                _ => EvalMode::CompileScript,
            }
        } else {
            run_mode
        };
        match ctx.eval_for_conformance("<test>", source, mode) {
            Ok(()) => {
                // Runtime-phase rejections may surface only after jobs drain.
                if !is_parse {
                    if let Err(thrown) = ctx.run_pending_jobs() {
                        return match_negative(neg, &thrown);
                    }
                }
                Outcome::Fail(format!(
                    "expected {} ({}) but no error was thrown",
                    neg.type_, neg.phase
                ))
            }
            Err(thrown) => match_negative(neg, &thrown),
        }
    } else {
        // Positive test: no throw, and async tests must signal completion.
        match ctx.eval_for_conformance("<test>", source, run_mode) {
            Ok(()) => {
                if let Err(thrown) = ctx.run_pending_jobs() {
                    return Outcome::Fail(format!("threw during jobs: {thrown}"));
                }
                if is_async {
                    check_async_done(&mut ctx)
                } else {
                    Outcome::Pass
                }
            }
            Err(thrown) => Outcome::Fail(format!("{}: {}", thrown.name, thrown.message)),
        }
    }
}

/// Bootstrap for the pure-Rust engine: a capturing `print` and a minimal `$262`.
/// `detachArrayBuffer` is backed by `__t262_detachArrayBuffer`, a native detach
/// installed on the engine before this runs (see `evaluate_rust`).
const BOOTSTRAP_RUST: &str = r#"
globalThis.__t262_print = [];
globalThis.print = function (msg) { globalThis.__t262_print.push(String(msg)); };
globalThis.$262 = {
  global: globalThis,
  gc: function () {},
  detachArrayBuffer: globalThis.__t262_detachArrayBuffer,
  agent: undefined,
};
"#;

/// Run a single Test262 variant against the pure-Rust `chidori-js` engine. We run
/// script variants (Raw/Sloppy/Strict) by concatenating bootstrap + harness +
/// test into one program; module variants are skipped (the Rust engine does not
/// yet implement the module-record pipeline). This measures the Rust engine's
/// language-conformance pass-rate on the same suite (plan P5).
fn run_variant_rust(
    source: &str,
    meta: &Meta,
    variant: Variant,
    test_dir: &Path,
    rel: &str,
    harness: &mut HarnessCache,
) -> Outcome {
    let is_async = meta.has_flag("async");
    let negative = meta.negative.as_ref();

    if variant == Variant::Module {
        return run_module_variant_rust(source, meta, test_dir, rel, harness, is_async, negative);
    }

    // Assemble the program.
    let mut program = String::new();
    // The strict variant runs the whole program in strict mode via a leading
    // directive prologue (the engine parses scripts as sloppy by default).
    if variant == Variant::Strict {
        program.push_str("\"use strict\";\n");
    }
    if variant != Variant::Raw {
        program.push_str(BOOTSTRAP_RUST);
        program.push('\n');
        let mut includes = vec!["assert.js".to_string(), "sta.js".to_string()];
        if is_async {
            includes.push("doneprintHandle.js".to_string());
        }
        includes.extend(meta.includes.iter().cloned());
        for inc in &includes {
            match harness.load(inc) {
                Ok(b) => {
                    program.push_str(&b);
                    program.push('\n');
                }
                Err(e) => return Outcome::Fail(format!("harness {inc}: {e}")),
            }
        }
    }
    program.push_str(source);

    // Run each test on its own worker thread joined with a wall-clock timeout, so
    // no single pathological test (e.g. a catastrophic regex or a near-budget
    // loop) can dominate or stall the whole conformance run. The thread keeps the
    // large native stack the engine needs for deep recursion. On timeout we record
    // a failure and abandon the worker (it is bounded by the op budget and will
    // exit on its own shortly).
    let is_async_w = is_async;
    let negative_w = negative.map(|n| (n.phase.clone(), n.type_.clone()));
    // Cooperative cancellation: the worker polls this flag via `vm.interrupt`; on
    // timeout we trip it so the worker stops grinding (and frees its CPU core)
    // rather than being abandoned to run to its op-budget in the background.
    let interrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let interrupt_w = interrupt.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
    // 256 MB is a ~10× margin over the realistic worst case (the VM caps JS
    // recursion at 2000 frames and regex at 100k steps); 1 GB per worker, spawned
    // once per test, needlessly inflated peak memory.
    let spawned = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let outcome = evaluate_rust(program, is_async_w, negative_w, interrupt_w);
            let _ = tx.send(outcome);
        });
    if spawned.is_err() {
        return Outcome::Fail("could not spawn test worker".into());
    }
    match rx.recv_timeout(rust_test_timeout()) {
        Ok(o) => o,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // Ask the worker to stop; it self-terminates promptly via the latched
            // op budget, then exits on its own (we do not join it).
            interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
            Outcome::Fail(format!("timeout (>{:?})", rust_test_timeout()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Outcome::Fail("engine panicked".into())
        }
    }
}

/// Run a `flags: [module]` test on the Rust engine. The harness includes run as
/// a sloppy script (defining the `assert`/`$DONE`/… globals) in the same realm,
/// then the test's module graph is loaded (its `import` specifiers resolved
/// against `test_dir`) and evaluated.
fn run_module_variant_rust(
    source: &str,
    meta: &Meta,
    test_dir: &Path,
    rel: &str,
    harness: &mut HarnessCache,
    is_async: bool,
    negative: Option<&Negative>,
) -> Outcome {
    // Harness prelude (sloppy script): bootstrap + assert/sta (+ async handler).
    let mut prelude = String::new();
    prelude.push_str(BOOTSTRAP_RUST);
    prelude.push('\n');
    let mut includes = vec!["assert.js".to_string(), "sta.js".to_string()];
    if is_async {
        includes.push("doneprintHandle.js".to_string());
    }
    includes.extend(meta.includes.iter().cloned());
    for inc in &includes {
        match harness.load(inc) {
            Ok(b) => {
                prelude.push_str(&b);
                prelude.push('\n');
            }
            Err(e) => return Outcome::Fail(format!("harness {inc}: {e}")),
        }
    }

    let entry_name = Path::new(rel)
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("__entry__.js"));
    let entry_path = test_dir.join(entry_name);
    let entry_source = source.to_string();
    let negative_w = negative.map(|n| (n.phase.clone(), n.type_.clone()));
    let is_async_w = is_async;
    let interrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let interrupt_w = interrupt.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
    let spawned = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let outcome = evaluate_rust_module(
                prelude,
                entry_source,
                entry_path,
                is_async_w,
                negative_w,
                interrupt_w,
            );
            let _ = tx.send(outcome);
        });
    if spawned.is_err() {
        return Outcome::Fail("could not spawn test worker".into());
    }
    match rx.recv_timeout(rust_test_timeout()) {
        Ok(o) => o,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
            Outcome::Fail(format!("timeout (>{:?})", rust_test_timeout()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Outcome::Fail("engine panicked".into())
        }
    }
}

/// Load (recursively) and evaluate a module graph, then classify the outcome the
/// same way `evaluate_rust` does (negative phase / async `$DONE`).
fn evaluate_rust_module(
    prelude: String,
    entry_source: String,
    entry_path: std::path::PathBuf,
    is_async: bool,
    negative: Option<(String, String)>,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Outcome {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut engine = chidori_js::Engine::new();
        engine.vm.op_budget = Some(50_000_000);
        engine.vm.interrupt = Some(interrupt);
        {
            use chidori_js::value::{Internal, Value as JsValue};
            let detach = engine.vm.new_native("detachArrayBuffer", 1, |_vm, _this, args| {
                if let Some(JsValue::Object(o)) = args.first() {
                    let is_ab = matches!(o.borrow().internal, Internal::ArrayBuffer(_));
                    if is_ab {
                        o.borrow_mut().internal = Internal::ArrayBuffer(None);
                    }
                }
                Ok(JsValue::Undefined)
            });
            let g = engine.vm.realm.global.clone();
            engine
                .vm
                .define_value(&g, "__t262_detachArrayBuffer", JsValue::Object(detach));
        }
        // Harness globals first (sloppy script), then the module graph.
        let result = match engine.eval(&prelude) {
            Ok(_) => run_module_graph_test(&mut engine, &entry_source, &entry_path),
            Err(e) => Err(format!("harness eval: {e}")),
        };
        (engine, result)
    }));
    let (mut engine, result) = match caught {
        Ok(pair) => pair,
        Err(_) => return Outcome::Fail("engine panicked".into()),
    };

    let outcome = if let Some((phase, neg_type)) = negative.as_ref() {
        match result {
            Ok(_) => Outcome::Fail(format!(
                "expected {} ({}) but no error was thrown",
                neg_type, phase
            )),
            Err(msg) => {
                let got = msg.split(':').next().unwrap_or("").trim();
                if got == neg_type {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!("expected {} but got {}", neg_type, msg))
                }
            }
        }
    } else {
        match result {
            Err(msg) => Outcome::Fail(msg),
            Ok(_) => {
                if is_async {
                    let prints = read_rust_print(&mut engine);
                    if prints.iter().any(|l| l.contains("Test262:AsyncTestComplete")) {
                        Outcome::Pass
                    } else if let Some(f) =
                        prints.iter().find(|l| l.contains("Test262:AsyncTestFailure"))
                    {
                        Outcome::Fail(f.clone())
                    } else {
                        Outcome::Fail("async test never signalled $DONE".into())
                    }
                } else {
                    Outcome::Pass
                }
            }
        }
    };
    engine.vm.dispose();
    outcome
}

/// Build the module registry from `entry_path` (using `entry_source` for the
/// entry, reading the rest from disk), then evaluate it. The returned `Err`
/// string is prefixed with the error constructor name so the negative-test
/// classifier in `evaluate_rust_module` can match it.
fn run_module_graph_test(
    engine: &mut chidori_js::Engine,
    entry_source: &str,
    entry_path: &Path,
) -> Result<(), String> {
    use chidori_js::module::ModuleRegistry;
    let mut registry = ModuleRegistry::default();
    let entry_key = module_key(entry_path);
    load_module_into(&mut registry, &entry_key, entry_path, Some(entry_source))?;
    match engine.vm.run_module_graph(&registry, &entry_key) {
        Ok(_) => {
            let _ = engine.vm.run_jobs_until_blocked();
            Ok(())
        }
        Err(e) => Err(engine.vm.error_to_string(&e)),
    }
}

/// Canonical registry key for a module path (falls back to the lexical path).
fn module_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Recursively compile a module and its dependencies into `registry`.
fn load_module_into(
    registry: &mut chidori_js::module::ModuleRegistry,
    key: &str,
    path: &Path,
    source_override: Option<&str>,
) -> Result<(), String> {
    use std::cell::RefCell;
    use std::rc::Rc;
    if registry.modules.contains_key(key) {
        return Ok(());
    }
    let src = match source_override {
        Some(s) => s.to_string(),
        None => std::fs::read_to_string(path)
            .map_err(|e| format!("SyntaxError: cannot read module {}: {e}", path.display()))?,
    };
    // compile_module already returns "SyntaxError: …" on parse/early errors.
    let compiled = chidori_js::compiler::compile_module(&src)?;
    let requested = compiled.requested.clone();
    let rec = Rc::new(RefCell::new(chidori_js::module::ModuleRecord::new(compiled)));
    registry.modules.insert(key.to_string(), rec.clone());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    for req in &requested {
        let dep_path = dir.join(req);
        let dep_key = module_key(&dep_path);
        rec.borrow_mut()
            .resolved
            .insert(req.clone(), dep_key.clone());
        load_module_into(registry, &dep_key, &dep_path, None)?;
    }
    Ok(())
}

/// Per-test wall-clock budget for the Rust engine (override with
/// `TEST262_TIMEOUT_MS`; default 10s). A test exceeding this is recorded as a
/// timeout failure rather than blocking the suite.
fn rust_test_timeout() -> std::time::Duration {
    let ms = std::env::var("TEST262_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10_000);
    std::time::Duration::from_millis(ms)
}

/// Evaluate an assembled Rust-engine program and decide its outcome. Runs on a
/// worker thread; wraps the engine in `catch_unwind` so a panic becomes a
/// recorded failure instead of aborting the process.
fn evaluate_rust(
    program: String,
    is_async: bool,
    negative: Option<(String, String)>,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Outcome {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut engine = chidori_js::Engine::new();
        engine.vm.op_budget = Some(50_000_000);
        engine.vm.interrupt = Some(interrupt);
        // Install a real `$262.detachArrayBuffer`: detaching sets the buffer's
        // backing store to `None`, which the TypedArray/DataView code already
        // treats as detached. Lets the ~330 `$DETACHBUFFER` tests exercise real
        // detached-buffer behavior instead of failing at the harness gate.
        {
            use chidori_js::value::{Internal, Value as JsValue};
            let detach = engine.vm.new_native("detachArrayBuffer", 1, |_vm, _this, args| {
                if let Some(JsValue::Object(o)) = args.first() {
                    let is_ab = matches!(o.borrow().internal, Internal::ArrayBuffer(_));
                    if is_ab {
                        o.borrow_mut().internal = Internal::ArrayBuffer(None);
                    }
                }
                Ok(JsValue::Undefined)
            });
            let g = engine.vm.realm.global.clone();
            engine
                .vm
                .define_value(&g, "__t262_detachArrayBuffer", JsValue::Object(detach));
        }
        let result = engine.eval(&program);
        (engine, result)
    }));
    let (mut engine, result) = match caught {
        Ok(pair) => pair,
        Err(_) => return Outcome::Fail("engine panicked".into()),
    };

    let outcome = if let Some((phase, neg_type)) = negative.as_ref() {
        // Negative test: expect a throw whose constructor name matches.
        match result {
            Ok(_) => Outcome::Fail(format!(
                "expected {} ({}) but no error was thrown",
                neg_type, phase
            )),
            Err(msg) => {
                let got = msg.split(':').next().unwrap_or("").trim();
                if got == neg_type {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!("expected {} but got {}", neg_type, msg))
                }
            }
        }
    } else {
        match result {
            Err(msg) => Outcome::Fail(msg),
            Ok(_) => {
                if is_async {
                    // Inspect the captured print buffer for the async sentinel.
                    let prints = read_rust_print(&mut engine);
                    if prints.iter().any(|l| l.contains("Test262:AsyncTestComplete")) {
                        Outcome::Pass
                    } else if let Some(f) =
                        prints.iter().find(|l| l.contains("Test262:AsyncTestFailure"))
                    {
                        Outcome::Fail(f.clone())
                    } else {
                        Outcome::Fail("async test never signalled $DONE".into())
                    }
                } else {
                    Outcome::Pass
                }
            }
        }
    };

    // Break the realm's Rc cycles before dropping the engine, so a long run does
    // not leak ~0.4 MB per test (the reference-counting GC cannot free cycles).
    engine.vm.dispose();
    outcome
}

/// Read `globalThis.__t262_print` (the captured `print` buffer) from the Rust
/// engine as a list of strings.
fn read_rust_print(engine: &mut chidori_js::Engine) -> Vec<String> {
    use chidori_js::value::PropertyKey;
    let global = engine.vm.realm.global.clone();
    let arr = match engine
        .vm
        .get_prop(&chidori_js::Value::Object(global), &PropertyKey::str("__t262_print"))
    {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let json = engine.vm.value_to_json(&arr);
    json.as_array()
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn match_negative(neg: &Negative, thrown: &JsThrow) -> Outcome {
    if thrown.name == neg.type_ {
        Outcome::Pass
    } else {
        Outcome::Fail(format!(
            "expected {} but got {} ({})",
            neg.type_, thrown.name, thrown.to_string
        ))
    }
}

/// After draining jobs, an async test must have called `$DONE()` with no error,
/// which `doneprintHandle.js` turns into a `Test262:AsyncTestComplete` print.
fn check_async_done(ctx: &mut chidori_quickjs::SnapshotContext<'_>) -> Outcome {
    let lines = ctx
        .read_global_json("__t262_print")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let joined: Vec<String> = lines
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    if joined.iter().any(|l| l.contains("Test262:AsyncTestComplete")) {
        Outcome::Pass
    } else if let Some(fail) = joined.iter().find(|l| l.contains("Test262:AsyncTestFailure")) {
        Outcome::Fail(fail.clone())
    } else {
        Outcome::Fail("async test never signalled $DONE".into())
    }
}

/// Caches harness include files (`assert.js`, `sta.js`, ...) read from disk.
struct HarnessCache {
    dir: PathBuf,
    files: HashMap<String, String>,
}

impl HarnessCache {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            files: HashMap::new(),
        }
    }

    fn load(&mut self, name: &str) -> Result<String, String> {
        if let Some(body) = self.files.get(name) {
            return Ok(body.clone());
        }
        let body = fs::read_to_string(self.dir.join(name)).map_err(|e| e.to_string())?;
        self.files.insert(name.to_string(), body.clone());
        Ok(body)
    }
}

/// Extract and parse the `/*--- ... ---*/` YAML metadata block. A file without
/// one is treated as having empty (default) metadata.
fn parse_meta(source: &str) -> Result<Meta, String> {
    let Some(start) = source.find("/*---") else {
        return Ok(Meta::default());
    };
    let after = &source[start + 5..];
    let Some(end) = after.find("---*/") else {
        return Err("unterminated metadata block".into());
    };
    let yaml = &after[..end];
    serde_yaml::from_str(yaml).map_err(|e| e.to_string())
}

/// Recursively collect runnable `.js` test files, skipping fixtures.
fn collect_tests(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if is_test_file(path) {
            out.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_tests(&p, out);
        } else if is_test_file(&p) {
            out.push(p);
        }
    }
}

fn is_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".js") && !name.ends_with("_FIXTURE.js")
}

fn resolve_path(root: &Path, p: &str) -> PathBuf {
    let candidate = Path::new(p);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

/// Load the persistent per-test result store (`rel_path -> "pass"|"fail"|"skip"`).
/// A missing or unparseable file starts empty (the run then populates it).
fn load_state(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return map;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("warning: state file {} is not valid JSON; starting fresh", path.display());
        return map;
    };
    if let Some(results) = doc.get("results").and_then(|r| r.as_object()) {
        for (k, v) in results {
            if let Some(s) = v.as_str() {
                map.insert(k.clone(), s.to_string());
            }
        }
    }
    map
}

/// Write the merged state store (with a human-readable summary header).
fn save_state(path: &Path, state: &std::collections::BTreeMap<String, String>, pass_pct: f64) {
    let (pass, fail, skip) =
        state.values().fold((0u64, 0u64, 0u64), |(p, f, k), v| match v.as_str() {
            "pass" => (p + 1, f, k),
            "fail" => (p, f + 1, k),
            _ => (p, f, k + 1),
        });
    let results: serde_json::Map<String, serde_json::Value> = state
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let doc = serde_json::json!({
        "summary": {"pass": pass, "fail": fail, "skip": skip, "pass_pct": pass_pct},
        "results": results,
    });
    if let Err(e) = fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
        eprintln!("warning: could not write state to {}: {e}", path.display());
    }
}

fn parse_args() -> Result<Args, String> {
    let mut root: Option<PathBuf> = None;
    let mut paths = Vec::new();
    let mut filter = None;
    let mut max = None;
    let mut json = None;
    let mut state = None;
    let mut verbose = false;
    let mut modules = true;
    let mut intl = false;
    let mut engine = EngineKind::QuickJs;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--test262" => {
                root = Some(PathBuf::from(it.next().ok_or("--test262 needs a value")?));
            }
            "--engine" => {
                engine = match it.next().ok_or("--engine needs a value")?.as_str() {
                    "quickjs" => EngineKind::QuickJs,
                    "rust" => EngineKind::Rust,
                    other => return Err(format!("unknown engine '{other}' (quickjs|rust)")),
                };
            }
            "--filter" => filter = Some(it.next().ok_or("--filter needs a value")?),
            "--max" => {
                max = Some(
                    it.next()
                        .ok_or("--max needs a value")?
                        .parse::<u64>()
                        .map_err(|_| "--max must be a number")?,
                );
            }
            "--json" => json = Some(PathBuf::from(it.next().ok_or("--json needs a value")?)),
            "--state" => state = Some(PathBuf::from(it.next().ok_or("--state needs a value")?)),
            "--verbose" | "-v" => verbose = true,
            "--modules" => modules = true,
            "--no-modules" => modules = false,
            "--intl" => intl = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag {other}"));
            }
            other => paths.push(other.to_string()),
        }
    }

    let root = root
        .or_else(|| std::env::var_os("TEST262_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("vendor/test262"));

    Ok(Args {
        root,
        paths,
        filter,
        max,
        json,
        verbose,
        modules,
        intl,
        engine,
        state,
    })
}
