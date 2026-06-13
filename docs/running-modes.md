# Running modes

Chidori agents run three ways: a one-shot CLI, an HTTP server with a session
API, and event-driven HTTP handlers.

## 1. One-shot CLI

```bash
chidori demo                                  # pick from runnable examples
chidori run agents/my_agent.ts --input key=value
chidori run agents/my_agent.ts --input '{"complex": "input"}'
chidori check agents/my_agent.ts            # validate without running
chidori tools --dir tools/                   # list available tools
```

## 2. HTTP server (event-driven + session API)

```bash
chidori serve agents/my_agent.ts --port 8080
```

The server is **deny-by-default**: unless you configure a policy
(`CHIDORI_POLICY*` env vars) or pass `--trusted`, gated effects (network
requests via `fetch`/`node:http`, workspace mutations) are refused — sessions
arrive from callers you may not control. Local `chidori run` keeps the
permissive default. See [`docs/sandbox-model.md`](./sandbox-model.md).

Exposes:
- `GET  /health` — health check
- `ANY  /*` — any request is passed to `agent(event)` as an event dict
- `POST /sessions` — create a session and run the agent with given input
- `GET  /sessions` — list all sessions
- `GET  /sessions/{id}` — get session result
- `GET  /sessions/{id}/checkpoint` — get the call log and snapshot manifest metadata
- `GET  /sessions/{id}/snapshot` — inspect the durable journal-scaffold manifest metadata (no VM image — resume is call-log replay)
- `POST /sessions/{id}/resume` — resume a paused `input()` or approval session
- `POST /sessions/{id}/signal` — deliver a signal `{ name, payload?, from? }`: resolves+resumes a run paused-waiting on that name (200); delivers in-memory to a live streaming run, resuming a matching pause in-process (202 `delivered_live`); else enqueues into the durable mailbox (202 `queued`); 409 for a terminal run
- `POST /sessions/{id}/replay` — replay from a session's checkpoint
- `POST /sessions/{id}/cancel` — cancel a running or stored session
- `POST /sessions/stream` — run a session with SSE call and prompt progress events

## 3. Event-driven agents

An agent can handle incoming HTTP events:

```ts
// agents/webhook.ts
import type { Chidori } from "chidori";

export async function agent(
  input: { url: string; payload?: Record<string, unknown> },
  chidori: Chidori,
) {
  // `fetch` is the runtime's captured networking surface — policy-gated,
  // pausable for approval, and recorded for replay.
  const response = await fetch(input.url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input.payload ?? { source: "chidori" }),
  });
  return { status: response.status, body: await response.json() };
}
```

```bash
chidori serve agents/webhook.ts --port 8080 --trusted   # the agent makes a network call via fetch

curl -X POST http://localhost:8080/github \
  -H "Content-Type: application/json" \
  -d '{"action": "opened", "pull_request": {"title": "Add login"}}'
```
