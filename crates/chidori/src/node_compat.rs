//! Node.js core test suite compatibility harness.
//!
//! The industry yardstick for "how Node-compatible is this runtime" is
//! Node's own test suite — Bun runs it wholesale and Deno vendors a curated
//! subset under `tests/node_compat` with an expectations file. This module is
//! chidori's equivalent, sibling to the `test262-runner` crate (which plays
//! the same role for pure language conformance): a curated subset of
//! `nodejs/node` `test/parallel` vendored under `tests/node_compat/suite`
//! (see `scripts/vendor-node-compat-tests.sh`; the pinned release is recorded
//! in `tests/node_compat/NODE_VERSION`), executed against the real chidori
//! engine + builtin shims, and gated by `tests/node_compat/expectations.json`.
//!
//! Node core tests are CommonJS scripts that `require()` builtins and the
//! suite's `common` helper module, assert as they run, and rely on
//! exit-time verification of `common.mustCall` counters. The harness bridges
//! each of those to chidori's ESM-only world:
//!
//! - `require()` is provided as a plain in-scope function backed by a
//!   registry of pre-imported builtin shims (only the builtins the test
//!   actually names are imported, so one broken shim can't fail the suite
//!   wholesale). `require('../common')` returns a minimal reimplementation
//!   of the helpers the vendored tests use — the same approach Deno takes.
//! - The test body is spliced into an async IIFE inside an `agent()` export
//!   (CommonJS allows top-level `return`; a function body allows it too),
//!   the virtual timer queue is drained afterwards, and `mustCall` counters
//!   are verified at the end, standing in for Node's exit-time check.
//! - `common.skip()` throws a marker the runner maps to a `skip` outcome —
//!   used for host facilities the harness cannot emulate (test fixtures).
//!   Everything else is honest: a test that needs surface chidori doesn't
//!   provide records as `fail`, and the report says so.
//!
//! The expectations gate makes the suite CI-useful today (a shim regression
//! flips a `pass` to `fail` and the test names it) while the report
//! (`docs/node-compat-report.md`, regenerated with `NODE_COMPAT_UPDATE=1`)
//! tracks progress toward more of the suite passing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::runtime::typescript::transpile::NODE_BUILTIN_ALLOWLIST;

/// One vendored test's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub file: String,
    pub status: Status,
    /// First line of the failure/skip reason, for the report.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

/// Minimal reimplementation of Node's `test/common` helpers, mirroring the
/// subset the vendored tests touch (the strategy Deno's node_compat suite
/// uses). `mustCall` counters are tracked here and checked by `__verify()`
/// after the test body and timer drain complete.
const COMMON_SHIM_JS: &str = r#"
const __mustCalls = [];
function __registerMustCall(fn, expected, kind, name) {
    const entry = { expected, actual: 0, kind, name: name || (fn && fn.name) || "<anonymous>" };
    __mustCalls.push(entry);
    const inner = typeof fn === "function" ? fn : function () {};
    return function (...args) {
        entry.actual += 1;
        return inner.apply(this, args);
    };
}
const __common = {
    mustCall(fn, expected) {
        if (typeof fn === "number") { expected = fn; fn = undefined; }
        return __registerMustCall(fn, expected === undefined ? 1 : expected, "exact", "mustCall");
    },
    mustCallAtLeast(fn, minimum) {
        if (typeof fn === "number") { minimum = fn; fn = undefined; }
        return __registerMustCall(fn, minimum === undefined ? 1 : minimum, "atLeast", "mustCallAtLeast");
    },
    mustNotCall(message) {
        // Named, exactly as Node's `test/common` names it: assertion messages
        // that embed the expected function's `name` are compared verbatim.
        return function mustNotCall() {
            throw new Error("mustNotCall violated: " + (message || "function should not have been called"));
        };
    },
    mustSucceed(fn) {
        return __common.mustCall(function (err, ...args) {
            if (err) throw err;
            if (typeof fn === "function") return fn(...args);
        });
    },
    mustNotMutateObjectDeep(value) { return value; },
    // Mirrors Node's helper: every typed-array/DataView view over the same
    // bytes, skipping constructors the engine lacks or lengths that don't
    // divide evenly.
    getArrayBufferViews(buf) {
        const out = [];
        const ctors = [
            globalThis.Uint8Array, globalThis.Int8Array, globalThis.Uint8ClampedArray,
            globalThis.Uint16Array, globalThis.Int16Array,
            globalThis.Uint32Array, globalThis.Int32Array,
            globalThis.Float32Array, globalThis.Float64Array,
            globalThis.BigInt64Array, globalThis.BigUint64Array,
            globalThis.DataView,
        ];
        for (const ctor of ctors) {
            if (typeof ctor !== "function") continue;
            const bytesPer = ctor === globalThis.DataView ? 1 : ctor.BYTES_PER_ELEMENT;
            if (buf.byteLength % bytesPer !== 0) continue;
            if (ctor === globalThis.DataView) {
                out.push(new ctor(buf.buffer, buf.byteOffset, buf.byteLength));
            } else {
                out.push(new ctor(buf.buffer, buf.byteOffset, buf.byteLength / bytesPer));
            }
        }
        return out;
    },
    getBufferSources(buf) {
        return [...__common.getArrayBufferViews(buf), buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength)];
    },
    skip(message) { throw { __chidori_skip: String(message || "skipped") }; },
    printSkipMessage() {},
    expectsError(settings) {
        const expected = settings || {};
        // Returns true on match: Node's assert.throws treats a validator
        // function's return value as the verdict, and the real
        // common.expectsError returns true.
        return __common.mustCall(function (err) {
            if (expected.code !== undefined && err.code !== expected.code) {
                throw new Error("expectsError: code " + err.code + " !== " + expected.code);
            }
            if (expected.name !== undefined && err.name !== expected.name) {
                throw new Error("expectsError: name " + err.name + " !== " + expected.name);
            }
            if (expected.message instanceof RegExp && !expected.message.test(err.message)) {
                throw new Error("expectsError: message " + err.message + " !~ " + expected.message);
            }
            if (typeof expected.message === "string" && err.message !== expected.message) {
                throw new Error("expectsError: message " + err.message + " !== " + expected.message);
            }
            return true;
        });
    },
    // Node's catalog semantics: one process-level 'warning' listener, and a
    // per-name mustCall handler that later expectWarning calls REPLACE (a
    // consumed handler makes room for re-arming the same warning name).
    expectWarning(nameOrMap, expected, code) {
        if (__common.__catchWarning === undefined) {
            __common.__catchWarning = {};
            process.on("warning", (warning) => {
                const handler = __common.__catchWarning[warning.name];
                if (!handler) {
                    throw new TypeError('"' + warning.name + '" was triggered without being expected: ' + warning.message);
                }
                handler(warning);
            });
        }
        const make = (name, exp, expCode) => {
            const list = typeof exp === "string"
                ? [[exp, expCode]]
                : exp.map((e) => (Array.isArray(e) ? e : [e, undefined]));
            return __common.mustCall((warning) => {
                const entry = list.shift();
                if (warning.name !== name) {
                    throw new Error("expectWarning: name " + warning.name + " !== " + name);
                }
                if (entry[0] !== undefined && warning.message !== entry[0]) {
                    throw new Error("expectWarning: message " + JSON.stringify(warning.message) + " !== " + JSON.stringify(entry[0]));
                }
                if (entry[1] !== undefined && warning.code !== entry[1]) {
                    throw new Error("expectWarning: code " + warning.code + " !== " + entry[1]);
                }
            }, list.length);
        };
        if (typeof nameOrMap === "string") {
            __common.__catchWarning[nameOrMap] = make(nameOrMap, expected, code);
        } else {
            for (const name of Object.keys(nameOrMap)) {
                __common.__catchWarning[name] = make(name, nameOrMap[name], undefined);
            }
        }
    },
    allowGlobals() {},
    platformTimeout(t) { return t; },
    busyLoop() {},
    // Mirrors Node's test/common/index.js helper (which mirrors the runtime's
    // ERR_INVALID_ARG_TYPE message tail) — several vendored tests build their
    // expected error messages through it.
    invalidArgTypeHelper(input) {
        function inspectPrimitive(value) {
            if (typeof value === "string") return "'" + value + "'";
            if (typeof value === "bigint") return String(value) + "n";
            if (typeof value === "symbol") return value.toString();
            return String(value);
        }
        if (input === null || input === undefined) return " Received " + String(input);
        if (typeof input === "function") {
            return " Received function " + (input.name || "");
        }
        if (typeof input === "object") {
            if (input.constructor && input.constructor.name) {
                return " Received an instance of " + input.constructor.name;
            }
            // Node inspects null-prototype objects; the empty shape is what
            // the vendored suite constructs.
            return " Received [Object: null prototype] " + (Object.keys(input).length === 0 ? "{}" : "{ ... }");
        }
        let inspected = inspectPrimitive(input);
        if (inspected.length > 28) inspected = inspected.slice(0, 25) + "...";
        return " Received type " + typeof input + " (" + inspected + ")";
    },
    canCreateSymLink() { return false; },
    hasCrypto: true,
    hasIntl: false,
    hasIPv6: false,
    isWindows: false,
    isLinux: false,
    isOSX: false,
    isMacOS: false,
    isAIX: false,
    isIBMi: false,
    isFreeBSD: false,
    isOpenBSD: false,
    isSunOS: false,
    isDumbTerminal: true,
    isMainThread: true,
    buildType: "Release",
    localhostIPv4: "127.0.0.1",
    PIPE: "/tmp/chidori-node-compat.pipe",
    __verify() {
        for (const entry of __mustCalls) {
            const ok = entry.kind === "exact"
                ? entry.actual === entry.expected
                : entry.actual >= entry.expected;
            if (!ok) {
                throw new Error(
                    entry.name + ": expected " + (entry.kind === "exact" ? "exactly " : "at least ") +
                    entry.expected + " call(s), got " + entry.actual
                );
            }
        }
    },
};
"#;

/// Extract the string arguments of every `require('...')` in the source.
/// Hand-rolled scan (same approach as `pkg::compat`): regex-free and immune
/// to require calls appearing mid-expression.
fn required_specifiers(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = source;
    while let Some(idx) = rest.find("require(") {
        rest = &rest[idx + "require(".len()..];
        let trimmed = rest.trim_start();
        let Some(quote) = trimmed.chars().next() else { break };
        if quote != '\'' && quote != '"' {
            continue;
        }
        let body = &trimmed[1..];
        let Some(end) = body.find(quote) else { break };
        out.insert(body[..end].to_string());
        rest = &body[end..];
    }
    out
}

/// Wrap a CommonJS Node core test as a chidori agent module. `file_name` is
/// the vendored test's basename, used for the `__filename`/`__dirname` CJS
/// globals. Public for the harness tests; agent code never sees this.
pub fn wrap_node_test(source: &str, file_name: &str) -> String {
    // Builtins the test names (plus the ones Node exposes as globals that the
    // wrapper must provide: Buffer, URL/URLSearchParams).
    let mut builtins: BTreeSet<String> = BTreeSet::new();
    builtins.insert("buffer".to_string());
    builtins.insert("url".to_string());
    for spec in required_specifiers(source) {
        let name = spec.strip_prefix("node:").unwrap_or(&spec);
        if NODE_BUILTIN_ALLOWLIST.contains(&name) {
            builtins.insert(name.to_string());
        }
    }

    let mut js = String::new();
    let mut registry = String::new();
    for (i, name) in builtins.iter().enumerate() {
        let _ = writeln!(js, "import __m{i} from \"node:{name}\";");
        let _ = writeln!(registry, "    {}: __m{i},", serde_json::to_string(name).unwrap());
    }
    js.push_str("const __builtins = {\n");
    js.push_str(&registry);
    js.push_str("};\n");
    js.push_str(COMMON_SHIM_JS);
    js.push_str(
        r#"
function require(name) {
    let key = String(name);
    if (key.startsWith("node:")) key = key.slice(5);
    if (key === "../common" || key.endsWith("/common") || key === "../common/index.js") {
        return __common;
    }
    if (key.indexOf("common/") !== -1) {
        // fixtures, tmpdir, … — host suite facilities the harness cannot
        // emulate; the runner records the test as skipped.
        throw { __chidori_skip: "requires Node test-suite facility '" + name + "'" };
    }
    // Node internals the vendored tests reach for under --expose-internals.
    // Only the exact members the suite dereferences, mapped onto chidori's
    // own machinery (the Deno node_compat approach).
    if (key === "internal/event_target") {
        return { kEvents: globalThis.__chidori_kEvents || Symbol.for("chidori.kEvents") };
    }
    if (key === "internal/util") {
        return { customPromisifyArgs: Symbol.for("nodejs.util.promisify.customArgs") };
    }
    if (key === "internal/test/binding") {
        return {
            internalBinding(binding) {
                if (binding === "js_stream") {
                    // The suite only uses JSStream to obtain an "external"
                    // (host-opaque) value; chidori has no V8 externals, so a
                    // branded placeholder stands in.
                    return {
                        JSStream: function JSStream() {
                            this._externalStream = { __chidoriExternal: true };
                        },
                    };
                }
                throw new Error("node-compat: internalBinding('" + binding + "') is not provided");
            },
        };
    }
    if (Object.prototype.hasOwnProperty.call(__builtins, key)) {
        // Node's CJS loader emits DEP0040 when *userland* code requires
        // punycode (internal users go through internal/idna and stay silent).
        // The ESM shims pre-evaluate before the test body runs, so the
        // require() emulation is the correct point to reproduce that.
        if (key === "punycode" && !require.__punycodeWarned) {
            require.__punycodeWarned = true;
            process.emitWarning(
                "The `punycode` module is deprecated. Please use a userland alternative instead.",
                "DeprecationWarning", "DEP0040");
        }
        return __builtins[key];
    }
    throw new Error("node-compat: cannot require('" + name + "'): module not provided by the chidori runtime");
}
export async function agent() {
    const global = globalThis;
    const Buffer = __builtins["buffer"].Buffer;
    const URL = __builtins["url"].URL;
    const URLSearchParams = __builtins["url"].URLSearchParams;
    const module = { exports: {} };
    const exports = module.exports;
"#,
    );
    let _ = writeln!(
        js,
        "    const __filename = {};\n    const __dirname = \"/test/parallel\";",
        serde_json::to_string(&format!("/test/parallel/{file_name}")).unwrap()
    );
    js.push_str(
        r#"    try {
        await (async () => {
"#,
    );
    js.push_str(source);
    js.push_str(
        r#"
        })();
        // Drain the virtual timer queue so timer-scheduled assertions run
        // before mustCall verification (stands in for Node's exit phase).
        for (let __i = 0; __i < 64; __i++) {
            await new Promise((__resolve) => setTimeout(__resolve, 0));
        }
        // Node runs 'exit' listeners as the process leaves; tests assert final
        // state in them. The process global's emit is a real registry.
        if (globalThis.process && typeof globalThis.process.emit === "function") {
            globalThis.process.emit("exit", 0);
        }
        __common.__verify();
    } catch (err) {
        if (err && err.__chidori_skip) {
            return { status: "skip", reason: err.__chidori_skip };
        }
        throw err;
    }
    return { status: "pass" };
}
"#,
    );
    js
}

/// First line of an error chain, trimmed to keep the report readable.
fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(160).collect();
    if out.len() < line.len() {
        out.push('…');
    }
    out
}

/// Run every vendored test under `suite_dir` and return per-file outcomes.
pub fn run_suite(suite_dir: &Path) -> Vec<Outcome> {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use crate::mcp::McpManager;
    use crate::policy::{PolicyCache, PolicyConfig};
    use crate::providers::ProviderRegistry;
    use crate::runtime::context::RuntimeContext;
    use crate::runtime::snapshot::RuntimePolicy;
    use crate::runtime::template::TemplateEngine;
    use crate::runtime::typescript::bindings::HostBindingBackend;
    use crate::tools::ToolRegistry;

    // NODE_COMPAT_FILTER=substring narrows a run to matching filenames — for
    // iterating on one vendored test without paying for the whole suite. The
    // expectations gate ignores filtered runs implicitly (drift on absent
    // files would fire), so use it only with --nocapture inspection.
    let filter = std::env::var("NODE_COMPAT_FILTER").ok();
    let mut files: Vec<_> = std::fs::read_dir(suite_dir)
        .unwrap_or_else(|e| panic!("reading suite dir {}: {e}", suite_dir.display()))
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("js")).then_some(path)
        })
        .filter(|path| {
            filter.as_deref().is_none_or(|f| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(f))
            })
        })
        .collect();
    files.sort();

    let tokio_rt = Arc::new(tokio::runtime::Runtime::new().expect("tokio runtime"));
    let scratch = std::env::temp_dir().join(format!("chidori-node-compat-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");

    // Bound each test's pure-JS compute so a pathological vendored test (an
    // effectively-infinite loop under a shim divergence) terminates as a
    // deterministic `fail` instead of stalling the suite for minutes under
    // the 5e9 default. The op budget is deterministic — the same test trips
    // at the same op, so expectations stay stable — unlike a wall-clock
    // deadline. 30M ops is ~30x any legitimate vendored test. Env mutation is
    // process-global; the previous value is restored on exit (the same
    // pattern the engine's own op-budget test uses).
    let previous_budget = std::env::var("CHIDORI_JS_OP_BUDGET").ok();
    std::env::set_var("CHIDORI_JS_OP_BUDGET", "30000000");

    let mut outcomes = Vec::with_capacity(files.len());
    for path in files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let source = std::fs::read_to_string(&path).expect("reading vendored test");
        let wrapped = wrap_node_test(&source, &file);
        let agent_path = scratch.join(&file).with_extension("wrapped.ts");
        std::fs::write(&agent_path, &wrapped).expect("writing wrapper");

        // Fresh context/backend per test so state (VFS, call log, timers)
        // never leaks between tests.
        let ctx = RuntimeContext::new();
        // Seed the VFS with the test's own source at its __filename — in Node
        // the test file exists on disk, and some tests stat/read themselves.
        let _ = ctx.vfs_mkdir("/test/parallel", true);
        let _ = ctx.vfs_write(&format!("/test/parallel/{file}"), source.clone().into_bytes());
        let backend = HostBindingBackend::for_runtime(
            ctx,
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(".")),
            tokio_rt.clone(),
            PolicyConfig::from_env(),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("node-compat"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        );
        let result = crate::runtime::rust_engine::run_agent(
            &agent_path,
            &wrapped,
            &serde_json::json!({}),
            &backend,
        );
        let outcome = match result {
            Ok(value) => match value.get("status").and_then(|s| s.as_str()) {
                Some("pass") => Outcome {
                    file,
                    status: Status::Pass,
                    detail: None,
                },
                Some("skip") => Outcome {
                    file,
                    status: Status::Skip,
                    detail: value
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .map(first_line),
                },
                _ => Outcome {
                    file,
                    status: Status::Fail,
                    detail: Some("harness: unexpected agent result shape".to_string()),
                },
            },
            Err(err) => Outcome {
                file,
                status: Status::Fail,
                detail: Some(first_line(&format!("{err:#}"))),
            },
        };
        eprintln!(
            "node-compat: {} {} {}",
            outcome.status.as_str(),
            outcome.file,
            outcome.detail.as_deref().unwrap_or("")
        );
        outcomes.push(outcome);
    }
    match previous_budget {
        Some(value) => std::env::set_var("CHIDORI_JS_OP_BUDGET", value),
        None => std::env::remove_var("CHIDORI_JS_OP_BUDGET"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
    outcomes
}

/// Group a vendored test filename into the builtin module it exercises, for
/// the per-module report table. Checked in order, longest prefix first.
fn module_of(file: &str) -> &'static str {
    const GROUPS: &[(&str, &str)] = &[
        ("test-event-emitter", "events"),
        ("test-events", "events"),
        ("test-async-local-storage", "async_hooks"),
        ("test-string-decoder", "string_decoder"),
        ("test-diagnostics-channel", "diagnostics_channel"),
        ("test-querystring", "querystring"),
        ("test-punycode", "punycode"),
        ("test-path", "path"),
        ("test-url", "url"),
        ("test-buffer", "buffer"),
        ("test-zlib", "zlib"),
        ("test-util", "util"),
        ("test-assert", "assert"),
        ("test-net", "net"),
        ("test-timers", "timers"),
        ("test-stream", "stream"),
        ("test-module", "module"),
        ("test-process", "process"),
    ];
    for (prefix, module) in GROUPS {
        if file.starts_with(prefix) {
            return module;
        }
    }
    "other"
}

/// Render the progress report committed at `docs/node-compat-report.md`.
pub fn render_report(outcomes: &[Outcome], node_version: &str) -> String {
    let total = outcomes.len();
    let passed = outcomes.iter().filter(|o| o.status == Status::Pass).count();
    let failed = outcomes.iter().filter(|o| o.status == Status::Fail).count();
    let skipped = outcomes.iter().filter(|o| o.status == Status::Skip).count();

    let mut by_module: BTreeMap<&str, Vec<&Outcome>> = BTreeMap::new();
    for outcome in outcomes {
        by_module.entry(module_of(&outcome.file)).or_default().push(outcome);
    }

    let mut out = String::new();
    let _ = writeln!(out, "# Node.js compatibility report");
    out.push('\n');
    let _ = writeln!(
        out,
        "Generated by the node-compat harness (`crates/chidori/src/node_compat.rs`) \
         against a curated subset of the Node.js core test suite \
         (`nodejs/node@{node_version}` `test/parallel`, vendored by \
         `scripts/vendor-node-compat-tests.sh`). Regenerate with:"
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "```\nNODE_COMPAT_UPDATE=1 cargo test -p chidori --lib -- node_compat\n```"
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "**{passed}/{total} passing** ({failed} failing, {skipped} skipped). \
         A test passes only if every assertion in the vendored Node test holds \
         under chidori's engine and builtin shims; skips are tests needing Node \
         test-suite facilities (fixtures, tmpdir) the harness does not emulate."
    );
    out.push('\n');
    let _ = writeln!(out, "| Module | Pass | Fail | Skip | Rate |");
    let _ = writeln!(out, "|---|---|---|---|---|");
    for (module, list) in &by_module {
        let p = list.iter().filter(|o| o.status == Status::Pass).count();
        let f = list.iter().filter(|o| o.status == Status::Fail).count();
        let s = list.iter().filter(|o| o.status == Status::Skip).count();
        let judged = p + f;
        let rate = if judged == 0 {
            "—".to_string()
        } else {
            format!("{}%", (p * 100) / judged)
        };
        let _ = writeln!(out, "| {module} | {p} | {f} | {s} | {rate} |");
    }
    out.push('\n');
    let _ = writeln!(out, "## Failing tests");
    out.push('\n');
    if failed == 0 {
        let _ = writeln!(out, "None.");
    } else {
        for outcome in outcomes.iter().filter(|o| o.status == Status::Fail) {
            let _ = writeln!(
                out,
                "- `{}` — {}",
                outcome.file,
                outcome.detail.as_deref().unwrap_or("(no detail)")
            );
        }
    }
    out.push('\n');
    let _ = writeln!(out, "## Skipped tests");
    out.push('\n');
    if skipped == 0 {
        let _ = writeln!(out, "None.");
    } else {
        for outcome in outcomes.iter().filter(|o| o.status == Status::Skip) {
            let _ = writeln!(
                out,
                "- `{}` — {}",
                outcome.file,
                outcome.detail.as_deref().unwrap_or("(no detail)")
            );
        }
    }
    out.push('\n');
    out.push_str(KNOWN_ROOT_CAUSES);
    out
}

/// Hand-maintained appendix rendered into the generated report: root causes
/// for failures that are understood and intentionally not (yet) fixed. Keep in
/// sync with the failing list above — remove an entry when its test flips.
const KNOWN_ROOT_CAUSES: &str = "\
## Known root causes

- `test-assert.js` fails at the `assert.ok` generated-message checks (`'The \
expression evaluated to a falsy value:\\n\\n  strict.ok(...)'`). Node builds \
that message by eagerly capturing V8 `CallSite` objects for the caller and \
re-reading the call expression out of the source file. The chidori-js engine \
only materializes stack frames on the throw-unwind path (`record_unwind_frame`), \
so no caller position exists at message-construction time; eager capture would \
have to thread a shadow call stack through all three interpreter tiers' call \
dispatch. Everything before those checks — including the full `createErrDiff` \
Myers-diff message format — passes.
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn harness_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/node_compat")
    }

    #[test]
    fn wrapper_imports_only_named_builtins_and_provides_require() {
        let wrapped = wrap_node_test(
            "'use strict';\nrequire('../common');\nconst assert = require('assert');\nconst net = require('net');\n",
            "test-sample.js",
        );
        assert!(wrapped.contains("import __m0 from \"node:assert\""));
        assert!(wrapped.contains("node:net"));
        // Globals Node exposes are always provided.
        assert!(wrapped.contains("node:buffer"));
        assert!(wrapped.contains("node:url"));
        // No unrelated builtins.
        assert!(!wrapped.contains("node:zlib"));
        assert!(wrapped.contains("function require(name)"));
        assert!(wrapped.contains("__common.__verify()"));
    }

    #[test]
    fn required_specifier_scan_handles_quotes_and_prefixes() {
        let specs = required_specifiers(
            "const a = require('assert');\nconst b = require(\"node:zlib\");\nrequire('../common');",
        );
        assert!(specs.contains("assert"));
        assert!(specs.contains("node:zlib"));
        assert!(specs.contains("../common"));
    }

    /// The full suite, gated by expectations: any drift — a regression OR an
    /// improvement — fails with instructions, so `expectations.json` and the
    /// committed report always describe reality.
    #[test]
    fn node_compat_suite_matches_expectations() {
        let root = harness_root();
        let outcomes = run_suite(&root.join("suite"));
        assert!(!outcomes.is_empty(), "no vendored tests found — run scripts/vendor-node-compat-tests.sh");

        let actual: BTreeMap<String, String> = outcomes
            .iter()
            .map(|o| (o.file.clone(), o.status.as_str().to_string()))
            .collect();

        let node_version = std::fs::read_to_string(root.join("NODE_VERSION"))
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        // Filtered runs are for iterating on one test: compare only what ran,
        // never rewrite expectations from a partial run.
        if std::env::var("NODE_COMPAT_FILTER").is_ok() {
            let expected: BTreeMap<String, String> = serde_json::from_str(
                &std::fs::read_to_string(root.join("expectations.json")).unwrap_or_default(),
            )
            .unwrap_or_default();
            for (file, status) in &actual {
                eprintln!(
                    "node-compat[filtered]: {file} {status} (expected {})",
                    expected.get(file).map(String::as_str).unwrap_or("<absent>")
                );
            }
            return;
        }

        if std::env::var("NODE_COMPAT_UPDATE").as_deref() == Ok("1") {
            std::fs::write(
                root.join("expectations.json"),
                serde_json::to_string_pretty(&actual).unwrap() + "\n",
            )
            .unwrap();
            let report = render_report(&outcomes, &node_version);
            let docs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/node-compat-report.md");
            std::fs::write(&docs, report).unwrap();
            eprintln!("node-compat: expectations and docs/node-compat-report.md updated");
            return;
        }

        let expected: BTreeMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(root.join("expectations.json"))
                .expect("expectations.json missing — run with NODE_COMPAT_UPDATE=1 to create it"),
        )
        .expect("expectations.json parses");

        let mut drift = Vec::new();
        for (file, status) in &actual {
            match expected.get(file) {
                Some(want) if want == status => {}
                Some(want) => drift.push(format!("{file}: expected {want}, got {status}")),
                None => drift.push(format!("{file}: not in expectations (got {status})")),
            }
        }
        for file in expected.keys() {
            if !actual.contains_key(file) {
                drift.push(format!("{file}: in expectations but not vendored"));
            }
        }
        assert!(
            drift.is_empty(),
            "node-compat drift ({} entries) — if intentional, rerun with NODE_COMPAT_UPDATE=1 \
             and commit expectations.json + docs/node-compat-report.md:\n{}",
            drift.len(),
            drift.join("\n")
        );
    }
}
