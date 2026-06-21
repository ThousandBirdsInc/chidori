# chidori TypeScript SDK

Zero-dependency TypeScript client for a running `chidori serve` instance.
Uses the global `fetch` (Node 18+, browsers). Mirrors the Python SDK.

> **This package is not the runtime.** It's an optional HTTP client for driving
> the Chidori **runtime** — the `chidori` binary — from a TypeScript app. You
> don't need it to write or run agents (those are plain `.ts` files the runtime
> executes directly). Install the runtime separately, no Rust toolchain needed:
> `curl -fsSL https://raw.githubusercontent.com/ThousandBirdsInc/chidori/main/scripts/install.sh | sh`
> — see the [project README](https://github.com/ThousandBirdsInc/chidori#%EF%B8%8F-quick-start).

## Install

The package is published to npm as
[`@1kbirds/chidori`](https://www.npmjs.com/package/@1kbirds/chidori). The
unscoped `chidori` npm name belongs to an unrelated project, so **always import
the scoped name** — never `npm install chidori`:

```bash
npm install @1kbirds/chidori
```

```ts
import { AgentClient, Checkpoint } from "@1kbirds/chidori";
```

To build from source instead:

```bash
cd sdk/typescript
npm install
npm run build
```

### Authoring agents and tools

Agent and tool files run *inside* the Chidori runtime, not as a normal Node
program. They get their authoring types — and the `chidori`/`run` globals — from
the **virtual** module `chidori:agent`:

```ts
/// <reference types="@1kbirds/chidori/agent-env" />
import type { Chidori, ToolDefinition } from "chidori:agent";
```

There is no installable package behind `chidori:agent`; it is a URL-style scheme
(like `node:fs`) that the runtime strips and injects at execution time, so the
unrelated `chidori` npm package can never be pulled in by mistake. The
`/// <reference …>` line (or a `compilerOptions.types: ["@1kbirds/chidori/agent-env"]`
entry in `tsconfig.json`) gives editors and `tsc` the types while you author;
the runtime itself needs nothing installed.

## Usage

```ts
import { AgentClient, Checkpoint, isSignalQueued } from "@1kbirds/chidori";

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

// Multiplayer signals (from chidori.signal / pollSignal): deliver
// { name, payload?, from? } to a run.
const result = await client.signal(session.id, {
  name: "review",
  payload: { decision: "approve", notes: "LGTM" },
  from: { kind: "human", id: "mara" },
});
if (isSignalQueued(result)) {
  // run wasn't paused-waiting on this name → enqueued in the durable mailbox (202)
  console.log("queued at delivery_seq", result.delivery_seq);
} else {
  // run was paused-waiting on this name → resolved + resumed (200)
  console.log(result.status, result.output);
}
```

See the top-level `sdk/python/chidori` for the Python equivalent.

## Snapshot-aware checkpoints

`Checkpoint` contains the replay call log plus optional `snapshotManifest`
metadata. The manifest records the runtime ABI, deterministic policy, source
hashes, pending host operation, and snapshot file name. Clients can use it to
display durable-resume state or diagnose why resume is blocked without handling
the raw `runtime.snapshot` VM bytes.

`client.replay(checkpoint)` uses the call log for deterministic replay. Durable
resume is exposed through `client.resume(sessionId, response)` for paused
sessions, recovering through persisted host-promise metadata and the replay
journal. Replay **is** the resume mechanism by design — the QuickJS live-VM
snapshot path was removed in #39, not merely deferred, so the manifest carries
journal/scaffold metadata rather than serialized VM bytes.

Use `client.getSnapshotManifest(sessionId)` when a UI needs only snapshot
metadata. The endpoint never returns the binary VM snapshot.

## Tests

The SDK ships a dependency-free test suite (Node's built-in `node:test`
runner) that drives `AgentClient` against a stdlib `node:http` mock server,
covering run/replay/resume/signal, SSE stream parsing, checkpoint
serialization, and error handling:

```bash
npm test   # builds, then runs node --test test/*.test.mjs
```

End-to-end coverage against a real `chidori serve` binary lives in the Python
SDK integration tests (`sdk/python/tests/`).
