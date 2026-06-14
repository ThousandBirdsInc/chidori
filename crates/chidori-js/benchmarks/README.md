# chidori-js cross-runtime benchmarks

A suite that runs the **same JavaScript workloads** under three runtimes and
compares wall-clock execution time:

| runtime    | what it is                                                            |
| ---------- | --------------------------------------------------------------------- |
| `chidori`  | the pure-Rust `chidori-js` engine (via the `run` example binary)      |
| `node`     | Node.js (V8)                                                          |
| `bun`      | Bun (JavaScriptCore)                                                  |

This sits alongside the in-process [`benches/execution.rs`](../benches/execution.rs)
criterion micro-benchmarks. Those isolate chidori-js's own hot paths (compile /
interpret / realm setup) in-process; **this** suite answers the different
question "how does chidori-js compare to the JITs people actually ship?" by
running each workload as a standalone script under all three runtimes.

## Quick start

```sh
# From the repo root. Builds the chidori `run` example (release) on first run.
node crates/chidori-js/benchmarks/run.mjs

# Faster smoke run (fewer iterations):
node crates/chidori-js/benchmarks/run.mjs --quick

# One workload, more samples, save raw data:
node crates/chidori-js/benchmarks/run.mjs --filter fib --runs 15 --json out.json
```

Node.js is required to run the harness. Bun is optional — if `bun` isn't on
`PATH` it is skipped with a warning. The chidori binary is built automatically
unless you pass `--no-build` (and point at a prebuilt binary via `--chidori-bin`
or `$CHIDORI_RUN_BIN`).

## What it reports

```
Execution-only time (subprocess wall-clock minus startup baseline)
Startup baselines: chidori 3.4ms  node 33.8ms  bun 9.8ms

workload             chidori         node          bun    fastest
-----------------------------------------------------------------
arith_loop       727.1ms 167.0x   7.2ms 1.6x        4.4ms        bun
fib_recursive    2.15s 228.4x  11.8ms 1.3x        9.4ms        bun
...
```

Two tables are printed:

- **Execution-only** — raw subprocess time **minus that runtime's startup
  baseline** (measured separately with `workloads/startup.js`). This is the
  fairer engine-vs-engine number. The `N.Nx` suffix is the slowdown relative to
  the fastest runtime on that row.
- **Total including startup** — the raw `spawn → exit` wall-clock. chidori-js is
  a small native binary and starts in ~3ms, whereas Node pays ~34ms of V8/runtime
  startup, so for very short scripts chidori can win the *total* even when it
  loses the *execution* — this table makes that visible.

Pass `--json PATH` to also dump every sample (min/median/mean/max per runtime)
for offline analysis.

## How it stays honest

Every workload prints exactly one `RESULT=<value>` line. The harness extracts it
from each runtime's stdout and **asserts all runtimes produced the same value**
before reporting timings — a fast-but-wrong engine is not a faster engine. If any
workload disagrees the row is flagged and the process exits non-zero. The
`sort` workload seeds a deterministic LCG so all three runtimes sort identical
input and must agree on the checksum.

## Workloads

Each file in `workloads/` is plain ES that runs unmodified on all three runtimes
and mirrors (scaled up) an `execution.rs` micro-benchmark where one exists:

| workload          | exercises                                                       |
| ----------------- | -------------------------------------------------------------- |
| `arith_loop`      | tight numeric loop — interpreter dispatch + arithmetic         |
| `fib_recursive`   | recursion + call-frame setup/teardown                          |
| `property_access` | object property get/set in a loop                              |
| `array_push_sum`  | array growth + indexed reads                                   |
| `array_hof`       | `map`/`filter`/`reduce` with per-element closures              |
| `string_build`    | `+=` string building + number→string coercion                  |
| `closures`        | closure capture + higher-order calls in a loop                 |
| `json_roundtrip`  | `JSON.stringify` / `JSON.parse` over a nested object           |
| `sort`            | `Array.prototype.sort` with a comparator                       |
| `startup.js`      | near-empty script — used only for the startup baseline         |

Iteration counts are tuned so each workload takes a meaningful-but-bounded time
on the chidori-js interpreter (~0.2–2s); on the JITs they finish in single-digit
to low-tens of milliseconds. To stress a runtime harder, bump the `N` / `ROUNDS`
constant at the top of a workload file — they're deliberately one-liners.

## Adding a workload

1. Drop a `<name>.js` in `workloads/`. It must run on Node, Bun, and chidori-js
   (the engine is a growing subset of ES — stick to widely-supported syntax and
   the built-ins chidori-js implements).
2. End it with `console.log("RESULT=" + <deterministic value>)` so the harness
   can cross-check correctness. Avoid `Date.now()`, `Math.random()`, or anything
   else that varies between runs or runtimes in the reported value.
3. Run `node run.mjs --filter <name>` and confirm it reports `ok` (not a
   `RESULT MISMATCH`).

## Caveats

- Absolute numbers are machine-, load-, and version-dependent; treat them as
  ratios on a quiet machine, not as a leaderboard. The sample table above is
  illustrative.
- This measures whole-script subprocess runs, so it captures parse + compile +
  execute, not steady-state JIT throughput. For chidori-js (an interpreter) that
  is representative; for V8/JSC it understates peak throughput on long-running
  code because much of the run is spent warming up.
