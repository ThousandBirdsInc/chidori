# Demonstrates the infix language layer on top of the WASM sandbox.
#
# `exec_expr` ships a small Rust-authored interpreter (lexer, recursive-
# descent parser, tree-walking evaluator) compiled to wasm32 and embedded
# in the chidori binary. The source below runs inside wasmer under a
# fuel budget + capped linear memory, with host-supplied `vars` passed in
# by prepending `let name = value in …` chains.

def agent(a, b):
    sum_expr = exec_expr("a + b", vars = {"a": a, "b": b})
    clamped = exec_expr(
        "if a < 0 then 0 else if a > 100 then 100 else a",
        vars = {"a": a},
    )
    max_expr = exec_expr(
        "if a > b then a else b",
        vars = {"a": a, "b": b},
    )
    factorial_5 = exec_expr("""
        let f1 = 1 in
        let f2 = f1 * 2 in
        let f3 = f2 * 3 in
        let f4 = f3 * 4 in
        let f5 = f4 * 5 in
        f5
    """)
    ops = exec_expr(
        "(a + b) * (a - b)",  # difference of squares in disguise
        vars = {"a": a, "b": b},
    )
    logical = exec_expr(
        "a > 0 && b > 0 && a + b < 100",
        vars = {"a": a, "b": b},
    )
    div_error = try_call(lambda: exec_expr("10 / 0"))

    return {
        "sum": sum_expr,
        "max": max_expr,
        "clamped_a": clamped,
        "factorial_5": factorial_5,
        "diff_of_squares_ish": ops,
        "both_positive_under_100": logical,
        "div_error": div_error["error"] != None,
    }
