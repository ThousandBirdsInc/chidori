//! Test262 conformance runner for chidori's pure-Rust JavaScript engine.
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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

const USAGE: &str = "\
test262-runner — run Test262 against chidori's pure-Rust JS engine

usage:
  test262-runner [--test262 <dir>] [--filter <substr>] [--max <n>]
                 [--json <out>] [--state <file>] [--baseline <file>]
                 [--verbose] [--no-modules] [--intl] [paths...]

options:
  --test262 <dir>   Test262 root (else $TEST262_DIR, else vendor/test262)
  paths...          files/dirs under the root (default: test/language test/built-ins)
  --filter <substr> only run paths containing the substring
  --max <n>         stop after n test files (smoke runs)
  --json <out>      write a per-file JSON report
  --state <file>    persist per-test results; a run updates only the tests it
                    executes, then prints the whole-suite total from the store
                    (so targeted re-runs refresh global stats without a full run)
  --baseline <file> gate against committed expectations: exit non-zero only on
                    a regression (a baseline `pass` that now fails), not merely
                    because some tests fail. Used by CI as a conformance gate.
  --verbose, -v     print each failure with the thrown message
  --no-modules      skip module-flag tests (they run by default)
  --intl            also run intl402 tests
  --help, -h        show this help";

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
    // SharedArrayBuffer + Atomics are implemented (single-agent semantics: every
    // op is a sequential read/RMW, `wait` reports the agent cannot block). Only
    // `Atomics.waitAsync` — which needs the job queue to resolve a wait — and the
    // genuinely-concurrent agent tests (skipped via the CanBlock flags) are out.
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
    // Legacy (normative-optional) Function.prototype.caller reflection on
    // non-strict functions: the engine implements the standard poisoned
    // accessor instead, so the legacy-behavior tests don't apply.
    "caller",
    // Host capabilities this runner does not provide. `cross-realm` needs
    // `$262.createRealm` (a second realm with cross-realm marshaling), which the
    // bare context cannot host; `ShadowRealm` is unimplemented in QuickJS. Bun
    // and Node likewise skip what their host/engine lacks.
    "cross-realm",
    "ShadowRealm",
    // Stage-2/3 proposals not implemented by this QuickJS build. Verified absent
    // (no implemented-surface passes to hide), so counting them as failures
    // would understate conformance of what IS implemented.
    "joint-iteration",        // Iterator.zip / Iterator.zipKeyed
    "iterator-sequencing",    // Iterator.concat
    "import-defer",           // import defer
    "upsert",                 // Map/WeakMap.prototype.getOrInsert
    "immutable-arraybuffer",  // ArrayBuffer immutable / transfer-to-immutable
    "error-stack-accessor",   // Error.prototype.stack accessor semantics
    "await-dictionary",       // Promise.{all,allSettled,...}Keyed
    "json-parse-with-source", // JSON.parse source / rawJSON
];

#[derive(Default)]
struct Tally {
    pass: u64,
    fail: u64,
    skip: u64,
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
    /// Persistent per-test result store. When set, this run UPDATES only the
    /// entries for the tests it executes, then recomputes and prints the
    /// whole-suite total from the merged store — so a targeted re-run (e.g. one
    /// directory) refreshes the global stats without re-running everything.
    state: Option<PathBuf>,
    /// Committed expectations to gate against. When set, the process exits
    /// non-zero only on a REGRESSION (a test the baseline records as `pass`
    /// that now fails or is gone), not merely because some tests fail. This is
    /// what makes the runner usable as a CI conformance gate.
    baseline: Option<PathBuf>,
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

    // Resolve the set of test files to run.
    let mut files = Vec::new();
    let roots: Vec<PathBuf> = if args.paths.is_empty() {
        vec![
            args.root.join("test/language"),
            args.root.join("test/built-ins"),
        ]
    } else {
        args.paths
            .iter()
            .map(|p| resolve_path(&args.root, p))
            .collect()
    };
    for r in &roots {
        collect_tests(r, &mut files);
    }
    files.sort();

    // Apply `--filter` and `--max` up front so the work list is exactly the set
    // that runs. Doing it here (rather than inside the loop) lets the file loop
    // fan out across cores against a fixed list with a deterministic merge.
    if let Some(filter) = &args.filter {
        files.retain(|f| {
            f.strip_prefix(&args.root)
                .unwrap_or(f)
                .to_string_lossy()
                .contains(filter.as_str())
        });
    }
    if let Some(max) = args.max {
        files.truncate(max as usize);
    }

    // Fan out across cores. A fixed pool of worker threads pulls file indices off
    // a shared atomic cursor — dynamic load-balancing, since second-level dirs
    // vary by orders of magnitude in cost — each worker with its own harness
    // cache. Per-test timeout/panic isolation is unchanged: every execution still
    // runs on its own worker thread inside `run_test`, confining the (non-`Send`,
    // `Rc`-based) engine to that thread. Outcomes are merged back in path order so
    // the printed report, `--state`, and `--baseline` gate stay deterministic
    // regardless of how the work was scheduled.
    let jobs = job_count();
    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let files_ref = &files;
    let args_ref = &args;
    let mut indexed: Vec<(usize, FileOutcome)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..jobs)
            .map(|_| {
                let cursor = &cursor;
                let hdir = harness_dir.clone();
                scope.spawn(move || {
                    let mut cache = HarnessCache::new(hdir);
                    let mut local: Vec<(usize, FileOutcome)> = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= files_ref.len() {
                            break;
                        }
                        local.push((i, run_file(&files_ref[i], args_ref, &mut cache)));
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    indexed.sort_by_key(|(i, _)| *i);

    let mut tally = Tally::default();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut report = Vec::new();

    // Persistent state: the merged per-test result store (loaded if it exists).
    // This run overwrites only the entries for the tests it executes.
    let mut state: Option<std::collections::BTreeMap<String, String>> =
        args.state.as_ref().map(|p| load_state(p));

    // This run's per-test results, always recorded so `--baseline` can diff
    // against committed expectations regardless of `--state`.
    let mut current: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    // Merge the fanned-out results sequentially, in path order, so tallies and
    // the persisted store are byte-for-byte stable across runs.
    for (_, fo) in &indexed {
        match fo.status {
            "fail" => {
                tally.fail += 1;
                failures.push((fo.rel.clone(), fo.failure.clone().unwrap_or_default()));
            }
            "skip" => tally.skip += 1,
            _ => tally.pass += 1,
        }

        if let Some(state) = state.as_mut() {
            state.insert(fo.rel.clone(), fo.status.to_string());
        }
        current.insert(fo.rel.clone(), fo.status.to_string());

        if args.json.is_some() {
            report.push(serde_json::json!({
                "file": fo.rel,
                "status": fo.status,
                "variants": fo
                    .variants
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
        "\nTest262 (chidori pure-Rust engine, bare context)\n  pass {}  fail {}  skip {}  =>  {:.2}% of executed",
        tally.pass, tally.fail, tally.skip, pct
    );

    // Persist the merged state and report the WHOLE-SUITE total from it, so a
    // targeted re-run refreshes the global stats without re-running everything.
    if let (Some(path), Some(state)) = (&args.state, &state) {
        let (sp, sf, sk) =
            state
                .values()
                .fold((0u64, 0u64, 0u64), |(p, f, k), v| match v.as_str() {
                    "pass" => (p + 1, f, k),
                    "fail" => (p, f + 1, k),
                    _ => (p, f, k + 1),
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

    // Baseline gate: when committed expectations are supplied, the exit code
    // reflects REGRESSIONS against them rather than the raw fail count, so the
    // job is green as long as no previously-passing test broke.
    if let Some(path) = &args.baseline {
        let expected = load_state(path);
        if expected.is_empty() {
            eprintln!(
                "error: baseline {} is missing or has no results; cannot gate.\n\
                 Regenerate it with `scripts/test262.sh --update-baseline`.",
                path.display()
            );
            return ExitCode::from(2);
        }

        let mut regressions: Vec<(String, String)> = Vec::new(); // pass -> not-pass
        let mut new_failures: Vec<String> = Vec::new(); // failing, absent from baseline
        let mut progressions = 0u64; // fail -> pass (baseline can be refreshed)
        for (rel, got) in &current {
            match expected.get(rel).map(String::as_str) {
                Some("pass") if got != "pass" => regressions.push((rel.clone(), got.clone())),
                Some("fail") if got == "pass" => progressions += 1,
                None if got == "fail" => new_failures.push(rel.clone()),
                _ => {}
            }
        }

        println!(
            "\nBaseline gate ({}):\n  regressions {}  new failures {}  progressions {}",
            path.display(),
            regressions.len(),
            new_failures.len(),
            progressions
        );
        for (rel, got) in regressions.iter().take(50) {
            println!("  REGRESSED {rel}  (baseline pass -> {got})");
        }
        for rel in new_failures.iter().take(50) {
            println!("  NEW FAIL  {rel}  (not in baseline)");
        }
        if progressions > 0 {
            println!(
                "  note: {progressions} test(s) now pass that the baseline marks as failing.\n\
                 Refresh with `scripts/test262.sh --update-baseline` to lock the gains in."
            );
        }

        return if regressions.is_empty() && new_failures.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }

    if tally.fail > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// The merged result of one test file: its overall status, the first failure
/// message (if any), and the per-variant detail for `--json`. Produced on a
/// worker thread and merged back on the main thread in path order.
struct FileOutcome {
    rel: String,
    status: &'static str,
    failure: Option<String>,
    variants: Vec<(&'static str, &'static str, String)>,
}

/// Number of parallel workers: `TEST262_JOBS` if set (and > 0), else the machine
/// parallelism, else 1. One worker streams files off the shared cursor.
fn job_count() -> usize {
    std::env::var("TEST262_JOBS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
}

/// Read, run, and classify a single test file (the per-worker unit of work).
fn run_file(file: &Path, args: &Args, cache: &mut HarnessCache) -> FileOutcome {
    let rel = file
        .strip_prefix(&args.root)
        .unwrap_or(file)
        .to_string_lossy()
        .to_string();

    let source = match fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            return FileOutcome {
                rel,
                status: "fail",
                failure: Some(format!("read error: {e}")),
                variants: Vec::new(),
            }
        }
    };

    let test_dir = file.parent().unwrap_or(Path::new("."));
    if std::env::var("T262_TRACE").is_ok() {
        eprintln!("RUNNING {}", rel);
    }
    let outcomes = run_test(&rel, &source, test_dir, cache, args);
    classify(rel, &outcomes)
}

/// Collapse a file's per-variant outcomes into one status: a file passes only if
/// every executed variant passes; an all-skip file skips; any failing variant
/// fails (recording the first failure message).
fn classify(rel: String, outcomes: &[(Variant, Outcome)]) -> FileOutcome {
    let mut variants = Vec::new();
    let mut any_fail = None;
    let mut all_skip = true;
    for (variant, outcome) in outcomes {
        match outcome {
            Outcome::Pass => {
                all_skip = false;
                variants.push((variant.label(), "pass", String::new()));
            }
            Outcome::Skip(why) => {
                variants.push((variant.label(), "skip", why.clone()));
            }
            Outcome::Fail(why) => {
                all_skip = false;
                if any_fail.is_none() {
                    any_fail = Some(format!("[{}] {}", variant.label(), why));
                }
                variants.push((variant.label(), "fail", why.clone()));
            }
        }
    }
    let (status, failure) = if let Some(why) = any_fail {
        ("fail", Some(why))
    } else if all_skip {
        ("skip", None)
    } else {
        ("pass", None)
    };
    FileOutcome {
        rel,
        status,
        failure,
        variants,
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
    // Multi-agent Atomics tests coordinate worker agents through `$262.agent`
    // (the `atomicsHelper.js` harness). The single-threaded, non-`Send` engine
    // cannot host a second agent, so — like the CanBlock agent tests above —
    // these are honest skips rather than failures. The single-agent Atomics
    // surface (load/store/RMW/wait-cannot-block/notify-zero) is still exercised
    // by the many non-agent tests.
    if meta.includes.iter().any(|i| i == "atomicsHelper.js") {
        return vec![(Variant::Sloppy, Outcome::Skip("agent".into()))];
    }

    let variants = select_variants(&meta, args);
    variants
        .into_iter()
        .map(|v| {
            let outcome = run_variant_rust(source, &meta, v, test_dir, rel, harness);
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
  evalScript: globalThis.__t262_evalScript,
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
    let test_dir_w = test_dir.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
    // 256 MB is a ~10× margin over the realistic worst case (the VM caps JS
    // recursion at 2000 frames and regex at 100k steps); 1 GB per worker, spawned
    // once per test, needlessly inflated peak memory.
    let spawned = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let outcome = evaluate_rust(program, is_async_w, negative_w, interrupt_w, test_dir_w);
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
            let detach = engine
                .vm
                .new_native("detachArrayBuffer", 1, |_vm, _this, args| {
                    if let Some(JsValue::Object(o)) = args.first() {
                        let is_ab = matches!(o.borrow().internal, Internal::ArrayBuffer(_));
                        if is_ab {
                            o.borrow_mut().internal = Internal::ArrayBuffer(None);
                        }
                    }
                    Ok(JsValue::Undefined)
                });
            let eval_script = engine.vm.new_native("evalScript", 1, |vm, _this, args| {
                let src = match args.first() {
                    Some(JsValue::String(s)) => s.as_str().to_string(),
                    other => vm
                        .to_js_string(other.unwrap_or(&JsValue::Undefined))?
                        .as_str()
                        .to_string(),
                };
                vm.eval_script(&src)
            });
            let g = engine.vm.realm.global.clone();
            engine
                .vm
                .define_value(&g, "__t262_detachArrayBuffer", JsValue::Object(detach));
            engine
                .vm
                .define_value(&g, "__t262_evalScript", JsValue::Object(eval_script));
        }
        // Dynamic `import()` resolves against the entry's directory and shares
        // the registry with the static graph (same module record → same
        // namespace identity for a specifier reached both ways).
        let registry = std::rc::Rc::new(std::cell::RefCell::new(
            chidori_js::module::ModuleRegistry::default(),
        ));
        let entry_dir = entry_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        install_dynamic_import(&mut engine, entry_dir, registry.clone());
        // Harness globals first (sloppy script), then the module graph.
        let result = match engine.eval(&prelude) {
            Ok(_) => run_module_graph_test(&mut engine, &entry_source, &entry_path, &registry),
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
                    if prints
                        .iter()
                        .any(|l| l.contains("Test262:AsyncTestComplete"))
                    {
                        Outcome::Pass
                    } else if let Some(f) = prints
                        .iter()
                        .find(|l| l.contains("Test262:AsyncTestFailure"))
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
    registry: &std::rc::Rc<std::cell::RefCell<chidori_js::module::ModuleRegistry>>,
) -> Result<(), String> {
    let entry_key = module_key(entry_path);
    load_module_into(
        &mut registry.borrow_mut(),
        &entry_key,
        entry_path,
        Some(entry_source),
    )?;
    // Evaluate against a shallow snapshot (shared `Rc` records) so no borrow
    // is held while the graph runs: a top-level-await body can trigger a
    // dynamic `import()` job mid-evaluation, whose hook must re-borrow the
    // live registry to load new modules.
    let reg = registry.borrow().clone();
    match engine.vm.run_module_graph(&reg, &entry_key) {
        Ok(_) => {
            let _ = engine.vm.run_jobs_until_blocked();
            Ok(())
        }
        Err(e) => Err(engine.vm.error_to_string(&e)),
    }
}

/// Install the dynamic-`import()` host hook: resolve the specifier against
/// `base_dir`, load the module graph into the shared `registry`, evaluate it,
/// and return its namespace object. Load/parse problems become thrown error
/// values (TypeError / SyntaxError), which reject the `import()` promise.
fn install_dynamic_import(
    engine: &mut chidori_js::Engine,
    base_dir: std::path::PathBuf,
    registry: std::rc::Rc<std::cell::RefCell<chidori_js::module::ModuleRegistry>>,
) {
    engine.vm.dynamic_import = Some(std::rc::Rc::new(move |vm, spec| {
        let path = base_dir.join(spec);
        let key = module_key(&path);
        if let Err(msg) = load_module_into(&mut registry.borrow_mut(), &key, &path, None) {
            return Err(if let Some(m) = msg.strip_prefix("SyntaxError: ") {
                vm.throw_syntax(m)
            } else {
                vm.throw_type(&msg)
            });
        }
        // Shallow snapshot — see run_module_graph_test: evaluation must not
        // hold a borrow open (a nested dynamic import re-borrows to load).
        let reg = registry.borrow().clone();
        vm.run_module_graph(&reg, &key)?;
        vm.module_namespace_by_key(&reg, &key)
    }));
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
    let rec = Rc::new(RefCell::new(chidori_js::module::ModuleRecord::new(
        compiled,
    )));
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
/// `TEST262_TIMEOUT_MS`; default 5s). A test exceeding this is recorded as a
/// timeout failure rather than blocking the suite. A conformant engine runs each
/// Test262 file in well under a second, so the budget only catches pathological
/// cases (catastrophic regex, near-op-budget loops); the gate scripts pin this
/// explicitly so the committed baseline is reproducible regardless of the
/// default here.
fn rust_test_timeout() -> std::time::Duration {
    let ms = std::env::var("TEST262_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5_000);
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
    test_dir: std::path::PathBuf,
) -> Outcome {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut engine = chidori_js::Engine::new();
        engine.vm.op_budget = Some(50_000_000);
        engine.vm.interrupt = Some(interrupt);
        let registry = std::rc::Rc::new(std::cell::RefCell::new(
            chidori_js::module::ModuleRegistry::default(),
        ));
        install_dynamic_import(&mut engine, test_dir, registry);
        // Install a real `$262.detachArrayBuffer`: detaching sets the buffer's
        // backing store to `None`, which the TypedArray/DataView code already
        // treats as detached. Lets the ~330 `$DETACHBUFFER` tests exercise real
        // detached-buffer behavior instead of failing at the harness gate.
        {
            use chidori_js::value::{Internal, Value as JsValue};
            let detach = engine
                .vm
                .new_native("detachArrayBuffer", 1, |_vm, _this, args| {
                    if let Some(JsValue::Object(o)) = args.first() {
                        let is_ab = matches!(o.borrow().internal, Internal::ArrayBuffer(_));
                        if is_ab {
                            o.borrow_mut().internal = Internal::ArrayBuffer(None);
                        }
                    }
                    Ok(JsValue::Undefined)
                });
            let eval_script = engine.vm.new_native("evalScript", 1, |vm, _this, args| {
                let src = match args.first() {
                    Some(JsValue::String(s)) => s.as_str().to_string(),
                    other => vm
                        .to_js_string(other.unwrap_or(&JsValue::Undefined))?
                        .as_str()
                        .to_string(),
                };
                vm.eval_script(&src)
            });
            let g = engine.vm.realm.global.clone();
            engine
                .vm
                .define_value(&g, "__t262_detachArrayBuffer", JsValue::Object(detach));
            engine
                .vm
                .define_value(&g, "__t262_evalScript", JsValue::Object(eval_script));
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
                    if prints
                        .iter()
                        .any(|l| l.contains("Test262:AsyncTestComplete"))
                    {
                        Outcome::Pass
                    } else if let Some(f) = prints
                        .iter()
                        .find(|l| l.contains("Test262:AsyncTestFailure"))
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
    let arr = match engine.vm.get_prop(
        &chidori_js::Value::Object(global),
        &PropertyKey::str("__t262_print"),
    ) {
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
        eprintln!(
            "warning: state file {} is not valid JSON; starting fresh",
            path.display()
        );
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
        state
            .values()
            .fold((0u64, 0u64, 0u64), |(p, f, k), v| match v.as_str() {
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
    let mut baseline = None;
    let mut verbose = false;
    let mut modules = true;
    let mut intl = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--test262" => {
                root = Some(PathBuf::from(it.next().ok_or("--test262 needs a value")?));
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
            // Accepted for backward compatibility; the pure-Rust engine is the
            // only engine, so the value is ignored.
            "--engine" => {
                let _ = it.next().ok_or("--engine needs a value")?;
            }
            "--state" => state = Some(PathBuf::from(it.next().ok_or("--state needs a value")?)),
            "--baseline" => {
                baseline = Some(PathBuf::from(it.next().ok_or("--baseline needs a value")?))
            }
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
        state,
        baseline,
    })
}
