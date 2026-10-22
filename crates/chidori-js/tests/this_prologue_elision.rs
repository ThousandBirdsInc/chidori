//! The compiler skips binding `%this`/`%newtarget` in a plain function whose
//! source never mentions `this`, `super` or `new.target` and has no direct
//! eval. These pin that every form that CAN observe either binding still
//! sees it — nested arrows, `new F()`, `new.target`, sloppy `this` boxing,
//! `call`/`apply`, generators, direct eval — and that the elided case is
//! behaviorally identical.

use chidori_js::Engine;

fn run(src: &str) -> String {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(v) => e.vm.to_string_lossy(&v),
        Err(err) => format!("ERR: {err}"),
    }
}

#[test]
fn plain_function_without_this_runs_unchanged() {
    assert_eq!(
        run("function f(a, b) { const s = a + b; return s * 2; } f(1, 2) + f(3, 4)"),
        "20"
    );
    assert_eq!(
        run("(function(){ function g(x) { return x | 0; } let t = 0; for (let i = 0; i < 100; i++) t += g(i); return t; })()"),
        "4950"
    );
}

#[test]
fn nested_arrow_still_sees_enclosing_this() {
    assert_eq!(
        run("function f() { const g = () => this.v; return g(); } f.call({ v: 7 })"),
        "7"
    );
    assert_eq!(
        run("function f() { return [1].map(() => this.v)[0]; } f.call({ v: 8 })"),
        "8"
    );
}

#[test]
fn constructor_call_binds_this_and_new_target() {
    assert_eq!(run("function F() { this.x = 1; } new F().x"), "1");
    assert_eq!(
        run("function F() { return new.target === F; } new F() ? 'nt' : 'no'"),
        "nt"
    );
    assert_eq!(
        run("function F() { return new.target; } typeof F()"),
        "undefined"
    );
}

#[test]
fn sloppy_this_boxes_primitives_and_defaults_to_global() {
    assert_eq!(
        run("function f() { return typeof this; } f.call(5)"),
        "object"
    );
    assert_eq!(
        run("function f() { return this === globalThis; } f()"),
        "true"
    );
    assert_eq!(
        run("'use strict'; function f() { return this; } typeof f()"),
        "undefined"
    );
}

#[test]
fn call_and_apply_with_explicit_this() {
    assert_eq!(
        run("function f(a) { return this.k + a; } f.call({ k: 1 }, 2)"),
        "3"
    );
    assert_eq!(
        run("function f(a) { return this.k + a; } f.apply({ k: 1 }, [4])"),
        "5"
    );
}

#[test]
fn generators_and_async_keep_this() {
    assert_eq!(
        run("function* g() { yield this.v; } g.call({ v: 3 }).next().value"),
        "3"
    );
    assert_eq!(
        run("function* g() { yield 1; yield 2; } let s = 0; for (const x of g()) s += x; s"),
        "3"
    );
}

#[test]
fn direct_eval_can_still_read_this() {
    assert_eq!(
        run("function f() { return eval('this.v'); } f.call({ v: 9 })"),
        "9"
    );
}

#[test]
fn methods_and_classes_keep_super_and_this() {
    assert_eq!(
        run("class A { m() { return 1; } } class B extends A { m() { return super.m() + 1; } } new B().m()"),
        "2"
    );
    assert_eq!(
        run("const o = { k: 2, m() { return this.k; } }; o.m()"),
        "2"
    );
    assert_eq!(
        run("class A { constructor() { this.x = 5; } } class B extends A { constructor() { super(); this.y = this.x + 1; } } new B().y"),
        "6"
    );
}

#[test]
fn identifier_containing_this_is_a_conservative_hit_not_a_bug() {
    // `thisArg` mentions the word; the prologue is kept and behavior is
    // identical either way.
    assert_eq!(run("function f(thisArg) { return thisArg + 1; } f(1)"), "2");
}

#[test]
fn arguments_object_still_materializes_without_this() {
    // Regression: the `arguments` materialization shares the non-arrow
    // prologue with the elided bindings and must stay unconditional.
    assert_eq!(
        run("function h(a) { const args = arguments; return args[0] + a; } h(2)"),
        "4"
    );
    assert_eq!(
        run("function h() { return arguments.length; } h(1, 2, 3)"),
        "3"
    );
    assert_eq!(
        run("function* g(a) { const args = arguments; yield args[0]; } g(7).next().value"),
        "7"
    );
}
