//! Differential + structural tests for the experimental closure-threading JIT
//! (`src/jit.rs`).
//!
//! The core guarantee mirrors the fusion pass's (`tests/fusion.rs`): the JIT is
//! a **pure performance side effect**. Running a program with the JIT ON
//! (`Vm::jit_enabled = true`, the default) must produce byte-identical
//! *observable* behavior to running it with the JIT OFF (the reference switch
//! interpreter) — same console output, same thrown error (or lack of one), same
//! ordering. This is the toggle-equivalence property the determinism contract
//! (`docs/interpreter-optimization.md` §7.3) demands of any optimization.
//!
//! The corpus deliberately spans the whole language surface, not just the ops
//! the JIT specializes: a divergence in a *specialized* op (arithmetic,
//! comparison, cell/local access, branch) OR in the `step`-delegating fallback
//! path (calls, property access, generators, async, classes, `with`, `super`,
//! exceptions, iteration) would fail loudly here.

use std::rc::Rc;

use chidori_js::bytecode::Const;
use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};

/// Compile `src`, run it to completion (draining microtasks) with the JIT
/// `jit`, and return its observable outcome: `(threw, console_lines, error)`.
fn run(src: &str, jit: bool) -> (bool, Vec<String>, String) {
    let proto = Rc::new(compile_script(src).expect("compiles"));
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

/// Broad corpus exercising both the specialized fast paths and the
/// `step`-delegating long tail through the JIT driver.
const CORPUS: &[&str] = &[
    // --- numeric loops: the specialized arithmetic/compare/branch core ---
    "let s = 0; for (let i = 0; i < 20000; i++) { s += i * 2 - (i % 3); } console.log(s);",
    "function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); } console.log(fib(20));",
    "let p = 1; for (let i = 1; i <= 15; i++) p *= i; console.log(p);",
    // --- every binary/unary/bitwise op (coercion must match exactly) ---
    "console.log(7 / 2, 7 % 3, 2 ** 10, -5, +'-3', ~6, !0, !'');",
    "console.log(5 & 3, 5 | 2, 5 ^ 1, 1 << 4, -8 >> 1, -8 >>> 28);",
    "console.log(1 < 2, 2 <= 2, 3 > 4, 4 >= 4, 1 == '1', 1 === '1', 1 != 2, 1 !== '1');",
    // --- mixed-type coercion & toPrimitive ordering ---
    "let o = { valueOf() { return 10; } }; console.log(o + 5, o * 2, o < 11, `${o}`);",
    "console.log('2' == 2, '2' === 2, null == undefined, NaN === NaN, 0 === -0, 1/0, -1/0, 0/0);",
    // --- BigInt arithmetic ---
    "let b = 2n; for (let i = 0n; i < 10n; i++) b *= 2n; console.log(b.toString());",
    // --- typeof on every value kind ---
    "console.log(typeof 1, typeof 'x', typeof true, typeof undefined, typeof null, typeof {}, typeof function(){}, typeof Symbol(), typeof 1n);",
    // --- cells / closures / upvalue capture and mutation ---
    "function adder(n){ return x => x + n; } let f = adder(5), s = 0; for (let i = 0; i < 1000; i++) s = f(s) - 4; console.log(s);",
    "let fns = []; for (let i = 0; i < 5; i++) fns.push(() => i); console.log(fns.map(g => g()).join(','));",
    // --- TDZ: reading a let before init throws identically ---
    "try { console.log(x); let x = 1; } catch (e) { console.log('tdz', e.constructor.name); }",
    "function g(){ return y; let y; } try { g(); } catch (e) { console.log('tdz2', e instanceof ReferenceError); }",
    // --- objects / arrays / property access (fallback path) ---
    "let o = { a: 0, b: 0, c: 0 }; for (let i = 0; i < 5000; i++) { o.a = i; o.b = o.a + 1; o.c = o.b + o.a; } console.log(o.c);",
    "let a = []; for (let i = 0; i < 2000; i++) a.push(i); console.log(a.map(x => x*x).filter(x => x%2===0).reduce((p,c)=>p+c,0));",
    "let m = new Map(); m.set('k', 1); let st = new Set([1,2,2,3]); console.log(m.get('k'), st.size, [...st].join(''));",
    // --- strings / templates / coercion ---
    "let s = ''; for (let i = 0; i < 300; i++) s += 'x' + i; console.log(s.length, s.slice(0,5));",
    "let n = 42, t = 'hi'; console.log(`${t} ${n} ${n > 40 ? 'big' : 'small'} ${[1,2,3].join('-')}`);",
    // --- control flow: try/catch/finally, switch, labeled break/continue ---
    "let s = 0; for (let i = 0; i < 6; i++) { try { if (i % 2 === 0) throw i; s += i; } catch (e) { console.log('caught', e); } finally { console.log('fin', i); } } console.log('s', s);",
    "for (let i = 0; i < 4; i++) { switch (i) { case 0: console.log('zero'); break; case 1: case 2: console.log('onetwo', i); break; default: console.log('def', i); } }",
    "outer: for (let i = 0; i < 5; i++) { for (let j = 0; j < 5; j++) { if (i*j >= 6) break outer; if (j === i) continue; console.log(i, j); } }",
    // --- optional chaining / short circuit / nullish ---
    "let o = { a: { b: 3 } }; console.log(o?.a?.b, o?.x?.y, o?.a?.b ?? 99, o?.x?.y ?? 99, (1>0) && 'yes' || 'no');",
    // --- for-of / for-in / iterators ---
    "let sum = 0; for (const x of [1,2,3,4,5]) sum += x; console.log(sum);",
    "let o = { a: 1, b: 2, c: 3 }; let ks = []; for (const k in o) ks.push(k); console.log(ks.join(','));",
    "function* gen(){ yield 1; yield 2; yield* [3,4]; return 5; } console.log([...gen()].join(','));",
    // --- destructuring (array + object + defaults + rest) ---
    "let [a, b = 9, ...rest] = [1, undefined, 3, 4]; let { x, y: yy = 7 } = { x: 5 }; console.log(a, b, rest.join(''), x, yy);",
    // --- classes / inheritance / super / private fields ---
    "class A { #v = 1; get v(){ return this.#v; } inc(){ this.#v++; return this; } } class B extends A { constructor(){ super(); } both(){ return this.v; } } let o = new B(); o.inc().inc(); console.log(o.both());",
    // --- exceptions thrown mid-expression (coercion error site) ---
    "let s = Symbol(); try { console.log(1 + s); } catch (e) { console.log('symerr', e.constructor.name); }",
    "try { null.x; } catch (e) { console.log('npe', e instanceof TypeError); }",
    // --- async / await / promises / microtask ordering ---
    "async function f(){ let s = 0; for (let i = 0; i < 5; i++) s += await Promise.resolve(i); return s; } f().then(v => console.log('async', v));",
    "Promise.resolve(1).then(v => console.log('a', v)); Promise.resolve(2).then(v => console.log('b', v)); console.log('sync');",
    // --- recursion depth + arguments object ---
    "function sum(){ let t = 0; for (let i = 0; i < arguments.length; i++) t += arguments[i]; return t; } console.log(sum(1,2,3,4,5));",
    // --- Math / number formatting via fallback ops ---
    "console.log(Math.max(3,7,2), Math.min(3,7,2), Math.floor(3.7), Math.abs(-4), (255).toString(16), parseInt('ff', 16));",
];

#[test]
fn jit_matches_interpreter() {
    for (n, src) in CORPUS.iter().enumerate() {
        let off = run(src, false);
        let on = run(src, true);
        assert_eq!(
            off, on,
            "JIT changed observable behavior for corpus[{n}]:\n  {src}\n  interp={off:?}\n  jit={on:?}"
        );
    }
}

#[test]
fn jit_compiles_and_specializes_hot_ops() {
    // A tight numeric loop should compile to a thread the same length as its
    // bytecode, with the hot loop body (loads, arithmetic, compare-and-branch)
    // specialized rather than delegated to `step`.
    let proto =
        compile_script("let s = 0; for (let i = 0; i < 100; i++) { s += i * 2 - (i % 3); }")
            .expect("compiles");
    assert!(!proto.jit.is_compiled(), "fresh proto starts uncompiled");
    let thread = proto.jit.get_or_compile(&proto);
    assert!(proto.jit.is_compiled(), "cache populated after compile");
    assert_eq!(
        thread.op_count(),
        proto.code.len(),
        "thread is index-parallel to bytecode (jump targets carry over unchanged)"
    );
    assert!(
        thread.specialized > 0,
        "the numeric loop body must specialize hot ops, got {} specialized / {} fallback",
        thread.specialized,
        thread.fallback
    );
    // For this arithmetic-only loop, the large majority of ops are specialized.
    assert!(
        thread.specialized >= thread.fallback,
        "expected mostly-specialized thread for a numeric loop, got {} specialized / {} fallback",
        thread.specialized,
        thread.fallback
    );
}

#[test]
fn jit_caches_per_proto() {
    let proto = compile_script("function f(n){ return n + 1; } f(1);").expect("compiles");
    let a = proto.jit.get_or_compile(&proto);
    let b = proto.jit.get_or_compile(&proto);
    assert!(
        Rc::ptr_eq(&a, &b),
        "second compile must return the cached thread, not recompile"
    );
}

/// The toggle must genuinely select a backend: with it off, no proto in the run
/// gets compiled; with it on, the entry proto does. (Behavioral equivalence is
/// covered by `jit_matches_interpreter`; this asserts the switch is real.)
#[test]
fn jit_toggle_selects_backend() {
    let src = "let s = 0; for (let i = 0; i < 50; i++) s += i; s;";

    // JIT off: the entry proto's cache stays empty after a full run.
    let proto_off = Rc::new(compile_script(src).expect("compiles"));
    let mut e = Engine::new();
    e.vm.jit_enabled = false;
    let f = e.vm.make_closure(proto_off.clone(), Vec::new());
    e.vm.call(Value::Object(f), Value::Undefined, &[]).unwrap();
    assert!(
        !proto_off.jit.is_compiled(),
        "JIT-off run must not compile the proto"
    );

    // JIT on: the entry proto gets compiled during the run.
    let proto_on = Rc::new(compile_script(src).expect("compiles"));
    let mut e = Engine::new();
    e.vm.jit_enabled = true;
    let f = e.vm.make_closure(proto_on.clone(), Vec::new());
    e.vm.call(Value::Object(f), Value::Undefined, &[]).unwrap();
    assert!(
        proto_on.jit.is_compiled(),
        "JIT-on run must compile the entry proto"
    );
}

/// Sanity: each proto (top-level and nested) owns an independent, lazily-filled
/// JIT cache — compiling the parent does not eagerly compile its children.
#[test]
fn nested_protos_compile_independently() {
    let proto = compile_script("function f(n){ return n*n; } for (let i = 0; i < 3; i++) f(i);")
        .expect("compiles");
    let nested = proto
        .consts
        .iter()
        .find_map(|c| match c {
            Const::Func(p) => Some(p.clone()),
            _ => None,
        })
        .expect("has a nested function proto");

    // Compiling the parent leaves the child untouched...
    assert!(!nested.jit.is_compiled());
    let _parent = proto.jit.get_or_compile(&proto);
    assert!(
        !nested.jit.is_compiled(),
        "parent compile must not touch child"
    );

    // ...and the child compiles on its own when asked.
    let child = nested.jit.get_or_compile(&nested);
    assert!(nested.jit.is_compiled());
    assert_eq!(child.op_count(), nested.code.len());
}
