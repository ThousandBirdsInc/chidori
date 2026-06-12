# Value Checkpoints — `chidori.step(name, fn)`

> **Status:** Implemented. This is the production landing of the engine plan's
> deferred **P6** ("value checkpoints"): bound resume cost on long histories by
> memoizing expensive deterministic computation into the durable call log. The
> engine-level prototype (`durableStep` on `chidori_js::ReplayRuntime`,
> `crates/chidori-js/src/replay.rs`) shipped with the engine; this doc covers
> the `chidori.*` host API every agent gets.
> **Related:** [`docs/pure-rust-js-engine-plan.md`](./pure-rust-js-engine-plan.md)
> (historical; P6 row), [`docs/fable_review.md`](./fable_review.md),
> [`docs/signals.md`](./signals.md).

---

## 1. Summary (TL;DR)

Chidori's durability model is **deterministic replay**: resume re-executes the
agent's JavaScript from the top while every recorded host effect (`prompt`,
`tool`, `http`, …) is served from the journal instead of re-performed. Host
effects are therefore cheap on resume — but **pure JS compute between effects
is re-executed every time**. For agents that do heavy deterministic work
(parsing large documents, building plans, transforming corpora), a long-lived
run gets progressively more expensive to resume.

`chidori.step(name, fn)` fixes that:

```ts
const plan = await chidori.step("plan", () => buildPlan(input)); // expensive, pure
```

Live, `fn` runs once and its JSON-serializable result is recorded as a `step`
call-log record. On **every** subsequent replay — crash recovery, `input()` /
approval / signal resume, `chidori trace` re-derivation, `POST
/sessions/{id}/replay` — the recorded value is returned (or the recorded error
re-thrown) **without re-running `fn`**. Resume cost becomes proportional to the
un-wrapped code, not the total compute the run has ever done.

## 2. The contract: pure, synchronous compute

A skipped callback must be skippable: if `fn` had observable effects, replay
(which never runs it) would lose them — state would silently diverge, or the
journal's sequence numbers would desynchronize. So step callbacks must be
**pure, synchronous computation**, and the runtime enforces the classes of
violation it can see, loudly:

| inside a step callback | behavior |
|---|---|
| any `chidori.*` effect (`log`, `prompt`, `tool`, nested `step`, …) | throws `chidori.<effect> is not allowed inside chidori.step(...)` |
| captured randomness (`node:crypto` `randomBytes`, `crypto.getRandomValues`) | throws (it would write a `crypto.random` record) |
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

## 3. Semantics and mechanics

A step is **one** `CallRecord` — `function: "step"`, `args: {name}`, the
result (or `error`) holding the outcome — at one sequence number. The
implementation is a two-phase protocol because the callback is a JS value that
cannot cross the JSON host boundary:

1. **`step_begin {name}`** (`host_core::execute_step_begin`): allocates the
   seq and checks the replay log.
   - *Replay hit* (`try_replay_checked(seq, "step")`): returns the recorded
     value or error. The recorded `name` must match the call's name, else the
     code was edited before the resume frontier — fail-loud divergence, same
     contract as every other host effect.
   - *Miss*: marks the step live on the `RuntimeContext`
     (`begin_step(seq, name)`) and the engine binding runs the callback. While
     the step is live, the effect dispatchers (`HostBindingBackend::dispatch`,
     the `__chidori_*` sync natives) refuse the calls in the table above.
2. **`step_end {name, value | error}`** (`host_core::execute_step_end`): takes
   the live-step marker back and writes the `step` record at the reserved seq.

The engine binding lives in `chidori-js`'s `install_chidori_effects`
(`crates/chidori-js/src/lib.rs`): probe, `vm.call(fn)` only on a miss, report.
A thrown callback records the error message and replays as the same throw.

**No pending host operation, no pause.** A step cannot suspend (everything
suspendable is refused inside it), so it needs no `PendingHostOperation` /
host-promise entry. A crash between begin and end simply re-runs the
(deterministic) callback on resume — memoization is an optimization, never a
correctness dependency.

## 4. Determinism analysis

- **Match key** is `(seq, "step")` plus the `name` carried in args. `seq` comes
  from the deterministic `next_seq()` walk, so a replayed run reaches the same
  step at the same seq (inductively: the prefix is deterministic). A renamed
  or moved step fails loudly instead of silently mis-replaying.
- **The journal cannot gap.** Because every record-producing or
  state-mutating operation is refused while a step is live, the step's record
  at seq `N` is always immediately followed by the next effect at `N+1` — in
  the live log and in every replayed log. Skipping the callback can therefore
  never desynchronize sequence numbers (the failure mode that would otherwise
  make memoize-and-skip unsound).
- **Errors replay as errors.** A failed step records `error` and re-throws on
  replay, so a `try/catch` around a step takes the same branch every run.
- **Edit-and-resume composes.** Editing a step's body *after* the resume
  frontier takes effect on the next fresh run; editing it *before* the
  frontier is invisible (the recorded value wins) — which is exactly the
  modify-and-resume contract everywhere else in the journal. Renaming a
  pre-frontier step is detected as divergence.

## 5. Relation to neighbors

- **`chidori.checkpoint(label, data)`** records an explicit *marker* you
  compute yourself; it doesn't skip anything. `step` is the memoizing version:
  the runtime decides record-vs-replay and the callback body is the thing
  being saved.
- **`durableStep(fn)`** (`ReplayRuntime::install_memo`) is the engine-crate
  prototype with the same record/replay semantics, keyed by invocation index
  only. `chidori.step` is the production surface: named, divergence-checked,
  recorded in the real call log, enforced pure, visible in traces and
  `chidori trace` output as a `step` record.
- **Provider prompt caching** (`docs/context-management.md`) bounds *token*
  re-billing; `step` bounds *CPU* re-execution. Both are live-only
  optimizations layered under the same source of truth, the call log.

## 6. Future work

- **Periodic value snapshots of agent-declared state** ("restore from the last
  checkpoint, skip the prefix entirely") would bound even the un-wrapped
  replay cost, at the price of a checkpoint-aware programming model for loops.
  `step` is the composable primitive that doesn't change the model; revisit
  the bigger version if journals grow past what stepped replay handles.
- **Async step bodies** (awaiting only pure promises) could be supported by
  draining jobs inside the binding; deferred until a real agent needs it.
- A `chidori stats` / trace view that reports replay time saved per step.
