---
title: "Value Checkpoints"
description: "chidori.step: journal expensive pure compute once so replay and resume never re-pay it."
---

# Value Checkpoints — `chidori.step(name, fn)`

> `chidori.step` bounds resume cost on long histories by memoizing expensive
> deterministic computation into the journal. A **value checkpoint** is one
> such memoized `chidori.step` result — not to be confused with
> `checkpoint.json` (the compacted whole-journal artifact —
> [Durable Storage](./durable-storage.md)) or with the run directory itself.
> **Related:** [Replay & Resume](./replay.md), [Memory](./memory.md),
> [Context Management](./context-management.md). API reference:
> [Host API](./host-api.md).

---

## Summary

On resume, journaled host calls are served from the journal, but the pure JS
compute between them is re-executed every time
([Replay & Resume](./replay.md)). For agents that do heavy deterministic work
(parsing large documents, building plans, transforming corpora), a long-lived
run therefore gets progressively more expensive to resume.

`chidori.step(name, fn)` fixes that:

```ts
const plan = await chidori.step("plan", () => buildPlan(input)); // expensive, pure
```

Live, `fn` runs once and its JSON-serializable result is journaled as a `step`
record. On **every** subsequent replay — crash recovery, `input()` /
approval / signal resume, `chidori resume`, `POST /sessions/{id}/replay` — the
journaled value is returned (or the journaled error re-thrown) **without
re-running `fn`**. Resume cost becomes proportional to the un-wrapped code,
not the total compute the run has ever done.

## The contract: pure, synchronous compute

A skipped callback must be skippable: if `fn` had observable effects, replay
(which never runs it) would lose them — state would silently diverge, or the
journal would fall out of step with the code. So step callbacks must be
**pure, synchronous computation**, and the runtime enforces the classes of
violation it can see, loudly:

| inside a step callback | behavior |
|---|---|
| any `chidori.*` effect (`log`, `prompt`, `tool`, nested `step`, …) | throws `chidori.<effect> is not allowed inside chidori.step(...)` |
| captured randomness (`node:crypto` `randomBytes`, `crypto.getRandomValues`) | throws (it would journal a `crypto.random` record) |
| VFS writes (`node:fs` write/append/mkdir/rm/rename) | throws (the mutation would be lost on replay) |
| timer / microtask scheduling (`setTimeout`, `setInterval`, `queueMicrotask`) | throws (the scheduled callback would never exist on replay) |
| an `async` callback / returned `Promise` | throws `chidori.step callback must return synchronously` |

Allowed: everything deterministic and recordless — plain compute, `JSON`,
`Math.random`/`Date` (deterministic by engine policy), crypto **hashing**, and
VFS **reads** (read-only, and the memoized result keeps replay exact
regardless). The result must be JSON-serializable; it is JSON round-tripped on
the live path too, so live and replayed runs observe byte-identical values.

What cannot be policed at reasonable cost: leaking work out of the callback by
closure mutation plus deferred promise reactions. Don't do that — the contract
is "compute a value from your inputs and return it".

## Semantics

A step is **one** journal record — function `step`, carrying the step's `name`
and its result (or error). Live, the callback runs once and the round-tripped
result is journaled at that point in the run. On replay, the journaled value
is returned (or the journaled error re-thrown) without running the callback —
and the journaled `name` must match the call's name, else the code was edited
before the resume frontier: a fail-loud divergence, the same contract as every
other host call. While a step's callback is running, the runtime refuses every
effect in the table above.

**A step never pauses.** Everything suspendable is refused inside it, so a
step can never be the host call a run parks on. A crash after the callback
starts but before its result is journaled simply re-runs the (deterministic)
callback on resume — memoization is an optimization, never a correctness
dependency.

## Determinism

- **A replayed run reaches the same step at the same point in the journal.**
  A renamed or moved step fails loudly as divergence instead of silently
  mis-replaying.
- **The journal cannot gap.** Because every record-producing or state-mutating
  operation is refused while a step is live, a step's record is always
  immediately followed by the run's next effect — in the live journal and in
  every replayed one. Skipping the callback can therefore never desynchronize
  the journal (the failure mode that would otherwise make memoize-and-skip
  unsound).
- **Errors replay as errors.** A failed step journals its error and re-throws
  on replay, so a `try/catch` around a step takes the same branch every run.
- **Edit-and-resume composes.** Editing a step's body *after* the resume
  frontier takes effect on the next fresh run; editing it *before* the
  frontier is invisible (the journaled value wins) — which is exactly the
  modify-and-resume contract everywhere else in the journal. Renaming a
  pre-frontier step is detected as divergence.

## Relation to neighbors

- **`chidori.mark(label, data)`** journals an explicit *marker* you compute
  yourself; it doesn't skip anything. `step` is the memoizing version: the
  runtime decides record-vs-replay and the callback body is the thing being
  saved.
- **Provider prompt caching** ([Context Management](./context-management.md))
  bounds *token* re-billing; `step` bounds *CPU* re-execution. Both are
  live-only optimizations layered under the same source of truth, the journal.
- For the full map of Chidori's state surfaces — memory vs. workspace vs.
  step vs. journal vs. run store — see the canonical boundary table in
  [Memory](./memory.md#memory-vs-its-neighbors).

Steps are visible in traces: each one appears as a `step` record in
`chidori trace` output.

## Limitations

- **Un-wrapped code still replays linearly.** Not supported: a periodic
  snapshot of agent-declared state that would let resume skip a prefix
  entirely. `chidori.step` is the composable primitive that bounds
  re-execution without changing the programming model.
- **Step bodies must be synchronous.** Async callbacks (even ones awaiting
  only pure promises) throw rather than being drained.
- Traces do not report replay time saved per step.
