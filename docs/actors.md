# Actors: supervised, message-passing agent processes

Actors let one agent run start other agent modules as long-lived, concurrent,
addressable processes — each with its own durable mailbox and a runtime-owned
restart policy — and coordinate with them through named messages. They compose
three things the runtime already guarantees (isolated per-run VMs, durable
mailboxes, resume-by-replay) into a process model:

```ts
import { chidori, run } from "chidori:agent";

run(async () => {
  // Start two workers with supervision: on failure, replay their completed
  // work from the journal and retry the failing call, up to 3 times.
  const a = await chidori.spawnActor("workers/researcher.ts", { topic: "pricing" }, {
    name: "researcher",
    restart: "resume",
    maxRestarts: 3,
    backoffMs: 500,
  });
  const b = await chidori.spawnActor("workers/critic.ts");

  // Talk to them while they run.
  await chidori.sendActor(a.pid, "focus", { region: "EU" });
  const draft = await chidori.receive("draft");           // sent by the researcher
  await chidori.sendActor(b.pid, "review", draft.payload);

  // Settle them: outcomes carry output/error/restart counts, and each actor's
  // full call history folds into this run's durable log.
  const research = await chidori.joinActor(a.pid);
  const review = await chidori.joinActor(b.pid);
  return { research: research.output, review: review.output };
});
```

## The model

**An actor is a supervised sibling of a [branch](./branching-execution.md)
sub-run.** Like a branch it runs its own source module on a fresh, isolated VM
with records confined to a reserved, disjoint call-log sequence range. Unlike a
branch it is *detached and addressable*: it runs concurrently with the spawning
run on its own OS thread, has a mailbox other parties can deliver into while it
runs, and settles when it finishes (or when its restart budget is spent), not
when a fan-out returns.

**Each actor runs a supervision loop.** One iteration = one pass of the actor's
module under the standard resume-by-replay model: the actor's accumulated call
log replays from the top (recorded effects return from cache, side-effect
free), then execution goes live at the frontier. The loop re-enters the module
when:

- a message arrives for an empty-mailbox listen point (`chidori.signal` /
  `signalAny` inside an actor park the thread instead of ending the run);
- an iteration fails and the spawn's restart policy allows another attempt.

**Messages are signals.** An actor's mailbox uses the same
`{ name, payload, from }` envelope and delivery-ordered consumption as the
[signals](./signals.md) mailbox, and messages are consumable at the standard
listen points (`chidori.signal`, `pollSignal`, `signalAny`) as well as with
`chidori.receive`. `from` is the sender's pid, or `"parent"` for the spawning
run.

## API

### spawnActor

```ts
const { pid, name } = await chidori.spawnActor(source, input?, {
  name?: string,          // register for whereis/sendActor addressing
  restart?: "never" | "clean" | "resume",   // default "never"
  maxRestarts?: number,   // default 3
  backoffMs?: number,     // base restart delay, doubles per attempt; default 0
  idleTimeoutMs?: number, // empty-mailbox park cap; default 300000
});
```

`source` resolves like a `branch` variant source: the actor's own module, run
with `input`. Pids are allocated in spawn order (`actor-1`, `actor-2`, …). A
run may spawn at most 128 actors in total (the whole tree, restarted children
included). Actors can spawn actors — see [Supervision
trees](#supervision-trees) — but joining and stopping are **owner-only**: an
actor is settled by whoever spawned it (its records fold into the spawner's
log). `spawnActor` inside a `chidori.branch` sub-run is rejected for the same
range-confinement reason as nested branches.

### sendActor

```ts
await chidori.sendActor(pidOrName, "message-name", payload?);
// → { delivered: boolean }   (false once the target has settled)
```

Delivery never blocks. `to = "parent"` addresses the sender's spawner: the
owning actor for a child in a supervision tree, or the spawning run for a
top-level actor.

### receive

```ts
const msg = await chidori.receive("draft");                  // { name, payload, from }
const msg = await chidori.receive(["draft", "cancel"]);      // fan-in
const msg = await chidori.receive("draft", { timeoutMs: 60000 }); // may be { timedOut: true }
```

Blocking, in-place consumption, in delivery order. Inside an actor it drains
the actor's own mailbox; in the spawning run it drains parent-addressed
messages (plus any pre-queued external signals). The difference from
`chidori.signal`: `signal` pauses the whole run — unwinding the VM so an
*external* party can deliver-and-resume later — while `receive` parks the
calling thread and is woken directly by in-process senders. Use `signal` for
deliveries from outside the process, `receive` for actor traffic. A `receive`
with no timeout, no live actors, and an empty mailbox fails fast instead of
blocking forever; the timeout sentinel is the same `{ timedOut: true }` shape
signals use.

### joinActor / stopActor

```ts
const outcome = await chidori.joinActor(pid);
// → { pid, status, output?, error?, pendingPrompt?, restarts }
const partial = await chidori.joinActor(pid, { timeoutMs: 5000 });
// → { pid, status: "running", restarts } when not settled yet — join again later
const outcome = await chidori.stopActor(pid);   // cooperative stop, then join
```

`status` is `"completed"` (with the actor's return value), `"failed"` (restart
budget spent; carries the final error), `"paused"` (parked on something the
runtime can't answer in-process — interactive `input()`, a policy approval, or
the idle cap on a mailbox wait), or `"stopped"`. `stopActor` is cooperative:
honored between iterations, at mailbox waits, and during restart backoff; a
live LLM/tool call finishes first. Both are owner-only: an actor is settled
by whoever spawned it.

### actorStatus / whereis

```ts
await chidori.actorStatus(pid);  // { pid, status, restarts, mailbox, waitingFor? }
await chidori.whereis("researcher");  // { pid } or { pid: null }
```

## Restart strategies

| Strategy | On iteration failure |
|---|---|
| `never` (default) | The failure is the actor's final outcome. |
| `clean` | Re-run the module from scratch: fresh call log, the spawn-time VFS anchor, the original input. |
| `resume` | Replay the accumulated call log with the **crash frontier** (the trailing failed records) stripped: completed work returns from cache, the failing call re-executes live. |

`resume` is the strategy a process-restart model cannot express: the actor
comes back *with its history* and retries from the exact point of failure,
without re-paying (or re-firing) any recorded LLM call, tool call, or message
consumption. Failed records *before* the frontier — errors the agent caught
and handled — are preserved, since their consumption shaped the control flow
that followed. Note that a deterministic in-code `throw` (one not caused by a
live host-call failure) will recur under `resume`; `maxRestarts` bounds the
loop either way. Messages consumed by a failed attempt are redelivered under
`resume` (their consumption is in the replayed log) but lost under `clean`,
matching the from-scratch semantics.

## Supervision trees

Actors spawn actors, forming a supervised hierarchy — a worker pool per
supervisor, a supervisor per pipeline stage, each level with its own restart
policy:

```ts
// supervisor.ts — spawned by the run, supervises its own worker pool.
export async function agent(input: { shards: string[] }) {
  const workers = [];
  for (const shard of input.shards) {
    workers.push(await chidori.spawnActor("worker.ts", { shard }, {
      restart: "resume",     // this supervisor's policy for ITS children
      maxRestarts: 3,
    }));
  }
  const results = [];
  for (const w of workers) {
    const outcome = await chidori.joinActor(w.pid);   // owner-only
    results.push(outcome.output);
  }
  return { results };
}
```

The tree rules:

- **Ownership.** Every actor records who spawned it. Only the owner may
  `joinActor`/`stopActor` it; anyone may `sendActor` to it. `"parent"` from a
  child addresses its owning actor's mailbox (received there with
  `chidori.receive`), not the run.
- **Ranges nest.** A child's reserved sequence range is carved out of its
  owner's range (each level subdivides by 1000: 10^12 for a top-level actor,
  10^9 for its children, 10^6, then 10^3). That containment is what lets a
  whole subtree merge upward join by join while every record still lands
  inside the top-level actor's range at the final confinement check. The
  subdivision bounds tree depth: a fourth-generation actor has no headroom
  left to subdivide and its `spawnActor` is refused with a clear error.
- **Supervisors reap their children.** When an actor settles — completed,
  failed, stopped, or paused — its still-live children are cooperatively
  stopped first, transitively, so children never outlive their supervisor. A
  `clean` restart also reaps the failed attempt's children (its discarded log
  is about to re-run the spawns live) and releases their registered names for
  the retry to re-claim. A `resume` restart keeps children: the replayed
  `spawn_actor` records return their cached pids and the same live children
  answer.
- **Replay absorbs the whole tree at one join.** A grandchild's records carry
  `parent_seq` → its owner's `join_actor` record → the run's `join_actor`
  record, so replaying the run's join absorbs every level in one pass.

## Durability and replay

Every actor primitive is an ordinary durable host call on the calling run's
log — `spawn_actor`, `send_actor`, `receive`, `join_actor`, `stop_actor`,
`actor_status`, `whereis` — so the whole conversation replays from cache:
a replayed parent never re-runs actors, re-delivers messages, or re-waits.

The actor's own records stay inside its reserved sequence range and fold into
the parent's log at the join, stamped with the `join_actor` call's seq as
their parent. On replay, absorbing the join record absorbs the whole actor
subtree — the same mechanism as branch fan-outs — keeping the sequence
counter aligned and the full cross-actor trace in one journal.

If a run crashes *between* a spawn and its join, the actor's in-flight records
were never merged and are discarded — but the recorded `spawn_actor` and
`send_actor` calls are sufficient to re-create it. On resume, the first live
call that addresses the actor (a send, join, stop, or status) re-spawns it
fresh and re-seeds its mailbox from the recorded sends, so unjoined actor work
re-executes rather than being lost (at-least-once semantics for the unjoined
window).

## Semantics worth knowing

- **Concurrency is real but bounded**: each actor is an OS thread with its own
  VM (like a concurrent branch wave). Actors suit tens of concurrent
  LLM-bound processes, not tens of thousands of compute-bound ones.
- **Selective receive** falls out of names: `receive(["a", "b"])` and
  `signalAny` consume the lowest-delivery-seq match and leave everything else
  queued.
- **Idle actors park, not leak**: an actor waiting on an empty mailbox with no
  explicit timeout settles as `paused` after `idleTimeoutMs` (default 5
  minutes), so an orphaned wait cannot hold a thread forever — and a settling
  supervisor reaps its subtree on the way out.
- **Join what you spawn**: records only merge at a join/stop, and only the
  spawner may settle an actor. Ending the parent run with actors unjoined
  discards their (unmerged) work.
- **Hot code reload across restarts**: each supervision-loop iteration re-reads
  the actor's source module, so an edited module + `resume` restart follows the
  same modify-and-resume contract as run resume (divergence detection applies).
