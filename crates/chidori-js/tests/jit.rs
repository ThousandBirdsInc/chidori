//! Differential + structural tests for the Cranelift kernel JIT (`src/jit.rs`,
//! `jit` feature; see docs/cranelift-jit.md).
//!
//! The guarantee is the kernel tier's, one level down: the native tier is a
//! pure performance side effect. Every program must produce byte-identical
//! observable behavior with `Vm::jit_enabled` on and off — same console
//! output, same completion value, same thrown error. Kernels stay ON in both
//! runs (the compiled form is identical); only the execution backend differs,
//! so the corpus concentrates on what native code could plausibly get wrong:
//! exact numeric semantics (NaN, -0, infinities, `%`, `**`, `>>>`, shift
//! masking, float precision at the integer edge), every comparison polarity,
//! the fused superinstructions' skip semantics, `Math` intrinsics (both the
//! inlined IEEE ops and the helper-called ones), boolean-typed registers,
//! localized property registers, function kernels (plain, cell-writing,
//! recursive — the latter declining natively and staying correct), and the
//! decline boundary (element access, strings, pinned callees keep the
//! interpreter tier without changing results).

#![cfg(feature = "jit")]

use std::rc::Rc;

use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};

/// Run `src` inside a function (top-level script `let`s compile to stable
/// global-lexical cells, which never kernelize; function bindings localize
/// and kernelize exactly like production agent code) with the JIT tier on or
/// off, and capture the observable triple.
fn run(src: &str, jit: bool) -> (bool, Vec<String>, String) {
    let wrapped = format!("(function () {{ {src} }})();");
    let proto = Rc::new(compile_script(&wrapped).expect("compiles"));
    let mut engine = Engine::new();
    engine.vm.jit_enabled = jit;
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
    // The canonical counting loop, all four inlined IEEE kinds.
    "let s = 0; for (let i = 0; i < 1000; i++) { s += i * 2 - (i / 3); } console.log(s);",
    // `%` (helper call): sign of dividend, -0 results, fmod fallback.
    "let s = ''; for (let i = -6; i <= 6; i += 3) { s += (i % 4) + ','; } console.log(s, Object.is(-6 % 3, -0));",
    "let s = 0; for (let i = 1; i < 50; i++) { s += (i * 1.5) % 2.5; } console.log(s);",
    // `**` (helper call): spec special cases pow(±1, ±Inf), pow(x, ±0).
    "let p = 0; for (let i = 1; i < 6; i++) { p += 2 ** i; } console.log(p, (-1) ** Infinity, NaN ** 0);",
    // 32-bit family (helper calls): shift masking, >>> unsigned, ~ trunc.
    "let h = 123456789; for (let i = 0; i < 100; i++) { h = (h * 31 + i) | 0; } console.log(h);",
    "let u = 0; for (let i = 0; i < 40; i++) { u = (u + (1 << i)) >>> 0; } console.log(u);",
    "let b = 0; for (let i = 0; i < 10; i++) { b ^= ~i & (i >> 1) | (i >>> 2); } console.log(b);",
    "let seed = 987654321, c = 0; for (let r = 0; r < 50; r++) { seed = (seed * 1103515245 + 12345) >>> 0; c = (c + seed) % 4294967296; } console.log(c);",
    // ToInt32 far outside the i64-saturation range (2^80 mod 2^32 == 0).
    "let x = 2 ** 80, s = 0; for (let i = 0; i < 3; i++) { s += x | 0; s += (x + i) & 7; } console.log(s);",
    // Exact float semantics: NaN propagation, -0, infinities, 0.1 sums.
    "let x = 0; for (let i = 0; i < 10; i++) { x += 0.1; } console.log(x);",
    "let s = 0; for (let i = -5; i < 5; i++) { s += 1 / i; } console.log(s, 1 / s);",
    "let z = 0; for (let i = 0; i < 3; i++) { z = -0 * i + z; } console.log(Object.is(z, -0), Object.is(z, 0));",
    "let n = 0; for (let i = 0; i < 4; i++) { n = n + (i === 2 ? NaN : i); } console.log(n, n === n);",
    // Negation vs subtraction from zero: -(-0) and unary on NaN.
    "let s = 0; for (let i = 0; i < 4; i++) { s = -(s + i); } console.log(s, Object.is(-0, -(0)));",
    // Precision at the MAX_SAFE_INTEGER edge.
    "let s = 9007199254740980; for (let i = 0; i < 20; i++) { s += 1; } console.log(s);",
    // Every comparison, both branch polarities, NaN in all of them.
    "let c = 0; for (let i = 0; i < 20; i++) { if (i >= 10) c++; if (i <= 3) c--; if (i === 5) c += 100; if (i !== 5) c += 2; if (i > 15) c += 7; if (i < 2) c += 11; } console.log(c);",
    "let c = 0, x = NaN; for (let i = 0; i < 5; i++) { if (x < i) c += 1; if (x >= i) c += 2; if (x === x) c += 4; if (x !== x) c += 8; } console.log(c);",
    // Comparison MATERIALIZED into a boolean register + boolean local + `!`.
    "let flag = false, c = 0; for (let i = 0; i < 20; i++) { flag = i % 3 === 0; if (flag) c++; flag = !flag; if (flag) c += 10; } console.log(c, flag);",
    "let odd = false, n = 0; for (let i = 0; i < 9; i++) { odd = !odd; n += odd ? 1 : 100; } console.log(n, odd);",
    // Short-circuits / ternaries (peek-jump shapes -> BrTruthy/BrFalsy).
    "let s = 0; for (let i = 0; i < 30; i++) { s += i % 2 && i; } console.log(s);",
    "let s = 1; for (let i = 0; i < 10; i++) { s = i || s; } console.log(s);",
    "let s = 0; for (let i = 0; i < 25; i++) { s += i > 12 ? i * 2 : -i; } console.log(s);",
    // The fused superinstructions: `s += a op b`, `i += k; continue`,
    // per-iteration `let` copies (Mov2 landing pads), `cell op const`.
    "let s = 0; for (let i = 0; i < 200; i++) { s += i * i; } console.log(s);",
    "let a = 1, b = 2, s = 0; for (let i = 0; i < 10; i++) { a *= 1.5; b -= 0.25; s = a + b + s; } console.log(s);",
    "let a = 0, b = 1; for (let i = 0; i < 30; i++) { const t = a + b; a = b; b = t; } console.log(a, b);",
    "let s = 0; for (let i = 0, j = 10; i < j; i++, j--) { s += i * j; } console.log(s);",
    // while / do-while shapes, break / continue / labels, infinite+break.
    "let i = 0, s = 0; while (i < 500) { s = (s + i) | 0; i++; } console.log(s);",
    "let i = 10, n = 0; do { n += i; i--; } while (i > 0); console.log(n);",
    "let s = 0; for (let i = 0; i < 100; i++) { if (i === 7) continue; if (i > 20) break; s += i; } console.log(s);",
    "let s = 0; outer: for (let i = 0; i < 10; i++) { for (let j = 0; j < 10; j++) { if (i * j > 30) break outer; s += j; } } console.log(s);",
    "let i = 0; for (;;) { i++; if (i > 100) break; } console.log(i);",
    // Nested numeric loops (one outer kernel).
    "let s = 0; for (let i = 0; i < 50; i++) { for (let j = 0; j < 50; j++) { s += i ^ j; } } console.log(s);",
    // Zero-iteration / single-iteration / empty bodies.
    "let s = 0; for (let i = 0; i < 0; i++) { s += i; } console.log(s);",
    "for (let i = 0; i < 3; i++) {} console.log('done');",
    // Math intrinsics — the INLINED IEEE ones...
    "let s = 0; for (let i = -20; i < 20; i++) { s += Math.abs(i) + Math.floor(i / 3) + Math.ceil(i / 7) + Math.trunc(i / 2); } console.log(s);",
    "let s = 0; for (let i = 0; i < 100; i++) { s += Math.sqrt(i); } console.log(s, Math.sqrt(-1));",
    // ...and the helper-called ones (round half-up, sign of -0, fround,
    // min/max NaN + -0 ordering, imul wraparound, pow specials).
    "let s = 0; for (let i = 0; i < 10; i++) { s += Math.round(i + 0.5) + Math.round(-i - 0.5) + Math.sign(i - 5); } console.log(s, Object.is(Math.round(-0.4), -0));",
    "let s = 0; for (let i = 0; i < 20; i++) { s += Math.fround(i * 0.1); } console.log(s);",
    "let m = 0; for (let i = 0; i < 10; i++) { m += Math.min(i, 5) + Math.max(i, 5); } console.log(m, Object.is(Math.min(0, -0), -0), Math.max(NaN, 1));",
    "let s = 0; for (let i = 0; i < 10; i++) { s = (s + Math.imul(i, 0x7fffffff)) | 0; s += Math.pow(i, 2); } console.log(s, Math.pow(-1, Infinity));",
    // Loop-carried compare operand reassigned mid-loop.
    "let n = 10, s = 0; for (let i = 0; i < n; i++) { if (i === 5) n = 8; s += 1; } console.log(s);",
    // GUARD BAIL / LATE ENTRY: a binding warms from undefined; a string
    // taints the loop-carried value. The tier must decline/re-enter exactly
    // like the interpreter tier (guards run before the seam either way).
    "let t; let s = 0; for (let i = 0; i < 10; i++) { t = (t || 0) + i; s = t; } console.log(s, t);",
    "let s = 0; for (let i = 0; i < 5; i++) { if (i === 3) s = '' + s; s += i; } console.log(s);",
    // Localized property registers (o.x rewritten to register Movs; the
    // entry/exit slot load/write-back is the caller's, shared with the JIT).
    "const o = { v: 0, w: 100 }; for (let i = 0; i < 50; i++) { o.v += i; o.w -= i; } console.log(o.v, o.w);",
    "const p = { hi: 0 }; let s = 0; for (let i = 0; i < 30; i++) { p.hi = p.hi + i; s += p.hi; } console.log(s, p.hi);",
    // DECLINED kernels (element access, strings, pinned callees): the
    // interpreter tier owns them; results must be identical anyway.
    "const a = [1,2,3,4,5]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
    "const a = [5,4,3,2,1]; for (let i = 0; i < a.length; i++) { a[i] += i; } console.log(a.join(','));",
    "let s = 0; const txt = 'kernel'; for (let i = 0; i < txt.length; i++) { s += txt.charCodeAt(i); } console.log(s);",
    "const arr = []; for (let i = 0; i < 10; i++) { arr.push(i * i); } console.log(arr.length, arr[9]);",
    "function dbl(x) { return x * 2; } let s = 0; for (let i = 0; i < 100; i++) { s += dbl(i); } console.log(s);",
    // FUNCTION kernels through the frameless call path: plain scalar...
    "function cmp(a, b) { return a - b; } let s = 0; for (let i = 0; i < 50; i++) { s += cmp(i, 25); } console.log(s, [3,1,2].sort(cmp).join(''));",
    "function clamp(x) { return Math.min(Math.max(x, 0), 10); } let s = 0; for (let i = -5; i < 20; i++) { s += clamp(i); } console.log(s);",
    // ...boolean-returning...
    "function isEven(n) { return n % 2 === 0; } let c = 0; for (let i = 0; i < 20; i++) { if (isEven(i)) c++; } console.log(c, typeof isEven(2));",
    // ...cell-writing (uv_writes flush on the native path)...
    "function mkAcc() { let total = 0; return function (x) { total += x; return total; }; } const acc = mkAcc(); let last = 0; for (let i = 1; i <= 10; i++) { last = acc(i); } console.log(last);",
    // ...and recursive (SelfCall declines translation; windowed executor).
    "function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); } console.log(fib(20));",
    "function isOdd(n) { return n === 0 ? false : isEvenM(n - 1); } function isEvenM(n) { return n === 0 ? true : isOdd(n - 1); } console.log(isEvenM(30), isOdd(17));",
    // Generator / async frames suspend AROUND kernel regions.
    "function* g() { let s = 0; for (let i = 0; i < 100; i++) { s += i; } yield s; for (let i = 0; i < 10; i++) { s -= i; } yield s; } const it = g(); console.log(it.next().value, it.next().value);",
    "(async () => { await 0; let s = 0; for (let i = 0; i < 50; i++) { s += i * i; } console.log('async', s); })();",
    // Deep expression chains (canonical stack-slot registers).
    "let s = 0; for (let i = 1; i < 20; i++) { s += ((i + 1) * (i + 2) - (i + 3)) / ((i % 5) + 1) + ((i << 2) ^ (i >> 1)); } console.log(s);",
    // A big single-expression body mixing every inlined op with helpers.
    "let s = 1; for (let i = 1; i < 60; i++) { s = (s * 1.000001 + i / 3 - Math.floor(i / 7)) % 1e9 + (i & 3) + (i % 11) ** 0.5; } console.log(s);",
];

/// The load-bearing gate: byte-identical observable behavior, tier on vs off.
#[test]
fn jit_matches_interpreter() {
    for src in CORPUS {
        let with_jit = run(src, true);
        let without = run(src, false);
        assert_eq!(
            with_jit, without,
            "JIT-on and JIT-off behavior diverged for:\n{src}"
        );
    }
}

/// Structural: the corpus actually exercises the native tier — kernels
/// compile and native activations run. (Counters are process-global and
/// monotonic, so a before/after delta is safe under parallel tests.)
#[test]
fn jit_actually_compiles_and_runs() {
    let before = chidori_js::jit::stats();
    let (threw, console, _err) = run(
        "let s = 0; for (let i = 0; i < 10000; i++) { s += i * 2 - (i % 3); } console.log(s);",
        true,
    );
    assert!(!threw);
    assert_eq!(console, vec!["99980001".to_string()]);
    let after = chidori_js::jit::stats();
    assert!(
        after.compiled > before.compiled,
        "expected the counting loop's kernel to compile natively"
    );
    assert!(
        after.native_runs > before.native_runs,
        "expected at least one native kernel activation"
    );
}

/// Structural: an element-access kernel declines translation (stays on the
/// interpreter tier) rather than erroring — and still computes correctly.
#[test]
fn jit_declines_non_scalar_kernels() {
    let before = chidori_js::jit::stats();
    let (threw, console, _err) = run(
        "const a = [1,2,3,4]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
        true,
    );
    assert!(!threw);
    assert_eq!(console, vec!["10".to_string()]);
    let after = chidori_js::jit::stats();
    assert!(
        after.declined > before.declined,
        "expected the element-access kernel to decline JIT translation"
    );
}

/// The tier is OFF by default even when compiled in: a fresh engine must not
/// touch the native tier. Asserted on the kernel's own compile-once cache
/// (untouched means the seam never ran) — the process-global counters are
/// shared with parallel tests and unusable for an equality check.
#[test]
fn jit_off_by_default() {
    let proto = Rc::new(
        compile_script(
            "(function () { let s = 0; for (let i = 0; i < 1000; i++) { s += i; } console.log(s); })();",
        )
        .expect("compiles"),
    );
    let mut engine = Engine::new();
    let func = engine.vm.make_closure(proto.clone(), Vec::new());
    let res = engine.vm.call(Value::Object(func), Value::Undefined, &[]);
    assert!(res.is_ok());
    assert_eq!(engine.console(), ["499500"]);
    // The loop's kernel lives on the IIFE's nested proto.
    let inner = proto
        .consts
        .iter()
        .find_map(|c| match c {
            chidori_js::bytecode::Const::Func(p) => Some(p.clone()),
            _ => None,
        })
        .expect("the IIFE proto");
    assert!(!inner.kernels.is_empty(), "the loop should kernelize");
    assert!(
        inner.kernels.iter().all(|k| k.native.get().is_none()),
        "the native tier must stay untouched with jit_enabled unset"
    );
}

/// Cooperative interrupt: a JIT-compiled infinite loop must still unwind
/// promptly when the flag is set from another thread — the native back-edge
/// poll at the interpreter's cadence.
#[test]
fn jit_loop_interrupts() {
    let proto = Rc::new(
        compile_script("(function () { let i = 0; for (;;) { i = (i + 1) % 1000000007; } })();")
            .expect("compiles"),
    );
    let mut engine = Engine::new();
    engine.vm.jit_enabled = true;
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    engine.vm.interrupt = Some(flag.clone());
    let setter = {
        let flag = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    };
    let func = engine.vm.make_closure(proto, Vec::new());
    let res = engine.vm.call(Value::Object(func), Value::Undefined, &[]);
    setter.join().expect("setter thread");
    let err = res.expect_err("interrupt must unwind the loop");
    assert!(
        engine.vm.error_to_string(&err).contains("interrupted"),
        "expected the interrupt error"
    );
}
