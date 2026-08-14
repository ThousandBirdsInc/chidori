---
title: "Source History"
description: "The git-like record of the agent's implementation kept alongside the journal: commits, fork points, chidori history."
---

# Source history: the code side of a run's history

A run's history has two halves. The journal ([Replay & Resume](./replay.md))
is a complete record of what the run **did** — every host call, in order, with
its result. Source history is the matching record of what the run **was**:
every version of the agent's implementation (the entry file plus every
imported module, full text) that existed alongside the execution history.

Without it, the code half is lossy. The run's manifest keeps only the *latest*
source fingerprints; a resume with `--allow-source-change` overwrites the
run's code identity with a warning; `chidori branch-rerun` edits a branch's
`source.ts` in place. In each case the version that actually produced the
journaled prefix would be gone. With source history, every one of those
transitions is a **commit** in a git-like store under the run directory, and
`chidori history` reads the whole graph back.

## The model

Each run (and each `chidori.branch` sub-run) carries its own history store
under its run directory: content-addressed blobs holding the full text of each
file version (stored once per unique content), plus an append-only commit log.
Branch stores live alongside the branch, in the same shape.

A commit captures the module **tree** — the entry file and every imported
module, each identified by a content hash — plus:

- **Parents** — the previous commit id(s). A run's trunk is a linear chain; a
  branch store's first commit carries the **parent run's head commit** as its
  parent, so fork points are edges in the graph exactly like branches in git.
- **Event** — why the version was recorded: `run_start`,
  `resume_source_change`, `branch_fork`, `branch_resume`, `branch_rerun`.
- **Journal anchor** — how many journal records already existed when this code
  took over. Between two consecutive commits, every record in that span
  executed under the earlier commit's code. This is what makes the two
  histories one: any journaled call can be traced back to the exact
  implementation that produced it.

Commit ids are deterministic, and every recording point **dedupes against the
head commit**: recording an identical tree is a no-op. Resume-without-edit,
replay, and repeated safepoints never grow the history; only real changes do.

## When commits are recorded

| Event | Recorded when | Anchor |
|---|---|---|
| `run_start` | the run first persists | current journal length (0 for a fresh run) |
| `resume_source_change` | any resume that accepts an edit via `--allow-source-change` (or the HTTP equivalent) — recorded **before** replay begins, so the accepted edit is history even if replay then diverges ([Replay & Resume](./replay.md)) | the journal position at resume |
| `branch_fork` | `chidori.branch` persists the fork anchor and per-branch `source.ts` copies (before any branch spends anything) | the fork point in the parent journal |
| `branch_resume` | `chidori branch-resume`, only if `source.ts` changed since the last branch commit | the length of the replayed branch journal |
| `branch_rerun` | `chidori branch-rerun`, with whatever `source.ts` now contains | 0 (reruns start fresh from the anchor) |

For runs persisted **before source history existed**, the first accepted
edit-and-resume synthesizes the `run_start` commit from the original entry
text already stored with the run's snapshot — when it still matches the
manifest's recorded fingerprint — so even old runs get a diffable
before/after.

Recording is best-effort everywhere: a history write failure logs a warning
and never fails or blocks the run. `chidori verify` is unaffected — it never
opts into source changes, so it records nothing.

## Reading it back: `chidori history`

```bash
# The interleaved timeline: trunk commits with the journal ranges that
# executed under each version, then each branch's chain with its fork point.
chidori history <run_id>

Implementation history for run e00458f7-…
run:
  * 9a2c3ed35ad1 run_start             2026-08-13 15:36:05 UTC  1 file(s): agent.ts
  |     journal records 1..3 executed under this version
  * 91a14334200f resume_source_change  2026-08-13 15:36:30 UTC  ~ agent.ts
  |     active from journal record 3

branch e00458f7-…-op2-branch-0 [label "double"] (completed), forked at parent seq 2 from 9a2c3ed35ad1:
  * 0ee566e47e67 branch_fork           2026-08-13 15:36:05 UTC  1 file(s): ./double.ts
  * 4632b334800c branch_rerun          2026-08-13 15:36:24 UTC  ~ ./double.ts

# Print the stored source of any recorded version (unique id prefix ≥ 4 chars):
chidori history <run_id> --show 0ee5 [--path ./double.ts]

# Unified diff between two versions — or one version against its parent:
chidori history <run_id> --diff 4632b334800c
chidori history <run_id> --diff 9a2c..91a1 --path agent.ts

# Machine-readable:
chidori history <run_id> --json
```

`--show` and `--diff` resolve commits across the whole graph (trunk and every
branch store), so diffing a branch's edit against the trunk version it forked
from works directly.

## What it costs

History is designed to never become a per-run tax:

- **Recording happens per event, not per host call.** A run records once when
  it first persists, and thereafter only when code actually changes — every
  other occasion is a dedupe no-op.
- **Content is stored once.** Blobs are immutable and content-addressed, so
  identical file versions are shared rather than rewritten — across sibling
  branches, between a branch and its parent run, and across runs of the same
  agent (a shared per-project cache backs this). Each run directory still
  contains its full history, so runs stay self-contained for copying and
  archiving while the disk stores one copy. The store grows with *change*, not
  with commits.
- **The editable copy is really a copy.** A branch's editable `source.ts` is
  materialized as an independent copy of the version it forked from, so
  editing it can never write through into the shared stored history.
- **Durable mirrors get the bytes.** With a durable run-store backend
  configured ([Durable Storage](./durable-storage.md)), a run's trunk history
  is mirrored along with the rest of the run; content sharing never trades
  away durability. Commits recorded by out-of-band CLI surfaces
  (edit-and-resume, branch stores) are local to the filesystem, like other
  out-of-band branch operations.

## Relationship to the rest of the system

- **The journal stays authoritative for execution.** Source history adds no
  replay semantics: divergence detection and the `--allow-source-change` gate
  work exactly as [Replay & Resume](./replay.md) describes. History is the
  audit trail that makes those gates' decisions inspectable after the fact.
- **Branch outcomes stay immutable.** A rerun's commit chain records how a
  branch's code evolved, but the parent's recorded `branch` outcome never
  changes ([Branching Execution](./branching-execution.md)).
