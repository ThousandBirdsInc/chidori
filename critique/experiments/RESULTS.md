# Experiment results — 2026-07-15

Environment: Linux x86_64 container, Rust 1.97.0, Node 22.22.2, no LLM API
keys. Original run built at commit `9d8ca78`; re-test run built at `6ccbfb4`
(#132). Reproduce with `bash critique/experiments/run_experiments.sh` from
the repo root.

## Re-test after #132 (baseline `6ccbfb4`, same day)

The full suite passes against the rebuilt binary, with previously
expected-failing experiments now asserting the fixed behavior:

| # | What changed | Verified result |
|---|--------------|-----------------|
| 5 | `--allow-source-change` added to `chidori resume` (+ server routes) | Tail-only edit + flag → resumes, output byte-identical to the recorded run. Edit to an already-executed step + flag → loud divergence refusal. No flag → safe refusal unchanged. |
| 9a | Parse errors carry spans | `chidori check` now prints a miette-style diagnostic with `file:line:col`, the offending source line, and a caret under the token. |
| 9b | Runtime errors carry stack traces | `Error: kaboom` now prints `at inner (...thrower.ts:2:15)` etc. **Residual nit:** frames above the throwing frame use post-transpile line numbers (`outer` reported at `:5:15`, source line 3; the `run(...)` callback at `:8:11`, source line 4) — mapping is only correct for the first frame. |
| 10 | `main.rs` count bug fixed | `(5 calls replayed)` now matches `trace`'s `Calls: 5` for the same run. |
| 11 | `.gitignore` covers `**/.chidori/sessions.sqlite3*` | `git check-ignore` passes after a `serve` session; workflow no longer dirties the repo. |
| 12 (new) | Example-README sweep | Still outstanding: `examples/record-replay/README.md` mentions neither `--trusted` nor `--allow-source-change`, so its commands still fail as written. |

Also landed in #132 and confirmed from source/diff: `rust-toolchain.toml`
pinned to `channel = "1.97"` (fixes the late first-build failure, experiment
0), root README + getting-started now document the ask-by-default policy and
`--trusted` (experiment 2's refusal is now documented behavior),
`sandbox-model.md`'s isolation-default contradiction fixed, Python SDK ships
`py.typed` and handles the `paused` SSE event (with new tests).

## Original run (baseline `9d8ca78`)

| # | Experiment | Result |
|---|------------|--------|
| 0 | Build from source | ⚠️ Failed on first try: installed stable Rust was 1.94, workspace needs ≥ 1.95. `rust-toolchain.toml` pins `channel = "stable"` which silently accepts a too-old stable; the failure surfaces as a cargo resolution error at the end, not an upfront check. Fixed with `rustup update`. |
| 1 | `chidori run examples/agents/hello.ts --input name=Colton` | ✅ 25–34 ms wall time, output matches `docs/getting-started.md` exactly. |
| 2 | README record-replay commands as written | ❌ `chidori run … -i name=Ada` fails: the (new) ask-by-default policy wants interactive approval for `tool:open_ticket`. The error message is excellent (three remedies offered), but **none of the README/getting-started commands mention `--trusted`**, so every documented command fails in a pipe/CI. |
| 3 | record → `trace` → `resume` | ✅ Replay output byte-identical to the recorded run. `trace` output (call tree, args, durations) is genuinely useful. |
| 4 | Exactly-once proof | ✅ Replaced `tools/send_email.ts` body with `throw new Error("boom")`; replay still returned the recorded `{delivered: true}` — the tool body was never re-executed. |
| 5 | Edit-and-resume (advertised in `examples/record-replay/README.md` §"Edit then resume") | ❌ **Unreachable.** Any source edit — even a trailing comment — is refused by the snapshot fingerprint gate (`resume refused: the agent source no longer matches this run's checkpoint`). `CHIDORI_REPLAY_LAX=1` does not apply (it governs argument drift, which is now unreachable behind the fingerprint gate at the CLI). No flag exists to opt in. The error message doesn't say how to proceed intentionally. |
| 6 | LLM record/replay via OpenAI-compatible stub (`fake_llm.py`, `LITELLM_API_URL`) | ✅ Recorded run made exactly 1 provider request (counter-verified). Killed the provider; replay returned byte-identical output with the provider unreachable — **zero-token replay confirmed**. `trace` shows model, request digest, token counts, and estimated cost. |
| 7 | Determinism across live runs | ✅ Two separate processes, same input → byte-identical output including "random" ids and `startedAt: 0` (seeded RNG, fixed clock by default). |
| 8 | Server + TS SDK pause/resume (`human_approval` via `driver.mjs`) | ✅ run → `status=paused` with prompt → `resume("approve")` → completed → replay with no re-prompt and identical output. `serve` startup banner (auth/policy/CORS state + route table) is exemplary. |
| 9a | Parse-error quality (`chidori check` on `{,}`) | ❌ `TypeScript parse error: Unexpected token` — **no line/column**, though the oxc parser has spans. |
| 9b | Runtime-error quality (nested `throw`) | ❌ `JavaScript exception: kaboom` — **no stack trace, no file/line**. The biggest day-to-day DX gap found. |
| 10 | `resume` summary line | 🐛 Prints `(26 calls replayed)` for a 5-call run — `main.rs:1372` passes `result.call_log.total_duration_ms()` (26 ms) where a call count belongs. |
| 11 | `serve` artifact hygiene | 🐛 Durable session store (`.chidori/sessions.sqlite3{,-wal,-shm}`, added in #130) is not covered by `.gitignore` (which only lists `**/.chidori/runs|wasm-cache|memory`), so following the README dirties `git status`. |
| 12 | Interpreter perf sanity (`fib(30)`) | ✅-with-caveat: ~440 ms vs Node ~15 ms compute (~30×) — consistent with the project's own honest numbers in `docs/js-performance-roadmap.md`. Startup: 25 ms total (including record pipeline) beats `node` bare startup (~40 ms). |
| 13 | `init` scaffold + `check` + `stats` | ✅ `init --template docs` scaffolds a clean 3-file project offline; `check` validates it; `stats` aggregates runs/tokens/cost per model. |
| 14 | Time-travel resume (`--until-seq 2`) | ✅ Replays the journal to the frontier, re-runs the tail live. |

Cosmetic notes gathered along the way:

- `trace` displays child calls before their parent (`#2 log` above `#1 tool`) —
  explainable (completion order) but initially confusing.
- `serve --help` says events are passed to "agent(event) as a structured event
  **dict**" — Python vocabulary in a TypeScript-first tool.
- The snapshot ABI label is still `"chidori-quickjs"` (`snapshot.rs:992`)
  although QuickJS is gone.
- The default model recorded in checkpoints is `claude-sonnet-4-6` even when
  routed through the generic OpenAI-compatible provider.
