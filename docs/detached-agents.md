# Detached agents: durable, addressable, hibernating processes

`chidori.agents.spawn` starts an agent module as a **detached durable
process** — the durable-object-shaped sibling of an in-run actor
(`docs/actors.md`). Where an actor lives inside its spawning run (its records
fold into the parent journal at a join; ending the run discards unjoined
work), a detached agent is **its own durable run**: its own journal under
`.chidori/runs/<run_id>/`, a registered name that outlives the spawner, a
durable mailbox any party can deliver into (agents, later runs, HTTP
clients), a runtime-owned restart policy, and a hibernate/wake lifecycle.

```ts
import { chidori, run } from "chidori:agent";

run(async () => {
  // A long-lived service: triages every email sent to it, forever.
  const svc = await chidori.agents.spawn("services/inbox-triager.ts", {}, {
    name: "inbox-triager",
    restart: "resume",     // on crash: replay completed work, retry the failure
    maxRestarts: 3,
  });

  await svc.send("email", { from: "a@example.com", subject: "hi" });
  const status = await svc.status();   // { status: "hibernating", waitingFor: ["email"], ... }
  return status;
});
```

```ts
// services/inbox-triager.ts — hibernates between emails, holding no thread
// and no VM; an alarm compacts state daily even with no traffic.
import { chidori, run } from "chidori:agent";

run(async () => {
  const triaged = [];
  while (true) {
    const msg = await chidori.signal("email");     // hibernate point
    const result = await chidori.prompt(`Triage: ${JSON.stringify(msg.payload)}`);
    triaged.push({ email: msg.payload, result });
  }
});
```

## The model

**A detached agent is a durable run with a name.** The spawning run's journal
records only the `spawn_agent` / `send_agent` / `join_agent` host calls —
one durable record each, replayed from cache like everything else. The
agent's own effects live in its own journal, under its own run id, its own
deterministic policy seed, and its own persistence handle (including any
configured durable mirror — `docs/durable-storage.md`). There is no
fold-at-join and no sequence-range carve-out: the two journals are simply
separate runs that talk.

**Hibernation holds nothing.** When the agent reaches a
`chidori.signal(name)` / `chidori.alarm(ms)` listen point with an empty
mailbox, the standard pause unwinds the VM; the supervisor persists the
listen state (names, pending seq, deadline) into the agent's registry entry
and the thread exits. A hibernating agent costs zero threads and zero
memory. A matching delivery — or the alarm deadline — re-enters the module
under resume-by-replay: recorded effects return from cache and execution
goes live at the listen frontier.

**The registry is durable.** Every lifecycle transition persists the agent's
descriptor to `<run_base>/agents/<name>.json` (and the durable mirror's
registry). At boot, `chidori serve` re-arms the whole fleet from the
registry: agents that were mid-run when the previous process died are woken
(resume-by-replay continues at the frontier), and hibernating agents'
alarm deadlines are re-armed. This is what makes a detached agent survive a
server restart — or, with a durable mirror, machine replacement.

**Leases prevent double-drivers.** Before executing, the supervisor takes
the agent run's lease (`lease.json`, TTL 5 minutes, renewed per iteration)
and releases it on hibernate/settle. A second process sharing the same store
stands down; a dead node's expired lease transfers on the next wake.

## API

### agents.spawn

```ts
const svc = await chidori.agents.spawn(source, input?, {
  name?: string,          // registry name; generated when omitted
  restart?: "never" | "clean" | "resume",   // default "resume"
  maxRestarts?: number,   // default 3
  backoffMs?: number,     // base restart delay, doubles per attempt; default 0
});
// → handle { name, runId, send(), join(), stop(), status() }
```

Requires persistence (detached agents *are* durable runs). A live agent
squats on its name; a settled one may be replaced by a fresh spawn. Replay of
the parent returns `{name, runId}` from cache without starting anything —
the agent is re-materialized from the registry by the next live call that
addresses it.

### agents.send / receive side

```ts
await chidori.agents.send("inbox-triager", "email", payload);
// → { delivered: boolean }    (false once the agent has settled)
```

Deliveries are durable (the agent's `signals/inbox.json`) and write-through
to a live agent's in-memory mailbox. A hibernating agent is woken only by a
name in its listen set; other messages queue for later listen points. Inside
the agent, messages are consumed with the ordinary listen verbs
(`chidori.signal`, `chidori.receive`, `pollSignal`).

### agents.join / stop / status / lookup

```ts
await svc.join({ timeoutMs: 30000 });
// → { name, runId, status, output?, error?, restarts, waitingFor?, deadline? }
await svc.stop();       // cooperative: a live LLM call finishes first
await svc.status();     // point-in-time snapshot, never blocks
await chidori.agents.lookup("inbox-triager");   // handle or null
```

`join` waits for a *settled* status (`completed` / `failed` / `stopped` /
`paused`). A hibernating service does not settle — that is its job — so a
join without `timeoutMs` on a deadline-less hibernating agent fails fast
with guidance rather than hanging.

### chidori.alarm

```ts
const fired = await chidori.alarm(24 * 60 * 60 * 1000);   // { timedOut: true }
```

A durable timer, lowered onto the signal machinery: a listen on the reserved
name `__chidori.alarm__` with the delay as its timeout. In a detached agent
the alarm **hibernates** the agent and the supervisor's timer wakes it at
the deadline — surviving process restarts, because the deadline rides the
registry descriptor. In a server session the existing signal-timeout
machinery arms it (and re-arms after a server restart). At-least-once: a
wake that finds the deadline passed fires immediately.

## Restart strategies

Same table as actors, applied to the agent's own durable journal:

| Strategy | On iteration failure |
|---|---|
| `never` | The failure is the agent's final outcome. |
| `clean` | Re-run from scratch: journal wiped, original input. Unconsumed mailbox entries survive; consumed ones are gone. |
| `resume` (default) | Strip the crash frontier from the journal and re-enter: completed LLM/tool calls replay from cache, the failing call re-executes live. |

## HTTP surface (`chidori serve`)

```
GET  /agents/detached               → registry listing
GET  /agents/detached/{name}        → status snapshot
POST /agents/detached/{name}/send   → { name, payload } — deliver + wake
POST /agents/detached/{name}/stop   → cooperative stop
```

`send` is how external systems talk to a hibernating fleet: a webhook
handler POSTs to the agent's mailbox and the server wakes it, runs it to its
next hibernate point, and goes back to holding nothing.

## Semantics worth knowing

- **Actors vs detached agents.** Actors are structured concurrency *inside*
  one run — supervised, joined, folded into the parent's journal, gone when
  the run ends. Detached agents are *durable processes beside* runs. Use
  actors for a fan-out the run will collect; use a detached agent for
  anything that should outlive the run that started it.
- **Process lifetime.** `chidori run` exits when the entry run settles;
  live detached agents die with the process but lose nothing — their
  journals, mailboxes, and listen state are durable, and the next process
  (`chidori serve`, or any run that sends to them) resumes them. A server is
  the natural home for a fleet.
- **Interactive pauses settle as `paused`.** `chidori.input()` and policy
  approvals inside a detached agent have no interactive counterpart yet; the
  agent settles as `paused` with the prompt in its status.
- **At-least-once around crashes.** A crash between an effect and its
  recording re-executes that effect on wake (the same window every replay
  system has); recorded effects are exactly-once by replay.
