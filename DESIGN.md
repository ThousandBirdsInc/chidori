# Chidori TypeScript Runtime Design

## Overview

Chidori is a Rust agent runtime where first-party agents and tools are written
in TypeScript. Agent code uses normal `async` / `await` control flow, while all
side effects go through an injected `chidori` host object. The host boundary is
where Chidori records call logs, streams prompt progress, enforces policy,
persists checkpoints, pauses for human input, and replays completed work.

The current migration target is TypeScript-only authoring. Legacy Starlark
examples are archived under `examples/legacy-starlark/` for reference, but the
runtime, CLI, server, and tool discovery paths now require `.ts` files.

For the detailed durable VM snapshot architecture, see
[`docs/typescript-vm-snapshot-runtime.md`](./docs/typescript-vm-snapshot-runtime.md).
For the prompt-to-artifact migration checklist, see
[`docs/typescript-migration-audit.md`](./docs/typescript-migration-audit.md).
For running the runtime against the official ECMAScript conformance suite
(the same Test262 corpus Bun and Node measure language parity with), see
[`docs/conformance.md`](./docs/conformance.md).
For what the pure-Rust engine confines (capability injection, resource limits)
and the gaps that remain, see [`docs/sandbox-model.md`](./docs/sandbox-model.md).

## Design Goals

- TypeScript-only agent and tool authoring.
- Embedded execution inside the Rust `chidori` binary, with no Node or Deno
  runtime dependency for agent execution.
- Normal TypeScript `async` / `await` orchestration.
- Deterministic host-call logging and replay for tests, debugging, and recovery.
- Durable pause and resume around explicit host safepoints such as `input()`,
  policy approval, and awaited Chidori host calls.
- SDKs that talk to the runtime over HTTP without native bindings.
- Runtime TypeScript transpilation only; full `tsc` typechecking remains a
  developer workflow.

## Non-Goals

- No automatic `.star` to `.ts` converter in the first migration.
- No Node package ecosystem execution inside agents for v1.
- No arbitrary mid-instruction VM snapshots for CPU-bound loops.
- No serialization of active OS handles, sockets, provider streams, or native
  Rust closures.
- No support for `SharedArrayBuffer`, `Atomics`, WeakRef, finalizers, or worker
  threads in durable v1 runs.
- No visual editor work in the current roadmap.

## Current State

The migration has these production pieces in place:

- `src/runtime/engine.rs` dispatches TypeScript agents, validates `.ts` files,
  sets durable runtime policy, wires replay and pause modes, and persists run
  checkpoints.
- `src/runtime/host_core.rs` owns the language-neutral host-call behavior:
  sequence allocation, replay lookup, policy enforcement, provider/tool/http
  execution, memory, templates, sandbox helpers, call-log recording, events,
  and safepoints.
- `src/runtime/typescript/` owns TypeScript transpilation, active
  `crates/chidori-quickjs` execution, native host bindings, value conversion,
  tool metadata parsing, checking, and snapshot scaffolding. The legacy
  `rquickjs` binding module is retained only for parity tests.
- `src/runtime/snapshot.rs` defines snapshot manifests, ABI/source/policy
  validation, host promise records, pending operation metadata, branch snapshot
  helpers, and store/load behavior.
- `crates/chidori-quickjs-sys` vendors the in-repo QuickJS fork and exposes the
  raw FFI surface required by Chidori.
- `crates/chidori-quickjs` wraps the fork with safe Rust helpers for runtime
  limits, JSON value conversion, bytecode/value snapshot coverage, host promise
  ids, and snapshot envelope validation.
- `src/tools/mod.rs` discovers TypeScript `.ts` tools and ignores `.star` tool
  files.
- `src/server.rs` and `src/main.rs` expose run, check, serve, sessions, replay,
  resume, streaming events, trace, stats, and snapshot metadata commands.
- `sdk/typescript/` and `sdk/python/` expose HTTP clients for run, replay,
  resume, checkpoints, streaming, and snapshot manifest inspection.

Direct live VM continuation is implemented for the current TypeScript runtime
surface. The repository has manifests, runtime policy gates, source
validation, live snapshot blob kind gating, host promise records, safepoint
hooks, branch resume helpers, imported-module restore coverage through the
bundled selected-root scaffold, and direct live resume coverage for nested
suspending TypeScript `callAgent()` paths including grandchild rejection
through parent `try/catch` handlers. Full QuickJS module/runtime-root
serialization beyond that scaffold is future runtime generalization work.

## Authoring Model

Agents are `.ts` files that export `async function agent(input, chidori)`.

```ts
import type { Chidori } from "chidori";

export async function agent(
  input: { document: string },
  chidori: Chidori,
) {
  const summary = await chidori.prompt(
    "Summarize in three bullets:\n" + input.document,
    { type: "progress" },
  );

  const approved = await chidori.input("Proceed with final answer?", {
    type: "approval",
    default: "yes",
  });

  return { summary, approved };
}
```

Tools are `.ts` files with an exported `tool` metadata value and an async `run`
export. Discovery evaluates metadata in a restricted VM context before
registering the tool.

```ts
import type { Chidori, ToolDefinition } from "chidori";

export const tool: ToolDefinition = {
  name: "web_search",
  description: "Search the web for a query.",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string" },
    },
    required: ["query"],
  },
};

export async function run(args: { query: string }, chidori: Chidori) {
  return chidori.http("https://example.com/search", {
    method: "POST",
    body: args,
  });
}
```

## Host API

The injected `chidori` object provides:

- `prompt(text, options?)`
- `input(message, options?)`
- `callAgent(path, input?)`
- `tool(name, args?)`
- `parallel(tasks, options?)`
- `retry(fn, options?)`
- `tryCall(fn)`
- `http(url, options?)`
- `template(pathOrText, vars?, options?)`
- `log(message, fields?)`
- `memory(action, key?, value?, options?)`
- `checkpoint(label?, data?)`

All side-effecting APIs are promise-returning durable host operations. Pure
helpers may return synchronously only when they cannot suspend and do not need a
call-log boundary.

## Execution Flow

1. The CLI or server reads a `.ts` agent file and input JSON.
2. Runtime policy is resolved from environment/config defaults.
3. TypeScript is transpiled to JavaScript without full runtime typechecking.
4. A bounded QuickJS runtime/context is created.
5. Deterministic globals are installed according to policy.
6. The `chidori` host object is installed.
7. The agent module and allowed relative imports are evaluated.
8. The exported `agent(input, chidori)` function is called.
9. Host calls allocate sequence numbers, consult replay data, persist
   safepoints, execute side effects, record results, and resolve promises.
10. The run finishes with JSON output, a pause state, or a structured error.

## Replay And Resume

Replay uses the persisted call log. Given the same source, input, runtime
policy, and recorded host-call results, Chidori can re-run the TypeScript agent
and return cached results instead of repeating external side effects.

Pause/resume currently supports `input()` and policy approval by recording the
pending host operation, appending the human response or policy decision, and
replaying to continue. Snapshot manifests are persisted alongside the call log
and validated for ABI, source hashes, module graph, and policy.

Live VM snapshot resume is intentionally gated until the QuickJS fork can
serialize and restore suspended continuations, pending promise reactions, the
job queue, evaluated module records, and host promise ids.

## Snapshot Files

Persisted runs live under `.chidori/runs/<run_id>/` and may contain:

- `input.json`: original run input.
- `checkpoint.json`: call log.
- `runtime.snapshot`: binary VM or scaffold snapshot blob.
- `runtime.snapshot.json`: manifest with ABI, policy, source hashes, pending
  operation, host promises, and snapshot kind.
- `pending.json`: pending durable host operation, when paused.
- `output.json`: final output, once complete.

The snapshot manifest is safe to expose through SDKs and HTTP responses. Raw VM
snapshot bytes stay server-side.

## Determinism Policy

Durable runs capture policy in the snapshot manifest and reject incompatible
resume attempts.

| Policy | Env var | Values | Default |
| --- | --- | --- | --- |
| Local TS imports | `CHIDORI_TS_IMPORTS` | `none`, `relative`, `project` | `relative` |
| `Date` behavior | `CHIDORI_TS_DATE` | `disabled`, `fixed`, `host` | `fixed` |
| Randomness behavior | `CHIDORI_TS_RANDOM` | `disabled`, `seeded`, `host` | `seeded` |
| Map/Set snapshot support | `CHIDORI_SNAPSHOT_MAPS_SETS` | `reject`, `serialize` | `reject` |

`host` date/random policies are rejected for durable runs because they can break
replay and snapshot compatibility.

## Project Layout

```text
src/
  runtime/
    engine.rs              # agent dispatch, replay, pause, persistence
    host_core.rs           # language-neutral host operation behavior
    snapshot.rs            # manifests, stores, validation, host promises
    typescript/            # TS transpile, VM, bindings, tools, snapshots
    sandbox.rs             # WASM / Python / JS sandbox helpers
  server.rs                # HTTP/session/streaming APIs
  tools/mod.rs             # TypeScript tool discovery
crates/
  chidori-quickjs-sys/     # vendored QuickJS fork and raw FFI
  chidori-quickjs/         # safe Rust wrapper
  test262-runner/          # Test262 conformance runner (bun/node JS parity)
examples/
  agents/                  # TypeScript examples
  tools/                   # TypeScript tool examples
  legacy-starlark/         # archived migration reference
sdk/
  typescript/              # HTTP client and authoring types
  python/                  # HTTP client
```

## Design Principles

- TypeScript is the authoring format; JSON is the host boundary format.
- Side effects are explicit and interceptable.
- Replay remains useful even after live VM snapshots land.
- Runtime policy is part of durable state, not ambient configuration.
- Snapshot bytes are versioned and validated before restore.
- Unsupported snapshot values fail loudly with actionable diagnostics.
- The embedded runtime remains small and owned by the Rust binary.
