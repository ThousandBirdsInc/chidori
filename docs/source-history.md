---
title: "Source History"
description: "The git-like record of the agent's implementation kept alongside the execution journal: commits, fork points, chidori history."
---

# Source history: the code side of a run's history

A run's history has two halves. The effect journal
([`docs/replay.md`](./replay.md)) is a complete record of what the run
**did** — every host call, in order, with its result. Source history is the
matching record of what the run **was**: every version of the agent's
implementation (the entry file plus every imported module, full text) that
existed alongside the execution history.

Without it, the code half is lossy. The snapshot manifest keeps only the
*latest* source fingerprints; `--allow-source-change` resumes overwrite the
run's code identity with a warning; `chidori branch-rerun` edits a branch's
`source.ts` in place. In each case the version that actually produced the
journaled prefix is gone. With source history, every one of those transitions
is a **commit** in a git-like store under the run directory, and
`chidori history` reads the whole DAG back.

## The model

Each run (and each `chidori.branch` sub-run) carries its own store:

```text
<run dir>/history/
  objects/<sha256 hex>     content-addressed source blobs — the full text of
                           one file version, stored once per unique content
  commits.jsonl            append-only commit log, one JSON commit per line
<run dir>/branches/op-*/branch-*/history/    the same shape, per branch
```

A commit snapshots the module **tree** — `(path, sha256 of content)` for the
entry and every imported module — plus:

- **`parents`** — the previous commit id(s). A run's trunk is a linear chain;
  a branch store's first commit carries the **parent run's head commit** as
  its parent, so fork points are edges in the DAG exactly like branches in
  git.
- **`event`** — why the version was recorded: `run_start`,
  `resume_source_change`, `branch_fork`, `branch_resume`, `branch_rerun`.
- **`journal_frontier`** — the execution anchor: how many journaled call
  records already existed when this code took over. Between two consecutive
  commits, every record in `(frontier_a, frontier_b]` executed under commit
  `a`'s code. This is what makes the two histories one: any journaled call
  can be traced back to the exact implementation that produced it.

Commit ids are deterministic (`sha256` over parents + event + tree + anchor),
and every recording point **dedupes against the head commit**: recording an
identical tree is a no-op. Resume-without-edit, replay, and repeated
safepoints never grow the history; only real changes do.

## When commits are recorded

| Event | Recorded by | Anchor |
|---|---|---|
| `run_start` | the run's first persisted safepoint | current journal length (0 for a fresh run) |
| `resume_source_change` | every resume surface that accepts an edit via `--allow-source-change` / `"allow_source_change": true` — recorded **before** replay begins, so the accepted edit is history even if replay then diverges | the manifest's durable journal frontier |
| `branch_fork` | `chidori.branch`, when the fork anchor and per-branch `source.ts` copies are persisted (before any branch spends anything) | the `branch` call's seq in the parent journal |
| `branch_resume` | `chidori branch-resume` (recorded only if `source.ts` changed since the last branch commit) | the replayed checkpoint length |
| `branch_rerun` | `chidori branch-rerun`, with whatever `source.ts` now contains | 0 (reruns start fresh from the anchor) |

For runs persisted **before source history existed**, the first accepted
edit-and-resume synthesizes the `run_start` commit from the original entry
text already stored in the run's snapshot blob (`DurableBlob.bundle`) — when
its hash matches the manifest's recorded fingerprint — so even old runs get a
diffable before/after.

Recording is best-effort everywhere: a history write failure logs a warning
and never fails or blocks the run. `chidori verify` is unaffected — it never
opts into source changes, so it records nothing and stays write-free.

## Reading it back: `chidori history`

```bash
# The interleaved timeline: trunk commits with the journal ranges that
# executed under each version, then each branch's chain with its fork point.
chidori history <run-id>

Implementation history for run e00458f7-…
run:
  * 9a2c3ed35ad1 run_start             2026-08-13 15:36:05 UTC  1 file(s): agent.ts
  |     journal records 1..3 executed under this version
  * 91a14334200f resume_source_change  2026-08-13 15:36:30 UTC  ~ agent.ts
  |     active from journal frontier 3

branch e00458f7-…-op2-branch-0 [label "double"] (completed), forked at parent seq 2 from 9a2c3ed35ad1:
  * 0ee566e47e67 branch_fork           2026-08-13 15:36:05 UTC  1 file(s): ./double.ts
  * 4632b334800c branch_rerun          2026-08-13 15:36:24 UTC  ~ ./double.ts

# Print the stored source of any recorded version (unique id prefix ≥ 4 chars):
chidori history <run-id> --show 0ee5 [--path ./double.ts]

# Unified diff between two versions — or one version against its parent:
chidori history <run-id> --diff 4632b334800c
chidori history <run-id> --diff 9a2c..91a1 --path agent.ts

# Machine-readable:
chidori history <run-id> --json
```

`--show` and `--diff` resolve commits across the whole DAG (trunk and every
branch store), so diffing a branch's edit against the trunk version it forked
from works directly.

## Relationship to the rest of the system

- **The journal stays authoritative for execution.** Source history adds no
  replay semantics: divergence detection is still positional replay plus the
  manifest fingerprint gate ([`docs/replay.md`](./replay.md)). History is the
  audit trail that makes those gates' decisions inspectable after the fact.
- **Run-trunk history flows through the run's `RunStore` handle**, so a
  configured durable mirror ([`docs/durable-storage.md`](./durable-storage.md))
  receives the commits and objects written at safepoints. Commits recorded by
  out-of-band CLI surfaces (edit-and-resume validation, branch stores) are
  filesystem-local, like other out-of-band branch reads.
- **Branch outcomes stay immutable.** A rerun's commit chain records how a
  branch's code evolved, but the parent's recorded `branch` outcome never
  changes ([`docs/branching-execution.md`](./branching-execution.md)).
- Objects are content-addressed, so an unchanged module shared by many
  commits is stored once; the store grows with *change*, not with commits.
