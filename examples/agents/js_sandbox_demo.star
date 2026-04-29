# Runs real JavaScript inside the WASM sandbox via the embedded sandbox-js
# binary (boa_engine compiled to wasm32-wasip1). Reuses the same hand-rolled
# WASI preview 1 shim as exec_python — stdin preloaded with source, stdout
# captured, fixed clock, zero preopens.

def agent(n):
    # Recursion.
    fact = exec_js("""
function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); }
fact(""" + str(n) + """)
""")

    # Array methods + arrow functions.
    sum_sq = exec_js("Array.from({length: " + str(n) + "}, (_, i) => i * i).reduce((a, b) => a + b, 0)")

    # String manipulation + template literals.
    greet = exec_js("""
const name = "world";
`${"hello " + name}`.split(" ").map(w => w[0].toUpperCase() + w.slice(1)).join(", ") + "!"
""")

    # Error propagation.
    boom = try_call(lambda: exec_js("throw new Error('nope')"))

    return {
        "factorial": fact,
        "sum_of_squares": sum_sq,
        "greet": greet,
        "threw": boom["error"] != None,
    }
