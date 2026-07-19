---
title: "Signals"
description: "Named listen points for multiplayer sessions: pause for humans or other agents, durable mailboxes, fan-in, timeouts."
---

# Signals — multiplayer agents

A **signal** is a named message `{ name, payload, from }` addressed to a specific
run and delivered mid-flight. The agent declares a listen point
(`const review = await chidori.signal("review")`); an outside party — a human UI,
a curl, or another agent — delivers to it (`POST /sessions/{id}/signal`). A
durable **per-run mailbox** absorbs signals that arrive *before* the agent is
listening, so delivery and listening never race. Every consumed signal is
recorded in the call_log, so the whole session replays deterministically.

Signals are the inverse of `chidori.input`. `input` is *agent-initiated
request/response* ("I need an answer, I'll block until I get one"). A signal is
*externally-initiated push at an agent-declared listen point* ("I'm now
receptive — anyone may send me a `review`"). Mechanically a signal is `input()`
with three additions: **(1) a name**, so many distinct listen points coexist;
**(2) the durable mailbox**, so delivery and listening can race safely; **(3) a
`from` provenance field**, so the trace records *who* steered the run.

This turns a run from a closed loop into a **multiplayer** session:

- **Steer a long-running agent without restarting it.** A human or supervisor
  agent pushes a correction, a new constraint, or a priority change at the next
  listen point instead of killing the run and re-paying the expensive prefix.
  The agent decides *where* it is safe to be steered.
- **Human-in-the-loop beyond approvals.** The human can volunteer information
  the agent didn't explicitly ask for, delivered when the agent reaches a
  receptive checkpoint.
- **Agent-to-agent coordination.** A supervisor hands a sub-goal to a worker
  mid-run; a peer delivers a result or a "stop pursuing X" message. `from`
  carries the sender's agent identity, so coordination is attributable.
- **Late-arriving external events.** A webhook fires, an upload finishes — the
  event lands as a signal instead of the agent busy-polling.

What makes this more than a message queue: every signal is a host effect in the
durable call_log, so a live, multi-participant session is *still fully
reproducible* — you can replay exactly what every human and agent sent, in the
order it was consumed, and get the identical run. A raw mpsc channel gives you
the first half and throws away the second.

Signals compose with [branching](./branching-execution.md): branching forks
*one* agent into many futures; signals open *one* run to many senders. See
[Composition with branching](#composition-with-branching). Related:
[`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md),
[`docs/architecture.md`](./architecture.md).

---

## API

### Agent-facing

```ts
import { chidori } from "chidori:agent";

type Signal<T = AgentJson> = { name: string; payload: T; from: SignalSender };
type SignalSender = { kind: "human" | "agent"; id: string; runId?: string };

type SignalTimeout = { name: string | null; payload: null; from: null; timedOut: true };

// Blocking: pause at a named listen point until a matching signal is delivered
// (or already queued in the mailbox). With timeoutMs, resolves to the
// SignalTimeout sentinel after the deadline (discriminate with "timedOut" in r).
chidori.signal<T = AgentJson>(name: string, opts?: {
  timeoutMs?: number;
}): Promise<Signal<T> | SignalTimeout>;

// Non-blocking: consume a queued signal if present, else null. Records
// the result (value OR null) so replay is deterministic at this seq.
chidori.pollSignal<T = AgentJson>(name: string): Promise<Signal<T> | null>;

// Fan-in: pause until ANY of the named signals is delivered; the result is the
// bare consumed signal — its `name` says which fired. Pre-arrived candidates
// are consumed in arrival order (lowest delivery_seq across the name set).
chidori.signal<T = AgentJson>(names: string[], opts?: {
  timeoutMs?: number;          // sentinel `name` is null (no name fired)
}): Promise<Signal<T> | SignalTimeout>;
```

`timeoutMs` never rejects — a timeout is an expected outcome the agent
discriminates on, not an error. The pause resolves to the
`{ name, payload: null, from: null, timedOut: true }` sentinel (`name` is null
for a multi-name fan-in listen, since no name fired). Enforcement is server-side:
an in-process timer armed against a persisted `pending_signal_deadline` on the
session, re-armed for every paused session at server startup. A delivery that
lands first wins; the late timer validates against the stored session and
no-ops. The sentinel is recorded like any signal result, so a timed-out run
replays deterministically.

### SDK types

`Signal`, `SignalSender`, and `signal`/`pollSignal` live on the
`Chidori` interface in `sdk/typescript/src/agent.ts` (types only; the runtime
supplies the methods, like the other host methods).

---

## Worked example — a multiplayer policy-doc drafting agent

A team uses an agent to turn a short brief into a publishable policy document.
The expensive part is the drafting (several LLM calls + retrieval); the
*judgment* part needs people and a compliance checker. Three participants
collaborate on one live run:

- a **human editor** (Mara) who reviews drafts and asks for changes,
- a **compliance-checker agent** that scans each draft and pushes a verdict, and
- a **human lead** (Sam) who can change the document's priority/scope mid-run.

None of these are agent-initiated `input()` questions. The editor and the
compliance agent **push** information *when they have it*; the lead **steers**
*whenever he wants*. The agent only *consumes* those pushes at the points it
declares safe.

### The agent (`examples/multiplayer-review/policy_doc.ts`)

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
- `chidori.signal("review")` **blocks** — the agent has nothing to do until a
  review arrives; it pauses, persists, and the run idles cheaply on disk.
- `chidori.pollSignal("steer")` is **non-blocking** — the lead's steering is
  optional; the agent checks the mailbox and moves on if it's empty.

### The senders

A **human** (Mara) delivers a review from a UI or curl:

```bash
curl -XPOST localhost:8080/sessions/$RUN/signal -d '{
  "name": "review",
  "payload": { "decision": "changes", "notes": "Tighten the data-retention section." },
  "from": { "kind": "human", "id": "mara" }
}'
```

The **compliance-checker agent** delivers its verdict by calling the same
endpoint — it is just another participant, identified as an agent:

```ts
// inside the compliance agent, after scanning the draft it fetched
await fetch(`${chidoriUrl}/sessions/${targetRun}/signal`, {
  method: "POST",
  body: JSON.stringify({
    name: "review",
    payload: { decision: violations.length ? "changes" : "approve", notes: summarize(violations) },
    from: { kind: "agent", id: "compliance-bot" },
  }),
});
```

The **lead** (Sam) steers at any time — even while the agent is mid-revision and
not yet listening. The signal lands in the **mailbox** and is consumed at the
next `pollSignal`:

```bash
curl -XPOST localhost:8080/sessions/$RUN/signal -d '{
  "name": "steer",
  "payload": { "priority": "high", "scope": "EU + UK only" },
  "from": { "kind": "human", "id": "sam" }
}'
```

### The trace

Each signal is a recorded host call, so the multiplayer session streams as one
trace with every participant attributed by `from`:

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

### Why durability matters here

`chidori trace $RUN` gives a complete, ordered audit: *who* reviewed each draft,
*what* they said, *when* the lead re-scoped, and which reviewer's "approve"
published the doc. And `chidori resume policy_doc.ts $RUN` (or any replay)
reproduces the **identical** run — the editor's notes, the compliance verdict,
and the steering come back from their recorded `CallRecord`s, so a later "why
did it publish?" investigation re-derives the exact decision path without
re-contacting any human or re-running the compliance agent. A live,
multi-participant collaboration that is also a deterministic, auditable
artifact.

---

## Delivering signals

```
POST /sessions/{id}/signal     body: { name, payload, from }
```

`name` is a required non-empty string (400 otherwise); `payload` is any JSON
(default `null`); `from` is an optional provenance object (default `null`).
The server routes on the run's state:

| Run state | Response | Behavior |
|---|---|---|
| **Streaming** (a live worker supervises the run) | `202 {"status":"delivered_live"}` | The signal is enqueued straight into the live run's in-memory mailbox (write-through to `signals/inbox.json` in the same critical section) and the worker is woken. A run mid-execution drains it at its next listen point; a run idling on a matching listen point is resolved and resumed **in-process** — no HTTP round-trip — and the SSE stream stays open across the resume. |
| **Paused, waiting on THIS name** (a `Signal` op whose `{name}` matches, or a fan-in listen set containing it) | `200` + updated session view | **Resolve + resume**: the pending op is resolved with `{name, payload, from}`, a synthetic `signal` CallRecord is injected at the pending seq, and the run re-runs to its next pause or completion — the same machinery `/resume` uses. |
| **Paused on a DIFFERENT name / on input / on approval, or Running** (no live worker) | `202 {"status":"queued"}` | **Enqueue** into `signals/inbox.json` with an assigned `delivery_seq`. The run stays where it is; the entry is drained when it reaches a matching listen point. |
| **Completed / Failed / Cancelled** | `409 Conflict` | No inbox write — an orphan inbox would mislead a later replay. |

Same-name tie-break: **pending-pause-wins-with-newest**. If the run is paused on
name X and a same-name entry is *also* already queued in the inbox, the pending
pause resolves with the just-delivered signal; the older queued entry stays in
the inbox for the next listen point. The live worker preserves the same rule by
taking the just-delivered entry back out of the mailbox by its `delivery_seq`.

Input and approval pauses end live supervision and hand off to the durable HTTP
endpoints; terminal states close the stream with the usual `done` event.

---

## How it works

A signal is a named, externally-deliverable flavor of an `input` pause, fronted
by a durable mailbox, built on the same suspension substrate as every other
pausing host call (`PendingHostOperation` / `HostPromiseTable` / `pending.json`).

**The listen point.** When the agent calls `chidori.signal(name)`, the runtime
(`execute_signal` in `crates/chidori/src/runtime/host_core.rs`), in order:

1. **Replays** a recorded result if this seq is already in the journal.
2. Returns a **completed host-op** if one matches
   `(seq, PendingHostOperationKind::Signal, args = {name})`. The match key is
   the *name only* — the payload is unknown at pause time; `{name, payload,
   from}` live in the recorded **result**, mirroring how `input` keeps the
   prompt in args and the answer in the result.
3. **Drains the mailbox** — if a queued signal of that name exists, consumes it
   *without pausing* and records a completed call.
4. Otherwise **pauses**: persists the pending op at a safepoint, sets a
   `PendingSignal { seq, name, id }` on the context (a sibling of
   `PendingInput`), and throws the pause marker. The engine surfaces the pause
   as `RunResult { paused_signal: Some(PendingSignal { seq, name, .. }) }`, so
   the server knows *which* named op is waiting. `pollSignal` stops after step
   3, recording the value *or* `null`; the fan-in `signal([...])` pauses on a name *set*
   (match key `{names: [...]}`) and drains the lowest-`delivery_seq` entry
   across the whole set.

**The mailbox.** `.chidori/runs/{id}/signals/inbox.json` is an ordered
`Vec<QueuedSignal { name, payload, from, delivery_seq, enqueued_at }>`.
`delivery_seq` is a monotonic counter assigned at delivery time, freezing global
arrival order across all senders. It is a standalone file (like `pending.json`),
not a manifest field, because the HTTP endpoint writes it while the run is not
live. In memory, the `RuntimeContext` carries the inbox (loaded at run/resume
start, threaded the same way the VFS is). `take_queued_signal(name)` removes the
lowest-`delivery_seq` matching entry and persists the shrunken inbox inside the
same critical section that records the completed call — so a crash cannot
double-deliver; on restart the recorded result wins and the inbox is never
re-drained for that seq. Concurrent deliveries to one run are serialized by a
per-run inbox lock.

**Resume.** Delivery to a matching pause reuses the `/resume` machinery:
`complete_persisted_pending_host_operation` resolves the persisted op, a
synthetic `signal` CallRecord is injected at the pending seq, and
`run_replay_pausable_with_host_promises_vfs_and_signals` re-runs the agent to
its next pause or completion. The resume worker loads the inbox alongside the
host promises and VFS, so a resumed run that reaches a *second* listen point
drains any still-queued entry instead of pausing again.

**Live delivery.** For a streaming run, the supervising worker is a supervision
loop: a `signal()` pause persists durably, emits a `paused` SSE event, and keeps
the stream open; the worker selects on the signal wake channel, cancellation,
and any `timeoutMs` deadline, and resolves a matching pause in-process (resolve
persisted op + synthetic record + replay re-run — the same shape as `/resume`,
minus the HTTP latency). Determinism is unchanged: the live path is still a
deterministic resume re-run.

**Tracing.** A signal call is recorded like any host call, so it carries
`parent_seq` and streams as a span under its parent. `name` and `from` ride in
the record and are stamped as OTEL span attributes (`signal.name`,
`signal.listen_names`, `signal.from.kind/id/run_id`, `signal.timed_out`) on
`signal`/`poll_signal`/`signal_any` spans.

---

## Determinism

A signal recorded in the call_log replays identically, regardless of whether it
was delivered by pause-resume, consumed from the mailbox, or resolved by the
live worker.

- **The match key is deterministic.** `(seq, PendingHostOperationKind::Signal,
  args = {name})`: `seq` comes from the deterministic `next_seq()` walk, so it
  is identical on every replay (inductively — the pre-signal prefix is
  deterministic, so the signal lands at the same seq); `kind` and `name` are
  structural. `completed_operation(seq, Signal, {name})` and the replay check
  find the same record every time.
- **The result is read from the log, not the world.** `{name, payload, from}`
  comes verbatim from the `CallRecord` — neither the inbox nor the endpoint is
  consulted for an already-recorded seq. The replay and completed-op checks
  short-circuit before the mailbox drain is ever reached, so a replay run can
  have an *empty* inbox and still reproduce a consumed signal. **The log is the
  source of truth; the inbox is a live-only convenience. Replay never re-reads
  the inbox.**
- **Ordering is captured two agreeing ways.** Across *different* listen points,
  `seq` totally-orders consumption by the agent's own control flow, independent
  of arrival timing. For *same-name* signals competing for one listener, the
  lowest-`delivery_seq` queued entry is consumed and that choice is frozen into
  the result. Two same-name signals arriving before two `signal(name)` calls:
  the first call consumes `delivery_seq` N, the second consumes N+1, both
  recorded; replay reproduces both from their records.

Consumption removes the entry from `inbox.json` *and* writes the `CallRecord`
in one critical section, so enqueue-then-consume is crash-safe and a run that
pre-arrived its signals produces the identical final call_log as one that
paused and was resumed.

Signals are consumed **only at agent-declared listen points** — never as
preemptive interrupts that could fire at an arbitrary instruction. That is the
determinism contract: delivery timing is free, consumption points are the
agent's, and everything consumed is recorded at a deterministic seq.

---

## Edge cases

- **Signal to a completed/failed/cancelled run:** `409 Conflict`, no inbox
  write.
- **Two same-name signals with the run paused waiting on that name:** the
  pending pause resolves with the *newly arrived* signal
  (pending-pause-wins-with-newest); the older queued entry stays for the next
  `signal(name)`.
- **Concurrent delivery and resume:** `inbox.json` read-modify-write is guarded
  by a per-run lock, so an endpoint enqueue cannot race a resume worker or the
  live worker's drain.
- **`from` provenance:** `from = {kind, id, runId?}` rides in the
  `CallRecord.result`, so it is in the durable log, rides the normal call
  events, and is stamped on the OTEL span.

---

## Composition with branching

The mailbox is **per-run, not per-branch** — a branch (see
[`docs/branching-execution.md`](./branching-execution.md)) listening on
`signal(name)` drains the shared parent inbox. Each branch's reserved
`CallLogSequenceRange` means its signal `CallRecord`s stay in-range
automatically, so determinism composes. Together the two primitives give the
full picture: **branch** to explore N futures, **signal** to let participants
steer or pick among them.
