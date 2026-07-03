//! Differential + structural tests for the cells→locals localization pass
//! (`localize.rs`).
//!
//! The guarantee: localization is a pure optimization. Every combination of
//! {localize on/off} × {fusion on/off} must produce byte-identical observable
//! behavior — same console output, same thrown error. The corpus concentrates
//! on exactly what localization must never break: closure capture (the
//! protected-cell analysis), per-iteration `let` semantics, TDZ errors,
//! `arguments` aliasing, direct `eval`, `with`, generators/async (suspended
//! frames carry `locals`), classes/derived constructors (`%this` stability),
//! and module-ish top-level patterns.

use std::rc::Rc;

use chidori_js::bytecode::{Const, FuncProto, Op};
use chidori_js::compiler::compile_script_passes;
use chidori_js::{Engine, Value};

fn run(src: &str, fuse: bool, localize: bool) -> (bool, Vec<String>, String) {
    let proto = Rc::new(compile_script_passes(src, fuse, localize).expect("compiles"));
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

const CORPUS: &[&str] = &[
    // Plain locals: loop counters, temporaries, arithmetic.
    "let s = 0; for (let i = 0; i < 10; i++) { s += i * 2 - (i % 3); } console.log(s);",
    // Capture forces cells: each iteration's closure sees a distinct binding.
    "const fs = []; for (let i = 0; i < 3; i++) fs.push(() => i); console.log(fs.map(f => f()).join(','));",
    // Mixed: captured accumulator + uncaptured counter in the same loop.
    "let acc = 0; const bump = () => { acc += 1; }; for (let i = 0; i < 5; i++) bump(); console.log(acc, typeof i === 'undefined');",
    // Capture by a LATER nested function (hoisting: the analysis must see it).
    "function outer() { let x = 1; function inner() { return x; } x = 5; return inner(); } console.log(outer());",
    // Deep transitive capture: grandchild reads grandparent's binding.
    "function a() { let v = 'g'; return function b() { return function c() { return v; }; }; } console.log(a()()());",
    // Parameter captured by a returned closure; sibling parameter localized.
    "function make(kept, dropped) { return () => kept + dropped; } console.log(make(1, 2)());",
    // TDZ on localized bindings: read and write before init both throw.
    "try { x; } catch (e) { console.log('r', e.constructor.name); } let x = 1; console.log(x);",
    "try { y = 1; } catch (e) { console.log('w', e.constructor.name); } let y; console.log(y);",
    // const assignment still throws through localized store.
    "const c = 1; try { c = 2; } catch (e) { console.log(e.constructor.name); } console.log(c);",
    // `arguments` aliasing (bails localization): writes flow both ways.
    "function f(p) { arguments[0] = 9; console.log(p); p = 3; console.log(arguments[0]); } f(1);",
    // Direct eval reads AND writes enclosing bindings (bails).
    "function g() { let v = 2; eval('v = v * 10'); return v; } console.log(g());",
    // `with` dynamic resolution (bails).
    "let w = 1; with ({ w: 42 }) { console.log(w); } console.log(w);",
    // Generator: suspended frame carries localized slots across yields.
    "function* gen(n) { let t = 0; for (let k = 0; k <= n; k++) { t += k; yield t; } } console.log([...gen(4)].join(','));",
    // Async/await: locals survive suspension and microtask resumption.
    "(async () => { let u = 1; await Promise.resolve(); u += 41; console.log('async', u); })();",
    // Generator captured state + uncaptured counter together.
    "function* g2() { let seen = []; const push = (v) => seen.push(v); for (let k = 0; k < 3; k++) { push(k); yield seen.length; } } console.log([...g2()].join(','));",
    // Classes: derived ctor %this stays a stable cell; methods + super.
    "class A { constructor() { this.v = 1; } m() { return this.v; } } class B extends A { constructor() { super(); this.v += 1; } m() { return super.m() + 10; } } console.log(new B().m());",
    // Private fields resolved through class-scope cells.
    "class P { #n = 5; get() { let d = 2; return this.#n * d; } } console.log(new P().get());",
    // try/catch binding + finally interplay with localized slots.
    "let r = ''; try { throw 'e'; } catch (err) { r += err; } finally { r += '!'; } console.log(r);",
    // Destructuring params/lets (many short-lived bindings).
    "function d({ a, b = 4 }, [c1, , c2]) { let { x: xx = a + b } = {}; return xx + c1 + c2; } console.log(d({ a: 1 }, [10, 20, 30]));",
    // Switch with lexical bindings per case block.
    "for (let i = 0; i < 3; i++) { switch (i) { case 1: { let q = 'one'; console.log(q); break; } default: { let q = 'd' + i; console.log(q); } } }",
    // Labeled break with captured loop variable (mixed protection).
    "const grabbed = []; outer: for (let i = 0; i < 4; i++) { for (let j = 0; j < 4; j++) { if (j === 2) continue outer; grabbed.push(() => i * 10 + j); if (i === 3) break outer; } } console.log(grabbed.map(f => f()).join(','));",
    // Statement-position updates on localized bindings (IncLocalStmt).
    "let i2 = 0; i2++; ++i2; i2--; console.log(i2); let b2 = 1n; b2++; console.log(b2);",
    // valueOf that REASSIGNS the binding mid-update (read-coerce-write order).
    "let m = { valueOf() { m = 100; return 7; } }; m++; console.log(m);",
    // Hoisted var + function declarations inside blocks.
    "function h() { var v = 1; { var v = 2; function inner2() { return 3; } } return v + inner2(); } console.log(h());",
];

#[test]
fn localization_preserves_observable_behavior() {
    for (n, src) in CORPUS.iter().enumerate() {
        let baseline = run(src, false, false);
        for (fuse, localize) in [(false, true), (true, false), (true, true)] {
            let got = run(src, fuse, localize);
            assert_eq!(
                baseline, got,
                "localize/fuse changed behavior for corpus[{n}] (fuse={fuse}, localize={localize}):\n  {src}\n  baseline={baseline:?}\n  got={got:?}"
            );
        }
    }
}

/// Count ops matching `pred` across a proto and its nested function
/// templates. The loop-kernel pass may replace a loop-header op with
/// `Op::LoopKernel`, preserving the original as the kernel's fallback —
/// count that op too, since it is what the generic path executes.
fn count_ops(proto: &FuncProto, pred: fn(&Op) -> bool) -> usize {
    let here = proto
        .code
        .iter()
        .map(|op| match op {
            Op::LoopKernel(i) => {
                let fb = &proto.kernels[*i as usize].fallback;
                usize::from(pred(op)) + usize::from(pred(fb))
            }
            _ => usize::from(pred(op)),
        })
        .sum::<usize>();
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
fn localization_actually_fires_and_bails() {
    // A plain counting loop localizes fully: no cell ops remain anywhere.
    let p = compile_script_passes(
        "let s = 0; for (let i = 0; i < 10; i++) { s += i; } console.log(s);",
        true,
        true,
    )
    .unwrap();
    let cell_ops = count_ops(&p, |op| {
        matches!(
            op,
            Op::LoadCell(_)
                | Op::StoreCell(_)
                | Op::StoreCellChecked(_)
                | Op::InitCell(_)
                | Op::InitCellTdz(_)
                | Op::LoadCellConst { .. }
                | Op::LoadCellInit { .. }
                | Op::IncCellStmt { .. }
                | Op::CmpCellConstBranchFalse { .. }
                | Op::CmpCellConstBranchTrue { .. }
                | Op::AddCellConst { .. }
                | Op::ArithCellConst { .. }
        )
    });
    assert_eq!(cell_ops, 0, "expected the counting loop to fully localize");
    assert!(
        count_ops(&p, |op| matches!(op, Op::CmpLocalConstBranchFalse { .. })) >= 1,
        "expected the loop test to fuse in LOCAL form"
    );

    // A captured binding stays a cell while its neighbors localize.
    let p = compile_script_passes(
        "function f() { let kept = 1, dropped = 2; const g = () => kept; return g() + dropped; } console.log(f());",
        true,
        true,
    )
    .unwrap();
    assert!(
        count_ops(&p, |op| matches!(
            op,
            Op::LoadCell(_)
                | Op::InitCell(_)
                | Op::InitCellTdz(_)
                | Op::StoreCell(_)
                | Op::LoadCellConst { .. }
        )) >= 1,
        "captured binding must remain a cell"
    );
    assert!(
        count_ops(&p, |op| matches!(op, Op::LoadLocal(_) | Op::StoreLocal(_))) >= 1,
        "uncaptured sibling must localize"
    );

    // Direct eval bails the whole function.
    let p = compile_script_passes(
        "function e() { let v = 1; eval('v'); return v; } console.log(e());",
        true,
        true,
    )
    .unwrap();
    let has_local_in_e = p.consts.iter().any(|c| match c {
        Const::Func(f) if f.name == "e" => {
            count_ops(f, |op| matches!(op, Op::LoadLocal(_) | Op::StoreLocal(_))) > 0
        }
        _ => false,
    });
    assert!(!has_local_in_e, "direct eval must bail localization");
}
