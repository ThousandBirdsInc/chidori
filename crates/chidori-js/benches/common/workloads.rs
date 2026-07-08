//! Shared workload corpus for the chidori-js in-process benchmarks.
//!
//! Both `benches/execution.rs` (wall-clock, criterion) and `benches/memory.rs`
//! (heap utilization) run this same set, so a time number and a memory number
//! for the same name describe the same program. Lives in a subdirectory so
//! cargo does not auto-discover it as a bench target of its own; each bench
//! pulls it in with `#[path = "common/workloads.rs"] mod workloads;`.
//!
//! Each workload is an IIFE so it returns a value and declares no globals
//! (lets a bench reuse one engine across iterations).

pub const WORKLOADS: &[(&str, &str)] = &[
    // Tight numeric loop — interpreter dispatch + integer/float arithmetic.
    (
        "arith_loop",
        "(function(){ let s = 0; for (let i = 0; i < 20000; i++) { s += i * 2 - (i % 3); } return s; })()",
    ),
    // Recursion + function-call overhead (frame setup/teardown).
    (
        "fib_recursive",
        "(function(){ function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); } return fib(24); })()",
    ),
    // Object property get/set in a loop (shape lookups, map access).
    (
        "property_access",
        "(function(){ let o = { a: 0, b: 0, c: 0 }; for (let i = 0; i < 10000; i++) { o.a = i; o.b = o.a + 1; o.c = o.b + o.a; } return o.c; })()",
    ),
    // Array growth + indexed read loop.
    (
        "array_push_sum",
        "(function(){ let a = []; for (let i = 0; i < 5000; i++) a.push(i); let s = 0; for (let i = 0; i < a.length; i++) s += a[i]; return s; })()",
    ),
    // Higher-order array methods (map/filter/reduce + per-element closures).
    (
        "array_hof",
        "(function(){ let a = []; for (let i = 0; i < 2000; i++) a.push(i); return a.map(x => x * x).filter(x => x % 2 === 0).reduce((p, c) => p + c, 0); })()",
    ),
    // String building (concatenation + number→string coercion).
    (
        "string_build",
        "(function(){ let s = ''; for (let i = 0; i < 3000; i++) s += 'x' + i; return s.length; })()",
    ),
    // Closures + higher-order calls in a loop (upvalue capture/read).
    (
        "closures",
        "(function(){ function adder(n){ return function(x){ return x + n; }; } let f = adder(5); let s = 0; for (let i = 0; i < 10000; i++) s = f(s) - 4; return s; })()",
    ),
];
