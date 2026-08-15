---
title: "Detached Agents"
description: "Durable, addressable, hibernating agent processes that outlive their spawner; chidori.alarm; the detached-agent HTTP surface."
---

# Detached agents: durable, addressable, hibernating processes

A **detached agent** is a long-lived, named agent process that the runtime
supervises outside any one run or session. It is its own durable run — its own
journal under `.chidori/runs/<run_id>/` — with a registered name that outlives
whoever spawned it, a durable mailbox any party can deliver into (other
agents, later runs, HTTP clients), a runtime-owned restart policy, and a
hibernate/wake lifecycle. `chidori.agents.spawn` starts one; where an
[actor](./actors.md) lives inside its spawning run (its records fold into the
parent journal at a join; ending the run discards unjoined work), a detached
agent keeps existing after the spawning run returns.

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
records only the spawn/send/join host calls — one durable record each,
replayed from cache like everything else. The agent's own effects live in its
own journal, under its own run id and its own persistence handle (including
any configured durable mirror — see [Durable Storage](./durable-storage.md)).
There is no fold-at-join: the two journals are simply separate runs that talk.

**Hibernation holds nothing.** When the agent reaches a
`chidori.signal(name)` / `chidori.alarm(ms)` listen point with an empty
mailbox, the standard pause unwinds the VM; the runtime persists the listen
state (names, pending position, any deadline) into the agent's durable
descriptor and the thread exits. A hibernating agent costs zero threads and
zero memory. A matching delivery — or the alarm or `timeoutMs` deadline,
which **is enforced in-process** for a detached agent — re-enters the module
under resume-by-replay: recorded effects return from cache and execution goes
live at the listen frontier.

**The registry is durable — and it is what survives restarts.** On disk, the
fleet lives in three places, all inside your project's `.chidori/`:

- the **registry descriptor**, `.chidori/runs/agents/<name>.json` — a sibling
  of the run directories, one file per named agent;
- a copy of the descriptor inside the agent's own run directory
  (`agent.json`);
- the agent's **signal inbox**, `<run dir>/signals/inbox.json`.

All of it mirrors to the configured run-store backend
([Durable Storage](./durable-storage.md)). At boot, `chidori serve` re-arms
the whole fleet from the **registry** — merged with the backend's copy, with
the local registry winning where they disagree: agents that were mid-run when
the previous process died are woken (resume-by-replay continues at the
frontier), and hibernating agents' alarm and timeout deadlines are re-armed.
This is what makes a detached agent survive a server restart — or, with a
durable backend, machine replacement.

**Source is materialized wherever the agent wakes.** The descriptor stores
the source path as given at spawn (so replay keys stay stable across hosts),
and resolving it against the local project directory is the fast path. When
that file is not on the waking node — the fleet case, where a wake lands
wherever a lease can be taken rather than where the tree happens to live —
the runtime rebuilds the agent's implementation from its own run store before
failing: the head commit of the run's
[source history](./source-history.md) (entry plus every imported module,
content-addressed full text) is written under
`.chidori/materialized/<run_id>/`, a sibling of `runs/`, at the tree's
recorded *relative* layout so relative imports still resolve. Runs recorded
before source history existed fall back to the entry text in the snapshot's
durable bundle — enough for a single-module agent. Paths from the store are
untrusted input to a path join: anything that would land outside the
materialization root is refused, never written. Materialization is idempotent
(a file already present with the recorded content is left alone), and a
failure keeps the original error, noting that materialization was attempted.

**One driver at a time — and dead drivers don't wedge agents.** Before
executing, the runtime takes a short-lived, regularly renewed lease on the
agent's run and releases it on hibernate/settle. A wake that finds the lease
held waits for it: a live holder keeps renewing (so the waiter stands down to
the genuine driver), while a dead process's lease expires unrenewed and
transfers. The server also periodically re-drives any `running` agent that has
no worker thread, so an agent whose driver died — including one killed before
it ever hibernated — is picked back up instead of sitting `running` forever.
A queued `send` to such an agent also wakes it directly.

**Status tells the truth about liveness.** `status()` /
`GET /agents/detached/{name}` include `live: boolean` — whether a worker
thread in this process is executing the agent right now. `status:
"running", live: false` means the driver died and the server will re-drive
it, not that work is happening.

## API

### agents.spawn

```ts
const svc = await chidori.agents.spawn(source, input?, {
  name?: string,          // registry name; generated when omitted.
                          // Allowed characters: letters, digits, "-", "_", "."
  restart?: "never" | "clean" | "resume",   // default "resume"
  maxRestarts?: number,   // default 3
  backoffMs?: number,     // base restart delay, doubles per attempt; default 0
  model?: string,         // default model for the agent's prompts; defaults to
                          // the spawner's resolved model and travels with the
                          // agent's durable descriptor across wakes and restarts
});
// → handle { name, runId, send(), join(), stop(), status() }
```

These options are deliberately **not** the same as `chidori.actors.spawn`:
`restart` defaults to `"resume"` (a service should come back) rather than
`"never"`; `model` is a top-level option rather than part of an `intercept`;
and there is no `intercept` and no `idleTimeoutMs` — a detached agent is not a
scoped child of its spawner, and hibernating on an empty mailbox is its normal
state, not a leak to cap. See the side-by-side comparison in
[Host API](./host-api.md).

Requires persistence (detached agents *are* durable runs). A live agent
squats on its name; a settled one may be replaced by a fresh spawn — and
**mail follows the name**: whatever the settled predecessor never consumed
is migrated into the replacement's inbox rather than stranding in the dead
run's mailbox. Replay of the parent returns `{name, runId}` from cache
without starting anything — the agent is re-materialized from the registry
by the next live call that addresses it.

### agents.send / receive side

```ts
await chidori.agents.send("inbox-triager", "email", payload);
// → { delivered: boolean }    (false once the agent has settled)
```

Deliveries are durable (they land in the agent's signal inbox on disk) and
write through to a live agent's in-memory mailbox. A hibernating agent is
woken only by a name in its listen set; other messages queue for later listen
points. Inside the agent, messages are consumed with the ordinary listen verbs
(`chidori.signal`, `chidori.receive`, `chidori.pollSignal`).

### agents.join / stop / status / lookup

```ts
await svc.join({ timeoutMs: 30000 });
// → { name, runId, status, output?, error?, restarts, waitingFor?, deadline? }
await svc.stop();       // cooperative: a live LLM call finishes first
await svc.status();     // point-in-time view, never blocks
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

A durable timer, built on the signal machinery: a listen on the reserved
name `__chidori.alarm__` with the delay as its timeout. In a detached agent
the alarm **hibernates** the agent and the runtime's timer wakes it at the
deadline — surviving process restarts, because the deadline rides the durable
descriptor. In a server session the signal-timeout machinery arms it (and
re-arms after a server restart) — see
[Signals](./signals.md#timeoutms) for where deadlines are and are not
enforced. At-least-once: a wake that finds the deadline passed fires
immediately.

## Restart strategies

Detached agents use the same three strategies as [actors](./actors.md),
applied to the agent's own durable journal — but the default is `resume`, not
`never`:

| Strategy | On iteration failure |
|---|---|
| `never` | The failure is the agent's final outcome. |
| `clean` | Re-run from scratch: journal wiped, original input. Unconsumed mailbox entries survive; consumed ones are gone. |
| `resume` (default) | Strip the crash frontier from the journal and re-enter: completed LLM/tool calls replay from cache, the failing call re-executes live. |

## HTTP surface (`chidori serve`)

```
GET  /agents/detached               → registry listing
GET  /agents/detached/{name}        → status view
POST /agents/detached/{name}/send   → { name, payload } — deliver + wake
POST /agents/detached/{name}/stop   → cooperative stop
```

The full HTTP endpoint reference lives in
[Running Modes](./running-modes.md). `send` is how external systems talk to a
hibernating fleet: a webhook handler POSTs to the agent's mailbox and the
server wakes it, runs it to its next hibernate point, and goes back to
holding nothing.

**Fleet-only serving.** `chidori serve` without an agent file hosts *just*
the fleet: it re-arms every agent in the current directory's registry and
exposes the `/agents/detached/*` endpoints; session requests must then name
an agent per request (the `agent` field) or are rejected with guidance. Use
it when the only thing to run is a fleet a previous `chidori run` spawned:

```bash
chidori serve --port 8080          # no FILE: fleet-only
```

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
  approvals inside a detached agent have no interactive counterpart; the
  agent settles as `paused` with the prompt in its status.
- **At-least-once around crashes.** A crash between an effect and its
  recording re-executes that effect on wake (the same window every replay
  system has); recorded effects are exactly-once by replay.
