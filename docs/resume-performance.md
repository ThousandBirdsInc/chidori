---
title: "Resume Performance"
description: "Resume cost analysis and optimization notes."
---

# Resume performance: what a resume costs, and how to make it cheap

> **Status:** VM images (§7) are **landed** — a suspended engine now has a
> durable form, so resume is no longer process-local. The two caches below are
> **landed** (branch
> `claude/chidori-js-jit-compiler-btc3ec`); the warm-standby design in §5 is
> **landed** for `input()` pauses on the session server (see §5.1). **Related:** [`docs/value-checkpoints.md`](./value-checkpoints.md)
> (the `chidori.step` memoization primitive),
> [`docs/interpreter-optimization.md`](./interpreter-optimization.md) and
> [`docs/jit.md`](./jit.md) (the retired dispatch-JIT experiment),
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
after compilation — all per-VM state (closures, the tagged-template cache,
the module-capture hook) lives on the VM, not the proto. `tests/replay.rs::
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

The **O(history) re-execution term** is untouched. Bounding re-execution is
`chidori.step`'s job today (`value-checkpoints.md`) and warm-standby's job
tomorrow (§5). The **realm build** has since been attacked directly: see
§4.2.

### 4.1 The realm build, actually measured

`interpreter-optimization.md` §11.1 reported `engine_new` ≈ 3.6 ms on one
developer machine, which framed realm construction as a dominant fixed cost.
A per-section profile on this container (release; the permanent tool is
`cargo run --release --example realm_profile -p chidori-js`, which iterates
the same `builtins::SECTIONS` table `install()` runs) puts it lower and
spreads it thinner:

| section | ms | share |
| --- | ---: | ---: |
| **total `Engine::new()` (min of 20)** | **0.58** | |
| temporal | 0.17 | 29% |
| typedarray | 0.08 | 13% |
| fundamental | 0.06 | 11% |
| numbers | 0.05 | 9% |
| everything else (13 sections) | ≤0.03 each | ~38% |

Two conclusions. First, **re-measure before optimizing**: on this machine the
realm build is ~0.6 ms min-of-N, not 3.6 — still worth removing for
high-frequency resume/test loops, but a fraction of the transpile win, not
four times it.
Second, warm min-of-N **understates the cold cost**: the first realm a fresh
process builds pays first-touch page faults and lazy statics on top (the
`Temporal` section alone measured 0.3–0.7 ms cold vs ~0.2 warm). The
permanent tool for the cold view is
`cargo run --release --example startup_cold -p chidori-js -- <script>`,
which phases one fresh-process run into read / `Engine::new` / eval / print.

### 4.2 Landed: lazy realm sections

The first lever from the list above — lazily materializing rarely-used
namespaces — is in (`builtins/mod.rs::install_lazy_globals`). The four
sections whose objects are reachable only through their global names and
that dominated the build while almost no agent touches them — `Temporal`,
`Intl`, `Date`, and the ArrayBuffer/TypedArray/DataView/Atomics family —
now install as configurable accessor stubs on the global; the first read of
any of a section's names runs the real install, which replaces the stubs in
place (same slots, so global key order matches an eager build) and restores
the ordinary data-property fast paths. Reflection is kept transparent by a
per-realm registry (`realm.lazy_sections`): descriptor reads, `[[DefineOwnProperty]]`,
`__lookupGetter__`/`__lookupSetter__`, and freeze/seal materialize a pending
section before answering, so even `Object.getOwnPropertyDescriptor(globalThis,
"Date")` before first use sees the eager build's data property (pinned by
`tests/lazy_builtins.rs` and the test262 gate). Sections that hang methods off
primitives (`String`, `Number`, `Array`…) cannot be deferred this way —
`"a".toUpperCase()` never reads the global — and stay eager.

Measured cold in a fresh process (release, this container): `Engine::new()`
~0.5 ms, down from ~0.9–1.1 ms eager; whole-process `spawn → eval → exit` on
the near-empty startup workload dropped ~0.4 ms. Scripts that do touch a
deferred namespace pay its install cost once, at first use, instead of every
engine paying all of them up front.

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

### 5.1 Landed: warm input resume (session server)

The conversion above landed in a simpler shape than the suspended-engine
pool: the pause never tears the engine down in the first place. A
`WarmInputBridge` installed on every server run leg intercepts the `input()`
pause AFTER the pending operation is durable (begin + safepoint), surfaces
the same paused `RunResult` the unwind path returns (so the HTTP handler
responds normally), and **parks the engine thread** awaiting the response.
`/resume` delivers through the parked channel: the continuation is O(1) —
no realm rebuild, no transpile, no replay. The parked engine reloads the
durable signal inbox before continuing (deliveries that arrived while
parked), and records the exact journal entry the replay path's synthetic
injection produces, so the durable artifacts are identical either way —
pinned by a differential test (`warm_fallback_replay_produces_identical_journal`).

Degradation is structural, not best-effort: `Park` (eviction deadline
`CHIDORI_WARM_RESUME_EVICT_MS`, capacity, cancel, server shutdown — the
entry's channel disconnecting) unwinds into the classic paused artifact,
and `/resume` falls back to replay, which then re-upgrades the continuation
back onto the warm path. `CHIDORI_WARM_RESUME=0` disables the whole
mechanism. The blob/journal replay path remains the source of truth for
crash recovery. Actor message loops got the equivalent treatment separately
(inline listen-point waits in `host_actor`, `docs/actors.md`).

Measured (debug build, 600-record history, via the HTTP server): resume
23 ms warm vs 213 ms replay — and the warm number is flat in history size
where the replay number grows with it.

## 6. Order of remaining work, by expected value

1. ~~**Warm-standby conversion (§5)**~~ — landed for input pauses (§5.1);
   signal/approval pauses still resume by replay (now fast: the journal
   replay path itself was made O(history) with small constants).
2. ~~**Lazy / shared-template realm construction**~~ — landed for the lazy
   half (§4.2): the four deferrable namespace sections now materialize on
   first use, cutting the cold realm build roughly in half.
   Build-once-clone-many shared templates remain unexplored.
3. **Build-once-clone-many realm templates** — the fixed ~1.4 ms an image
   restore pays to rebuild the eager realm and walk the baseline (§7.3) is now
   the dominant term on that path, and it is the same cost
   `interpreter-optimization.md` §11.4 flags as INVESTIGATE.
4. **Per-segment replay-cost tracing** — a `chidori trace` view attributing
   replay time to inter-effect segments, so authors know exactly what to wrap
   in `chidori.step`.
5. **Interpreter data-model work** (shape-keyed inline caches, property-key
   interning) — speeds whatever replay remains; see the research summary in
   `interpreter-optimization.md`/`jit.md`.

---

## 7. Landed: VM images

> `crates/chidori-js/src/image.rs`

§5.1's warm resume removes the O(history) term only where the process never
died — the paused engine is parked in memory, so the fast path belongs to
whoever started the run. That is exactly the property a fleet cannot have. A
run migrating to another node is the cold path by definition, and the cold
path was replay.

A **VM image** is the durable form of a suspended engine: the live heap,
closures and upvalue cells, promises with their reaction lists, and suspended
async/generator frames — serialized, and rebuilt in a *different* process
without re-executing anything.

### 7.1 Realm-relative, so intrinsics are never serialized

A realm is thousands of objects wired together with native Rust function
pointers. None of that is writable, and none of it needs to be: building an
engine and evaluating the same prelude is deterministic. So an image is taken
against a **baseline**.

`Vm::mark_image_baseline()` walks the realm from `Realm::object_roots()` in a
fixed order and numbers every reachable object, binding cell and symbol. After
that, the image carries only what the *program* creates; anything older is a
`u32`. The restoring VM reaches the same baseline by construction — same
engine, same effect names, same order — and the ids resolve against its own
objects.

That is also how host effects re-bind. The effect functions are native
closures over *this process's* journal state; they are in the baseline, so the
image refers to them by id and the restored program calls the new process's
closures without anything having to serialize a Rust closure.

Two consequences worth knowing:

- **Baseline objects are not frozen.** Top-level `var`s land on the global
  object and scripts do patch `Array.prototype`. Every baseline object is
  fingerprinted at mark time and re-checked at snapshot time; whatever moved
  is written as an **overlay** (its full property table) and reapplied on
  restore. An untouched intrinsic costs one hash.
- **Imaging forces eager realm construction.** Lazily-installed builtin
  sections (§4.2) would otherwise land *after* the baseline, making
  `new Date()` enough to fail an image — and, worse, the restoring side would
  install a different subset because it does not execute the program.
  `mark_image_baseline` materializes them all. The lazy path is unchanged for
  everyone else; this is a one-time build cost per imaging VM, traded for
  making resume history-independent.

Bytecode is not serialized either. `FuncProto`s are addressed as
`(unit, path-of-const-indices)` against compilation units registered with
`Vm::register_image_unit`, and each unit's shape is digested so a recompile
that produced different bytecode fails as a `Mismatch` instead of resuming
wrong.

### 7.2 Partiality is the design

Some live state genuinely has no serialized form: a queued `Microtask::Job` is
a Rust closure, a `new Promise(executor)` hands the program native
resolve/reject functions capturing Rust state, a generator caught mid-step has
its frame on the native stack. `snapshot_image` returns
`ImageError::Unsupported` naming the cause rather than guessing.

That is a routine outcome, not a bug. The image rides *inside* the existing
durable artifact as an additive field:

```rust
DurableBlob { bundle, effects, journal, image: Option<RuntimeImage> }
```

`from_blob` prefers the image and falls back to journal replay whenever it is
absent, stale, or inapplicable — reporting which path it took
(`RestorePath`). The journal remains the source of truth throughout; the image
is a cache over deterministic computation, and a cache miss costs latency,
never correctness. The field is additive, so an artifact carrying an image
still restores in a reader that has never heard of one.

`CHIDORI_VM_IMAGE=0` turns image-writing off.

### 7.3 Measured

`cargo bench -p chidori-js --bench vm_image` resumes the same suspension both
ways across run lengths. A loop that awaits N host effects and then suspends,
with a heap that does not grow:

| journaled effects | journal | image | replay resume | image resume |
|---|---|---|---|---|
| 10 | 0.6 KB | 10.2 KB | 0.5 ms | 5.1 ms |
| 100 | 6.2 KB | 10.2 KB | 2.5 ms | 5.4 ms |
| 400 | 25 KB | 10.2 KB | 3.5 ms | 6.4 ms |
| 1600 | 103 KB | 10.2 KB | 6.9 ms | 5.6 ms |

The image is **flat in history** — 10.2 KB whether the journal is 0.6 KB or
103 KB — which is the property that matters, and the one that a duplicated
journal or an unpruned pending-op map silently destroys (a test pins it:
`image_size_tracks_live_state_not_history`).

Resume time shows the honest trade: the image path pays a fixed ~1.4 ms to
rebuild the eager realm and walk the baseline, so **short runs still resume
faster by replay**, and the crossover here is around a thousand journaled
effects. Replay keeps growing past it; the image does not. The fixed term is
the next lever, and it is the same one already flagged in §4 and
`interpreter-optimization.md` §11.4: build-once-clone-many realm templates
would cut most of it.

### 7.4 What this unlocks

Resume stops being process-local. A suspended agent can be picked up by any
node holding its cell, at a cost set by its live state rather than its age —
which is the prerequisite for treating storage nodes as workers
(`docs/durable-storage.md`). It does not by itself make that fleet exist; the
remaining gaps (materializing agent source on the waking node, per-node config
and secrets, multi-tenant isolation with a warm pool, inbound routing) are
unchanged.

### 7.5 Not covered yet

Extending the format is additive — add an `IntImg` arm and its decode. Today's
refusals:

- `Internal::Temporal` (opaque `temporal_rs` slot) and `IteratorHelper`.
- Native closures created after the baseline — chiefly promise-executor
  resolve/reject functions held across a suspension, and proxy revokers.
- `Microtask::Job` in the queue, and generators in `Executing`.
- Baseline objects whose *internal slot* changed kind (nothing in script does
  this today; refused so the format stays honest if that changes).
