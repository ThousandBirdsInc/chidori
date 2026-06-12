# Removing QuickJS: gaps to close first

> **⚠️ Historical — superseded.** The migration this document tracks is
> **complete**: QuickJS was removed in #39 and `chidori-js` is now the only JS
> engine in the tree. The "G5 conformance bar still pending" and "default stays
> QuickJS" framing below describes the in-progress state and **no longer reflects
> the codebase**. Kept for historical context only. For the current state see
> [`docs/conformance.md`](./conformance.md) and
> [`docs/fable_review.md`](./fable_review.md).

**Status:** G1, G2, G3, G4, G6 **closed**; only G5 (conformance bar) remains.
**Goal:** make the pure-Rust `chidori-js` engine the *only* engine and delete the
QuickJS/C path entirely.

**Acceptance test (G1/G2) now passes:** the full `cargo test` suite is green
with `CHIDORI_JS_ENGINE=rust` (346 lib tests, incl. all engine + server
pause/resume/snapshot/resume tests), and the default QuickJS path stays green.
The runtime-default flip and QuickJS deletion are now gated only on the agreed
G5 conformance bar (G3 `node:` builtins now run and replay on the rust path).

## What landed to close G1/G2/G4/G6

- **G1 — pause/resume surfacing.** `engine.rs`'s rust arm now drives pause
  surfacing: a `chidori.input()` in Pause mode or a policy-approval block sets
  `pending_input`/`pending_approval` on the ctx and returns the `PAUSE_MARKER`
  sentinel, which bubbles up as `Err`; the rust arm checks
  `take_pending_input()`/`take_pending_approval()` on both `Ok` and `Err` and
  returns a paused `RunResult`. In-process resume works through the existing
  call-log replay.
- **G2 — durable persistence + server resume.** The rust live path's durability
  is the `RuntimeContext` call log (resume = deterministic call-log replay), not
  a VM image. `persist_rust_journal_scaffold` (`engine.rs`) writes a manifest
  (`InitialTypeScriptStateScaffold` kind) + call log + pending + host-promise
  table + VFS at each safepoint, using the `chidori-quickjs` ABI token so the
  unchanged server resume gate accepts it. The server's call-log replay resume
  (Branch D) preserves the original run id (`run_id` continuity, matching the
  live-VM path) via `…_preserving_run_id`.
- **G4 — native nested tool / sub-agent execution.** `chidori.tool` (TypeScript
  backend) and `chidori.callAgent` re-enter `rust_engine::run_tool_file` /
  `run_agent_file` natively when the rust engine is active, threading the same
  `HostBindingBackend` so nested host effects nest under the parent
  (`parent_seq`) and a nested suspension propagates as a pause of the whole run.
- **G6 — replay parity for suspending host effects.** Verified end-to-end by the
  server resume suite running green under rust (input, prompt, http, tool,
  sub-agent, prompt tool-loops incl. repeated mid-loop double-suspension,
  nested/suspending call_agent). Two replay-correctness fixes landed: `try_replay`
  now nests a replayed call under the executing container (so a replayed nested
  input is absorbed by `absorb_replayed_subtree` on a later resume, fixing a
  second-suspension divergence), and the approve-replay path replays the call log
  with the approval seeded instead of re-running fresh.

Cross-engine parity fixes made along the way: chidori-js host-effect rejections
now surface as a plain `Error` (not `TypeError`); an uncaught entrypoint
exception is framed as `JavaScript exception: <message>` like the QuickJS host;
`build_engine` reuses the app's provider registry (so replay-based resume sees
the same providers as the live-VM path); the entrypoint receives `chidori` as
its second argument (QuickJS `agent(input, chidori)` convention); and the rust
arm clears the streaming `event_sender` after a run (`ctx.clear_event_sender()`)
so a `--stream` drain loop terminates — at the time the chidori-js VM could
leak its heap on drop (Rc cycles), which would otherwise keep the dispatch
closure → ctx → sender alive and hang the channel. (Since then the engine grew
a per-VM allocation registry and cycle collector — `crates/chidori-js/src/gc.rs`
— `run_module` and tool-metadata evaluation now call `vm.dispose()`, which
breaks every allocation's edges, so a long-lived server no longer accretes a
leaked realm per agent run.)

QuickJS-mechanism-specific test assertions (live-VM blob kind, `PROMPT_TOOL_PAUSE_FILE`,
per-turn `max_turns` metadata, nested-suspension trace *shape*) were made
engine-aware: behavioral assertions (output, status, run id, pending prompt) run
identically for both engines; only the internal durability *shape* differs by
model (live-VM continuation vs deterministic replay).

## Where we are now

Landed (this iteration):

- `chidori-js` is compiled into every build (`default = ["rust-engine"]`) but is
  **opt-in at runtime** — `selected_engine()` returns `QuickJs` unless
  `CHIDORI_JS_ENGINE=rust` (`src/runtime/rust_engine.rs`). The default flip is
  gated on the G5 bar (see the Decision section).
- The full `chidori.*` host-effect surface runs on the rust engine — `log`, `input`,
  `prompt` (incl. the tool-use loop), `tool`, `callAgent`, `http`, `memory`,
  `template`, `checkpoint`, `execJs`/`execPython`/`execWasm`, `workspace.*` — routed
  through the shared `HostBindingBackend::dispatch` so the durable call log, policy,
  MCP, and OTEL span tree behave identically (`src/runtime/typescript/bindings.rs`,
  `src/runtime/rust_engine.rs`).
- Relative multi-file module imports resolve, transpile, and link via a host loader +
  chidori-js's existing ESM graph linker (`run_entrypoint_graph` in
  `crates/chidori-js/src/lib.rs`).

So a **single-file or relative-import agent that runs to completion** works end-to-end
on the rust engine today, as does a `node:`-using agent (crypto/fs/timers).
QuickJS removal now hinges only on the agreed conformance bar (G5).

The removal surface is large — `src/runtime/typescript/snapshot.rs` (~7.2k lines),
`src/server.rs` (~8.6k), `src/runtime/typescript/engine.rs` (~1.2k), plus the
`chidori-quickjs` / `chidori-quickjs-sys` crates. Do not delete until the gates below
close, or the build keeps compiling but the server loses durability with no replacement.

## Gates (must close before deletion)

### G1 — Pause/resume + suspension surfacing on the rust path
**CLOSED** (see "What landed" above). Original write-up retained for context:

**Blocker.** `Engine::run_agent`'s rust branch always returns `paused: None`
(`src/runtime/engine.rs`, the `#[cfg(feature = "rust-engine")]` arm). It never calls
`ctx.take_pending_input()` / `take_pending_approval()` / `take_pending_signal()`, so:
- `chidori.input()` in `Pause` mode and policy-approval pauses are dropped.
- The server's resume flow has nothing to resume.

The pieces exist but aren't wired: the engine already implements
`SnapshotCapableJsEngine` (`RustReplayEngine` in `rust_engine.rs`) with
`run_jobs_until_blocked` → `JsRunState::BlockedOnHostOperation`. Work: drive the agent
through that blocked-state loop in `engine.rs` (instead of the one-shot `run_agent`),
map a block on `input`/`approval`/`signal` to the `RunResult.paused*` fields, and
resume by resolving the host promise. ~Medium-large.

### G2 — Durable snapshot/restore + manifest persistence
**CLOSED** (see "What landed" above). The chosen format is the scaffold manifest
+ call log; resume is engine-agnostic call-log replay, so no separate rust
journal blob is threaded through the server. Original write-up retained:

**Blocker (server).** Persistence today is the QuickJS snapshot manifest
(`persist_ts_snapshot_manifest_scaffold`, `SnapshotStore`, `SnapshotManifest`,
`SnapshotAbi`) consumed by `src/server.rs` for resume. The rust engine's durability is a
different shape — the `DurableBlob` journal (`chidori_js::replay`, round-tripped through
`RustReplayEngine::snapshot`/`restore`). Work: teach the persistence + server resume
layer to store/load the rust journal blob (either behind the existing `SnapshotStore`
trait or a parallel store), and pick a single on-disk format. Depends on G1. ~Large.

### G3 — `node:` builtins (captured-effects VFS / crypto / timers)
**CLOSED.** The rust path now accepts `node:` specifiers and runs the full
captured-effects surface:

- `run_module` (`src/runtime/rust_engine.rs`) transpiles under
  `TypeScriptImportPolicy::Node` (matching the QuickJS durable default), and its
  module loader serves `node:` specifiers straight from
  `builtins::shim_source(name)` — the same engine-agnostic shim sources QuickJS
  uses (`src/runtime/typescript/builtins.rs`). Shim-to-shim `node:` imports
  (e.g. `node:crypto` → `node:buffer`) recurse through the same branch.
- A new `Engine::install_sync_natives` (`crates/chidori-js/src/lib.rs`) installs
  the synchronous `__chidori_crypto_*` / `__chidori_fs_*` / `__chidori_note_capability`
  globals. Their bodies live in the main crate (`build_sync_native_dispatch` in
  `rust_engine.rs`): hashing/HMAC run inline via `crate::runtime::crypto`;
  randomness replicates `execute_captured_random` against the shared
  `RuntimeContext` call log (recorded as `crypto.random`, replayed byte-for-byte);
  the VFS ops call `RuntimeContext::vfs_*` — the *same* snapshot-resident
  filesystem the QuickJS path uses. So a `node:fs`/`node:crypto` agent records and
  replays identically on either engine.
- `rust_engine_prelude` installs the determinism prelude (logical clock,
  `process.env`, `TEXT_ENCODING_POLYFILL`, `WEB_CRYPTO_POLYFILL`, and the
  `TIMER_VIRTUAL_POLYFILL`/`TIMER_DISABLED_POLYFILL`) — the same JS shared with
  QuickJS. chidori-js's native `Date`/`Math.random` already cover determinism, so
  (unlike QuickJS) no Date/random shim is installed.
- `fs`/`crypto`/`timers` `RuntimePolicy` gates are honored (`fs_policy_guard`,
  `crypto_policy_guard`, the disabled-timer polyfill); capability flags are raised
  via `note_capability` for parity.
- A native-builtin-subclassing gap was fixed in chidori-js to support
  `class Buffer extends Uint8Array`: `super(...)` to a native typed-array
  constructor now allocates the exotic internal slot and adopts it into the
  derived instance (`crates/chidori-js/src/builtins/typedarray.rs`).

Verified by `engine_node_builtins_crypto_fs_record_replay_parity`
(`src/runtime/engine.rs`), which runs a `node:crypto`+`node:fs` agent and asserts
a record→replay round-trip reproduces identical output (including the captured
randomness) — green under both `CHIDORI_JS_ENGINE=quickjs` and `=rust`.

**Known minor deltas (not blockers):** `node:http`/`node:https` shims are wired
but unverified on the rust path; `fetch`/`URLSearchParams` polyfills are not
installed on the rust path yet; and the rust `Date` does not advance with
`__chidori_now` as virtual timers fire (chidori-js's `Date` is a fixed-epoch
native; QuickJS's fixed-Date shim reads `__chidori_now`), a deterministic
difference only observable by an agent that mixes timers with `Date.now()`.

Original write-up:

**Blocker for parity.** The rust engine transpiles with `TypeScriptImportPolicy::Relative`,
so `node:fs`/`node:crypto`/`node:timers` (and bare specifiers) are rejected at transpile.
The whole captured-effects subsystem is QuickJS-only — the `FS_SHIM`, the native
`__chidori_crypto_hash`/`_hmac`/`_random` functions, and the virtual-timer model live in
`src/runtime/typescript/{builtins,snapshot}.rs` and are installed on the QuickJS global.
Work: install the equivalent native functions on the chidori-js VM, ship the `node:`
shim modules through the loader, and honor `RuntimePolicy` `fs`/`crypto`/`timers`. See
`docs/captured-effects-vfs-crypto-timers.md`. ~Large; can land incrementally per module.

### G4 — Native nested tool / sub-agent execution
**CLOSED** (see "What landed" above). `bindings.rs` now branches to
`rust_engine::run_tool_file` / `run_agent_file` when the rust engine is active;
the `TypeScriptVmRuntime` re-entry remains only as the QuickJS-path fallback and
goes away with deletion. Original write-up retained:

The rust path's `chidori.tool` (TypeScript backend) and `chidori.callAgent` currently
re-enter **QuickJS** via `TypeScriptVmRuntime` (`bindings.rs:444`, `:497`). Removing
QuickJS deletes `TypeScriptVmRuntime`, so these must re-enter `rust_engine::run_module`
natively while preserving durable nesting (`parent_seq`), policy enforcement, and MCP.
The dispatch logic also needs decoupling from `TypeScriptVmRuntime` (today
`HostBindingBackend::tool`/`call_agent` are the only non-rquickjs methods that still
reference it). ~Medium.

### G5 — Conformance bar
94.52% of executed Test262 today (37,618 pass / 2,179 fail / 7,494 skip; full
language + built-ins, rust engine, 2026-06-11 — was 91.69% on 2026-06-04; the
2026-06-11 iterations added dynamic `import()` (host-hook loader, +~250),
once-resolved `with`-scope references incl. closures capturing the with chain
(+~120), spec-order member compound assignment, define-semantics Array result
writes, Object-only String symbol-protocol dispatch, the per-instance private
brand model with lexically resolved class-unique private names (+~90),
constructor return-override via `super()`, a real Module Namespace exotic
object, generic `RegExp[@@split]`, spec `Promise.prototype.finally`, and
strict-mode `delete` TypeErrors — see `docs/conformance.md` for the current
cluster table; a third 2026-06-11 iteration added spec-ordered destructuring
assignment with abrupt-step [[Done]] latching, `yield*` sent-value forwarding +
`throw` delegation, and strict ECMA-262 `\p{…}` property matching with exact
UCD spellings). Agree on a
target (e.g. ≥95% language + built-ins) before removal so we don't silently regress
real agents. Progress + highest-impact remaining gaps:
- **Non-local completion + iterator close — largely DONE.** Via a Frame
  completion register (`Completion` + `do_completion` in `vm.rs`/`exec.rs`;
  single-landing-pad `compile_try_with_finally`):
  - `return`/`break`/`continue`/throw through `finally` run the finalizer(s)
    (`statements/try` 82.8%→90.4%);
  - `for-of` calls `IteratorClose` on abrupt exit, declaration **and**
    assignment forms (`for-of` 87.4%→**91.3%**);
  - array **destructuring** (declaration + assignment) closes the iterator on
    leftover/throw (`done` cell + `emit_iter_step_tracked`);
  - generator `.return()` runs enclosing `finally` (`pending_return` +
    `resume_frame_return`; `GeneratorPrototype` **48%→100%**).
  Still open: async-generator `.return()` finally, niche try `cptn-*`
  completion-value tests. (Function-param destructuring close came for free —
  params route through the same `bind_pattern`.) See `[[try-finally-nonlocal-gap]]`.
- **Class — mostly the big clusters remain.** Private-method *calls* now
  brand-check (`obj.#m()` on a non-instance throws `TypeError`: `PrivateGet` not
  `GetProp` in the call path) — a correct fix (smoke-verified), though Test262-
  neutral on its own since those tests also need the per-instance private-element
  model. Largest remaining class clusters: per-instance private-method/accessor
  brand model (~50; brand added at construction, not via the prototype), and
  derived-constructor `this`-TDZ (`this`/implicit-return before `super()` →
  `ReferenceError`, needs the spec construction model where `super()` *creates*
  `this`, ~40). `statements/class` ~92%, `expressions/class` ~95%.
- **Native-builtin subclassing** — `class X extends Set`/`Map`/`Uint8Array`
  now works: each native ctor's call-handler detects a `super()` invocation
  (this is a subclass instance) and adopts the exotic internal slot in place.
  Per-type hack; the general fix is the derived-ctor construction model (also
  clears the ~40 `this`-TDZ tests). See `[[native-subclass-super-pattern]]`.
- **ES2024 Set methods** — `difference`/`symmetricDifference` corrected to the
  spec protocol (size-based `has`-vs-`keys` branching; `symmetricDifference`
  never calls `other.has`). Set built-ins **64%→94%**. Remaining Set fails (~21)
  are `isSubset/isSuperset/isDisjoint` ordering edges + forEach receiver.
- **RegExp** — `RegExp.escape` (ES2025) and the `d`-flag `.indices` on match
  results now implemented (escape ~95%, match-indices 13→3 fail). Remaining:
  `\p{}` Unicode property escapes (needs Unicode property tables, ~440); the
  `prototype` long tail (diffuse Symbol.replace/receiver/`\u`-escape edges, ~60);
  one lone-surrogate escape case (blocked on UTF-16 string representation).
- **Array iteration-method hole semantics** (`forEach`/`map`/… `HasProperty`-gating; ~96).
- Resizable `ArrayBuffer` mid-operation detach/shrink edges; `Symbol.species` *usage* in
  builtin methods; deeper Promise/async ordering.
~Large, ongoing.

### G6 — Replay/journal parity for suspending host effects
**CLOSED** (see "What landed" above). The server resume suite exercises every
suspending host op under rust and is green, including repeated mid-prompt-loop
double-suspension and nested/suspending `call_agent`. Original write-up retained:

Confirm the journal-based replay covers every host op that can suspend (prompt tool-loop,
http, tool, sub-agent) with the same `try_replay` / `PendingHostOperationKind` semantics
the QuickJS path guarantees — including the effect-log nesting invariant on replay
(`[[effect-log-nesting-invariant]]`). Add record/replay parity tests per effect. Depends
on G1. ~Medium.

## Suggested sequencing

1. **G1** (pause/resume surfacing) — unblocks the durable story; testable in isolation.
2. **G6** (replay parity tests) — lock in correctness as G1 lands.
3. **G2** (snapshot persistence + server resume) — makes the server engine-agnostic.
4. **G4** (native nested execution) — removes the last `TypeScriptVmRuntime` dependency
   from the live path.
5. **G3** (`node:` builtins) — ✅ done; the agent-visible feature gap is closed.
6. **G5** (conformance) — the only remaining gate; runs in parallel throughout.
7. **Deletion**: drop the QuickJS branch in `engine.rs`, delete `snapshot.rs`'s C
   callbacks + `TypeScriptVmRuntime`, remove `chidori-quickjs{,-sys}` from the workspace
   and the `rquickjs` dev-dependency, and collapse `selected_engine()`/`EngineKind`.

## Decision: runtime default stays QuickJS until the G5 bar

The earlier flip attempt failed **54 tests**, all in engine pause/resume/snapshot
and server resume — exactly gates **G1**/**G2** (and the nested/suspension parts of
**G4**/**G6**). Those are now **closed**: the full suite is green with
`CHIDORI_JS_ENGINE=rust`, so the acceptance test for G1/G2 passes. **G3** is now
closed too (`node:` crypto/fs/timers run and replay on the rust path).

- **Compiled in by default** (`default = ["rust-engine"]`), **opt-in at runtime**
  (`CHIDORI_JS_ENGINE=rust`), and now **green under that opt-in** — committed state.
- The **runtime default flip** (`selected_engine()` → `Rust`) and the **QuickJS
  deletion** are gated only on agreement on the **G5** conformance bar.
- Re-run the flip as the standing acceptance test: `CHIDORI_JS_ENGINE` defaulting
  to `rust` must leave `cargo test` green. It does today across G1–G4 and G6.
