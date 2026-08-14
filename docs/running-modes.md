---
title: "Running Modes"
description: "One-shot CLI runs, the HTTP session server and its endpoint reference, event-driven handlers and the serve status-code contract, and approval postures."
---

# Running modes

Chidori agents run three ways: a one-shot CLI, an HTTP server with a session
API, and event-driven HTTP handlers.

## 1. One-shot CLI

```bash
chidori init my-agent --template chat         # scaffold a starter project (or: docs, worker)
chidori run agents/my_agent.ts --input key=value
chidori run agents/my_agent.ts --input '{"complex": "input"}'
chidori chat --system "You are concise."     # interactive multi-turn chat REPL
chidori chat agents/chat.ts                   # chat through a conversational agent file
chidori check agents/my_agent.ts            # validate without running
```

`chidori init [dir] --template docs|chat|worker` scaffolds a starter project —
an agent and README. Omit `--template` to choose interactively. The `docs`
template chats with a bundled copy of the Chidori docs; the `chat` template is
a conversational agent; the `worker` template is an autonomous tool-using loop
whose tools are defined inline with `defineTool`.

`chidori run` asks for approval at the terminal before powerful effects and
fails closed without a terminal — postures and `--trusted` are in the
[CLI reference](./cli.md#approval-postures).

`chidori chat` is a built-in conversational REPL backed by
[`chidori.conversation()`](./core-concepts.md#conversational-agents). With no
agent file it chats with the model directly; pass a conversational agent file
(one accepting `{ messages, system?, model? }` and returning
`{ transcript }` or `{ history }`, like the `chat` init template) to chat through
it. Each turn is a durable host call and streams its reply token-by-token; the
prior turns replay for free, so only your newest message reaches the provider.
Flags: `--system` and `--model`. Type `exit`/`quit` or Ctrl-D to end.

Every chat session is an ordinary durable run: the session id is announced at
start, each turn journals into `.chidori/runs/<session_id>` (next to the agent
file, or the cwd for the built-in agent), and the run's `input.json` always
holds the full dialogue state. `chidori chat [FILE] --resume <session_id>`
replays the journal — reprinting the transcript for $0, completing a turn that
a crash interrupted mid-generation — and continues the conversation in place.
`chidori trace <session_id>` inspects a session like any other run.

## 2. HTTP server (event-driven + session API)

```bash
chidori serve agents/my_agent.ts --port 8080
```

Bare `chidori serve` runs the **`untrusted`** policy profile: gated effects
(network via `fetch`/`node:http`, tool calls, workspace writes, app data)
are refused
— sessions arrive from callers you may not control. `--trusted` opts into
the permissive allow-all posture, and a per-session `policy_profile`
overlay can only tighten the server's policy, never loosen it. Full posture
table: [CLI reference](./cli.md#approval-postures) and
[Sandbox Model](./sandbox-model.md).

The server also binds **loopback only** (`127.0.0.1`) by default. To make it
reachable from the network, pass `--host 0.0.0.0` (or set `CHIDORI_HOST`) —
which requires `CHIDORI_API_KEY` to be set, since an exposed unauthenticated
server would let anyone on the network execute agents
(`CHIDORI_ALLOW_UNAUTHENTICATED=1` explicitly opts out). See
[Deployment](./deployment.md).

A **session** is a run addressed over HTTP: a session id *is* a run id, and
the session's journal lives in `.chidori/runs/<session_id>/`.

Exposes:
- `GET  /health` — health check
- `ANY  /*` — any other request is folded into `{ event: … }` and run as the
  agent's input (see [Event-driven agents](#3-event-driven-agents))
- `POST /sessions` — create a session and run the agent with given input
- `GET  /sessions` — list all sessions
- `GET  /sessions/{id}` — get session result
- `GET  /sessions/{id}/checkpoint` — get the session's journal records and snapshot manifest metadata
- `GET  /sessions/{id}/snapshot` — inspect the snapshot manifest metadata (no VM image — resume is journal replay)
- `GET  /sessions/{id}/holdings` — what the run is holding right now: the pending host call it is parked on, queued signals, unsettled actors, detached agents (with registry state), open branches, armed compensations
- `POST /sessions/{id}/resume` — answer a paused `input()` call and continue the run
- `POST /sessions/{id}/approve` — approve or deny a policy-gated call that paused the run
- `POST /sessions/{id}/signal` — deliver a signal `{ name, payload?, from? }`: resolves+resumes a run paused-waiting on that name (200); delivers in-memory to a live streaming run, resuming a matching pause in-process (202 `delivered_live`); else enqueues into the durable mailbox (202 `queued`); 409 for a terminal run
- `POST /sessions/{id}/replay` — replay a session from its journal
- `POST /sessions/{id}/cancel` — cancel a running or stored session
- `POST /sessions/stream` — run a session with SSE call and prompt progress events
- `GET  /sessions/{id}/stream` — re-attach to a session's SSE events: replays everything already emitted (so a dropped client catches up), then follows a still-running streaming session live until it settles; for a settled session, replays the logged call records and closes with a `done` event carrying the final state
- `GET  /agents/detached` — list registered [detached agents](./detached-agents.md) and their registry state
- `POST /agents/detached/{name}/send` — deliver a signal into a [detached agent](./detached-agents.md)'s durable mailbox
- `GET  /recipes` — list scheduled recipes (from the [application manifest](#the-application-manifest-chidoriappyml))
- `POST /recipes/{name}/run` — run a scheduled recipe manually, outside its cron loop

### The application manifest (`chidori.app.yml`)

A server usually hosts more than one thing: a detached-agent fleet, cron
schedules, webhook endpoints. The application manifest gives that composition
a source-controlled definition instead of runtime state — `chidori serve`
boots the whole application from it:

```yaml
name: support-desk
agents:
  - name: triage
    agent: agents/triage.ts     # entry, relative to the manifest
    keep_alive: true            # spawn at boot; re-arm forever after
    input: { queue: "inbound" }
    restart: resume             # never | clean | resume (default)
  - name: standup-scribe
    agent: agents/scribe.ts
    schedule: "0 9 * * 1-5"     # cron → runs as a scheduled session
routes:
  - path: /webhooks/github
    agent: triage               # deliver the request body into this agent's
    signal: github-event        # mailbox as this named signal
```

The server picks up `chidori.app.yml` (or `.yaml`/`.json`) next to the agent
file automatically; `--app <path>` or `CHIDORI_APP_MANIFEST` names one
explicitly. Semantics:

- **`keep_alive: true`** — at boot, if the name is not already live in the
  [detached-agent registry](./detached-agents.md), the agent is spawned; live
  incarnations are re-armed as usual, settled ones are replaced by a fresh
  spawn (mailbox migration included). The manifest is idempotent across
  restarts.
- **`schedule`** — the entry becomes a recipe: same cron loop, listed under
  `GET /recipes`, runnable manually via `POST /recipes/{name}/run` (both in
  the endpoint list above).
- **`routes`** — each path is served as a real route (behind the same bearer
  auth as everything else); a request's JSON body is delivered to the named
  agent's durable mailbox as the named signal, waking a hibernating agent.

A manifest error — a missing agent file, an invalid cron, a route path
without a leading `/` — stops the server before it binds.

## 3. Event-driven agents

Any request to a non-session route is folded into an **event object** and
passed to your `run(async (input) => …)` handler as its input:

```jsonc
{
  "event": {
    "method": "POST",            // HTTP method
    "path": "/alerts/pagerduty", // request path
    "headers": { "content-type": "application/json", ... },
    "query": { "key": "value" }, // query-string parameters
    "body": { ... }              // parsed JSON, or the raw string if not JSON
  }
}
```

There is no built-in routing: the whole agent runs for every request, so
branch on `input.event` early and return a cheap 404 for paths you don't
handle:

```ts
// agents/pr_triage.ts
import { chidori, run } from "chidori:agent";

run(async (input: { event: { method: string; path: string; body?: unknown } }) => {
  const { event } = input;
  if (event.method !== "POST" || event.path !== "/hooks/pr") {
    return { status: 404, body: { error: "not found" } }; // cheap 404 before any model call
  }
  const triage = await chidori.prompt(
    `Triage this pull request event:\n${JSON.stringify(event.body)}`,
    { type: "final" },
  );
  return { status: 200, body: { triage } };
});
```

```bash
chidori serve agents/pr_triage.ts --port 8080

curl -X POST http://localhost:8080/hooks/pr \
  -H "Content-Type: application/json" \
  -d '{"action": "opened", "pull_request": {"title": "Add login"}}'
```

The status-code contract:

- An agent output carrying **both** `status` and `body` becomes the HTTP
  response — that status code, JSON body, and any extra `headers`. Any
  other output (including `status` without `body`) returns as `200` JSON.
- **A run that pauses becomes a session, answered `202`.** If the agent
  reaches a `chidori.signal(...)` listen point, an `input()` call, or a
  policy approval gate, the server persists it as a real session and
  answers `202 Accepted` with the session view (`id`, `status`,
  `pending_signal_names`, ...). Deliver / resume / approve it through the
  normal `/sessions/{id}/*` endpoints — a webhook can open a long-lived,
  human-gated run and hand the caller the id to drive it with.
- **An agent throw returns `500`** with `{ "error": … }`.
- **Probe noise is short-circuited with an empty `404`**: requests for
  `/favicon.ico`, `/robots.txt`, `/apple-touch-icon*`, and anything under
  `/.well-known/` never invoke the agent. If your agent genuinely serves
  those paths, set `CHIDORI_SERVE_ALL_PATHS=1` to route every path to your
  handler again. (Beyond that noise, **every request runs the whole
  agent** — including health probes and scanner traffic; that's why the
  early cheap 404 above matters, or the strays will cost tokens. With
  `CHIDORI_API_KEY` set, unauthenticated requests are rejected before the
  agent runs.)

An agent can also make *outbound* requests while handling an event: `fetch`
is the runtime's captured networking surface — policy-gated, pausable for
approval, and journaled for replay.
[`examples/agents/webhook.ts`](../examples/agents/webhook.ts) is the
outbound-fetch example — note it is a `chidori run` demo taking
`--input url=…`, not an inbound handler.
