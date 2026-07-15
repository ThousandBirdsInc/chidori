//! Boundary tests for the engine allocation caps (`MAX_DENSE_ARRAY`,
//! `MAX_STRING_LEN` in `src/value.rs`). The caps guarantee no single opcode
//! can allocate without bound (docs/sandbox-model.md "Memory ceiling"); each
//! guard raises a *catchable* `RangeError` before allocating, so agent code
//! can recover. These tests pin the current values (2^25 elements / 2^28 code
//! units) so a future change to either constant is a deliberate, test-visible
//! decision — the previous bump (1M -> 2^25, 2^24 -> 2^28) landed with no test
//! pinning either boundary.

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
    // The constructor guard fires before allocating, so probing one past the
    // cap is cheap. The error must be a catchable RangeError.
    let src = format!(
        r#"
        (function () {{
          try {{
            new Array({over});
            return "allocated";
          }} catch (err) {{
            return err instanceof RangeError ? "range-error" : "wrong-error: " + err;
          }}
        }})();
        "#,
        over = MAX_DENSE_ARRAY + 1
    );
    assert_eq!(eval_str(&mut e, &src), "range-error");

    // Setting `.length` past the cap is guarded the same way.
    let src = format!(
        r#"
        (function () {{
          const a = [];
          try {{
            a.length = {over};
            return "grew";
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
