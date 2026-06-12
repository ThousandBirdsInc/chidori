# TODO - Chidori TypeScript Runtime

This roadmap tracks the TypeScript-runtime migration. Historical Starlark work
is preserved in git history and under `examples/legacy-starlark/`, but new
runtime, CLI, server, and tool work should target `.ts` agents and tools.

## Status At A Glance

| Area | Status |
| --- | --- |
| TypeScript agent dispatch | Done |
| TypeScript host API bindings | Done |
| Language-neutral host core | Done |
| Call-log replay | Done |
| Human input and policy approval pause/resume | Done through live VM resume with replay fallback |
| Multiplayer signals (`chidori.signal` / `pollSignal`, `POST /sessions/{id}/signal`) | Done — Phase 1 + `pollSignal` (`docs/signals.md`); `signalAny` / `timeoutMs` / live in-memory delivery are future work |
| TypeScript tool discovery | Done |
| TypeScript and Python SDK parity | Done |
| Snapshot manifests, policy/source validation, host promise records | Done |
| In-repo QuickJS fork and Rust wrapper | Done for current runtime surface |
| Full live VM continuation snapshot/restore | Done for current TypeScript runtime surface |
| Server resume from `LiveQuickJsVm` blobs | Done for current production TypeScript host paths |

See [DESIGN.md](./DESIGN.md) for the top-level design and
[docs/typescript-vm-snapshot-runtime.md](./docs/typescript-vm-snapshot-runtime.md)
for the durable VM snapshot design. The concrete migration evidence and
completion audit lives in
[docs/typescript-migration-audit.md](./docs/typescript-migration-audit.md).

## Completed Migration Work

### TypeScript Runtime

- [x] Require `.ts` agent files in runtime dispatch.
- [x] Reject unsupported non-TypeScript agent files with clear errors.
- [x] Transpile TypeScript at runtime without requiring `tsc`.
- [x] Install deterministic `Date` and `Math.random` behavior.
- [x] Enforce durable-run policy for host date/random settings.
- [x] Reject dynamic imports and unsupported local import policies.
- [x] Support relative TypeScript imports under runtime policy.
- [x] Convert JS values to JSON-compatible host values.
- [x] Fail clearly for functions, unsupported native values, class instances,
  shared memory, WeakRef, and finalizers at the host boundary.
- [x] Run TypeScript agents from CLI and server paths.

### Host API

- [x] `chidori.prompt()`
- [x] `chidori.input()`
- [x] `chidori.signal()` / `chidori.pollSignal()` — multiplayer named signals:
      blocking listen point, durable per-run mailbox, `POST /sessions/{id}/signal`
      delivery (resolve+resume / enqueue / 409), deterministic replay (Phase 1 +
      `pollSignal` of `docs/signals.md`; `signalAny` / `timeoutMs` / live in-memory
      delivery are future work).
- [x] `chidori.callAgent()`
- [x] `chidori.tool()`
- [x] `chidori.parallel()`
- [x] `chidori.branch()` — in-agent execution branching (Phase 1 of
      `docs/branching-execution.md`): fork into per-strategy sub-runs from the
      anchored state, outcomes returned for comparison; pausable/persisted
      branches and the whole-agent replay-prefix model are future work.
- [x] `chidori.retry()`
- [x] `chidori.tryCall()`
- [x] `chidori.http()`
- [x] `chidori.template()`
- [x] `chidori.log()`
- [x] `chidori.memory()`
- [x] `chidori.checkpoint()`
- [x] `chidori.execJs()`
- [x] `chidori.execPython()`
- [x] `chidori.execWasm()`
- [x] Replay cached host-call results without repeating side effects.
- [x] Record host promise lifecycle metadata for durable host operations.
- [x] Persist safepoints before live side effects and after results.

### Tools And Examples

- [x] Discover `.ts` tools and evaluate exported `tool` metadata.
- [x] Validate TypeScript tool JSON schema metadata.
- [x] Invoke TypeScript tool `run(args, chidori)` exports.
- [x] Ignore `.star` files during tool discovery.
- [x] Convert first-party examples to TypeScript.
- [x] Move legacy Starlark examples to `examples/legacy-starlark/`.
- [x] Reject Starlark sub-agents from TypeScript agents.

### CLI, Server, And SDKs

- [x] `chidori check <file.ts>`
- [x] `chidori run <file.ts>`
- [x] `chidori serve <file.ts>`
- [x] `chidori trace <run_id>`
- [x] `chidori stats`
- [x] Snapshot metadata command/endpoint without exposing raw VM bytes.
- [x] Session create/list/get/checkpoint/replay/resume APIs.
- [x] Server-sent streaming for host calls and labelled prompt progress.
- [x] Python SDK run/replay/resume/checkpoint/stream support.
- [x] TypeScript SDK run/replay/resume/checkpoint/stream support.
- [x] SDK snapshot manifest metadata types.

### Snapshot Data Model

- [x] Snapshot manifest types.
- [x] Snapshot store read/write helpers.
- [x] ABI, source, module graph, and policy validation.
- [x] Snapshot blob kind gating.
- [x] Pending host operation persistence.
- [x] Host promise records for pending/resolved/rejected operations.
- [x] Branch snapshot metadata and merge helpers.
- [x] Live VM store helpers that reject invalid snapshot envelopes.

### QuickJS Fork Bootstrap

- [x] Add `crates/chidori-quickjs-sys`.
- [x] Add `crates/chidori-quickjs`.
- [x] Vendor QuickJS source under `crates/chidori-quickjs-sys/quickjs`.
- [x] Use local path dependency from the main runtime.
- [x] Add runtime memory limits and interrupt budget.
- [x] Expose raw snapshot/restore FFI symbols.
- [x] Expose host promise FFI symbols.
- [x] Expose run-jobs-until-blocked FFI symbol.
- [x] Add Rust snapshot reader/writer callback ABI.
- [x] Add unsupported-value callback ABI.
- [x] Validate runtime snapshot byte envelopes before persisting or loading.
- [x] Exercise value/bytecode snapshot coverage in wrapper tests.
- [x] Exercise snapshot-side generic `chidori.<method>()` host-promise
  pause/restore coverage in wrapper-backed TypeScript tests, including
  `input()` and `prompt()`.
- [x] Include active host operation id and host-call records in default
  TypeScript snapshot roots.
- [x] Clear the snapshot-side active host operation id after restored host
  promise resolution or rejection.
- [x] Preserve queued snapshot-side host method promises so a restored agent can
  pause again on a later host call with a distinct operation id.
- [x] Return `BlockedOnHostOperation` from snapshot wrapper job draining when
  resumed execution reaches another host pause.
- [x] Add snapshot wrapper resolve/reject-and-run helpers that return the next
  run state for future server restore wiring.
- [x] Add TypeScript snapshot restore-from-snapshot plus resolve/reject helpers
  that return the restored context plus next run state.
- [x] Add a snapshot-side adapter from persisted pending host operations to
  `chidori` host-promise method queues for unambiguous host APIs.
- [x] Add a snapshot-side adapter from persisted host-promise records that
  installs only pending operations for restore.

## Remaining Work

### Runtime Backend Convergence

- [x] Move the active TypeScript host-binding execution path off `rquickjs` or
  teach it to capture equivalent live snapshots from the in-repo QuickJS fork.
- [x] Install the full non-suspending `chidori` host object on the
  snapshot-capable `crates/chidori-quickjs` context for the active
  TypeScript execution path.
- [x] Complete the remaining server direct live-resume bookkeeping for nested
  suspending `callAgent()` rejection paths, including `return await
  chidori.callAgent(...)` and grandchild `input()` rejection propagation through
  parent `try/catch` handlers.
- [x] Move recorder/no-context TypeScript agent evaluation from `rquickjs` to
  `crates/chidori-quickjs`, including deterministic runtime policy prelude.
- [x] Move TypeScript tool metadata evaluation from `rquickjs` to
  `crates/chidori-quickjs` so tool discovery exercises the in-repo runtime.
- [x] Enforce Chidori JSON host-boundary validation in the
  `crates/chidori-quickjs` wrapper for function and class-instance results.
- [x] Expose the minimal QuickJS native callback/context-opaque FFI and
  `SnapshotContext` wrapper API needed to start installing production
  Rust-backed `chidori` methods in `crates/chidori-quickjs`.
- [x] Add `SnapshotContext` native object-method installation and callback
  argument/JSON return helpers, with Rust-backed `chidori.log`-style coverage.
- [x] Wire native callback/context-opaque helpers through
  `TypeScriptSnapshotContext`, with TypeScript agent coverage calling a
  Rust-backed `chidori.log` method.
- [x] Install `RuntimeContext`/`host_core`-backed native `chidori.log`,
  `chidori.checkpoint`, `chidori.memory`, and `chidori.template` methods on
  `TypeScriptSnapshotContext` as the first real snapshot-runtime host method
  slices.
- [x] Install `RuntimeContext`/`host_core`-backed native `chidori.execJs`,
  `chidori.execPython`, and `chidori.execWasm` methods on
  `TypeScriptSnapshotContext`.
- [x] Install `RuntimeContext`/`host_core`-backed native `chidori.http` policy
  denial and `chidori.input` pause paths on `TypeScriptSnapshotContext`.
- [x] Install `RuntimeContext`/provider-backed native plain-text
  `chidori.prompt` on `TypeScriptSnapshotContext`.
- [x] Run provider-requested `chidori.prompt` tool loops through the native
  snapshot host path for non-suspending registered TypeScript tools.
- [x] Install `RuntimeContext`/registry-backed native `chidori.tool` and
  `chidori.callAgent` paths on `TypeScriptSnapshotContext` for non-suspending
  TypeScript tools and child agents.
- [x] Install snapshot-runtime JavaScript helpers for `chidori.tryCall`,
  `chidori.retry`, and `chidori.parallel`.
- [x] Persist concrete sandbox host function names in pending host operation
  metadata so `execJs`, `execPython`, and `execWasm` can be restored without
  ambiguity.
- [x] Persist restorable `LiveQuickJsVm` blobs from ordinary TypeScript
  persisted safepoints by rebuilding the continuation from persisted host
  promise records, without test-only injected snapshotters.

### Full QuickJS Runtime Snapshot Serialization

The in-repo fork already exercises value/object graph, bytecode, host promise,
async continuation, and microtask queue snapshots through
`chidori_quickjs::SnapshotContext`. The context ABI now exposes selected
Chidori TypeScript roots and the microtask queue through
`CHIDORI_JS_SnapshotContext` / `CHIDORI_JS_RestoreContext`. Ordinary Chidori
TypeScript runs use a bundled selected-root module scaffold, so full QuickJS
`JSModule` record serialization is deferred until the runtime supports a
broader JavaScript module surface.

- [x] Implement `CHIDORI_JS_SnapshotRuntime` as a versioned runtime envelope.
- [x] Implement `CHIDORI_JS_RestoreRuntime` by restoring a fresh QuickJS
  runtime for the context payload.
- [x] Implement `CHIDORI_JS_SnapshotContext` for selected Chidori TypeScript
  roots and the microtask/job queue.
- [x] Implement `CHIDORI_JS_RestoreContext` for selected Chidori TypeScript
  roots and the microtask/job queue.
- [x] Compose runtime and context payload restore for a suspended Chidori host
  call completed by host promise id.
- [x] Preserve selected-root bundled TypeScript module namespace exports through
  snapshot restore, including exported functions that close over exported
  constants.
- [x] Fail selected-root context snapshot creation with path/type detail for
  unsupported native values.
- [x] Add fork-level context snapshot tests for suspended async functions and
  host promise resolution after restore.
- [x] Add fork-level `CHIDORI_JS_SnapshotContext` /
  `CHIDORI_JS_RestoreContext` entrypoint coverage for a suspended Chidori host
  call restored and completed by host promise id.
- [x] Add fork-level runtime-plus-context entrypoint tests for suspended async
  functions and host promise resolution after restore.
- [x] Add ordinary engine persistence coverage that restores a
  `LiveQuickJsVm` input pause and resolves the suspended host promise.

### Server Resume From Live VM Snapshots

- [x] Remove the server resume validation guard that rejected
  `LiveQuickJsVm` manifests up front.
- [x] On `input()` resume, load `runtime.snapshot`, restore the QuickJS
  runtime/context, resolve the saved host promise id, and use the live result
  when it completes.
- [x] Persist a newly reached direct live-VM `input()` pause, including the
  next pending host operation, host-promise table entry, and updated live
  snapshot.
- [x] Execute and record an awaited direct live-VM `log()` host operation
  without falling back to replay.
- [x] Execute and record an awaited direct live-VM `template()` host operation
  without falling back to replay.
- [x] Execute and record awaited direct live-VM `memory()` host operations
  without falling back to replay.
- [x] Execute and record awaited direct live-VM `checkpoint()` host operations
  without falling back to replay.
- [x] Execute and record awaited direct live-VM sandbox host operations
  (`execJs`, `execPython`, `execWasm`) without falling back to replay.
- [x] Reject failed direct live-VM host promises for handled operations and
  continue the live VM when agent code catches the error.
- [x] Reject direct live-VM `http()` calls denied by policy without falling
  back to replay.
- [x] Execute and record always-allowed direct live-VM `http()` calls without
  falling back to replay.
- [x] Persist direct live-VM `http()` policy approval pauses without falling
  back to replay.
- [x] Continue approved policy-gated direct live-VM `http()` calls without
  falling back to replay.
- [x] Execute and record direct live-VM TypeScript `tool()` calls without
  falling back to replay for non-suspending tools.
- [x] Execute and record direct live-VM TypeScript `callAgent()` calls without
  falling back to replay for non-suspending child agents.
- [x] Execute and record direct live-VM plain-text `prompt()` calls without
  falling back to replay.
- [x] Execute and record direct live-VM `prompt()` tool loops without falling
  back to replay for non-suspending tools.
- [x] Persist and resume direct live-VM `prompt()` tool loops when a
  TypeScript tool suspends on nested `input()`.
- [x] Persist and resume top-level direct live-VM TypeScript `tool()` calls
  that suspend on nested `input()`.
- [x] Persist and resume top-level direct live-VM TypeScript `callAgent()`
  calls when the child agent suspends on nested `input()`.
- [x] Persist and resume nested direct live-VM TypeScript `callAgent()` calls
  when a grandchild agent suspends on nested `input()`.
- [x] Preserve top-level suspending `tool()`/`callAgent()` rejection paths
  after nested `input()` resumes, including parent `try/catch` handlers.
- [x] Keep replay recovery covered as a fallback for nested suspending
  `callAgent()` rejection paths.
- [x] Persist final output for direct live-VM completion.
- [x] Extend server direct live-VM resume bookkeeping to the nested
  `callAgent()` rejection path after a grandchild `input()` resumes.
- [x] Keep replay as an explicit recovery path when direct live-VM resume
  cannot drive a newly reached production host call.

### Tool Metadata Hardening

- [x] Replace static text metadata extraction with restricted VM-backed
  TypeScript metadata evaluation.
- [x] Capture tool source fingerprints in tool registry entries.
- [x] Snapshot and directly resume TypeScript tool execution when a tool
  suspends on nested `input()`, with replay retained as a fallback.

### Quality Gates

- [x] Root Rust test suite passes with TypeScript runtime coverage.
- [x] Add CI for `cargo fmt --check`.
- [x] Add CI for `cargo test --workspace`.
- [x] Add CI for `npm run typecheck` in `sdk/typescript`.
- [x] Add CI for `npm run build` in `sdk/typescript`.
- [x] Add CI for Python SDK tests.
- [x] Add focused integration tests for CLI `.ts` examples.
- [x] Add end-to-end pause/resume tests that exercise `LiveQuickJsVm` restore.

## Useful Verification Commands

```bash
cargo fmt --check
cargo test
cargo test --workspace
cd sdk/typescript && npm run build
cd sdk/typescript && npm run typecheck
python -m unittest sdk/python/tests/test_session_api.py
```

## Deferred Or Descoped

- Visual editor work is not planned for this migration.
- Automatic `.star` to `.ts` conversion is not required for v1.
- Node package compatibility inside agents is not required for v1.
- Arbitrary mid-instruction snapshots for CPU-bound loops are not required for
  v1.
- Full QuickJS `JSModule` record serialization beyond Chidori's bundled
  selected-root TypeScript module scaffold is future runtime generalization
  work.
- Promoting the runtime envelope beyond fresh-runtime plus selected context
  roots is deferred until production host bindings require runtime-global heap
  state outside those roots.
- Unsupported-value path/type detail for future runtime roots outside the
  selected context-root graph is deferred with those roots.
