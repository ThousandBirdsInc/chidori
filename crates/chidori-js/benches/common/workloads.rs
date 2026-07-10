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
    // Per-code-unit string reads (charCodeAt hash loop — the tokenizer/parser
    // idiom). Exercises the code-unit accessor path, which today materializes
    // the full string per call (`builtins/string.rs::units_this`).
    (
        "string_scan",
        "(function(){ let s = ''; for (let i = 0; i < 512; i++) s += String.fromCharCode(97 + (i % 26)); let h = 0; for (let r = 0; r < 40; r++) { for (let i = 0; i < s.length; i++) { h = (h * 31 + s.charCodeAt(i)) % 1000000007; } } return h; })()",
    ),
    // Typed-array element traffic (fill, dot product, in-place transform).
    // Same shape as the dense-array workloads but over Float64Array (a loop
    // kernel base since docs/js-performance-roadmap.md §6.8).
    (
        "typed_array",
        "(function(){ const n = 8000; const a = new Float64Array(n); const b = new Float64Array(n); for (let i = 0; i < n; i++) { a[i] = i % 97; b[i] = i % 89; } let s = 0; for (let r = 0; r < 3; r++) { let d = 0; for (let i = 0; i < n; i++) d += a[i] * b[i]; for (let i = 0; i < n; i++) a[i] = (a[i] + b[i]) % 97; s += d; } return s; })()",
    ),
    // Recursion shapes the function kernels decline today: mutual recursion
    // (isEven/isOdd), boolean returns, and self-recursion through a const
    // binding (gcd) rather than a global name.
    (
        "mutual_recursion",
        "(function(){ function isEven(n){ return n === 0 ? true : isOdd(n - 1); } function isOdd(n){ return n === 0 ? false : isEven(n - 1); } const gcd = (a, b) => b === 0 ? a : gcd(b, a % b); let c = 0; for (let i = 0; i < 600; i++) { if (isEven(i % 97)) c++; c += gcd(i + 1234, 991); } return c; })()",
    ),
    // Captured loop bounds/accumulators: bindings a closure captures stay
    // heap CELLS, mapped into kernel registers since §6.10.
    (
        "cell_accumulate",
        "(function(){ const N = 20000; let total = 0; const peek = () => total; for (let i = 0; i < N; i++) { total += (i % 7) - (i & 3); } for (let i = 0; i < N; i++) { total += i % 5; } return total + peek(); })()",
    ),
    // Array-typed function-kernel arguments: the `(a, i) => a[i]` accessor
    // class, frameless with per-access re-checks since §6.10.
    (
        "fn_array_args",
        "(function(){ function get(a, i){ return a[i]; } function dot(a, b, n){ let s = 0; for (let i = 0; i < n; i++) { s += a[i] * b[i]; } return s; } const n = 500; const x = new Array(n); const y = new Array(n); for (let i = 0; i < n; i++) { x[i] = i * 0.5; y[i] = (i % 7) + 1; } let h = 0; for (let r = 0; r < 20; r++) { for (let i = 0; i < n; i++) h = (h + get(x, i) * 2) % 1000003; h = (h + dot(x, y, n)) % 1000003; } return h; })()",
    ),
];
