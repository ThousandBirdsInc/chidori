//! Differential + structural tests for the register-bytecode tier (`reg.rs`,
//! docs/js-performance-roadmap.md §3.5).
//!
//! The guarantee: register execution is a pure optimization. Running any
//! program with the register tier on must produce byte-identical observable
//! behavior — same console output, same thrown error — as the stack
//! interpreter. The corpus concentrates on exactly what the translation must
//! never break: virtual-stack shuffles (ternaries, logical operators,
//! optional chains, method calls, compound assignments), TDZ checks and the
//! init dataflow that elides them, the shared inline-cache paths, dense-array
//! element fast paths, every call shape (plain, method, native, bound,
//! proxy, construct, spread, function-kernel callees, recursion at the depth
//! limit), closures and per-iteration capture, `arguments` aliasing, the
//! try/catch/finally handler machinery (rethrow, nested finallys,
//! return-in-finally, break/continue crossing finally regions, for-of and
//! destructuring iterator close), and the decline boundaries (`with`,
//! direct eval, generators, real awaits, `using`) where the stack tier
//! keeps the frame.

use std::rc::Rc;

use chidori_js::bytecode::{Const, FuncProto};
use chidori_js::compiler::{compile_script, compile_script_regs};
use chidori_js::{Engine, Value};

fn run(src: &str, regs: bool) -> (bool, Vec<String>, String) {
    let proto = Rc::new(compile_script_regs(src, regs).expect("compiles"));
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
    // ---- virtual-stack shuffles ----
    // Ternaries nested in call arguments (branch joins mid-expression).
    "function f(a, b) { return (a > b ? a : b) + (a < b ? 'x' : 'y'); } console.log(f(1, 2), f(2, 1));",
    // Logical operators: peek-branches keep/pop the value per edge.
    "console.log(0 || 'a', 1 && 'b', null ?? 'c', 'd' ?? 'e', 0 && 'f', '' || (() => 'g')());",
    // Logical assignment operators.
    "let a = 0, b = 1, c = null; a ||= 10; b &&= 20; c ??= 30; console.log(a, b, c);",
    // Optional chains: short-circuit leaves undefined; calls included.
    "const o = { m() { return { n: 1 }; } }; console.log(o.m?.().n, o.x?.y?.z, o.q?.());",
    // Comma/sequence expressions and chained assignment.
    "let x, y, z; x = y = z = (1, 2, 3); console.log(x, y, z);",
    // Compound assignment on properties (Dup/GetProp/SetProp shuffle).
    "const p = { n: 1 }; p.n += 2; p.n *= 3; p['n'] -= 4; console.log(p.n);",
    // Method-call fusion window (`Dup; GetProp; Swap`) on every receiver
    // kind: local, this, temp result, constant-ish.
    "const arr = [3, 1, 2]; console.log(arr.sort().join('-'), [4, 5].concat(6).length, 'ab'.toUpperCase());",
    "const obj = { v: 2, m() { return this.v; }, chain() { return this.m() + this.m(); } }; console.log(obj.chain());",
    // Swap without the fusion window (computed method key).
    "const k = 'm'; const o2 = { m(v) { return v * 2; } }; console.log(o2[k](21));",
    // Deeply nested call expressions (argument ranges stay contiguous).
    "function add(a, b) { return a + b; } console.log(add(add(1, add(2, 3)), add(add(4, 5), 6)));",

    // ---- TDZ and the init dataflow ----
    "try { t1; } catch (e) { console.log('read', e.constructor.name); } let t1 = 1; console.log(t1);",
    "try { t2 = 5; } catch (e) { console.log('write', e.constructor.name); } let t2; console.log(t2);",
    // Conditional TDZ: initialized on one path only — the check must stay.
    "function ct(f) { if (f) { var r; let v = 1; r = v; } try { { let w; if (f) w = 1; console.log('w', w); } } catch (e) { console.log(e.constructor.name); } } ct(true); ct(false);",
    // Loop-carried init: reads inside the body are provably initialized.
    "let s0 = 0; for (let i = 0; i < 5; i++) { s0 += i + i * 2; } console.log(s0);",
    // const assignment through the checked-store path.
    "const cc = 1; try { cc = 2; } catch (e) { console.log(e.constructor.name); } console.log(cc);",
    // TDZ in a closure-captured cell (upvalue checked path).
    "try { (() => tz)(); } catch (e) { console.log('cell', e.constructor.name); } let tz = 9; console.log((() => tz)());",

    // ---- property access / inline caches ----
    // Monomorphic own-property get/set loop (IC hits).
    "const po = { a: 0, b: 0 }; for (let i = 0; i < 20; i++) { po.a = i; po.b = po.a + 1; } console.log(po.a, po.b);",
    // IC invalidation: same site sees different shapes.
    "function rd(o) { return o.v; } console.log(rd({ v: 1 }), rd({ w: 0, v: 2 }), rd(Object.create({ v: 3 })));",
    // Prototype-holder IC (method lookup pattern) + shadowing.
    "class C { constructor() { this.n = 1; } m() { return this.n; } } const ci = new C(); let t = 0; for (let i = 0; i < 10; i++) t += ci.m(); ci.m = () => 100; console.log(t + ci.m());",
    // Accessors through the IC-miss path; setter observation order.
    "const log = []; const ao = { get g() { log.push('get'); return 1; }, set s(v) { log.push('set' + v); } }; ao.s = ao.g + 1; console.log(log.join(','));",
    // Strict-mode write failures (frozen object, non-writable).
    "'use strict'; const fo = Object.freeze({ a: 1 }); try { fo.a = 2; } catch (e) { console.log('frozen', e.constructor.name); } console.log(fo.a);",
    // Sloppy silent failure on frozen.
    "const fs = Object.freeze({ a: 1 }); fs.a = 2; console.log(fs.a);",
    // Getter that THROWS mid-expression (register state unwinds cleanly).
    "const gt = { get boom() { throw new Error('bang'); } }; try { const v = 1 + gt.boom + 2; } catch (e) { console.log('caught', e.message); } console.log('after');",
    // delete: named, dynamic, strict failure.
    "const dl = { a: 1, b: 2 }; console.log(delete dl.a, delete dl['b'], dl.a, 'a' in dl);",
    "'use strict'; try { delete Object.prototype; } catch (e) { console.log('del', e.constructor.name); }",
    // `in` operator and Symbol keys.
    "const sym = Symbol('s'); const so = { [sym]: 1 }; console.log(sym in so, 'x' in so, so[sym]);",

    // ---- dense-array element fast paths ----
    "const da = [1, 2, 3]; da[1] = 20; da[3] = 40; let ds = 0; for (let i = 0; i < da.length; i++) ds += da[i]; console.log(ds, da.length);",
    // Holes read through the prototype; OOB reads.
    "const ha = [1, , 3]; Array.prototype[1] = 99; console.log(ha[1], ha[5]); delete Array.prototype[1];",
    // Sealed array rejects appends (generic path owns semantics).
    "const sa = Object.seal([1, 2]); sa[2] = 3; console.log(sa.length); 'use strict';",
    "'use strict'; const sb = Object.seal([1]); try { sb[1] = 2; } catch (e) { console.log('seal', e.constructor.name); }",
    // Float/negative/string keys take the spec path.
    "const fk = [1, 2]; fk[-1] = 'neg'; fk[0.5] = 'half'; fk['01'] = 'pad'; console.log(fk[-1], fk[0.5], fk['01'], fk.length);",

    // ---- calls: every shape ----
    // Plain, methodless, natives, bound, proxied.
    "function pf(a, b, c) { return a + b * c; } console.log(pf(1, 2, 3), pf(1, 2), pf());",
    "const bf = function (a, b) { return this.v + a + b; }.bind({ v: 10 }, 1); console.log(bf(2));",
    "const px = new Proxy(function (a) { return a * 2; }, { apply(t, _th, args) { return t(...args) + 1; } }); console.log(px(21));",
    // Constructors: New, NewSpread, class ctor without new throws.
    "function Ctor(v) { this.v = v; } console.log(new Ctor(7).v, new Ctor(...[8]).v);",
    "class K { constructor() {} } try { K(); } catch (e) { console.log('ctor', e.constructor.name); }",
    // Spread calls with custom iterator (observable iteration order).
    "const it = { *[Symbol.iterator]() { yield 1; yield 2; } }; function sc(a, b) { return a + b; } console.log(sc(...it));",
    // `this` binding: sloppy boxes primitives, strict does not.
    "function sloppyThis() { return typeof this; } console.log(sloppyThis(), sloppyThis.call('s'), sloppyThis.call(null));",
    "'use strict'; function strictThis() { return this === undefined ? 'undef' : typeof this; } console.log(strictThis(), strictThis.call('s'));",
    // new.target with and without new.
    "function nt() { return new.target === undefined ? 'plain' : 'new'; } console.log(nt(), new nt() instanceof nt ? 'new' : '?');",
    // arguments object: mapped aliasing both directions (reg frame, cells).
    "function ma(p) { arguments[0] = 9; const before = p; p = 3; return before + ',' + arguments[0] + ',' + arguments.length; } console.log(ma(1, 2));",
    // rest params + defaults evaluated per call.
    "function rp(a = 1, ...rest) { return a + rest.length; } console.log(rp(), rp(5), rp(5, 6, 7));",
    // Not-callable error through the generic path.
    "try { (undefined)(); } catch (e) { console.log('call', e.constructor.name); } try { ({}).nope(); } catch (e) { console.log('method', e.constructor.name); }",
    // Function-kernel callee (comparator) invoked from a reg frame.
    "const ks = [5, 1, 4, 2, 3].sort((a, b) => a - b); console.log(ks.join(''));",

    // ---- closures / cells / per-iteration capture ----
    "const fns = []; for (let i = 0; i < 3; i++) fns.push(() => i); console.log(fns.map(f => f()).join(','));",
    "function counter() { let n = 0; return { inc() { return ++n; }, dec() { return --n; } }; } const c1 = counter(); c1.inc(); c1.inc(); console.log(c1.inc(), c1.dec());",
    // Captured accumulator mutated through an upvalue (checked stores).
    "let acc = 0; const bump = () => { acc += 2; }; for (let i = 0; i < 4; i++) bump(); console.log(acc);",
    // Deep transitive capture.
    "function ta() { let v = 'deep'; return () => () => v; } console.log(ta()()());",

    // ---- globals ----
    "var gv = 1; function gf() { return gv + 1; } gv = 5; console.log(gf(), typeof gundef, typeof gv);",
    "try { missing_global; } catch (e) { console.log('ref', e.constructor.name); }",
    "'use strict'; try { undeclared_w = 1; } catch (e) { console.log('sw', e.constructor.name); }",
    "sloppy_global = 42; console.log(sloppy_global);",

    // ---- literals / templates / regexp ----
    // Object literals: computed keys, methods, accessors, __proto__.
    "const base = { kind: 'base' }; const ol = { __proto__: base, a: 1, ['c' + 'k']: 2, m() { return 3; }, get g() { return 4; }, set g(v) {} }; console.log(ol.kind, ol.a, ol.ck, ol.m(), ol.g);",
    // Object/array spread and rest destructuring (object pattern).
    "const src = { a: 1, b: 2, c: 3 }; const { a: ra, ...rest } = src; const merged = { ...src, d: 4 }; console.log(ra, JSON.stringify(rest), JSON.stringify(merged));",
    "const asp = [0, ...[1, 2], 3, ...'ab']; console.log(asp.join('|'), asp.length);",
    // Array holes / elisions.
    "const el = [1, , 3]; console.log(el.length, 1 in el, el[1]);",
    // Tagged templates: frozen template object identity across calls.
    "function tag(s) { return s === tag.last ? 'same' : ((tag.last = s), 'fresh'); } function go() { return tag`a${1}b`; } console.log(go(), go());",
    // Template literal concatenation with coercing parts.
    "const tv = { toString() { return 'T'; } }; console.log(`x=${1 + 1} ${tv} ${'s'}`);",
    // Anonymous function naming from computed keys.
    "const key = 'named'; const nf = { [key]: function () {} }; console.log(nf[key].name);",
    // RegExp literal construction + use.
    "const re = /a(b+)c/; console.log(re.test('abbc'), 'xabbcx'.match(re)[1]);",

    // ---- control flow without handlers ----
    "let sw = ''; for (let i = 0; i < 4; i++) { switch (i) { case 0: sw += 'z'; break; case 2: { let q = 't'; sw += q; break; } default: sw += i; } } console.log(sw);",
    "let lb = ''; outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j === 2) continue outer; if (i === 2) break outer; lb += `${i}${j},`; } } console.log(lb);",
    "let wl = 0, dw = 0; while (wl < 5) wl += 2; do { dw += 3; } while (dw < 9); console.log(wl, dw);",
    // for-in: enumeration order, shadowing, nested.
    "const fi = { b: 1, a: 2 }; Object.defineProperty(fi, 'h', { value: 3, enumerable: false }); let ks2 = ''; for (const k in fi) ks2 += k; for (const k in [10, 20]) ks2 += k; console.log(ks2);",

    // ---- operators / coercions ----
    "console.log(1 + '2', '3' * '4', 5 % 3, 2 ** 10, 7 / 2 | 0, -'8', +true, ~1, !0);",
    "console.log(1 < '2', 'a' < 'b', null == undefined, null === undefined, NaN === NaN, 0 === -0);",
    "console.log(3n + 4n, 2n ** 10n, typeof 5n); let bi = 1n; bi++; console.log(bi);",
    // valueOf ordering in binary ops and updates.
    "const ord = []; const va = { valueOf() { ord.push('a'); return 1; } }, vb = { valueOf() { ord.push('b'); return 2; } }; console.log(va < vb, va + vb, ord.join(''));",
    "let mv = { valueOf() { mv = 100; return 7; } }; mv++; console.log(mv);",
    // instanceof with Symbol.hasInstance.
    "class HI { static [Symbol.hasInstance](v) { return v === 42; } } console.log(42 instanceof HI, 41 instanceof HI);",
    // typeof on every kind.
    "console.log(typeof 1, typeof 's', typeof true, typeof undefined, typeof null, typeof {}, typeof [], typeof (() => 0), typeof Symbol(), typeof 1n);",
    // String building via += (rope path through op_add).
    "let sb2 = ''; for (let i = 0; i < 50; i++) sb2 += 'ab'; console.log(sb2.length, sb2.slice(0, 4), sb2.charCodeAt(99));",

    // ---- classes (no extends — those decline) ----
    "class Pt { constructor(x, y) { this.x = x; this.y = y; } dist() { return Math.abs(this.x) + Math.abs(this.y); } static of(x, y) { return new Pt(x, y); } get sum() { return this.x + this.y; } } const pt = Pt.of(-1, 4); console.log(pt.dist(), pt.sum);",

    // ---- decline boundaries: identical behavior via the stack tier ----
    "function* gg() { yield 1; yield 2; } console.log([...gg()].join(','));",
    "(async () => { const v = await Promise.resolve(41); console.log('await', v + 1); })();",
    "function ev() { let v = 3; return eval('v * 7'); } console.log(ev());",
    "with ({ wv: 5 }) { console.log(wv); }",

    // ---- try/catch/finally (register-mode handler machinery) ----
    // Plain catch, rethrow, nested handlers, error identity.
    "let tf1 = ''; try { tf1 += 'a'; throw new Error('x'); tf1 += 'never'; } catch (e) { tf1 += 'b' + e.message; } finally { tf1 += 'c'; } console.log(tf1);",
    "try { try { throw new TypeError('inner'); } catch (e) { throw new RangeError('re:' + e.message); } } catch (e) { console.log(e.constructor.name, e.message); }",
    "let nst = ''; try { try { nst += '1'; throw 'x'; } finally { nst += '2'; } } catch (e) { nst += '3' + e; } console.log(nst);",
    // finally runs on the normal path; completion value flows through.
    "function fn1() { try { return 'ret'; } finally { console.log('fin'); } } console.log(fn1());",
    // return-in-finally OVERRIDES the parked completion.
    "function fn2() { try { return 1; } finally { return 2; } } console.log(fn2());",
    "function fn3() { try { throw new Error('gone'); } finally { return 'swallowed'; } } console.log(fn3());",
    // break/continue crossing one and two finally regions (CompletionJump).
    "let bc = ''; for (let i = 0; i < 4; i++) { try { if (i === 2) break; if (i === 1) continue; bc += i; } finally { bc += 'f' + i; } } console.log(bc);",
    "let bc2 = ''; outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { try { if (j === 1) continue outer; bc2 += `${i}${j}`; } finally { bc2 += 'f'; } } } console.log(bc2);",
    // throw from a nested CALL caught by this frame's handler.
    "function boom() { throw new Error('deep'); } let ct2 = ''; try { boom(); } catch (e) { ct2 = 'caught ' + e.message; } console.log(ct2);",
    // catch binding shadowing + TDZ interaction inside catch.
    "let cb = 'outer'; try { throw 'in'; } catch (cb2) { let inner = cb2 + '!'; console.log(cb, inner); } console.log(cb);",
    // Exception value replaces mid-expression register state cleanly.
    "const trap = { get g() { throw 'mid'; } }; let ms = 0; try { ms = 1 + trap.g + 100; } catch (e) { ms = 'e:' + e; } console.log(ms);",
    // Conditional TDZ observed FROM catch code (conservative handler-entry set).
    "function ctz(f) { try { if (f) throw 'early'; } catch (e) { try { tv; console.log('init?'); } catch (e2) { console.log('tdz', e2.constructor.name); } } let tv = 1; return tv; } console.log(ctz(true));",
    // Deep finally chain unwinding a return.
    "function deep() { try { try { try { return 'r'; } finally { console.log('f1'); } } finally { console.log('f2'); } } finally { console.log('f3'); } } console.log(deep());",
    // while(true) whose ONLY exit crosses a finally (declines translation —
    // parked-jump-only label — and must behave identically on the stack tier).
    "function wt() { let n = 0; while (true) { try { n++; if (n > 2) break; } finally { n += 10; } } return n; } console.log(wt());",

    // ---- for-of / destructuring (iterator-close landing pads) ----
    "let fo3 = ''; for (const v of [10, 20, 30]) { fo3 += v + ','; } console.log(fo3);",
    "let fo4 = 0; for (const v of [1, 2, 3, 4, 5]) { if (v === 4) break; fo4 += v; } console.log(fo4);",
    // Custom iterator observes return() on break and NOT on exhaustion.
    "const seen = []; function mkIt(n) { let i = 0; return { [Symbol.iterator]() { return this; }, next() { return { value: i, done: i++ >= n }; }, return(v) { seen.push('ret@' + i); return { done: true }; } }; } for (const v of mkIt(3)) {} for (const v of mkIt(9)) { if (v === 1) break; } console.log(seen.join('|'));",
    // Iterator return() that THROWS during a break (spec: propagates).
    "const badRet = { [Symbol.iterator]() { let i = 0; return { next: () => ({ value: i++, done: false }), return() { throw new Error('badret'); } }; } }; try { for (const v of badRet) { if (v === 1) break; } } catch (e) { console.log('close', e.message); }",
    // Iterator return() error is SUPPRESSED when the body threw.
    "const badRet2 = { [Symbol.iterator]() { let i = 0; return { next: () => ({ value: i++, done: false }), return() { throw new Error('masked'); } }; } }; try { for (const v of badRet2) { if (v === 1) throw new Error('body'); } } catch (e) { console.log('won', e.message); }",
    // next() throwing mid-loop (no close on next-throw, per spec).
    "const badNext = { [Symbol.iterator]() { let i = 0; return { next() { if (i === 2) throw new Error('next!'); return { value: i++, done: false }; } }; } }; let bn = 0; try { for (const v of badNext) bn += v; } catch (e) { console.log(bn, e.message); }",
    // Array destructuring: holes, defaults, rest, iterator close.
    "const [d3, , d4 = 7, ...dr] = [1, 2, undefined, 4, 5]; console.log(d3, d4, dr.join('+'));",
    "function swapd(a, b) { [a, b] = [b, a]; return a + '/' + b; } console.log(swapd(1, 2));",
    // Nested destructuring in for-of over entries (the idiomatic shape).
    "const m = { x: 1, y: 2 }; let ent = ''; for (const [k, v] of Object.entries(m)) ent += k + '=' + v + ';'; console.log(ent);",
    // String iteration (surrogate pairs stay whole).
    "let si = []; for (const ch of 'a\u{1F600}b') si.push(ch.length); console.log(si.join(','));",
    // for-of over a Map/Set (native iterators through the reg frame).
    "const mp = new Map([[1, 'a'], [2, 'b']]); let mo = ''; for (const [k, v] of mp) mo += k + v; const st2 = new Set([3, 4]); for (const v of st2) mo += v; console.log(mo);",

    // ---- mixed tiers: throws crossing reg/stack frames ----
    // reg frame throws → stack frame (try) catches.
    "function thrower() { return missing_in_reg_frame; } function catcher() { try { return thrower(); } catch (e) { return 'caught ' + e.constructor.name; } } console.log(catcher());",
    // stack frame (for-of) calls reg callbacks.
    "function cb(x) { return x * 3; } let mt = 0; for (const v of [1, 2].map(cb)) mt += v; console.log(mt);",
    // Await-free async function: runs whole through the register tier and
    // resolves its promise from the returned value.
    "async function af(n) { return n * 2; } af(21).then(v => console.log('async', v));",
    "async function ar() { throw new Error('rej'); } ar().catch(e => console.log('rejected', e.message));",

    // ---- misc semantics the shuffles must preserve ----
    // Assignment result values (SetProp/SetElem push the VALUE).
    "const rv = {}; console.log(rv.a = 5, rv['b'] = 6, (rv.c = 7) + 1);",
    // JSON round-trip through native calls from reg frames.
    "console.log(JSON.stringify(JSON.parse('{\"a\":[1,2,{\"b\":null}]}')));",
    // getter/setter defined via literal then hit through IC sites.
    "let gs = 0; const gso = { get v() { return ++gs; }, set v(x) { gs = x; } }; gso.v; gso.v; gso.v = 10; console.log(gso.v);",
    // Number formatting edge cases through ToString sites.
    "console.log(`${-0} ${1e21} ${0.1 + 0.2} ${2 ** 53}`);",
];

#[test]
fn register_tier_preserves_observable_behavior() {
    for (n, src) in CORPUS.iter().enumerate() {
        let baseline = run(src, false);
        let got = run(src, true);
        assert_eq!(
            baseline, got,
            "register tier changed behavior for corpus[{n}]:\n  {src}\n  baseline={baseline:?}\n  got={got:?}"
        );
    }
}

/// Deep recursion through register frames must raise the exact spec
/// RangeError at the same depth as the stack tier (the `call_depth` guard is
/// shared), and recover cleanly.
#[test]
fn reg_depth_overflow_matches_stack() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let src = "function rec(n) { return n <= 0 ? 0 : 1 + rec(n - 1); } \
                 try { console.log(rec(1000)); } catch (e) { console.log('deep', e.constructor.name); } \
                 console.log(rec(10));";
            let mut outs = Vec::new();
            for regs in [true, false] {
                let proto = Rc::new(compile_script_regs(src, regs).expect("compiles"));
                let mut engine = Engine::new();
                engine.vm.max_call_depth = 64;
                let func = engine.vm.make_closure(proto, Vec::new());
                let res = engine.vm.call(Value::Object(func), Value::Undefined, &[]);
                assert!(res.is_ok(), "caught in-script (regs={regs})");
                outs.push(engine.console().to_vec());
            }
            assert_eq!(outs[0], outs[1], "depth overflow diverged");
            assert_eq!(outs[0][0], "deep RangeError");
        })
        .expect("spawns")
        .join()
        .expect("no panic");
}

/// An installed op budget must keep per-stack-op accounting EXACT: the
/// register tier declines and the budgeted run terminates with the same
/// uncatchable RangeError either way — including for call-heavy programs
/// whose callees carry register programs.
#[test]
fn op_budget_stays_exact_with_reg_protos() {
    let src =
        "function work(n) { let s = 0; for (let i = 0; i < n; i++) s += helper(i); return s; } \
               function helper(i) { return i % 3; } \
               let t = 0; while (true) { t = work(100) + t; }";
    for regs in [true, false] {
        let proto = Rc::new(compile_script_regs(src, regs).expect("compiles"));
        let mut engine = Engine::new();
        engine.vm.op_budget = Some(200_000);
        let func = engine.vm.make_closure(proto, Vec::new());
        let res = engine.vm.call(Value::Object(func), Value::Undefined, &[]);
        let err = res.expect_err("budget must trip");
        let msg = engine.vm.error_to_string(&err);
        assert!(
            msg.contains("execution budget exceeded"),
            "unexpected error (regs={regs}): {msg}"
        );
        // Identical budget consumption on both compiles: the remaining
        // budget after the abort must match exactly.
        assert_eq!(engine.vm.op_budget, Some(0), "budget drained (regs={regs})");
    }
}

fn walk_protos(p: &FuncProto, f: &mut impl FnMut(&FuncProto)) {
    f(p);
    for c in &p.consts {
        if let Const::Func(nested) = c {
            walk_protos(nested, f);
        }
    }
}

/// Structural pins: the canonical shapes MUST carry register programs, and
/// the excluded shapes must NOT — so coverage can't silently evaporate.
#[test]
fn structural_translation_pins() {
    // Must translate: plain functions with calls, property access, loops
    // over cells, closures, object literals, for-in, method calls.
    for (name, src) in [
        ("props+calls", "function f(o) { o.a = 1; return f2(o.a + o.b); } function f2(x) { return x * 2; } f({ b: 1 });"),
        ("method call", "function m(a) { return a.slice(1).concat(9).length; } m([1, 2, 3]);"),
        ("closure", "function mk(x) { return (y) => x + y; } mk(1)(2);"),
        ("for-in", "function fi(o) { let s = ''; for (const k in o) s += k; return s; } fi({ a: 1 });"),
        ("obj literal", "function ol() { return { a: 1, m() { return 2; }, get g() { return 3; } }; } ol();"),
        ("ternary+logic", "function tl(a, b) { return (a ?? b) ? a && b : a || b; } tl(1, 2);"),
        ("try/catch/finally", "function tc() { try { return 1; } catch (e) { return 2; } finally { } } tc();"),
        ("for-of", "function fo(a) { let s = 0; for (const v of a) s += v; return s; } fo([1]);"),
        ("array destructuring", "function ad(a) { const [x, , y = 9] = a; return x + y; } ad([1, 2]);"),
    ] {
        let proto = compile_script(src).expect("compiles");
        let mut found = false;
        walk_protos(&proto, &mut |p| {
            if p.reg.is_some() && !p.name.is_empty() {
                found = true;
            }
        });
        assert!(found, "{name}: expected a register-translated function:\n{src}");
    }
    // Must NOT translate: with, direct eval, generators, suspending async
    // bodies, `using` declarations, and loop-kernelized functions.
    for (name, needle, src) in [
        (
            "try",
            "tc",
            "function tc() { try { return 1; } catch (e) { return 2; } } tc();",
        ),
        (
            "for-of",
            "fo",
            "function fo(a) { let s = 0; for (const v of a) s += v; return s; } fo([1]);",
        ),
        (
            "with",
            "w",
            "function w(o) { with (o) { return v; } } w({ v: 1 });",
        ),
        (
            "eval",
            "ev",
            "function ev() { let v = 1; return eval('v'); } ev();",
        ),
        ("generator", "g", "function* g() { yield 1; } [...g()];"),
        (
            "await",
            "aw",
            "async function aw() { return await 1; } aw();",
        ),
        (
            "kernel loop",
            "k",
            "function k(n) { let s = 0; for (let i = 0; i < n; i++) s += i * 2; return s; } k(3);",
        ),
        (
            "extends",
            "D",
            "class B0 {} class D extends B0 { constructor() { super(); } } new D();",
        ),
    ] {
        let proto = compile_script(src).expect("compiles");
        walk_protos(&proto, &mut |p| {
            if p.name == needle {
                assert!(
                    p.reg.is_none(),
                    "{name}: `{needle}` must DECLINE register translation:\n{src}"
                );
            }
        });
    }
    // A function-kernel comparator still carries a register program too:
    // the kernel guard declining (e.g. non-Number args) falls back to the
    // register tier, not the stack loop.
    let proto = compile_script("[3, 1].sort((a, b) => a - b);").expect("compiles");
    let mut both = false;
    walk_protos(&proto, &mut |p| {
        if p.fn_kernel.is_some() && p.reg.is_some() {
            both = true;
        }
    });
    assert!(
        both,
        "fn-kernel callee should also carry a register program"
    );
}

/// Deterministic fuzz: generate random whole programs — mixed-type
/// arithmetic, property get/set on shared objects, nested helper calls,
/// ternaries and short-circuits, string building, occasional for-in and
/// dynamic element traffic — and require reg-on/off equivalence on every
/// one. Unlike the kernel fuzz (numeric loops), this exercises the
/// whole-body translation: virtual-stack shuffles, IC sites, the call
/// paths, and the TDZ dataflow.
#[test]
fn reg_fuzz_differential() {
    // Tiny LCG — the test must be deterministic (fixed seed, no host RNG).
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = move |n: u64| -> u64 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % n
    };

    for case in 0..300 {
        let nvars = 2 + rnd(3) as usize;
        let mut body = String::new();
        let stmts = 2 + rnd(6);
        for _ in 0..stmts {
            let v = rnd(nvars as u64);
            let w = rnd(nvars as u64);
            let ops = ["+", "-", "*", "%", "|", "^"];
            let op = ops[rnd(ops.len() as u64) as usize];
            match rnd(10) {
                0 => body.push_str(&format!("v{v} = v{v} {op} v{w};\n")),
                1 => body.push_str(&format!("o.p{v} = v{w} {op} i;\n")),
                2 => body.push_str(&format!("v{v} = (o.p{w} ?? 0) {op} 2;\n")),
                3 => body.push_str(&format!("v{v} = helper(v{w}, i) {op} 1;\n")),
                4 => body.push_str(&format!("v{v} = i % 2 ? v{w} : o.m(v{v});\n")),
                5 => body.push_str(&format!("s += '' + v{v} + ',';\n")),
                6 => body.push_str(&format!("arr[i % arr.length] = v{v};\n")),
                7 => body.push_str(&format!("v{v} = arr[(i + {w}) % arr.length] {op} v{v};\n")),
                8 => {
                    let cmps = ["<", "<=", ">", ">=", "===", "!=="];
                    let c = cmps[rnd(6) as usize];
                    body.push_str(&format!(
                        "if (v{v} {c} v{w}) {{ v{v} = v{w} {op} 3; }} else {{ o.p{w} = v{v}; }}\n"
                    ));
                }
                _ => body.push_str(&format!("v{v} = (v{v} {op} 5) || v{w};\n")),
            }
        }
        if case % 7 == 0 {
            body.push_str("for (const k in o) { s += k; break; }\n");
        }
        if case % 11 == 0 {
            body.push_str("if (i === 3) continue;\n");
        }
        if case % 13 == 0 {
            let v = rnd(nvars as u64);
            body.push_str(&format!("v{v} = 'poison' + i;\n"));
        }
        if case % 5 == 0 {
            let v = rnd(nvars as u64);
            body.push_str(&format!("v{v} = mk(v{v})();\n"));
        }
        // Handler machinery: try/catch/finally around mixed statements,
        // throws caught in-loop, for-of with early exits.
        if case % 3 == 1 {
            let v = rnd(nvars as u64);
            match rnd(4) {
                0 => body.push_str(&format!(
                    "try {{ if (i === 5) throw v{v}; v{v} += 1; }} catch (e) {{ v{v} = ('' + e).length; }}\n"
                )),
                1 => body.push_str(&format!(
                    "try {{ v{v} = helper(v{v}, i); }} finally {{ s += 'f'; }}\n"
                )),
                2 => body.push_str(&format!(
                    "for (const q of arr) {{ v{v} = (v{v} | 0) + (q | 0); if (q === 6) break; }}\n"
                )),
                _ => body.push_str(&format!(
                    "try {{ try {{ if (v{v} > 20) throw 'deep'; }} finally {{ s += 'g'; }} }} catch (e) {{ v{v} = 0; }}\n"
                )),
            }
        }
        let decls: Vec<String> = (0..nvars)
            .map(|v| format!("let v{v} = {};", [0, 1, -1, 42][v % 4]))
            .collect();
        let prints: Vec<String> = (0..nvars).map(|v| format!("v{v}")).collect();
        let src = format!(
            "function helper(a, b) {{ return a % 7 + b; }}\n\
             function mk(x) {{ return () => (x | 0) + 1; }}\n\
             const o = {{ p0: 1, p1: 2, p2: 3, m(x) {{ return (x | 0) - 1; }} }};\n\
             const arr = [2, 4, 6, 8];\n\
             let s = '';\n\
             {}\nfor (let i = 0; i < 12; i++) {{\n{body}}}\n\
             console.log({}, s, JSON.stringify(o), arr.join('|'));",
            decls.join(" "),
            prints.join(", ")
        );
        let with = run(&src, true);
        let without = run(&src, false);
        assert_eq!(with, without, "fuzz case {case} diverged:\n{src}");
    }
}

/// The register translator's own corpus-independent invariants: the
/// top-level script of every corpus entry either translates or declines,
/// and translated programs never carry a zero-length code vector.
#[test]
fn translated_programs_are_wellformed() {
    for src in CORPUS {
        let proto = compile_script_regs(src, true).expect("compiles");
        walk_protos(&proto, &mut |p| {
            if let Some(reg) = &p.reg {
                assert!(!reg.code.is_empty(), "empty register program:\n{src}");
                assert!(
                    reg.num_regs as u32 >= p.num_locals,
                    "register file smaller than locals:\n{src}"
                );
            }
        });
    }
}
