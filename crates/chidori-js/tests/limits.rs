//! Boundary tests for the engine allocation caps (`MAX_DENSE_ARRAY`,
//! `MAX_STRING_LEN` in `src/value.rs`). The caps guarantee no single opcode
//! can allocate without bound (docs/sandbox-model.md "Memory ceiling"); each
//! guard raises a *catchable* `RangeError` before allocating, so agent code
//! can recover. These tests pin the current values (2^25 elements / 2^28 code
//! units) so a future change to either constant is a deliberate, test-visible
//! decision — the previous bump (1M -> 2^25, 2^24 -> 2^28) landed with no test
//! pinning either boundary.
//!
//! `MAX_DENSE_ARRAY` bounds the DENSE backing store, not the array `length`:
//! a `length` past the cap is honoured with a sparse tail that allocates
//! nothing, so the ceiling holds without the (non-conformant) RangeError the
//! cap used to raise.

use chidori_js::value::{Value, MAX_DENSE_ARRAY, MAX_STRING_LEN};
use chidori_js::Engine;

fn eval_str(e: &mut Engine, src: &str) -> String {
    match e.eval(src).unwrap() {
        Value::String(s) => s.as_str().to_string(),
        other => panic!("expected string result, got {other:?}"),
    }
}

#[test]
fn dense_array_cap_is_a_catchable_range_error() {
    let mut e = Engine::new();
    // A `length` past the cap is NOT an error: the array simply keeps no dense
    // backing for the tail (the spec's sparse array), so nothing is allocated
    // and the ceiling still holds. `new Array(n)` and `a.length = n` both do
    // this, up to the spec's own 2^32-1 bound.
    let src = format!("new Array({over}).length", over = MAX_DENSE_ARRAY + 1);
    let v = e.eval(&src).unwrap();
    assert!(matches!(v, Value::Number(n) if n == (MAX_DENSE_ARRAY + 1) as f64));

    let src = format!(
        "(function () {{ const a = []; a.length = {over}; return a.length; }})();",
        over = MAX_DENSE_ARRAY + 1
    );
    let v = e.eval(&src).unwrap();
    assert!(matches!(v, Value::Number(n) if n == (MAX_DENSE_ARRAY + 1) as f64));

    // Past 2^32-1 the spec bound applies, as a catchable RangeError.
    let src = r#"
        (function () {
          try {
            new Array(4294967296);
            return "allocated";
          } catch (err) {
            return err instanceof RangeError ? "range-error" : "wrong-error: " + err;
          }
        })();
    "#;
    assert_eq!(eval_str(&mut e, src), "range-error");

    // What the cap still guards is EAGER DENSE materialization: a builtin
    // asked to build a real backing store past the cap refuses before
    // allocating, with a catchable RangeError.
    let src = format!(
        r#"
        (function () {{
          try {{
            Array.from({{ length: {over} }});
            return "allocated";
          }} catch (err) {{
            return err instanceof RangeError ? "range-error" : "wrong-error: " + err;
          }}
        }})();
        "#,
        over = MAX_DENSE_ARRAY + 1
    );
    assert_eq!(eval_str(&mut e, &src), "range-error");

    // Ordinary allocation below the cap is unaffected.
    let v = e.eval("new Array(4096).length").unwrap();
    assert!(matches!(v, Value::Number(n) if n == 4096.0));
}

#[test]
fn string_cap_is_a_catchable_range_error() {
    let mut e = Engine::new();
    // `repeat` guards before allocating; one past the cap must throw a
    // catchable RangeError.
    let src = format!(
        r#"
        (function () {{
          try {{
            "ab".repeat({half} + 1);
            return "allocated";
          }} catch (err) {{
            return err instanceof RangeError ? "range-error" : "wrong-error: " + err;
          }}
        }})();
        "#,
        half = MAX_STRING_LEN / 2
    );
    assert_eq!(eval_str(&mut e, &src), "range-error");

    // The exponential `s += s` OOM pattern terminates in a catchable
    // RangeError from the concat guard instead of exhausting memory.
    let src = r#"
        (function () {
          let s = "x".repeat(1 << 20); // 1M, far below the cap
          try {
            for (let i = 0; i < 40; i++) s += s;
            return "unbounded";
          } catch (err) {
            return err instanceof RangeError ? "range-error" : "wrong-error: " + err;
          }
        })();
    "#;
    assert_eq!(eval_str(&mut e, src), "range-error");

    // Normal string work below the cap is unaffected.
    let v = e.eval(r#""ab".repeat(1024).length"#).unwrap();
    assert!(matches!(v, Value::Number(n) if n == 2048.0));
}
