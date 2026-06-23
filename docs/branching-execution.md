# In-Agent Execution Branching — Design Doc & Implementation Plan

> **Status:** Phases 1 and 2 **implemented** — `chidori.branch` ships as a host
> effect (`src/runtime/host_branch.rs`, dispatched from `bindings.rs`), with
> persisted branch stores, out-of-band resume and edit-and-rerun (CLI:
> `chidori branches` / `branch-resume` / `branch-rerun`), concurrency-capped
> wave execution, SDK types, tests, and `examples/branching/`. Phase 3 (the
> whole-agent replay-prefix model) remains future work.
> **Target engine:** the doc was drafted before the QuickJS removal (#39);
> `chidori-js` is now the only engine, so the `CHIDORI_JS_ENGINE=rust` /
> `rust-engine`-feature framing below is historical. Implementation notes where
> the shipped version deviates from the draft:
> - Branch `source` paths resolve like `callAgent` paths (relative to the
>   working directory) — the host backend doesn't track the parent agent's
>   path. `source` is **required** ("omit to reuse parent" would re-reach
>   `chidori.branch` and recurse, §8.2).
> - `branchId` is `<parent run id>-op<branch seq>-branch-<k>` (not
>   `<parent run id>-branch-<k>`) so ids stay unique when one run forks more
>   than once; it maps 1:1 to the branch-store path.
> - Sequence ranges still come from `ParallelBranchManifest::with_sequence_width`,
>   but the slot id is derived from the parent's `branch`-call seq
>   (`slot = seq / (width × count) + 1`) instead of a host-promise op id, so
>   successive branch ops' reserved blocks grow linearly and stay disjoint.
>   After the fan-out the branch records are folded into the parent log and the
>   parent's counter advances past them — exactly what
>   `absorb_replayed_subtree` reproduces on replay, keeping live and replayed
>   sequence numbering aligned.
> - Nested `chidori.branch` inside a branch is rejected (`is_branch` on the
>   branch `RuntimeContext`): a nested fork would allocate ranges outside the
>   parent branch's reserved range.
> - Phase 2 persistence is **replay-native**, not VM snapshots: the QuickJS-era
>   `save_live_parallel_branch_runtime_snapshot` / `resume_live_parallel_branch_from_store`
>   blob helpers were not used (resume is call-log replay everywhere post-#39).
>   Each branch persists `source.ts` + `checkpoint.json` + `branch.json` under
>   `.chidori/runs/<run>/branches/op-<seq>/branch-<k>/`, with the fork-time VFS
>   in a per-op `anchor.json`. Resume = replay the branch checkpoint with a
>   synthetic `input` record (the server's `/resume` mechanism); edit-and-rerun
>   = fresh run from the anchor with the stored (editable) source. The "relaxed
>   `ensure_sources_match` loader" is moot — branch runs never go through the
>   manifest source gate. Resume answers `input()` pauses; approval/signal
>   pauses are reported but not yet resumable out-of-band.
> - Concurrency runs variants in waves of `options.concurrency` OS threads
>   (default 1 — sequential), settling/merging in variant order after each wave
>   joins, so the durable log and outcome order are deterministic regardless of
>   completion order. (The server's `run_semaphore` is not involved — branches
>   are in-process workers under the parent run's slot.)
> - A resumed/re-run branch updates only its own store; the parent's recorded
>   `branch` outcome is immutable history (compare, don't merge).
> **Related:** [`docs/pure-rust-js-engine-plan.md`](./pure-rust-js-engine-plan.md),
> [`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md).

---

## 1. Summary (TL;DR)

Add a runtime primitive, `chidori.branch(variants)`, that lets an **agent fork
itself mid-run** into N branches. Each branch explores a strategy from the *same
anchored state*, runs its **own editable source**, can **pause**, and returns an
outcome so the agent (or a human) can **compare and pick one**. Because the shared
prefix is identical across branches, a branch is a *controlled experiment*: the only
variable is the branch's code/input — and each branch streams as its own subtree in
tael for side-by-side comparison.

This is `chidori.parallel` generalized along three axes: (1) per-branch **editable
source** (not just inline closures), (2) **pausable/persistable** branches, (3)
**outcomes returned for selection** rather than auto-merged. ~80% of the machinery
already exists (the parallel-branch snapshot scaffolding, `host_core` durable-call
boundary, reserved seq ranges, streaming spans); the new work is the primitive, the
orchestrator, per-branch source storage, and pause/edit/re-run.

---

## 2. Motivation (the "why")

Iterating on agents is uniquely painful: a run is a long chain of steps that are
**expensive** (LLM/tool calls cost money + seconds), **stochastic** (same prompt →
different output), and **stateful** (step N depends on all prior steps). The default
loop — *change code → re-run the whole thing* — re-pays for the entire prefix, and
because the model is stochastic the prefix comes out *different*, so you cannot tell
whether your change helped or the randomness moved. There is no controlled variable.

Chidori already holds the cure: **durable execution**. Every host effect is recorded
in a `call_log` keyed by a monotonic `seq`; the prefix `[0..N]` is reproducible.
Branching turns that latent property into the workflow agents actually want:

- **Anchor** at a decision point.
- **Reuse the prefix for free** (its result is the shared starting state).
- **Vary one thing** — the branch's code/input from that point on.
- **Run each branch and compare** — which is exactly what the streaming OTEL→tael
  tracing already built is for.

The framing is an **in-agent runtime primitive** ("the agent forks itself to explore
strategies and pick one"), not a developer-only CLI debugger. The branches are
pausable and each has its own source that can be run and modified independently.

---

## 3. Goals / Non-Goals

**Goals**
- An agent can call `chidori.branch(variants)` to fork into N branches from its
  current state and receive each branch's outcome.
- Each branch runs its **own source module**, editable and independently runnable.
- Branches are **pausable** (suspend on a host op, persist, resume later).
- Branches **nest in the trace** (a `branch` span with one subtree per branch) and
  stream to tael as they run.
- Determinism preserved at the parent level: a branch op is recorded, so the parent's
  own replay does not re-run the fan-out.
- Reuse the existing parallel-branch + `host_core` + snapshot machinery; do not fork a
  parallel subsystem.

**Non-Goals (initially)**
- Auto-merging branch results back into one timeline (we **compare**, not merge; the
  agent's `pick()` chooses which branch's output to use).
- Cross-branch shared mutable state / live message passing between branches.
- The QuickJS engine path (rust engine first; QuickJS is a later port if wanted).
- A general "rewind to arbitrary seq + edit the whole agent + replay" dev tool — that
  is the **phase 3** richer model (§8.9), staged behind soft-divergence.

---

## 4. Background (verified, with file references)

### 4.1 Durable-call boundary & the call log
`src/runtime/host_core.rs::execute_durable_json_call(ctx, function, args, live)` is the
single durable boundary: it `ctx.next_seq()`s, checks `try_replay_checked(seq, fn)`
(cache hit → return recorded result; miss → run `live()`), wraps `live()` in
`ctx.enter_call(seq)`/`exit_call(seq)` (so nested calls get `parent_seq` stamped), then
`ctx.record_call(CallRecord{ seq, parent_seq, function, args, result, … })`.
`execute_tool_call`, `execute_input`, `execute_call_agent` are thin wrappers. The
record stream is the durable substrate; `CallRecord` lives in
`src/runtime/call_log.rs`.

### 4.2 The rust-engine agent path (what `chidori.branch` plugs into)
`crates/chidori/src/runtime/rust_engine.rs`: `run_agent` → `run_module(path, source, fallback, input,
ctx, tools)`. `run_module` transpiles TS→JS, builds a **plain `chidori_js::Engine`**,
installs the `chidori` host object (`install_chidori_effects(make_dispatch(ctx.clone(),
tools.clone()))`) whose methods route through `RuntimeBindingBackend::dispatch(ctx, tools, effect, args)`
(in `crates/chidori/src/runtime/typescript/bindings.rs`) → `host_core` on the **shared `RuntimeContext`**, installs the `run(handler)` entrypoint,
and runs it. **Key fact:** on this path the durable record is the `RuntimeContext`
`call_log` (global `seq`) — *not* the `crates/chidori-js/src/replay.rs` journal (that
journal is only used by the `RustReplayEngine` snapshot seam). So branching here is
naturally expressed over the call_log, and a branch sub-run is just `run_module` on a
fresh/seeded `RuntimeContext`.

### 4.3 Replay & divergence
`src/runtime/context.rs::try_replay_checked(seq, fn)` returns `Ok(Some(record))` on a
matching cache hit, `Ok(None)` on a miss (→ run live), and `Err(...)` on a
**function-name mismatch** ("Replay divergence at seq N…"). `try_replay` /
`absorb_replayed_subtree` push replayed records into the active log. The `replay.rs`
journal (rust engine snapshot seam) has the analogous `cursor`/`Frontier`/`Diverged`
model. These are the hooks the phase-3 soft-divergence model edits (§8.9).

### 4.4 Existing parallel-branch scaffolding (the precedent to reuse)
`src/runtime/snapshot.rs`:
- `ParallelBranchManifest::with_sequence_width(parent_run_id, parallel_op_id,
  branch_count, requested_concurrency, branch_sequence_width)` — reserves a **disjoint
  `CallLogSequenceRange` per branch** (`base = parallel_op_id * width * branch_count`;
  each branch gets `[base + i*width + 1, … + width)`), so branch records never collide.
- `start_live_parallel_branch_runtimes<E: SnapshotCapableJsEngine>(store, manifest,
  …) -> Result<Vec<E>>` — saves the manifest, loads the parent snapshot, and
  `E::restore(&parent_blob)` once per branch.
- `save_live_parallel_branch_runtime_snapshot<E>(store, manifest, branch_index,
  runtime, …, pending, call_log)` — snapshots a branch and writes it under
  `store.branch_store(manifest, branch_index)` (`.chidori/runs/<id>/branches/op-<n>/branch-<k>/`)
  with `SnapshotBranchMetadata { parent_run_id, parallel_op_id, branch_index,
  branch_operation_id }`.
- `resume_live_parallel_branch_from_store<E>(…, host_operation_id, value) ->
  Result<(E, JsRunState)>` and `reject_live_parallel_branch_from_store<E>` — restore a
  paused branch, resolve/reject its pending host op, run to the next block.
- `merge_parallel_branch_outcomes(manifest, outcomes) -> ParallelMergeResult` —
  validates each branch's records fall in its reserved range and concatenates.
- QuickJS precedent: `src/runtime/typescript/snapshot.rs::run_parallel_branches_from_snapshot`
  forks the parent context (`fork_context` = `restore_context`), `call_agent(input)`
  per branch, collects outputs. (Per-branch call-log capture there is still a stub —
  it merges empty logs; the rust-engine design below captures real per-branch logs.)

### 4.5 State that survives a fork
`SnapshotManifest` (snapshot.rs) carries `entry`/`modules` `SourceFingerprint`,
`pending`, `host_promises`, **`vfs`** (in-memory captured filesystem), `capabilities`,
`call_log_len`, and optional `branch` metadata. The VFS + host-promise table + the
parent's `call_log` are what a branch inherits as its anchored state.

### 4.6 Tracing
`record_call` → `RunSpan::stream_record` streams each call's span during the run,
nesting by `parent_seq` (the OTEL `parent_span_id` is the only thing tael reads). A
`branch` host call therefore appears as a span, and each branch's calls nest under it
automatically — no extra tracing work.

---

## 5. Design overview

A branch is **a new durable sub-run that shares the parent's anchored state and then
runs its own source.** `chidori.branch` is a recorded host call on the parent; inside
it, the orchestrator runs N branch sub-runs (each `run_module` on a fresh
`RuntimeContext` seeded from the parent's VFS/memory + a reserved seq range), collects
their outcomes, and returns them to the agent. The parent records the branch op's
result, so the parent's own replay returns it cached.

The decisive design question — *does a branch re-run an edited copy of the **whole
agent** (replaying the prefix), or run a **separate continuation source** once?* — is
resolved in §8.2 in favor of separate continuation sources for the MVP, because
re-running the whole agent re-reaches `chidori.branch` and recurses. The whole-agent
replay-prefix model is real and powerful and is staged as phase 3 (§8.9) behind two
extra mechanisms (soft divergence + a `fork()`-style return).

---

## 6. Detailed design — API surface

### 6.1 Agent-facing (`chidori.branch`)
```ts
import { chidori } from "chidori:agent";

type BranchVariant = {
  /** Branch label (shown in outcomes + trace). */
  label: string;
  /** Branch source module path (relative to the agent), or omitted to reuse parent. */
  source?: string;
  /** State handed to the branch as its run input. */
  input?: Json;
};

type BranchOutcome = {
  label: string;
  branchId: string;              // <parent_run_id>-op<branch_seq>-branch-<k>
  status: "completed" | "paused" | "failed";
  output?: Json;                 // when completed
  pendingPrompt?: string;        // when paused (e.g. chidori.input)
  error?: string;                // when failed
};

// On the chidori object:
branch(variants: BranchVariant[], options?: {
  concurrency?: number;          // max branches running live at once (cost cap)
}): Promise<BranchOutcome[]>;
```
- Returns **all** outcomes (compare, don't merge). The agent runs its own selection:
  `const best = outcomes.reduce(pick);`
- A `paused` outcome carries a `branchId` the host can resume out-of-band (§8.6),
  keeping the JS surface a single awaited Promise (mirrors `chidori.parallel`).

### 6.2 SDK types
Add `BranchVariant`, `BranchOutcome`, and the `branch` method to the `Chidori`
interface in `sdk/typescript/src/agent.ts`; export nothing new beyond types (the
runtime supplies `chidori.branch`, like the other methods).

---

## 7. Detailed design — chidori-js host binding

In `crates/chidori-js/src/lib.rs::install_chidori_effects`, add a `branch` method that
marshals `(variants, options)` to JSON and calls the dispatcher:
```rust
self.vm.define_method(&chidori, "branch", 2, move |vm, _t, args| {
    let variants = args.first().map(|v| vm.value_to_json(v)).unwrap_or(Null);
    let options  = args.get(1).map(|v| vm.value_to_json(v)).unwrap_or(Null);
    forward_effect(vm, &d, "branch", json!({ "variants": variants, "options": options }))
});
```
The result JSON (`BranchOutcome[]`) becomes the awaited value. No other chidori-js
changes for the MVP (the journal `Mode::Branch` is phase 3 only).

---

## 8. Detailed design — orchestration (rust engine)

### 8.1 Dispatch
`crates/chidori/src/runtime/typescript/bindings.rs::dispatch` gains `"branch" => host_branch::run_branches(ctx,
tools, args)`. `run_branches` wraps the whole fan-out in the durable boundary so it's
one recorded, nested call:
```rust
host_core::execute_durable_json_call(ctx, "branch", args.clone(), || {
    // live(): fork + run branches, return BranchOutcome[] as JSON
})
```
On parent replay this returns cached → branches don't re-run.

### 8.2 The recursion trap → continuation-source model
If a branch re-ran the parent's **full** source, it would re-reach `chidori.branch`
(which is *not* yet in the prefix — `execute_durable_json_call` records *after*
`live()` returns) and fork again → infinite recursion. Therefore a branch is a
**separate continuation source run once**, not a re-run of the parent. Consequences:
- "Two editable source versions" are first-class: each branch is its own file under
  `branches/`.
- No re-run ⇒ no recursion, and the prefix is **handed over as state** (the parent's
  VFS/memory + explicit `input`), not replayed. For "explore strategies from here"
  that's the correct semantics — branches act on the result of the prefix, they don't
  re-derive it.
- Reuses `execute_call_agent`-style sub-run machinery (a branch ≈ a sub-agent that
  inherits the parent's captured state and a reserved seq range).

### 8.3 Per-branch sub-run
For each variant (sequentially in the MVP; up to `options.concurrency` later):
1. Allocate the branch via `ParallelBranchManifest::with_sequence_width(parent_run_id,
   branch_op_id, n, concurrency, width)` → a reserved `CallLogSequenceRange`.
2. Build a **fresh `RuntimeContext`** seeded with: the parent's `vfs_snapshot()`, the
   parent's memory, the reserved seq range as its base, and the same `otel_run`
   `RunSpan` (so branch spans stream under the parent's `branch` span). Branch calls go
   **live** through `host_core` (real effects — the exploration cost; gated by
   concurrency).
3. Run the branch source: `run_module(branch_source_path, src, "agent"/run, input,
   branch_ctx, tools)`.
4. Capture `BranchOutcome { label, branchId, status, output?, pendingPrompt?, error? }`
   and the branch's own `call_log` (within its reserved range).

### 8.4 Seq ranges, nesting, determinism
- Reserved ranges (`with_sequence_width`) keep branch records disjoint from the parent
  and each other; the parent's `seq` advances past the whole branch op (analogous to
  `absorb_replayed_subtree` for cached subtrees).
- Branch records carry `parent_seq` = the `branch` call's seq → the trace nests
  correctly; tael shows a `branch` span with one subtree per branch.
- The parent's `branch` record's `result` = the outcomes array, so a later **parent
  replay** returns the fan-out from cache (no re-execution). Each *branch's own*
  determinism (its replayability) comes once it has a stored journal (§8.6, phase 2).

### 8.5 Per-branch editable source + storage
Reuse `SnapshotStore::branch_store(manifest, branch_index)` →
`.chidori/runs/<parent>/branches/op-<n>/branch-<k>/`. Persist per branch: `source.ts`
(the editable copy, seeded from the variant or the parent), `checkpoint.json` (the
branch's call_log), and (phase 2) the branch snapshot blob + manifest. Editing
`source.ts` and re-running the branch re-anchors to the same parent state.

### 8.6 Pause / resume / edit-and-rerun (phase 2)
- **Pause:** a branch that suspends on a host op (e.g. `chidori.input`) persists via
  `save_live_parallel_branch_runtime_snapshot` (rust-engine blob = the branch's
  `DurableBlob{ bundle = branch source, journal = prefix+live-so-far }`); the outcome
  is `status:"paused"` + `branchId`.
- **Resume:** `resume_live_parallel_branch_from_store(…, host_op_id, value)` restores
  the branch, resolves the pending op, runs to the next block.
- **Edit-and-rerun:** re-run the branch from the parent anchor with the edited
  `source.ts`. Because branch source intentionally differs, the loader must **skip
  `SnapshotManifest::ensure_sources_match` for branches** (a relaxed branch loader) —
  the anchor is the captured state, not a source-hash identity check.

### 8.7 Outcome collection & selection
`run_branches` returns `BranchOutcome[]`; the agent compares and picks. The picked
branch's output is just a value the parent continues with. (If a future use case wants
the picked branch's *tail effects* folded into the parent timeline, that's a merge
step on top of `merge_parallel_branch_outcomes` + the reserved ranges — explicitly out
of scope for v1, see Non-Goals.)

### 8.8 Tracing integration
Free, via the streaming-span work: the `branch` call streams a span; each branch's
`record_call`s stream under it by `parent_seq`. With `OTEL_EXPORTER_OTLP_ENDPOINT` set
(tael), the operator sees the fork as a `branch` span with N child subtrees, live.

### 8.9 Phase 3 — the whole-agent replay-prefix model (richer, deferred)
For the "edit a copy of the **whole agent** and replay the expensive prefix" flavor
(closer to time-travel debugging), add:
- **Soft divergence:** a branch-mode flag so `try_replay_checked` returns `None` (drop
  the replay log, continue live) on a function-name mismatch instead of erroring — so
  an edit *past* the fork transitions replay→live cleanly; an edit *before* the fork
  diverges gracefully at that point. Mirror in `replay.rs`: a `Mode::Branch` where
  `Decision::Diverged` **truncates the journal at the cursor + flips to record** (add
  `Journal::truncate(cursor)`).
- **`fork()`-style return:** mark the branch ctx `is_branch = Some(k)`; when a re-run
  reaches `chidori.branch` it returns the branch identity instead of recursing (Unix
  `fork()` semantics). The branch then runs its (edited) post-fork tail.

This recovers the parent's JS locals by re-running (replaying the prefix from cache),
at the cost of the fork-return ergonomics. Deferred because §8.2's model already
satisfies the stated goal.

---

## 9. Correctness & determinism analysis
- **Parent determinism:** the `branch` op is recorded with the outcomes as its result;
  parent replay short-circuits the fan-out (like any cached host call). ✓
- **No record collisions:** reserved per-branch `seq` ranges (`with_sequence_width`)
  guarantee disjointness; `merge_parallel_branch_outcomes` already validates this. ✓
- **Nesting integrity:** `enter_call`/`exit_call` around the `branch` `live()` stamps
  `parent_seq` on branch records, matching the established nesting invariant
  (`absorb_replayed_subtree`). ✓
- **Branch determinism:** a branch is replayable from its own stored journal/log once
  persisted (phase 2). Until then a branch is a one-shot live exploration. (Documented.)
- **State-handover fidelity:** branches inherit VFS/memory + explicit `input`, not the
  parent's in-flight JS locals. The agent passes what a branch needs. (The phase-3
  replay model re-derives locals by re-running.) (Documented limitation.)

## 10. Cost, safety, concurrency
- N branches make N sets of **live** host calls past the fork (real LLM/tool spend).
  Gate with `options.concurrency` (max simultaneous live branches) + a hard branch cap
  + the existing policy layer (each branch ctx enforces the same `PolicyConfig`).
- Branches use **separate `RuntimeContext`s** (isolation-ready). MVP runs them
  sequentially; concurrency is a later enablement using the existing `run_semaphore`.
- No new global mutable state.

## 11. Alternatives considered
- **`chidori.parallel` only:** branches as inline closures, auto-merged. Rejected — no
  editable per-branch *source*, no pause, merge-not-compare.
- **`chidori.callAgent` in a loop:** close, but no shared anchor/seq-range bookkeeping,
  no pause/persist, no comparison structure. (We *reuse* its sub-run mechanism.)
- **Live VM-image snapshot to continue in place:** would avoid the recursion trap
  cleanly, but the rust engine has no mid-call live-VM image (its snapshot is the
  journal/re-run blob), so "restore + continue from the fork" isn't available without
  the phase-3 work. The QuickJS engine *does* have live-VM snapshots (a future port).
- **Replay-prefix-as-MVP (§8.9 first):** more literal "two versions of the agent," but
  the recursion/fork-return ergonomics make it a worse first step. Staged as phase 3.

## 12. Implementation plan (phased)

**Phase 1 — MVP: synchronous branching, outcomes returned** ✅ **shipped**
- [x] `crates/chidori-js/src/lib.rs`: `branch` added to `install_chidori_effects` (§7),
  marshalling `(variants, options)`.
- [x] `src/runtime/host_branch.rs`: `run_branches(backend, args)` — reserved ranges via
  `ParallelBranchManifest::with_sequence_width` (slot derived from the branch call's
  seq), per-branch fresh `RuntimeContext` seeded from parent VFS, native
  `run_agent_file` per variant, `BranchOutcome[]` collected, all inside
  `execute_durable_json_call_at_seq(ctx, seq, "branch", …)`. Dispatched from the
  `"branch"` arm in `bindings.rs` (the engine's effect dispatcher).
- [x] `src/runtime/context.rs`: `RuntimeContext::for_branch` (parent VFS + config +
  input mode + shared OTEL run span/event sink, reserved base seq, call stack seeded
  with the parent `branch` seq so records nest), `is_branch`, and
  `merge_branch_records` (folds branch records into the parent log without
  re-emitting, advancing the counter the way replay's `absorb_replayed_subtree` does).
- [x] `sdk/typescript/src/agent.ts`: `BranchVariant`/`BranchOutcome`/`BranchOptions` +
  `branch` on `Chidori`.
- [x] Tests (`src/runtime/host_branch.rs`): (a) two outcomes with the right outputs,
  (b) branch records carry `parent_seq` = the `branch` seq, (c) records land in
  disjoint reserved ranges, (d) a counting native tool proves the parent prefix fired
  once (handed over, not re-run) — plus parent replay returning cached outcomes
  without re-running branches, a paused-branch outcome, nested-branch rejection, and
  fail-fast variant validation.
- [x] Example: `examples/branching/` — shared research, two strategy modules, compare
  and pick; replays via `chidori resume`.

**Phase 2 — pausable + editable + persisted branches** ✅ **shipped**
(replay-native adaptation — see the status header for the deltas from this draft)
- [x] Persist each branch under `.chidori/runs/<run>/branches/op-<seq>/branch-<k>/`
  (`source.ts`, `checkpoint.json`, `branch.json`; fork-time VFS in the per-op
  `anchor.json`). The anchor and source copies are written *before* the fan-out
  runs, so a crash mid-fan-out still leaves re-runnable stores.
  `status:"paused"`+`branchId` outcomes carry the resume handle.
- [x] Resume: `resume_branch` (host_branch.rs; `Engine::resume_branch`;
  `chidori branch-resume <run> <branch-id> --value …`) — synthetic `input`
  record + checkpoint replay, continuing live to the next outcome.
- [x] Edit-and-rerun: `rerun_branch` (`Engine::rerun_branch`;
  `chidori branch-rerun <run> <branch-id>`) — fresh run from the anchor with
  the edited `source.ts`; `ensure_sources_match` never applies to branch runs.
- [x] `chidori branches <run>` lists a run's persisted branch stores.
- [x] Concurrency (from §15): `options.concurrency` runs variants in waves of
  worker threads; outcome order and the merged log stay variant-ordered.
- [x] Tests: pause a branch on `chidori.input`, resume with a value out-of-band,
  assert continuation + store update + range confinement; edit a branch source,
  re-run, assert divergent output from the same anchor (fork-time VFS + input
  preserved); a rendezvous tool proves real overlap under `concurrency: 2`.

**Phase 3 — whole-agent replay-prefix model (optional, §8.9)**
- `try_replay_checked` branch-mode soft divergence; `replay.rs` `Mode::Branch` +
  `Journal::truncate`; `fork()`-style `chidori.branch` return.
- Tests: an edited copy of the whole agent replays the prefix from cache and diverges
  to live at the edit; recursion is bounded by the fork-return.

## 13. Verification & rollout
- `cargo check -p chidori` = 0 errors **with and without** `rust-engine`; QuickJS path
  untouched (`cargo test -p chidori --lib` green).
- `cargo test -p chidori --features rust-engine --lib rust_engine` green (new branch
  tests).
- Manual: `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 CHIDORI_JS_ENGINE=rust
  cargo run --features rust-engine -- run examples/branching/agent.ts` → confirm a
  `branch` span with two child subtrees in tael, and two outcomes printed.
- Determinism guard: `chidori-js` Test262 baseline unchanged (branching is additive,
  off unless the agent calls it).

## 14. Open questions
- **Branch input ergonomics:** is explicit `input` state-passing enough, or do we want
  a captured-closure convenience (phase-3 replay model recovers locals automatically)?
- **Selection-as-continuation:** should a picked branch's *effects* be foldable into
  the parent timeline (a real merge), or do branches stay independent referenced
  sub-runs? (v1: independent + compare.)
- **Concurrency default:** sequential MVP; what's the safe default `concurrency` and
  hard cap given live LLM spend?
- **Cross-engine:** is a QuickJS port wanted (it has live-VM snapshots, enabling the
  §8.9 "continue in place" without re-run)?

## 15. Future work
- Merge/promote a chosen branch into the parent run.
- ~~Concurrent branch execution via `run_semaphore`.~~ Done — in-process worker
  threads capped by `options.concurrency` (see the status header).
- A comparison view in tael keyed on the `branch` span (diff branch subtrees).
- Server/SDK surface (`POST /sessions/{id}/fork`, `session.fork()`) if a programmatic
  (non-in-agent) driver is later wanted.
- QuickJS live-VM "continue in place" branching.
