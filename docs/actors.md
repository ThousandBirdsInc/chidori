---
title: "Actors"
description: "Supervised, message-passing agent processes: spawn, send, receive, join, restart strategies, and supervision trees."
---

# Actors: supervised, message-passing agent processes

An **actor** is a supervised, addressable, concurrent agent process: an agent
module started by a run, executing alongside it with its own isolated VM, its
own durable mailbox that other parties can deliver into while it runs, and a
runtime-owned restart policy. The spawning run talks to it through named
messages and eventually **settles** it — collecting its outcome and folding
its recorded history into the run's own journal.

```ts
import { chidori, run } from "chidori:agent";

run(async () => {
  // Start two workers with supervision: on failure, replay their completed
  // work from the journal and retry the failing call, up to 3 times.
  const a = await chidori.actors.spawn("workers/researcher.ts", { topic: "pricing" }, {
    name: "researcher",
    restart: "resume",
    maxRestarts: 3,
    backoffMs: 500,
  });
  const b = await chidori.actors.spawn("workers/critic.ts");

  // Talk to them through their handles while they run.
  await a.send("focus", { region: "EU" });
  const draft = await chidori.receive("draft");           // sent by the researcher
  await b.send("review", draft.payload);

  // Settle them: outcomes carry output/error/restart counts, and each actor's
  // full call history folds into this run's journal.
  const research = await a.join();
  const review = await b.join();
  return { research: research.output, review: review.output };
});
```

## The model

Actors sit between two neighboring primitives. Like a
[branch](./branching-execution.md) sub-run, an actor runs its own source
module on a fresh, isolated VM and its records ultimately join the spawning
run's journal. Unlike a branch it is *concurrent and addressable*: it runs
alongside the spawning run, has a mailbox other parties can deliver into while
it runs, and settles when it finishes (or when its restart budget is spent),
not when a fan-out returns. And unlike a
[detached agent](./detached-agents.md), an actor lives *inside* its spawning
run — it is joined by its owner and gone when the run ends.

**Each actor runs under a supervision loop.** One iteration = one pass of the
actor's module under the standard resume-by-replay model: the actor's
accumulated journal replays from the top (recorded effects return from cache,
side-effect free), then execution goes live at the frontier.

In the steady state an actor stays **live across messages**: a
`chidori.signal` listen point with an empty mailbox blocks in place until the
next matching message (or the listen point's own `timeoutMs` — enforced
in-process inside an actor) arrives and then simply continues — the module is
not re-executed per message, so processing M messages costs O(M), not O(M²).
The loop re-enters the module only when:

- the actor parks — the idle cap elapses with no message, or a stop is
  requested — and a later delivery wakes it (resume-by-replay);
- an iteration fails and the spawn's restart policy allows another attempt.

**Messages are signals.** An actor's mailbox uses the same
`{ name, payload, from }` envelope and delivery-ordered consumption as the
[signals](./signals.md) mailbox, and messages are consumable at the standard
listen points (`chidori.signal`, `chidori.pollSignal`) as well as with
`chidori.receive`. `from` carries the sender's identity in the same shape
external signals use: `{ kind: "agent", id }`, where `id` is the sending
actor's pid or `"run"`.

## API

### actors.spawn

```ts
const worker = await chidori.actors.spawn(source, input?, {
  name?: string,          // register for actors.lookup / actors.send addressing;
                          // "parent" and "actor-*" are reserved and rejected
  restart?: "never" | "clean" | "resume",   // default "never"
  maxRestarts?: number,   // default 3
  backoffMs?: number,     // base restart delay, doubles per attempt; default 0
  idleTimeoutMs?: number, // empty-mailbox park cap; default 300000 (5 minutes)
  intercept?: {           // narrow the child's context (never widen)
    model?: string,       // default model for the child's prompts
    tools?: string[],     // registry tools the child may call (intersection)
    workspace?: string,   // relative subpath the child's workspace narrows to
  },
});
```

`source` resolves like a `branch` variant source: the actor's own module, run
with `input`. Pids are allocated in spawn order (`actor-1`, `actor-2`, …) —
which is why `actor-*` names are reserved. A run may spawn at most 128 actors
in total (the whole tree, restarted children included). Actors can spawn
actors — see [Supervision trees](#supervision-trees) — but joining and
stopping are **owner-only**: an actor is settled by whoever spawned it (its
records fold into the spawner's journal). `actors.spawn` inside a
`chidori.branch` sub-run is rejected, like nested branches.

### intercept: handing a child a narrower context

`intercept` scopes what the child sees, and every field can only **narrow**
what the spawner itself holds — a child never widens:

- `model` re-points the child's default model (a routing choice, not a
  capability): an orchestrator on a strong model fans work out to cheap
  workers without each worker hard-coding one.
- `tools` intersects with the spawner's tool registry — a name the spawner
  doesn't hold simply doesn't exist for the child. Registry tools only
  (MCP / native); in-VM `defineTool` functions are plain code in the child's
  own module.
- `workspace` must be a relative, `..`-free subpath; the child's workspace
  root becomes the spawner's root joined with it, so its reads and writes are
  confined to that subtree.

The intercept travels in the durable spawn record, so restarts and replay
re-create the child under the identical narrowed view. Children that spawn
their own actors narrow from their already-narrowed context — the tree only
ever gets tighter toward the leaves.

```ts
const researcher = await chidori.actors.spawn("workers/research.ts", input, {
  intercept: {
    model: "claude-haiku-4-5",
    tools: ["search"],
    workspace: "research",
  },
});
```

### handles and actors.send

```ts
// The handle is the usual way to talk to an actor you spawned:
await worker.send("message-name", payload?);
const outcome = await worker.join();

// String-addressed forms cover actors known only by pid or registered name:
await chidori.actors.send(pidOrName, "message-name", payload?);
// → { delivered: boolean }   (false once the target has settled)
```

Delivery never blocks. `to = "parent"` addresses the sender's spawner: the
owning actor for a child in a supervision tree, or the spawning run for a
top-level actor.

### receive

`chidori.receive` is a **top-level host function** — it is not under
`chidori.actors`, because it works in any run, actor or not:

```ts
const msg = await chidori.receive("draft");                  // { name, payload, from }
const msg = await chidori.receive(["draft", "cancel"]);      // fan-in
const msg = await chidori.receive("draft", { timeoutMs: 60000 }); // may be { timedOut: true }
```

Blocking, in-place consumption, in delivery order. Inside an actor it drains
the actor's own mailbox; in the spawning run it drains parent-addressed
messages (plus any pre-queued external signals). The difference from
`chidori.signal`: `signal` pauses the whole run — unwinding the VM so an
*external* party can deliver-and-resume later — while `receive` parks in
place and is woken directly by in-process senders. Use `signal` for
deliveries from outside the process, `receive` for actor traffic. A `receive`
with no timeout, no live actors, and an empty mailbox fails fast instead of
blocking forever; the timeout sentinel is the same `{ timedOut: true }` shape
signals use.

### Monitoring: `__chidori.down__`

An actor that settles **without producing what its owner is waiting on** —
`failed` (restart budget spent) or `paused` (parked on something the runtime
can't answer in-process) — delivers a monitor message to its owner's mailbox
under the reserved name `__chidori.down__`, with payload
`{ pid, name, status, error?, pendingPrompt?, restarts }`. Include it in a
fan-in so a collection loop reacts to worker death immediately instead of
waiting out its timeout:

```ts
const msg = await chidori.receive(["finding", "__chidori.down__"], { timeoutMs: 480000 });
if (msg.name === "__chidori.down__") {
  const down = msg.payload as { pid: string; status: string; error?: string };
  await chidori.log("worker down", down);   // reassign, degrade, or bail
}
```

(`completed` and `stopped` settles deliver nothing — those are the owner's
own `join`/`stop` flow.)

As a backstop, a `receive` — even one with a `timeoutMs` — **fails fast**
once every spawned actor has settled and no matching message is queued:
nothing in-process can deliver anymore, so waiting out the timeout would be
pure starvation. The error names `__chidori.down__` as the way to observe
the failures.

### actors.join / actors.stop

```ts
const outcome = await worker.join();
// → { pid, status, output?, error?, pendingPrompt?, restarts }
const partial = await worker.join({ timeoutMs: 5000 });
// → { pid, status: "running", restarts } when not settled yet — join again later
const stopped = await worker.stop();   // cooperative stop, then join
```

`status` is `"completed"` (with the actor's return value), `"failed"` (restart
budget spent; carries the final error), `"paused"` (parked on something the
runtime can't answer in-process — interactive `input()`, a policy approval, or
the idle cap on a mailbox wait), or `"stopped"`. `stop` is cooperative:
honored between iterations, at mailbox waits, and during restart backoff; a
live LLM/tool call finishes first. Both are owner-only: an actor is settled
by whoever spawned it.

### actors.status / actors.lookup

```ts
await worker.status();  // { pid, status, restarts, mailbox, waitingFor? }
await chidori.actors.lookup("researcher");  // a handle, or null
```

## Restart strategies

| Strategy | On iteration failure |
|---|---|
| `never` (default) | The failure is the actor's final outcome. |
| `clean` | Re-run the module from scratch: fresh journal, the spawn-time workspace anchor, the original input. |
| `resume` | Replay the accumulated journal with the **crash frontier** (the trailing failed records) stripped: completed work returns from cache, the failing call re-executes live. The strip cascades to the frontier's *nested* effects — a failed tool call's inner HTTP record is discarded with it, so the retry re-drives the upstream for real instead of replaying a recorded 5xx forever. |

`resume` is the strategy a process-restart model cannot express: the actor
comes back *with its history* and retries from the exact point of failure,
without re-paying (or re-firing) any recorded LLM call, tool call, or message
consumption. Failed records *before* the frontier — errors the agent caught
and handled — are preserved, since their consumption shaped the control flow
that followed. Note that a deterministic in-code `throw` (one not caused by a
live host-call failure) will recur under `resume`; `maxRestarts` bounds the
loop either way. Messages consumed by a failed attempt are redelivered under
`resume` (their consumption is in the replayed journal) but lost under
`clean`, matching the from-scratch semantics.

## Supervision trees

Actors spawn actors, forming a supervised hierarchy — a worker pool per
supervisor, a supervisor per pipeline stage, each level with its own restart
policy:

```ts
// supervisor.ts — spawned by the run, supervises its own worker pool.
import { chidori, run } from "chidori:agent";

run(async (input: { shards: string[] }) => {
  const workers = [];
  for (const shard of input.shards) {
    workers.push(await chidori.actors.spawn("worker.ts", { shard }, {
      restart: "resume",     // this supervisor's policy for ITS children
      maxRestarts: 3,
    }));
  }
  const results = [];
  for (const w of workers) {
    const outcome = await w.join();   // owner-only
    results.push(outcome.output);
  }
  return { results };
});
```

The tree rules:

- **Ownership.** Every actor records who spawned it. Only the owner may
  `join`/`stop` it; anyone may `send` to it. `"parent"` from a
  child addresses its owning actor's mailbox (received there with
  `chidori.receive`), not the run.
- **Depth is bounded.** The tree supports three generations of actors below
  the run; a fourth-generation `spawn` is refused with a clear error.
- **Supervisors reap their children.** When an actor settles — completed,
  failed, stopped, or paused — its still-live children are cooperatively
  stopped first, transitively, so children never outlive their supervisor. A
  `clean` restart also reaps the failed attempt's children (its discarded
  journal is about to re-run the spawns live) and releases their registered
  names for the retry to re-claim. A `resume` restart keeps children: the
  replayed spawns return their cached pids and the same live children answer.
- **A join settles the whole subtree.** A child's records fold into its
  owner's history at the owner's join, so by the time the run joins a
  top-level actor, that one settle carries every level below it.

## Durability and replay

Every actor primitive — spawn, send, receive, join, stop, status, lookup — is
an ordinary recorded host call on the calling run's journal, so the whole
conversation replays from cache: a replayed parent never re-runs actors,
re-delivers messages, or re-waits.

The actor's own records fold into the parent's journal at the join, nested
under it — so on replay, replaying the join replays the whole actor subtree,
and the full cross-actor trace lives in one journal.

If a run crashes *between* a spawn and its join, the actor's in-flight records
were never folded in and are discarded — but the recorded spawn and sends are
sufficient to re-create it. On resume, the first live call that addresses the
actor (a send, join, stop, or status) re-spawns it fresh and re-seeds its
mailbox from the recorded sends, so unjoined actor work re-executes rather
than being lost (at-least-once semantics for the unjoined window).

## Semantics worth knowing

- **Concurrency is real but bounded**: each actor runs on its own thread with
  its own VM (like a concurrent branch wave). Actors suit tens of concurrent
  LLM-bound processes, not tens of thousands of compute-bound ones.
- **Selective receive** falls out of names: `receive(["a", "b"])` and the
  fan-in `signal([...])` consume the earliest-delivered match and leave
  everything else queued.
- **Idle actors park, not leak**: an actor waiting on an empty mailbox with no
  explicit timeout settles as `paused` after `idleTimeoutMs` (default 5
  minutes), so an orphaned wait cannot hold a thread forever — and a settling
  supervisor reaps its subtree on the way out.
- **Join what you spawn**: records only fold in at a join/stop, and only the
  spawner may settle an actor. Ending the parent run with actors unjoined
  discards their (unfolded) work.
- **Hot code reload across restarts**: each supervision-loop iteration
  re-reads the actor's source module, so an edited module + `resume` restart
  follows the same modify-and-resume contract as run resume (divergence
  detection applies — see [Replay & Resume](./replay.md)).
