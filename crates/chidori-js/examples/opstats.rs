//! Phase-0 opcode-frequency analyzer for the `chidori-js` interpreter.
//!
//! Produces the data that drives the interpreter-optimization plan
//! (`docs/interpreter-optimization.md`): which opcodes dominate, and which
//! adjacent opcode *pairs* are the best superinstruction / op-fusion candidates
//! (Phase 2).
//!
//! Two views:
//!   * **Static** — walk the compiled bytecode of each workload (recursing into
//!     nested function templates) and count opcodes + adjacent pairs *as emitted*.
//!     Always available; no engine instrumentation. Undercounts loop bodies
//!     (each instruction is counted once regardless of how often it runs).
//!   * **Dynamic** — count opcodes + adjacent pairs *as executed*, weighted by
//!     real control flow (loops, recursion). Requires the `op-histogram` feature,
//!     which instruments the interpreter loop. This is the view that matters for
//!     fusion.
//!
//! Run:
//! ```sh
//! # static only:
//! cargo run -q --release --example opstats -p chidori-js
//! # static + dynamic (execution-weighted):
//! cargo run -q --release --example opstats -p chidori-js --features op-histogram
//! ```

use std::collections::HashMap;

use chidori_js::bytecode::{Const, FuncProto, Op};
use chidori_js::compiler::compile_script;

/// The same representative workloads as `benches/execution.rs`, plus a `mixed`
/// program that interleaves them — a stand-in for the JS an agent runs *between*
/// host calls (the unit that interpreter speed actually affects in production).
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

/// `Op` variant name without payload (`Op` derives `Debug`).
fn variant_name(op: &Op) -> String {
    let dbg = format!("{op:?}");
    let end = dbg
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(dbg.len());
    dbg[..end].to_string()
}

/// Recursively count opcodes + adjacent pairs across a proto and its nested
/// function templates (`Const::Func`). Pairs are counted within a single
/// `code` vec only (no cross-function pairing), matching how a peephole pass
/// over emitted bytecode would see them.
fn walk_static(
    proto: &FuncProto,
    ops: &mut HashMap<String, u64>,
    pairs: &mut HashMap<(String, String), u64>,
) {
    let mut prev: Option<String> = None;
    for op in &proto.code {
        let name = variant_name(op);
        *ops.entry(name.clone()).or_insert(0) += 1;
        if let Some(p) = prev.take() {
            *pairs.entry((p, name.clone())).or_insert(0) += 1;
        }
        prev = Some(name);
    }
    for c in &proto.consts {
        if let Const::Func(f) = c {
            walk_static(f, ops, pairs);
        }
    }
}

fn print_table<K: std::fmt::Display>(title: &str, rows: &[(K, u64)], total: u64, top: usize) {
    println!("  {title}");
    for (k, n) in rows.iter().take(top) {
        let pct = if total > 0 {
            100.0 * (*n as f64) / (total as f64)
        } else {
            0.0
        };
        println!("    {n:>12}  {pct:>5.1}%  {k}");
    }
}

fn sorted_ops(m: HashMap<String, u64>) -> Vec<(String, u64)> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

fn sorted_pairs(m: HashMap<(String, String), u64>) -> Vec<(String, u64)> {
    let mut v: Vec<_> = m
        .into_iter()
        .map(|((a, b), n)| (format!("{a} ; {b}"), n))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

fn main() {
    const TOP: usize = 15;

    println!("=== STATIC opcode frequency (as emitted; undercounts loops) ===\n");
    let mut all_ops: HashMap<String, u64> = HashMap::new();
    let mut all_pairs: HashMap<(String, String), u64> = HashMap::new();
    for (name, src) in WORKLOADS {
        let proto = compile_script(src).expect("compiles");
        let mut ops = HashMap::new();
        let mut pairs = HashMap::new();
        walk_static(&proto, &mut ops, &mut pairs);
        for (k, v) in &ops {
            *all_ops.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &pairs {
            *all_pairs.entry(k.clone()).or_insert(0) += v;
        }
        let total: u64 = ops.values().sum();
        println!("[{name}] {total} static opcodes");
    }
    let total: u64 = all_ops.values().sum();
    println!("\nAll workloads combined ({total} static opcodes):");
    print_table("top opcodes:", &sorted_ops(all_ops), total, TOP);
    let ptotal: u64 = total; // pairs ~ ops-1 per proto; use ops total as scale
    print_table(
        "top adjacent pairs (fusion candidates):",
        &sorted_pairs(all_pairs),
        ptotal,
        TOP,
    );

    #[cfg(feature = "op-histogram")]
    {
        use chidori_js::{opstats, Engine};
        println!("\n=== DYNAMIC opcode frequency (as executed; weighted by control flow) ===\n");
        let mut tot_ops: HashMap<String, u64> = HashMap::new();
        let mut tot_pairs: HashMap<(String, String), u64> = HashMap::new();
        let mut grand_total: u64 = 0;
        for (name, src) in WORKLOADS {
            opstats::reset();
            let mut engine = Engine::new();
            let _ = engine.eval(src).expect("evaluates");
            let report = opstats::take();
            grand_total += report.total;
            println!("[{name}] {} executed opcodes", report.total);
            for (k, v) in &report.ops {
                *tot_ops.entry((*k).to_string()).or_insert(0) += v;
            }
            for ((a, b), v) in &report.pairs {
                *tot_pairs
                    .entry(((*a).to_string(), (*b).to_string()))
                    .or_insert(0) += v;
            }
        }
        println!("\nAll workloads combined ({grand_total} executed opcodes):");
        print_table("top opcodes:", &sorted_ops(tot_ops), grand_total, TOP);
        print_table(
            "top adjacent pairs (fusion candidates):",
            &sorted_pairs(tot_pairs),
            grand_total,
            TOP,
        );
    }
    #[cfg(not(feature = "op-histogram"))]
    {
        println!(
            "\n(Dynamic execution-weighted view skipped: rebuild with \
             `--features op-histogram` to enable interpreter instrumentation.)"
        );
    }
}
