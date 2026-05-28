# Runs real Python inside the WASM sandbox via the embedded sandbox-python
# binary (RustPython compiled to wasm32-wasip1). The host drives a minimal
# hand-rolled WASI preview 1 shim so stdin, stdout, and a fixed clock all
# work without pulling in wasmer-wasix.

def agent(n):
    # Recursive function — a real Python call stack, not a rewrite.
    factorial = exec_python("""
def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)
result = fact(""" + str(n) + """)
""")

    # List comprehensions and sum() both work.
    sum_sq = exec_python("result = sum(i * i for i in range(" + str(n) + "))")

    # Comprehensive string ops.
    greet = exec_python("""
name = "world"
result = ", ".join([w.capitalize() for w in ("hello " + name).split()]) + "!"
""")

    # Error path — ZeroDivisionError bubbles out as a string error; we catch
    # it with try_call so the agent can respond gracefully.
    divzero = try_call(lambda: exec_python("result = 1 / 0"))

    return {
        "factorial": factorial,
        "sum_of_squares": sum_sq,
        "greet": greet,
        "div_error": divzero["error"] != None,
    }
