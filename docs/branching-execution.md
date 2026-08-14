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
recurse, so the prefix is **handed over as state** (the parent's captured
workspace state plus an explicit `input`), not replayed. Branches act on the
*result* of the prefix; they don't re-derive it. Each branch is a new durable
sub-run seeded from the parent's anchor; the runtime runs each variant's
module, collects the outcomes, and returns them. The whole fan-out is one
recorded host call on the parent, so the parent's own [replay](./replay.md)
returns the outcomes from cache.

Signals compose with branching — a branch listening on a signal drains the
parent run's shared mailbox; see
[Signals § Composition with branching](./signals.md#composition-with-branching).

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
  branchId: string;              // maps 1:1 to the branch's store path
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
- A `paused` outcome carries a `branchId` you can resume out-of-band with
  `chidori branch-resume` (see below), keeping the JS surface a single awaited
  Promise.

## How it works

The `branch` call is a single recorded host call whose result is the outcomes
array — on parent replay it returns cached and the branches never re-run. For
each variant, the runtime:

1. Anchors the branch on the parent's state at the fork: the parent's captured
   workspace state plus the variant's explicit `input`.
2. Runs the branch's source module as its own durable sub-run — live, through
   the same recorded-effect path as any run, under the same approval policy as
   the parent, journaling into the branch's own store.
3. Settles the outcome — `completed` with the module's return value, `paused`
   with the pending prompt when the branch suspended on a host call, or
   `failed` with the error — and folds the branch's records into the parent's
   journal, so the full fan-out is one auditable history.

Variants run in **waves of `options.concurrency` workers** (default 1 —
sequential; clamped to the variant count). Each branch gets its own isolated
JS VM; outcomes are settled and persisted in variant order after each wave
finishes, so the journal and the outcomes array are deterministic regardless
of completion order.

**Nested `chidori.branch` inside a branch is rejected**; the rejection
surfaces as a `failed` outcome for that branch rather than failing the whole
call.

Tracing is free: branch records nest under the `branch` call, so with
`OTEL_EXPORTER_OTLP_ENDPOINT` set the operator sees the fork live as a
`branch` span with one child subtree per strategy, side by side.

## The branch store

When the parent run persists (`.chidori/runs/<run_id>/`), every branch sub-run
is persisted under it:

```text
<run dir>/branches/op-<fork point>/
  anchor.json              fork-time anchor: the parent's captured workspace state
  branch-<k>/
    source.ts              the branch's own EDITABLE source copy
    checkpoint.json        the branch's journal artifact (a compacted journal —
                           not the parent's records.jsonl format)
    branch.json            metadata: label, id, status, pending input, input,
                           output/error
    history/               the branch's git-like source history: a fork commit
                           of the variant's source, parented on the parent
                           run's head commit, plus one commit per accepted
                           edit ([Source History](./source-history.md))
```

The anchor and the per-branch source copies are written **before** the fan-out
runs, so even a crash mid-fan-out leaves re-runnable branch stores behind. The
`branchId` in each outcome maps 1:1 to the branch's store path.

## Resume and edit-and-rerun

The store makes a branch independently operable out-of-band, after the parent
has moved on:

```bash
# List a run's persisted branches and their states:
chidori branches <run_id>

# A branch paused on chidori.input()? Answer it:
chidori branch-resume <run_id> <branch_id> --value "blue"

# Edit a strategy and re-run ONLY that branch from the same anchored state:
$EDITOR .chidori/runs/<run_id>/branches/op-*/branch-001/source.ts
chidori branch-rerun <run_id> <branch_id>
```

Note that `branch-resume`'s short flag `-v` means `--value` (the response to
deliver), not verbose. Both commands default their model to the one recorded
in the parent run's manifest (override with `--model` or `CHIDORI_MODEL`), and
accept `--trusted`/`--untrusted` for the branch's live gated effects — the
same posture flags as `chidori run`.

- **Resume** replays the branch's journal with your `--value` answering the
  pending `input()` (the same mechanism the server's `/resume` uses), then
  runs the branch's `source.ts` live to its next outcome. Resume answers
  `input()` pauses; approval/signal pauses are reported but not resumable
  out-of-band.
- **Edit-and-rerun** discards the branch's previous journal and re-runs the
  branch **fresh from the parent anchor** with whatever `source.ts` now
  contains. The anchored state (fork-time workspace state + the variant's
  `input`) is identical to the original fork, so only the branch's code is the
  variable. Branch runs never go through the run manifest's source-identity
  gate — the anchor is the captured state, not a source check.

A resumed or re-run branch updates only its own store; the parent's recorded
`branch` outcome is immutable history (compare, don't merge). The branch's
**code** history is kept too: each edit that actually runs (`branch-rerun`, or
a resume whose `source.ts` changed) chains a commit onto the branch's
`history/` store, so every strategy version that ever ran from the anchor
stays recoverable and diffable — `chidori history <run_id>` shows the chains
and `--diff` compares any two versions (see
[Source History](./source-history.md)).

## Correctness and determinism

- **Parent determinism:** the `branch` call is recorded with the outcomes as
  its result; parent replay short-circuits the fan-out like any cached host
  call.
- **Branch determinism:** a persisted branch is replayable from its own stored
  journal; resume is that replay plus live continuation.
- **One coherent history:** each branch journals into its own store; settling
  folds its records into the parent's journal, so live runs and replays see
  the identical, collision-free record — a branch that violates this invariant
  fails the call rather than corrupting the journal.
- **State-handover fidelity:** branches inherit the captured workspace state
  plus the explicit `input`, not the parent's in-flight JS locals. The agent passes
  what a branch needs.

## Cost, safety, concurrency

N branches make N sets of **live** host calls past the fork — real LLM/tool
spend. The controls: `options.concurrency` caps simultaneous live branches
(default 1), the fan-out is hard-capped at 16 variants, and each branch runs
under the same approval policy as the parent. Branches use separate, isolated
VMs; no shared mutable state.

## Example

[`examples/branching/`](../examples/branching/) is a runnable end-to-end
example: shared research once, a two-strategy fork, compare-and-pick, replay
via `chidori resume`, and the resume/edit-and-rerun workflows against the
branch store.
