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
chidori tools --dir tools/                   # list available tools
```

`chidori init [dir] --template docs|chat|worker` scaffolds a starter project —
an agent and README, plus a `tools/` directory for the `worker` template. Omit
`--template` to choose interactively. The `docs` template chats with a bundled
copy of the Chidori docs; the `chat` template is a conversational agent; the
`worker` template is an autonomous tool-using loop.

`chidori chat` is a built-in conversational REPL backed by
[`chidori.conversation()`](./core-concepts.md#conversational-agents). With no
agent file it chats with the model directly; pass a conversational agent file
(one accepting `{ messages, system?, model?, tools? }` and returning
`{ transcript }` or `{ history }`, like the `chat` init template) to chat through
it. Each turn is a durable host call and streams its reply token-by-token; the
prior turns replay for free, so only your newest message reaches the provider.
Flags: `--system`, `--model`, and `--tools <dir>` (discovered tools are offered
to the model on every turn). Type `exit`/`quit` or Ctrl-D to end.

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
- `POST /sessions/{id}/resume` — answer a paused `input()` call and continue the run
- `POST /sessions/{id}/approve` — approve or deny a policy-gated call that paused the run
- `POST /sessions/{id}/signal` — deliver a signal `{ name, payload?, from? }`: resolves+resumes a run paused-waiting on that name (200); delivers in-memory to a live streaming run, resuming a matching pause in-process (202 `delivered_live`); else enqueues into the durable mailbox (202 `queued`); 409 for a terminal run
- `POST /sessions/{id}/replay` — replay from a session's checkpoint
- `POST /sessions/{id}/cancel` — cancel a running or stored session
- `POST /sessions/stream` — run a session with SSE call and prompt progress events

## 3. Event-driven agents

An agent can handle incoming HTTP events:

```ts
// agents/webhook.ts
import type { Chidori } from "chidori:agent";

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
