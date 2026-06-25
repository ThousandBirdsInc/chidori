# Runtime Signals — Multiplayer Agents — Design Doc & Implementation Plan

> **Status:** Implemented — Phases 1–3 have all shipped. Phase 1: the named
> **blocking** `chidori.signal(name)`, the durable **per-run mailbox**, the
> `POST /sessions/{id}/signal` **delivery endpoint** (resolve+resume, enqueue, or
> 409), and **deterministic replay** of the whole multiplayer session.
> Phase 2: **`pollSignal`**, the fan-in **`chidori.signalAny([names])`**,
> **`timeoutMs`** (pinned: resolve to a `{timedOut: true}` **sentinel**, enforced
> by a server timer against a persisted deadline, re-armed on restart), and
> signal `name`/`from` stamped as **OTEL span attributes**. Phase 3: **live
> in-memory delivery** — the delivery endpoint enqueues straight into a
> streaming run's in-memory mailbox (write-through to `signals/inbox.json`) and
> wakes the supervising worker, which resolves a matching pause **in-process**
> (no HTTP `/resume` round-trip) and keeps the SSE stream open across the
> resume. Pinned decisions: the same-name tie-break is
> **pending-pause-wins-with-newest** (also applied by the live worker, which
> takes the just-delivered entry back out of the mailbox); the `signalAny`
> result shape is the **bare `Signal`** whose `name` says which fired.
> **Engine:** chidori-js (the pure-Rust JS engine) is now the only runtime — there
> is no `CHIDORI_JS_ENGINE=rust` selector, no `rust-engine` cargo feature gate to
> opt in, and no separate QuickJS path to keep untouched.
> **Related:** [`docs/branching-execution.md`](./branching-execution.md),
> [`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md),
> [`docs/architecture.md`](./architecture.md).

---

## 1. Summary (TL;DR)

Add a runtime primitive, **`chidori.signal(name)`**, that lets a running agent reach an
explicit point where it **pauses and listens for additional information delivered
mid-run** — where the senders can be **multiple humans *and* other agents**. A signal is
a named message `{ name, payload, from }` addressed to a specific run. The agent awaits
it (`const review = await chidori.signal("review")`); an outside party delivers it
(`POST /sessions/{id}/signal`). A durable **per-run mailbox** absorbs signals that arrive
*before* the agent is listening, so there is no race. Every signal is recorded in the
call_log, so the entire multiplayer session **replays deterministically**.

This is the inverse of `chidori.input`. `input` is *agent-initiated request/response*
("I need an answer, I'll block until I get one"). A **signal** is *externally-initiated
push at an agent-declared listen point* ("I'm now receptive — anyone may send me a
`review`"). Mechanically a signal is `input()` with three additions: **(1) a name**, so
many distinct listen points coexist; **(2) a durable pre-arrival mailbox**, so delivery
and listening can race safely; **(3) a `from` provenance field**, so the trace records
*who* steered the run. ~85% of the machinery already exists (the
`PendingHostOperation`/`HostPromiseTable` suspension model, the `pending.json` +
`host_promises.json` persistence, the `/sessions/{id}/resume` delivery path, and the
streaming OTEL→tael tracing); the new work is the named primitive, the mailbox, the
delivery endpoint, and one prerequisite bug fix.

---

## 2. Motivation (the "why") — *multiplayer agents*

Today a chidori run is a **closed loop**: it takes an input, runs to completion or to an
agent-initiated pause (`chidori.input`), and the only way the outside world influences a
run mid-flight is to answer a question the agent *chose* to ask. That is single-player.
The interesting frontier is **multiplayer**: a run that stays open to information pushed
in by *other participants* — humans **and** agents — while it executes. Signals are the
substrate for that. Concretely, they unlock:

- **Steering a long-running agent without restarting it.** A human (or supervisor agent)
  notices the run is heading the wrong way and pushes a correction, a new constraint, or
  a priority change at the next listen point — instead of killing the run and re-paying
  for the whole expensive prefix. The agent decides *where* it is safe to be steered.
- **Human-in-the-loop beyond approvals.** `chidori.input` and policy-approval are
  *agent-asks-human*. Signals invert it: the human can **volunteer** information the agent
  didn't explicitly request ("here's the doc you'll need in step 4"), delivered when the
  agent reaches a receptive checkpoint.
- **Agent-to-agent coordination (the core multiplayer case).** A supervisor agent hands a
  sub-goal to a worker mid-run; a peer agent delivers a result, a revised spec, or a
  "stop pursuing X" message. This is the building block for **long-lived collaborating
  agents** — participants in a shared session — rather than one-shot `callAgent`
  request/response. `from` carries the sender's agent identity, so coordination is
  attributable and traceable.
- **Late-arriving external events / data.** A webhook fires, a user finishes an upload, an
  out-of-band tool completes — the event is delivered into the run as a signal when it is
  ready, rather than the agent busy-polling.

**Why chidori specifically should have this, and why it is more than a message queue:**
every signal is recorded as a host effect in the durable `call_log`. So a collaborative,
externally-driven, multi-participant session is *still fully reproducible* — you can
replay exactly what every human and agent sent, in the order it was consumed, and get the
identical run. That combination — **open to many participants live, yet deterministically
replayable** — is the differentiator. A raw mpsc channel gives you the first half and
throws away the second.

**Relationship to branching ([`docs/branching-execution.md`](./branching-execution.md)).**
Branching forks *one* agent to explore strategies and pick one (one agent, many futures).
Signals open *one* agent to *many participants* (one run, many senders). They are
complementary and compose: a branch can listen on signals; a participant can steer which
branch to keep. §12 covers the composition.

---

## 3. Goals / Non-Goals

**Goals**
- An agent can `await chidori.signal(name)` to pause at a named point and receive an
  externally-delivered `{ name, payload, from }`.
- A durable **per-run mailbox** absorbs signals that arrive before the agent listens, and
  is drained deterministically when the agent reaches the matching `signal(name)`.
- An external party (human UI or another agent) can deliver a signal to a run via
  `POST /sessions/{id}/signal`, whether the run is paused-waiting, paused-on-something-
  else, or still running.
- The whole session **replays deterministically**: a signal recorded in the call_log
  returns the identical `{payload, from}` on every replay, with consumption order frozen.
- Sender provenance (`from`) is captured in the durable log and surfaced in the trace.
- Reuse the existing `input` suspension + `HostPromiseTable` + resume machinery; do not
  build a parallel suspension subsystem.

**Non-Goals (initially)**
- True zero-latency in-memory delivery to a running task (Phase 3; MVP delivers via the
  durable pause→persist→resume loop, exactly like `/resume` today).
- Preemptive interrupts that can fire at *any* instruction (breaks checkpointability).
  Signals are consumed only at agent-declared listen points — that is the determinism
  contract and matches the framing ("points where they pause and listen").
- The QuickJS engine path (rust engine first; QuickJS port is later if wanted).
- Per-branch signal addressing (single shared per-run mailbox in v1; §12).
- Broadcast/pub-sub fan-out to many runs from one publish; signals are addressed to one
  run id.

---

## 4. Background (verified, with file references)

### 4.1 The suspension model `input` already uses
`crates/chidori/src/runtime/host_core.rs::execute_input(ctx, args)` is the existing pause primitive:
`ctx.next_seq()`, `try_replay_checked(seq, "input")` (replay short-circuit), then
`begin_host_operation_with_function(seq, PendingHostOperationKind::Input, Some("input"),
args)`, a safepoint to persist the pending op, then either `InputMode::Stdin` (read
stdin, resolve, record) or `InputMode::Pause` (`set_pending_input(PendingInput{seq,
prompt})` + throw the `PAUSE_MARKER` sentinel `"__CHIDORI_PAUSED_FOR_INPUT__"`). The
durable boundary is `execute_durable_json_call_at_seq(ctx, seq, fn, args, live)`, which
tries `try_replay_checked`, then `replay_completed_host_operation` (scans the
`HostPromiseTable` for a completed op by `(seq, kind, args)`), else begins a host op +
runs `live()` + records a `CallRecord`.

### 4.2 Pending-operation + host-promise substrate
`crates/chidori/src/runtime/snapshot.rs`: `HostOperationId(u64)`; `PendingHostOperation { id, seq, kind,
function, args, created_at }`; `PendingHostOperationKind` enum (Prompt/Input/Tool/
CallAgent/Http/Timer/...); `HostPromiseState { Pending, Resolved{value}, Rejected{error}
}`; `HostPromiseRecord { operation, state }`; `HostPromiseTable` (BTreeMap keyed by id:
`create_with_function`, `pending_operation`, `resolve`, `reject`,
`active_pending_operation`, **`completed_operation(seq, kind, args)`** — the match used on
resume). `SnapshotManifest` carries `pending`, `host_promises`, `vfs`, `call_log_len`.
`SnapshotCapableJsEngine`: `resolve_host_promise(id, value)`, `reject_host_promise`,
`run_jobs_until_blocked() -> JsRunState{Completed | BlockedOnHostOperation(id)}`.

### 4.3 Persistence + resume delivery (the path a signal reuses)
A paused run persists `.chidori/runs/{id}/pending.json` (the `PendingHostOperation`) and
`host_promises.json` (the table). `crates/chidori/src/server.rs::resume_session` (the
`/sessions/{id}/resume` handler) → `complete_persisted_pending_host_operation(run_base,
run_id, expected=(seq,kind), HostPromiseCompletion::Resolved(value))` resolves the pending
op, injects a synthetic `input` `CallRecord` at the pending seq, then
`run_replay_pausable_with_host_promises_and_vfs(...)` re-runs to the next pause/
completion. `load_persisted_host_promises` / `load_persisted_vfs` rehydrate state. This is
**exactly** the shape signal delivery needs, parameterized by `Signal` instead of `Input`.

### 4.4 Replay vs live decision
`crates/chidori/src/runtime/context.rs::try_replay_checked(seq, fn)` → cache hit from `replay_log`
(records into the new log), `None` → run live, `Err` on function-name mismatch
(divergence). For a value that arrives *after* the original journal was written (a fresh
signal), `replay_completed_host_operation` finds it in the `HostPromiseTable` by `(seq,
kind, args)` and records a new `CallRecord` with the resolved value — i.e. a newly
delivered signal becomes a first-class recorded call, replayable thereafter.

### 4.5 Server run-lifecycle + live channels
`crates/chidori/src/server.rs`: router with `POST /sessions`, `/sessions/{id}/resume`, `/approve`,
`/cancel`, `/sessions/stream`. `create_session` runs `spawn_blocking` to first pause/
completion, persists to disk. Paused runs live **on disk**, not in memory. Streaming runs
have an in-memory `ActiveSession { cancelled: AtomicBool, cancel_tx: mpsc }` in
`AppState.active_sessions` — the precedent for a live `signal_tx` (Phase 3).
`run_semaphore` caps concurrency.

### 4.6 Tracing
`record_call` → `RunSpan::stream_record` streams each call's span by `parent_seq`. A
`signal` host call therefore appears as a span automatically; `from`/`name` ride in the
record and can be stamped as span attributes (Phase 2) — no new tracing pipeline.

### 4.7 ⚠️ Load-bearing prerequisite (a latent bug)
`crates/chidori/src/runtime/engine.rs` `run_with_context` (~lines 425–451): the **rust-engine arm**
returns `Ok(output)` on success and `Err(e)` on *any* error, and **never calls
`ctx.take_pending_input()`**. The TypeScript/QuickJS arm (~516–584) does the pause-
surfacing dance; the rust arm does not. So today, on the rust engine, an `input()` in
Pause mode throws `PAUSE_MARKER`, `run_agent` returns `Err`, and the run is reported as
**failed, not paused**. Signals cannot pause-and-resume on the rust engine until this is
fixed. **Phase 1 task 1** fixes the rust arm to call `take_pending_input()` /
`take_pending_signal()` and return `RunResult{paused}` — which also fixes rust-engine
`input()` pausing as a side benefit.

---

## 5. Design overview

A signal is **a named, externally-deliverable flavor of an `input` pause, fronted by a
durable mailbox.** When the agent calls `chidori.signal(name)` the runtime, in order:
**(1)** replays a recorded result if this seq is already in the journal; **(2)** returns a
completed host-op if one matches `(seq, Signal, {name})`; **(3)** drains the mailbox —
if a queued signal of that `name` exists, consumes it *without pausing* and records a
completed call; **(4)** otherwise **pauses** (sets a `PendingSignal`, throws
`PAUSE_MARKER`). Delivery from outside is an HTTP `POST .../signal {name,payload,from}`
that either **resolves a matching pending pause and resumes** (reusing the `/resume`
machinery) or **enqueues into the mailbox** for a future listen point. The recorded
`CallRecord` freezes `{payload, from}` and the consumption choice, so replay is identical
regardless of arrival timing.

The one non-obvious correctness pillar is **the mailbox-vs-pause determinism argument**
(§10): the inbox is a *live-only convenience* whose every effect is frozen into the
call_log; a replay run never re-reads the inbox.

---

## 6. API surface

### 6.1 Agent-facing (`chidori.signal`)
```ts
import { chidori } from "chidori:agent";

type Signal<T = Json> = { name: string; payload: T; from: SignalSender };
type SignalSender = { kind: "human" | "agent"; id: string; runId?: string };

type SignalTimeout = { name: string | null; payload: null; from: null; timedOut: true };

// Blocking: pause at a named listen point until a matching signal is delivered
// (or already queued in the mailbox). With timeoutMs, resolves to the
// SignalTimeout sentinel after the deadline (discriminate with "timedOut" in r).
chidori.signal<T = Json>(name: string, opts?: {
  timeoutMs?: number;
}): Promise<Signal<T> | SignalTimeout>;

// Non-blocking: consume a queued signal if present, else null. Records
// the result (value OR null) so replay is deterministic at this seq.
chidori.pollSignal<T = Json>(name: string): Promise<Signal<T> | null>;

// Fan-in: pause until ANY of the named signals is delivered; the result is the
// bare consumed signal — its `name` says which fired. Pre-arrived candidates
// are consumed in arrival order (lowest delivery_seq across the name set).
chidori.signalAny<T = Json>(names: string[], opts?: {
  timeoutMs?: number;          // sentinel `name` is null (no name fired)
}): Promise<Signal<T> | SignalTimeout>;
```

### 6.2 SDK types
Add `Signal`, `SignalSender`, and `signal`/`pollSignal`/`signalAny` to the `Chidori`
interface in `sdk/typescript/src/agent.ts` (types only; the runtime supplies the methods,
like the other host methods).

---

## 7. Worked example — *what this enables in a real agent*

### 7.1 The scenario: a multiplayer policy-doc drafting agent

A team uses an agent to turn a short brief into a publishable policy document. The
expensive part is the drafting (several LLM calls + retrieval); the *judgment* part needs
people and a compliance checker. Three participants collaborate on one live run:

- a **human editor** (Mara) who reviews drafts and asks for changes,
- a **compliance-checker agent** that scans each draft for policy violations and pushes a
  verdict, and
- a **human lead** (Sam) who can change the document's priority/scope mid-run.

None of these are agent-initiated `input()` questions. The editor and the compliance agent
**push** information *when they have it*; the lead **steers** *whenever he wants*. The
agent only *consumes* those pushes at the points it declares safe.

### 7.2 The agent (`examples/multiplayer-review/policy_doc.ts`)

```ts
import { chidori, run } from "chidori:agent";

type Brief = { topic: string; audience: string };
type Review = { decision: "approve" | "changes"; notes: string };

run(async (brief: Brief) => {
  let draft = await writeDraft(brief);                 // expensive: LLM + retrieval
  let round = 0;

  while (true) {
    round++;
    await chidori.log(`draft round ${round} ready`, { words: draft.length });

    // Open this run to reviewers. The compliance agent AND the human editor both
    // send a "review" signal; whichever lands first (or is already queued in the
    // mailbox) is consumed here. `from` tells us who reviewed.
    const review = await chidori.signal<Review>("review");
    await chidori.log("review received", {
      from: review.from,                                // { kind:"agent", id:"compliance-bot" } or { kind:"human", id:"mara" }
      decision: review.payload.decision,
    });

    if (review.payload.decision === "approve") {
      return { status: "published", rounds: round, approvedBy: review.from, draft };
    }

    // A reviewer asked for changes — revise and loop. Before revising, opportunistically
    // pick up any steering the lead pushed (non-blocking; null if none waiting).
    const steer = await chidori.pollSignal<{ priority: string; scope?: string }>("steer");
    if (steer) {
      await chidori.log("scope changed mid-run", { from: steer.from, ...steer.payload });
      brief = { ...brief, ...steer.payload };           // re-scope without restarting
    }
    draft = await revise(draft, review.payload.notes, brief);
  }
});
```

Two listen points, two very different ergonomics:
- `chidori.signal("review")` **blocks** — the agent has nothing to do until a review
  arrives; it pauses, persists, and the run idles cheaply on disk.
- `chidori.pollSignal("steer")` is **non-blocking** — the lead's steering is optional; the
  agent checks the mailbox and moves on if it's empty.

### 7.3 The senders (multiplayer delivery)

A **human** (Mara) delivers a review from a UI or curl:

```bash
curl -XPOST localhost:8080/sessions/$RUN/signal -d '{
  "name": "review",
  "payload": { "decision": "changes", "notes": "Tighten the data-retention section." },
  "from": { "kind": "human", "id": "mara" }
}'
```

The **compliance-checker agent** delivers its verdict by calling the same endpoint —
it is just another participant, identified as an agent:

```ts
// inside the compliance agent, after scanning the draft it fetched
await fetch(`${chidoriUrl}/sessions/${targetRun}/signal`, {
  method: "POST",
  body: JSON.stringify({
    name: "review",
    payload: { decision: violations.length ? "changes" : "approve", notes: summarize(violations) },
    from: { kind: "agent", id: "compliance-bot", runId: chidori.runId },
  }),
});
```

The **lead** (Sam) steers at any time — even while the agent is mid-revision and not yet
listening. The signal lands in the **mailbox** and is consumed at the next `pollSignal`:

```bash
curl -XPOST localhost:8080/sessions/$RUN/signal -d '{
  "name": "steer",
  "payload": { "priority": "high", "scope": "EU + UK only" },
  "from": { "kind": "human", "id": "sam" }
}'
```

### 7.4 What you see in tael (live, nested, attributed)

Because each signal is a recorded host call, the multiplayer session streams as one trace
with every participant attributed by `from`:

```
agent.run policy_doc
├─ tool.call   writeDraft
├─ host.log    draft round 1 ready
├─ host.signal review            ← idles here; resolves when a review lands
│              from=agent:compliance-bot  decision=changes
├─ host.log    review received (compliance-bot)
├─ host.signal steer (poll)      ← from=human:sam (was queued before the agent looked)
├─ host.log    scope changed mid-run (sam)
├─ tool.call   revise
├─ host.log    draft round 2 ready
├─ host.signal review            ← from=human:mara  decision=approve
└─ … published, approvedBy=human:mara
```

### 7.5 Why durability matters here (the payoff)

Run `chidori trace $RUN` and you get a complete, ordered audit: *who* reviewed each draft,
*what* they said, *when* the lead re-scoped, and which reviewer's "approve" published the
doc — all reconstructable because the signals are in the call_log, not lost in a chat
channel. And `chidori resume $RUN` (or any replay) reproduces the **identical** run:
the editor's notes, the compliance verdict, and the steering are replayed from their
recorded `CallRecord`s, so a later "why did it publish?" investigation re-derives the exact
decision path without re-contacting any human or re-running the compliance agent. That is
the thing a bare message bus cannot give you: **a live, multi-participant collaboration
that is also a deterministic, auditable artifact.**

---

## 8. Host binding & orchestration (rust engine)

### 8.1 chidori-js host binding
In `crates/chidori-js/src/lib.rs::install_chidori_effects`, add methods mirroring `input`
(args: name in slot 0, opts JSON in slot 1), forwarding via the existing `forward_effect`
JSON bridge:
```rust
self.vm.define_method(&chidori, "signal", 2, move |vm, _t, args| {
    let name = args.first().map(|v| vm.value_to_json(v)).unwrap_or(Null);
    let opts = args.get(1).map(|v| vm.value_to_json(v)).unwrap_or(Null);
    forward_effect(vm, &d, "signal", json!({ "name": name, "opts": opts }))
});
// Phase 2: "pollSignal" -> "poll_signal", "signalAny" -> "signal_any".
```
**Reused:** `forward_effect`, `define_method`, `Vm::settle`/`run_jobs_until_blocked`.

### 8.2 New host-op kind & dispatch
- `crates/chidori/src/runtime/snapshot.rs`: add `PendingHostOperationKind::Signal` (one additive enum
  variant; `snake_case` serde → `"signal"`; old manifests still deserialize).
- `crates/chidori/src/runtime/typescript/bindings.rs::dispatch`: add arm `"signal" =>
  host_core::execute_signal(ctx, args)` (and Phase 2 `"poll_signal"`, `"signal_any"`).

### 8.3 `execute_signal` (the core; `crates/chidori/src/runtime/host_core.rs`)
Modeled on `execute_input`. The **match key is `args = { "name": name }`** — *not* the
payload (which is unknown at pause time). The payload/from live in the **result**
`{ name, payload, from }`, mirroring how `input` records `{prompt}` in args and the
answer in result. Steps: replay-check → completed-host-op check → **mailbox drain**
(`ctx.take_queued_signal(name)`: if present, `begin_host_operation` + safepoint +
`resolve_host_operation` + `record_call` with the consumed value, **no pause**) →
otherwise `begin_host_operation` + safepoint + `set_pending_signal(PendingSignal{seq,
name, id})` + throw `PAUSE_MARKER`. All sub-primitives are reused (§4.1).

### 8.4 The mailbox (durable, per-run)
- **Storage:** `.chidori/runs/{id}/signals/inbox.json` — an **ordered** `Vec<QueuedSignal
  { name, payload, from, delivery_seq, enqueued_at }>`. `delivery_seq` is a monotonic
  counter assigned by the delivery endpoint, freezing global arrival order across all
  senders. It is a standalone file (like `pending.json`), *not* a manifest field, because
  it is written by the HTTP endpoint while the run is not live.
- **In-memory:** `RuntimeContext` gains `signal_inbox: Vec<QueuedSignal>`, loaded at
  run/resume start (threaded the same way `vfs` is, via a new
  `with_replay_..._and_signals` variant). `take_queued_signal(name)` removes and returns
  the lowest-`delivery_seq` entry whose name matches, then **persists the shrunken inbox
  immediately** inside the same critical section that records the completed call (so a
  crash can't double-deliver; on restart the recorded result wins and the inbox is never
  re-drained for that seq).

### 8.5 Pause / resume state
- `PendingSignal { seq, name, id }` on the context (sibling of `PendingInput`);
  `set_pending_signal` / `take_pending_signal`.
- `engine.rs::run_with_context` (rust arm, §4.7 fix) checks `take_pending_signal()` (and
  `take_pending_input()`) and returns `RunResult{ paused: Some(PendingPause::Signal{seq,
  name}) }`. The existing `RunResult.paused` shape is extended to carry the name (or a
  small enum) so the server knows *which* named op is waiting.

### 8.6 Determinism, nesting, tracing — free from the substrate
A `signal` call goes through `execute_durable_json_call`-style recording, so it carries
`parent_seq` and streams a span under its parent. `from`/`name` ride in the record;
Phase 2 stamps them as span attributes in `RunSpan::stream_record`. On parent replay the
recorded result is returned from cache — no re-delivery, no inbox read.

---

## 9. Delivery endpoint & routing (server)

New route in `crates/chidori/src/server.rs` (beside `/resume`):
```
POST /sessions/{id}/signal     body: { name, payload, from }
```
Routing — the run is in exactly one state:

| Run state | Detection | Action |
|---|---|---|
| **Paused, waiting on THIS name** | `status == Paused` AND `pending.json` is a `Signal` op whose args `{name}` == body.name | **Resolve + resume** (reuse `complete_persisted_pending_host_operation` with `Signal`; inject synthetic `signal` CallRecord; `run_replay_pausable_with_host_promises_and_vfs`). |
| **Paused on a DIFFERENT name / on input / approval** | `pending.json` kind/name ≠ body | **Enqueue** into `inbox.json`. Run stays paused; drained when it later reaches `signal(name)`. |
| **Running, not yet listening** | `id` in `active_sessions`, or `status == Running` | Phase 1/2: **enqueue** into `inbox.json` (drained at next matching listen point). Phase 3: also push to live `signal_tx`. |
| **Completed / Failed / Cancelled** | terminal status | `409 Conflict` — no inbox write (an orphan inbox would mislead a later replay). |

The "resolve + resume" branch is structurally identical to `resume_session`; **factor its
shared tail into `complete_pending_and_resume(...)`** and call it from both, passing
`Signal` + the `{name,payload,from}` value. The resume worker also loads the inbox
(`load_persisted_signal_inbox`, sibling of `load_persisted_vfs`) so a resume that reaches
a *second* queued signal drains it.

**Reused:** `complete_persisted_pending_host_operation`, `complete_persisted_host_promise_record`,
`load_persisted_host_promises`, `load_persisted_vfs`,
`run_replay_pausable_with_host_promises_and_vfs`, `SessionStatus`, `session_view`,
`store_or_500`. **New:** `signal_session` handler, `inbox.json` read/write +
`enqueue_signal_to_inbox` + `load_persisted_signal_inbox`, the `complete_pending_and_resume`
extraction.

---

## 10. Determinism analysis (the crux)

**Claim:** a signal recorded in the call_log replays identically, regardless of whether it
was delivered by pause-resume or consumed from the mailbox.

- **Match key** `(seq, PendingHostOperationKind::Signal, args={name})`. `seq` comes from
  the deterministic `next_seq()` walk → identical on every replay (inductive: the
  pre-signal prefix is deterministic ⇒ same seq for the signal). `kind` + `name` are
  structural. So `completed_operation(seq, Signal, {name})` and `try_replay_checked(seq,
  "signal")` find the same record every time.
- **Result** (`{name, payload, from}`) is read verbatim from the `CallRecord` —
  **neither the inbox nor the endpoint is consulted for an already-recorded seq.** Steps
  (1)/(2) of `execute_signal` short-circuit before the mailbox drain (step 3) is ever
  reached on replay. So a replay run can have an *empty* inbox and still reproduce a
  consumed signal: **the log is the source of truth; the inbox is a live-only
  convenience.**
- **Ordering** is captured two agreeing ways: across *different* listen points by `seq`
  (the agent's own control flow totally-orders them, independent of arrival); for *same-
  name* signals competing for one listener, the lowest-`delivery_seq` queued entry is
  consumed and that choice is frozen into the result. Two same-name signals arriving
  before two `signal(name)` calls: first call consumes `delivery_seq` N, second consumes
  N+1, both recorded; replay reproduces both from their records.
- **Enqueued-then-consumed** is therefore safe: consumption removes from `inbox.json`
  *and* writes a `CallRecord` in one critical section; replay never re-enters step 3 for
  that seq.

---

## 11. Edge cases & risks

- **Signal to a completed/failed run:** `409`, no inbox write.
- **Two same-name signals + run paused-waiting-on-that-name:** the pending pause resolves
  with the *newly arrived* signal, leaving any older queued same-name entry for the next
  `signal(name)`. (Alternative: drain the oldest queued, enqueue the new — also valid;
  pick one and pin it with a test. Default: pending-pause-wins-with-newest, documented.)
- **Endpoint enqueues while resume worker loads the inbox:** Phase 1 is safe because a
  paused run is not a live task; still, guard `inbox.json` read-modify-write with a
  per-run advisory file lock (mandatory once Phase 3 makes runs live).
- **`from` provenance / multiplayer trace:** `from = {kind, id, runId?}` rides in the
  `CallRecord.result`, so it is in the durable log and streams via `RuntimeEvent::Call`
  → tael with no new code; Phase 2 also stamps it as an OTEL span attribute.

---

## 12. Composition with branching

The mailbox is **per-run, not per-branch** in v1 — a branch (see
[`docs/branching-execution.md`](./branching-execution.md)) listening on `signal(name)`
drains the parent inbox (single shared mailbox). The reserved per-branch
`CallLogSequenceRange` means a branch's signal `CallRecord`s stay in-range automatically,
so determinism composes. Per-branch addressing (delivery endpoint takes a `branch_index`,
inbox partitions by branch) is deferred to a later phase. Together the two primitives give
the full picture: **branch** to explore N futures, **signal** to let participants steer or
pick among them.

---

## 13. Alternatives considered
- **Reuse `chidori.input` with a `name` arg, no mailbox.** Rejected: without a mailbox a
  signal arriving before the listener has nowhere to go (lost or a hard race), and there
  is no clean multi-sender/`from` story. The mailbox is the load-bearing addition.
- **Preemptive interrupts (signal fires at any instruction, handler-style).** Rejected:
  un-checkpointable and non-deterministic; breaks the replay contract. Listen points keep
  delivery at deterministic seqs — and match the "points where they pause and listen"
  framing.
- **A raw external message queue (Redis/NATS) beside the run.** Rejected: gives live
  multiplayer but discards deterministic replay — the whole chidori differentiator. The
  call_log *is* the durable queue-of-record.
- **Live in-memory delivery as the MVP.** Deferred to Phase 3: the durable pause→resume
  path already delivers correctly and reuses `/resume`; the in-memory channel is a latency
  optimization, not a correctness requirement.

---

## 14. Implementation plan (phased)

**Phase 1 — Named blocking signal + durable mailbox + paused-run delivery (deterministic)** *(shipped)*
1. `crates/chidori/src/runtime/engine.rs` (~425–451): **fix the rust-engine arm of `run_with_context`**
   to surface pauses via `take_pending_input()` / `take_pending_signal()` →
   `RunResult{paused}` (prerequisite; also fixes rust-engine `input()` pausing).
2. `crates/chidori/src/runtime/snapshot.rs`: add `PendingHostOperationKind::Signal`; `QueuedSignal`
   struct + `SIGNAL_INBOX_FILE` const.
3. `crates/chidori/src/runtime/context.rs`: `signal_inbox: Vec<QueuedSignal>` + `pending_signal:
   Option<PendingSignal>`; `set/take_pending_signal`, `take_queued_signal(name)`,
   `load_signal_inbox`/`persist_signal_inbox`; a `with_replay_..._and_signals` variant.
4. `crates/chidori/src/runtime/host_core.rs`: `execute_signal` (§8.3).
5. `crates/chidori/src/runtime/typescript/bindings.rs`: `dispatch` `"signal"` arm.
6. `crates/chidori-js/src/lib.rs`: `chidori.signal` method (§8.1).
7. `crates/chidori/src/server.rs`: `POST /sessions/{id}/signal` route + `signal_session` handler;
   `load_persisted_signal_inbox` / `enqueue_signal_to_inbox`; extract
   `complete_pending_and_resume`; thread the inbox into the resume path.
8. `sdk/typescript/src/agent.ts`: `Signal`/`SignalSender` + `signal` on `Chidori`.

Tests (`--features rust-engine`):
- `host_core`: `execute_signal` pauses when inbox empty; consumes a queued signal without
  pausing and records a completed op; replay of a recorded signal never touches the inbox.
- `rust_engine`: agent calls `chidori.signal("review")` → pauses; resume with
  `{payload,from}` → completes; full re-run from call_log is byte-identical.
- `server`: signal-to-paused-waiting-this-name resolves+resumes; signal-to-paused-on-
  other enqueues; signal-to-completed → 409.
- **Determinism**: enqueue-before-listen vs pause-then-deliver produce **identical** final
  call_logs.
- Example: `examples/multiplayer-review/` (the §7 agent) — drafts, `await
  chidori.signal("review")`, a second terminal (or peer agent) POSTs the review; streams
  to tael (`CHIDORI_JS_ENGINE=rust`).

**Phase 2 — Non-blocking poll + fan-in/select + timeout + sender identity in trace** *(shipped)*
- `chidori.pollSignal` (records value *or* null at the seq → deterministic).
- `chidori.signalAny([names])`: pauses on a name *set* (match key `{names:[...]}`,
  function `signal_any`); the result is the bare consumed signal whose `name` says which
  fired; the mailbox drain takes the lowest-`delivery_seq` entry across the whole set.
  A delivery matching ANY name in `pending_signal_names` resolves the pause.
- `timeoutMs` (on both `signal` and `signalAny`): the pause records the timeout, the
  server persists an absolute `pending_signal_deadline` on the session and arms an
  in-process timer (re-armed for every paused session at server startup). On expiry the
  pause resolves to the `{name, payload: null, from: null, timedOut: true}` sentinel
  (`name` is null for a multi-name `signalAny`) — recorded like any signal result, so
  the timed-out run replays deterministically. A delivery that lands first wins; the
  late timer validates against the stored session and no-ops.
- `name`/`from` stamped as OTEL span attributes (`signal.name`, `signal.listen_names`,
  `signal.from.kind/id/run_id`, `signal.timed_out`) on `signal`/`poll_signal`/
  `signal_any` spans.
- Tests: poll returns null then a value deterministically; select fires on the earliest
  matching delivery; timeout resolves the sentinel and replays; delivery-before-timeout
  wins.

**Phase 3 — Live in-memory delivery to running tasks** *(shipped)*
- `ActiveSession.signals` carries the live run's context slot plus a `signal_tx` wake
  channel (analogous to `cancel_tx`). The delivery endpoint enqueues straight into the
  live run's in-memory mailbox — write-through persisted to `inbox.json` in the same
  critical section — and wakes the streaming worker (`202 {"status":"delivered_live"}`).
- The streaming worker (`stream_session`) is a supervision loop: a `signal()` pause
  persists durably, emits a `paused` SSE event, and keeps the stream open; the worker
  `select!`s on `signal_rx` / cancel / the `timeoutMs` deadline and resolves a matching
  pause **in-process** (resolve persisted op + synthetic record + replay re-run, the
  same shape as `/resume`) — skipping the HTTP round-trip. The pinned
  pending-pause-wins-with-newest tie-break is preserved: the worker takes the
  just-delivered entry back out of the mailbox by its `delivery_seq`, leaving older
  queued entries for later listen points. Determinism unchanged (it is still a
  deterministic resume re-run; the live path only removes the disk/HTTP latency).
- Input/approval pauses end live supervision and hand off to the durable HTTP
  endpoints; terminal states close the stream with the usual `done` event.
- Tests: a streaming run's delivered signal records a call identical to the
  persist-resume path; a non-matching live delivery survives the in-process resume and
  is drained by a later `pollSignal`; a streaming `timeoutMs` pause resolves in-process.

---

## 15. Verification & rollout
- `cargo check -p chidori` = 0 errors **with and without** `rust-engine`; QuickJS path
  untouched (`cargo test -p chidori --lib` green).
- `cargo test -p chidori --features rust-engine --lib` green (new signal + the
  `run_with_context` pause-surfacing fix tests).
- Manual multiplayer: terminal A runs the example agent (paused on `signal("review")`);
  terminal B `curl -XPOST .../sessions/{id}/signal -d '{"name":"review","payload":{...},
  "from":{"kind":"human","id":"mara"}}'` → agent resumes; tael shows a `signal` span with
  the `from` attribute, nested correctly.
- Determinism guard: `chidori-js` Test262 baseline unchanged (signals are additive, off
  unless the agent calls them).

## 16. Open questions

**Pinned (decided with Phase 2/3):**
- **Same-name tie-break**: **pending-pause-wins-with-newest** — the pending pause
  resolves with the just-delivered signal; older queued same-name entries stay for later
  listen points. The live worker preserves this by taking the delivered entry back out
  of the mailbox by `delivery_seq`.
- **`timeoutMs` semantics**: **resolve to a sentinel** (`{name, payload: null,
  from: null, timedOut: true}`), never reject — a timeout is an expected outcome the
  agent discriminates on, not an error. Enforcement is server-side (in-process timer
  against a persisted `pending_signal_deadline`, re-armed on restart); a CLI run that
  pauses on a timed listen point just reports the pause.
- **`signalAny` result shape**: the **bare `Signal`** — its `name` already says which
  fired; no wrapper object.

**Still open:**
- **Per-branch addressing** — is a single per-run mailbox enough for the branching
  composition, or is `branch_index`-addressed delivery wanted sooner?
- **Auth/identity for `from`** — is sender identity asserted by the caller (trusted) or
  verified by the server in the multiplayer/agent-to-agent case?

## 17. Future work
- Per-branch signal addressing and a branch-aware delivery endpoint.
- A typed signal-schema registry (declare a run's accepted signal names + payload shapes;
  validate on delivery).
- A tael "participants" view keyed on `from` across a session.
- Broadcast/pub-sub to multiple runs; agent-to-agent signal helpers in the SDK
  (`chidori.sendSignal(runId, name, payload)` as the symmetric send side).
- QuickJS port (it has live-VM snapshots, enabling true continue-in-place delivery).
