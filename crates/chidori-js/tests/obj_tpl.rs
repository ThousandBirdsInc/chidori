//! Structural + behavioral tests for object-literal TEMPLATES
//! (`Op::NewObjectTpl` / `FuncProto::obj_tpls`): an all-static-data-key
//! literal instantiates by cloning a pre-built property map instead of
//! running the generic `NewObject` + N×`DefineField` sequence. The template
//! path must be observably identical — key order, enumeration, JSON output,
//! descriptors, later mutation — and every shape it cannot represent
//! (computed keys, accessors, methods, spread, `__proto__`, duplicates)
//! must keep the generic path.

use chidori_js::bytecode::{Const, FuncProto, Op};
use chidori_js::compiler::compile_script;
use chidori_js::Engine;

fn count_tpl_ops(p: &FuncProto) -> usize {
    let mut n = p
        .code
        .iter()
        .filter(|op| matches!(op, Op::NewObjectTpl { .. }))
        .count();
    for c in &p.consts {
        if let Const::Func(f) = c {
            n += count_tpl_ops(f);
        }
    }
    n
}

#[test]
fn eligible_literals_compile_to_templates() {
    for src in [
        "const o = { a: 1, b: 2 };",
        "const o = { id: 0, name: \"w\", tags: [1], nested: { x: 1, y: 2 }, flag: false };",
        "function f(x) { return { value: x, done: false }; }",
        "const a = 1, b = 2; const o = { a, b };", // shorthand
        "const o = { 0: \"a\", 1: \"b\" };",       // numeric-string keys
        "const o = { fn: () => 1, g: function () {} };", // plain data closures
    ] {
        let proto = compile_script(src).expect("compiles");
        assert!(
            count_tpl_ops(&proto) >= 1,
            "expected a template literal in {src:?}"
        );
    }
}

#[test]
fn ineligible_literals_stay_generic() {
    for src in [
        "const o = { a: 1 };", // single key: not worth a template
        "const k = \"a\"; const o = { [k]: 1, b: 2 };", // computed key
        "const o = { get a() { return 1; }, b: 2 };", // accessor
        "const o = { m() {}, b: 2 };", // method (home object)
        "const p = { x: 1 }; const o = { ...p, b: 2 };", // spread
        "const o = { __proto__: null, b: 2 };", // prototype set
        "const o = { a: 1, a: 2 };", // duplicate (later wins)
    ] {
        let proto = compile_script(src).expect("compiles");
        assert_eq!(count_tpl_ops(&proto), 0, "expected NO template in {src:?}");
    }
}

/// Behavior parity corpus: each program's console output is pinned to the
/// value Node.js 22 produces (cross-checked when this test was written).
#[test]
fn template_objects_behave_identically() {
    let cases: &[(&str, &[&str])] = &[
        (
            "const o = { a: 1, b: \"x\", c: true }; console.log(JSON.stringify(o)); console.log(Object.keys(o).join(\",\"));",
            &["{\"a\":1,\"b\":\"x\",\"c\":true}", "a,b,c"],
        ),
        // Later mutation, extension, delete.
        (
            "const o = { a: 1, b: 2 }; o.c = 3; delete o.a; console.log(JSON.stringify(o));",
            &["{\"b\":2,\"c\":3}"],
        ),
        // Descriptors: template properties are plain writable/enumerable/configurable data.
        (
            "const o = { a: 1, b: 2 }; const d = Object.getOwnPropertyDescriptor(o, \"a\"); console.log(d.value, d.writable, d.enumerable, d.configurable);",
            &["1 true true true"],
        ),
        // Integer-like keys enumerate first, ascending (spec own-key order).
        (
            "const o = { b: 1, 2: \"two\", a: 3, 0: \"zero\" }; console.log(Object.keys(o).join(\",\"));",
            &["0,2,b,a"],
        ),
        // Value expressions evaluate in source order with visible side effects.
        (
            "let log = \"\"; const t = (x) => { log += x; return x; }; const o = { a: t(1), b: t(2), c: t(3) }; console.log(log, o.a + o.b + o.c);",
            &["123 6"],
        ),
        // A throwing value expression aborts the literal (no partial object escapes).
        (
            "try { const o = { a: 1, b: (() => { throw new Error(\"boom\"); })() }; } catch (e) { console.log(\"caught\", e.message); }",
            &["caught boom"],
        ),
        // Anonymous function values get named from their key.
        (
            "const o = { f: () => 1, g: function () {} }; console.log(o.f.name, o.g.name);",
            &["f g"],
        ),
        // Shorthand `__proto__` is a plain data property (no proto set).
        (
            "const __proto__ = 5; const o = { __proto__, z: 1 }; console.log(o.__proto__, Object.getPrototypeOf(o) === Object.prototype);",
            &["5 true"],
        ),
        // for-in over a template object.
        (
            "const rec = { id: 7, name: \"n\", kind: \"y\" }; let s = \"\"; for (const k in rec) s += k + \"=\" + rec[k] + \";\"; console.log(s);",
            &["id=7;name=n;kind=y;"],
        ),
        // Template objects freeze/seal like ordinary objects.
        (
            "const o = Object.freeze({ a: 1, b: 2 }); o.a = 9; console.log(o.a, Object.isFrozen(o));",
            &["1 true"],
        ),
    ];
    for (src, expected) in cases {
        let mut e = Engine::new();
        e.eval(src)
            .unwrap_or_else(|err| panic!("{src:?} threw {err}"));
        assert_eq!(e.console(), *expected, "output mismatch for {src:?}");
    }
}
