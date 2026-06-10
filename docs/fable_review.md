# Chidori v3 — Project Review

*An in-depth review of the repository: what works, what is limited, and what
is incomplete. Snapshot taken 2026-06-10 at commit `f93c2cd` ("runtime").*

## Scope and method

This review covers the whole repository: the Rust core runtime (`src/`), the
two embedded JavaScript engines (`crates/chidori-quickjs[-sys]` and
`crates/chidori-js`), the WASM sandbox crates (`sandbox-runtime/`,
`sandbox-python/`, `sandbox-js/`), the SDKs (`sdk/typescript`, `sdk/python`),
examples, tests, CI, and the existing design docs. Findings were verified
against the code (a fresh `cargo build` was attempted, CI history was checked
on GitHub) rather than taken from the docs alone.

**Code size at a glance** (lines, excluding vendored QuickJS C sources):

| Area | Approx. LOC | Notes |
|---|--:|---|
| `src/` (runtime, server, CLI, providers, tools) | ~42,000 | `server.rs` alone is ~8,800 |
| `crates/chidori-js` (pure-Rust JS engine) | ~32,000 | zero `unsafe`, oxc parser only |
| `crates/chidori-quickjs-sys` (vendored QuickJS fork) | ~80,000 | C sources + FFI |
| `crates/chidori-quickjs` (safe wrapper) | ~4,200 | single file |
| `crates/test262-runner` | ~1,300 | conformance harness |
| `sdk/` (TypeScript + Python) | ~1,500 | zero-dependency clients |

---

## Executive summary

Chidori v3 is an ambitious and unusually deep system: a single Rust binary
that runs TypeScript agents on an embedded JS engine, records every side
effect through a host boundary, and uses that journal for deterministic
replay, durable pause/resume, and human-in-the-loop workflows. The TypeScript
migration described in `TODO.md` is genuinely complete — every roadmap item is
checked off — and the docs are unusually honest about remaining gaps.

The headline findings of this review:

1. **The build is broken for fresh clones, and CI on `main` has been red
   since 2026-05-28.** `src/runtime/sandbox.rs` embeds three WASM binaries via
   `include_bytes!`, but those artifacts are not committed and neither the
   README's `cargo build` quick start nor `.github/workflows/ci.yml` runs the
   required `scripts/build-wasm.sh` first. The last six CI runs on `main` all
   concluded `failure`. This is the single most important thing to fix.
2. **The project carries three JavaScript engines at once** — the vendored
   QuickJS fork (production default), the pure-Rust `chidori-js` engine
   (opt-in, 91.69% Test262), and Boa compiled to WASM (for the `execJs`
   sandbox). This is a deliberate migration state, but it is the project's
   largest ongoing maintenance burden, and the QuickJS removal is blocked on
   one remaining gate (G5, conformance).
3. **Everything else is in good shape but intentionally narrow**: two LLM
   providers, a blob-style persistence layer, capability-confinement (not
   OS-level) sandboxing, no npm package support, and an 8-module `node:`
   builtin allowlist. These are documented design choices rather than bugs,
   but they bound what the runtime can be used for today.

---

## Critical issue: fresh builds fail and CI is red

A plain `cargo build` on a fresh checkout fails:

```
error: couldn't read `src/runtime/../../sandbox-runtime/target/wasm32-unknown-unknown/release/sandbox_runtime.wasm`: No such file or directory
error: couldn't read `src/runtime/../../sandbox-python/target/wasm32-wasip1/release/sandbox-python.wasm`: No such file or directory
error: couldn't read `src/runtime/../../sandbox-js/target/wasm32-wasip1/release/sandbox-js.wasm`: No such file or directory
```

The cause is the unconditional `include_bytes!` of prebuilt sandbox binaries
in `src/runtime/sandbox.rs` (lines ~466, ~592, ~1005). `scripts/build-wasm.sh`
exists and documents the prerequisite (it builds `sandbox-runtime` for
`wasm32-unknown-unknown` and `sandbox-python`/`sandbox-js` for
`wasm32-wasip1`), but:

- The README quick start (`README.md:74`, `:109`) says only `cargo build`.
- `.github/workflows/ci.yml` runs `cargo test --workspace` and `cargo build`
  without ever invoking `build-wasm.sh` or installing the wasm targets.
- The `.wasm` artifacts have never been committed (verified via
  `git log --all -- '*.wasm'`).

Consequently the `ci.yml` workflow has failed on every run of `main` since at
least `770bc53` (2026-05-28), including the most recent commit `f93c2cd`.
`scripts/publish.sh` would hit the same wall at its `cargo build --release`
step.

**Suggested fixes (any one of these unblocks):** make the sandbox embedding a
cargo feature with a stub fallback; commit the (small, deterministic) wasm
artifacts; generate them from `build.rs`; or simply add the
`rustup target add` + `scripts/build-wasm.sh` steps to CI and the README.
Until then, every "Quality Gates" claim in `TODO.md` ("Root Rust test suite
passes", "Add CI for cargo test --workspace") is true on a machine that has
run the script and silently false everywhere else.

---

## The three-engine situation

This is the project's defining in-flight migration.

| Engine | Where | Role | Status |
|---|---|---|---|
| QuickJS fork (C) | `crates/chidori-quickjs[-sys]` | **Production default** for agent execution and live-VM snapshots | Stable; ~99.5% of executed Test262 |
| `chidori-js` (pure Rust) | `crates/chidori-js` | Opt-in via `CHIDORI_JS_ENGINE=rust`; intended replacement | 91.69% of executed Test262 (36,490 pass / 3,307 fail / 7,468 skip, 2026-06-04) |
| Boa (WASM) | `sandbox-js/` → embedded blob | Powers the `chidori.execJs()` sandbox only | Frozen at boa_engine 0.21, default features off |

`docs/rust-engine-quickjs-removal-gaps.md` tracks the removal gates. G1
(pause/resume), G2 (durable snapshot/restore), G3 (`node:` builtins), G4
(native nested tool/sub-agent execution), and G6 (replay parity) are all
closed. **G5 — the conformance bar — is the only open gate**: the default
flip and QuickJS deletion wait on an agreed target (≥95% language +
builtins).

The practical cost of this state:

- Two implementations of the `SnapshotCapableJsEngine` surface must be kept
  in behavioral lockstep; the legacy `rquickjs` binding module is *also*
  retained for parity tests.
- ~80k lines of vendored C remain in the repo, with a dedicated
  `quickjs-fork.yml` workflow to keep the fork updated.
- Stale comments have already crept in: `crates/chidori-js/src/lib.rs:277`
  still says module imports are unsupported on the rust path, but G3/G4
  closure notes say relative imports and `node:` shims now work.

### Remaining `chidori-js` language gaps (from Test262 + docs)

Highest-value first, per `docs/rust-engine-quickjs-removal-gaps.md` and
`docs/pure-rust-js-engine-plan.md`:

| Gap | ~Tests | Notes |
|---|--:|---|
| RegExp `\p{…}` Unicode property escapes | ~440 | Unicode tables are generated (`unicode_tables.rs`); the property-name→range mapping is unfinished. Biggest single cluster; RegExp sits at ~54%. |
| Resizable `ArrayBuffer` mid-op edge cases | ~150 | detach/shrink-mid-operation checks; DataView-on-resizable. |
| Promise/async ordering depths | ~100 | Promise at ~55%; spec-detailed timing combinations. |
| Array iteration-method hole semantics | ~96 | `forEach`/`map`/etc. must `HasProperty`-gate holes; `copyWithin` still dense-only. |
| Per-instance private brand model | ~50 | Class private methods/accessors; brand checks on calls landed, instance tracking didn't. |
| Derived-constructor `this`-TDZ | ~40 | Engine pre-creates `this` instead of letting `super()` create it; needs the spec construction model. |
| Async-generator `.return()` + `finally` | ~40 | Sync generators fixed (GeneratorPrototype 48%→100%); async path pending. |

**Intentionally unsupported everywhere** (consistent with the deterministic
replay contract): `Intl`, `Temporal`, `Atomics`/`SharedArrayBuffer`,
`WeakRef`/`FinalizationRegistry`, `ShadowRealm`, decorators, iterator
helpers, dynamic module loading (dynamic `import()` returns a rejected
promise).

### Engine-internal limitations worth knowing

- **Reference-counting GC leaks cycles.** `Rc<RefCell<…>>` cannot reclaim
  ctor↔prototype and closure cycles; `Vm::dispose()` breaks known cycles
  manually (the conformance runner calls it per test — an earlier full run
  OOM'd a 64 GB machine before this landed). Long-lived VM reuse without
  `dispose()` will leak.
- **No value checkpointing yet** (deferred P6): resume cost equals full
  re-execution of the journal from the top, so very long histories get
  slower to resume. `durableStep(fn)` memoization exists as a partial
  mitigation.
- **Regex engine is a custom backtracker** with a 100k step budget — safe
  against ReDoS, but future TC39 regex features (v-flag, modifiers) all land
  on this team.

---

## Runtime and host API limitations

### Imports and the standard library

- Import policies are static: `None` / `Relative` / `Project` / `Node`
  (`Node` is the durable default so the VFS is reachable). **No dynamic
  imports, no npm packages, no JSX/TSX.** Node package compatibility is an
  explicit v1 non-goal (`DESIGN.md`).
- The `node:` builtin allowlist covers only 8 modules
  (`src/runtime/typescript/transpile.rs:57–66`): `process`, `buffer`, `util`,
  `fs`, `fs/promises`, `crypto`, `http`, `https`. Missing staples include
  `path`, `os`, `events`, `stream`, `url`, `assert`, `zlib`,
  `child_process`. Agents that look "node-like" will hit this wall quickly.
- `node:fs` is backed by an in-memory, snapshot-resident VFS — agents cannot
  read the host filesystem (a deliberate confinement property, but worth
  stating as a limitation: there is no opt-in host FS access either;
  `FsPolicy::Host` is rejected in durable runs).
- `node:crypto` is shimmed over synchronous host-backed hashing; timers are
  virtual (logical clock) or disabled — no real wall-clock timers in durable
  mode.
- Transpilation (oxc-based) strips types only; modern syntax is not
  downleveled. No JSDoc-to-schema extraction for tool metadata.

### LLM providers

- **Two real providers**: Anthropic and OpenAI (`src/providers/`), plus a
  catch-all LiteLLM-compatible provider (any model routes there when
  `LITELLM_API_URL` is set) and a `StaticProvider` for tests. No native
  Gemini, Bedrock, Vertex, Azure OpenAI, Mistral, or Ollama integrations —
  LiteLLM is the implicit escape hatch for all of them.
- The base `LlmProvider::stream()` (`src/providers/mod.rs:117–125`) falls
  back to a blocking `send()` and emits one synthetic delta; only Anthropic
  and OpenAI implement true streaming.
- No embeddings, no image/audio modalities, no structured-output/JSON-mode
  plumbing beyond tool calls.

### Sandboxed execution (`execJs` / `execPython` / `execWasm`)

- `execPython` is RustPython-on-WASI and `execJs` is Boa-on-WASI — both are
  *separate interpreters from the agent engine*, embedded as WASM blobs with
  a hand-rolled 18-function WASI preview-1 shim. They are metered (fuel) but
  feature-frozen at whatever those interpreter versions support.
- Raw `execWasm` arguments and returns are limited to numeric types
  (i32/i64/f32/f64); strings/objects must be marshaled via linear memory.
  There are no host→guest callbacks beyond a `host.log` string channel.
- `exec_expr`'s miniscript runtime caps source+vars at 16 KiB.

### Policy and sandbox model

`docs/sandbox-model.md` is admirably explicit that this is
**capability-confinement, not OS isolation**. The documented gaps are real
and worth restating:

1. **Powerful injected effects are mostly ungated** — only `http` passes
   through `enforce_policy`; `execPython`, `execWasm`, and `workspace.*`
   appear unconditional. Deny-by-default routing through the policy gate is
   the stated fix and has not landed.
2. **Memory accounting is process-wide, not per-VM**
   (`src/mem_guard.rs`): under concurrent agents, one run's allocations can
   be attributed to another. The cap is a backstop, not a per-tenant quota.
   Enforcement is also polled (~20 ms watchdog / every 256 ops), so brief
   overshoot is possible.
3. **No per-agent CPU quota** — the opcode budget bounds a run, not a
   tenant.
4. **No seccomp/namespace/process isolation** — the engine runs in-process;
   Rust memory safety plus capability injection are the only boundaries.
   Not suitable for genuinely hostile code without an outer container.
5. Policy `match_args` matching is shallow (contains-check for objects,
   equality for arrays — `src/policy.rs:98–105`); no regex or nested
   queries. Approvals are cached per-session only, and policy cannot change
   mid-run.

### Persistence and scale

- Storage (`src/storage.rs`) is **JSON files** (default,
  `.chidori/runs/…`) or **SQLite** (opt-in via `CHIDORI_DB_PATH`), both
  storing sessions as opaque JSON blobs. No field-level queries, no
  migrations between backends, no multi-node story. Fine for a single-node
  dev/prod box; not a fleet substrate.
- Concurrency is bounded by a per-run tokio semaphore in the server; there
  is no distributed scheduling or remote execution.
- Full QuickJS `JSModule` graph serialization is deferred — snapshots use a
  "selected roots + bundled module scaffold" model. If snapshot bundle
  creation fails, the engine **silently falls back to replay**
  (`src/runtime/engine.rs:109–116`) rather than surfacing an error.

### Server and protocol surface

- `src/server.rs` is ~8,800 lines — session API, SSE streaming, live-VM
  resume, and replay-fallback logic in one module. It works, but it's the
  file most in need of decomposition; the same goes for
  `src/runtime/typescript/snapshot.rs` (~7,200 lines).
- ACP (Agent Client Protocol, `src/acp.rs`) is self-described as a minimal
  subset: create thread, send prompt, list/get threads. Streaming,
  tool-approval flows, and richer session management are not covered.
- MCP support exists (`src/mcp/`) but shares the same "minimal viable
  surface" character.

---

## SDKs, examples, tests, CI

### SDKs — complete but unpublished

Both SDKs are zero-dependency, mirror each other method-for-method, and
cover the full host/session API (run, replay, resume, checkpoint, stream,
snapshot manifests). Two gaps:

- `scripts/publish.sh` publishes only the Rust crates
  (`chidori-quickjs-sys`, `chidori-quickjs`, `chidori`). **There is no npm or
  PyPI publish automation** despite both SDKs being at version 3.0.0 and the
  README badging npm/PyPI.
- Both SDK READMEs state that checkpoint resume goes through call-log
  replay because "direct live VM continuation … is still gated on the
  QuickJS serializer," while `docs/typescript-migration-audit.md` says
  direct live-VM continuation **is** implemented for production paths. One
  of these is stale; reconcile them.

### Tests

- Good: engine unit/integration tests (`crates/chidori-js/tests/`), QuickJS
  wrapper snapshot-ABI tests, CLI integration tests
  (`tests/cli_typescript.rs`, 10 cases), and 8 Python-SDK integration tests
  that exercise a real server (sessions, auth, CORS, concurrency,
  pause/resume).
- Missing: **any TypeScript SDK tests** (CI only typechecks and builds it);
  meaningful integration coverage for the three `exec*` sandboxes; Test262
  in CI (conformance is measured locally only, so the 91.69% number can rot
  silently); MCP/ACP protocol tests.
- And, per the critical issue above, the Rust jobs in CI cannot currently
  pass at all.

### Examples and docs

- Examples are in good shape: ~20 agents plus a dedicated
  `examples/record-replay/` set demonstrating determinism, retries, human
  approval, and exactly-once semantics. None reference unimplemented APIs.
- The docs set is a genuine strength — `sandbox-model.md`,
  `pure-rust-js-engine-plan.md`, and `rust-engine-quickjs-removal-gaps.md`
  document their own gaps candidly. The main doc debt is *staleness drift*
  between fast-moving reality and narrative docs (SDK READMEs vs migration
  audit; `lib.rs:277`; README quick start vs the wasm prebuild requirement).
- The three `sandbox-*` directories are intentionally outside the cargo
  workspace (each is its own workspace so it can target wasm/no_std), but
  nothing in the README explains this — they look vestigial until you find
  `scripts/build-wasm.sh`.

---

## What is genuinely done

To keep the limitations in perspective, the following are implemented and
verified by the migration audit and test suite (modulo the CI build issue):

- TypeScript-only agent authoring with runtime transpilation; the full
  `chidori.*` host API (prompt, input, tool, callAgent, parallel, retry,
  tryCall, http, template, log, memory, checkpoint, execJs/Python/Wasm,
  workspace).
- Deterministic call-log record/replay with zero-LLM-call replays.
- Durable pause/resume across `input()`, policy approval, and host calls —
  including direct live-VM resume on the QuickJS path with replay as an
  explicit fallback, and nested tool/sub-agent suspension.
- CLI (`check`, `run`, `serve`, `trace`, `stats`, `demo`, snapshot
  inspection), HTTP session API with SSE streaming, OTEL trace emission.
- Test262 conformance harness runnable against both engines.

---

## Prioritized recommendations

1. **Fix the build/CI break** (critical): add the wasm prebuild to CI and
   the README, or make the sandbox blobs a feature/build-script concern.
   Nothing else on this list is verifiable until CI is green again.
2. **Finish G5 and delete an engine.** The four highest-value conformance
   clusters (array holes ~96, async-gen finally ~40, resizable ArrayBuffer
   ~150, RegExp `\p{}` ~440) plausibly reach the ~95% bar; flipping the
   default and removing the QuickJS fork + `rquickjs` parity path would
   eliminate ~84k lines and the dual-maintenance tax.
3. **Gate the powerful effects** (`execPython`, `execWasm`, `workspace.*`)
   through the policy layer, deny-by-default for untrusted profiles —
   sandbox-model gap #1 and the most security-relevant single change.
4. **Run Test262 in CI** (even a curated subset) so the conformance number
   is continuously true, and add TypeScript SDK tests.
5. **Automate SDK publishing** to npm/PyPI in `publish.sh`/CI, or remove the
   registry badges until then.
6. **Reconcile stale docs**: SDK READMEs vs `typescript-migration-audit.md`
   on live-VM resume; `chidori-js/src/lib.rs:277`; README build
   instructions; a short note on why `sandbox-*` are separate workspaces.
7. **Decompose `server.rs` and `typescript/snapshot.rs`** before they grow
   further; both are load-bearing and effectively unreviewable as single
   files.
8. Longer-term, as adoption demands: per-VM memory accounting, value
   checkpointing for long journals (P6), a broader `node:` allowlist
   (`path`, `events`, `url` are cheap wins), more native providers or
   first-class embeddings, and a queryable storage schema.

---

## Bottom line

Chidori v3 is a coherent, well-documented system with one foot still in a
major engine migration. Its limitations split cleanly into three kinds:
**deliberate scope cuts** (no npm, no OS sandbox, no Intl/Temporal, blob
storage) that are defensible and documented; **migration residue** (three JS
engines, G5 conformance gate, stale doc drift) that has a clear finish line;
and **one operational regression** (the wasm prebuild / red CI) that
contradicts the project's own quality gates and should be fixed first.
