# Consumer usability review, round 6: the long-haul conversational surface

**Date:** 2026-07-18 · **Chidori:** 3.6.1, built from source at `a98a686` ·
**Perspective:** the same consumer as rounds
[1](./consumer-usability-review.md) · [2](./consumer-usability-review-2.md) ·
[3](./consumer-usability-review-3.md) · [4](./consumer-usability-review-4.md) ·
[5](./consumer-usability-review-5.md) — a developer whose provider is
DeepSeek. Rounds 1–5 proved the engine (replay, actors, day-2 ops, serving
to users). This round lives where an agent spends its *life*: the surfaces
you touch every day for weeks, none of which any prior round exercised —
the **`chidori init` / `chidori chat` onboarding funnel** the README now
opens with, **`chidori.template`** as the prompt layer,
**`chidori.memory`** as state that outlives a run, explicit
**`Context.compact()`** window management on a conversation that outgrows
its budget, and the opt-in **local content-addressed prompt cache**
(`CHIDORI_PROMPT_CACHE_DIR`).

## What was built

A **Standup Scribe** ([`examples/standup-scribe/`](../examples/standup-scribe/)):
an agent that lives with a fictional five-person team for two weeks, live on
`deepseek-v4-flash`. Each week (`chidori run agent.ts --input week=week1`):

1. reads the week's five raw standup transcripts from the workspace, one
   day at a time, in **one running conversation** (`chidori.context`,
   prefix frozen as a cacheable head);
2. renders every prompt through **`chidori.template`** — four `.jinja`
   files, zero string concatenation in the agent;
3. calls **`ctx.compact({ budgetTokens })`** before each day — a pure no-op
   until the conversation outgrows the budget, then a recorded summarizer
   prompt folds the older days into one summary segment;
4. distills a **thread ledger** (structured JSON: id, owner, status, note
   per ongoing thread) and stores it in **`chidori.memory`** — week 2
   starts from the open threads week 1 left behind, as a `doc()` in its
   stable head;
5. pauses on **`chidori.input`** with the drafted brief as `details`, then
   publishes it to `briefs/<week>.md` with `workspace.write`.

Around it: `ask.ts`, a conversational companion over the accumulated
ledger + briefs, driven through the **`chidori chat` REPL** and the driven
`messages` mode — plus the three `chidori init` templates run exactly as
the README's quick start prescribes.

## The numbers first

| Scenario | Result |
|---|---|
| Onboarding funnel: `init --template docs` → `chidori chat agent.ts`, DeepSeek env vars only | **Zero to a correct, streamed docs answer in under a minute**, first try, no OpenRouter login needed |
| `init --template worker` autonomous tool loop | Completed first try: tool called, result observed, answer composed |
| Week 1: 5 transcripts → 8 prompts → ledger + published brief | **Completed first try, 57s, $0.0031**; every number in the brief traces to a transcript |
| DeepSeek prompt-cache accounting on the growing conversation | **Visible per prompt and growing turn over turn** (384 → 896 → 1664 → 2176 → 2816 → 3072 cache-read tokens); 11,008 total reads week 1 |
| Week 2: `kill -9` mid-flight on the day-4 prompt | `chidori resume --trusted`: **17 recorded calls replayed, 10 live**, the in-flight prompt re-ran, week completed with the pre-crash digests intact — 31s |
| Compaction (budget 2200) firing *inside the resumed segment* | Worked: recorded summarizer prompt, context segments 10 → 8, estimate 2556 → 1605 tokens, day-5 digested against the summary |
| Cross-run memory | Week 2's ledger updated week 1's thread ids in place (T1/T3/T4) and added T5 — carryover exactly as designed |
| `chidori verify` on both runs (incl. the crash + compaction run) | **exit 0 in ≤55ms, $0, output identical** — a run that spans a SIGKILL and a live compaction is a valid CI fixture |
| Replay with no provider configured | 52ms, 0 live calls, correct output |
| Local prompt cache: identical week-1 run twice | **51.7s / $0.0031 warm → 92ms / $0 the second time** — all 8 prompts served locally, recorded as normal call-log entries with no token usage |
| `chidori chat ask.ts` over the accumulated state | Grounded, correct answers (open threads with owners; Jules's two-week shipping record) |
| `chidori run --stream` on the same `ask.ts` | **Fails**: `chidori.workspace` errors out and the run leaves no journal at all (Finding 1) |
| `chidori stats` roll-up for the demo directory | 4 runs, 16 prompt calls, 20,352 cache-read tokens, **$0.006877**, per-model table |
| Whole campaign (funnel + 2 weeks + chat + probes) | ≈ **$0.009** by chidori's meter; DeepSeek balance unmoved at display precision (19.88 → 19.88) |

Same headline as every round, and it still deserves saying: **the engine
did not miss once.** Crash-resume across a compaction boundary with memory
carryover is about as adversarial as this surface gets, and it was
uneventful. The findings, as usual, live at the edges — and this round's
biggest one is that the `--stream` flag quietly runs a *different,
degraded* runtime.

## The funnel: genuinely 30 seconds (credit where due)

Round 1 (3.6.0) had no `chidori init`, no `chidori chat`, no OpenRouter
fallback — onboarding meant reading `llm.txt` and writing a file. The
README's new step 1 is real now:

```
$ chidori init funnel-docs --template docs
Scaffolding 'docs' template into funnel-docs
  created funnel-docs/agent.ts
  ...
$ chidori chat agent.ts
you> What is a host call?
> A host call is any interaction an agent has with the outside world, routed
> through the runtime so it can be recorded and replayed ...
```

As a DeepSeek consumer I skipped `model-login` entirely: the two
`CHIDORI_OPENAI_COMPAT_*` variables from round 1 carried the whole funnel —
docs chat, the `worker` template's tool loop, everything — with
`CHIDORI_MODEL=deepseek-v4-flash` picking the model. No round-1-style
routing archaeology. All three templates are small, readable, and idiomatic;
the worker template is a better tutorial on author-driven tool loops than
any doc page.

Two nits. The REPL prints the assistant's reply with **no marker** — the
answer appears directly after the `you>` prompt line, so a scrolled-back
transcript reads as one undifferentiated column of text. And exiting prints
nothing about what the conversation cost, even though every turn was
metered (the primitives clearly exist — `chidori stats` prints exactly this
for persisted runs).

## Living with it for "two weeks"

The demo's texture matters for this round's question — is this framework
pleasant as a *daily* driver for a conversational, stateful agent? Verdict:
mostly yes, and three specific things stood out.

**Templates keep the agent honest.** `agent.ts` contains no prompt prose at
all; the four `.jinja` files are the prompt surface. `chidori.template`
resolves relative to the agent's directory (learned by reading
`template.rs` — see Finding 4), renders through minijinja, and each render
is a journaled host call, so `chidori trace` shows exactly which template
with which variables produced each prompt. That last property — prompt
provenance in the trace — is quietly excellent and I haven't seen it
elsewhere.

**Memory is boring in the right way.** `memory.set("threads", ledger)` in
week 1; week 2's run `get`s it, filters to open threads, and injects them
as a `doc()` in the stable head. The store is a readable JSON file next to
the runs (`.chidori/memory/default.json`), anchored to the agent directory,
shared naturally by `ask.ts` living in the same folder. Replay and verify
treat memory actions as recorded calls, so replaying week 1 after week 2
has run does **not** clobber the newer ledger — the replays returned
recorded values without touching the store. That's the behavior you want
and it just happened.

**Compaction is explicit, observable, and survives a crash.** The
`budgetTokens` no-op contract means the call sits unconditionally at the
top of the day loop; `estimateTokens()` tracked DeepSeek's actual prompt
sizes within a few percent, close enough to budget against. When the tail
finally crossed budget, the summarizer prompt appeared in the trace as an
ordinary recorded call (`max_tokens: 4096`, 1663→356 tokens), segments
dropped 10 → 8, and — the part I was trying to break — this happened *in
the live continuation of a resumed run* whose first half was replayed from
the journal of a SIGKILLed process. `chidori verify` then replayed the
whole hybrid for $0 in 40ms, byte-identical.

**And the cache economics compound.** Three layers were live at once, all
visible: DeepSeek's own context cache read the shared conversation prefix
at a growing discount every turn (chidori parses and displays the reads
per prompt — 11K tokens read in week 1 alone); the journal made every
replay/verify/resume free; and `CHIDORI_PROMPT_CACHE_DIR` made even a
*fresh identical run* free — 92ms and $0 for a rerun that would otherwise
have been 52 seconds and three-tenths of a cent. For a team running the
same weekly digest in CI, that last one is the difference between "don't
re-run it" and "re-run it whenever".

## Findings

### Finding 1 — `--stream` silently runs a degraded runtime: workspace calls fail and the run is never persisted

**Severity: high (silent behavior divergence).** The same invocation that
works plainly:

```
$ chidori run ask.ts --input '{"messages":["hi"]}'            # works
$ chidori run ask.ts --input '{"messages":["hi"]}' --stream
{"record":{...,"error":"chidori.workspace requires CHIDORI_WORKSPACE_ROOT
  or a runtime workspace root","function":"workspace",...}}
Error: uncaught JavaScript exception
```

`cmd_run_stream` (`crates/chidori/src/main.rs:1301`) builds its `Engine`
without `.with_workspace_root(...)` **and** without
`.with_persist_base(...)`, both of which the plain `run` path sets
(`main.rs:1239-1240`). Two consequences:

1. **Any workspace-using agent breaks the moment you add `--stream`** —
   the flag that docs and examples present as "same run, plus progress
   events". The error message even suggests an env var workaround for what
   is really a CLI wiring gap.
2. **A streamed run has zero durability**: no run id printed, no journal,
   no `.chidori/runs/` entry — nothing to `trace`, `resume`, or `verify`.
   The long, expensive runs are precisely the ones you stream, and they are
   the only CLI runs that evaporate. Nothing in `--help` or the docs says
   so; I found out from `ls`.

For a framework whose banner is *"every run is durable, replayable, and
resumable by default"*, `--stream` is a default-off switch nobody knows
they're flipping. Fix is presumably the two missing builder calls (plus
streaming persisted events), or at minimum a loud warning.

### Finding 2 — chat REPL conversations are ephemeral, and nothing says so

**Severity: medium.** `chidori chat` (both bare and through an agent file)
keeps the call log **in memory only** (`cmd_chat`, `main.rs:1430`): each
turn re-runs the agent with the prior log for replay — the mechanism is
sound and prior turns really do replay for $0 — but on exit the log is
dropped. No run id, no journal, no way to `trace` what the session cost,
`resume` yesterday's conversation, or `verify` it in CI. A crash loses the
conversation outright.

The quick start says *"Every turn is a recorded host call, so replaying
the whole conversation costs zero tokens"* — true within the session,
but the recording does not outlive the process, which is not what five
rounds of Chidori vocabulary have trained me to expect "recorded" to mean.
The interactive surface most likely to accumulate irreplaceable context
(a human typing) is the one surface with no durability. `chidori serve`
sessions persist by default in SQLite; the REPL deserves the same, or at
least an exit line: "session not persisted".

### Finding 3 — template errors swallow the cause

**Severity: medium-low, but every template consumer hits it.** A missing
variable — the number-one template mistake — reports:

```
× Error: Failed to render inline template
  │     at <anonymous> (tmpl_probe2.ts:3:19)
```

No variable name, no template line/column, no distinction between a
missing variable, a typo'd filter, or a syntax error. minijinja produces
precise, located errors ("undefined value … in template line 1, col 34");
the host call discards them for a generic string. The code frame points at
the *agent* line, which is the right half of the story — the template half
is missing. (The bad-*path* case is better: "Failed to read template file:
prompts/dialy.jinja" names the path, though not the directory it resolved
against, which matters because resolution is relative to the agent file,
not the cwd.)

### Finding 4 — the surfaces this round runs on are documented in one line each

**Severity: low, but it shapes the whole round.** `chidori.template` and
`chidori.memory` each get exactly one table row in `core-concepts.md`; no
dedicated doc, no mention in the README's feature list. Questions I could
only answer by experiment or by reading Rust: template path resolution
(agent dir, not cwd), the undefined-variable posture, which Jinja features
the engine supports; memory's namespace semantics (what makes it
`default`? when would it be something else?), concurrency behavior across
simultaneous runs, and the replay-vs-store interaction (excellent, and
documented nowhere). Meanwhile actors, signals, storage, and deployment
each have a full doc. The everyday surfaces deserve a fraction of that
ink — `compact()` has it (`context-management.md` is thorough and its §9
worked example matches reality); `template`/`memory` don't.

### Finding 5 — checkout papercut: the `file:` SDK dependency ships no `dist`

**Severity: papercut (repo checkouts only).** The examples' convention
`"@1kbirds/chidori": "file:../../sdk/typescript"` gives `tsc` a TS2688
(`Cannot find type definition file for '@1kbirds/chidori/agent-env'`)
until you know to run `npm run build` inside `sdk/typescript` — the
package's `exports` point at an untracked `dist/`. The npm-published
package is fine. A `prepare` script in the SDK's package.json would make
the checkout path work like the published one.

## Fixed since prior rounds (observed, not taken on faith)

- **Replay reporting now separates workspace re-materializations from live
  execution** — "26 recorded calls replayed, 7 workspace
  re-materialization(s), 0 executed live". Round 5's Finding 7 (a
  re-materialization reported as "1 executed live") is gone.
- **Unknown-model pricing says so and says what to do** — "Est cost:
  unknown (no pricing data for: deepseek-v4-flash; supply rates via
  CHIDORI_PRICING)" — and `CHIDORI_PRICING` is applied at *display* time,
  so runs recorded before I set it were priced retroactively. Round 3
  asked for exactly this.
- **Error frames still point at the failing `await`** with a real code
  frame (rounds 1–2's ask) — confirmed again on the missing-week probe.
- **Failed runs persist a debuggable journal** (the `week3` probe left a
  one-call run I could trace) — while `--stream` failures leave nothing,
  which sharpens Finding 1.

## Verdict

The question this round asked: does Chidori hold up as the thing you *live
in* — the daily conversational agent with real state, real prompt
hygiene, and a context window that fills up? The engine's answer is an
unqualified yes: one journal absorbed a SIGKILL, a window compaction, a
cross-run memory handoff, and three layers of caching, and replayed the
whole thing byte-identically for nothing. The economics are almost comic —
two weeks of team history digested, briefed, questioned, crashed,
recovered, and re-verified for under a cent.

The consumer's answer is "yes, but stay off the decorated paths". The two
flags that make the daily experience *nicer* — `--stream` for progress,
`chidori chat` for talking — are exactly where durability quietly stops:
one breaks workspace agents and journals nothing, the other discards the
conversation on exit. Both are wiring gaps, not design flaws, and both are
in `main.rs` rather than the engine. That is this codebase's recurring
shape, five rounds running: the core is armored; the on-ramps are where
the consumer trips.

---

## Appendix: selected verbatim output

Week-1 trace tail (DeepSeek cache reads growing turn over turn; pricing
applied retroactively at display time):

```
main  #23   7227ms  prompt  {...,"model":"deepseek-v4-flash",...} [695→552 tok, 2176 cache-read]
main  #26  12237ms  prompt  {...} [340→1041 tok, 2816 cache-read]
main  #30   9056ms  prompt  {...} [443→651 tok, 3072 cache-read]
main  #31      0ms  input   {"prompt":"Publish the week1 brief?"}
main  #32      0ms  workspace {"action":"write",...,"path":"briefs/week1.md"}
Tokens:   3952 in / 4148 out
Cache:    11008 read / 0 written (prompt-cache tokens)
```

Week-2 resume after `kill -9` (compaction firing inside the live
continuation — #21 is the summarizer):

```
main  #19   9062ms  prompt  {"context_segments":10,...} [604→758 tok, 1792 cache-read]
main  #21   4363ms  prompt  {"max_tokens":4096,...} [1663→356 tok]
main  #24   7093ms  prompt  {"context_segments":8,...} [64→510 tok, 1408 cache-read]
main  #25      0ms  log     {"fields":{"compacted":true,"day":"day-5","estTokens":1605},...}
...
Resumed from 9bb731db (17 recorded calls replayed, 6 workspace
re-materialization(s), 10 executed live)
```

Checkpoint-as-test on the crash-spanning run, and the local prompt cache:

```
$ chidori verify agent.ts 9bb731db-...
verified: 26 calls replayed, 7 workspace re-materialization(s), output identical — $0
real  0m0.037s

$ chidori run agent.ts --input week=week1 --trusted   # identical rerun, cache warm
real  0m0.092s                                        # was 51.7s / $0.0031
```

Full demo source: [`examples/standup-scribe/`](../examples/standup-scribe/).
