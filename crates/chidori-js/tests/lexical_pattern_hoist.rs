//! Hoisted function declarations must capture lexical bindings introduced by
//! DESTRUCTURING declarators, not just simple identifiers — regression tests
//! for the bug where `const { x } = obj; function f() { return x; }` inside a
//! function body left `x` unresolvable (ReferenceError) or stuck in TDZ.

use chidori_js::Engine;

fn console(src: &str) -> String {
    let mut e = Engine::new();
    if let Err(err) = e.eval(src) {
        return format!("ERR: {err}");
    }
    let _ = e.vm.run_jobs_until_blocked();
    e.console().join("\n")
}

#[test]
fn object_pattern_visible_to_hoisted_function_in_function_body() {
    assert_eq!(
        console(
            "(function () {\n\
             const o = { x: 42 };\n\
             const { x } = o;\n\
             function read() { return x; }\n\
             console.log(read());\n\
             })();"
        ),
        "42"
    );
}

#[test]
fn object_pattern_visible_to_hoisted_async_function() {
    assert_eq!(
        console(
            "(async function () {\n\
             const o = { x: 7 };\n\
             const { x } = o;\n\
             async function read() { return x; }\n\
             console.log(await read());\n\
             })();"
        ),
        "7"
    );
}

#[test]
fn array_and_renamed_and_nested_patterns_hoist() {
    assert_eq!(
        console(
            "(function () {\n\
             const [a, b = 5] = [1];\n\
             const { c: renamed, d: { e } } = { c: 2, d: { e: 3 } };\n\
             const { ...rest } = { r: 4 };\n\
             function read() { return [a, b, renamed, e, rest.r].join(','); }\n\
             console.log(read());\n\
             })();"
        ),
        "1,5,2,3,4"
    );
}

#[test]
fn module_scope_pattern_visible_to_hoisted_function() {
    assert_eq!(
        console(
            "const { x } = { x: 9 };\n\
             function read() { return x; }\n\
             console.log(read());"
        ),
        "9"
    );
}

#[test]
fn tdz_still_enforced_before_declaration() {
    let out = console(
        "(function () {\n\
         function read() { return x; }\n\
         try { read(); console.log('no-tdz'); } catch (e) { console.log('tdz'); }\n\
         const { x } = { x: 1 };\n\
         console.log(read());\n\
         })();",
    );
    assert_eq!(out, "tdz\n1");
}

#[test]
fn block_scoped_pattern_shadows_correctly() {
    assert_eq!(
        console(
            "(function () {\n\
             const { x } = { x: 'outer' };\n\
             {\n\
               const { x } = { x: 'inner' };\n\
               console.log(x);\n\
             }\n\
             console.log(x);\n\
             })();"
        ),
        "inner\nouter"
    );
}
