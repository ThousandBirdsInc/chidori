# Running modes

Chidori agents run three ways: a one-shot CLI, an HTTP server with a session
API, and event-driven HTTP handlers.

## 1. One-shot CLI

```bash
chidori init my-agent --template chat         # scaffold a starter project (or: docs, worker)
chidori demo                                  # pick from runnable examples
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

The server is **deny-by-default**: unless you configure a policy
(`CHIDORI_POLICY*` env vars) or pass `--trusted`, gated effects (network
requests via `fetch`/`node:http`, workspace mutations) are refused — sessions
arrive from callers you may not control. Local `chidori run` is
**ask-by-default**: with nothing configured, gated effects pause for a y/a/N
prompt on your terminal (and fail closed without one); pass `--trusted` for
the permissive allow-all posture when running agents you wrote yourself. See
[`docs/sandbox-model.md`](./sandbox-model.md).

It also binds **loopback only** (`127.0.0.1`) by default. To make it reachable
from the network, pass `--host 0.0.0.0` (or set `CHIDORI_HOST`) — which
requires `CHIDORI_API_KEY` to be set, since an exposed unauthenticated server
would let anyone on the network execute agents. See
[`docs/deployment.md`](./deployment.md).

Exposes:
- `GET  /health` — health check
- `ANY  /*` — any request is passed to `agent(event)` as an event dict
- `POST /sessions` — create a session and run the agent with given input
- `GET  /sessions` — list all sessions
- `GET  /sessions/{id}` — get session result
- `GET  /sessions/{id}/checkpoint` — get the call log and snapshot manifest metadata
- `GET  /sessions/{id}/snapshot` — inspect the durable journal-scaffold manifest metadata (no VM image — resume is call-log replay)
- `POST /sessions/{id}/resume` — answer a paused `input()` call and continue the run
- `POST /sessions/{id}/approve` — approve or deny a policy-gated call that paused the run
- `POST /sessions/{id}/signal` — deliver a signal `{ name, payload?, from? }`: resolves+resumes a run paused-waiting on that name (200); delivers in-memory to a live streaming run, resuming a matching pause in-process (202 `delivered_live`); else enqueues into the durable mailbox (202 `queued`); 409 for a terminal run
- `POST /sessions/{id}/replay` — replay from a session's checkpoint
- `POST /sessions/{id}/cancel` — cancel a running or stored session
- `POST /sessions/stream` — run a session with SSE call and prompt progress events

## 3. Event-driven agents

Any request to a non-session route is folded into an **event dict** and
passed to the agent as its input:

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

The response mapping: an agent output of `{status, body, headers?}` becomes
the HTTP response (status code, JSON body, extra headers); any other output
returns as `200` JSON. Two behaviors to design for:

- **Every request runs the whole agent** — including health probes and
  scanner traffic. Branch on `input.event` early and return a cheap
  `{status: 400, ...}` for non-events before any model call, or the strays
  will cost tokens. (With `CHIDORI_API_KEY` set, unauthenticated requests
  are rejected before the agent runs.)
- **A run that pauses becomes a session.** If the agent reaches a
  `chidori.signal(...)` listen point, an `input()` call, or a policy
  approval gate, the server persists it as a real session and answers
  `202 Accepted` with the session view (`id`, `status`,
  `pending_signal_names`, ...). Deliver / resume / approve it through the
  normal `/sessions/{id}/*` endpoints — a webhook can open a long-lived,
  human-gated run and hand the caller the id to drive it with.

An agent can also make outbound requests while handling an event:

```ts
// agents/webhook.ts
import { run, type JsonObject } from "chidori:agent";

run(async (input: { url: string; payload?: JsonObject }) => {
  // `fetch` is the runtime's captured networking surface — policy-gated,
  // pausable for approval, and recorded for replay.
  const response = await fetch(input.url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input.payload ?? { source: "chidori" }),
  });
  return { status: response.status, body: await response.json() };
});
```

```bash
chidori serve agents/webhook.ts --port 8080 --trusted   # the agent makes a network call via fetch

curl -X POST http://localhost:8080/github \
  -H "Content-Type: application/json" \
  -d '{"action": "opened", "pull_request": {"title": "Add login"}}'
```
