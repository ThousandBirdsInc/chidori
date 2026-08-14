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
recorded in the journal, so the whole session replays deterministically.

Signals are the inverse of `chidori.input`. `input` is *agent-initiated
request/response* ("I need an answer, I'll block until I get one"). A signal is
*externally-initiated push at an agent-declared listen point* ("I'm now
receptive — anyone may send me a `review`"). Mechanically a signal is `input()`
with three additions: **(1) a name**, so many distinct listen points coexist;
**(2) the durable mailbox**, so delivery and listening can race safely; **(3) a
`from` provenance field**, so the trace records *who* steered the run.

This turns a run from a closed loop into a **multiplayer** session:

- **Steer a long-running agent without restarting it.** A human or an
  orchestrating agent pushes a correction, a new constraint, or a priority
  change at the next listen point instead of killing the run and re-paying the
  expensive prefix. The agent decides *where* it is safe to be steered.
- **Human-in-the-loop beyond approvals.** The human can volunteer information
  the agent didn't explicitly ask for, delivered when the agent reaches a
  receptive listen point.
- **Agent-to-agent coordination.** An orchestrating agent hands a sub-goal to
  a worker mid-run; a peer delivers a result or a "stop pursuing X" message.
  `from` carries the sender's agent identity, so coordination is attributable.
- **Late-arriving external events.** A webhook fires, an upload finishes — the
  event lands as a signal instead of the agent busy-polling.

What makes this more than a message queue: every signal is a host call in the
journal (the run's call log), so a live, multi-participant session is *still
fully reproducible* — you can replay exactly what every human and agent sent,
in the order it was consumed, and get the identical run. An ordinary message
queue gives you the first half and throws away the second.

Signals compose with [branching](./branching-execution.md): branching forks
*one* agent into many futures; signals open *one* run to many senders. See
[Composition with branching](#composition-with-branching).

---

## API

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
// the result (value OR null) so replay is deterministic at this point.
chidori.pollSignal<T = AgentJson>(name: string): Promise<Signal<T> | null>;

// Fan-in: pause until ANY of the named signals is delivered; the result is the
// bare consumed signal — its `name` says which fired. Pre-arrived candidates
// are consumed in arrival order across the whole name set.
chidori.signal<T = AgentJson>(names: string[], opts?: {
  timeoutMs?: number;          // sentinel `name` is null (no name fired)
}): Promise<Signal<T> | SignalTimeout>;
```

Full signatures live in the [Host API](./host-api.md) reference.

### `timeoutMs`

`timeoutMs` never rejects — a timeout is an expected outcome the agent
discriminates on, not an error. The pause resolves to the
`{ name, payload: null, from: null, timedOut: true }` sentinel (`name` is null
for a multi-name fan-in listen, since no name fired). A delivery that lands
before the deadline wins. The sentinel is recorded like any signal result, so a
timed-out run replays deterministically.

Who actually enforces the deadline depends on where the wait happens:

- **Under `chidori serve`**, the runtime records the deadline on the pause and
  the serving process enforces it: a timer is armed for every paused session,
  and re-armed for all of them when the server starts up — so deadlines survive
  a server restart.
- **Inside an [actor](./actors.md) or a [detached agent](./detached-agents.md)**,
  the deadline is enforced in-process — the wait resolves to the timeout
  sentinel on its own, with no server involved.
- **Under a bare top-level `chidori run`**, a signal pause ends the process:
  the CLI prints that the run paused awaiting the signal and exits, and no
  process remains to fire the timer — `timeoutMs` is inert there. Delivering
  the signal later (through a server, or on resume) still works normally.

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

### The agent ([`examples/multiplayer-review/policy_doc.ts`](../examples/multiplayer-review/policy_doc.ts))

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

A **human** (Mara) delivers a review from a UI or curl. (A session id is a run
id — the same identifier addresses the run over HTTP; see
[Running Modes](./running-modes.md).)

```bash
curl -XPOST localhost:8080/sessions/$SESSION_ID/signal -d '{
  "name": "review",
  "payload": { "decision": "changes", "notes": "Tighten the data-retention section." },
  "from": { "kind": "human", "id": "mara" }
}'
```

The **compliance-checker agent** delivers its verdict by calling the same
endpoint — it is just another participant, identified as an agent:

```ts
// inside the compliance agent, after scanning the draft it fetched
await fetch(`${chidoriUrl}/sessions/${targetSessionId}/signal`, {
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
next `chidori.pollSignal`:

```bash
curl -XPOST localhost:8080/sessions/$SESSION_ID/signal -d '{
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

`chidori trace <run_id>` gives a complete, ordered audit: *who* reviewed each
draft, *what* they said, *when* the lead re-scoped, and which reviewer's
"approve" published the doc. And `chidori resume policy_doc.ts <run_id>` (or
any [replay](./replay.md)) reproduces the **identical** run — the editor's
notes, the compliance verdict, and the steering come back from the journal, so
a later "why did it publish?" investigation re-derives the exact decision path
without re-contacting any human or re-running the compliance agent. A live,
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
| **Streaming** (a live worker is supervising the run) | `202 {"status":"delivered_live"}` | The signal goes straight into the live run's mailbox (durably — it is written to disk in the same step). A run mid-execution consumes it at its next listen point; a run idling on a matching listen point resolves and continues **in-process**, and the SSE stream stays open across the resume. |
| **Paused, waiting on THIS name** (a `chidori.signal` on that name, or a fan-in listen set containing it) | `200` + updated session view | **Resolve + resume**: the pending listen resolves with `{name, payload, from}`, the consumption is recorded in the journal, and the run continues to its next pause or completion — the same flow `/resume` uses. |
| **Paused on a DIFFERENT name / on input / on approval, or running with no live worker** | `202 {"status":"queued"}` | **Enqueue** into the run's durable mailbox. The run stays where it is; the entry is consumed when it reaches a matching listen point. |
| **Completed / Failed / Cancelled** | `409 Conflict` | The signal is rejected outright — nothing is queued, so a finished run's record is never muddied by mail it can no longer read. |

Same-name tie-break: **the pending pause wins, with the newest signal**. If the
run is paused waiting on name X and an older same-name entry is *also* already
queued in the mailbox, the pause resolves with the just-delivered signal; the
older queued entry stays in the mailbox for the next listen point.

---

## Determinism

A signal recorded in the journal replays identically, regardless of whether it
was delivered by pause-and-resume, consumed from the mailbox, or resolved by a
live worker.

- **The result is read from the journal, not the world.** On replay,
  `{name, payload, from}` comes verbatim from the recorded call — neither the
  mailbox nor the HTTP endpoint is consulted for an already-recorded
  consumption. A replay run can have an *empty* mailbox and still reproduce
  every consumed signal. **The journal is the source of truth; the mailbox is a
  live-only convenience. Replay never re-reads the mailbox.**
- **Ordering is captured two agreeing ways.** Across *different* listen points,
  the journal orders consumption by the agent's own control flow, independent
  of arrival timing. For *same-name* signals competing for one listener, the
  earliest-delivered queued entry is consumed and that choice is frozen into
  the recorded result. Two same-name signals arriving before two
  `chidori.signal(name)` calls: the first call consumes the earlier arrival,
  the second consumes the later one, both recorded; replay reproduces both.
- **Consumption is crash-safe.** Removing an entry from the mailbox and
  recording its consumption in the journal happen as one atomic step, so a
  crash cannot double-deliver: on restart the recorded result wins and the
  mailbox is never re-drained for that consumption. A run whose signals all
  pre-arrived produces the identical final journal as one that paused and was
  resumed for each.

Signals are consumed **only at agent-declared listen points** — never as
preemptive interrupts that could fire at an arbitrary instruction. That is the
determinism contract: delivery timing is free, consumption points are the
agent's, and everything consumed is recorded in the journal in a deterministic
order.

---

## Edge cases

- **Signal to a completed/failed/cancelled run:** `409 Conflict`, nothing
  queued.
- **Two same-name signals with the run paused waiting on that name:** the
  pending pause resolves with the *newly arrived* signal; the older queued
  entry stays for the next `chidori.signal(name)`.
- **Concurrent delivery and resume:** deliveries to one run are serialized, so
  an HTTP delivery cannot race a resuming run or a live worker's mailbox drain.
- **`from` provenance:** `from = {kind, id, runId?}` rides in the recorded
  result, so it is in the durable journal, appears in streamed call events, and
  is stamped on the run's OTEL trace spans.

---

## Composition with branching

The mailbox is **per-run, not per-branch** — a
[branch](./branching-execution.md) sub-run listening on `chidori.signal(name)`
drains the shared parent mailbox, and its consumption records land in the
branch's own journal, so determinism composes. Together the two primitives give
the full picture: **branch** to explore N futures, **signal** to let
participants steer or pick among them.

---

## Design notes

The mailbox is a small durable inbox file inside the run directory, separate
from the journal, precisely so the HTTP endpoint can accept deliveries while
the run is not live. Consuming an entry removes it from the inbox and records
the consumption in the journal in one atomic step — which is why crash recovery
can never double-deliver, and why replay only ever needs the journal.
