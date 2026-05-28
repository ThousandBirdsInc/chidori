# chidori TypeScript SDK

Zero-dependency TypeScript client for a running `chidori serve` instance.
Uses the global `fetch` (Node 18+, browsers). Mirrors the Python SDK.

## Install

```bash
cd sdk/typescript
npm install
npm run build
```

Then import from the package or link it locally:

```ts
import { AgentClient, Checkpoint } from "chidori";
```

Agent and tool authoring types are also exported:

```ts
import type { Chidori, ToolDefinition } from "chidori";
```

## Usage

```ts
import { AgentClient, Checkpoint } from "chidori";

const client = new AgentClient("http://localhost:8080");

// Run an agent
const session = await client.run({ document: "Rust is a systems language." });
console.log(session.output);

// Save and replay a checkpoint — zero LLM calls on replay
const checkpoint = await session.checkpoint();
const replayed = await client.replay(checkpoint);

// Durable TypeScript runs may include snapshot metadata in the checkpoint.
// The manifest is safe to inspect; raw VM snapshot bytes remain server-side.
if (checkpoint.snapshotManifest) {
  console.log(checkpoint.snapshotManifest.pending?.kind);
  console.log(checkpoint.snapshotManifest.abi.engine_fork);
}

const manifest = await client.getSnapshotManifest(session.id);
console.log(manifest.policy.typescript_imports);

// Live streaming: host calls, prompt stream deltas, then `done`
for await (const evt of client.stream({ document: "hi" })) {
  if (evt.type === "call") console.log(evt.record.function);
  if (evt.type === "prompt_delta" && evt.prompt_type === "progress") {
    process.stdout.write(evt.delta);
  }
  if (evt.type === "done") console.log(evt.status, evt.output);
}

// Paused sessions (from input())
if (session.status === "paused") {
  const resumed = await client.resume(session.id, "yes");
  console.log(resumed.output);
}
```

See the top-level `sdk/python/chidori` for the Python equivalent.

## Snapshot-aware checkpoints

`Checkpoint` contains the replay call log plus optional `snapshotManifest`
metadata. The manifest records the runtime ABI, deterministic policy, source
hashes, pending host operation, and snapshot file name. Clients can use it to
display durable-resume state or diagnose why resume is blocked without handling
the raw `runtime.snapshot` VM bytes.

`client.replay(checkpoint)` still uses the call log for deterministic replay.
Durable resume is exposed through `client.resume(sessionId, response)` for
paused sessions. Today it resumes through persisted host-promise metadata and
replay/scaffold recovery; direct live VM continuation from the server-side
snapshot is still gated on the QuickJS serializer.

Use `client.getSnapshotManifest(sessionId)` when a UI needs only snapshot
metadata. The endpoint never returns the binary VM snapshot.
