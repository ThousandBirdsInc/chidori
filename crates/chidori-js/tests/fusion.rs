//! Differential + structural tests for the Phase-2 op-fusion pass (`fuse.rs`).
//!
//! The core guarantee: fusion is a pure optimization. Compiling a program with
//! the peephole pass ON must produce byte-identical *observable* behavior to
//! compiling it OFF — same console output, same thrown error (or lack of one).
//! Because fusion shortens bytecode and thus shifts absolute jump targets, the
//! corpus deliberately stresses control flow that indexes into `code`: loops,
//! `break`/`continue`, labeled loops, `try`/`catch`/`finally`, `switch`,
//! optional chaining, and short-circuit operators. A mis-remapped jump target
//! would make the fused run diverge from the unfused run and fail loudly here.

use std::rc::Rc;

use chidori_js::bytecode::{Const, FuncProto, Op};
use chidori_js::compiler::compile_script_opts;
use chidori_js::{Engine, Value};

/// Compile `src` with fusion `fuse`, run it to completion (draining microtasks),
/// and return its observable outcome: `(threw, console_lines, error_string)`.
fn run(src: &str, fuse: bool) -> (bool, Vec<String>, String) {
    let proto = Rc::new(compile_script_opts(src, fuse).expect("compiles"));
    let mut engine = Engine::new();
    let func = engine.vm.make_closure(proto, Vec::new());
    let res = engine.vm.call(Value::Object(func), Value::Undefined, &[]);
    let _ = engine.vm.run_jobs_until_blocked();
    let console = engine.console().to_vec();
    match res {
        Ok(_) => (false, console, String::new()),
        Err(e) => (true, console, engine.vm.error_to_string(&e)),
    }
}

/// Programs that mix the fused idiom (`cmp ; JumpIfFalse`) with control flow
/// whose jump targets must survive the bytecode-shortening remap.
const CORPUS: &[&str] = &[
    // Every fuseable comparison as a loop condition.
    "for (let i = 0; i < 5; i++) console.log('lt', i);",
    "for (let i = 0; i <= 5; i++) console.log('le', i);",
    "for (let i = 5; i > 0; i--) console.log('gt', i);",
    "for (let i = 5; i >= 0; i--) console.log('ge', i);",
    "let i = 0; while (i != 4) { console.log('ne', i); i++; }",
    "let i = 0; while (i !== 4) { console.log('sne', i); i++; }",
    "for (let i = 0; i == 0 || i < 3; i++) console.log('eq/seq', i === 1);",
    // Nested loops with break/continue — jump targets after fused ops.
    "for (let i = 0; i < 4; i++) { if (i === 2) continue; for (let j = 0; j < i; j++) { if (j > 5) break; console.log(i, j); } }",
    // Labeled break across nested loops.
    "outer: for (let i = 0; i < 5; i++) { for (let j = 0; j < 5; j++) { if (i * j >= 6) break outer; console.log('lab', i, j); } }",
    // try/catch/finally interleaved with a fused loop condition.
    "let s = 0; for (let i = 0; i < 6; i++) { try { if (i % 2 === 0) throw i; s += i; } catch (e) { console.log('caught', e); } finally { console.log('fin', i); } } console.log('s', s);",
    // switch (jump table) with a comparison-driven loop around it.
    "for (let i = 0; i < 4; i++) { switch (i) { case 0: console.log('zero'); break; case 1: case 2: console.log('one-two', i); break; default: console.log('def', i); } }",
    // Optional chaining (JumpIfNullish) + comparison.
    "let o = { a: { b: 3 } }; for (let i = 0; i < 3; i++) { console.log(o?.a?.b, o?.x?.y, i < 2); }",
    // Short-circuit peek-jumps adjacent to comparisons.
    "for (let i = 0; i < 5; i++) { let r = (i > 1) && (i < 4) || (i === 0); console.log('sc', r); }",
    // do/while (condition at the bottom) — comparison before a back-edge.
    "let i = 0; do { console.log('dw', i); i++; } while (i < 3);",
    // Comparison result used as a value (NOT immediately branched) must stay a
    // standalone comparison op and still be correct.
    "for (let i = 0; i < 4; i++) { let b = i < 2; console.log('val', b); }",
    // Functions/closures: fusion runs per-proto, nested protos included.
    "function f(n) { let s = 0; for (let k = 0; k < n; k++) if (k !== 1) s += k; return s; } console.log('f', f(5), f(0));",
    // Recursion + ternary comparisons.
    "function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); } console.log('fib', fib(10));",
    // String / mixed-type comparisons (coercion must match exactly).
    "for (let i = 0; i < 3; i++) { console.log('mix', '2' == 2, '2' === 2, 'a' < 'b', null == undefined); }",
    // A comparison whose operand coercion THROWS — fused and unfused must throw
    // identically (Symbol → number is a TypeError).
    "let s = Symbol(); for (let i = 0; i < 3; i++) { console.log(i < s); }",
];

#[test]
fn fusion_preserves_observable_behavior() {
    for (n, src) in CORPUS.iter().enumerate() {
        let off = run(src, false);
        let on = run(src, true);
        assert_eq!(
            off, on,
            "fusion changed observable behavior for corpus[{n}]:\n  {src}\n  unfused={off:?}\n  fused={on:?}"
        );
    }
}

/// Count ops matching `pred` across a proto and its nested function templates.
fn count_ops(proto: &FuncProto, pred: fn(&Op) -> bool) -> usize {
    let here = proto.code.iter().filter(|op| pred(op)).count();
    let nested: usize = proto
        .consts
        .iter()
        .map(|c| match c {
            Const::Func(f) => count_ops(f, pred),
            _ => 0,
        })
        .sum();
    here + nested
}

#[test]
fn fusion_actually_fires() {
    // A plain counting loop fuses both its operand-load pair (`LoadCell ;
    // LoadConst`) and its compare-and-branch (`Lt ; JumpIfFalse`).
    let fused = compile_script_opts("for (let i = 0; i < 10; i++) { i + 1; }", true).unwrap();
    assert!(
        count_ops(&fused, |op| matches!(op, Op::CmpBranchFalse { .. })) >= 1,
        "expected the loop condition to fuse into CmpBranchFalse"
    );
    assert!(
        count_ops(&fused, |op| matches!(op, Op::LoadCellConst { .. })) >= 1,
        "expected the operand loads to fuse into LoadCellConst"
    );
    // A bottom-tested loop fuses its back-edge into CmpBranchTrue.
    let dw = compile_script_opts("let i = 0; do { i++; } while (i < 5);", true).unwrap();
    assert!(
        count_ops(&dw, |op| matches!(op, Op::CmpBranchTrue { .. })) >= 1,
        "expected the do/while back-edge to fuse into CmpBranchTrue"
    );
    // The toggle must genuinely suppress every fusion (so the unfused side of the
    // differential test is really unfused).
    let unfused = compile_script_opts("for (let i = 0; i < 10; i++) { i + 1; }", false).unwrap();
    let any_fused = count_ops(&unfused, |op| {
        matches!(
            op,
            Op::CmpBranchFalse { .. } | Op::CmpBranchTrue { .. } | Op::LoadCellConst { .. }
        )
    });
    assert_eq!(any_fused, 0, "fusion toggle off must emit no fused ops");
}
