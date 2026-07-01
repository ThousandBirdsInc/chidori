# Resume performance: what a resume costs, and how to make it cheap

> **Status:** the two caches below are **landed** (branch
> `claude/chidori-js-jit-compiler-btc3ec`); the warm-standby design in §5 is a
> proposal. **Related:** [`docs/value-checkpoints.md`](./value-checkpoints.md)
> (the `chidori.step` memoization primitive),
> [`docs/interpreter-optimization.md`](./interpreter-optimization.md) and
> [`docs/jit.md`](./jit.md) (interpreter-side speed),
> [`docs/replay.md`](./replay.md) (the durability model this must never bend).

---

## 1. The reframe

Chidori's performance product is not "JS ops per second" — the agent-replay
measurement (`interpreter-optimization.md` §11.5) shows JS execution is well
under 1% of an agent's *live* wall-clock. The product is the latency of the
**durability operations**: resuming after `input()`/approvals/timers, crash
recovery, branch delivery, `chidori trace` re-derivation, and test throughput.

Traced through the code, every one of those funnels into the same shape of
work. A resume today:

1. **Re-transpiles** the agent entry and every imported module through the
   full oxc pipeline (`typescript/transpile.rs::transpile_module`) — sources
   that are byte-identical to the last run's in the overwhelmingly common
   case.
2. **Builds a fresh realm** (`Engine::new()`, ~3.6 ms of builtin
   construction, `interpreter-optimization.md` §11.1) and **re-compiles the
   setup scripts** (determinism prelude, `chidori` SDK helpers, fetch
   polyfill) evaluated verbatim on every engine (`rust_engine.rs::run_module`).
3. **Re-executes the run's JS from the top** against the recorded call log
   (mainline: `run_module` re-run; blob path:
   `ReplayRuntime::from_blob` → replay to the frontier), re-compiling the
   bundle first (`replay.rs::start`).

Cost class per resume: **O(total run history) re-execution + fixed
transpile/realm/compile setup** — paid again on *every* resume, growing with
run length. The interpreter-side work (fusion, the closure JIT) only shrinks
the constant on the O(history) term. The levers below attack the terms
themselves. None of them touch the durability contract: the journal/call log
remains the single source of truth, and every cache here is a pure
performance side effect over deterministic computation.

## 2. Landed: transpile cache (`crates/chidori`)

`transpile_module` now memoizes the **pure oxc pipeline**
(parse → semantic → transform → codegen → strip → collapse) process-wide,
keyed by the full `(path, source)` pair — hash *plus* equality, so a hit can
never alias distinct inputs. It hits every agent execution: initial runs,
every pause→resume re-execution, tool files, sub-agents, branch waves and
resumes, and each imported module load.

Two deliberate boundaries:

- **Import validation always re-runs.** `validate_imports` consults the
  filesystem (relative-import extension probing, package.json resolution), so
  its outcome can change under an unchanged source. It is excluded from the
  cache — a warm cache can never mask a filesystem or policy change. The
  import policy is deliberately *not* part of the cache key for the same
  reason: policy enforcement happens outside the cached region. Both
  properties are pinned by tests
  (`transpile_cache_never_skips_import_validation`,
  `transpile_cache_is_transparent_and_never_aliases`).
- **Success-only, bounded.** Errors are deterministic and cheap to recompute;
  the map clears wholesale at a cap rather than tracking LRU order.

Measured with the in-tree probe (`cargo test -p chidori --release
transpile_cache_timing_probe -- --ignored --nocapture`, synthetic 71 KB
agent-shaped source):

| quantity | value |
| --- | ---: |
| transpile, cold (full oxc pipeline) | 4.0 ms |
| transpile, warm (cached) | 0.13 ms |

~3.9 ms removed per agent-sized transpile — the same order of magnitude as
the realm build, and previously paid per source file on *every* execution
(the entry plus each imported module, again on every resume re-execution).
Of the two caches this is the larger win; the proto cache (§3) is smaller
but free.

## 3. Landed: compiled-script proto cache (`crates/chidori-js`)

`compiler::compile_script_cached` memoizes source → `Rc<FuncProto>` per
thread (protos are `Rc`-shared and immutable; a thread-local cache avoids any
cross-thread state). Two consumers:

- **`ReplayRuntime::start`** (`replay.rs`): a restore re-compiles the *same*
  bundle the journal pins by `bundle_hash`; repeated restores (crash
  recovery, branch delivers, trace re-derivation, tests) now compile once per
  thread. An *edited* bundle is a different source string and simply misses.
- **`Engine::eval_cached`** (`lib.rs`), used by `run_module` for the three
  fixed setup scripts (determinism prelude, SDK helpers, fetch polyfill).
  Execution still runs on every engine — it must, to populate the fresh
  realm — but the parse+lower step is memoized.

Sharing one proto across VMs is sound because a `FuncProto` is immutable
after compilation; the only interior-mutable field (the experimental JIT
thread cache, `jit.rs`) memoizes a pure function of the bytecode and is
VM-independent by design. `tests/replay.rs::
shared_cached_proto_replays_are_independent_and_identical` pins the property
that matters: two runtimes sharing a cached proto replay independently with
byte-identical journals.

Measured with `cargo run --release --example restore_latency -p chidori-js`
(synthetic 27 KB / 300-function bundle, 50-effect journal with real
inter-effect compute; restore+replay, 20 restores):

| quantity | value |
| --- | ---: |
| restore+replay, cold (compile + realm + replay) | 44.0 ms |
| restore+replay, warm (cached proto) | 43.9 ms |
| per-restore compile cost removed by the cache | ~0.12 ms |

**Read this honestly.** The engine-side compile of a 27 KB bundle is ~0.1 ms —
oxc is fast — so the proto cache is a small, essentially free win that scales
with bundle size and restore frequency (tests, branch fan-outs). The number
that dominates is the **~44 ms of replay re-execution**, which grows with run
history and which no compile cache touches. That is the measured version of
the reframe in §1: the fixed setup costs are worth removing because removing
them is free, but the O(history) term is where resume latency actually lives —
which is exactly what `chidori.step` (today) and warm-standby (§5) exist to
bound.

## 4. What these caches do NOT fix

The **O(history) re-execution term** and the **realm build** are untouched.
Bounding re-execution is `chidori.step`'s job today (`value-checkpoints.md`)
and warm-standby's job tomorrow (§5).

### 4.1 The realm build, actually measured

`interpreter-optimization.md` §11.1 reported `engine_new` ≈ 3.6 ms on one
developer machine, which framed realm construction as a dominant fixed cost.
A per-section profile on this container (release; the permanent tool is
`cargo run --release --example realm_profile -p chidori-js`, which iterates
the same `builtins::SECTIONS` table `install()` runs) puts it lower and
spreads it thinner:

| section | ms | share |
| --- | ---: | ---: |
| **total `Engine::new()`** | **~1.0** | |
| temporal | 0.21 | ~21% |
| typedarray | 0.11 | ~11% |
| fundamental | 0.07 | ~7% |
| everything else (14 sections) | ≤0.05 each | ~60% |

Two conclusions. First, **re-measure before optimizing**: on this machine the
realm build is ~1 ms, not 3.6 — still worth removing for high-frequency
resume/test loops, but a quarter of the transpile win, not four times it.
Second, **there is no cheap targeted fix**: no single section dominates
enough that a spot optimization moves the total much. The real levers remain
the invasive ones — lazily materializing rarely-used namespaces (`Temporal`
is both the largest single section and the least likely to be touched by an
agent, so it goes first), or build-once-clone-many shared templates — and
they should be taken up only after warm-standby (§5), which removes far more
per resume for comparable effort.

## 5. Proposed: warm-standby resume (design note)

The dominant production resume — pause on `input()`/approval, deliver,
continue — re-executes the whole run even though **the process never died**.
The obvious fix is to keep the paused VM alive and resolve its pending
promise on delivery: resume becomes O(1) instead of O(history).

Why that is *not* a bolt-on cache today: in the mainline path
(`rust_engine.rs::run_module`) host effects dispatch **synchronously** and a
pause is implemented as an **error unwind** (`PAUSE_MARKER`) that tears the
engine down with the Rust stack. There is no suspended VM to keep. The
engine itself already supports true suspension (async frames block on host
promises; `run_jobs_until_blocked` → `BlockedOnHost`), and the
`SnapshotCapableJsEngine` seam (`snapshot.rs`) + `RustReplayEngine` implement
exactly the suspend/resolve/resume lifecycle — but the mainline agent loop
does not run on it.

The conversion, then, is: route mainline pausable effects through the
host-promise path instead of the synchronous unwind, so a pause leaves a
suspended `RustReplayEngine`; hold it in a bounded, per-thread pool keyed by
run id; on delivery, resolve the promise and continue. Blob-restore (the
current path) remains as the fallback for cache miss, crash, and migration —
and as the **verifier**: a differential mode replays the blob alongside a
warm resume and asserts byte-identical journals, turning the existing replay
machinery into the safety net for its own cache. The journal remains the
source of truth throughout; the warm VM is state the journal can always
reconstruct.

This is a scoped redesign of the pause/dispatch path, not an afternoon
change — it should land behind a flag with the differential verifier on. It
is the single largest remaining resume win: it removes the O(history) term
entirely for the pause→deliver→resume class.

## 6. Order of remaining work, by expected value

1. **Warm-standby conversion (§5)** — removes the O(history) term for live
   resumes.
2. **Lazy / shared-template realm construction** — removes most of the fixed
   ~3.6 ms per engine (`interpreter-optimization.md` §11.4 bonus row).
3. **Per-segment replay-cost tracing** — a `chidori trace` view attributing
   replay time to inter-effect segments, so authors know exactly what to wrap
   in `chidori.step`.
4. **Interpreter data-model work** (shape-keyed inline caches, property-key
   interning) — speeds whatever replay remains; see the research summary in
   `interpreter-optimization.md`/`jit.md`.
