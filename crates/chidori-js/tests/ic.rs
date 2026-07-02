//! Tests for the key-verified inline caches (`FuncProto::ic`) behind
//! `GetProp` / `SetProp` / `LoadGlobal`.
//!
//! The cache stores only a slot-index *hint* that is verified against the key
//! actually stored at that slot on every use, so there is no invalidation
//! protocol to test — instead this corpus stresses every way a hint can go
//! stale (delete, re-add at a new slot, reconfigure data→accessor,
//! writable→non-writable, freeze, prototype accessors, global redefinition
//! and deletion) and asserts the observable behavior matches the spec. Each
//! program funnels many receivers/states through ONE shared access site so
//! the same cache entry sees them all. The expectations were cross-checked
//! byte-for-byte against Node (V8).

use std::rc::Rc;

use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};

fn run(src: &str) -> (bool, Vec<String>, String) {
    let proto = Rc::new(compile_script(src).expect("compiles"));
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

#[test]
fn stale_hints_never_change_behavior() {
    let src = r#"
        const log = [];
        function get(o) { return o.x; }        // one shared GetProp site
        function set(o, v) { o.x = v; return o.x; }
        const a = { x: 1, y: 2 }, b = { y: 1, x: 10 };   // x at different slots
        log.push(get(a), get(b), get(a));
        delete a.x;                                        // stale hint
        log.push(get(a));
        a.x = 5; log.push(get(a));                         // re-added, new slot
        Object.defineProperty(a, "x", { get() { return 42; } });
        log.push(get(a));                                  // accessor now
        Object.defineProperty(b, "x", { value: 7, writable: false });
        log.push(set(b, 99));                              // sloppy no-op -> 7
        const c = Object.create({ set x(v) { log.push("protoset:" + v); } });
        set(c, 3);                                         // proto setter fires
        log.push(c.x);                                     // undefined
        const d = Object.freeze({ x: 8 });
        log.push(set(d, 1));                               // frozen -> stays 8
        globalThis.f = () => "one"; function callf() { return f(); }
        log.push(callf()); globalThis.f = () => "two"; log.push(callf());
        delete globalThis.f;
        try { callf(); log.push("no-throw"); } catch (e) { log.push(e.constructor.name); }
        console.log(JSON.stringify(log));
    "#;
    let (threw, console, err) = run(src);
    assert!(!threw, "threw: {err}");
    assert_eq!(
        console,
        vec![
            r#"[1,10,1,null,5,42,7,"protoset:3",null,8,"one","two","ReferenceError"]"#.to_string()
        ]
    );
}

#[test]
fn strict_mode_still_throws_through_the_cache() {
    // The SetProp fast path must never swallow a strict-mode TypeError: a
    // non-writable own property fails hint verification (writable is checked)
    // and the slow path throws.
    let src = r#"
        "use strict";
        function set(o, v) { o.x = v; }
        const o = { x: 1 };
        set(o, 2);                       // warms the cache
        Object.freeze(o);
        set(o, 3);                       // must throw TypeError
    "#;
    let (threw, _console, err) = run(src);
    assert!(threw, "expected strict-mode TypeError");
    assert!(err.contains("TypeError"), "got: {err}");
}

#[test]
fn shared_site_polymorphic_receivers_stay_correct() {
    // A megamorphic site: shapes with `x` at slots 0..4 rotate through one
    // GetProp site; the sum is order-dependent and must be exact.
    let src = r#"
        function get(o) { return o.x; }
        const objs = [];
        for (let i = 0; i < 5; i++) {
            const o = {};
            for (let j = 0; j < i; j++) o["p" + j] = j;
            o.x = i;                    // x lands at slot i
            objs.push(o);
        }
        let sum = 0;
        for (let r = 0; r < 100; r++) for (const o of objs) sum += get(o);
        console.log(sum);
    "#;
    let (threw, console, err) = run(src);
    assert!(!threw, "threw: {err}");
    assert_eq!(console, vec!["1000".to_string()]);
}
