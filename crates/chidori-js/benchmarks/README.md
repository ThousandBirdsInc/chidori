# chidori-js cross-runtime benchmarks

A suite that runs the **same JavaScript workloads** under three runtimes and
compares wall-clock execution time and peak memory (max RSS):

| runtime    | what it is                                                            |
| ---------- | --------------------------------------------------------------------- |
| `chidori`  | the pure-Rust `chidori-js` engine (via the `run` example binary)      |
| `node`     | Node.js (V8)                                                          |
| `bun`      | Bun (JavaScriptCore)                                                  |

This sits alongside two in-process benchmarks over the same workload corpus:
the [`benches/execution.rs`](../benches/execution.rs) criterion
micro-benchmarks, which isolate chidori-js's own hot paths (compile /
interpret / realm setup), and [`benches/memory.rs`](../benches/memory.rs)
(`cargo bench -p chidori-js --bench memory`), which reports exact heap
utilization (realm footprint, per-run peak/churn/retained, leak check) via a
tracking allocator. **This** suite answers the different question "how does
chidori-js compare to the JITs people actually ship?" by running each workload
as a standalone script under all three runtimes.

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

Three tables are printed:

- **Execution-only** — raw subprocess time **minus that runtime's startup
  baseline** (measured separately with `workloads/startup.js`). This is the
  fairer engine-vs-engine number. The `N.Nx` suffix is the slowdown relative to
  the fastest runtime on that row.
- **Total including startup** — the raw `spawn → exit` wall-clock. chidori-js is
  a small native binary and starts in ~3ms, whereas Node pays ~34ms of V8/runtime
  startup, so for very short scripts chidori can win the *total* even when it
  loses the *execution* — this table makes that visible.
- **Peak memory** — max RSS of the subprocess, per workload plus a `(startup)`
  row (the runtime's floor footprint before any workload allocates). Measured
  in dedicated extra runs (default 3, median; `--mem-runs N`) so the timing
  methodology is untouched, and reported absolute rather than
  startup-subtracted — unlike wall-clock, RSS doesn't subtract linearly. The
  `N.Nx` suffix is the blow-up relative to the smallest runtime on that row.
  Skip it with `--no-memory`.

Peak RSS comes from the best available source: GNU `time -v` (or Homebrew
`gtime`) / BSD `time -l`, which report the kernel's exact `ru_maxrss`; without
those, on Linux the harness polls the monotonic `VmHWM` high-water mark in
`/proc/<pid>/status` every ~1ms — exact whenever a sample lands after the
peak, slightly under-reading only for very short-lived processes. On other
platforms with no usable `time`, the memory table is skipped with a warning.

Pass `--json PATH` to also dump every sample (min/median/mean/max per runtime)
for offline analysis, or `--markdown PATH` to write the same two tables as a
Markdown report.

## CI integration

[`.github/workflows/js-benchmarks.yml`](../../../.github/workflows/js-benchmarks.yml)
runs this suite on every PR that touches `crates/chidori-js/**` and posts the
Markdown report (`--markdown`) as a **single sticky comment** on the PR —
updated in place on each push, never re-posted. The comment also carries the
in-process heap numbers (`cargo bench -p chidori-js --bench memory`), appended
as a final section. The full report is also uploaded as a build artifact
(`js-benchmark-report`).

The job only fails the build on a **correctness mismatch** between runtimes (the
harness exits non-zero), never on timing — the numbers come from a shared
GitHub-hosted runner and are meant as a ratio smell-test, not a hard perf gate.
The comment step is skipped for fork PRs (their token can't comment) and for
manual `workflow_dispatch` runs; the artifact is still available in both cases.

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
