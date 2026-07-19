---
title: "Branching Execution"
description: "chidori.branch sub-runs: fork a run into per-strategy variants from the current state and compare every outcome."
---

# Branching execution

`chidori.branch(variants)` lets an agent **fork itself mid-run** into N
branches. Each branch explores a strategy from the *same anchored state*, runs
its **own editable source**, can **pause**, and returns an outcome so the agent
(or a human) can **compare and pick one**. Because the shared prefix is
identical across branches, a branch is a *controlled experiment*: the only
variable is the branch's code/input — and each branch streams as its own
subtree in an OTLP trace viewer for side-by-side comparison.

This exists because iterating on agents is uniquely painful: a run is a long
chain of steps that are expensive (LLM/tool calls cost money and seconds),
stochastic (same prompt → different output), and stateful (step N depends on
all prior steps). The default loop — change code, re-run the whole thing —
re-pays for the entire prefix, and because the model is stochastic the prefix
comes out *different*, so you cannot tell whether your change helped or the
randomness moved. Branching turns durable execution into the workflow agents
actually want: anchor at a decision point, reuse the prefix for free (its
result is the shared starting state), vary one thing per branch, run each
branch and compare.

A branch is a **separate continuation source run once** — not a re-run of the
parent. Re-running the parent's source would re-reach `chidori.branch` and
recurse, so the prefix is **handed over as state** (the parent's captured VFS
plus an explicit `input`), not replayed. Branches act on the *result* of the
prefix; they don't re-derive it. Each branch is a new durable sub-run: the
orchestrator runs each variant's module on a fresh `RuntimeContext` seeded from
the parent's anchor, collects the outcomes, and returns them. The whole fan-out
is one recorded host call on the parent, so the parent's own replay returns the
outcomes from cache.

The orchestration lives in `crates/chidori/src/runtime/host_branch.rs`; the SDK
types live in `sdk/typescript/src/agent.ts`. See also
[`docs/architecture.md`](./architecture.md) and
[`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md).

## The agent-facing API

```ts
type BranchVariant = {
  /** Branch label (shown in outcomes + trace). Defaults to `branch-<k>`. */
  label?: string;
  /** Branch source module path, resolved like `callAgent` paths. Required. */
  source: string;
  /** State handed to the branch as its run input. Defaults to `{}`. */
  input?: AgentJson;
};

type BranchOutcome = {
  label: string;
  branchId: string;              // <parent run id>-op<branch seq>-branch-<k>
  status: "completed" | "paused" | "failed";
  output?: AgentJson;            // when completed
  pendingPrompt?: string;        // when paused (e.g. a chidori.input prompt)
  error?: string;                // when failed
};

// On the chidori object:
branch(variants: BranchVariant[], options?: {
  concurrency?: number;          // max branches running live at once (cost cap)
}): Promise<BranchOutcome[]>;
```

- `source` is **required**: a branch runs its own continuation module, never a
  copy of the parent (which would re-reach `chidori.branch` and recurse).
  Paths resolve like `callAgent` paths — relative to the working directory.
- At most **16 variants** per call: every branch makes live host calls past the
  fork (real LLM/tool spend), so an unbounded fan-out is a cost hazard before
  it is a correctness one.
- Every variant is validated (and its source read) **before any branch runs**,
  so a missing `source` or a typo'd path fails the whole call without spending
  anything — and without recording anything.
- Returns **all** outcomes (compare, don't merge). The agent runs its own
  selection: `const best = outcomes.reduce(pick);`
- A `paused` outcome carries a `branchId` the host can resume out-of-band (see
  below), keeping the JS surface a single awaited Promise.

## How it works

The `branch` call executes inside the durable boundary as a single recorded
call whose result is the outcomes array — on parent replay it returns cached
and the branches never re-run. For each variant, the orchestrator:

1. Reserves a disjoint `CallLogSequenceRange` via `ParallelBranchManifest`
   (width 10,000 per branch). The slot is derived from the `branch` call's own
   seq — `slot = seq / (width × count) + 1`, `base = slot × width × count` —
   so successive branch ops in one run get reserved blocks that grow linearly
   and stay disjoint from each other and from every earlier record.
2. Builds a **fresh `RuntimeContext`** seeded with the parent's VFS snapshot,
   the reserved range as its sequence base, a call stack seeded with the
   parent's `branch` seq (so branch records nest), and the parent's OTEL run
   span (so branch spans stream under the parent's `branch` span).
3. Runs the branch's source module with the variant's `input` — live, through
   the same host-effect path as any run, under the same `PolicyConfig`.
4. Settles the outcome — `completed` with the module's return value, `paused`
   with the pending prompt when the branch suspended on a host op, or `failed`
   with the error — validates that every record the branch produced sits
   inside its reserved range, and folds the branch's records into the parent
   log. The fold advances the parent's counter past the reserved ranges the
   same way `absorb_replayed_subtree` does on replay, so live and replayed
   sequence numbering stay aligned.

Variants run in **waves of `options.concurrency` worker threads** (default 1 —
sequential; clamped to the variant count). Each branch gets its own JS VM and
context inside its thread; settling, validation, merging, and persistence
happen back on the parent thread, in variant order, after the wave joins — so
the durable log and the outcomes array are deterministic regardless of
completion order. Branches are in-process workers under the parent run's slot;
the server's run semaphore is not involved.

**Nested `chidori.branch` inside a branch is rejected**: a nested fork would
allocate sequence ranges outside the parent branch's reserved range. The
rejection surfaces as a `failed` outcome for that branch.

Tracing is free: branch records carry `parent_seq` = the `branch` call's seq,
so with `OTEL_EXPORTER_OTLP_ENDPOINT` set the operator sees the fork live as a
`branch` span with one child subtree per strategy, side by side.

## The branch store

When the parent run persists (`.chidori/runs/<run id>/`), every branch sub-run
is persisted under it:

```text
<run dir>/branches/op-<branch seq, zero-padded to 20 digits>/
  anchor.json              fork-time anchor: the parent VFS snapshot
  branch-<k>/
    source.ts              the branch's own EDITABLE source copy
    checkpoint.json        the branch's call log (same shape as a run's)
    branch.json            metadata: label, id, status, pending input,
                           reserved sequence range, input, output/error
```

The anchor and the per-branch source copies are written **before** the fan-out
runs, so even a crash mid-fan-out leaves re-runnable branch stores behind. The
`branchId` (`<run>-op<seq>-branch-<k>`) maps 1:1 to the store path.

## Resume and edit-and-rerun

The store makes a branch independently operable out-of-band, after the parent
has moved on:

```bash
# List a run's persisted branches and their states:
chidori branches <run-id>

# A branch paused on chidori.input()? Answer it:
chidori branch-resume <run-id> <branch-id> --value "blue"

# Edit a strategy and re-run ONLY that branch from the same anchored state:
$EDITOR .chidori/runs/<run-id>/branches/op-*/branch-001/source.ts
chidori branch-rerun <run-id> <branch-id>
```

Both commands default their model to the one recorded in the parent run's
manifest (override with `--model` or `CHIDORI_MODEL`), and accept
`--trusted`/`--untrusted` for the branch's live gated effects — the same
posture flags as `chidori run`.

- **Resume** replays the branch's checkpoint with a synthetic `input` record at
  the pending seq (the same mechanism the server's `/resume` uses), then runs
  the branch's `source.ts` live to its next outcome. Resume answers `input()`
  pauses; approval/signal pauses are reported but not resumable out-of-band.
- **Edit-and-rerun** discards the previous checkpoint and re-runs the branch
  **fresh from the parent anchor** with whatever `source.ts` now contains. The
  anchored state (fork-time VFS + the variant's `input`) is identical to the
  original fork, so only the branch's code is the variable. Branch runs never
  go through the run manifest's source-hash gate — the anchor is the captured
  state, not a source identity check.

A resumed or re-run branch updates only its own store; the parent's recorded
`branch` outcome is immutable history (compare, don't merge).

## Correctness and determinism

- **Parent determinism:** the `branch` op is recorded with the outcomes as its
  result; parent replay short-circuits the fan-out like any cached host call.
- **No record collisions:** reserved per-branch sequence ranges guarantee
  disjointness, and every branch record is validated against its range before
  it joins the parent's durable log — a violation (e.g. a branch that outgrew
  its range width) fails the call rather than corrupting the log.
- **Nesting integrity:** branch records carry `parent_seq` = the `branch`
  call's seq, matching the established nesting invariant.
- **Branch determinism:** a persisted branch is replayable from its own stored
  checkpoint; resume is checkpoint replay plus live continuation, confined to
  the same reserved range.
- **State-handover fidelity:** branches inherit the VFS plus the explicit
  `input`, not the parent's in-flight JS locals. The agent passes what a
  branch needs.

## Cost, safety, concurrency

N branches make N sets of **live** host calls past the fork — real LLM/tool
spend. The controls: `options.concurrency` caps simultaneous live branches
(default 1), the fan-out is hard-capped at 16 variants, and each branch
context enforces the same policy layer as the parent. Branches use separate
`RuntimeContext`s and separate VMs on separate threads; no shared mutable
state.

## Example

[`examples/branching/`](../examples/branching/) is a runnable end-to-end
example: shared research once, a two-strategy fork, compare-and-pick, replay
via `chidori resume`, and the resume/edit-and-rerun workflows against the
branch store.
