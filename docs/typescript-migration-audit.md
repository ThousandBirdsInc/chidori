# TypeScript Migration Audit

> **⚠️ Historical — superseded.** This audit describes the QuickJS-era
> architecture that has since been removed. The QuickJS crates
> (`crates/chidori-quickjs`, `crates/chidori-quickjs-sys`) and the
> `crates/chidori/src/runtime/typescript/engine.rs` it cites **no longer exist**:
> QuickJS was removed in #39 and `chidori-js` (pure-Rust) is now the sole JS
> engine. The live VM snapshot/restore model described here (`LiveQuickJsVm`,
> "snapshot-capable QuickJS wrapper", direct live-VM continuation restore with
> replay as a fallback) was **descoped** — deterministic replay is now the
> **only** durability mechanism, not a fallback. For the current state see
> [`docs/DESIGN.md`](./DESIGN.md), [`docs/TODO.md`](./TODO.md), and
> [`docs/conformance.md`](./conformance.md).

This audit records the concrete state of the TypeScript runtime migration. It
is intentionally separate from the roadmap so completion claims can be checked
against files, tests, and known blockers.

## Success Criteria

- TypeScript `.ts` agents are the primary runtime surface.
- Legacy Starlark runtime code is removed from production paths; archived
  examples remain only as migration reference.
- Host APIs work from TypeScript and preserve replay/durable host-operation
  behavior.
- TypeScript tools are discovered, validated, invoked, and covered by tests.
- CLI, server, Python SDK, and TypeScript SDK expose TypeScript sessions,
  checkpoints, streaming, replay, resume, and snapshot manifest metadata.
- Design docs and LLM-facing docs describe the current TypeScript runtime state
  without overstating live VM restore support.
- CI covers the migration gates used locally.
- Production live VM continuation restore is either implemented or explicitly
  blocked with failing guards.

## Evidence

| Area | Evidence |
| --- | --- |
| TypeScript agent runtime | `src/runtime/typescript/`, `src/runtime/engine.rs`, `src/main.rs`; `cargo test --workspace` includes TypeScript runtime, CLI, and server tests. |
| Snapshot backend status | Active runtime-context host-binding execution now runs through `crates/chidori-quickjs` via `src/runtime/typescript/engine.rs` and the native `TypeScriptSnapshotContext` host object. The legacy `rquickjs` binding module is compiled only for parity tests, and `rquickjs` is a dev-dependency rather than a production dependency. Full QuickJS `JSModule` and runtime-root coverage beyond Chidori's selected-root scaffold is deferred future runtime generalization work. |
| Legacy Starlark removal | `src/runtime/dialect.rs` and `src/runtime/host_functions.rs` removed; `.star` examples removed (kept in git history); `.star` dispatch and tools are rejected/ignored by tests. |
| Host API durability | `src/runtime/host_core.rs`, `src/runtime/context.rs`, `src/runtime/snapshot.rs`; tests cover pending/completed host promises, safepoints, replay, input pause, policy approval, prompt/tool/sub-agent recovery, and nested tool suspension. |
| TypeScript tools | `src/runtime/typescript/tools.rs`, `src/tools/mod.rs`; metadata is evaluated in a restricted `crates/chidori-quickjs` VM context, tool source fingerprints are captured, `.star` tools are ignored, and TypeScript tool execution is tested. |
| Fork runtime/context snapshots | `crates/chidori-quickjs/src/lib.rs`, `crates/chidori-quickjs-sys/quickjs/chidori_snapshot_stub.c`, and the patched `quickjs.c` cover a versioned runtime envelope, restored fresh runtime, selected Chidori TypeScript roots, value/object graph snapshots, bytecode, closures, pending promises, host promise ids, async continuation frames, microtask/job queue snapshots, JSON host-boundary rejection for function/class-instance results, and a `SnapshotContext` native Rust callback/context-opaque wrapper API. Wrapper tests install Rust-backed methods on `globalThis.chidori`, read JS arguments into Rust state, convert callback arguments to JSON, and return JSON to JS; `TypeScriptSnapshotContext` now forwards that native callback bridge and covers both a generic Rust callback and `RuntimeContext`/`host_core`-backed `chidori.log`/`chidori.checkpoint`/`chidori.memory`/`chidori.template`/`chidori.execJs`/`chidori.execPython`/`chidori.execWasm` methods called from TypeScript agents, captured-networking (`fetch`/`node:http`) policy denial and `chidori.input` pause paths, provider-backed `chidori.prompt` including non-suspending tool loops, registry-backed non-suspending `chidori.tool`/`chidori.callAgent`, and the pure JS `tryCall`/`retry`/`parallel` helpers. Runtime-plus-context tests restore a suspended Chidori host call and complete it by host promise id. This is now the active TypeScript host runtime for ordinary execution. |
| Pending host operation adapter | `src/runtime/context.rs` persists concrete host function names, and `src/runtime/typescript/snapshot.rs` maps persisted pending operation metadata and pending host-promise records to snapshot-side `chidori` host-promise method queues for prompt/input/tool/callAgent/http/template/memory/checkpoint/log plus concrete sandbox functions. Policy approval remains out-of-band. |
| Unsupported snapshot values | `CHIDORI_JS_SnapshotContext` reports selected-root serialization failures through the unsupported-value hook, and `crates/chidori-quickjs/src/lib.rs` asserts path/type detail for an unsupported native root. |
| Snapshot-side host pause | `src/runtime/typescript/snapshot.rs` covers `(input, chidori)` TypeScript agents suspended on snapshot-side `chidori.input()` and `chidori.prompt()` host-promise shims. Default snapshot roots preserve the active host operation id, host promise registry, host-call records, and queued host method promises so the wrapper can restore, complete by resolving or rejecting the saved host promise, and pause again on a later host call with a distinct operation id. `run_jobs_until_blocked()`, the context-level resolve/reject-and-run helpers, and the TypeScript restore-from-snapshot plus resolve/reject helpers report that later pause as `BlockedOnHostOperation`. |
| CLI coverage | `tests/cli_typescript.rs` covers `check`, `run`, `stream`, snapshot manifest persistence, tool listing, tool invocation, and legacy `.star` rejection. |
| Server and SDK parity | `src/server.rs`, `sdk/python/chidori/client.py`, `sdk/typescript/src/index.ts`; Python integration tests cover sessions, checkpoint metadata, resume, stream events, auth, CORS, and concurrency. |
| Docs | `README.md`, `DESIGN.md`, `TODO.md`, `llm.txt`, `docs/typescript-vm-snapshot-runtime.md`; current docs distinguish direct live VM restore coverage from replay fallback and from future full QuickJS module/runtime-root serialization. |
| CI | `.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo test --workspace`, TypeScript SDK `npm run typecheck`, TypeScript SDK `npm run build`, and Python SDK integration tests. |

## Current Verification Gates

```bash
cargo fmt --check
cargo test --workspace
cd sdk/typescript && npm run typecheck
cd sdk/typescript && npm run build
python -m unittest sdk/python/tests/test_session_api.py
```

The Python integration suite binds loopback ports for its mock provider and
server.

## Completion Notes

The TypeScript migration is complete for the current Chidori runtime surface.
Direct live VM continuation restore is implemented for the production
TypeScript host paths, with durable replay retained as a fallback. Remaining
QuickJS work is future runtime generalization beyond Chidori's bundled
selected-root TypeScript module scaffold:

- `crates/chidori-quickjs-sys/quickjs/chidori_snapshot_stub.c` now provides a
  versioned runtime envelope plus selected-root context restore. Full QuickJS
  `JSModule` record serialization remains deferred because Chidori TypeScript
  imports are bundled into selected snapshot roots.
- `crates/chidori-quickjs/src/lib.rs` covers runtime-plus-context restore for
  a suspended host call. Ordinary `chidori run` / `chidori serve` TypeScript
  execution now uses the snapshot-capable runtime and native host object; full
  QuickJS live module records remain outside the selected-root graph.
- `src/runtime/typescript/tools.rs` now uses the snapshot-capable
  `crates/chidori-quickjs` wrapper for restricted TypeScript tool metadata
  evaluation, `TypeScriptVmRuntime::run_agent_source` uses the same wrapper
  for recorder/no-context agent evaluation with the deterministic runtime
  policy prelude, and `run_agent_source_with_context` now installs the native
  snapshot `chidori` host object on `crates/chidori-quickjs` for ordinary
  production execution. The TypeScript snapshot wrapper can install
  Rust-backed native `chidori` methods through the context-opaque callback
  bridge, and `chidori.log`/`chidori.checkpoint`/
  `chidori.memory`/`chidori.template`/`chidori.execJs`/`chidori.execPython`/
  `chidori.execWasm` now record through `RuntimeContext`/`host_core` on that
  path. The captured networking host op (reached via `fetch`/`node:http`, not a
  public `chidori.http`) covers policy denial, and native `chidori.input` covers
  pause-mode pending input. Plain-text `chidori.prompt`
  and provider-requested prompt tool loops now record through the
  provider-backed host core on the active native snapshot path for non-suspending
  registered TypeScript tools, and non-suspending `chidori.tool`/
  `chidori.callAgent` now record through the registry-backed native snapshot
  path. Nested suspending sub-agent direct live-resume paths are covered through
  grandchild input success and rejection cases.
  `tryCall`, `retry`, and `parallel` can now be installed directly on the
  snapshot `chidori` object.
  Ordinary
  persisted safepoints now rebuild a restorable
  `LiveQuickJsVm` envelope from persisted host promise records when possible,
  and the wrapper covers generic `chidori.<method>()` host-promise suspension,
  and the active production host-binding path now uses that native snapshot
  context. Imported TypeScript modules in the bundled selected-root scaffold
  are covered through direct server live resume, including exported functions
  that close over exported constants.
- The QuickJS fork runtime entrypoint currently restores a fresh runtime and
  then restores selected context roots. Runtime-global heap state outside those
  roots is future generalization work, not part of the current TypeScript
  migration surface.
- `src/server.rs` now accepts `SnapshotBlobKind::LiveQuickJsVm` manifests
  during resume validation and attempts direct live-VM completion for resumed
  `input()` pauses before falling back to durable host-promise replay. It also
  persists a newly reached direct live-VM `input()` pause and handles awaited
  `log()`, `template()`, `memory()`, `checkpoint()`, and sandbox calls without
  replay. Failed handled host calls reject the restored JS promise and can
  continue when agent code catches the error. Policy-denied `http()` calls are
  rejected directly without replay, and always-allowed `http()` calls execute
  through the shared HTTP host core. Policy-gated `http()` calls now persist an
  approval pause with the live VM snapshot and continue directly after
  approval. Non-suspending TypeScript `tool()` calls execute through the server
  tool registry, and non-suspending TypeScript `callAgent()` calls execute
  through the TypeScript child-agent runtime. Plain-text `prompt()` calls
  execute through the provider registry and record token usage. `prompt()` tool
  loops execute directly for non-suspending tools. Top-level TypeScript
  `tool()` calls that suspend on nested `input()` now persist and resume
  directly, as do top-level `callAgent()` calls whose child agent suspends on
  nested `input()`. Their post-resume rejection paths are covered, including
  parent `try/catch` handlers; the replay engine also treats any still-pending
  `input()` operation as a pause even if user JavaScript caught the internal
  pause marker. `prompt()` tool loops now persist enough provider-loop state to
  resume a TypeScript tool that suspended on nested `input()`, continue the
  prompt conversation, pause again for repeated suspending tool calls, and
  resolve the parent live VM prompt promise directly. Nested `callAgent()` can
  now continue directly through a grandchild `input()` pause when the nested
  child path completes successfully or rejects after the grandchild input
  resumes. The `return await chidori.callAgent(...)` path is covered by direct
  live resume and by replay fallback coverage.
- `TODO.md` keeps full QuickJS `JSModule` record serialization and runtime
  roots outside the selected context graph listed as deferred/future work.

Replay recovery remains tested as an explicit fallback for cases where direct
live VM continuation cannot safely drive a newly reached production host call.
