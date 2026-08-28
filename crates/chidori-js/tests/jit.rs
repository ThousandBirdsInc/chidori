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
    // The depth-limit corpus entries recurse to the budget, and the generic
    // rerun's ~per-call interpreter frames are debug-build large; the
    // default budget (2000) is calibrated for main-thread stacks, not the
    // test harness's. A smaller budget exercises the identical abandon /
    // RangeError paths within the corpus thread's stack.
    engine.vm.max_call_depth = 1000;
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
    // Dense-array element kernels (now compiled: reads/writes through the
    // shared fast-path core, bail edges on every miss).
    "const a = [1,2,3,4,5]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
    "const a = [5,4,3,2,1]; for (let i = 0; i < a.length; i++) { a[i] += i; } console.log(a.join(','));",
    "const x = [1,2,3,4], y = [10,20,30,40]; let d = 0; for (let i = 0; i < x.length; i++) { d += x[i] * y[i]; } console.log(d);",
    // Aliased bases: writes through one visible through the other.
    "const a = [1,2,3]; const b = a; let s = 0; for (let i = 0; i < a.length; i++) { b[i] = a[i] + 1; s += a[i]; } console.log(s, a.join(','));",
    // Holes (prototype consult), OOB reads, fractional indices, non-Number
    // elements: per-access bails must land identically.
    "Array.prototype[1] = 99; const a = [1,,3]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } delete Array.prototype[1]; console.log(s);",
    "const a = [1,,3]; for (let i = 0; i < a.length; i++) { a[i] = (a[i] || 0) + 1; } console.log(a.join(','), 1 in a);",
    "const a = [1,2]; let s = 0; for (let i = 0; i < 4; i++) { s += a[i] === undefined ? 100 : a[i]; } console.log(s);",
    "const a = [1,'x',3]; let s = ''; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
    "const a = [1,2,3]; a[1.5] = 7; let s = 0; for (let i = 0; i < 3; i += 0.5) { s += a[i] || 0; } console.log(s, a['1.5']);",
    // Store-side creation: hole fill and exact append (growth mid-loop).
    "const a = [0]; for (let i = 0; i < 20; i++) { a[a.length] = i; } console.log(a.length, a[20]);",
    // push/pop kernels (receiver re-checked per op).
    "const arr = []; for (let i = 0; i < 10; i++) { arr.push(i * i); } console.log(arr.length, arr[9]);",
    "const arr = [1,2,3,4,5]; let s = 0; for (let i = 0; i < 5; i++) { s = s * 10 + arr.pop(); } console.log(s, arr.length);",
    // Float64Array: the DIRECT-view path (in-bounds reads/writes) and its
    // bail edges (OOB, fractional index), plus a dot product and an
    // in-place transform over two views.
    "const t = new Float64Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 1.5; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t[7]);",
    "const a = new Float64Array(16), b = new Float64Array(16); for (let i = 0; i < 16; i++) { a[i] = i % 5; b[i] = i % 3; } let d = 0; for (let i = 0; i < a.length; i++) { d += a[i] * b[i]; } for (let i = 0; i < a.length; i++) { a[i] = (a[i] + b[i]) % 4; } console.log(d, a.join(','));",
    "const t = new Float64Array(4); let s = 0; for (let i = 0; i < 6; i++) { s += t[i] === undefined ? 100 : t[i]; t[i] = i; } console.log(s, t.join(','));",
    "const t = new Float64Array(4); t[0] = 1; let s = 0; for (let i = 0; i < 2; i += 0.5) { s += t[i] || 0; } console.log(s);",
    // NaN/-0 round-trip through typed storage, and Infinity.
    "const t = new Float64Array(3); t[0] = -0; t[1] = NaN; t[2] = Infinity; let c = 0; for (let i = 0; i < 3; i++) { const v = t[i]; if (Object.is(v, -0)) c += 1; if (v !== v) c += 10; if (v === Infinity) c += 100; } console.log(c);",
    // Int32Array: the DIRECT per-kind path with wrapping stores.
    "const t = new Int32Array(6); for (let i = 0; i < 6; i++) { t[i] = i * 1e9; } let s = 0; for (let i = 0; i < t.length; i++) { s += t[i]; } console.log(s, t.join(','));",
    // Every direct numeric kind: fill with values past the kind's range so
    // the store conversion (trunc + wraparound / f32 rounding) is exercised,
    // then read back and sum. One line per kind keeps failures attributable.
    "const t = new Int8Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 50 - 175.7; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    "const t = new Uint8Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 100 - 150.5; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    "const t = new Int16Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 20000 - 50000.3; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    "const t = new Uint16Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 30000 - 40000.9; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    "const t = new Uint32Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 3e9 - 5e9; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    "const t = new Float32Array(8); for (let i = 0; i < 8; i++) { t[i] = i * 0.1 + 1e-8; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    // Uint8ClampedArray: direct reads, but stores must take the shim (the
    // clamp — NaN→0, round-half-to-even, saturate — is not wraparound).
    "const t = new Uint8ClampedArray(8); for (let i = 0; i < 8; i++) { t[i] = i * 100 - 150.5; } let s = 0; for (let i = 0; i < 8; i++) { s += t[i]; } console.log(s, t.join(','));",
    "const t = new Uint8ClampedArray(4); t[0] = 0.5; t[1] = 1.5; t[2] = 2.5; t[3] = 300; let s = 0; for (let i = 0; i < 4; i++) { s = s * 1000 + t[i]; } console.log(s);",
    // The integer-store edges through the direct path: NaN and ±Infinity
    // encode as 0; huge-but-finite magnitudes hit the engine's i64
    // saturation before the element wrap.
    "const src = [NaN, Infinity, -Infinity, 1e300, -1e300, 2147483647.9, -2147483648.5, 4294967296]; const t = new Int32Array(8); const u = new Uint8Array(8); for (let i = 0; i < 8; i++) { t[i] = src[i]; u[i] = src[i]; } console.log(t.join(','), u.join(','));",
    // An Int32Array bit-mix loop (direct loads/stores + inlined ToInt32).
    "const t = new Int32Array(16); t[0] = 12345; for (let i = 1; i < 16; i++) { t[i] = (t[i-1] * 1103515245 + 12345) & 0x7fffffff; } let h = 0; for (let i = 0; i < 16; i++) { h = (h ^ t[i]) >>> 1; } console.log(h, t[15]);",
    // KIND-MISMATCH reactivation: the same function's kernel first compiles
    // against Int32Array (baking i32 sequences), then runs over a
    // Float64Array and a plain array — both must fail the baked-kind guard
    // and stay correct through the helper path.
    "function sumInto(t) { let s = 0; for (let i = 0; i < t.length; i++) { t[i] = i * 2.5; s += t[i]; } return s; } const a = sumInto(new Int32Array(6)); const b = sumInto(new Float64Array(6)); const c = sumInto([0, 0, 0, 0, 0, 0]); console.log(a, b, c);",
    // Two views of DIFFERENT kinds over one buffer in one kernel (aliased
    // element storage, distinct baked sequences per oslot).
    "const buf = new ArrayBuffer(16); const i32 = new Int32Array(buf); const u8 = new Uint8Array(buf); for (let i = 0; i < 4; i++) { i32[i] = i * 100000 + 7; } let s = 0; for (let i = 0; i < 16; i++) { s = s * 31 + u8[i]; } console.log(s % 1000000007, i32.join(','));",
    // Pinned-string kernels (StrLen/CharCodeAt — total, no bail): the
    // tokenizer hash idiom, plus NaN/OOB index handling.
    "const txt = 'kernel'; let s = 0; for (let i = 0; i < txt.length; i++) { s += txt.charCodeAt(i); } console.log(s);",
    "const txt = 'abcdef'; let h = 0; for (let r = 0; r < 5; r++) { for (let i = 0; i < txt.length; i++) { h = (h * 31 + txt.charCodeAt(i)) % 1000000007; } } console.log(h);",
    "const txt = 'xy'; let c = 0; for (let i = -1; i < 4; i++) { const v = txt.charCodeAt(i); c += v === v ? v : 1000; } console.log(c);",
    // Pinned-callee loops: the callee kernel INLINES into the compiled
    // caller (a global-resolved callee and an oslot-resolved closure).
    "function dbl(x) { return x * 2; } let s = 0; for (let i = 0; i < 100; i++) { s += dbl(i); } console.log(s);",
    "function adder(n) { return function (x) { return x + n; }; } let f = adder(5); let s = 0; for (let i = 0; i < 200; i++) { s = f(s) - 4; } f = adder(9); for (let i = 0; i < 50; i++) { s = f(s) - 8; } console.log(s);",
    // Identity guard: the SAME kernel re-activated with a DIFFERENT pinned
    // callee proto must decline the compiled code (interpreter tier) and
    // still compute identically; re-pinning the compiled proto re-enters it.
    "function add1(x) { return x + 1; } function trpl(x) { return x * 3; } function runWith(f, n) { let s = 1; for (let i = 0; i < n; i++) { s = f(s) % 97; } return s; } console.log(runWith(add1, 50), runWith(trpl, 50), runWith(add1, 7));",
    // A Math-using callee (entry-guard canonicals) inlined into the caller.
    "function clampP(x) { return Math.min(Math.max(x, -3), 3); } let s = 0; for (let i = -10; i < 10; i++) { s += clampP(i); } console.log(s);",
    // FUNCTION kernels through the frameless call path: plain scalar...
    "function cmp(a, b) { return a - b; } let s = 0; for (let i = 0; i < 50; i++) { s += cmp(i, 25); } console.log(s, [3,1,2].sort(cmp).join(''));",
    "function clamp(x) { return Math.min(Math.max(x, 0), 10); } let s = 0; for (let i = -5; i < 20; i++) { s += clamp(i); } console.log(s);",
    // ...boolean-returning...
    "function isEven(n) { return n % 2 === 0; } let c = 0; for (let i = 0; i < 20; i++) { if (isEven(i)) c++; } console.log(c, typeof isEven(2));",
    // ...cell-writing (uv_writes flush on the native path)...
    "function mkAcc() { let total = 0; return function (x) { total += x; return total; }; } const acc = mkAcc(); let last = 0; for (let i = 1; i <= 10; i++) { last = acc(i); } console.log(last);",
    // ...and recursive: SELF-only recursion compiles to a real native
    // recursive function (global and captured-binding self-references,
    // boolean returns, helpers in the body)...
    "function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); } console.log(fib(20));",
    "const gcd = (a, b) => b === 0 ? a : gcd(b, a % b); console.log(gcd(1071, 462), gcd(35, 64));",
    "const isE = n => n === 0 ? true : !isE(n - 1); console.log(isE(10), isE(7));",
    "function pow2(n) { return n === 0 ? 1 : 2 * pow2(n - 1); } function deep(n) { return n === 0 ? 0 : deep(n - 1) + 1; } console.log(pow2(20), deep(800));",
    // The depth budget: a too-deep recursion must abandon the native
    // activation and raise the SAME RangeError the generic path does.
    "function down(n) { return n === 0 ? 0 : down(n - 1) + 1; } try { console.log(down(1000000)); } catch (e) { console.log('deep', e instanceof RangeError); }",
    // Function-scoped MUTUAL recursion: the resolver pins the captured
    // partners into a family, compiled as mutually-calling native functions.
    "function isOdd(n) { return n === 0 ? false : isEvenM(n - 1); } function isEvenM(n) { return n === 0 ? true : isOdd(n - 1); } console.log(isEvenM(30), isOdd(17));",
    // A three-member family with mixed step sizes (mod-3 classifier).
    "function m0(n) { return n === 0 ? 0 : m2(n - 1); } function m2(n) { return n === 0 ? 2 : m1(n - 1); } function m1(n) { return n === 0 ? 1 : m0(n - 1); } let s = 0; for (let i = 0; i < 40; i++) { s = s * 3 + m0(i); s %= 1000003; } console.log(s);",
    // A recursive function calling a captured NON-recursive helper: the
    // helper joins the family as a plain member.
    "function stepDown(x) { return x - 2; } function count(n) { return n <= 0 ? 0 : count(stepDown(n)) + 1; } console.log(count(31), count(0));",
    // FAMILY IDENTITY: the captured callee binding is REASSIGNED between
    // activations — the per-activation re-verification must decline the
    // stale family/compiled code and still compute exactly.
    "let helper = n => n === 0 ? 0 : walk(n - 1) + 1; function walk(n) { return n <= 0 ? 0 : helper(n - 1) + 2; } console.log(walk(9)); helper = n => 100; console.log(walk(9));",
    // Mixed-return-type family (boolean entry, number partner): must
    // decline the family tiers and stay exactly right generically.
    "function evenish(n) { return n === 0 ? true : depth(n - 1) === 0; } function depth(n) { return n === 0 ? 0 : (evenish(n - 1) ? 1 : 2); } console.log(evenish(6), depth(5));",
    // Generator / async frames suspend AROUND kernel regions.
    "function* g() { let s = 0; for (let i = 0; i < 100; i++) { s += i; } yield s; for (let i = 0; i < 10; i++) { s -= i; } yield s; } const it = g(); console.log(it.next().value, it.next().value);",
    "(async () => { await 0; let s = 0; for (let i = 0; i < 50; i++) { s += i * i; } console.log('async', s); })();",
    // Deep expression chains (canonical stack-slot registers).
    "let s = 0; for (let i = 1; i < 20; i++) { s += ((i + 1) * (i + 2) - (i + 3)) / ((i % 5) + 1) + ((i << 2) ^ (i >> 1)); } console.log(s);",
    // A big single-expression body mixing every inlined op with helpers.
    "let s = 1; for (let i = 1; i < 60; i++) { s = (s * 1.000001 + i / 3 - Math.floor(i / 7)) % 1e9 + (i & 3) + (i % 11) ** 0.5; } console.log(s);",
];

/// The load-bearing gate: byte-identical observable behavior, tier on vs off.
/// Runs on a big-stack thread: the depth-limit corpus entry legitimately
/// recurses to `max_call_depth` (~2000 interpreter frames on the generic
/// rerun), which is calibrated for main-thread stacks, not the test
/// harness's smaller worker stacks.
#[test]
fn jit_matches_interpreter() {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            for src in CORPUS {
                let with_jit = run(src, true);
                let without = run(src, false);
                assert_eq!(
                    with_jit, without,
                    "JIT-on and JIT-off behavior diverged for:\n{src}"
                );
            }
        })
        .expect("spawn")
        .join()
        .expect("corpus thread");
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

/// Structural: a kernel outside the compiled subset (here a pinned-callee
/// loop whose callee WRITES a captured cell — per-call cell flushes belong
/// to the interpreter tier) declines translation, staying on the
/// interpreter tier rather than erroring — and still computes correctly.
#[test]
fn jit_declines_non_scalar_kernels() {
    let before = chidori_js::jit::stats();
    let (threw, console, _err) = run(
        "function mk() { let t = 0; return function (x) { t += x; return t; }; } const acc = mk(); let s = 0; for (let i = 0; i < 100; i++) { s = acc(i); } console.log(s);",
        true,
    );
    assert!(!threw);
    assert_eq!(console, vec!["4950".to_string()]);
    let after = chidori_js::jit::stats();
    assert!(
        after.declined > before.declined,
        "expected the cell-writing pinned-callee kernel to decline JIT translation"
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

/// Cooperative interrupt inside NATIVE RECURSION: a shallow-but-hot family
/// (fib with an argument that would run for hours) must still unwind
/// promptly — the per-call poll counter in the activation context, at the
/// interpreter's every-256-calls cadence.
#[test]
fn jit_recursion_interrupts() {
    let proto = Rc::new(
        compile_script(
            "(function () { function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); } fib(60); })();",
        )
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
    let err = res.expect_err("interrupt must unwind the recursion");
    assert!(
        engine.vm.error_to_string(&err).contains("interrupted"),
        "expected the interrupt error"
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
