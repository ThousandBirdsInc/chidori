# TODO - Chidori TypeScript Runtime

This roadmap tracks the TypeScript runtime. Historical Starlark work is
preserved in git history and under `examples/legacy-starlark/`; new runtime,
CLI, server, and tool work targets `.ts` agents and tools.

The TypeScript-runtime migration and the JavaScript-engine consolidation are
**complete**. Agents, tools, sub-agents, and the conformance harness all run on
the single in-tree pure-Rust engine `crates/chidori-js`; the vendored QuickJS
fork, the `rquickjs` parity path, and the WASM/Python exec sandboxes were
removed (#39). Durability is the deterministic-replay journal, not VM-image
snapshots — so the bulk of the remaining work is now the standing conformance
bar plus product follow-through, not migration.

## Status At A Glance

| Area | Status |
| --- | --- |
| TypeScript agent/tool/sub-agent dispatch | Done |
| Single pure-Rust JS engine (`chidori-js`), QuickJS removed | Done (#39) |
| Language-neutral host core | Done |
| Captured base effects: networking (`fetch`/`node:http`), `node:fs` VFS, crypto, timers | Done |
| Call-log replay + durable pause/resume (`input()`, policy approval, suspending host calls) | Done |
| Value checkpoints (`chidori.step`) | Done — `docs/value-checkpoints.md` |
| Multiplayer signals (`signal`/`pollSignal`/`signalAny`, `POST /sessions/{id}/signal`, live delivery) | Done — `docs/signals.md` |
| Context management (prompt caching, `chidori.context`, `Context.compact()`, local prompt cache) | Done — `docs/context-management.md` |
| In-agent branching (`chidori.branch`, branch stores, resume/rerun, capped waves) | Done (Phases 1–2) — `docs/branching-execution.md` |
| Policy profiles (`untrusted`/`supervised`, `--untrusted`/`--trusted`, per-session) + deny-by-default `serve` | Done |
| Test262 conformance with committed-baseline CI gate | Done; bar is now load-bearing (no fallback engine) |
| TypeScript and Python SDK parity (run/replay/resume/checkpoint/stream) | Done |

See [DESIGN.md](./DESIGN.md) for the top-level design,
[docs/conformance.md](./docs/conformance.md) for the conformance methodology and
gate, [docs/sandbox-model.md](./docs/sandbox-model.md) for the confinement model
and its gaps, and [docs/fable_review.md](./docs/fable_review.md) for the most
recent full-repository review and prioritized recommendations.

## Completed Work (summary)

Detailed, dated history lives in git and in the per-feature docs. At a high
level:

- **TypeScript runtime** — `.ts`-only agent/tool dispatch, runtime oxc
  transpilation (type-stripping), deterministic `Date`/`Math.random`, durable
  import/date/random/fs/crypto/timer policy, JSON host-boundary validation.
- **Host API** — `prompt`, `input`, `signal`/`pollSignal`/`signalAny`,
  `callAgent`, `tool`, `parallel`, `branch`, `retry`, `tryCall`, `step`,
  `context`/`Context.compact`, `template`, `log`, `memory`, `checkpoint`,
  `workspace`; captured `fetch`/`node:http` networking replacing `chidori.http`.
- **Engine consolidation (#39)** — moved all execution off the QuickJS fork /
  `rquickjs` onto `crates/chidori-js`, deleted the vendored C, the WASM/Python
  exec sandboxes, and the `chidori-quickjs{,-sys}` crates; replaced VM-image
  snapshot/resume with the deterministic-replay journal.
- **Conformance** — `crates/test262-runner` against the pinned Test262 corpus,
  a committed per-test baseline, and a CI gate that fails on regressions and
  hints on new passes (PRs touching the engine, pushes to `main`, nightly).
- **CLI / server / SDKs** — `check`, `run`, `serve`, `trace`, `stats`, `demo`,
  `branches`/`branch-resume`/`branch-rerun`; session create/list/get/checkpoint/
  replay/resume APIs with SSE streaming; OTEL trace emission; zero-dependency
  TypeScript and Python HTTP clients with snapshot-manifest types.

## Remaining Work

### Conformance (standing bar)

Test262 conformance is load-bearing now that there is no fallback engine. Keep
picking off the cluster table in `docs/conformance.md`. The current top
clusters:

- [x] **UTF-16 string representation** — `JsString` is now WTF-8-backed with
  full UTF-16 code-unit semantics (`docs/conformance.md`): `.length`/indexing/
  iteration, all `String.prototype` methods, the RegExp matcher (non-unicode
  per code unit, unicode per code point) and its builtin layer, lone-surrogate
  subjects **and** patterns, string/template literals, `decodeURI`, and `iu`
  case folding. Remaining sub-item: `String.fromCharCode` still replaces lone
  surrogates with U+FFFD instead of preserving them — emitting real lone
  surrogates must land with regex-/eval-source fidelity (the UTF-8 oxc front end
  loses a lone surrogate in `eval("/"+cu+"/").source`; the fix recovers the
  literal's code units from the original WTF-8 by byte span, since a lossy
  U+FFFD and a WTF-8 lone surrogate are both 3 bytes), else `S7.8.5_*_T2`
  regress.
- [ ] `language/expressions` corners — dynamic-`import()` semantics and the last
  class/eval corners.
- [ ] `language/module-code` — namespace internals, hoisted default-function
  exports, TLA ordering.
- [ ] `language/eval-code` / `language/global-code` — eval-created global
  binding attributes and lexical/var binding interactions.
- [ ] Sparse-index semantics beyond the dense cap (`built-ins/Object`,
  `built-ins/Array`).
- [ ] Resizable-`ArrayBuffer` corners (`built-ins/TypedArray`,
  `built-ins/ArrayBuffer`): `subarray`/`set`/`slice`/transfer.

### Product follow-through (from `docs/fable_review.md`)

- [x] **SDK publishing** — `.github/workflows/release.yml` publishes the
  TypeScript SDK to npm and the Python SDK to PyPI (both via OIDC trusted
  publishing), alongside the crates.io and prebuilt-binary jobs, on tag push.
  The npm/PyPI README badges are now backed by automation.
- [x] **TypeScript SDK tests** — `sdk/typescript/test/` exercises the HTTP
  client (run/replay/resume/signal/stream/checkpoint), SSE parsing, and
  manifest/serialization round-trips against a mock server via Node's built-in
  test runner (`npm test`), wired into CI. The Python-SDK integration tests
  against a real server remain the end-to-end complement.
- [ ] **Broader `node:` allowlist** — `stream`, `zlib`, and `child_process`
  remain unsupported (the current allowlist covers `process`, `buffer`, `util`,
  `fs`, `fs/promises`, `crypto`, `http`, `https`, `path`, `events`, `url`,
  `assert`, `os`).
- [ ] **Providers and modalities** — only Anthropic and OpenAI stream natively
  (LiteLLM is the escape hatch). No embeddings, image/audio modalities, or
  JSON-mode/structured-output plumbing beyond tool calls.
- [ ] **Code-comment doc drift** — several `src/` comments still explain intent
  by reference to "the QuickJS path" (e.g. `engine.rs`, `bindings.rs`,
  `rust_engine.rs`, `server.rs`). Low-priority cleanup now that the
  roadmap docs are corrected.

### Sandbox hardening (from `docs/sandbox-model.md`)

- [ ] Per-VM (ownership-attributed) memory accounting — the per-run meter is
  thread-attributed today, so concurrent runs can still drift.
- [ ] Per-tenant CPU quota — the opcode budget bounds a run, not a tenant.
- [ ] Deeper policy matching — `match_args` is shallow; approvals are cached
  per-session and policy cannot change mid-run.
- OS-level isolation (seccomp/namespace/process) remains an explicit non-goal;
  the engine is capability confinement, not a container.

### Optional feature extensions

- [ ] Branching Phase 3 — the whole-agent replay-prefix model
  (`docs/branching-execution.md` §8.9); deferred because the §8.2 fork-return
  model already covers the MVP. Branch merge/promote into the parent run, a
  tael comparison view, and a programmatic `POST /sessions/{id}/fork` surface
  are smaller follow-ups.
- [ ] Signals — per-branch addressing, a typed signal-schema registry, a
  symmetric `chidori.sendSignal(runId, name, payload)` send side, and
  broadcast/pub-sub to multiple runs (`docs/signals.md` §17).
- [ ] Context management — a raw-`Message[]` escape hatch, a typed
  prompt/segment schema registry, fleet-wide content-addressed cache sharing,
  and per-provider cache-strategy plugins (`docs/context-management.md` §16).

## Useful Verification Commands

```bash
cargo fmt --check
cargo test
cargo test --workspace
scripts/test262.sh --gate            # conformance baseline gate
cd sdk/typescript && npm run build
cd sdk/typescript && npm run typecheck
python -m unittest sdk/python/tests/test_session_api.py
```

## Deferred Or Descoped

- Visual editor work is not planned.
- Automatic `.star` to `.ts` conversion is not required.
- Node package compatibility (npm) inside agents is not required for v1.
- Polyglot snippet execution (`execJs`/`execPython`/`execWasm`) was removed with
  the WASM/Python sandboxes (#39) and has no replacement.
- VM-image snapshot/restore of suspended continuations is descoped: durability
  is the deterministic-replay journal. (The QuickJS-era live-VM snapshot design
  in `docs/typescript-vm-snapshot-runtime.md` is historical.)
- `Intl`, `Temporal`, `Atomics`/`SharedArrayBuffer`, `WeakRef`/finalizers,
  `ShadowRealm`, decorators, and iterator helpers are intentionally unsupported
  and skipped honestly by the conformance runner.
