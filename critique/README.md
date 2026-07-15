# Chidori project critique — code quality & end-user experience

*Date: 2026-07-15 · Baseline: commit `9d8ca78` (v3.6.0) · Method: full-source
review of all three crates and both SDKs, plus a hands-on experiment suite run
as a first-time end user (no LLM API keys) on Linux x86_64.*

Everything empirical in this document is reproducible:
`bash critique/experiments/run_experiments.sh` (see
[experiments/RESULTS.md](./experiments/RESULTS.md) for the full run log).

---

## Verdict

**This is an unusually well-engineered project whose core promises are real,
undermined mainly by small, cheap-to-fix seams — most of them fallout from
recent safety hardening that the docs haven't caught up with.**

The flagship claims all survived adversarial testing: replay is byte-identical
with the LLM provider literally unplugged; exactly-once held when I replaced a
tool body with `throw`; two independent live runs of a "random" workload
produced identical bytes; pause → resume → replay worked over the SDK on the
first (correctly-run) attempt. That is rarer than it should be for agent
frameworks.

Overall grade: **A-** — the same grade three independent deep-dives produced
for the runtime crate, the JS engine, and the SDK/docs/examples surface,
which is itself telling: quality here is *uniform*, not concentrated in a
showcase module.

---

## 1. Code quality

### What's genuinely excellent

- **The determinism/replay architecture is honest all the way down.** The
  single host-call boundary (`docs/architecture.md`) isn't marketing: every
  effect really does flow through one recorded journal
  (`crates/chidori/src/runtime/host_core.rs`), which is why the replay
  experiments pass. Design constraints are enforced in dependency choices
  (`indexmap` chosen because "iteration order must be address-independent for
  replay to be correct").
- **Security posture beyond the norm.** Fail-closed refusal to bind
  non-loopback without an API key, constant-time bearer comparison
  (`server.rs:819`), an SSRF guard installed as the DNS resolver so it closes
  the rebinding TOCTOU (`ssrf.rs`), seccomp + Landlock + Seatbelt process
  isolation on by default, per-run heap metering (`mem_guard.rs`), and a
  policy engine whose default is *ask* on the CLI and *deny* on the server.
- **Comment discipline that most teams claim and few have.** Zero TODO/FIXME
  markers in ~100k lines of hand-written Rust; intent lives in why-comments
  citing a 28-file docs tree that is verifiably accurate (30+ documented env
  vars, flags, and routes spot-checked against source — all exist). Negative
  results are documented rather than deleted (the mimalloc feature is kept but
  off, with measurements explaining why).
- **Tests that mean something.** 430 test functions in the runtime crate,
  including spawn-the-real-binary CLI tests, sandbox tests that assert a
  denied syscall is actually blocked, and replay edge cases (a completed host
  promise whose producing child is gone). The JS engine adds differential
  corpora requiring byte-identical output with each optimization tier on/off,
  cross-checked against Node.
- **The from-scratch JS engine is defensible, not NIH.** It reuses oxc for
  parsing and temporal_rs/ICU4X for the hardest builtins; the from-scratch VM
  is tied to a real requirement (deterministic, snapshot-able execution that
  fights V8/JSC embedding). 99.1% of *executed* test262 with a 47k-entry
  committed per-test baseline, CI-gated per PR plus nightly, zero `unsafe` in
  the entire engine, and honest skip accounting.

### What needs work

1. **God files.** `server.rs` (5,508 lines), `exec.rs` (8,685 lines, one
   `impl` block spanning ~7,800), `compiler.rs` (7,196), `host_core.rs`
   (3,473). The two hardest-to-review files in the project are also the two
   most load-bearing.
2. **Stringly-typed errors.** No `thiserror`-style enums anywhere; 123
   occurrences of `Result<_, String>` at the host-call boundary, ad-hoc
   `json!({"error": …})` HTTP bodies, and behavior gated on message substrings
   (`message.contains("CHIDORI_REPLAY_LAX")`, `host_core.rs:1934`). This will
   hurt more every year as the API surface grows.
3. **Handler boilerplate.** The session-lookup → 409/404/500 preamble is
   copy-pasted across ~5 server handlers; `spawn_blocking(...).await.unwrap()`
   in handlers turns a panicking run leg into a handler panic instead of a 500
   (`server.rs:1324` et al.).
4. **68 `#[allow(dead_code)]` "staged API" items** (concentrated in
   `engine.rs`/`bindings.rs`) — honestly labeled, but unshipped surface that
   will drift without callers or tests.
5. **Engine hard edges:** native stack overflow on pathological input can
   still abort the process (handled by process-chunking in the harness, not
   in-engine); "zero unsafe" is convention, not `#![forbid(unsafe_code)]`;
   13.9% of test262 is skipped and some skipped features (iterator helpers,
   WeakRef) do appear in modern npm code.

---

## 2. End-user experience (what I actually hit, in order)

### The good

- **Time-to-first-success is real.** `run examples/agents/hello.ts` worked
  first try, in 34 ms, with output matching the docs character-for-character.
  `init --template docs` scaffolds a clean, readable project fully offline.
  `trace` and `stats` (tokens + estimated cost per model) are the kind of
  operational niceties most frameworks never ship.
- **The `serve` startup banner** prints auth/policy/CORS state and the full
  route table — the single most useful server banner I've seen; it saved a
  round trip to docs during testing.
- **The flagship loop works.** Record with a live (stub) LLM provider → kill
  the provider → replay: byte-identical output, zero provider calls
  (counter-verified). Sabotaged-tool replay proved exactly-once. Pause/resume
  over the SDK worked as documented, including replay of the approved run.
- **Failure messages at the policy layer are model citizens** — the
  `--trusted` refusal names the exact effect and offers three remedies.

### The friction, ranked by pain

1. **Runtime errors have no stack traces** (`Error: JavaScript exception:
   kaboom` — no frames, no file, no line) and **parse errors have no
   line/column** (`TypeScript parse error: Unexpected token`) even though oxc
   has spans. For a framework whose pitch is *debuggability*, this is the gap
   an agent author hits within the first ten minutes of writing real code.
2. **A headline documented feature is currently unreachable.**
   `examples/record-replay/README.md` §"Edit then resume" promises
   modify-and-resume with divergence checking; the fingerprint gate added in
   the recent guarded-replay hardening (#130) refuses *any* source edit at
   `chidori resume`, with no opt-in flag, and `CHIDORI_REPLAY_LAX` governs a
   different (now-unreachable-from-here) layer. The refusal message doesn't
   say how to do it on purpose.
3. **Docs lag the ask-by-default policy (#130).** Every `chidori run` command
   in the README, getting-started, and example READMEs fails when stdin isn't
   a TTY (CI, pipes, scripts) because none mention `--trusted` or a policy
   profile.
4. **First-build friction:** stable Rust 1.94 fails late with a version
   resolution error; `rust-toolchain.toml` pins `channel = "stable"` which
   can't express the ≥1.95 floor it documents in a comment.
5. **Papercuts:** `resume` prints the run *duration in ms* as "`N` calls
   replayed" (`main.rs:1372`); `serve`'s session store
   (`.chidori/sessions.sqlite3*`, also from #130) isn't gitignored, so the
   documented workflow dirties the repo; `trace` lists child calls above their
   parents; `serve --help` says "event dict" (Python-ism); the snapshot ABI
   label is still `"chidori-quickjs"`.
6. **SDK seams** (from the parallel SDK review): the Python package ships no
   `py.typed`, so its careful annotations are invisible to consumers'
   type-checkers; Python `stream()` silently drops the `paused` SSE event that
   the TS SDK handles (breaking the advertised method-for-method parity
   exactly where signal-driven flows need it); no SDK method wraps
   `/approve` or `/cancel`; `docs/sandbox-model.md` contradicts itself (and
   the code) about whether OS isolation is default-on.

### A meta-observation

Findings 2, 3, and 5 share one root cause: **PR #130 landed safety
improvements (ask-by-default policy, fingerprint-gated resume, durable SQLite
sessions) without sweeping the documentation, examples, `.gitignore`, or error
messages that the old behavior had shaped.** The hardening itself is
good — the defaults are the right defaults — but a release checklist item
("grep docs+examples for every behavior this PR changes") would have caught
all of it.

---

## 3. How I feel about it

Skeptical going in, won over by the end. "Durable, replayable agents on our
own JS engine" reads like a pitch deck; three days of trying to break it says
otherwise. The determinism claims are the most falsifiable claims a framework
can make, and this one passes its own audit — including under sabotage. The
engineering culture visible in the tree (honest benchmark tables that admit
being 30× slower than V8, documented negative results, zero TODO debt,
committed conformance baselines) is the strongest predictor of a project aging
well, and it's the same culture that makes the current doc drift feel like a
solvable process bug rather than a trajectory.

What would make me hesitate to bet production work on it today: single-digit
bus factor implied by the uniform authorial voice, the maintenance tax of a
private JS engine chasing a moving spec, and the missing stack traces. What
would make me bet anyway: every safety default is fail-closed, every claim I
tested was true, and the worst bugs I could find in three days were a
duration printed as a count and a stale README.

## 4. Top recommendations (highest leverage first)

1. **Stack traces and parse spans.** Thread oxc spans into `check`/parse
   errors and capture JS frames into `JavaScript exception:` messages. This is
   the single biggest UX multiplier available.
2. **Re-enable edit-and-resume behind an explicit flag**
   (`chidori resume --allow-edited-source`, refusal message pointing to it),
   restoring the documented workflow without weakening the safe default.
3. **Sweep docs/examples for #130 fallout:** add `--trusted` (or a
   policy-profile stanza) to every non-interactive command; fix
   `sandbox-model.md`'s self-contradiction; gitignore
   `**/.chidori/sessions.sqlite3*`.
4. **Fix `main.rs:1372`** (`total_duration_ms()` printed as call count).
5. **Ship `py.typed` + the Python `paused` SSE event**; add `approve`/`cancel`
   to both SDKs.
6. Then, at leisure: split `server.rs`/`exec.rs`, introduce typed error enums
   at the host-call boundary, add `#![forbid(unsafe_code)]` to `chidori-js`,
   and encode the ≥1.95 floor as `rust-version` in the workspace so old
   toolchains fail fast with a clear message.

---

### Appendix: independent deep-dive grades

| Area | Grade | One-line summary |
|------|-------|------------------|
| `crates/chidori` (runtime/CLI/server) | A- | Excellent security + tests; god files and stringly errors |
| `crates/chidori-js` (JS engine) | A- | Rigorous conformance + zero unsafe; perf gap and two god files |
| SDKs / docs / examples | A- | Near-perfect doc accuracy; py.typed, stream parity, doc drift |
