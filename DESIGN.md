# Chidori TypeScript Runtime Design

## Overview

Chidori is a Rust agent runtime where first-party agents and tools are written
in TypeScript. Agent code uses normal `async` / `await` control flow, while all
side effects go through an injected `chidori` host object (plus captured base
APIs such as `fetch`). The host boundary is where Chidori records call logs,
streams prompt progress, enforces policy, persists checkpoints, pauses for human
input, and replays completed work.

TypeScript is the only supported authoring format. Legacy Starlark examples are
archived under `examples/legacy-starlark/` for reference, but the runtime, CLI,
server, and tool discovery paths require `.ts` files.

Agents run on **`crates/chidori-js`**, an in-tree, pure-Rust JavaScript engine
(zero `unsafe`, oxc parser). It is the *only* JavaScript engine in the tree —
for agents, tools, sub-agents, and the Test262 conformance harness alike. The
earlier vendored QuickJS fork, the `rquickjs` parity path, and the WASM/Python
exec sandboxes were removed; there is no fallback engine, so JavaScript-language
correctness is measured continuously against Test262 (see below).

Related design docs:

- [`docs/pure-rust-js-engine-plan.md`](./docs/pure-rust-js-engine-plan.md) —
  the engine's design (historical migration plan; the engine it describes is
  now the shipping runtime).
- [`docs/conformance.md`](./docs/conformance.md) — how the runtime is measured
  against the official ECMAScript Test262 corpus and the CI baseline gate.
- [`docs/sandbox-model.md`](./docs/sandbox-model.md) — what the engine confines
  (capability injection, resource limits) and the gaps that remain.
- [`docs/value-checkpoints.md`](./docs/value-checkpoints.md),
  [`docs/context-management.md`](./docs/context-management.md),
  [`docs/signals.md`](./docs/signals.md),
  [`docs/branching-execution.md`](./docs/branching-execution.md), and
  [`docs/captured-effects-vfs-crypto-timers.md`](./docs/captured-effects-vfs-crypto-timers.md)
  — the host-API features layered on the runtime.

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

- No automatic `.star` to `.ts` converter.
- No Node package ecosystem execution inside agents for v1.
- No polyglot snippet execution: the `execJs` / `execPython` / `execWasm`
  sandboxes were removed (the JS stubs remain but the host backend rejects the
  effect).
- No `Intl`, `Temporal`, `SharedArrayBuffer`, `Atomics`, `WeakRef`,
  finalizers, decorators, or worker threads in durable v1 runs (skipped
  honestly by the conformance runner).
- No VM-image snapshots of suspended continuations: durability is the
  deterministic-replay journal, not a frozen heap. Resume re-runs the agent and
  serves recorded host results from the journal.
- No OS-level isolation: the sandbox is capability confinement plus resource
  limits, not seccomp/namespaces.
- No visual editor work.

## Current State

The runtime has these production pieces in place:

- `crates/chidori-js` is the pure-Rust JavaScript engine: oxc-based parser,
  bytecode compiler and interpreter, reference-counting GC with a cycle
  collector (`gc.rs`), a custom ReDoS-bounded regex backtracker (`regexp.rs`),
  a deterministic-replay journal (`journal.rs` / `replay.rs`), and the host
  function seam (`host.rs`).
- `crates/test262-runner` runs the pinned Test262 corpus against the engine and
  gates CI on a committed per-test baseline.
- `src/runtime/engine.rs` dispatches TypeScript agents, validates `.ts` files,
  resolves durable runtime policy, wires replay and pause modes, and persists
  run checkpoints.
- `src/runtime/rust_engine.rs` adapts `chidori-js` to the runtime via the
  `SnapshotCapableJsEngine` trait, exposing host effects as global async
  functions and round-tripping a `{bundle, effects, journal}` blob for
  snapshot/restore.
- `src/runtime/host_core.rs` owns the language-neutral host-call behavior:
  sequence allocation, replay lookup, policy enforcement, provider/tool/network
  execution, memory, templates, call-log recording, events, and safepoints.
- `src/runtime/context.rs`, `call_log.rs`, `capability.rs`, `cost.rs`,
  `crypto.rs`, `vfs.rs`, `memory.rs`, `template.rs`, `prompt_cache.rs`,
  `secret_env.rs`, `workspace.rs`, `host_branch.rs`, and `otel.rs` provide the
  surrounding host machinery (runtime context, captured effects, capability
  ledger, cost accounting, virtual filesystem, prompt caching, secrets,
  workspace, branching, OTEL).
- `src/runtime/typescript/` owns TypeScript transpilation (`transpile.rs`, oxc),
  native host bindings (`bindings.rs`), the module graph and resolver
  (`module_graph.rs`, `resolver.rs`), tool metadata evaluation (`tools.rs`),
  `check.rs`, and runtime prelude/builtins (`builtins.rs`, `helpers.rs`).
- `src/runtime/snapshot.rs` defines snapshot/journal manifests, runtime policy,
  ABI/source/policy validation, capability ledgers, and store/load behavior.
- `src/tools/mod.rs` discovers TypeScript `.ts` tools and ignores `.star` files.
- `src/server.rs` and `src/main.rs` expose run, check, serve, sessions, replay,
  resume, streaming events, trace, stats, branches, and snapshot metadata
  commands. `src/providers/`, `src/mcp/`, `src/acp.rs`, `src/policy.rs`,
  `src/scheduler.rs`, and `src/storage.rs` provide providers, MCP/ACP surfaces,
  policy, scheduling, and persistence.
- `sdk/typescript/` and `sdk/python/` expose zero-dependency HTTP clients for
  run, replay, resume, checkpoints, streaming, and snapshot manifest inspection.

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
  // Networking uses the standard `fetch` API, which the runtime replaces with a
  // captured, policy-gated, replayable version.
  const res = await fetch("https://example.com/search", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args),
  });
  return res.json();
}
```

## Host API

The injected `chidori` object provides:

- `prompt(text, options?)`
- `input(message, options?)`
- `signal(name, options?)` / `pollSignal(name)` / `signalAny(names, options?)`
  — multiplayer named signals (see `docs/signals.md`).
- `callAgent(path, input?)`
- `tool(name, args?)`
- `parallel(tasks, options?)`
- `branch(strategies, options?)` — in-agent execution branching
  (`docs/branching-execution.md`).
- `retry(fn, options?)`
- `tryCall(fn)`
- `step(name, fn)` — durable value checkpoints; memoize pure compute into the
  call log so replay/resume never re-pays it (`docs/value-checkpoints.md`).
- `context(...)` / `Context.compact(...)` — prompt context builder and window
  compaction (`docs/context-management.md`).
- `template(pathOrText, vars?, options?)`
- `log(message, fields?)`
- `memory(action, key?, value?, options?)`
- `checkpoint(label?, data?)`
- `workspace(action, ...)` — policy-gated workspace access.

All side-effecting APIs are promise-returning durable host operations. Pure
helpers may return synchronously only when they cannot suspend and do not need a
call-log boundary.

Networking is **not** on the `chidori` object: agents use the standard `fetch`
(plus `Headers`/`Request`/`Response`) and `node:http`/`node:https` client APIs.
The runtime replaces those base networking APIs with captured implementations
that route through one policy-gated, replayable host op, so every request — even
one issued inside a dependency — is gated and recorded automatically.
Filesystem (`node:fs`), crypto (`node:crypto`, Web Crypto), and timers are
similarly captured against a snapshot-resident virtual filesystem and a virtual
clock (`docs/captured-effects-vfs-crypto-timers.md`).

## Execution Flow

1. The CLI or server reads a `.ts` agent file and input JSON.
2. Runtime policy is resolved from environment/config defaults.
3. TypeScript is transpiled to JavaScript (oxc, type-stripping only — no
   downleveling and no full typecheck).
4. A bounded `chidori-js` VM is created (memory cap, opcode budget, deadline).
5. Deterministic globals and captured base APIs are installed per policy.
6. The `chidori` host object is installed.
7. The agent module and allowed imports are evaluated through the module graph.
8. The exported `agent(input, chidori)` function is called.
9. Host calls allocate sequence numbers, consult journal/replay data, persist
   safepoints, execute side effects, record results, and resolve promises.
10. The run finishes with JSON output, a pause state, or a structured error.

## Replay And Resume

Durability is the deterministic-replay journal. Each host effect is recorded in
order against a code bundle referenced by content hash. Given the same source,
input, runtime policy, and recorded host-call results, Chidori re-runs the
TypeScript agent and serves cached results from the journal instead of repeating
external side effects.

Pause/resume (`input()`, policy approval, and other suspending host calls) is
implemented by recording the pending host operation, appending the human
response or policy decision, and replaying to continue. There is no VM-image
snapshot of a suspended continuation; resume re-executes deterministically and
the journal supplies every prior effect result. `chidori.step(name, fn)`
(value checkpoints) memoizes pure compute into the journal so long histories do
not re-pay that compute on resume; host effects are already journal-served, so
only un-wrapped pure JS between effects is re-executed.

## Snapshot Files

Persisted runs live under `.chidori/runs/<run_id>/` and may contain:

- `input.json`: original run input.
- `checkpoint.json`: call log / journal.
- `runtime.snapshot`: a self-describing `{bundle, effects, journal}` blob (the
  code bundle, exposed host effect names, and the replay journal) — not a VM
  heap image.
- `runtime.snapshot.json`: manifest with ABI, policy, source hashes, pending
  operation, capability ledger, and snapshot kind.
- `pending.json`: pending durable host operation, when paused.
- `output.json`: final output, once complete.

The snapshot manifest is safe to expose through SDKs and HTTP responses.

## Determinism Policy

Durable runs capture policy in the snapshot manifest and reject incompatible
resume attempts.

| Policy | Env var | Values | Default |
| --- | --- | --- | --- |
| Local TS imports | `CHIDORI_TS_IMPORTS` | `none`, `relative`, `project`, `node` | `node` |
| `Date` behavior | `CHIDORI_TS_DATE` | `disabled`, `fixed`, `host` | `fixed` |
| Randomness behavior | `CHIDORI_TS_RANDOM` | `disabled`, `seeded`, `host` | `seeded` |
| Filesystem | `CHIDORI_TS_FS` | `disabled`, `captured`, `host` | `captured` |
| Crypto | `CHIDORI_TS_CRYPTO` | `disabled`, `seeded`, `captured`, `host` | `captured` |
| Timers | `CHIDORI_TS_TIMERS` | `disabled`, `virtual`, `host` | `virtual` |
| Map/Set snapshot support | `CHIDORI_SNAPSHOT_MAPS_SETS` | `reject`, `serialize` | `reject` |

`host` variants are rejected for durable runs because they can break replay and
snapshot compatibility. The `node` import policy is the durable default so the
snapshot-resident virtual filesystem (`node:fs`) is reachable.

## Project Layout

```text
src/
  runtime/
    engine.rs              # agent dispatch, replay, pause, persistence
    rust_engine.rs         # chidori-js adapter (SnapshotCapableJsEngine)
    host_core.rs           # language-neutral host operation behavior
    context.rs             # runtime context shared across host calls
    snapshot.rs            # manifests, policy, journal validation, stores
    call_log.rs            # call-log / journal records
    capability.rs          # capability ledger
    vfs.rs crypto.rs       # captured filesystem and crypto effects
    memory.rs template.rs  # memory store and templates
    prompt_cache.rs        # opt-in local prompt cache
    workspace.rs           # policy-gated workspace access
    host_branch.rs         # in-agent branching
    cost.rs otel.rs        # cost accounting and OTEL spans
    typescript/            # TS transpile, bindings, module graph, tools, check
  server.rs                # HTTP/session/streaming APIs
  providers/               # Anthropic, OpenAI, LiteLLM-compatible, static
  tools/mod.rs             # TypeScript tool discovery
  mcp/ acp.rs              # MCP and ACP protocol surfaces
  policy.rs scheduler.rs storage.rs   # policy, scheduling, persistence
crates/
  chidori-js/              # the pure-Rust JavaScript engine (the only engine)
  test262-runner/          # Test262 conformance runner + baseline gate
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
- One JavaScript engine, no fallback — so language correctness is a continuous,
  CI-gated conformance bar, not a migration milestone.
- Durability is the deterministic-replay journal; runtime policy is part of
  durable state, not ambient configuration.
- Journal blobs and manifests are versioned and validated before restore.
- Unsupported values fail loudly with actionable diagnostics.
- The embedded runtime remains small and owned by the Rust binary.
