//! Regex budget semantics: exhausting the matcher's step or depth budget must
//! surface as a *catchable error*, never as a silent wrong answer ("no match"
//! for a pattern that would match with more work, or a spurious match
//! fabricated by a budget-starved negative lookaround).

use chidori_js::Engine;

fn run(src: &str) -> String {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(v) => e.vm.to_string_lossy(&v),
        Err(err) => format!("ERR: {err}"),
    }
}

#[test]
fn catastrophic_backtracking_throws_catchable_range_error() {
    // /(a+)+b/ on a long "aaaa…" tail with no `b` is the classic exponential
    // blowup. It must throw (RangeError), not return null.
    let out = run(r#"
        try {
            /(a+)+b/.test('a'.repeat(80) + 'c');
            'no-error'
        } catch (e) {
            e instanceof RangeError ? 'range-error' : 'other:' + e
        }
    "#);
    assert_eq!(out, "range-error");
}

#[test]
fn budget_error_does_not_poison_the_regexp() {
    // After a budget error the same RegExp object still works on benign input.
    let out = run(r#"
        const re = /(a+)+b/;
        let threw = false;
        try { re.test('a'.repeat(80) + 'c'); } catch (e) { threw = true; }
        threw + ':' + re.test('aab')
    "#);
    assert_eq!(out, "true:true");
}

#[test]
fn long_simple_quantifier_matches_iteratively() {
    // Large inputs under simple quantifiers must keep working (iterative fast
    // path, no step-budget or stack blowup).
    let out = run(r#"/^(?:ab)+$/.test('ab'.repeat(200000))"#);
    assert_eq!(out, "true");
    let out = run(r#"/^(?:a|b)+$/.test('ab'.repeat(200000))"#);
    assert_eq!(out, "true");
    let out = run(r#"'x'.repeat(500000).match(/^x*$/)[0].length"#);
    assert_eq!(out, "500000");
    // Sequence fast path must still backtrack by whole repetitions.
    let out = run(r#"/^(?:ab)+abc$/.test('ab'.repeat(1000) + 'abc')"#);
    assert_eq!(out, "true");
    // …and honor {n,m} bounds.
    let out = run(r#"/^(?:ab){2,3}$/.test('abababab')"#);
    assert_eq!(out, "false");
    let out = run(r#"/^(?:ab){2,4}$/.test('abababab')"#);
    assert_eq!(out, "true");
}

#[test]
fn deep_cps_recursion_errors_instead_of_overflowing() {
    // A quantifier body with a capture can't use the iterative fast path; on a
    // huge input the CPS recursion must trip the stack budget and throw a
    // catchable error rather than overflow the native stack (process abort).
    let out = run(r#"
        try {
            /^(?:a(b))+$/.test('ab'.repeat(100000)) ? 'matched' : 'no-match'
        } catch (e) {
            e instanceof RangeError ? 'range-error' : 'other:' + e
        }
    "#);
    assert_eq!(out, "range-error");
}

#[test]
fn negative_lookahead_is_not_fabricated_by_budget_exhaustion() {
    // The inner pattern of the negative lookahead is catastrophic. If budget
    // exhaustion inside the lookaround were treated as "did not match", the
    // assertion would spuriously SUCCEED. It must throw instead.
    let out = run(r#"
        try {
            /(?!(a+)+b)aaa/.test('a'.repeat(80) + 'c') ? 'matched' : 'no-match'
        } catch (e) {
            e instanceof RangeError ? 'range-error' : 'other:' + e
        }
    "#);
    assert_eq!(out, "range-error");
}

#[test]
fn moderate_backtracking_still_works() {
    // Patterns that need real (but bounded) backtracking keep working.
    let out = run(r#"'aaa aab aac'.match(/a+b/)[0]"#);
    assert_eq!(out, "aab");
    let out = run(r#"/^([\s\S]*?)(b+)$/.exec('a'.repeat(3000) + 'bbb')[2]"#);
    assert_eq!(out, "bbb");
}
