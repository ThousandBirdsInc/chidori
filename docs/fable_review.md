# Chidori v3 — Project Review

*An in-depth review of the repository: what works, what is limited, and what
is incomplete. Originally taken 2026-06-10 at commit `f93c2cd` ("runtime");
updated 2026-06-11 after the QuickJS removal (#39), the CI fixes (#40), the
Test262 CI gate (#41), the conformance work in #42, and the
derived-constructor / arguments / restricted-property / explicit-resource
rework in #43; the parallel Test262 runner (#44) and the workspace policy
gate (#45). Doc-drift pass on 2026-06-12 (#46), followed by the secret
broker (#47) and the node-shim batch + built-in `untrusted` policy profile
(#48); a `--untrusted` CLI flag landed on this branch. All of #39–#48 are
merged into the branch history.*

> **Historical snapshot (~2026-06-10/12).** This document is kept as a dated
> record of repo state at that time. Its conformance numbers, LOC counts, and
> bare `src/` paths predate the move of the main crate into `crates/chidori/`
> and later conformance work; for the current conformance table see
> [`docs/conformance.md`](./conformance.md).

> **Addendum (2026-06-12, conformance campaign):** after this review was
> taken, a focused sweep drove Test262 conformance from 96.22 % to
> **98.10 %** (39,017 pass / 757 fail / 7,517 skip) — recommendation 2's
> "keep picking off the cluster table" executed at scale. The Array,
> TypedArray/DataView (resizable-ArrayBuffer), Promise, Set/Map, JSON,
> generator (`yield*` return delegation), class-field, statement-
> completion-value, mapped-`arguments`, and module-linking clusters were
> substantially or fully cleared, with zero regressions across seven
> gated batches. Conformance numbers quoted below are the 2026-06-12
> snapshot this review measured; see `docs/conformance.md` for the
> current table.

## Scope and method

This review covers the whole repository: the Rust core runtime (`src/`), the
pure-Rust JavaScript engine (`crates/chidori-js`), the Test262 harness
(`crates/test262-runner`), the SDKs (`sdk/typescript`, `sdk/python`),
examples, tests, CI, and the existing design docs. Findings were verified
against the code and the CI history on GitHub rather than taken from the
docs alone.

**Code size at a glance** (lines of Rust unless noted):

| Area | Approx. LOC | Notes |
|---|--:|---|
| `src/` (runtime, server, CLI, providers, tools) | ~22,600 | `server.rs` is ~3,100 (was ~8,800 pre-#39) |
| `crates/chidori-js` (pure-Rust JS engine) | ~57,000 | zero `unsafe`, oxc parser; ~26k of this is generated Unicode tables |
| `crates/test262-runner` | ~1,200 | conformance harness + baseline gate |
| `sdk/` (TypeScript + Python) | ~1,500 | zero-dependency clients |

The previous snapshot carried ~84,000 additional lines of vendored QuickJS C
and WASM-sandbox code; #39 deleted all of it (−112,897 lines in one commit).

---

## Executive summary

Chidori v3 is a single Rust binary that runs TypeScript agents on an embedded
pure-Rust JS engine, records every side effect through a host boundary, and
uses that journal for deterministic replay, durable pause/resume, and
human-in-the-loop workflows.

Since the 2026-06-10 snapshot, the two headline problems identified by this
review have been **fixed**:

1. **The build/CI break is gone.** The broken `include_bytes!` of
   never-committed WASM sandbox blobs disappeared along with the sandboxes
   themselves. `cargo build` works on a fresh clone again, and both the `CI`
   and `Test262 conformance` workflows are green on `main` (first green run:
   `c964aea`, 2026-06-10).
2. **The three-engine situation is resolved.** The vendored QuickJS fork,
   the `rquickjs` parity path, and the Boa/RustPython/WASM exec sandboxes are
   deleted. `chidori-js` is now the **only** JavaScript engine in the tree,
   for agent, tool, and sub-agent execution alike.

The flip side of the consolidation: conformance stopped being a migration
gate ("G5") and became **load-bearing** — a language bug in `chidori-js` now
breaks real agents with no fallback engine. The project responded correctly:
Test262 runs in CI against a committed per-test baseline (PRs touching the
engine, pushes to `main`, and a nightly schedule), so the number can no
longer rot silently. Conformance is **96.22 %** of executed tests at the
pinned suite commit (38,271 pass / 1,503 fail / 7,517 skip), after the
engine work in #43 — the derived-constructor construction model,
a correctness batch (arguments object, function-name bindings, restricted
properties, `__proto__` literals, `delete` semantics), and a full
implementation of explicit resource management (`using`/`await using`) —
cleared ~400 baseline failures with zero regressions.

What remains is intentional narrowness, not breakage: two LLM providers, a
blob-style persistence layer, capability-confinement (not OS-level)
sandboxing, no npm packages, an 8-module `node:` allowlist — documented
design choices that bound what the runtime can be used for today. The main
*new* debt is documentation drift: a body of docs still describes the
QuickJS-era architecture.

---

## The engine: one runtime, conformance now load-bearing

| Engine | Where | Role | Status |
|---|---|---|---|
| `chidori-js` (pure Rust) | `crates/chidori-js` | **The** engine — agents, tools, sub-agents, Test262 | 96.22 % of executed Test262 (38,271 / 1,503 / 7,517 pass/fail/skip at the pinned commit) |

`docs/conformance.md` describes the measurement methodology (bare-context,
fresh VM per variant, honest skip accounting) and the CI gate
(`.github/workflows/test262.yml` + `scripts/test262.sh --gate` against
`crates/test262-runner/test262-expectations.json`). Regressions fail the
build; new passes print a baseline-refresh hint.

### Recent engine work

- **#42**: dynamic `import()` via a `Vm::dynamic_import` host hook (the
  production runtime still forbids it by policy — the *engine* supports it,
  the *durability contract* rejects it), `with`-scope reference/closure
  semantics, spec-order member writes, `export default` self-reference
  fixes.
- **#43** (first batch): the spec construction model for derived classes —
  `super()` now performs a real `Construct(parent, args, new.target)` so
  `class A extends Array` (or `Map`, `Error`, …) produces a genuine exotic
  instance; `this` is in TDZ until `super()` returns (ReferenceError before,
  "may only be called once" after); instance fields/private brands install
  when `super()` returns; derived `return` follows the object /
  undefined-becomes-this / else-TypeError rules **at frame exit** (so a
  `super()` inside `finally` is honored); native constructors honor
  `new.target` for the instance prototype (`Reflect.construct(Array, [],
  Other)` included); class constructors throw when invoked without `new`;
  `extends null` and `extends Symbol` behave per spec. This cleared 144
  baseline failures (1,911 → 1,767) with zero regressions across the full
  suite — the bulk of what had been a ~300-test `class` cluster.
- **#43** (a second batch, −194 failures → 1,571): named
  function expressions and classes bind their own names per spec (immutable,
  TDZ for class heritage); the `arguments` object is a real exotic
  Arguments object (length/callee/@@iterator/tag) instead of a plain array;
  the %ThrowTypeError% intrinsic poisons `Function.prototype.caller` /
  `arguments`; object-literal `__proto__` sets the prototype;
  computed-key anonymous functions get spec SetFunctionName; `delete` on
  identifiers follows the spec (strict SyntaxError, binding/global
  configurability); %Object.prototype% is an immutable-prototype exotic
  object; eval-created globals are deletable, script-level ones are not.
- **#43 — explicit resource management** (a third batch, −68 → 1,503): `using` /
  `await using` declarations now dispose per spec — resources recorded
  before the binding initializes, disposed in reverse on EVERY exit path
  (throw/return/break/continue included) via a finally-style landing pad,
  dispose errors chained through SuppressedError, async disposal genuinely
  awaited, and `for (using x of …)` disposing per iteration. The entire
  66-test cluster passes.

### Remaining language gaps (top clusters of the baseline's 1,503 failures)

| count | area | nature |
|--:|---|---|
| 303 | `language/expressions` | class element corners, dynamic-`import()` semantics, `yield*` delegation ordering |
| 222 | `language/statements` | remaining class element corners, `for-of` iterator-close |
| 136 | `built-ins/Array` | species/proxy interplay, length-boundary semantics |
| 98 | `built-ins/RegExp` | lone-surrogate matching (needs UTF-16 strings); `v`-flag; `prototype` long tail |
| 96 | `built-ins/TypedArray` | resizable-`ArrayBuffer` / out-of-bounds tracking |
| 59 | `built-ins/String` | `normalize`, Unicode/surrogate edge cases |
| 52 | `built-ins/Promise` | spec-detailed async ordering combinations |
| 51 | `language/module-code` | TLA ordering, cyclic-graph corner cases |
| 23 | `language/arguments-object` | mapped-arguments index/parameter aliasing |

**Intentionally unsupported** (consistent with the deterministic replay
contract, skipped honestly in the runner): `Intl`, `Temporal`,
`Atomics`/`SharedArrayBuffer`, `WeakRef`/`FinalizationRegistry`,
`ShadowRealm`, decorators, iterator helpers.

### Engine-internal notes

- **GC**: reference counting plus a real cycle collector (`gc.rs`) — every
  allocation registers per-VM, `Vm::dispose()` breaks the outgoing edges of
  everything the VM allocated, `Vm::collect_cycles()` offers mark-sweep for
  long-lived VMs. The earlier "full Test262 run OOMs a 64 GB machine"
  problem is gone (~20 MB flat RSS over the 21k `language/` tests); the
  suite is still chunked per directory in CI, but for crash isolation, not
  memory.
- **Regex** is a custom backtracker with a 100k-step budget — ReDoS-safe,
  but future TC39 regex features all land on this team.
- `lib.rs`'s single-file `run_entrypoint` helper still carries a "module
  imports are not supported on the rust engine path yet" error string —
  stale phrasing (there is no other path; the real runtime resolves imports
  through `typescript/module_graph.rs`).

---

## Runtime and host API limitations

### Imports and the standard library

- Import policies are static: `None` / `Relative` / `Project` / `Node`
  (`Node` is the durable default so the VFS is reachable). **No dynamic
  imports (by policy), no npm packages, no JSX/TSX.** Node package
  compatibility is an explicit v1 non-goal (`DESIGN.md`).
- The `node:` builtin allowlist (`src/runtime/typescript/transpile.rs`)
  covers `process`, `buffer`, `util`, `fs`, `fs/promises`, `crypto`,
  `http`, `https`, plus (added 2026-06-12) `path` (and `path/posix`),
  `events`, `url`, `assert` (and `assert/strict`), and `os` (fixed
  virtualized constants, in the same spirit as `process.platform`).
  Missing staples now: `stream`, `zlib`, `child_process`. The "node-like
  agents hit this wall quickly" complaint is substantially blunted.
- `node:fs` is backed by an in-memory, snapshot-resident VFS — agents cannot
  read the host filesystem (deliberate confinement; there is no opt-in host
  FS access either — `FsPolicy::Host` is rejected in durable runs).
- Timers are virtual (logical clock) or disabled — no real wall-clock timers
  in durable mode.
- Transpilation (oxc-based) strips types only; modern syntax is not
  downleveled.

### Execution surface (narrowed by #39)

The `execJs` / `execPython` / `execWasm` / `exec_expr` sandboxes are
**removed**, along with their WASM interpreter blobs and the hand-rolled
WASI shim. This deleted both a feature (polyglot snippet execution) and an
attack/maintenance surface; agents now execute TypeScript only. Anyone
relying on `chidori.execPython(...)` has no replacement. (The
`execJs`/`execPython`/`execWasm` JS *stubs* still exist in
`crates/chidori-js/src/lib.rs`, but they are inert — the host backend
rejects the effect with `chidori.<name> is not supported on the rust engine`,
so there is no path back to snippet execution.)

### LLM providers

- **Two real providers**: Anthropic and OpenAI (`src/providers/`), plus a
  catch-all LiteLLM-compatible provider (any model routes there when
  `LITELLM_API_URL` is set) and a `StaticProvider` for tests. No native
  Gemini, Bedrock, Vertex, Azure OpenAI, Mistral, or Ollama — LiteLLM is the
  implicit escape hatch.
- The base `LlmProvider::stream()` falls back to a blocking `send()` with
  one synthetic delta; only Anthropic and OpenAI stream for real.
- No embeddings, no image/audio modalities, no structured-output/JSON-mode
  plumbing beyond tool calls.

### Policy and sandbox model

Still **capability-confinement, not OS isolation** (`docs/sandbox-model.md`
— note that doc still describes the rust engine as opt-in; see doc drift).
The real gaps, restated post-#39:

1. **Powerful effects now route through `enforce_policy`, but the default is
   still allow.** `http` and every `workspace.*` action (`workspace:list` /
   `read` / `write` / `delete` / `manifest`) pass through the policy gate, so a
   restrictive profile can deny or gate disk writes while allowing reads. The
   `exec*` family is gone. What remains is the *default decision*: the fallback
   is still `AlwaysAllow`. Deny-by-default is now a one-switch opt-in — the
   built-in `untrusted` profile (#48), selectable via
   `CHIDORI_POLICY_PROFILE=untrusted` or the `--untrusted` flag on
   `chidori run` / `chidori serve` (the flag wins over all `CHIDORI_POLICY*`
   env vars) — but it is still opt-in, not automatic. A `supervised`
   sibling (ask-by-default, settled through the server's `/approve` flow)
   and per-session profile selection over the HTTP API (stricter-wins
   layering on the server policy) landed on this branch. *(Update
   2026-06-12: resolved for the network surface — `chidori serve` is now
   deny-by-default when no `CHIDORI_POLICY*` configuration is present, with
   a `--trusted` opt-out; `chidori run` deliberately keeps the permissive
   default.)*
2. **Memory accounting is process-wide, not per-VM** (`src/mem_guard.rs`):
   under concurrent agents one run's allocations can be attributed to
   another; enforcement is polled, so brief overshoot is possible.
   *(Update 2026-06-12: each run now registers a per-run meter on its
   execution thread, so concurrent runs no longer trip each other's caps;
   the watchdog polls every 10 ms, tunable via `CHIDORI_JS_MEM_POLL_MS`.
   Residual drift: attribution is by thread, not ownership.)*
3. **No per-agent CPU quota** — the opcode budget bounds a run, not a
   tenant.
4. **No seccomp/namespace/process isolation** — though "zero `unsafe`, no C"
   is now true of the entire engine, which is a categorically better story
   than the vendored-C era. Still not suitable for genuinely hostile code
   without an outer container.
5. Policy `match_args` matching is shallow; approvals are cached per-session
   only; policy cannot change mid-run.

### Persistence, resume, and scale

- Storage is **JSON files** (default) or **SQLite** (opt-in), both storing
  sessions as opaque blobs. No field-level queries, no migrations, no
  multi-node story.
- **Resume is always call-log replay now.** The QuickJS live-VM
  snapshot/resume machinery was deleted in #39, which *simplified* the
  architecture (the silent snapshot→replay fallback this review previously
  flagged is gone — replay is the only, explicit mechanism) at the cost of
  the original performance idea. ~~With **no value checkpointing** (the
  deferred P6), resume cost equals re-execution of the journal from the
  top, so very long histories get slower to resume; `durableStep(fn)`
  memoization is the partial mitigation.~~ **Update (2026-06-12): P6 landed
  as `chidori.step(name, fn)`** — pure deterministic compute is memoized
  into the call log and skipped on replay/resume, with the pure-compute
  contract enforced loudly (see `docs/value-checkpoints.md`). Host effects
  were already journal-served; un-wrapped pure JS between effects remains
  the only re-executed cost.
- Concurrency is bounded by a per-run tokio semaphore in the server; no
  distributed scheduling.

### Server and protocol surface

- `src/server.rs` is ~3,100 lines post-#39 — the earlier "8,800-line
  unreviewable module" complaint is resolved by deletion rather than
  decomposition, and the file is now reasonable.
- ACP (`src/acp.rs`) remains a minimal subset: create thread, send prompt,
  list/get threads. MCP support (`src/mcp/`) is similarly minimal-viable.

---

## SDKs, examples, tests, CI

### CI — green, with a conformance gate

`.github/workflows/ci.yml` (build, tests, formatting, TS SDK typecheck) and
`.github/workflows/test262.yml` (baseline-gated conformance, also nightly)
both pass on `main`. The "Quality Gates" in `TODO.md` are finally true on a
fresh clone.

### SDKs — complete but unpublished, with stale READMEs

Both SDKs are zero-dependency and mirror each other. Gaps:

- ~~**No npm or PyPI publish automation** despite both SDKs being at 3.0.0 and
  the README badging npm/PyPI.~~ *(Update 2026-06-21: resolved —
  `.github/workflows/release.yml` publishes the TypeScript SDK to npm and the
  Python SDK to PyPI via OIDC trusted publishing on tag push, alongside the
  crates.io and binary jobs.)*
- Both SDK READMEs say checkpoint resume goes through call-log replay
  because "direct live VM continuation … is still gated on the QuickJS
  serializer". That was stale before; it is wrong in a new way now — live-VM
  continuation isn't gated, it's *removed*. Replay is the design, not a
  stopgap; the READMEs should say so.

### Tests

- Good: engine unit/integration tests (`crates/chidori-js/tests/`), CLI
  integration tests, 8 Python-SDK integration tests against a real server,
  and Test262 in CI.
- ~~Missing: **any TypeScript SDK tests** (CI only typechecks and builds
  it)~~ *(Update 2026-06-21: added — `sdk/typescript/test/` drives the HTTP
  client against a `node:http` mock via Node's built-in test runner, wired
  into CI as a `npm test` step)*; MCP/ACP protocol tests remain absent.

### Examples and docs — the doc-drift list

Examples (~20 agents plus `examples/record-replay/`) remain in good shape.
The docs *were* the main debt: a body of them still described the QuickJS era.
**Update (2026-06-12): this list is now resolved** — see recommendation 6.
The original items, for the record:

- ~~`README.md` (~lines 358–410): says agents run on "an embedded QuickJS
  runtime", cites the dead 99.5 % QuickJS number, and describes
  `chidori-js` as the "younger" alternative path.~~ Rewritten: `chidori-js`
  is now described as the sole engine at 96.22 %, with no QuickJS framing,
  no `--engine rust` example, and a current cluster-table of remaining gaps.
- ~~`docs/sandbox-model.md`: frames the rust engine as opt-in via a
  `rust-engine` cargo feature (removed in #39).~~ Preamble rewritten; the
  `CHIDORI_JS_ENGINE`/`rust-engine`/"default QuickJS path" framing, the
  `exec*` capability references, and the stale "~91 %" number are gone.
- ~~`docs/rust-engine-quickjs-removal-gaps.md` and
  `docs/pure-rust-js-engine-plan.md`: the migration they track is done;
  they should be marked historical.~~ Both now carry a "historical —
  superseded" banner up top.
- ~~`scripts/conformance.sh`: still advertises `ENGINE=rust|quickjs`.~~
  Already gone — the script wraps `test262-runner --state` with no engine knob.
- ~~SDK READMEs (above), and `crates/chidori-js/src/lib.rs` ~420 ("rust
  engine path yet").~~ Both SDK READMEs no longer claim resume is "gated on
  the QuickJS serializer"; the `lib.rs` string already read "not supported in
  single-file entrypoints". The stale "QuickJS path / `--features rust-engine`"
  comments in `src/runtime/engine.rs` were corrected too.

---

## What is genuinely done

- TypeScript-only agent authoring with runtime transpilation; the
  `chidori.*` host API (prompt, input, tool, callAgent, parallel, retry,
  tryCall, template, log, memory, checkpoint, workspace).
- Deterministic call-log record/replay with zero-LLM-call replays; durable
  pause/resume across `input()`, policy approval, and host calls, including
  nested tool/sub-agent suspension — all on the single pure-Rust engine.
- CLI (`check`, `run`, `serve`, `trace`, `stats`, `demo`), HTTP session API
  with SSE streaming, OTEL trace emission.
- A conformance story with teeth: pinned-suite Test262, committed per-test
  baseline, CI gate, nightly run.

---

## Prioritized recommendations

1. ~~Fix the build/CI break~~ — **done** (#39/#40).
2. ~~Finish G5 and delete an engine~~ — **done** (#39); conformance is now a
   permanent quality bar, not a gate. Keep picking off the cluster table
   above (Array species, RegExp `v`-flag/UTF-16, resizable ArrayBuffer,
   Promise ordering are the next four).
3. ~~Gate the remaining powerful effects (`workspace.*`) through the policy
   layer~~ — **done**: every `workspace.*` action now routes through
   `enforce_policy` (targets `workspace:list` / `read` / `write` / `delete` /
   `manifest`), joining `http`. ~~Shipping a ready-made untrusted profile is
   the next step.~~ — **done** (#48 + this branch): the built-in `untrusted`
   profile (deny-by-default fallback, read-only workspace allowlist) ships
   behind `CHIDORI_POLICY_PROFILE=untrusted` and an `--untrusted` flag on
   `chidori run` / `chidori serve`; the flag takes precedence over all
   `CHIDORI_POLICY*` env vars, with CLI integration tests covering denial,
   the read-only allowlist, and flag-over-env precedence. Two follow-ups
   from this review also landed on this branch: a **`supervised`** profile
   (same allowlist, `AskBefore` fallback — gated calls suspend as
   `awaiting_approval` and settle through `/approve` instead of failing)
   and **per-session policy selection** over the HTTP API (`policy_profile`
   on `POST /sessions` / `/sessions/stream`, persisted on the session and
   re-applied across resume/approve/replay, exposed in both SDKs). Session
   profiles layer on the server policy with stricter-wins semantics, so a
   caller can tighten but never relax the operator's policy. ~~The default
   profile remains `AlwaysAllow` — making `untrusted` automatic for
   untrusted callers is the remaining (design-level) follow-up.~~ — **done**
   (2026-06-12): `chidori serve` is deny-by-default when the operator has
   configured no `CHIDORI_POLICY*` source (malformed configuration fails
   closed), with `--trusted` as the explicit opt-out; `chidori run` keeps
   the permissive default for local developer-authored code.
4. ~~Run Test262 in CI~~ — **done** (#41). ~~Add TypeScript SDK tests next.~~
   — **done** (2026-06-21): `sdk/typescript/test/` runs on Node's built-in
   test runner and is gated in CI.
5. ~~**Automate SDK publishing** to npm/PyPI, or remove the registry badges.~~
   — **done**: `.github/workflows/release.yml` publishes both SDKs (npm + PyPI
   via OIDC trusted publishing) on tag.
6. ~~**Pay down the doc drift** (the list above): README engine section, SDK
   READMEs, sandbox-model preamble, archive the two migration docs, the
   conformance.sh `ENGINE` knob.~~ — **done**: the README engine/conformance
   section now describes `chidori-js` as the sole engine at 96.22 % (no QuickJS,
   no `--engine rust`); `docs/sandbox-model.md`'s preamble drops the
   `CHIDORI_JS_ENGINE`/`rust-engine` framing and the `exec*` references; the two
   migration docs carry a "historical — superseded" banner; both SDK READMEs no
   longer claim resume is "gated on the QuickJS serializer"; and the stale
   QuickJS-path comments in `src/runtime/engine.rs` are corrected.
7. Longer-term, as adoption demands: per-VM memory accounting, ~~value
   checkpointing for long journals (P6)~~ (done 2026-06-12:
   `chidori.step(name, fn)`, `docs/value-checkpoints.md`), a broader
   `node:` allowlist (~~`path`, `events`, `url` are cheap wins~~ — done
   2026-06-12, along with `assert` and a virtualized `os`; `stream`/`zlib`
   remain), more native providers or first-class embeddings, and a
   queryable storage schema.

---

## Bottom line

The 2026-06-10 review closed on "one foot still in a major engine
migration." That foot has landed: the migration finished by deletion, CI is
green, and conformance is continuously measured against a committed
baseline. The remaining limitations now split into two kinds: **deliberate
scope cuts** (no npm, no OS sandbox, no Intl/Temporal, blob storage,
replay-only resume) that are defensible and documented, and **follow-through
work** (workspace policy gating, SDK publishing, TS SDK tests, doc drift,
the conformance cluster table) that has a clear finish line.
