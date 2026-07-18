//! Error-reporting DX: line/column on parse errors, and stack traces on
//! runtime errors (recorded on `.stack` as the exception unwinds — see
//! `Vm::record_unwind_frame`). Each frame's position is where the frame *is*
//! when the throw crosses it — the throwing statement for the innermost
//! frame, the active call site for outer frames — resolved through the
//! bytecode's per-op position table (`FuncProto::pos`); the function's
//! definition site is only the fallback for protos compiled without source.

use chidori_js::Engine;

fn run(src: &str) -> String {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(v) => e.vm.to_string_lossy(&v),
        Err(err) => format!("ERR: {err}"),
    }
}

#[test]
fn parse_error_reports_line_and_column() {
    let err = run("let x = 1;\nlet y = ;\n");
    assert!(err.starts_with("ERR: SyntaxError:"), "got: {err}");
    assert!(err.contains("(line 2, column 9)"), "got: {err}");
}

#[test]
fn semantic_early_error_reports_line_and_column() {
    // Duplicate lexical declaration is a semantic-pass early error.
    let err = run("let a = 1;\nlet a = 2;");
    assert!(err.starts_with("ERR: SyntaxError:"), "got: {err}");
    assert!(
        err.contains("line 2") || err.contains("line 1"),
        "got: {err}"
    );
}

#[test]
fn parse_error_without_position_still_reports() {
    // Whatever the diagnostic, the SyntaxError prefix survives.
    let err = run("for (;;");
    assert!(err.starts_with("ERR: SyntaxError:"), "got: {err}");
}

#[test]
fn caught_error_stack_lists_frames_innermost_first() {
    let stack = run("function inner() { throw new TypeError('boom'); }\n\
         function outer() { inner(); }\n\
         var s; try { outer(); } catch (e) { s = e.stack; } s");
    let inner_at = stack.find("at inner (1:").expect(&stack);
    let outer_at = stack.find("at outer (2:").expect(&stack);
    assert!(stack.starts_with("TypeError: boom\n"), "got: {stack}");
    assert!(inner_at < outer_at, "innermost first: {stack}");
}

#[test]
fn innermost_frame_anchors_at_the_throwing_statement() {
    // `inner` is declared on line 1 but throws on line 3; `outer` is declared
    // on line 5 but calls on line 6. Each frame must carry the position the
    // throw crossed it at, not its function's declaration line.
    let stack = run("function inner() {\n\
           const o = undefined;\n\
           return o.p.q;\n\
         }\n\
         function outer() {\n\
           return inner();\n\
         }\n\
         var s; try { outer(); } catch (e) { s = e.stack; } s");
    assert!(
        stack.contains("at inner (3:"),
        "innermost frame anchors at the throw line, not the declaration: {stack}"
    );
    assert!(
        stack.contains("at outer (6:"),
        "outer frame anchors at its call-site line, not the declaration: {stack}"
    );
}

#[test]
fn stack_tier_frames_anchor_at_the_throwing_statement_too() {
    // Generators contain suspension ops, which decline register translation —
    // this exercises the stack interpreter's unwind-position capture (the
    // plain functions above run on the register tier).
    let stack = run("function* gen() {\n\
           const o = undefined;\n\
           o.p.q;\n\
         }\n\
         var s; try { gen().next(); } catch (e) { s = e.stack; } s");
    assert!(
        stack.contains("at gen (3:"),
        "generator frame anchors at the throw line: {stack}"
    );
}

#[test]
fn awaited_rejection_anchors_at_the_await_site() {
    // The rejection is delivered where the frame suspended — the `await` on
    // line 2 — not at the async function's declaration on line 1.
    let mut e = Engine::new();
    e.eval(
        "async function waits() {\n\
           await Promise.reject(new Error('nope'));\n\
         }\n\
         waits().catch((e) => { console.log(e.stack); });",
    )
    .expect("eval ok");
    let out = e.console().join("\n");
    assert!(out.contains("at waits (2:"), "got: {out}");
}

#[test]
fn frames_embedded_in_the_message_are_not_duplicated() {
    // A nested tool/sub-agent error re-enters the awaiting engine as a NEW
    // `Error` whose MESSAGE embeds the inner engine's rendered trace. The
    // with-stack rendering must append only the frames recorded by THIS
    // engine's unwind, not re-append the frames already shown by the message
    // head (the "same frame printed twice" bug).
    let mut e = Engine::new();
    let v = e
        .eval(
            "function boom() { throw new Error('JavaScript exception: TypeError: x\\n    at run (tools/x.ts:19:23)'); }\n\
             function outer() { boom(); }\n\
             try { outer(); } catch (err) { err }",
        )
        .expect("eval ok");
    let out = e.vm.error_to_string_with_stack(&v);
    assert_eq!(
        out.matches("at run (tools/x.ts:19:23)").count(),
        1,
        "the message's embedded frame renders exactly once: {out}"
    );
    assert_eq!(out.matches("at boom (").count(), 1, "got: {out}");
    assert_eq!(out.matches("at outer (").count(), 1, "got: {out}");
}

#[test]
fn uncaught_error_message_format_is_unchanged() {
    // `Engine::eval` renders thrown values with `error_to_string` — the
    // single-line `Name: message` shape is a compatibility contract for
    // embedders; the frames live on `.stack` (and on the module entrypoint
    // paths, which render with `error_to_string_with_stack`).
    let err = run("function f(){ throw new TypeError('x'); } f();");
    assert_eq!(err, "ERR: TypeError: x");
}

#[test]
fn async_rejection_carries_frames() {
    // The completion value is computed before microtasks drain, so observe
    // the rejection through the console (captured after the drain).
    let mut e = Engine::new();
    e.eval(
        "async function fails() { throw new Error('nope'); }\n\
         fails().catch((e) => { console.log(e.stack); });",
    )
    .expect("eval ok");
    let out = e.console().join("\n");
    assert!(out.contains("at fails (1:"), "got: {out}");
}

#[test]
fn thrown_non_error_values_are_untouched() {
    assert_eq!(
        run("function f(){ throw 'plain string'; } try { f(); } catch (e) { e }"),
        "plain string"
    );
}

#[test]
fn rethrow_loop_stops_accumulating_frames_at_the_cap() {
    let stack = run("var e = new RangeError('deep');\n\
         function hop() { throw e; }\n\
         for (let i = 0; i < 100; i++) { try { hop(); } catch (_) {} }\n\
         e.stack");
    let frames = stack.matches("\n    at ").count();
    assert!(frames <= 32, "cap held: {frames} frames");
    assert!(frames >= 30, "frames recorded up to the cap: {frames}");
}

#[test]
fn anonymous_functions_render_as_anonymous() {
    let stack = run("var s; try { (function () { throw new Error('x'); })(); } \
         catch (e) { s = e.stack; } s");
    assert!(stack.contains("at <anonymous> (1:"), "got: {stack}");
}
