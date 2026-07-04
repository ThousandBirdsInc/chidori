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
    // ---- dense-array element access (kernel v2) ----
    // Read loop with `a.length` condition.
    "const a = [1,2,3,4,5]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
    // In-place write loop; values visible after.
    "const a = [1,2,3]; for (let i = 0; i < a.length; i++) { a[i] = a[i] * 10 + i; } console.log(a.join(','));",
    // Compound element update, reversed iteration, index arithmetic.
    "const a = [5,4,3,2,1]; let s = 0; for (let i = a.length - 1; i >= 0; i--) { a[i] += i; s += a[i]; } console.log(s, a.join(','));",
    // Dot product across two arrays.
    "const x = [1,2,3,4], y = [10,20,30,40]; let d = 0; for (let i = 0; i < x.length; i++) { d += x[i] * y[i]; } console.log(d);",
    // ALIASED bases: writes through one visible through the other.
    "const a = [1,2,3]; const b = a; let s = 0; for (let i = 0; i < a.length; i++) { b[i] = a[i] + 1; s += a[i]; } console.log(s, a.join(','));",
    // Nested indexing a[b[i]].
    "const idx = [2,0,1], v = [10,20,30]; let s = 0; for (let i = 0; i < idx.length; i++) { s = s * 100 + v[idx[i]]; } console.log(s);",
    // HOLES: element read falls back to the prototype chain.
    "Array.prototype[1] = 99; const a = [1,,3]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } delete Array.prototype[1]; console.log(s);",
    // Hole WRITE (creates a property — non-extensible interactions aside,
    // plain arrays fill the hole; kernel must bail and match).
    "const a = [1,,3]; for (let i = 0; i < a.length; i++) { a[i] = (a[i] || 0) + 1; } console.log(a.join(','), 1 in a);",
    // Out-of-bounds read (undefined -> NaN via arithmetic; loop bound lies).
    "const a = [1,2]; let s = 0; for (let i = 0; i < 4; i++) { s += a[i] === undefined ? 100 : a[i]; } console.log(s);",
    // Non-number elements: strings force per-access bail; result exact.
    "const a = [1,'x',3]; let s = ''; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s);",
    // Float / negative / huge indices take the generic path mid-kernel.
    "const a = [1,2,3]; a[1.5] = 7; let s = 0; for (let i = 0; i < 3; i += 0.5) { s += a[i] || 0; } console.log(s, a['1.5']);",
    "const a = [9]; let s = 0; for (let i = -1; i < 2; i++) { s += a[i] || 0; } console.log(s);",
    // Frozen / sealed arrays: reified props make every access bail; sloppy
    // writes are silently ignored, exactly like the generic path.
    "const a = Object.freeze([1,2,3]); let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; a[i] = 0; } console.log(s, a.join(','));",
    "const a = Object.seal([1,2,3]); for (let i = 0; i < a.length; i++) { a[i] = a[i] * 2; } console.log(a.join(','));",
    // Accessor element (defineProperty): getter must fire on every read.
    "const a = [1,2,3]; let got = 0; Object.defineProperty(a, 1, { get() { got++; return 50; } }); let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } console.log(s, got);",
    // Array GROWTH inside the loop (appends bail; length re-read each pass).
    "const a = [1]; for (let i = 0; i < a.length && i < 5; i++) { a[a.length] = a[i] + 1; } console.log(a.join(','));",
    // Base REASSIGNED inside the loop: translation must reject (store to a
    // base local) and the generic loop must swap arrays mid-flight.
    "let a = [1,2,3,4]; const b = [100,200,300,400]; let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; if (i === 1) a = b; } console.log(s);",
    // The base local holding a non-array (typed later): guard declines.
    "let a = 5; let s = 0; for (let i = 0; i < 3; i++) { s += a; } console.log(s);",
    // Fully-kernelized nested loops inside a function (incl. 2D array walk).
    "function grid(n) { let m = 0; for (let i = 0; i < n; i++) { for (let j = 0; j < n; j++) { m += i ^ j; } } return m; } console.log(grid(20));",
    "function sum2d(g) { let s = 0; for (let i = 0; i < g.length; i++) { const row = g[i]; for (let j = 0; j < row.length; j++) { s += row[j]; } } return s; } console.log(sum2d([[1,2],[3,4],[5,6]]));",
    // Element values feeding branches and short-circuits.
    "const a = [0,1,NaN,3]; let c = 0; for (let i = 0; i < a.length; i++) { if (a[i]) c += 1; c += a[i] || 10; } console.log(c);",
    // Writing NaN/-0/Infinity through the kernel store.
    "const a = [0,0,0]; for (let i = 0; i < 3; i++) { a[i] = i === 0 ? -0 : i === 1 ? NaN : 1/0; } console.log(Object.is(a[0], -0), a[1] !== a[1], a[2]);",
    // ---- Math intrinsics (kernel v3) ----
    // The supported set, exercised across sign/NaN/-0/half-way edges.
    "let s = 0; for (let i = -5; i < 6; i++) { s += Math.abs(i) + Math.max(i, 2) + Math.min(i, -2); } console.log(s);",
    "let s = ''; for (let i = 0; i < 5; i++) { const x = i - 2.5; s += Math.round(x) + '/' + Math.floor(x) + '/' + Math.ceil(x) + '/' + Math.trunc(x) + ';'; } console.log(s);",
    "console.log((() => { let r = 0; for (let i = 0; i < 4; i++) { r += Math.round(-0.5 - i) * 2 + Math.sign(i - 2); } return r; })());",
    "let z = 0; for (let i = 0; i < 3; i++) { z = Math.min(0, -0) + Math.max(0, -0) + z; } console.log(Object.is(Math.min(0,-0), -0), Object.is(Math.max(-0,0), 0), z);",
    "let s = 0; for (let i = 1; i <= 10; i++) { s += Math.sqrt(i) + Math.pow(i, 1.5); } console.log(s);",
    "let h = 0; for (let i = 0; i < 200; i++) { h = (Math.imul(h, 31) + i) | 0; } console.log(h);",
    "let f = 0; for (let i = 0; i < 8; i++) { f += Math.fround(0.1 * i); } console.log(f);",
    "let n = 0; for (let i = 0; i < 4; i++) { n += Math.max(i === 2 ? NaN : i, 1); } console.log(n, n === n);",
    // Math constants fold (non-writable, non-configurable on the canonical).
    "let c = 0; for (let i = 0; i < 5; i++) { c += Math.PI * i + Math.E - Math.LN2 + Math.SQRT2 * Math.LOG2E; } console.log(c);",
    // Math mixed with array access (bail shapes carrying Math entries).
    "const a = [3,-1,4,-1,5]; let s = 0; for (let i = 0; i < a.length; i++) { s += Math.abs(a[i]); } console.log(s);",
    "const a = [1,'x',3]; let s = 0; for (let i = 0; i < a.length; i++) { s += Math.max(+a[i] || 0, 1); } console.log(s);",
    // MONKEYPATCHED Math.max: the guard must decline and run the patch.
    "let calls = 0; const orig = Math.max; Math.max = function(a, b) { calls++; return orig(a, b) + 100; }; let s = 0; for (let i = 0; i < 3; i++) { s += Math.max(i, 1); } Math.max = orig; console.log(s, calls);",
    // Math REPLACED wholesale / deleted mid-program.
    "const RealMath = Math; let s = 0; globalThis.Math = { max: () => 7 }; for (let i = 0; i < 3; i++) { s += Math.max(i, 1); } globalThis.Math = RealMath; console.log(s);",
    // Patch BETWEEN two activations of the same kernelized function.
    "function f() { let s = 0; for (let i = 0; i < 3; i++) { s += Math.abs(i - 1); } return s; } const first = f(); const orig = Math.abs; Math.abs = () => 42; const second = f(); Math.abs = orig; console.log(first, second);",
    // Accessor on globalThis.Math must fire per LoadGlobal (guard declines).
    "const RealMath = Math; let gets = 0; Object.defineProperty(globalThis, 'Math', { get() { gets++; return RealMath; }, configurable: true }); let s = 0; for (let i = 0; i < 3; i++) { s += Math.abs(-i); } Object.defineProperty(globalThis, 'Math', { value: RealMath, writable: true, configurable: true }); console.log(s, gets >= 3);",
    // Unsupported arities / methods stay generic but correct.
    "let s = 0; for (let i = 0; i < 4; i++) { s += Math.max(i, 1, 2) + Math.min(i) + Math.hypot(i, 4); } console.log(s);",
    "let s = 0; for (let i = 1; i < 5; i++) { s += Math.log(i) + Math.atan2(i, 2); } console.log(s);",
    // ---- in-body block-scoped declarations (TDZ-init elision) ----
    // Multiple consts per iteration, chained.
    "function f(n) { let s = 0; for (let i = 0; i < n; i++) { const a = i * 2; const b = a + 1; s += b - a; } return s; } console.log(f(50));",
    // A REAL TDZ read on one path must still throw identically (region has
    // a conditional read before the init -> stays generic).
    "try { for (let i = 1; i >= 0; i--) { if (i === 0) { y; } const y = i; } } catch (e) { console.log('tdz', e.constructor.name); }",
    // const inside the loop feeding Math and array access.
    "const arr = [4,1,3,2]; let s = 0; for (let i = 0; i < arr.length; i++) { const v = arr[i]; s += Math.max(v, 2); } console.log(s);",
    // ---- materialized booleans (kernel v4) ----
    // A stored comparison stays a BOOLEAN across write-back (typeof!).
    "let ok = false; for (let i = 0; i < 10; i++) { ok = i > 4; } console.log(ok, typeof ok);",
    "function f(n) { let even = true; for (let i = 0; i < n; i++) { even = !even; } return [even, typeof even]; } console.log(f(7).join(','), f(8).join(','));",
    // Boolean fed to arithmetic coerces to 0/1; result is a NUMBER.
    "let c = 0; for (let i = 0; i < 20; i++) { const big = i >= 10; c += big; } console.log(c, typeof c);",
    // Strict equality between a boolean and a number is ALWAYS false.
    "let hits = 0; for (let i = 0; i < 5; i++) { const t = i < 3; if (t === 1) hits += 100; if (t == 1) hits += 1; if (t === true) hits += 10; } console.log(hits);",
    "let s = 0; for (let i = 0; i < 6; i++) { const b = i % 2 === 0; s += b !== 1 ? 1 : 0; s += b === (i < 100) ? 10 : 0; } console.log(s);",
    // !x / !!x chains on numbers and booleans.
    "let n = 0; for (let i = -3; i < 4; i++) { n += !i ? 100 : 0; n += !!i ? 1 : 0; } console.log(n);",
    // Boolean as ARRAY INDEX reads the \"true\"/\"false\" properties, not 0/1.
    "const a = [10, 20]; a[true] = 77; let s = 0; for (let i = 0; i < 4; i++) { const b = i % 2 === 1; s += a[b] || 0; } console.log(s, a['true']);",
    // Boolean STORED INTO an element must stay a boolean element.
    "const a = [0, 0, 0]; for (let i = 0; i < 3; i++) { a[i] = i > 0; } console.log(a.join(','), typeof a[0], typeof a[2]);",
    // Late entry: undefined -> boolean transition.
    "let f; let c = 0; for (let i = 0; i < 6; i++) { if (f) c++; f = i % 2 === 0; } console.log(c, typeof f);",
    // Loop-carried toggle driving control flow.
    "let flip = false, s = 0; for (let i = 0; i < 9; i++) { if (flip) s += i; flip = !flip; } console.log(s, flip);",
    // Booleans crossing an exit shape (break with a live comparison).
    "let last = false; for (let i = 0; i < 10; i++) { last = i > 7; if (last) break; } console.log(last, typeof last);",
    // Boolean local compared with relational operators (coerces to 0/1).
    "let s = 0; for (let i = 0; i < 6; i++) { const b = i > 2; if (b < 1) s += 1; if (b >= 1) s += 10; } console.log(s);",
    // A local that is SOMETIMES number, sometimes boolean: stays generic.
    "let m = 0; for (let i = 0; i < 6; i++) { m = i % 2 ? (i > 2) : i; } console.log(m, typeof m);",
    // ---- dense appends (kernel v4) ----
    // The classic fill-by-append loop.
    "const a = []; for (let i = 0; i < 100; i++) { a[a.length] = i * 2; } console.log(a.length, a[0], a[99]);",
    // Fill of a `new Array(n)` (in-bounds hole fills).
    "const a = new Array(50); for (let i = 0; i < a.length; i++) { a[i] = i * i; } console.log(a.length, a[49], 0 in a);",
    // Append with the length read live in the condition (bounded growth).
    "const a = [1]; for (let i = 0; i < a.length && a.length < 20; i++) { a[a.length] = a[i] + 1; } console.log(a.length, a.join(','));",
    // Append blocked by preventExtensions (generic path: silent in sloppy).
    "const a = [1]; Object.preventExtensions(a); for (let i = 0; i < 3; i++) { a[a.length] = 9; } console.log(a.length, a.join(','));",
    // ... and throwing in strict mode.
    "'use strict'; const a = [1]; Object.preventExtensions(a); try { for (let i = 0; i < 3; i++) { a[a.length] = 9; } } catch (e) { console.log('strict', e.constructor.name); } console.log(a.length);",
    // Append far past the end leaves holes (generic path).
    "const a = []; for (let i = 0; i < 4; i++) { a[i * 2] = i; } console.log(a.length, 1 in a, a.join('|'));",
    // ---- function kernels (frameless tiny callees) ----
    // The canonical sort comparators, ascending/descending/ternary/bitmask.
    "const a = [5,3,8,1,9,2,7]; a.sort((x, y) => x - y); console.log(a.join(','));",
    "const a = [5,3,8,1,9,2,7]; a.sort((x, y) => y - x); console.log(a.join(','));",
    "const a = [3,1,2,1,3]; a.sort((x, y) => x < y ? -1 : x > y ? 1 : 0); console.log(a.join(','));",
    "const f = (a, b) => (a & 15) - (b & 15); const xs = [30, 7, 22, 13]; xs.sort(f); console.log(xs.join(','));",
    // A BOOLEAN-returning comparator (ToNumber(true/false) inside sort).
    "const f = (a, b) => a > b; console.log([3,1,2].sort(f).join(','), f(1, 2), typeof f(1, 2));",
    // Declared function with a body-local temporary.
    "function f(a, b) { const d = a - b; return d; } console.log(f(9, 4), typeof f(9, 4), f(0.5, 0.25));",
    // HOF callbacks: map / filter / reduce / every / findIndex.
    "const f = (x) => x % 2 === 0; console.log([1,2,3,4,5,6].filter(f).join(','));",
    "console.log([1,2,3].map((x) => x * x + 1).join(','));",
    "console.log([1,2,3,4].reduce((acc, v) => acc + v * 2, 0));",
    "console.log([2,4,6].every((x) => x % 2 === 0), [1,2].findIndex((x) => x > 1));",
    // Captured numeric upvalue (guarded snapshot) — and a non-number one
    // (guard declines; the generic path concatenates).
    "const k = 3; console.log([1,2,3].map(x => x * k).join(','));",
    "let k = 'no'; const g = x => x + k; console.log(g(1));",
    // Missing / extra / non-number arguments decline per CALL, not per fn.
    "function f(a, b) { return a + b; } console.log(f(1), f(1, 2), f(1, 2, 3), f('x', 'y'));",
    // Math intrinsics inside a function kernel; then monkeypatched (decline).
    "const f = x => Math.abs(x) + Math.max(x, 0); console.log(f(-5), f(5));",
    "const orig = Math.abs; Math.abs = () => 42; const f = x => Math.abs(x); console.log(f(-5)); Math.abs = orig;",
    // Boolean returns and typeof pins.
    "const f = x => !x; console.log(f(0), f(1), f(NaN), typeof f(0));",
    "const f = n => n !== n; console.log(f(NaN), f(1), typeof f(NaN));",
    // Exact float semantics through the frameless path: -0 and NaN.
    "const f = (x, y) => x * y; console.log(Object.is(f(-0, 5), -0), Object.is(f(0, -5), -0), f(NaN, 1) !== f(NaN, 1) === false);",
    "const a = [1e-9, -0, 0, 2]; a.sort((x, y) => 1 / x - 1 / y); console.log(a.map(v => Object.is(v, -0) ? '-0' : String(v)).join(','));",
    // Short-circuit predicate (&& across blocks) and a loop INSIDE the body.
    "const f = x => x > 0 && x < 10; console.log(f(5), f(-1), f(20), typeof f(5));",
    "function f(x) { let s = 0; for (let i = 0; i < x; i++) s += i; return s; } console.log(f(10), f(0), f(1));",
    // Recursion is a call op — never kernelized, still correct.
    "function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); } console.log(fact(6));",
    // Comparator called through .call / .apply (generic entry paths).
    "const f = (a, b) => a - b; console.log(f.call(null, 5, 2), f.apply(null, [5, 2]));",
    // `new` on a kernel-carrying function takes [[Construct]] (an object!).
    "function f(a, b) { return a - b; } const o = new f(1, 2); console.log(typeof o, f(1, 2));",
    // Default parameter: supplied-numbers kernelize; the default fires only
    // via the guard-declined generic call.
    "const f = (a, b = 10) => a - b; console.log(f(3, 1), f(3));",
    // Strict-mode body (different `this` prologue shape).
    "'use strict'; function f(a, b) { return a * b; } console.log(f(6, 7));",
    // Boolean ARGUMENT declines (guard is Number-only) — generic coercion.
    "const f = (a, b) => a - b; console.log(f(true, 1), f(5, false));",
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

/// Array-access loops and function-level nested loops actually kernelize
/// (pins v2 eligibility against silent regressions).
#[test]
fn array_and_nested_loops_get_kernels() {
    fn kernels_in(src: &str) -> usize {
        fn count(p: &FuncProto) -> usize {
            let mut n = p.kernels.len();
            for c in &p.consts {
                if let Const::Func(f) = c {
                    n += count(f);
                }
            }
            n
        }
        count(&compile_script(src).expect("compiles"))
    }
    // `s += a[i]` with an `a.length` bound: one kernel.
    assert!(
        kernels_in("function f(a) { let s = 0; for (let i = 0; i < a.length; i++) { s += a[i]; } return s; }") >= 1,
        "array read loop must kernelize"
    );
    // In-place write loop: one kernel.
    assert!(
        kernels_in("function f(a) { for (let i = 0; i < a.length; i++) { a[i] = a[i] * 2; } }")
            >= 1,
        "array write loop must kernelize"
    );
    // Nested numeric loops in a function: TWO kernels (inner on its own, and
    // the outer subsuming it via the inner header's fallback).
    assert!(
        kernels_in("function g(n) { let m = 0; for (let i = 0; i < n; i++) { for (let j = 0; j < n; j++) { m += i ^ j; } } return m; }") >= 2,
        "nested loops must both kernelize"
    );
}

/// Boolean, append, and captured-bound loops actually kernelize (pins v4).
#[test]
fn v4_loops_get_kernels() {
    fn kernels_in(src: &str) -> usize {
        fn count(p: &FuncProto) -> usize {
            let mut n = p.kernels.len();
            for c in &p.consts {
                if let Const::Func(f) = c {
                    n += count(f);
                }
            }
            n
        }
        count(&compile_script(src).expect("compiles"))
    }
    // Materialized boolean local (stored comparison + toggle + branch).
    assert!(
        kernels_in("function f(n) { let flip = false, s = 0; for (let i = 0; i < n; i++) { const hi = i > 5; if (hi !== flip) s++; flip = !hi; } return s; }") >= 1,
        "boolean loop must kernelize"
    );
    // Append-fill loop.
    assert!(
        kernels_in("function f(n) { const a = []; for (let i = 0; i < n; i++) { a[a.length] = i; } return a.length; }") >= 1,
        "append loop must kernelize"
    );
    // Loop bound captured from the enclosing scope (upvalue snapshot).
    assert!(
        kernels_in("const N = 100; function f() { let s = 0; for (let i = 0; i < N; i++) { s += i; } return s; } f();") >= 1,
        "upvalue-bound loop must kernelize"
    );
}

/// Tiny pure-scalar functions get a FUNCTION kernel (frameless call path) —
/// and frame-dependent bodies never do (pins fn-kernel eligibility).
#[test]
fn tiny_functions_get_fn_kernels() {
    fn fn_kernels(p: &FuncProto) -> usize {
        let mut n = usize::from(p.fn_kernel.is_some());
        for c in &p.consts {
            if let Const::Func(f) = c {
                n += fn_kernels(f);
            }
        }
        n
    }
    for src in [
        "const f = (a, b) => a - b;",
        "const f = (a, b) => a < b ? -1 : a > b ? 1 : 0;",
        "const f = x => x % 2 === 0;",
        "const f = x => Math.abs(x) + Math.max(x, 0);",
        "const k = 2; const f = x => x * k;",
        "function f(a, b) { const d = a - b; return d; }",
        "const f = x => x > 0 && x < 10;",
    ] {
        let proto = compile_script(src).expect("compiles");
        assert!(
            fn_kernels(&proto) >= 1,
            "expected a function kernel in {src:?}"
        );
    }
    for src in [
        // `arguments` needs a real frame.
        "function f() { return arguments.length; }",
        // Property access needs a frame to bail into.
        "const f = (a) => a.length;",
        // Calls (incl. recursion) are off the allowlist.
        "function f(a, b) { return f(a) - b; }",
        // Allocation is off the allowlist.
        "const f = (a) => [a];",
        // Falling off the end returns `undefined` — not a scalar `Return`.
        "const f = (a) => { a - 1; };",
    ] {
        let proto = compile_script(src).expect("compiles");
        assert_eq!(
            fn_kernels(&proto),
            0,
            "expected NO function kernel in {src:?}"
        );
    }
}

/// Math-using loops actually kernelize (pins v3 eligibility).
#[test]
fn math_loops_get_kernels() {
    let proto = compile_script(
        "function f(n) { let s = 0; for (let i = 0; i < n; i++) { s += Math.max(Math.abs(i - 5), 1) * Math.PI; } return s; }",
    )
    .expect("compiles");
    fn count(p: &FuncProto) -> usize {
        let mut n = p.kernels.len();
        for c in &p.consts {
            if let Const::Func(f) = c {
                n += count(f);
            }
        }
        n
    }
    assert!(count(&proto) >= 1, "Math loop must kernelize");
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
        // Some cases carry a BOOLEAN variable: stored comparisons, negation,
        // use in conditions and arithmetic (differential must preserve
        // boolean-ness through write-back).
        if case % 4 == 0 {
            let v = rnd(nvars as u64);
            let w = rnd(nvars as u64);
            match rnd(3) {
                0 => body.push_str(&format!("bfz = v{v} < v{w};\n")),
                1 => body.push_str(&format!("bfz = !bfz; if (bfz) v{v} += 1;\n")),
                _ => body.push_str(&format!("v{v} += bfz; bfz = v{w} % 2 === 0;\n")),
            }
        }
        // Some cases route a value through a supported Math intrinsic.
        if case % 5 == 0 {
            let v = rnd(nvars as u64);
            let w = rnd(nvars as u64);
            let m = [
                "Math.abs(v{W})",
                "Math.max(v{W}, i)",
                "Math.min(v{W}, 10)",
                "Math.floor(v{W} / 3)",
                "Math.imul(v{W}, 7)",
                "Math.round(v{W} * 0.3)",
            ][rnd(6) as usize];
            body.push_str(&format!("v{v} = {};\n", m.replace("{W}", &w.to_string())));
        }
        // A third of the cases mix in dense-array reads/writes (in-bounds,
        // hole-adjacent, and occasionally out-of-bounds — all must bail or
        // fast-path to identical results).
        let use_array = case % 3 == 0;
        if use_array {
            let v = rnd(nvars as u64);
            match rnd(4) {
                0 => body.push_str(&format!("v{v} += arr[i % arr.length];\n")),
                1 => body.push_str(&format!("arr[i % arr.length] = v{v} + i;\n")),
                2 => body.push_str(&format!("v{v} = arr[i] === undefined ? 1 : arr[i];\n")),
                _ => body.push_str(&format!(
                    "arr[i % arr.length] += v{v}; v{v} = arr[(i + 1) % arr.length];\n"
                )),
            }
        }
        let decls: Vec<String> = (0..nvars)
            .map(|v| format!("let v{v} = {};", [0, 1, -1, 42][v % 4]))
            .collect();
        let prints: Vec<String> = (0..nvars).map(|v| format!("v{v}")).collect();
        let arr_decl = if use_array {
            match case % 9 {
                0 => "const arr = [3,,7,1];\n",         // holey
                3 => "const arr = [0.5, 2, 'k', 4];\n", // string element
                _ => "const arr = [2,4,6,8,10];\n",
            }
        } else {
            ""
        };
        let arr_print = if use_array { ", arr.join('|')" } else { "" };
        let bool_decl = if case % 4 == 0 {
            "let bfz = false; "
        } else {
            ""
        };
        let bool_print = if case % 4 == 0 {
            ", bfz, typeof bfz"
        } else {
            ""
        };
        let src = format!(
            "{}\n{bool_decl}{arr_decl}for (let i = 0; i < 25; i++) {{\n{body}}}\nconsole.log({}{arr_print}{bool_print});",
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
    for src in [
        "let s = 0; for (let i = 0; i < 100; i++) { s += i; }",
        // Call-heavy: function kernels must also be disabled under a budget.
        "const f = (a, b) => a - b; let s = 0; for (let i = 0; i < 40; i++) { s += f(i, 2 * i); }",
    ] {
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
}
