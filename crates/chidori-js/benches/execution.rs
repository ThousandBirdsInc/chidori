//! Micro-benchmarks for the JavaScript-execution hot paths of the chidori-js
//! engine: front-end compilation (oxc → bytecode), the bytecode interpreter
//! loop, and the common operation mixes (numeric loops, function calls,
//! property access, arrays, strings, closures).
//!
//! Three angles per workload:
//!   * `compile`  — parse + lower to bytecode only (front-end cost).
//!   * `eval`     — full `Engine::new()` + parse + compile + run (end-to-end,
//!                  what a real `eval()` call pays, realm setup included).
//!   * `interp`   — compile ONCE, then time only the VM running the bytecode
//!                  (isolates the interpreter dispatch loop from the front end).
//!
//! Run with: `cargo bench -p chidori-js`
//!
//! These isolate chidori-js's own hot paths in-process. For a cross-runtime
//! comparison of the same workloads against Node.js and Bun, see the harness in
//! `benchmarks/` (`node crates/chidori-js/benchmarks/run.mjs`).

use std::rc::Rc;
use std::time::Duration;

use chidori_js::bytecode::FuncProto;
use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// Representative workloads, each an IIFE so it returns a value and declares no
/// globals (lets the `interp` group reuse one engine across iterations).
const WORKLOADS: &[(&str, &str)] = &[
    // Tight numeric loop — interpreter dispatch + integer/float arithmetic.
    (
        "arith_loop",
        "(function(){ let s = 0; for (let i = 0; i < 20000; i++) { s += i * 2 - (i % 3); } return s; })()",
    ),
    // Recursion + function-call overhead (frame setup/teardown).
    (
        "fib_recursive",
        "(function(){ function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); } return fib(24); })()",
    ),
    // Object property get/set in a loop (shape lookups, map access).
    (
        "property_access",
        "(function(){ let o = { a: 0, b: 0, c: 0 }; for (let i = 0; i < 10000; i++) { o.a = i; o.b = o.a + 1; o.c = o.b + o.a; } return o.c; })()",
    ),
    // Array growth + indexed read loop.
    (
        "array_push_sum",
        "(function(){ let a = []; for (let i = 0; i < 5000; i++) a.push(i); let s = 0; for (let i = 0; i < a.length; i++) s += a[i]; return s; })()",
    ),
    // Higher-order array methods (map/filter/reduce + per-element closures).
    (
        "array_hof",
        "(function(){ let a = []; for (let i = 0; i < 2000; i++) a.push(i); return a.map(x => x * x).filter(x => x % 2 === 0).reduce((p, c) => p + c, 0); })()",
    ),
    // String building (concatenation + number→string coercion).
    (
        "string_build",
        "(function(){ let s = ''; for (let i = 0; i < 3000; i++) s += 'x' + i; return s.length; })()",
    ),
    // Closures + higher-order calls in a loop (upvalue capture/read).
    (
        "closures",
        "(function(){ function adder(n){ return function(x){ return x + n; }; } let f = adder(5); let s = 0; for (let i = 0; i < 10000; i++) s = f(s) - 4; return s; })()",
    ),
];

/// Build a fresh closure for `proto` and run it to completion on `engine`.
fn run_proto(engine: &mut Engine, proto: &Rc<FuncProto>) -> Value {
    let func = engine.vm.make_closure(proto.clone(), Vec::new());
    engine
        .vm
        .call(Value::Object(func), Value::Undefined, &[])
        .expect("benchmark workload must not throw")
}

fn bench_compile(c: &mut Criterion) {
    let mut g = c.benchmark_group("compile");
    for (name, src) in WORKLOADS {
        g.bench_function(*name, |b| {
            b.iter(|| {
                let proto = compile_script(black_box(src)).expect("compiles");
                black_box(proto);
            })
        });
    }
    g.finish();
}

fn bench_eval(c: &mut Criterion) {
    let mut g = c.benchmark_group("eval");
    for (name, src) in WORKLOADS {
        g.bench_function(*name, |b| {
            b.iter(|| {
                let mut engine = Engine::new();
                let v = engine.eval(black_box(src)).expect("evaluates");
                black_box(v);
            })
        });
    }
    g.finish();
}

fn bench_interp(c: &mut Criterion) {
    let mut g = c.benchmark_group("interp");
    for (name, src) in WORKLOADS {
        // Compile once, outside the timing loop; reuse one engine.
        let proto = Rc::new(compile_script(src).expect("compiles"));
        let mut engine = Engine::new();
        g.bench_function(*name, |b| {
            b.iter(|| {
                let v = run_proto(&mut engine, black_box(&proto));
                black_box(v);
            })
        });
    }
    g.finish();
}

/// Closure-threading JIT vs. the switch interpreter on the same workloads
/// (`docs/jit.md`). Each workload is benched twice — `<name>/jit` with the
/// experimental backend (the default) and `<name>/interp` with `jit_enabled =
/// false` — so a single run reports the per-workload delta. NOTE: per
/// `docs/interpreter-optimization.md` §7.6 the dev/CI container's wall-clock
/// noise floor is ~10–15%, so treat small deltas here as inconclusive and lean
/// on the deterministic dispatch proxy in `examples/jit_stats.rs`; run this on a
/// quiet, frequency-pinned machine to resolve real wins.
fn bench_jit_vs_interp(c: &mut Criterion) {
    let mut g = c.benchmark_group("jit_vs_interp");
    for (name, src) in WORKLOADS {
        let proto = Rc::new(compile_script(src).expect("compiles"));

        let mut jit_engine = Engine::new();
        jit_engine.vm.jit_enabled = true;
        g.bench_function(format!("{name}/jit"), |b| {
            b.iter(|| {
                let v = run_proto(&mut jit_engine, black_box(&proto));
                black_box(v);
            })
        });

        let mut interp_engine = Engine::new();
        interp_engine.vm.jit_enabled = false;
        g.bench_function(format!("{name}/interp"), |b| {
            b.iter(|| {
                let v = run_proto(&mut interp_engine, black_box(&proto));
                black_box(v);
            })
        });
    }
    g.finish();
}

/// Cost of standing up a fresh realm (global object + all built-in prototypes) —
/// paid on every `Engine::new()` / `eval()`.
fn bench_engine_new(c: &mut Criterion) {
    c.bench_function("engine_new", |b| {
        b.iter(|| {
            let engine = Engine::new();
            black_box(engine);
        })
    });
}

criterion_group! {
    name = benches;
    // Keep wall-clock reasonable: these workloads are individually cheap.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));
    targets = bench_compile, bench_eval, bench_interp, bench_jit_vs_interp, bench_engine_new
}
criterion_main!(benches);
