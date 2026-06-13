# TypeScript VM Snapshot Runtime Design

## Purpose

This document defines the architecture for moving Chidori from legacy Starlark
agents to TypeScript agents while preserving the core product promise:
durable, inspectable, resumable agent execution.

The legacy runtime got durability by replaying deterministic Starlark from the
top with a cached call log. The TypeScript runtime keeps call-log replay and
adds a stronger target capability: suspend TypeScript execution, persist the
runtime state, restart Chidori, restore the runtime, and continue the same
async execution.

This requires an owned JavaScript engine fork. Stock `rquickjs` and Boa are not
sufficient because they do not expose stable APIs for serializing heap state,
async continuation frames, pending promises, and the job queue.

## Goals

- TypeScript-only agent and tool authoring.
- Embedded execution inside the Rust Chidori binary, with no Node or Deno
  runtime dependency.
- Durable snapshots that survive process restart.
- Normal TypeScript `async` / `await` authoring.
- Snapshot and resume at awaited Chidori host calls and explicit checkpoints.
- Preserve Chidori features: host-call tracing, replay, sessions, streaming
  prompt output, approval/input pauses, tools, sub-agents, policy, MCP, OTEL,
  and sandbox helpers.
- Keep runtime TypeScript transpile-only. Full `tsc` typechecking remains a
  developer workflow, not a runtime requirement.

## Non-Goals

- No automatic `.star` to `.ts` converter in the first migration.
- No Node package ecosystem execution in-agent for v1.
- No arbitrary mid-instruction VM snapshots for CPU-bound loops.
- No serialization of active OS handles, sockets, provider streams, or native
  Rust closures.
- No support for `SharedArrayBuffer`, `Atomics`, WeakRef, finalizers, or worker
  threads in v1.

## Current State

Chidori currently has these relevant pieces:

- `src/runtime/engine.rs`: owns TypeScript agent dispatch, replay setup, pause
  handling, and persistence.
- `src/runtime/typescript/`: binds Rust host functions into QuickJS and
  implements side effects such as `prompt`, `tool`, `http`, `input`,
  `parallel`, `callAgent`, sandbox execution, file operations, memory, retry,
  and compaction.
- `src/runtime/context.rs`: owns run id, config, call log, replay log, sequence
  numbers, input/policy pause state, event streaming, persistence, and OTEL.
- `src/tools/mod.rs`: discovers `.ts` tools and extracts TypeScript tool
  metadata.
- `src/server.rs` and `src/main.rs`: expose sessions, checkpoint/replay,
  resume, streaming events, CLI commands, and serving.
- `sandbox-js`: runs JavaScript snippets inside WASM for `exec_js`; it is not
  the agent runtime.
- `crates/chidori-quickjs-sys` vendors the in-repo QuickJS fork and exposes
  low-level value, bytecode, and host-promise APIs.
- `crates/chidori-quickjs` wraps the fork with safe Rust helpers for current
  value/bytecode snapshot coverage plus native callback/context-opaque helpers
  for Rust-backed `globalThis.chidori` methods. The TypeScript snapshot wrapper
  forwards those helpers and now installs `RuntimeContext`/`host_core`-backed
  native `chidori.log`, `chidori.checkpoint`, `chidori.memory`, and
  `chidori.template`, plus `chidori.execJs`, `chidori.execPython`, and
  `chidori.execWasm`. The captured networking host op (reached via `fetch`/
  `node:http`, not a public `chidori.http`) covers policy denial and native
  `chidori.input` covers pause-mode pending input. Provider-backed native
  `chidori.prompt` covers the plain-text prompt path and non-suspending prompt
  tool loops, and registry-backed native `chidori.tool`/`chidori.callAgent`
  cover non-suspending TypeScript tools and child agents. The pure JavaScript
  `tryCall`, `retry`, and `parallel` helpers can also be installed directly on
  the snapshot `chidori` object. `TypeScriptVmRuntime` now uses this
  `chidori-quickjs` path for ordinary context-backed TypeScript execution; the
  old `rquickjs` binding module is retained only for parity tests.

The language-independent host behavior now lives in `src/runtime/host_core.rs`
and is bound into the TypeScript runtime. The remaining project work is the
QuickJS fork/runtime snapshot work required to cover production module records
and runtime roots beyond the selected-root scaffold.

## Target Authoring Model

Agents are `.ts` files exporting `async function agent(input, chidori)`.

```ts
import type { Chidori } from "chidori";

export async function agent(
  input: { document: string },
  chidori: Chidori,
) {
  const summary = await chidori.prompt(
    `Summarize this document:\n${input.document}`,
    { type: "progress" },
  );

  const approved = await chidori.input("Proceed with final answer?", {
    context: { summary },
  });

  return { summary, approved };
}
```

Tools are `.ts` files with explicit metadata and an async `run` export.

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
  const res = await fetch("https://example.com/search", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args),
  });
  return res.json();
}
```

## Public Runtime API

The `chidori` object exposed to TypeScript provides:

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
- `execJs(source, options?)`
- `execPython(source, options?)`
- `execWasm(source, options?)`

All side-effecting APIs are promise-returning durable host operations. Pure
helpers may return synchronously only if they cannot suspend and do not need a
call-log boundary.

## Snapshot Semantics

Snapshots are guaranteed at these safepoints:

- before an awaited Chidori host operation performs an irreversible side effect
- after a host operation records its result but before the JS promise is
  resolved
- when `await chidori.checkpoint()` is called
- when `input()` or policy approval suspends for human action

Snapshot files are persisted in the run directory:

- `input.json`: original run input
- `checkpoint.json`: call log
- `runtime.snapshot`: binary VM snapshot blob
- `runtime.snapshot.json`: snapshot manifest
- `pending.json`: pending durable host operation, if any
- `output.json`: final output, once complete

A runtime snapshot must include:

- JS heap
- object identity graph and cycles
- closures and lexical environments
- async frames and continuations
- pending promises and promise reactions
- microtask/job queue
- evaluated module records
- stable host promise ids
- runtime ABI metadata
- module source hashes

A snapshot must not include:

- Rust closures
- file descriptors
- HTTP connections
- provider streaming handles
- arbitrary native class pointers

Those are represented as durable host operations with explicit ids and
serialized metadata.

### Snapshot Compatibility

Every snapshot manifest records a `SnapshotAbi`, runtime policy, entry source
fingerprint, and module source fingerprints. Resume must validate these before
loading `runtime.snapshot` into the VM.

ABI compatibility is exact-match for v1:

- `typescript_runtime` must match the compiled Chidori TypeScript runtime ABI.
- `quickjs_snapshot` must match the in-repo QuickJS snapshot serializer ABI.
- `engine_fork` must match the expected fork identifier.

A mismatch fails resume before VM restore with a diagnostic shaped like
`runtime snapshot ABI mismatch: snapshot has ... runtime expects ...`. This is
intentional; snapshot bytes are not self-describing enough to safely load across
serializer versions.

Source compatibility is also exact-match for v1. The entry file and all
recorded modules use stable SHA-256 fingerprints over the source text that was
checked/transpiled for the run. A mismatch fails before VM restore with a
diagnostic shaped like `runtime snapshot source mismatch: snapshot has ...
runtime has ...`. Users must restart the run after editing agent or module
source.

Runtime policy compatibility is exact-match as well. `CHIDORI_TS_IMPORTS`,
`CHIDORI_TS_DATE`, `CHIDORI_TS_RANDOM`, and `CHIDORI_SNAPSHOT_MAPS_SETS` are
part of the resume contract because they affect deterministic execution and
snapshot shape.

## QuickJS Fork Work

The QuickJS fork lives inside this repository. It is part of the Chidori
workspace, reviewed with the runtime changes, and versioned atomically with the
snapshot ABI. Do not depend on an adjacent private fork or a remote git
dependency for the core engine.

Bun should be used as an implementation reference for a Rust-hosted
JavaScript/TypeScript runtime. `oven-sh/bun` merged PR
[`#30412`](https://github.com/oven-sh/bun/pull/30412), "Rewrite Bun in Rust",
into `main` on May 14, 2026, so the relevant reference is the current Rust port,
not only Bun's older Zig implementation. Study it for module loading,
TypeScript transpilation, Node-compatible API surface decisions, test
organization, and JavaScriptCore integration patterns.

Chidori is still not adopting Bun as the embedded runtime for v1 because our
central requirement is durable VM snapshot/restore with host promise ids. Bun's
Rust port is a reference for runtime structure, porting strategy, and test
migration patterns, not a replacement for the in-repo snapshot-capable QuickJS
fork.

Bun Rust-port reference decisions:

| Decision | Chidori action |
| --- | --- |
| Preserve the existing runtime architecture during a language-porting pass. | Adapt. Preserve host-call replay semantics while moving them into a language-neutral host core. |
| Use Rust ownership and compiler checks to reduce memory-management bugs around runtime infrastructure. | Copy. The Chidori wrapper owns snapshot buffers, host promise ids, and restore lifetimes in Rust. |
| Keep the runtime dependency surface small. | Copy. Do not add Node, Deno, or a large JS framework as a runtime dependency. |
| Validate the port against the pre-existing test suite while adding targeted new tests. | Copy. Preserve pre-migration host behavior with regression coverage while adding TypeScript snapshot tests incrementally. |
| Use JavaScriptCore as the embedded engine. | Reject for v1. Chidori needs direct VM snapshot patches, so the engine remains the in-repo QuickJS fork. |
| Implement broad Node compatibility as a product goal. | Reject for v1. Agent code gets a small durable `chidori` host API, not a general Node runtime. |
| Avoid async Rust inside the JS runtime core. | Adapt. Chidori still uses Rust async at host boundaries, but VM suspension is modeled through host promise ids and explicit job draining. |

Repository layout:

- `crates/chidori-quickjs-sys`: C fork source plus raw FFI bindings
- `crates/chidori-quickjs`: safe Rust wrapper used by Chidori runtime
- `crates/chidori-quickjs-sys/quickjs`: vendored QuickJS source tree with
  Chidori snapshot patches
- `crates/chidori-quickjs/tests`: Rust integration tests for snapshot/restore
  behavior
- `crates/chidori-quickjs-sys/tests`: C/FFI-level tests for fork primitives

The root `Cargo.toml` should become a workspace manifest that includes these
crates and the main `chidori` package. The main runtime should depend on
`chidori-quickjs` via a local path dependency.

Remaining production gap: the current code has the Rust crate layout, manifest
format, runtime policy gates, value/context snapshot coverage, live VM blob
kind gating, safepoint hooks, branch resume helpers, and snapshot-side generic
`chidori.<method>()` host-promise pause/restore coverage for `input()` and
`prompt()` with active operation id and host-call records included in the
default TypeScript snapshot roots. The active operation id is cleared after
restored host promise resolution or rejection, and queued host method promises
survive restore so a resumed agent can pause again on a later host call with a
distinct operation id reported by `run_jobs_until_blocked()` or the
resolve/reject-and-run helpers, including TypeScript restore-from-snapshot plus
resolve/reject helpers that return the restored context and next run state.
The fork-level `CHIDORI_JS_SnapshotContext` / `CHIDORI_JS_RestoreContext`
entry points now round-trip selected Chidori TypeScript roots and the
microtask/job queue, including a suspended Chidori host call restored and
completed by host promise id.
The fork-level `CHIDORI_JS_SnapshotRuntime` / `CHIDORI_JS_RestoreRuntime`
entry points now provide a versioned runtime envelope and restore a fresh
QuickJS runtime that composes with the context payload.
Production TypeScript execution now uses the snapshot-capable
`crates/chidori-quickjs` wrapper and native `chidori` host object. Remaining
resume work is focused on covering live module records/runtime roots beyond the
selected-root scaffold.
The native callback bridge is available at the TypeScript snapshot wrapper
layer, and `chidori.log`, `chidori.checkpoint`, `chidori.memory`,
`chidori.template`, `chidori.execJs`, `chidori.execPython`, and
`chidori.execWasm` record through `RuntimeContext` and the shared host core.
Captured-networking (`fetch`/`node:http`) policy denial and `chidori.input`
pause setup are covered on the native snapshot-host path.
Plain-text `chidori.prompt` and non-suspending prompt tool loops are covered
through the provider-backed host core; non-suspending `chidori.tool` and
`chidori.callAgent` are covered through the registry-backed native path. Nested
suspending tool/sub-agent paths remain future convergence work.
The snapshot runtime also installs the same pure JavaScript `tryCall`, `retry`,
and `parallel` helpers used by the current production binding.

Required C API surface:

```c
int CHIDORI_JS_SnapshotRuntime(JSRuntime *rt, CHIDORI_JSSnapshotWriter *writer);
JSRuntime *CHIDORI_JS_RestoreRuntime(CHIDORI_JSSnapshotReader *reader);

int CHIDORI_JS_SnapshotContext(JSContext *ctx, CHIDORI_JSSnapshotWriter *writer);
JSContext *CHIDORI_JS_RestoreContext(JSRuntime *rt, CHIDORI_JSSnapshotReader *reader);

JSValue CHIDORI_JS_NewHostPromise(JSContext *ctx, uint64_t host_operation_id);
int CHIDORI_JS_ResolveHostPromise(JSContext *ctx, uint64_t host_operation_id, JSValue value);
int CHIDORI_JS_RejectHostPromise(JSContext *ctx, uint64_t host_operation_id, JSValue reason);

int CHIDORI_JS_RunJobsUntilBlocked(JSRuntime *rt, JSContext **ctx);
int CHIDORI_JS_SetSnapshotUnsupportedHook(JSRuntime *rt, CHIDORI_JSUnsupportedHook *hook);
```

Required serialized VM state:

- atoms and strings
- shapes and property tables
- object graph with stable ids
- arrays and sparse arrays
- typed arrays and ArrayBuffer contents
- maps and sets, if enabled
- symbols and bigint values
- function bytecode
- closures and lexical variables
- module records and export bindings
- promise state
- promise fulfill/reject reaction lists
- async function continuation frames
- generator frames if generators remain enabled
- microtask/job queue
- pending host promise table

Unsupported values must fail snapshot creation with a structured error:

- native functions without registered restore metadata
- opaque external classes without snapshot hooks
- weak references and finalizers
- worker handles
- shared memory
- host objects that directly embed non-serializable Rust state

Required deterministic controls:

- memory limit
- interrupt/budget callback
- runtime policy for `Date`
- runtime policy for randomness
- runtime policy for local TypeScript imports
- disabled timers unless implemented as durable host operations
- disabled filesystem/network globals

### Runtime Policy Configuration

These policies are configurable, but every value must be captured in the
snapshot manifest. A resume must fail if the saved policy differs from the
current runtime policy.

Configuration source precedence:

1. Per-run server/CLI option.
2. Project config file.
3. Environment variable.
4. Built-in default.

Policy keys and defaults:

| Policy | Config key | Env var | Values | Default |
|---|---|---|---|---|
| Local TS imports | `typescript.imports` | `CHIDORI_TS_IMPORTS` | `none`, `relative`, `project` | `relative` |
| `Date` behavior | `runtime.date` | `CHIDORI_TS_DATE` | `disabled`, `fixed`, `host` | `fixed` |
| Randomness behavior | `runtime.random` | `CHIDORI_TS_RANDOM` | `disabled`, `seeded`, `host` | `seeded` |
| Map/Set snapshot support | `snapshot.maps_sets` | `CHIDORI_SNAPSHOT_MAPS_SETS` | `reject`, `serialize` | `reject` |

Policy semantics:

- `typescript.imports = none`: reject all local imports. Only the entry module
  is evaluated.
- `typescript.imports = relative`: allow relative imports such as `./lib.ts`
  and `../shared.ts` within the project root. Reject bare package imports and
  paths outside the root.
- `typescript.imports = project`: allow relative imports and configured
  project import aliases. Still reject network, npm package, and filesystem
  escape imports.
- `runtime.date = disabled`: `Date`, `Date.now`, and time constructors throw.
- `runtime.date = fixed`: `Date` returns the run's deterministic timestamp
  from the snapshot policy seed.
- `runtime.date = host`: use wall-clock host time. This is allowed only for
  non-durable development runs; durable snapshot runs must reject it.
- `runtime.random = disabled`: `Math.random` throws.
- `runtime.random = seeded`: `Math.random` uses a deterministic per-run seed
  captured in the snapshot manifest.
- `runtime.random = host`: use host randomness. This is allowed only for
  non-durable development runs; durable snapshot runs must reject it.
- `snapshot.maps_sets = reject`: snapshots fail clearly if live `Map` or `Set`
  values are reachable.
- `snapshot.maps_sets = serialize`: the engine fork serializes `Map` and `Set`
  entries in insertion order and restores identity/cycles.

Required safe Rust wrapper API:

```rust
pub struct SnapshotRuntime;
pub struct SnapshotContext;
pub struct RuntimeSnapshot(Vec<u8>);
pub struct HostPromiseId(u64);

impl SnapshotRuntime {
    pub fn new(limits: RuntimeLimits) -> Result<Self>;
    pub fn restore(snapshot: &[u8]) -> Result<Self>;
    pub fn snapshot(&mut self) -> Result<RuntimeSnapshot>;
    pub fn run_jobs_until_blocked(&mut self) -> Result<RunState>;
}

impl SnapshotContext {
    pub fn eval_module(&mut self, name: &str, source: &str) -> Result<()>;
    pub fn call_export_json(&mut self, export: &str, args: serde_json::Value) -> Result<RunState>;
    pub fn new_host_promise(&mut self, id: HostPromiseId) -> Result<JsValue>;
    pub fn resolve_host_promise(&mut self, id: HostPromiseId, value: serde_json::Value) -> Result<()>;
    pub fn reject_host_promise(&mut self, id: HostPromiseId, error: String) -> Result<()>;
}

pub enum RunState {
    Completed(serde_json::Value),
    BlockedOnHostOperation(PendingHostOperation),
}
```

Fork validation tests:

- heap primitive round trip
- nested object graph round trip
- cyclic object graph round trip
- closure local preservation
- module export preservation
- async function suspended at await and resumed
- nested async function suspended at await and resumed
- pending promise reactions preserved
- microtask queue order preserved
- host promise restored and resolved by id
- unsupported native object fails snapshot with path/type detail
- ABI mismatch fails restore
- source hash mismatch fails Chidori resume before VM restore

## Chidori Runtime Architecture

### Language-Neutral Host Core

Create a host core that no longer depends on Starlark value types. It should
accept and return `serde_json::Value` at the boundary.

Core responsibilities:

- allocate call sequence numbers
- check replay cache
- enforce policy
- execute providers/tools/http/sandbox/memory/template/file operations
- record call logs
- emit streaming events
- persist checkpoints and runtime snapshots
- return typed host-operation outcomes

The TypeScript binding layer should be thin. It only converts JS values to
JSON, calls the host core, then returns/resolves JS promises.

### TypeScript Runtime

Create `src/runtime/typescript/` with:

- `mod.rs`: public runtime module
- `engine.rs`: `TypeScriptVmRuntime`
- `transpile.rs`: transpile-only TS to JS
- `bindings.rs`: install `chidori` host object
- `snapshot.rs`: store/load/validate VM snapshots
- `tools.rs`: `.ts` tool discovery and metadata loading through
  `crates/chidori-quickjs`
- `values.rs`: JSON <-> JS conversion

The engine flow:

1. Read `.ts` source.
2. Compute source fingerprint.
3. Transpile TypeScript to JavaScript.
4. Create snapshot-capable JS runtime/context.
5. Install deterministic global environment.
6. Install `chidori` host API.
7. Evaluate module.
8. Call exported `agent(input, chidori)`.
9. Drive jobs until completion or durable suspension.
10. Persist output or snapshot state.

### Durable Host Operation Flow

For every awaited side-effecting host call:

1. JS calls a host binding.
2. Binding allocates `HostOperationId` and call `seq`.
3. Binding creates a host-backed JS promise.
4. Runtime persists a VM snapshot with pending operation metadata.
5. Rust executes or schedules the side effect.
6. Result or error is recorded in `checkpoint.json`.
7. Runtime persists another snapshot before promise resolution.
8. Runtime resolves or rejects the host promise.
9. Runtime drains jobs until completion or next suspension.

For operations that require external input, such as `input()` and policy
approval, the flow stops after step 4 and returns a paused session.

### Direct Live VM Resume Flow

This is the direct live VM continuation flow used by the current server resume
path, with durable replay retained as a fallback.

1. Load session row from storage.
2. Load `runtime.snapshot.json`.
3. Validate runtime ABI.
4. Validate source hashes.
5. Load `runtime.snapshot`.
6. Restore JS runtime/context.
7. Load pending operation.
8. Apply external response or stored operation result.
9. Resolve/reject host promise by `HostOperationId`.
10. Drain jobs until completion or next suspension.
11. Persist new snapshot or output.

### Parallel Execution

`chidori.parallel(tasks)` runs task closures concurrently while preserving
durability.

Required behavior:

- each branch gets its own JS runtime/context
- branch contexts restore from the parent snapshot at the parallel call
- branch host calls use branch-local sequence ranges until merge
- branch snapshots persist independently while running
- parent merges branch results and call logs in branch order
- first error fails the parent `parallel` promise
- resumed branches continue from their own snapshots

For v1, it is acceptable to cap parallel branch concurrency with
`CHIDORI_PARALLEL_CONCURRENCY`.

Branch snapshot layout:

```text
.chidori/runs/<run_id>/
  checkpoint.json
  runtime.snapshot
  runtime.snapshot.json
  branches/
    <parallel_op_id>/
      manifest.json
      branch-000/
        checkpoint.json
        runtime.snapshot
        runtime.snapshot.json
      branch-001/
        checkpoint.json
        runtime.snapshot
        runtime.snapshot.json
```

`manifest.json` records the parent snapshot id, branch count, requested
concurrency, branch operation ids, branch sequence ranges, and merge order. A
branch `runtime.snapshot.json` records the same ABI, policy, and source
fingerprints as the parent plus `parent_run_id`, `parallel_op_id`,
`branch_index`, and `branch_operation_id`. Parent call-log sequence allocation
is deterministic: the parent reserves a contiguous range for the parallel
operation, each branch writes branch-local sequence numbers inside its assigned
range, and merge appends branch records in branch index order.

## Server And API Changes

Existing session APIs remain, but their semantics expand:

- `GET /sessions/{id}/checkpoint` returns call log plus snapshot manifest
  metadata when present.
- Current `POST /sessions/{id}/resume` first tries to restore
  `runtime.snapshot`, resolve the persisted host promise, and continue the live
  VM directly. Durable host-promise replay remains available as an explicit
  recovery fallback.
- `POST /sessions/{id}/replay` remains available for deterministic debugging
  and recovery fallback.
- `POST /sessions/stream` continues to emit:
  - `call`
  - `prompt_start`
  - `prompt_delta`
  - `prompt_end`
  - `done`

New optional endpoint:

- `GET /sessions/{id}/snapshot` returns snapshot manifest metadata, not the raw
  binary snapshot by default.

Raw snapshots should not be exposed over HTTP unless an authenticated admin
mode is added.

## Tool Discovery

Replace `.star` discovery with `.ts` discovery.

Tool loading:

1. Scan configured tool directories for `.ts`.
2. Transpile and evaluate each module in a restricted metadata context.
3. Read exported `tool`.
4. Validate name, description, and JSON schema.
5. Register the tool with source fingerprint and source path.

Tool execution:

1. Invoke exported `run(args, chidori)`.
2. Use the same durable host runtime as agents.
3. Record tool calls in parent call log.
4. Snapshot if the tool suspends.

The production loader evaluates `tool` inside a restricted metadata VM context
instead of parsing source text. That keeps discovery aligned with real
TypeScript module semantics while avoiding access to the `chidori` host object,
network helpers, timers, `Date`, and `Math.random`.

## Migration Strategy

The target is TypeScript-only. The migration has been staged so the runtime can
keep replay behavior working while live VM snapshot support lands incrementally.

Staged sequence:

1. Add snapshot manifest/store and fork boundary types.
2. Extract language-neutral host core from Starlark bindings.
3. Introduce TypeScript runtime behind internal feature flag.
4. Implement QuickJS fork and Rust wrapper.
5. Run TS examples in parallel with archived `.star` examples.
6. Convert first-party examples and docs.
7. Switch CLI/server defaults from `.star` to `.ts`.
8. Remove Starlark runtime and dependency.

No automatic migration converter is required.

## Risks

- QuickJS continuation serialization is deep engine work and will be the
  longest pole.
- Native host objects can silently break snapshot safety unless unsupported
  values fail loudly.
- Async job queue ordering must be stable or resumed agents may diverge.
- Snapshot format must be versioned from day one.
- TypeScript transpilation can create helper code that affects source hash and
  stack traces; source maps should be planned early.
- Tool metadata evaluation must not accidentally expose runtime side effects.
- `parallel()` with independent snapshots can create complicated recovery
  states.

## Progress Checklist

### Phase 0: Alignment And Guardrails

- [x] Confirm TS-only migration scope.
- [x] Confirm no `.star` auto-converter for v1.
- [x] Confirm snapshot guarantee is only at async safepoints.
- [x] Confirm unsupported JS features for v1.
- [x] Document source hash and ABI mismatch behavior.
- [x] QuickJS fork lives inside this repository.

### Phase 1: Snapshot Data Model

- [x] Add snapshot manifest types.
- [x] Add pending host operation types.
- [x] Add snapshot store read/write.
- [x] Persist `runtime.snapshot`.
- [x] Persist `runtime.snapshot.json`.
- [x] Persist `pending.json`.
- [x] Include source fingerprints.
- [x] Include runtime ABI metadata.
- [x] Include runtime policy metadata.
- [x] Include snapshot blob kind so initial-state scaffolds cannot be treated
      as live VM continuation snapshots.
- [x] Include call log length and run id.
- [x] Add unit tests for manifest round trips.
- [x] Add unit tests for ABI mismatch.
- [x] Add unit tests for source mismatch.
- [x] Add unit tests for runtime policy mismatch.
- [x] Add unit tests for durable policy safety.

### Phase 2: QuickJS Fork Bootstrap

- [x] Add `crates/chidori-quickjs-sys`.
- [x] Add `crates/chidori-quickjs`.
- [x] Vendor QuickJS source under `crates/chidori-quickjs-sys/quickjs`.
- [x] Add local path dependency from `chidori` to `chidori-quickjs`.
- [x] Convert root `Cargo.toml` to include a workspace member list.
- [x] Review `oven-sh/bun#30412` and the merged Rust port for runtime structure
      and testing patterns.
- [x] Document which Bun Rust-port patterns are copied, adapted, or rejected.
- [x] Add build for macOS arm64.
- [x] Add build for macOS x64.
- [x] Add build for Linux x64.
- [x] Add CI job for fork build.
- [x] Add runtime memory limits.
- [x] Add interrupt budget support.
- [x] Add deterministic global configuration.
- [x] Expose raw snapshot/restore C APIs.
- [x] Route Rust runtime-level snapshot/restore APIs through the in-repo
      QuickJS fork hooks, currently failing loudly at the unsupported
      serializer boundary.
- [x] Define Rust-backed snapshot reader/writer callback ABI for fork
      snapshot bytes.
- [x] Add versioned runtime snapshot byte envelope before passing fork payloads
      to restore.
- [x] Split the runtime snapshot envelope into runtime and context fork
      payload sections.
- [x] Reject runtime snapshot envelopes with empty runtime or context payloads
      before invoking fork restore hooks.
- [x] Reuse the same restorable runtime snapshot envelope validation before
      persisting or loading live VM blobs.
- [x] Exercise raw fork snapshot reader/writer callbacks from the C hook
      boundary while full VM serialization remains unsupported.
- [x] Exercise context-level raw fork snapshot reader/writer callbacks from
      the C hook boundary.
- [x] Define and exercise the raw unsupported-value callback ABI so fork
      snapshot failures can report path, type, and message details.
- [x] Surface unsupported-value callback details through the safe runtime and
      restored-context snapshot wrappers.
- [x] Add live VM snapshot store helper that stamps live snapshot kind and
      rejects invalid runtime snapshot envelopes before writing.
- [x] Reject live VM snapshot envelopes with missing runtime or context fork
      payloads before marking or loading them as resumable live continuations.
- [x] Add TypeScript runtime live-VM save entry point that reaches the QuickJS
      snapshot boundary before writing to the store.
- [x] Add explicit restored-context guard for runtime-level host promise
      resolve/reject until live VM restore returns a context.
- [x] Attempt context restoration during runtime snapshot restore and store the
      restored context for future runtime-level host promise resolution.
- [x] Keep restored-context fork snapshot bytes separate from runtime fork
      snapshot bytes in the versioned runtime snapshot envelope.
- [x] Expose host promise C APIs.
- [x] Expose run-jobs-until-blocked C API.
- [x] Expose native callback/context-opaque C APIs and wrapper helpers.
- [x] Wire native callback helpers through `TypeScriptSnapshotContext` and
      cover a TypeScript agent calling a Rust-backed `chidori.log`.
- [x] Install `RuntimeContext`/`host_core`-backed native `chidori.log`,
      `chidori.checkpoint`, `chidori.memory`, and `chidori.template` on the
      TypeScript snapshot context.
- [x] Install `RuntimeContext`/`host_core`-backed native `chidori.execJs`,
      `chidori.execPython`, and `chidori.execWasm` on the TypeScript snapshot
      context.
- [x] Install captured-networking (`fetch`/`node:http`) policy denial and
      `chidori.input` pause paths on the TypeScript snapshot context.
- [x] Install provider-backed native plain-text `chidori.prompt` on the
      TypeScript snapshot context.
- [x] Execute provider-requested `chidori.prompt` tool loops through the native
      snapshot host path for non-suspending registered TypeScript tools.
- [x] Install registry-backed native `chidori.tool` and `chidori.callAgent` for
      non-suspending TypeScript tools and child agents on the TypeScript
      snapshot context.
- [x] Install pure JavaScript `tryCall`, `retry`, and `parallel` helpers on the
      TypeScript snapshot context.

### Phase 3: QuickJS Snapshot Serialization

Current implementation note: `crates/chidori-quickjs` now exposes a
value-level QuickJS object writer/reader wrapper around `JS_WriteObject` and
`JS_ReadObject`, with tests for primitives, nested objects, identity, cycles,
arrays, typed arrays, ArrayBuffer contents, symbols, BigInt, compiled global
script bytecode, compiled ES module bytecode execution, sparse array holes,
custom enumerable array properties, Map values, Set values, and detached
bytecode-backed closure function objects with captured local values. Settled
promises round trip fulfilled/rejected state and result values; pending
host-backed promises round trip their host-operation id so they can be
resolved or rejected after restore, including attached host-promise reaction
handlers and downstream promise resolving capabilities. Ordinary pending
promises now round trip pending state and can resume when a resolving function
is also reachable in the snapshot graph. The pending QuickJS job queue now
round trips `queueMicrotask`, promise reaction, and thenable-resolution jobs in
FIFO order. Suspended async functions at awaited host-backed promises now
round trip their active frame state, bytecode PC offset, stack slots, outer
promise resolution, and nested async caller chains for the covered v1 cases.
Queued `queueMicrotask(fn)` callback jobs and already-queued promise reaction
jobs can be snapshotted and restored in FIFO order.
Closure function snapshots preserve shared detached lexical environment cells
across multiple restored closures. A first context snapshot envelope now
serializes a caller-provided set of global roots as one object graph plus the
pending microtask queue, which preserves identity between named roots and
lets restored host-backed promises rebuild the host-promise registry. The
runtime entrypoint now wraps that context payload in a versioned runtime
envelope and restores a fresh QuickJS runtime. This deliberately covers
Chidori's selected root graph rather than arbitrary QuickJS heap roots such as
live module records, runtime-global host binding state, active stack lexical
environments, or ordinary pending promises outside the selected root graph.
Restored compiled module bytecode is also validated through its namespace exports.
Evaluated module namespace objects can now be snapshotted with current export
bindings; full live module record graph serialization is deferred future
generalization work.
Ordinary object property tables now preserve property-name atoms and
configurable/writable/enumerable descriptor flags; hidden VM shape identity is
rebuilt on restore rather than reused.

- [x] Serialize atoms.
- [x] Serialize strings.
- [x] Serialize shapes.
- [x] Serialize object graph.
- [x] Preserve object identity.
- [x] Preserve cycles.
- [x] Serialize arrays.
- [x] Serialize typed arrays.
- [x] Serialize ArrayBuffer contents.
- [x] Serialize maps and sets or reject them explicitly.
- [x] Serialize symbols.
- [x] Serialize bigint.
- [x] Serialize function bytecode.
- [x] Serialize detached closure function objects.
- [x] Serialize shared detached closure lexical environment cells.
- [x] Serialize evaluated module namespace export bindings.
- [x] Serialize selected global roots in one context snapshot graph.
- [x] Serialize settled promise state.
- [x] Serialize ordinary pending promise state.
- [x] Serialize pending host-backed promise ids.
- [x] Serialize pending host-backed promise reaction handlers.
- [x] Serialize promise reaction resolving functions.
- [x] Serialize async continuation frames at awaited host promises.
- [x] Serialize `queueMicrotask` callback jobs.
- [x] Serialize queued promise reaction jobs.
- [x] Serialize QuickJS pending job queue.
- [x] Serialize pending host promise table.
- [x] Reject unsupported native functions.
- [x] Reject unsupported external classes.
- [x] Reject WeakRef/finalizer use.
- [x] Reject shared memory.

### Phase 4: Fork Validation

- [x] Round trip primitive values.
- [x] Round trip nested objects.
- [x] Round trip cyclic objects.
- [x] Round trip arrays.
- [x] Round trip typed arrays.
- [x] Restore closure locals.
- [x] Preserve shared closure lexical environment.
- [x] Restore module exports.
- [x] Restore evaluated module namespace live export bindings.
- [x] Round trip fulfilled promise state.
- [x] Round trip rejected promise state.
- [x] Round trip ordinary pending promise state.
- [x] Restore ordinary pending promise with reachable resolver root.
- [x] Restore suspended async function.
- [x] Restore nested async function stack.
- [x] Restore pending promise reactions reached from host-backed promises.
- [x] Restore pending host-backed promise reaction handlers.
- [x] Restore selected global roots with cross-root object identity.
- [x] Restore host-promise registry from selected global roots.
- [x] Restore `queueMicrotask` queue order.
- [x] Restore queued promise reaction jobs.
- [x] Restore mixed pending job queue order.
- [x] Restore host-backed promise and resolve it.
- [x] Restore host-backed promise and reject it.
- [x] Fail unsupported native object snapshots clearly.
- [x] Fail ABI mismatch clearly.
- [x] Fuzz snapshot/restore simple object graphs.

### Phase 5: Language-Neutral Host Core

- [x] Move prompt implementation behind JSON host core API.
- [x] Move tool implementation behind JSON host core API.
- [x] Move HTTP implementation behind JSON host core API.
- [x] Move input implementation behind JSON host core API.
- [x] Move call-agent implementation behind JSON host core API.
- [x] Move template implementation behind JSON host core API.
- [x] Move memory implementation behind JSON host core API.
- [x] Move sandbox helpers behind JSON host core API.
- [x] Move log implementation behind JSON host core API.
- [x] Preserve policy enforcement.
- [x] Preserve replay lookup.
- [x] Preserve call logging.
- [x] Preserve prompt stream events.
- [x] Preserve OTEL spans.
- [x] Persist pending host operation metadata before live side effects.
- [x] Add host-operation safepoint hook after pending metadata persistence and
      before live side effects.
- [x] Fail closed when a host-operation safepoint cannot persist its
      snapshot, leaving the live side effect unexecuted.
- [x] Wire the host-operation safepoint into persisted TypeScript engine runs
      so the snapshot manifest is refreshed before live host execution.
- [x] Add host-operation completion safepoint after result persistence and
      call-log recording, before control returns to JavaScript.
- [x] Preserve completed host-promise result and call log if the completion
      safepoint fails after a host result is recorded.
- [x] Wire the completion safepoint into persisted TypeScript engine runs so
      the snapshot manifest is refreshed after host results are recorded.
- [x] Add a runtime-context live VM snapshotter hook so snapshot-capable
      TypeScript runtimes can persist live continuation bytes through the
      existing host-operation safepoints.
- [x] Persist registered live VM snapshotter blobs through generic durable
      host-operation safepoints before and after a non-provider side effect.
- [x] Persist a checkpoint marker call through the safepoint path, including
      the resolved checkpoint host-promise record and checkpoint call log.
- [x] Persist prompt provider results and token usage through the completion
      safepoint before later JavaScript can fail.
- [x] Persist TypeScript tool results through the completion safepoint before
      later agent JavaScript can fail.
- [x] Persist sub-agent results through the completion safepoint before later
      parent-agent JavaScript can fail.
- [x] Replay completed persisted host operation results before live side
      effects when a call log record is missing.
- [x] Preserve pre-migration host behavior with regression coverage during
      extraction.

### Phase 6: TypeScript Transpile And Module Loader

- [x] Add transpile-only TS to JS path.
- [x] Decide embedded transpiler implementation.
- [x] Preserve source maps or line mapping.
- [x] Validate `agent` export exists.
- [x] Validate `tool` and `run` exports for tools.
- [x] Disable dynamic import by default.
- [x] Add deterministic module resolver for local imports if supported.
- [x] Add syntax/check command for `.ts`.
- [x] Add diagnostics with source file and line number.

### Phase 7: TypeScript Runtime Binding

- [x] Create JS `chidori` object.
- [x] Add initial `TypeScriptVmRuntime` module-evaluation scaffold.
- [x] Add Chidori-QuickJS-backed TypeScript snapshot runtime scaffold.
- [x] Move recorder/no-context TypeScript agent evaluation onto
  `crates/chidori-quickjs` with deterministic runtime policy prelude.
- [x] Move restricted TypeScript tool metadata evaluation onto
  `crates/chidori-quickjs`.
- [x] Persist initial TypeScript VM snapshot blob for persisted runs.
- [x] Add JSON input/output conversion through QuickJS JSON parse/stringify.
- [x] Reject function and class-instance values at the Chidori JSON
  host-boundary in the QuickJS wrapper.
- [x] Expose and test native Rust callback/context-opaque FFI plus
  `SnapshotContext` wrapper API for future production `chidori` host method
  installation.
- [x] Add native object-method callback helpers that read JS arguments into
  Rust state and return JSON values to JS.
- [x] Add deterministic `Date.now` / `Math.random` installation scaffold.
- [x] Apply `snapshot.maps_sets=reject` in the TS VM scaffold.
- [x] Bind `log`.
- [x] Bind `checkpoint` as a call-log checkpoint marker.
- [x] Bind `memory` with call logging and replay-first disk access.
- [x] Bind `template` with call logging and replay-first rendering.
- [x] Bind `http` with policy enforcement, call logging, replay-first
      results, and recorded error outcomes.
- [x] Bind `input` with replay-first answers and pause-mode `PendingInput`.
- [x] Bind `execJs`, `execPython`, and `execWasm` to the existing WASM sandbox
      helpers with call-log replay.
- [x] Bind `callAgent` for TypeScript sub-agents with shared
      runtime context and call-log replay.
- [x] Bind `prompt` calls with replay, token usage logging, labelled prompt
      stream events, and the registered-tool use loop.
- [x] Bind `tool` through the shared TypeScript/MCP tool registry
      with policy enforcement and call-log replay.
- [x] Bind `parallel` as the TypeScript Promise helper API. True concurrent
      durable host work remains gated on host-backed promises.
- [x] Bind pure JS `retry` and `tryCall` helpers.
- [x] Dispatch `.ts` agents through `Engine::run`.
- [x] Preserve call logging and `--stream` call events for `log`.
- [x] Preserve replay lookup and divergence checks for bound `log` and
      `checkpoint` calls.
- [x] Bind `prompt`.
- [x] Bind `input`.
- [x] Bind `tool`.
- [x] Bind `callAgent`.
- [x] Bind `parallel`.
- [x] Bind `retry`.
- [x] Bind `tryCall`.
- [x] Bind `http`.
- [x] Bind `template`.
- [x] Bind `log`.
- [x] Bind `memory`.
- [x] Bind `checkpoint`.
- [x] Bind sandbox helpers.
- [x] Convert JS values to JSON.
- [x] Convert JSON to JS values.
- [x] Create host-backed promises.
- [x] Restore transpiled async TypeScript agent state in snapshot runtime
      scaffold.
- [x] Restore initial TypeScript snapshot scaffold with relative named and
      namespace imports bundled into closure-captured bindings.
- [x] Record local TypeScript module fingerprints from the same dependency
      walk used by the initial snapshot scaffold.
- [x] Record source-level local TypeScript module graph import edges in
      snapshot manifests.
- [x] Include bundled TypeScript module namespace roots in initial snapshot
      scaffold blobs.
- [x] Snapshot before durable side effects through the current
      scaffold/registered-live-snapshotter safepoint path.
- [x] Resolve host promises after results.
- [x] Reject host promises after errors.

### Phase 8: Durable Session Resume

The checked safepoint items below cover the initial snapshot scaffold,
registered live-VM snapshotter boundary, ordinary persisted safepoints that
rebuild a restorable `LiveQuickJsVm` envelope from host promise records, and
direct server `LiveQuickJsVm` restore for the current TypeScript host paths.
The remaining runtime work is full QuickJS live module/runtime-root
serialization beyond the selected-root scaffold.

- [x] Persist initial TypeScript VM snapshot blob on `input()` pause.
- [x] Persist pending input metadata and host-promise table state alongside
      the initial TypeScript VM snapshot blob.
- [x] Persist snapshot on `input()` pause through the current safepoint path.
- [x] Persist registered live VM snapshotter blobs on `input()` pause.
- [x] Persist ordinary `LiveQuickJsVm` blobs on host-promise safepoints by
      rebuilding the continuation from persisted host promise records.
- [x] Persist initial TypeScript VM snapshot blob on policy approval pause.
- [x] Persist pending policy-gated host operation metadata and host-promise
      table state alongside the initial TypeScript VM snapshot blob.
- [x] Persist snapshot on policy approval pause through the current safepoint
      path.
- [x] Persist registered live VM snapshotter blobs on policy approval pause.
- [x] Persist snapshot before provider calls through the current safepoint path.
- [x] Persist initial TypeScript VM snapshot blob plus pending prompt metadata
      before provider execution begins.
- [x] Persist registered live VM snapshotter blobs plus pending prompt
      metadata before provider execution begins.
- [x] Persist snapshot after provider result record through the current
      safepoint path.
- [x] Persist initial TypeScript VM snapshot blob plus completed prompt
      provider result record after provider completion.
- [x] Persist registered live VM snapshotter blobs plus completed prompt
      provider result record after provider completion.
- [x] Resume `input()` by resolving host promise.
- [x] Resume approval by resolving host promise.
- [x] Resume failed host call by rejecting host promise.
- [x] Fail closed if a persisted pending host operation is missing from the
      host-promise table or is already completed before server resume resolves
      it.
- [x] Add kind-gated TypeScript live VM restore loader that reaches the
      QuickJS fork restore boundary only for live snapshot blobs.
- [x] Reject live VM manifests in the current replay-based server resume
      validator until server resume is wired to live VM restore.
- [x] Reject resume on source hash mismatch.
- [x] Reject resume on TypeScript module graph mismatch.
- [x] Reject resume on ABI mismatch.
- [x] Preserve existing session statuses.
- [x] Update `/sessions/{id}/resume`.
- [x] Update `/sessions/{id}/checkpoint`.
- [x] Add snapshot manifest endpoint if needed.

### Phase 9: Parallel Runtime

- [x] Define branch snapshot layout.
- [x] Start branch runtimes from parent snapshot through the current
      scaffold/live-snapshot helper boundary.
- [x] Add kind-gated live VM helper that restores one branch runtime per
      branch from the parent live VM snapshot blob.
- [x] Start forked TypeScript snapshot contexts from parent snapshot.
- [x] Start manifest-driven TypeScript snapshot branch contexts from parent
      snapshot and merge outputs deterministically.
- [x] Start TypeScript snapshot branch contexts from a persisted parent
      snapshot store and persist per-branch snapshot scaffolds.
- [x] Assign branch operation ids.
- [x] Assign branch call-log sequence ranges.
- [x] Record parent run id, parallel operation id, branch index, and branch
      operation id in branch snapshot manifests.
- [x] Validate branch snapshot metadata before live or scaffolded branch
      resume uses a persisted branch blob.
- [x] Validate the persisted branch snapshot is paused on the host operation
      being resolved or rejected before live branch resume touches the restored
      runtime.
- [x] Persist branch snapshots.
- [x] Persist live VM branch runtime snapshots with branch metadata and live
      blob-kind validation.
- [x] Resume paused branch through the current scaffold/live-snapshot helper
      boundary.
- [x] Add kind-gated live VM branch resume helper that restores the branch
      runtime, resolves the pending host promise, and drains jobs.
- [x] Add kind-gated live VM branch error-resume helper that restores the
      branch runtime, rejects the pending host promise, and drains jobs.
- [x] Add TypeScript snapshot restore-from-snapshot plus resolve/reject helpers
      that return the restored context and next run state.
- [x] Resume paused TypeScript snapshot branch by restoring a branch snapshot
      and resolving its host promise.
- [x] Resume paused TypeScript snapshot branch from the persisted branch store
      scaffold.
- [x] Merge branch logs deterministically.
- [x] Preserve branch output order.
- [x] Propagate first branch error.
- [x] Enforce concurrency limit.

### Phase 10: Tooling And Docs

- [x] Generate TypeScript declarations for `chidori`.
- [x] Update README quickstart to `.ts`.
- [x] Update `llm.txt` API reference.
- [x] Update SDK docs for snapshot-aware checkpoints.
- [x] Add TS examples for hello, summarizer, webhook, tools, input pause,
  streaming progress, sub-agent, parallel, and sandbox execution.
- [x] Remove or archive `.star` examples.
- [x] Update package metadata from Starlark to TypeScript.

### Phase 11: Removal Of Starlark Runtime

- [x] Remove `starlark` dependency.
- [x] Remove Starlark dialect module.
- [x] Remove Starlark engine implementation.
- [x] Remove Starlark host binding code.
- [x] Remove `.star` tool parser.
- [x] Remove `.star` examples or move them to historical docs.
- [x] Update CLI help text from `.star` to `.ts`.
- [x] Update server agent listing from `.star` to `.ts`.
- [x] Update recipes to point at `.ts`.

### Phase 12: Acceptance Tests

The checked crash-survival items below describe persisted host-promise
recovery, restorable live VM blob persistence at ordinary safepoints, replay
fallback, and direct live-VM resume for the current TypeScript host paths.
Direct continuation to another `input()` pause is covered by preallocated
snapshot host promises, and awaited `log()` calls can be driven directly.
Awaited inline `template()` calls can also be driven directly through the
server template engine. Awaited `memory()` calls can be driven directly through
the shared memory host core, and awaited `checkpoint()` calls are
recorded directly as marker records. Awaited sandbox calls can also be driven
directly through the shared sandbox host core. Failed handled host calls reject
the restored JS promise so caught errors can continue on the live VM.
Policy-denied `http()` calls are rejected directly without replay, and
always-allowed `http()` calls execute through the shared HTTP host core.
Policy-gated `http()` calls persist an approval pause with the live VM
snapshot and continue directly after approval. Non-suspending TypeScript
`tool()` calls execute through the server tool registry, and non-suspending
TypeScript `callAgent()` calls execute through the TypeScript child-agent
runtime. Plain-text `prompt()` calls execute through the provider registry and
record token usage. `prompt()` tool loops execute directly for non-suspending
tools. Top-level TypeScript `tool()` calls that suspend on nested `input()`
now persist and resume directly, and top-level `callAgent()` calls whose child
agent suspends on nested `input()` do the same. Their post-resume rejection
paths are covered, including parent `try/catch` handlers; replay recovery also
reports a pause when a pending `input()` survives a caught internal pause
marker. `prompt()` tool loops now persist enough provider-loop state to resume
a TypeScript tool that suspended on nested `input()`, continue the prompt
conversation, pause again for repeated suspending tool calls, and resolve the
parent live VM prompt promise directly. Nested `callAgent()` can now continue
directly through a grandchild `input()` pause when the nested child path
completes successfully or rejects after the grandchild input resumes. The
`return await chidori.callAgent(...)` path is covered by direct live resume and
by replay fallback coverage.

- [x] `chidori check examples/agents/hello.ts`.
- [x] `chidori run examples/agents/hello.ts`.
- [x] `chidori run examples/agents/summarizer.ts --stream`.
- [x] `chidori serve examples/agents/webhook.ts`.
- [x] Pause on `input()`, stop server, restart, resume successfully.
- [x] Pause on policy approval, stop server, restart, approve successfully.
- [x] Prompt result survives a crash-like interruption after provider result
      persistence through completed host-promise replay.
- [x] Prompt provider result and token usage survive a post-provider JavaScript
      failure in persisted host-promise and call-log state.
- [x] Completed prompt host operation result is replayed without provider
      execution when the call-log record is missing.
- [x] Server replay consumes completed prompt host-promise state without
      provider execution when the session call log is empty.
- [x] Server resume consumes completed prompt host-promise state before
      resolving a later paused input.
- [x] Tool call survives a crash-like interruption through completed
      host-promise replay without re-executing the tool.
- [x] Tool result survives a post-tool JavaScript failure in persisted
      host-promise and call-log state.
- [x] Completed tool host operation result is replayed without live execution
      when the call-log record is missing.
- [x] Server replay consumes completed tool host-promise state without tool
      registry execution when the session call log is empty.
- [x] Server resume consumes completed tool host-promise state before
      resolving a later paused input.
- [x] Sub-agent call survives a crash-like interruption through completed
      host-promise replay without re-executing the child agent.
- [x] Sub-agent result survives a post-sub-agent JavaScript failure in
      persisted host-promise and call-log state.
- [x] Completed sub-agent host operation result is replayed without child
      execution when the call-log record is missing.
- [x] Server replay consumes completed sub-agent host-promise state without
      child agent execution when the session call log is empty.
- [x] Server resume consumes completed sub-agent host-promise state before
      resolving a later paused input.
- [x] Parallel branch survives snapshot-store pause/resume through the current
      TypeScript snapshot scaffold.
- [x] Source hash mismatch blocks resume.
- [x] Module graph mismatch blocks resume.
- [x] ABI mismatch blocks resume.
- [x] Snapshot manifest can be inspected without exposing raw VM bytes.

Deferred future QuickJS generalization:

- Serialize live QuickJS `JSModule` records and module graph beyond Chidori's
  bundled selected-root TypeScript module scaffold.
- Add end-to-end live VM crash/resume acceptance tests for those full
  module/runtime-root serializer roots if they become part of the production
  surface.
- [x] `cargo test` passes.
- [x] `cargo check` passes.
- [x] First-party TS SDK build passes.

## Open Decisions

- Exact TS transpiler implementation.
- Whether raw snapshot download is ever exposed to admins.
