# app-agent TypeScript SDK

Zero-dependency TypeScript client for a running `app-agent serve` instance.
Uses the global `fetch` (Node 18+, browsers). Mirrors the Python SDK.

## Install

```bash
cd sdk/typescript
npm install
npm run build
```

Then import from the package or link it locally:

```ts
import { AgentClient, Checkpoint } from "app-agent";
```

## Usage

```ts
import { AgentClient, Checkpoint } from "app-agent";

const client = new AgentClient("http://localhost:8080");

// Run an agent
const session = await client.run({ document: "Rust is a systems language." });
console.log(session.output);

// Save and replay a checkpoint — zero LLM calls on replay
const checkpoint = await session.checkpoint();
const replayed = await client.replay(checkpoint);

// Live streaming: yields one event per host function call, then `done`
for await (const evt of client.stream({ document: "hi" })) {
  if (evt.type === "call") console.log(evt.record.function);
  if (evt.type === "done") console.log(evt.status, evt.output);
}

// Paused sessions (from input())
if (session.status === "paused") {
  const resumed = await client.resume(session.id, "yes");
  console.log(resumed.output);
}
```

See the top-level `sdk/python/app_agent` for the Python equivalent.
