# Experiment results — 2026-07-16

Binary: `cargo build --release` from commit `ea0e70e` (chidori 3.6.0), Linux x86_64.
No LLM key configured anywhere; all experiments exercise the durability layer on
deterministic behaviors.

## E1 — Build from source
- `cargo build --release`: **9m 29s**, zero errors, zero warnings. Toolchain
  auto-pinned by `rust-toolchain.toml` (1.97). One-binary claim holds.

## E2 — Hello world
- `chidori run examples/agents/hello.ts --input name=Colton`: correct output in
  **~30-37ms wall clock** (cold). Node running an empty script takes longer than the
  entire chidori run.
- Papercut: `isolate worker: sandbox: landlock not enforced: no kernel support`
  printed to stderr on **every** run in this environment; informative once, noise thereafter.

## E3 — Ask-by-default policy
- Without `--trusted`, no TTY: fails **closed**, exit 1. Error names the gated
  effect (`tool:web_search`), explains the default, and lists three remediations. Excellent.
- With `--trusted`: runs.
- Nit: the annotated caret anchors at `run(` (line 3) instead of the actual
  `chidori.tool(...)` call (line 4).

## E4 — Exactly-once side effects  ✅ CLAIM VERIFIED
- Tool appends to `ledger.txt` per real invocation. After record: 3 lines.
- `chidori resume <run>`: output **byte-identical** (`diff` clean), ledger still
  3 lines → tool executed **zero** times on replay.

## E5 — Determinism policy  ✅ CLAIM VERIFIED (with a surprise)
- Replay of the recorded run: byte-identical, including `Math.random()` values.
- Surprise: **two fresh runs are also fully identical** — default policy pins
  `Date.now()` to epoch 0 and seeds `Math.random()`. Consequences an author
  should know:
  - `new Date().toISOString()` → `1970-01-01T00:00:00.000Z` in production output.
  - **You cannot self-time code inside an agent** (all durations report 0ms) —
    E8 had to be measured externally because of this.
  - "Unique" IDs from `Math.random()` repeat across runs.
  - Policy escape hatches exist (`RandomPolicy = "disabled" | "seeded" | "host"`)
    but the default surprised us in practice.

## E6 — JS engine coverage: 28/30 modern-JS probes pass
- Passing includes: BigInt, lookbehind/named-group/unicode-property regex,
  private fields, static blocks, structuredClone, Temporal, Proxy/Reflect,
  async generators, Object.groupBy, TextEncoder, atob/btoa, DataView.
- `WeakRef`: absent — **deliberate** (nondeterministic; also in the test262
  skip list). Defensible.
- `Intl.DateTimeFormat`: `TypeError: not a constructor` — partial Intl.
- **Bug found**: dynamic `import()` rejection is a raw substring scan
  (`line.contains("import(")`, `crates/chidori/src/runtime/typescript/transpile.rs:356`).
  A **comment** or string literal containing `import(` — or any identifier
  ending in `import` — kills the whole file with
  `dynamic import is disabled in durable TypeScript agents`. Reproduced with a
  comment on line 84 of the probe.

## E7 — Error message quality: excellent, two nits
- Runtime TypeError: full stack (`levelThree → levelTwo → levelOne → <anonymous>`),
  each frame with file:line:col, plus a miette-style annotated source snippet.
- Parse error: precise, shows the opening `{` and the expected token.
- Nit 1: stack frames anchor at the *function declaration* line, not the
  throwing statement (reported 6:10; the throw is at line 8).
- Nit 2 (from E9): a duplicated frame appeared in one server-mode trace
  (`at run (tools/side_effect.ts:19:23)` printed twice).

## E8 — Interpreter throughput vs Node (identical workload)
| | chidori | node 22 |
|---|---|---|
| wall clock (median of 3) | 519ms | 132ms |
| minus measured startup | ~485ms | ~70ms |
- Roughly **7× slower** on fib(27) + 200k-object churn + string building —
  consistent with the project's own honest 8-199× published gap. Startup is
  *much* faster than Node (~34ms full run vs ~60ms node boot alone).

## E9 — Pause → SIGKILL → resume in a new process  ✅ FLAGSHIP CLAIM VERIFIED
- `chidori serve`, POST /sessions → `status: "paused"`, prompt persisted, ledger has 1 line.
- `kill -9` the server. Start a **new** server process on a different port.
- POST /sessions/{id}/resume `{"response":"yes"}` → `status: "completed"`,
  `decision: "shipped"`, and the ledger **still has exactly 1 line** — the
  pre-pause tool call was replayed from the journal, not re-executed.
- Frictions found on the way:
  - `chidori serve` does **not** infer the workspace root the way `chidori run`
    does; workspace ops fail until `CHIDORI_WORKSPACE_ROOT` is set. Same agent,
    different behavior between the two entry points.
  - CLI-mode `chidori.input()` reads stdin; at EOF it silently resolves to `""`
    instead of failing loudly — a piped/CI run can proceed on an empty answer.

## E10 — Guarded replay / edit-and-resume  ✅ CLAIM VERIFIED
- `chidori trace <run>`: readable call log (9 calls, args, durations, errors, nesting).
- Edit an already-replayed call's argument, `resume`: **refused** with both
  fnv1a64 source hashes shown, names the exact opt-in flag.
- With `--allow-source-change`: **argument-level divergence detected at seq 1**,
  error shows recorded vs current args and two remediation paths. This is
  exactly the fail-loud behavior the docs promise.

## Score card

| Claim | Verdict |
|---|---|
| One self-contained binary | ✅ holds |
| Replay = zero re-execution, byte-identical | ✅ verified (ledger + diff) |
| Survive crash, resume in new process | ✅ verified (SIGKILL test) |
| Fail-loud divergence on edited history | ✅ verified |
| Ask-by-default fails closed | ✅ verified |
| "If you can write a function, you can write an agent" | ✅ hello.ts in 6 lines |
| Modern-JS surface | 28/30 (WeakRef deliberate, Intl partial) |
| Raw JS throughput | ~7× slower than Node (as self-reported) |

Bugs / papercuts filed from this session:
1. `import(` substring false-positive (comments/strings/identifiers) — transpile.rs:356.
2. Stack frames anchor at declaration lines, not throw sites.
3. Policy-violation caret points at `run(`, not the gated call.
4. Duplicated stack frame in server-mode error.
5. `serve` vs `run` workspace-root inconsistency.
6. `input()` EOF → silent empty-string answer.
7. Landlock warning printed on every run (needs a once-per-session or --quiet path).
8. Default fixed clock/seed makes two *fresh* runs identical — powerful but
   under-signposted at the CLI (a one-line "clock pinned, rng seeded" notice would do).

---

## Fixes (same branch, after the critique)

All eight findings were fixed and re-verified against their original repros:

1. **`import(` substring scan** → dynamic import is now rejected from the oxc
   AST (`ImportExpression` visitor); comments/strings/identifiers no longer
   false-positive; parse errors defer to transpile diagnostics. 3 new unit tests.
2. **Stack frames anchor at declarations** → a pc→source position table is
   threaded through compilation and both interpreter tiers; innermost frame =
   throw site (8:3), outer frames = call sites, awaited rejections = the await.
3. **Policy caret at `run(`** → same fix; now lands on the gated
   `chidori.tool(...)` call (4:24).
4. **Duplicated frame** → frame recovery matched a trace embedded in a nested
   tool error's *message*; recovery now strips the creation-time stack head.
   (Fixing this also surfaced and fixed a pre-existing +2-line drift on
   tool-file frames: the TS-remap project root was set on the wrong thread.)
5. **`serve` vs `run` workspace root** → `serve` now defaults the workspace to
   the served agent's project dir; explicit `CHIDORI_WORKSPACE_ROOT` wins.
6. **`input()` EOF → silent ""** → fails loudly at EOF; piped answers still work.
7. **Landlock warning every run** → sandbox degradation notes print once per
   parent process.
8. **Invisible determinism defaults** → `chidori run` prints a one-line,
   tty-only notice (clock pinned, RNG seeded, override env vars named).

Also: `#![forbid(unsafe_code)]` now enforces the engine's zero-unsafe claim at
compile time; `docs/conformance.md` synced to the committed baseline
(39,837 / 357); new `docs/README.md` separates user docs from internal notes.

Verification: full `chidori` + `chidori-js` test suites pass, clippy clean at
`-D warnings`, Test262 gate zero regressions (99.11% of executed), and the
E4/E9 exactly-once + byte-identical replay experiments re-run clean on the
fixed binary.
