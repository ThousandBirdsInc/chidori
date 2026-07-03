//! Differential + structural tests for the typed loop-kernel pass
//! (`kernel.rs`).
//!
//! The guarantee: kernelization is a pure optimization. Every program must
//! produce byte-identical observable behavior with kernels on and off — same
//! console output, same completion value, same thrown error. The corpus
//! concentrates on what the kernels must never break: exact numeric semantics
//! (NaN, -0, infinities, `%`, `>>>`, shift masking, float precision),
//! control flow (break/continue/labels, nested loops, short-circuits,
//! ternaries, do/while), guard bail-outs and LATE ENTRY (a binding that warms
//! from undefined to a number mid-loop), and the boundary between kernel and
//! generic execution (calls, strings, property access inside loops disable
//! the kernel without changing results).

use std::rc::Rc;

use chidori_js::bytecode::{Const, FuncProto, Op};
use chidori_js::compiler::{compile_script, compile_script_kernels};
use chidori_js::{Engine, Value};

fn run(src: &str, kernels: bool) -> (bool, Vec<String>, String) {
    let proto = Rc::new(compile_script_kernels(src, kernels).expect("compiles"));
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
    // The canonical counting loop (mirrors the arith_loop workload).
    "let s = 0; for (let i = 0; i < 1000; i++) { s += i * 2 - (i % 3); } console.log(s);",
    // while / do-while shapes (conditional back-edge).
    "let i = 0, s = 0; while (i < 500) { s = (s + i) | 0; i++; } console.log(s);",
    "let i = 10, n = 0; do { n += i; i--; } while (i > 0); console.log(n);",
    "let i = 0; do { i++; } while (false); console.log(i);",
    // break / continue / labeled break out of nested loops.
    "let s = 0; for (let i = 0; i < 100; i++) { if (i === 7) continue; if (i > 20) break; s += i; } console.log(s);",
    "let s = 0; outer: for (let i = 0; i < 10; i++) { for (let j = 0; j < 10; j++) { if (i * j > 30) break outer; s += j; } } console.log(s);",
    // Nested numeric loops: the inner one kernels; the outer stays generic.
    "let s = 0; for (let i = 0; i < 50; i++) { for (let j = 0; j < 50; j++) { s += i ^ j; } } console.log(s);",
    // Exact float semantics: NaN, -0, infinities, precision.
    "let x = 0; for (let i = 0; i < 10; i++) { x += 0.1; } console.log(x);",
    "let s = 0; for (let i = -5; i < 5; i++) { s += 1 / i; } console.log(s, 1 / s);",
    "let z = 0; for (let i = 0; i < 3; i++) { z = -0 * i + z; } console.log(Object.is(z, -0), Object.is(z, 0));",
    "let n = 0; for (let i = 0; i < 4; i++) { n = n + (i === 2 ? NaN : i); } console.log(n, n === n);",
    // Modulo sign / integer fast path vs fmod, ** precedence, division.
    "let s = ''; for (let i = -6; i <= 6; i += 3) { s += (i % 4) + ','; } console.log(s);",
    "let p = 0; for (let i = 1; i < 6; i++) { p += 2 ** i; } console.log(p, (-8) % 3, 7 / 2);",
    // 32-bit ops: shifts mask their count, >>> produces unsigned, ~ truncates.
    "let h = 123456789; for (let i = 0; i < 100; i++) { h = (h * 31 + i) | 0; } console.log(h);",
    "let u = 0; for (let i = 0; i < 40; i++) { u = (u + (1 << i)) >>> 0; } console.log(u);",
    "let b = 0; for (let i = 0; i < 10; i++) { b ^= ~i & (i >> 1) | (i >>> 2); } console.log(b);",
    "let seed = 123456789, c = 0; for (let r = 0; r < 6; r++) { seed = (seed * 1103515245 + 12345) >>> 0; c = (c + seed) >>> 0; } console.log(c);",
    // Comparisons in all branch polarities, equality with NaN.
    "let c = 0; for (let i = 0; i < 20; i++) { if (i >= 10) c++; if (i <= 3) c--; if (i === 5) c += 100; if (i !== 5) c += 2; } console.log(c);",
    "let c = 0, x = NaN; for (let i = 0; i < 5; i++) { if (x < i) c += 1; if (x >= i) c += 2; if (x === x) c += 4; } console.log(c);",
    // Short-circuits and ternaries inside the loop body (peek jumps).
    "let s = 0; for (let i = 0; i < 30; i++) { s += i % 2 && i; } console.log(s);",
    "let s = 1; for (let i = 0; i < 10; i++) { s = i || s; } console.log(s);",
    "let s = 0; for (let i = 0; i < 25; i++) { s += i > 12 ? i * 2 : -i; } console.log(s);",
    // Compound assignments and update expressions in expression position.
    "let a = 1, b = 2, s = 0; for (let i = 0; i < 10; i++) { a *= 1.5; b -= 0.25; s = a + b + s; } console.log(s);",
    "let i = 0, s = 0; while (i < 10) { s += i++; } console.log(s, i);",
    "let i = 10, s = 0; while (i > 0) { s += --i; } console.log(s, i);",
    // Multiple locals, swaps via temporaries (fibonacci iteration).
    "let a = 0, b = 1; for (let i = 0; i < 30; i++) { const t = a + b; a = b; b = t; } console.log(a, b);",
    // Empty / single-iteration / zero-iteration loops.
    "let s = 0; for (let i = 0; i < 0; i++) { s += i; } console.log(s);",
    "for (let i = 0; i < 3; i++) {} console.log('done');",
    // Infinite loop broken from inside.
    "let i = 0; for (;;) { i++; if (i > 100) break; } console.log(i);",
    // GUARD BAIL: a string flows through the loop-carried binding — the loop
    // must stay generic (or bail) and concatenate exactly.
    "let s = 0; for (let i = 0; i < 5; i++) { if (i === 3) s = '' + s; s += i; } console.log(s);",
    "let x = 'a'; for (let i = 0; i < 3; i++) { x += i; } console.log(x);",
    // LATE ENTRY: binding starts undefined (guard fails on iteration 1),
    // becomes a number, kernel may take over — result must be identical.
    "let t; let s = 0; for (let i = 0; i < 10; i++) { t = (t || 0) + i; s = t; } console.log(s, t);",
    // Calls / property access / arrays in the body disable kernels entirely.
    "let s = 0; for (let i = 0; i < 10; i++) { s += Math.max(i, 5); } console.log(s);",
    "const a = [1, 2, 3]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
    // Loop condition via function call each iteration (header not eligible).
    "let i = 0; function lim() { return 5; } let s = 0; while (i < lim()) { s += i; i++; } console.log(s);",
    // Captured binding inside the loop -> cells, not locals: stays generic.
    "const fs = []; let s = 0; for (let i = 0; i < 3; i++) { fs.push(() => i); s += i; } console.log(s, fs.map(f => f()).join(','));",
    // try/catch enclosing the loop (handlers OUTSIDE the region are fine).
    "try { let s = 0; for (let i = 0; i < 10; i++) { s += i; } console.log(s); } catch (e) { console.log('no'); }",
    // try/catch INSIDE the loop body (handlers in-region: stays generic).
    "let s = 0; for (let i = 0; i < 5; i++) { try { s += i; } catch (e) {} } console.log(s);",
    // Throw from a loop that LOOKS numeric until it isn't: division by a
    // string-tainted binding after bail.
    "let d = 2; let s = 0; for (let i = 0; i < 6; i++) { if (i === 4) d = '0'; s += i / d; } console.log(s);",
    // Sequence/comma expressions and grouped arithmetic.
    "let s = 0; for (let i = 0, j = 10; i < j; i++, j--) { s += i * j; } console.log(s);",
    // Unary minus / plus on loop values.
    "let s = 0; for (let i = 0; i < 8; i++) { s += -i + +i * 2; } console.log(s);",
    // A loop whose body reassigns the LOOP BOUND (loop-carried compare operand).
    "let n = 10, s = 0; for (let i = 0; i < n; i++) { if (i === 5) n = 8; s += 1; } console.log(s);",
    // Number.MAX_SAFE_INTEGER-scale accumulation (precision at the edge).
    "let s = 9007199254740980; for (let i = 0; i < 20; i++) { s += 1; } console.log(s);",
    // Loop inside a FUNCTION (kernels apply per-proto, not just top level).
    "function hot(n) { let s = 0; for (let i = 0; i < n; i++) { s = (s * 3 + i) % 1000003; } return s; } console.log(hot(1000), hot(0), hot(1));",
    // Generator containing a numeric loop between yields (frame suspension
    // around — never inside — the kernel region).
    "function* g() { let s = 0; for (let i = 0; i < 100; i++) { s += i; } yield s; for (let i = 0; i < 10; i++) { s -= i; } yield s; } const it = g(); console.log(it.next().value, it.next().value);",
    // Async function with a kernel-able loop after an await.
    "(async () => { await 0; let s = 0; for (let i = 0; i < 50; i++) { s += i * i; } console.log('async', s); })();",
    // Switch dispatch inside a loop (CompletionJump/complex flow: generic).
    "let s = 0; for (let i = 0; i < 12; i++) { switch (i % 3) { case 0: s += 1; break; case 1: s += 10; break; default: s += 100; } } console.log(s);",
    // Deeply chained expression stressing canonical stack depth.
    "let s = 0; for (let i = 1; i < 20; i++) { s += ((i + 1) * (i + 2) - (i + 3)) / ((i % 5) + 1) + ((i << 2) ^ (i >> 1)); } console.log(s);",
];

#[test]
fn kernel_differential_corpus() {
    for (n, src) in CORPUS.iter().enumerate() {
        let with = run(src, true);
        let without = run(src, false);
        assert_eq!(
            with, without,
            "kernels changed observable behavior for corpus[{n}]:\n{src}"
        );
    }
}

/// The production compile of the canonical counting loop actually installs a
/// kernel (guards the pass against silently regressing to "never eligible").
#[test]
fn canonical_loop_gets_a_kernel() {
    fn count_kernels(p: &FuncProto) -> usize {
        let mut n = p.kernels.len();
        for c in &p.consts {
            if let Const::Func(f) = c {
                n += count_kernels(f);
            }
        }
        n
    }
    let proto =
        compile_script("let s = 0; for (let i = 0; i < 10; i++) { s += i * 2 - (i % 3); } s;")
            .expect("compiles");
    assert!(
        count_kernels(&proto) >= 1,
        "expected at least one kernel in the canonical loop"
    );
    assert!(
        proto.code.iter().any(|op| matches!(op, Op::LoopKernel(_))),
        "expected a LoopKernel op at the loop header"
    );
}

/// Deterministic fuzz: generate random numeric loops (random arithmetic,
/// comparisons, short-circuits, breaks, and occasional type pollution to
/// force guard bails) and require kernel-on/off equivalence on every one.
#[test]
fn kernel_fuzz_differential() {
    // Tiny LCG — the test must be deterministic (fixed seed, no host RNG).
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut rnd = move |n: u64| -> u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % n
    };

    for case in 0..300 {
        let nvars = 2 + rnd(3) as usize; // v0..v{n}
        let mut body = String::new();
        let stmts = 1 + rnd(5);
        for _ in 0..stmts {
            let v = rnd(nvars as u64);
            let w = rnd(nvars as u64);
            let konst = [0.0, 1.0, 2.5, -3.0, 7.0, 0.1, 1e9][rnd(7) as usize];
            let ops = ["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", ">>>"];
            let op = ops[rnd(ops.len() as u64) as usize];
            match rnd(6) {
                0 => body.push_str(&format!("v{v} = v{v} {op} v{w};\n")),
                1 => body.push_str(&format!("v{v} = v{w} {op} {konst};\n")),
                2 => body.push_str(&format!("v{v} += i {op} {konst};\n")),
                3 => {
                    let cmps = ["<", "<=", ">", ">=", "===", "!=="];
                    let c = cmps[rnd(6) as usize];
                    body.push_str(&format!("if (v{v} {c} v{w}) v{v} = v{w} {op} i;\n"));
                }
                4 => body.push_str(&format!("v{v} = i % 2 ? v{w} : -v{v};\n")),
                _ => body.push_str(&format!("v{v} = (v{v} {op} i) || v{w};\n")),
            }
        }
        // A few cases break early or pollute a variable with a string to
        // exercise exits and guard bails.
        if case % 11 == 0 {
            body.push_str("if (i === 13) break;\n");
        }
        if case % 17 == 0 {
            body.push_str(&format!("if (i === 7) v{} = 'x';\n", rnd(nvars as u64)));
        }
        let decls: Vec<String> = (0..nvars)
            .map(|v| format!("let v{v} = {};", [0, 1, -1, 42][v % 4]))
            .collect();
        let prints: Vec<String> = (0..nvars).map(|v| format!("v{v}")).collect();
        let src = format!(
            "{}\nfor (let i = 0; i < 25; i++) {{\n{body}}}\nconsole.log({});",
            decls.join(" "),
            prints.join(", ")
        );
        let with = run(&src, true);
        let without = run(&src, false);
        assert_eq!(with, without, "fuzz case {case} diverged:\n{src}");
    }
}

/// Kernels must not run under an op budget (exact per-op accounting), and the
/// budget must observe the SAME counts as the generic path.
#[test]
fn op_budget_identical_with_kernels() {
    let src = "let s = 0; for (let i = 0; i < 100; i++) { s += i; }";
    // Find the exact op count via a generous budget on a kernel-on engine.
    let mut probe = Engine::new();
    probe.vm.op_budget = Some(1_000_000);
    probe.eval(src).expect("runs");
    let used = 1_000_000 - probe.vm.op_budget.unwrap();

    // Exhaustion one op short must throw on BOTH compilations.
    for kernels in [true, false] {
        let proto = Rc::new(compile_script_kernels(src, kernels).expect("compiles"));
        let mut engine = Engine::new();
        engine.vm.op_budget = Some(used - 1);
        let func = engine.vm.make_closure(proto, Vec::new());
        let res = engine.vm.call(Value::Object(func), Value::Undefined, &[]);
        assert!(
            res.is_err(),
            "budget exhaustion must throw (kernels={kernels})"
        );
    }
}
