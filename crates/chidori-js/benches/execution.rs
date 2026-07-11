//! Micro-benchmarks for the JavaScript-execution hot paths of the chidori-js
//! engine: front-end compilation (oxc → bytecode), the bytecode interpreter
//! loop, and the common operation mixes (numeric loops, function calls,
//! property access, arrays, strings, closures).
//!
//! Three angles per workload:
//!
//! ```text
//! compile  — parse + lower to bytecode only (front-end cost).
//! eval     — full `Engine::new()` + parse + compile + run (end-to-end,
//!            what a real `eval()` call pays, realm setup included).
//! interp   — compile ONCE, then time only the VM running the bytecode
//!            (isolates the interpreter dispatch loop from the front end).
//! ```
//!
//! Run with: `cargo bench -p chidori-js`
//!
//! These isolate chidori-js's own hot paths in-process. For a cross-runtime
//! comparison of the same workloads against Node.js, Bun, and CPython, see the
//! harness in `benchmarks/` (`node crates/chidori-js/benchmarks/run.mjs`).

use std::rc::Rc;
use std::time::Duration;

use chidori_js::bytecode::FuncProto;
use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

// Workload corpus shared with `benches/memory.rs` so time and memory numbers
// describe the same programs.
#[path = "common/workloads.rs"]
mod workloads;
use workloads::WORKLOADS;

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
    targets = bench_compile, bench_eval, bench_interp, bench_engine_new
}
criterion_main!(benches);
