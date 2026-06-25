//! Closure-threading JIT diagnostics for `chidori-js` (see `docs/jit.md`).
//!
//! Reports two things per workload:
//!
//!   * **Static dispatch proxy (deterministic, environment-independent).** Walks
//!     the whole proto tree (entry script + every nested function template),
//!     JIT-compiles each, and sums how many ops were lowered to a *specialized*
//!     inline closure vs. *delegated* to the switch interpreter's `step`.
//!     `specialized` is the number of central-`match` dispatches the JIT removes
//!     on each pass over the code — a reproducible figure the dev/CI container's
//!     ~10–15% wall-clock noise floor (`docs/interpreter-optimization.md` §7.6)
//!     cannot obscure. (Static: each op counted once, regardless of loop trips.)
//!   * **Indicative wall-clock (min-of-N, JIT on vs. off).** Compile once, then
//!     run the workload many times on a reused engine with `jit_enabled` toggled.
//!     Min-of-N is the noise-robust statistic §7.6 recommends; still, treat
//!     small deltas here as inconclusive and rerun on a quiet, pinned machine.
//!
//! Run: `cargo run -q --release --example jit_stats -p chidori-js`

use std::rc::Rc;
use std::time::Instant;

use chidori_js::bytecode::{Const, FuncProto};
use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};

/// The same representative workloads as `benches/execution.rs`.
const WORKLOADS: &[(&str, &str)] = &[
    (
        "arith_loop",
        "(function(){ let s = 0; for (let i = 0; i < 20000; i++) { s += i * 2 - (i % 3); } return s; })()",
    ),
    (
        "fib_recursive",
        "(function(){ function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); } return fib(24); })()",
    ),
    (
        "property_access",
        "(function(){ let o = { a: 0, b: 0, c: 0 }; for (let i = 0; i < 10000; i++) { o.a = i; o.b = o.a + 1; o.c = o.b + o.a; } return o.c; })()",
    ),
    (
        "array_push_sum",
        "(function(){ let a = []; for (let i = 0; i < 5000; i++) a.push(i); let s = 0; for (let i = 0; i < a.length; i++) s += a[i]; return s; })()",
    ),
    (
        "array_hof",
        "(function(){ let a = []; for (let i = 0; i < 2000; i++) a.push(i); return a.map(x => x * x).filter(x => x % 2 === 0).reduce((p, c) => p + c, 0); })()",
    ),
    (
        "string_build",
        "(function(){ let s = ''; for (let i = 0; i < 3000; i++) s += 'x' + i; return s.length; })()",
    ),
    (
        "closures",
        "(function(){ function adder(n){ return function(x){ return x + n; }; } let f = adder(5); let s = 0; for (let i = 0; i < 10000; i++) s = f(s) - 4; return s; })()",
    ),
];

/// Recursively JIT-compile a proto and its nested templates, summing
/// `(specialized, fallback)` op counts across the whole tree.
fn walk(proto: &FuncProto, spec: &mut u64, fall: &mut u64) {
    let thread = proto.jit.get_or_compile(proto);
    *spec += thread.specialized as u64;
    *fall += thread.fallback as u64;
    for c in &proto.consts {
        if let Const::Func(f) = c {
            walk(f, spec, fall);
        }
    }
}

/// Min wall-clock over `batches` batches of `iters` runs of `proto` on one
/// reused engine with the given JIT setting.
fn time_min(proto: &Rc<FuncProto>, jit: bool, batches: u32, iters: u32) -> std::time::Duration {
    let mut engine = Engine::new();
    engine.vm.jit_enabled = jit;
    // Warm up (compile the thread / fault in allocations) before timing.
    for _ in 0..iters.min(8) {
        run_once(&mut engine, proto);
    }
    let mut best = std::time::Duration::MAX;
    for _ in 0..batches {
        let t0 = Instant::now();
        for _ in 0..iters {
            run_once(&mut engine, proto);
        }
        best = best.min(t0.elapsed());
    }
    best
}

fn run_once(engine: &mut Engine, proto: &Rc<FuncProto>) {
    let func = engine.vm.make_closure(proto.clone(), Vec::new());
    let _ = engine
        .vm
        .call(Value::Object(func), Value::Undefined, &[])
        .expect("workload must not throw");
}

fn main() {
    // Lighter inner loops keep the example quick; the ratio is what matters.
    const BATCHES: u32 = 12;
    const ITERS: u32 = 20;

    println!("=== JIT static dispatch proxy (whole proto tree, as-emitted) ===\n");
    println!(
        "  {:<16} {:>10} {:>10} {:>12}",
        "workload", "special.", "fallback", "specialized%"
    );
    for (name, src) in WORKLOADS {
        let proto = compile_script(src).expect("compiles");
        let (mut spec, mut fall) = (0u64, 0u64);
        walk(&proto, &mut spec, &mut fall);
        let total = spec + fall;
        let pct = if total > 0 {
            100.0 * spec as f64 / total as f64
        } else {
            0.0
        };
        println!("  {name:<16} {spec:>10} {fall:>10} {pct:>11.1}%");
    }

    println!("\n=== Indicative wall-clock (min-of-N), JIT on vs. off ===");
    println!(
        "  (noise floor ~10–15% here, docs/interpreter-optimization.md §7.6 — \
         treat small deltas as inconclusive)\n"
    );
    println!(
        "  {:<16} {:>12} {:>12} {:>10}",
        "workload", "jit (ms/run)", "interp", "speedup"
    );
    for (name, src) in WORKLOADS {
        let proto = Rc::new(compile_script(src).expect("compiles"));
        let jit = time_min(&proto, true, BATCHES, ITERS);
        let interp = time_min(&proto, false, BATCHES, ITERS);
        let jit_ms = jit.as_secs_f64() * 1e3 / ITERS as f64;
        let interp_ms = interp.as_secs_f64() * 1e3 / ITERS as f64;
        let speedup = interp.as_secs_f64() / jit.as_secs_f64();
        println!("  {name:<16} {jit_ms:>12.4} {interp_ms:>12.4} {speedup:>9.2}x");
    }
}
