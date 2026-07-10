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
    // Same shape as the dense-array workloads but over Float64Array, which the
    // loop kernels do not yet accept as a base.
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
    // Mixed "glue code": small helpers over objects and strings — property
    // traffic across calls, string building, for-in, ternary classification.
    // Every function here declines the typed kernel tiers, so this is the
    // register-bytecode tier's home turf (docs/js-performance-roadmap.md
    // §6.10); mirrors benchmarks/workloads/mixed_helpers.js at bench scale.
    (
        "mixed_helpers",
        "(function(){ function normalize(rec){ const out = { id: rec.id | 0, label: '', score: 0 }; for (const k in rec) { if (k === 'id') continue; const v = rec[k]; if (typeof v === 'number') out.score = out.score + v; else if (typeof v === 'string') out.label = out.label ? out.label + ':' + v : v; } return out; } function render(rec){ return '[' + rec.id + '] ' + (rec.label || '?') + ' => ' + rec.score; } function classify(score){ return score > 40 ? 'hi' : score > 15 ? 'mid' : 'lo'; } const buckets = { hi: 0, mid: 0, lo: 0 }; let checksum = 0; for (let i = 0; i < 4000; i++) { const rec = { id: i, name: 'item' + (i % 7), a: i % 13, b: (i * 3) % 29, kind: i % 2 ? 'x' : 'y' }; const norm = normalize(rec); const line = render(norm); buckets[classify(norm.score)]++; checksum = (checksum + line.length + norm.score) % 1000000007; } return checksum + buckets.hi + buckets.mid + buckets.lo; })()",
    ),
];
