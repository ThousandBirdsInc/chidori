# Consumer usability review, round 3: the everyday-agent surface

**Date:** 2026-07-17 · **Chidori:** 3.6.0, built from source at `eb3c788` ·
**Perspective:** the same consumer as
[round 1](./consumer-usability-review.md) and
[round 2](./consumer-usability-review-2.md) — a developer whose provider is
DeepSeek — but this time building the agent shape most people actually ship:
a single-file, tool-using, conversational assistant that a team would run
every week. Rounds 1–2 stress-tested the durability engine and the
multi-agent surface. This round asks the quieter question: **what is it like
to live with Chidori as a daily driver** — the editor experience, the
feedback loop when something is misconfigured, the cost story across
repeated runs, and whether "check in a checkpoint as a test" is a real
workflow or a README sentence.

## What was built

A **Release-Notes Concierge**
([`examples/release-notes-concierge/`](../examples/release-notes-concierge/)):
~125 lines of agent plus two tools and a shared parser module, run live on
`deepseek-v4-flash`. Given a `git log --numstat` dump of this repository's
last 50 commits (~90KB), it:

1. parses the dump into structured commits — `chidori.step`, so replays
   never re-pay the parse;
2. clusters the window into release themes with one structured-output
   prompt (`format: "json"`);
3. investigates each theme with the **built-in provider tool loop**
   (`prompt(..., { tools: ["commit_detail", "search_commits"], maxTurns: 8 })`),
   pulling commit bodies and file stats on demand instead of stuffing 90KB
   into the context;
4. assembles and revises the document in a `chidori.conversation()`
   editorial dialogue, seeded with the **house style stored in
   `chidori.memory` by previous sessions**;
5. gates publication on `chidori.input()` feedback (scripted for recorded
   runs, stdin/HTTP for interactive ones);
6. publishes `RELEASE_NOTES.md` via `workspace.write`.

New surface exercised this round: `conversation()`, the built-in tool loop,
`chidori.step`, `chidori.memory`, `format:"json"`, `chidori chat`,
`CHIDORI_PROMPT_CACHE_DIR`, `chidori check` / `tools` / `stats`, the Python
SDK checkpoint workflow, and the published npm/PyPI type packages against a
strict `tsc` project.

## The numbers first

| Scenario | Result |
|---|---|
| Full pipeline, first *correct* run | **134s, 124 recorded calls, $0.018** — 34.4k in / 17.1k out tokens, plus 46.3k tokens served from DeepSeek's cache. Every spot-checked commit hash in the output was real and accurately described |
| Replay of that run (invalid API key on purpose) | **Byte-identical output in 139ms, zero provider calls** |
| `kill -9` mid tool-loop → `chidori resume … --trusted --tools tools` | **Completed the whole run** — feedback round, style distillation, publish — in 67s. Round 2's Finding 1 is fixed |
| Same run next session with `CHIDORI_PROMPT_CACHE_DIR` set | **16.5s, $0.0016** — ~8× faster, ~11× cheaper; identical prompts served from the local content-addressed cache |
| House style learned in run N | **Applied in run N+1** — `memory.get` returned the distilled preferences and the output visibly followed them |
| `chidori serve` + Python SDK: pause → human feedback → revise → pause | Worked over plain HTTP, 69s to first pause |
| …then the final "approve" | **Run failed at the last line** — `workspace:write` denied under serve's deny-by-default (Finding 3) |
| Server killed and restarted with `--trusted`, paused session resumed | **Completed** — journal replayed free, only the write executed live |
| Entire campaign: 8+ live runs, serve sessions, chat REPL, all testing | **$0.02 actual provider spend** (DeepSeek balance: 19.95 → 19.93) |

> **Follow-up (same day):** the findings below were re-verified while fixing
> them, and three claims did not fully survive: (1) `chidori serve --help`
> *does* document `--trusted`/`--untrusted` and the startup banner *does*
> print the policy posture — the review read a truncated help output and
> never read the top of the server log; (2) the human's final answer in
> Finding 3 *was* journaled, and `POST /sessions/{id}/replay` recovered the
> failed session end-to-end once the policy was fixed — the "discarded
> answer" diagnosis came from reading the wrong run directory mid-flush
> (itself the id-confusion problem of Finding 4); (3) replayed records
> *preserve* their original timestamps — the "rewritten" stamps in Finding 4
> belong to in-flight container work that genuinely re-executed at resume
> (documented at-least-once semantics). Corrections are marked inline.
> Everything else stood, and the fixes landed alongside this note: `log`
> fields journaled, skipped tool files warn with reasons everywhere,
> `format:"json"` is strict by default, unknown-tool errors name the
> available tools and the load failures, session lists carry
> `run_id`/`pending_prompt`, both SDKs expose `run_id`/`pending_details`,
> `resume` reports the true replayed/live split, and `stats` per-model rows
> reconcile with the top line.

The engine's promises survive contact with a consumer better than round 2
left me expecting: this round found **no failure of the durability model
itself**. What it found instead is a pattern the first two rounds only
brushed: **when the consumer misconfigures something, Chidori's default
behavior is to succeed silently with a degraded result, or to fail late —
after the tokens are spent and the human has answered.** Every finding below
is a variation on that theme, and every one was cheap to hit and cheap to
fix.

Credit where due first, because the round-1/round-2 fix record is
excellent: `CHIDORI_OPENAI_COMPAT_URL`/`_KEY` + `--model deepseek-v4-flash`
worked first try in `run`, `chat`, and `serve`; `resume` now carries
`--trusted`/`--tools` and crash recovery of a trusted tool-using run just
works; the truncation warning fired at exactly the right moment with
exactly the right advice; policy-denial and divergence error frames point at
the right `await`; `llm.txt` documents `CHIDORI_PRICING` with a DeepSeek
example. A consumer who files an issue against this project has good odds
of seeing it fixed within a release.

---

## Finding 1: a tool that fails to load is silently not a tool

**The single worst hour of the round, and it cost a paid run.** The two
tools import a shared parser module. My first layout put it next to the
agent (`lib/parse.ts`) and imported it from the tools as `../lib/parse.ts`.
That import escapes the tools directory, which the loader evidently
disallows — **by silently skipping the tool file**. No warning at
`chidori run --tools tools`, no error from `chidori check`, nothing in the
journal. `chidori tools` says only:

```
No tools found in: tools
```

— two syntactically valid, correctly shaped tool files sitting right there.
The failure finally surfaced *mid-run*, at the first
`prompt({tools: [...]})`, as `Unknown tool in prompt tools: commit_detail` —
**after the 33-second, 16k-token clustering prompt had already been paid
for.** Sibling imports inside the tools directory work fine (with or
without the `.ts` extension), so the fix was moving one file. But nothing
tells you that rule exists: not the tools docs, not the loader, not the
error. Three cheap fixes, any of which would have saved the run:

- `chidori tools` (and the `run` startup scan) should say *why* a `.ts`
  file in the directory was skipped — "import `../lib/parse.ts` escapes the
  tool directory" would have ended the investigation in seconds.
- `prompt({tools})` names should be validated **at launch** when `--tools`
  is passed, not at the first prompt that uses them. `chidori check`
  ideally accepts `--tools` too.
- The unknown-tool error frame pointed at the *wrong line* — the
  `chidori.log()` call ten lines below the failing `prompt(...)` await.
  (Policy-denial and divergence frames were correct, so this looks specific
  to errors raised inside the prompt/tool-loop path.)

## Finding 2: a truncated reasoning model + `format:"json"` = a run that "succeeds" with an empty product

My clustering prompt set `maxTokens: 4000` — generous for 3 themes of JSON.
`deepseek-v4-flash` spent the entire budget on hidden reasoning and emitted
**zero visible characters**. What happened next is the instructive part:

- The runtime **did** warn, precisely and helpfully, on stderr:
  *"prompt (seq 4) hit the 4000-token output cap (stop reason `length`) …
  reasoning models also spend this budget on hidden reasoning."* This is
  round 1's fix and it's a good one.
- But `format: "json"` **silently fell back to the raw (empty) string** —
  the documented fallback behavior. My `themes ?? []` produced `[]`, the
  loop over themes ran zero times, the editorial conversation gamely
  assembled a document out of nothing, and the run **exited 0** having
  "published" release notes whose body was *"No sections were provided…"*.
  56 seconds, $0.003, exit code success.

In a cron job or CI, nobody reads stderr on a green exit. The warning knows
the run is degraded; the API result doesn't. What a consumer needs is a way
to make this **fail loud in code**, not in logs:

- `format: "json"` should have a strict mode (or simply *be* strict by
  default): truncated or unparseable → throw, with the stop reason in the
  error. The silent string fallback means every consumer must write the
  same defensive guard I added (`if (themes.length === 0) throw`).
- More generally `prompt()` hides `stopReason` — you must rewrite the call
  as `context().respond()` to see it. A `stopReason` (or
  `{ strict: true }`) escape on `prompt()` options would keep the
  simple API honest.

The follow-up cost asymmetry is worth stating: the correct-budget run cost
**6×** the broken one. Cheap garbage that exits 0 is the worst of both.

## Finding 3: `chidori serve` denies the *last* effect of a 3-minute run — and the human's answer rolls back with it

The serve + SDK session was the most instructive failure of the round. The
pipeline ran fine over HTTP — paused at `input()`, took my feedback, revised,
paused again. On the final "approve", the run **failed at the very last
line**: `workspace:write` denied, because `chidori serve` is
deny-by-default. Three observations, in increasing order of concern:

1. **The error message is excellent** — it names `CHIDORI_POLICY`,
   `CHIDORI_POLICY_FILE`, `CHIDORI_POLICY_PROFILE`, and `--trusted`, and the
   frame points at the exact `workspace.write` line.
   **Correction (follow-up):** the original text here claimed `chidori serve
   --help` documents none of this — false. The review read only the first
   twenty lines of the help output; `--trusted`/`--untrusted` are fully
   documented further down, and the startup banner prints
   `Policy: deny-by-default (…pass --trusted or set CHIDORI_POLICY* to
   relax)` on every boot. The consumer *was* told, twice, and didn't read
   either. What stands is only the softer point: the posture line scrolls
   away with the endpoint listing, and nothing at *session-creation* time
   repeats it.
2. **The denial is late.** Everything cheap happened first; the gated
   effect was the last record of the run. Tool calls and `workspace.read`
   sailed through the default policy, so nothing early in the run hinted at
   the cliff. Static analysis can't predict every effect, but the CLI knows
   at startup that *this agent file calls `workspace.write`* — even a
   startup note ("this server's policy denies `workspace:write`; runs will
   fail if they attempt it") would set expectations. Failing three minutes
   and two human interactions in is the expensive way to learn the default.
3. **The failed continuation rolled back the human's answer.**
   **Correction (follow-up): it didn't.** The "approve" input record, the
   distillation prompt, and the memory write were all journaled before the
   denied `workspace.write` — the review's diagnosis read a *different
   session's* run directory mid-investigation (the session-id/run-id
   confusion of Finding 4 claiming a scalp). And the recovery path is not
   only real but documented in the failure itself: the 409 from
   `POST …/resume` on a failed session points at `POST …/replay`, which —
   verified live after restarting the server with `--trusted` — replayed
   the whole journal including the human's answer for free and executed
   only the denied write. The engine and its error surface were both right;
   the consumer (me) was lost in the identity maze below.

What Finding 3 reduces to after correction: the deny-by-default cliff is
real but *announced*; the recovery exists and is signposted at the moment
of failure. The residual asks are visibility ones — repeat the posture at
session creation, and make the session↔run mapping legible enough that a
consumer under pressure reads the right journal (Finding 4).

## Finding 4: session ids, run ids, and an audit trail that rewrites itself

Small identity frictions that each cost minutes:

- A server session's `id` is **unrelated** to its `.chidori/runs/<run_id>`
  directory name. The mapping exists (`run_id` in the session detail JSON)
  but the Python SDK's `Session` doesn't expose it — nor
  `pending_details`, so an SDK consumer can't show the human what they're
  approving (the server sends it; the SDK drops it). The sessions *list*
  endpoint omits `pending_prompt` (the detail endpoint has it), so a
  dashboard can't say what each paused run is waiting for without N+1
  fetches.
- After a crash-resume, the journal's timestamps looked **rewritten and out
  of order**. **Correction (follow-up):** they aren't rewritten — replayed
  records are cloned verbatim from the journal, original timestamps
  included; the fresh stamps belong to the in-flight tool-loop *container*
  that legitimately re-executed (a half-finished container can't replay —
  the documented at-least-once window), and the apparent disorder is
  flush-order in `records.jsonl`, which was never seq-ordered. So the
  audit trail was sound; what was actually missing is the summary: the
  `Resumed from … (118 calls replayed)` message counts the *entire* final
  journal, not the replayed prefix, so the consumer can't see the
  replayed/re-executed split without diffing timestamps by hand. (Fixed
  alongside this review: resume now reports "N recorded calls replayed, M
  executed live" from a real replay-hit counter.) One genuine semantic
  discovered while fixing this: **top-level `workspace` effects re-execute
  on every replay** — workspace state is real disk, re-materialized rather
  than journal-served — which is also what re-stamped seq 1 in the crash
  drill. Byte-identical inputs make it invisible, but a consumer who edits
  a workspace file between record and replay changes what a "replay" reads.

## Finding 5: `chidori.log(message, fields)` discards `fields`

The types say `log(message: string, fields?: JsonObject)`. The docs call it
"structured logging". **Every bundled example** passes a fields object. The
runtime binding (`crates/chidori-js/src/lib.rs`, arity 1) forwards only
`{message}` — the journal records `{"message": "themes"}` and the
`{titles: [...]}` I logged is gone, everywhere, silently. I built my
debugging around reading the journal (which is otherwise excellent —
greppable JSONL you can watch mid-flight), and repeatedly went looking for
log data that was never written. One-line fix; disproportionate trust cost,
because the first thing a consumer checks when debugging is the log record —
and it's quietly lossy.

## Finding 6: the published type packages disagree with the runtime

`npm i -D @1kbirds/chidori@3.6.0` + strict `tsc` against an agent written
from the docs produced three classes of error, all false:

- `chidori.memory.get/set(...)` — docs and runtime expose a namespace; the
  published types declare `memory(action, key, value, opts)`. `(chidori.memory
  as any).get(...)` is the workaround in an otherwise fully typed file.
- `chidori.input(msg, { details })` — the "approval gates can show their
  artifact" feature (which works, in the CLI and over HTTP) is missing from
  the published `InputOptions`.
- `chidori.step<T>` constrains `T extends AgentJson`, and TypeScript
  `interface`s don't satisfy `AgentJson`'s index signature — so the natural
  `interface Commit {...}` fails with a five-level assignability error.
  `type Commit = {...}` works. No doc mentions this; the fix is one
  keyword, the diagnostic is noise. (Either document "use `type` aliases
  for step/tool payloads" or loosen the constraint.)

Same story one shelf over: the PyPI 3.6.0 wheel lags the repo's SDK (the
typed error classes — `HttpError` with `.status` etc. — aren't published,
so the documented "catch 409 on terminal resume" pattern string-matches
instead). Round 1 flagged stale published types; the runtime has since
grown faster than the packages. A CI check that type-checks the bundled
examples against the *published* `.d.ts` would catch every one of these.

## Smaller notes

- **Silence during long calls.** A 2-minute reasoning prompt gives no
  terminal feedback under `chidori run` — no spinner, no "prompt started".
  `--stream` exists and is good; a one-line "seq 4: prompt started
  (deepseek-v4-flash)…" default would stop the is-it-hung doubt.
- **`chidori stats` is a great idea** (per-model tokens/cost across run
  history) and worked, though "Prompt calls: 59" vs the per-model row's "48
  calls" left me unsure what the difference counts.
- **`chidori chat` piped from stdin worked**, streamed, and exited cleanly —
  nice for smoke tests.
- **DeepSeek's automatic prompt cache flows through correctly**: the trace's
  cache column showed 46.3k cache-read tokens on the tool-loop run and the
  cost line priced them via `CHIDORI_PRICING`'s `cache_read_multiplier`.
- **Build from source: ~12 minutes** (LTO, codegen-units=1) on a fast box.
  Fine for contributors; the prebuilt binary remains the right consumer
  path.

## The checkpoint-as-test verdict

The README's testing pitch — *"commit a recorded run to git and assert the
agent's behavior hasn't drifted"* — is **real, and the guardrails are the
best part**:

- Replay is fast enough to be a unit test (139ms for 124 calls) and needs
  no key.
- Edit the agent and `resume` **refuses**, naming both source fingerprints
  and the opt-in flag.
- Force it with `--allow-source-change` and an edit that touches a recorded
  call **fails loudly at the exact call**, names the differing field
  (`text`), and suggests remedies — this is drift detection working
  exactly as a test should.

What's missing is the last mile of ergonomics: there is no `chidori verify
<run_id>` that replays and reports pass/fail for CI — you assemble it from
`resume` + exit code + output diff, and you learn the exit-code contract by
experiment. The SDK side (`session.checkpoint()` → `checkpoint.save()` →
`client.replay(cp)`) worked as documented and is arguably the cleaner CI
primitive today. Given how good the underlying guards are, a first-class
test command is cheap adoption fuel being left on the table.
*(Follow-up: `chidori verify <agent.ts> <run_id>` shipped with this
review's fixes — no provider, deny-all policy, no run-dir writes, asserts
completion with byte-identical output and zero live calls.)*

## Where this leaves a consumer

Three rounds in, the shape of Chidori is clear from the outside: the
**hard things are solid** — I threw an invalid key, a SIGKILL, a server
restart, and a source edit at real recorded runs and the engine did the
right, loud, cheap thing every time. The **soft things are where runs go to
die quietly**: a tool that doesn't load, a JSON reply that came back empty,
a policy that denies the final write, a log field that never lands. None of
those failure modes broke the engine's promises; all of them cost a
consumer real money, real minutes, or (worst) silent wrong output with a
green exit code.

The common fix is a posture, not a feature: **when the runtime knows
something is degraded — a skipped tool file, a truncated structured reply, a
policy that will deny a declared effect, a dropped argument — say so in the
result, not just on stderr, and say it before the spend when possible.**
The engine already sees everything; that's the whole design. Round 3's ask
is that it tell the consumer.

---

## Appendix: reproducing this round

```bash
# demo lives in-tree
cd examples/release-notes-concierge
export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-...
export CHIDORI_MODEL=deepseek-v4-flash
export CHIDORI_PRICING='{"deepseek-v4-flash":{"input_per_mtok":0.28,"output_per_mtok":0.42,"cache_read_multiplier":0.1}}'

# the recorded-run shape used throughout this review
chidori run agent.ts --trusted --tools tools \
  --input '{"feedback": ["Tighten the intro.", "approve"]}'

# replay it for $0 (works with no key at all)
chidori resume agent.ts <run_id>

# the crash drill
kill -9 <pid mid-run>
chidori resume agent.ts <run_id> --trusted --tools tools

# the cost drill
export CHIDORI_PROMPT_CACHE_DIR=.chidori/prompt-cache   # then run twice
```

Runs referenced: first-correct `0d78f862` (134s, $0.018), empty-product
`7be7fed7` (Finding 2), unknown-tool `e94bf06b` (Finding 1), crash drill
`11e181d9`, memory-carryover `35ec73f4`, plus the serve/SDK sessions of
Finding 3. All on `deepseek-v4-flash` via `CHIDORI_OPENAI_COMPAT_*`.
