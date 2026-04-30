WAT = """
(module
    (import "host" "log" (func $log (param i32 i32)))
    (memory (export "memory") 1)
    (data (i32.const 0) "wasm says hello")
    (func $add (export "add") (param i32 i32) (result i32)
        local.get 0
        local.get 1
        i32.add)
    (func $mul (export "mul") (param i32 i32) (result i32)
        local.get 0
        local.get 1
        i32.mul)
    (func (export "greet")
        i32.const 0
        i32.const 15
        call $log)
)
"""

INFINITE = """
(module
    (func $loop (export "loop") (result i32)
        (loop $lp (br $lp))
        i32.const 0)
)
"""

def agent(x, y):
    added = exec(WAT, function = "add", args = [x, y], fuel = 100000, memory_pages = 1)
    multiplied = exec(WAT, function = "mul", args = [x, y], fuel = 100000, memory_pages = 1)
    greeted = exec(WAT, function = "greet", args = [], fuel = 100000, memory_pages = 1)

    # Fuel exhaustion path — use try_call to capture the error instead of crashing.
    runaway = try_call(
        lambda: exec(INFINITE, function = "loop", args = [], fuel = 1000),
    )

    return {
        "add": added["returns"][0],
        "mul": multiplied["returns"][0],
        "add_fuel_remaining": added["fuel_remaining"],
        "greet_fuel_used": 100000 - greeted["fuel_remaining"],
        "runaway_error": runaway["error"],
    }
