# Consumer usability review, round 2: the multi-agent surface

**Date:** 2026-07-16 · **Chidori:** 3.6.0, built from source at `1694faa` ·
**Perspective:** the same kind of user as
[round 1](./consumer-usability-review.md) — a developer whose provider is
DeepSeek — but this time building on the features round 1 explicitly could
not vouch for: **actors, branching, detached agents, and the crash-recovery
story when all of them are in play**.

Round 1 established that the linear story works (provider onboarding,
replay, resume guards, human-in-the-loop over HTTP) and its fixes have
landed — this round could feel them: `CHIDORI_OPENAI_COMPAT_URL` worked
first try, the truncation warning fired at exactly the right moment, cost
lines say `unknown` instead of `$0`, and error frames now point at the
failing `await`, not `run(`. Credit where due: the polish shipped.

This round asks the next question: **can a consumer actually build the
multi-agent systems the README sells?** Short answer: yes on the happy
path — strikingly so — and the durability engine underneath is real. But
the moment a *supervised* thing fails, the supervision surface tells you
nothing, retries nothing, and in one case bricks the agent. The failure
paths of the fault-tolerance features are where the consumer trust burns
down.

## What was built

A **multi-agent newsroom** (~150-line editor + 2 worker modules + 2
strategy modules + 2 real HN tools), all on live `deepseek-v4-flash`:

1. The editor plans two research angles (one `prompt`).
2. Two supervised **researcher actors** (`actors.spawn`, `restart:
   "resume"`) work the angles concurrently against the Hacker News Algolia
   API, streaming `progress` messages to the parent (`actors.send` /
   `receive`).
3. A **critic actor** reviews the combined dossier.
4. `chidori.branch` forks **two synthesis strategies** (exec brief vs.
   narrative feature, `concurrency: 2`) from the same anchored state; one
   more prompt picks the winner.
5. `chidori.input()` gates publication; `workspace.write` publishes.

Plus a **detached news-desk service** (`agents.spawn`, hibernating
`signal(["story","digest_now","close"])` loop with a 24h digest alarm),
driven over `chidori serve`'s HTTP surface, and a **flaky-upstream lab**
(a local HTTP server that 500s once then 200s) to test `restart: "resume"`
semantics honestly. Sources in the appendix.

## The numbers first

| Scenario | Result |
|---|---|
| Full newsroom pipeline, first attempt | **Completed first try.** 106s, 175 recorded calls, 17 prompts, 28 real tool calls, 15.7k in / 3.9k out tokens |
| Replay of that run (invalid API key on purpose) | **Byte-identical, 79ms, zero provider calls** — actors and branches included |
| Branch edit-and-rerun from stored anchor | Worked (once the model default was worked around) |
| Detached desk: wake→triage→hibernate cycles | Worked; journal grew 3→6→9 records, alarm deadline persisted |
| Detached desk: `kill -9` the server, restart, request digest | **Digest correctly covered state from before the kill** |
| `kill -9` the newsroom mid-fan-out, `chidori resume` | **Failed after 8 minutes** (see Finding 1) |
| Actor `restart: "resume"` against a flaky upstream (500 once, then 200) | **Never retried the upstream: 1 hit across 3 attempts** (see Finding 2) |
| Detached agent whose spawning `chidori run` exited mid-execution | **Bricked permanently** (see Finding 4) |

The two halves of that table are the review in miniature: the *recording*
half of the durability engine is superb; the *recovery* half — the reason
a consumer reaches for supervision options at all — failed every
non-trivial test I threw at it.

---

## Finding 1: a `--trusted` run cannot be crash-resumed

The README's crash-recovery pitch — "kill the process mid-run and resume
exactly where it left off" — held in round 1 for a linear agent. For the
newsroom it does not hold, and the reason is a missing flag:

- The run was started `chidori run … --tools tools --trusted`.
- SIGKILL mid-fan-out (43 parent records journaled; the actors' in-flight
  work correctly discarded per the documented at-least-once window).
- `chidori resume newsroom.ts <id> --model deepseek-v4-flash` then ran for
  **8 minutes** and died with `newsroom: researchers timed out`.

What happened, reconstructed from `cmd_resume` and the journals: `resume`
accepts neither `--tools` nor `--trusted`. Tools happen to load anyway
(an undocumented `<dir>/tools` convention). But the resume engine is built
with **no policy at all — deny-by-default** — so every re-spawned
researcher's first `chidori.tool` call was refused, each actor burned its
full restart budget re-hitting a deterministic denial, and the parent sat
in `receive()` until its own 480s timeout. The consumer paid for the
re-spawned researchers' live prompts *twice* (once per restart wave) and
got nothing.

The trust decision was made at `run` time, by a human, at a terminal. The
resume of *the same run* should at minimum offer the same flags
(`--trusted`, `--tools`), and arguably should default to the recorded
run's policy. Right now the marquee scenario — crash recovery of a real
tool-using agent — is unreachable from the CLI for exactly the runs that
need it.

Related paper cut discovered on the way: the README's own zero-cost-replay
command (`chidori resume agent.ts <run-id>`) fails for any run started
with `--model`: the model default (`claude-sonnet-4-6`) diverges from the
recorded prompt args, and the error says *"The agent code (or its inputs)
changed since the checkpoint was saved"* — it didn't — and recommends
`CHIDORI_REPLAY_LAX=1`, which is the wrong fix (the right one is re-passing
`--model`). The run knows its model; the manifest should carry it so
`resume`, `branch-rerun`, and fleet wakes stop guessing (see Finding 5).

## Finding 2: `restart: "resume"` cannot retry a flaky upstream through a tool

This is the deepest one, because it defeats the *stated purpose* of the
`resume` restart strategy ("the failing call re-executes live").

Lab setup: a tool whose `fetch` hits a local server that returns 500 on
the first request and 200 forever after; the worker actor does one LLM
call then the tool call, spawned with `restart: "resume", maxRestarts: 2`.
Expected: attempt 1 fails on the 500; restart replays the (cached) prompt,
re-executes the tool, gets the 200, completes with `restarts: 1`.

Observed: `status: "failed", restarts: 2` — and the upstream server
received **exactly one request across all three attempts**.

The journal explains it: the tool's inner `fetch` is recorded as its own
nested `http` record, and a fetch that returns a 500 is a *successful*
http effect — response received, effect complete. So when the restart
strips the crash frontier, it strips the failed `tool` record but keeps
the completed `http` record beneath it; the re-executed tool replays the
cached 500 and throws identically, forever. The restart budget burns with
no possibility of a different outcome, and nothing tells you the "retry"
never touched the network.

For the most common real-world flake there is — an upstream 5xx/timeout
inside a tool — supervision with `resume` is currently a slower way to
fail. The frontier strip needs to cascade to the failed call's *nested*
effects (they were consumed by the failing iteration; replaying them is
exactly the "re-firing a recorded call" the docs promise not to do in
reverse), or tools need a first-class way to say "this result is a
failure, don't cache it as done".

## Finding 3: actors die silently — a `receive()`-driven parent starves

The natural way to write a fan-out/fan-in (it's what the shipped
`actor_pipeline.ts` teaches) is: spawn workers, then `receive()` results
until you have N. The newsroom does exactly this. When both researchers
failed during the broken resume of Finding 1, the parent learned nothing:
no message, no exception, no wake-up — it blocked until its own 480s
timeout, because **an actor's death delivers nothing to anyone**.

The model this borrows from solved this decades ago: Erlang processes
have links and monitors; a supervisor gets a `DOWN` message the moment a
child dies. Chidori has the mailbox machinery already — a runtime-delivered
`{ name: "__chidori.down__", payload: { pid, error } }` (or an
`onSettle` option on spawn, or letting `receive` fail fast when every
possible sender has settled — the machinery for that exists in the
no-live-actors check) would turn a 480-second silent starvation into an
immediate, actionable signal. Until then, every collection loop must be
written defensively with `join({timeoutMs})` polling instead of the
message-driven style the examples teach.

## Finding 4: a detached agent can wedge permanently, and its mailbox is quicksand

Reproduced twice: `chidori run spawn_desk.ts --trusted` spawns the desk,
sends it a tip, and exits — while the desk is still mid-first-execution
(`status: "running"`). The docs say live agents die with the process and
"lose nothing". What actually happened:

- `chidori serve` boots, prints `Re-armed 1 detached agent(s) from the
  registry`, takes the desk's lease — and then no worker process ever
  runs the desk. The lease silently expires five minutes later.
- The registry (and `GET /agents/detached/news-desk`) reports `running`
  **forever** — status is a stored descriptor, not a liveness probe —
  with `waitingFor: null`, an empty journal, and an unconsumed mailbox.
- A new `POST /send` dutifully returns `{"delivered": true}` and queues
  the message; nothing will ever read it. There is no error anywhere: not
  in `serve`'s log, not in the status, not in the journal.

The only recovery is `POST /stop` + respawn — and the respawn is a **new
run id**, so every message queued to the wedged incarnation (my EU-story
tip, still sitting in the old run's `signals/inbox.json`) is silently
orphaned. "Durable mailbox any party can deliver into" is the pitch;
deliveries during a wedge are accepted and then stranded.

Three separable asks: (a) whatever killed the boot-time wake of a
mid-run agent needs to fail *loudly* (registry → `failed`, error in the
descriptor); (b) status for a supposedly-running agent should be checked
against an actual lease/worker, not parroted from disk; (c) a named
respawn should inherit (or at least warn about) the predecessor's
unconsumed inbox.

Once past the wedge, the lifecycle genuinely shines — wake-on-send,
re-hibernate, a 24h alarm deadline that survived `kill -9` of the server,
and a digest that correctly folded in state from before the kill. This is
the best feature in the framework wearing the worst failure mode.

## Finding 5: the run's model doesn't travel with the run

The same trap fired three independent times, in three costumes:

1. `chidori resume` → replay divergence with a misleading "code changed"
   error (Finding 1).
2. `chidori branch-rerun` → live 400 from DeepSeek: *"you passed
   claude-sonnet-4-6"* — the rerun forgot the run's `--model` and
   `branch-rerun` has no `--model` flag at all (env var only).
3. Any fleet wake under a server started without `--model` would do the
   same to a detached agent's next prompt.

A run whose every prompt was recorded with `model: deepseek-v4-flash`
knows its model. Stamp it in the run manifest and make every out-of-band
re-entry (`resume`, `branch-rerun`, `branch-resume`, registry wakes)
default to it. For a user on the two blessed vendors this bug is
invisible; for everyone else it's a recurring toll booth.

## Finding 6: the multi-actor trace is write-only

`chidori trace` on the newsroom run prints 175 lines in one flat list:
parent records interleaved with `#1000000000001`-style 13-digit sequence
numbers, no indication of *which actor* a record belongs to, no
grouping, no tree. Half the parent's lines are the `receive`/`log` spam of
its own progress loop. Finding "what did researcher-2 actually do" means
grepping seq prefixes by hand (and knowing the range-carving scheme from
`docs/actors.md`). The OTLP story is presumably better, but the built-in
tool — the one a consumer debugs with at 2am — hasn't caught up with the
process model: it needs per-actor grouping/labels (`actors.spawn` takes a
`name`; the trace never shows it), a tree view, and a `--actor <pid>`
filter.

Also in the "observability debt" bucket:

- **Prompt-cache telemetry is journaled but invisible.** The records
  carry `cache_read_tokens` (DeepSeek's automatic prefix cache was
  hitting — 512 tokens on one call), but neither `trace` nor `stats`
  prints a word about cache. For a framework that sells "structural
  prompt caching built in", the operator cannot see whether it works.
- **Unknown-model pricing is a dead end.** `Est cost: unknown (no pricing
  data for: deepseek-v4-flash)` is honest (round 1 fix), but there is
  still no way to *teach* it — no `CHIDORI_PRICING` env/config. One JSON
  map away from useful.
- **`stats` ignores everything below the parent?** No — it aggregates
  fine; but it has no per-actor / per-branch breakdown either.

## Finding 7: assorted first-day friction, ranked

- **`chidori serve` requires an agent file even when you only want the
  fleet.** The docs call the server "the natural home for a fleet", but
  you cannot host one without also exposing some session agent; there's
  no `chidori serve --fleet-only`, and no server-side way to spawn a
  detached agent (spawning is run-only, so "deploy a service" means
  "write a spawner run and execute it once").
- **`chidori.input()` can't show the human what they're approving.** The
  approval gate takes a prompt string only; my draft had to be inlined
  into the prompt text as a `---`-fenced blob to be reviewable at the
  terminal. An `attachment`/`document` option (rendered by the CLI,
  carried in `pending_prompt` over HTTP) matches how approval gates
  actually get used.
- **The built-in tool loop is a secret.** `prompt(text, { tools,
  maxTurns })` runs a provider-side tool loop — exactly what most agents
  want — but its only documentation is one line in `llm.txt`
  ("`maxTurns`: cap on provider tool-use turns"); every doc and example
  teaches the hand-rolled `respond()`/`toolResult` loop instead. I wrote
  40 lines of loop I may not have needed; I still don't know the
  differences (does it journal per-turn? honor `type: "progress"`
  streams? surface toolCalls?) because nothing says.
- **No lock on a run dir.** A wedged `resume` I forgot to kill and a
  second `resume` of the same run ran concurrently against the same
  `.chidori/runs/<id>` with no complaint. Detached agents have leases;
  plain runs have nothing.
- **SIGKILL eats the run's stdout.** Output (including the run id line)
  is block-buffered when redirected to a file, so a crashed run's log is
  0 bytes; the run id must be recovered from `ls .chidori/runs`.
  Line-buffer stdout (or `eprintln` the run id at start, which is where
  the crash-recovery user needs it anyway).
- **SSRF guard vs. local tools.** `fetch` from a tool to `127.0.0.1` is
  refused by default — right default, excellent error (it names
  `CHIDORI_HTTP_ALLOW_HOSTS`) — but anyone whose *tool* talks to a local
  service (Ollama sidecars, local indexes) hits it even under
  `--trusted`. Worth one line in the tools doc. (Provider calls to
  localhost are unaffected — verified — so local-LLM users are fine.)
- **Docs drift.** `docs/branching-execution.md` says the branch store is
  `branches/op-<branch seq>/`; it's actually zero-padded
  (`op-00000003000000000002`), which costs a confused minute mid-debug.

## What worked — and it's the hard part, again

Fairness requires the same list rigor as the complaints:

- **The whole newsroom composed first-try on a non-blessed provider.**
  Actors messaging the parent while branches fork strategy modules while
  tools hit real HTTP — plain TypeScript, no graph, no YAML — and DeepSeek
  reasoning-model tool-calling just worked through the compat provider.
- **Replay absorbed the entire process tree.** 175 calls including two
  actors' folded histories and a 2-way branch fan-out: 79ms, invalid API
  key, byte-identical output. Nobody else in this space has this as a
  one-liner.
- **Detached-agent durability is real.** Hibernation held zero threads;
  the alarm deadline and triage state survived `kill -9` of the server;
  the digest after restart knew everything from before it. The
  Durable-Objects-shaped model on a laptop, as advertised.
- **Branch edit-and-rerun** is the agent-iteration workflow I've wanted:
  tweak one strategy's stored source, re-run only that branch from the
  identical anchored state, compare.
- **Round 1's fixes held up in anger**: the truncation warning fired on
  the first reasoning-model response that hit its cap; the
  endpoint-named provider errors ("OpenAI-compatible endpoint
  api.deepseek.com") de-confused every failure; `input()` honored its
  default at EOF through the approval gate.

## The consumer verdict

Would I build on this today? For a **single durable agent with
human-in-the-loop** — yes, without hesitation; that path is now smooth
end-to-end, and the replay/checkpoint-test story is a genuine unlock. For
the **multi-agent surface**, the primitives are the right primitives and
the happy path is shockingly good — but I'd be knowingly signing up to
hand-roll the safety net the framework advertises: defensive
`join({timeoutMs})` polling instead of trusting `receive`, my own
retry-with-jitter inside tools because `restart: "resume"` can't retry a
5xx, a watchdog that pokes detached agents because `running` might mean
"dead", and a sticky note that says *never crash-resume without
re-deriving every flag the original run had*.

Every one of those is fixable at the surface (a flag, a monitor message,
a cascaded frontier strip, a loud registry error, a manifest field) —
none require touching the engine, which is visibly the strongest thing
here. Round 1 ended "ship the polish"; round 2's version is: **ship the
failure paths.** The features work; it's their *failures* that don't.

---

## Appendix A: hard-evidence log

- Newsroom run `137674ee` — 175 calls, 106,529ms, 15,722/3,947 tokens;
  replay with `CHIDORI_OPENAI_COMPAT_KEY=sk-invalid…`: identical output,
  `0m0.079s`.
- Crash run `3b3c156e` — SIGKILL at 18s; 43 parent records (1 prompt, 2
  `spawn_actor`, 19 `receive`); `resume … --model deepseek-v4-flash`
  exited 1 after ~8min: `newsroom: researchers timed out` at
  `newsroom.ts:52` (the parent's own receive timeout).
- Flaky lab — server hit-counter file read `1` after a run that reported
  `restarts: 2`; journal holds the 500-body `http` record as a completed
  effect at seq `1000000000003`.
- Desk wedge — registry `running`, `listen: null`, `restarts: 0`, no
  `records.jsonl`, lease expired `22:40:54Z`, serve log shows only
  `Re-armed 1 detached agent(s)`; `POST /send` → `{"delivered":true}`
  into the orphaned inbox.
- Desk (healthy incarnation, run `7f066b4e`) — journal 3 records after
  first triage, 6 after wake-on-send, 9 after post-restart digest; digest
  text referenced both triaged tips; alarm `deadline` persisted across
  server SIGKILL.

## Appendix B: the newsroom (abridged)

`newsroom.ts` — the editor:

```ts
import { chidori, run, type AgentJson, type JsonObject } from "chidori:agent";

run(async (input: { topic: string }) => {
  const planRaw = await chidori.prompt(
    `…propose exactly 2 research angles… Reply as a JSON array…`,
    { type: "progress", format: "json", maxTokens: 2000 },
  );
  const angles = Array.isArray(planRaw) ? planRaw.slice(0, 2) : [input.topic];

  const researchers = [];
  for (const [i, angle] of angles.entries()) {
    researchers.push(await chidori.actors.spawn(
      "workers/researcher.ts",
      { topic: input.topic, angle, id: i + 1 },
      { name: `researcher-${i + 1}`, restart: "resume", maxRestarts: 2 },
    ));
  }

  const findings: JsonObject[] = [];
  while (findings.length < researchers.length) {
    const msg = await chidori.receive(["progress", "finding"], { timeoutMs: 480000 });
    if (msg.timedOut) throw new Error("newsroom: researchers timed out");
    if (msg.name === "finding") findings.push(msg.payload as JsonObject);
  }
  for (const r of researchers) await r.join();

  const dossier = findings.map((f, i) => `### Angle ${i + 1}: ${f.angle}\n\n${f.findings}`).join("\n\n");

  const critic = await chidori.actors.spawn("workers/critic.ts", { topic: input.topic, dossier }, { name: "critic" });
  const critique = ((await critic.join()).output as JsonObject)?.critique as string;

  const outcomes = await chidori.branch([
    { label: "exec-brief",    source: "strategies/exec_brief.ts",    input: { topic: input.topic, dossier, critique } },
    { label: "feature-story", source: "strategies/feature_story.ts", input: { topic: input.topic, dossier, critique } },
  ], { concurrency: 2 });

  // …editor-in-chief prompt picks a draft; chidori.input() gates;
  // chidori.workspace.write publishes. Full flow as in round 1's appendix.
});
```

`workers/researcher.ts` — the supervised worker (the budget-nudge shape
that made round 2's output publication-quality):

```ts
run(async (input: { topic: string; angle: string; id: number }) => {
  let ctx = chidori.context()
    .system("You are a research analyst… use hn_search / hn_thread… read ≥2 threads…")
    .tools(["hn_search", "hn_thread"])
    .user(`Topic: ${input.topic}\nYour assigned angle: ${input.angle}`);

  let findings = ""; let toolCalls = 0;
  for (let step = 0; step < 6; step++) {
    const stepsLeft = 6 - step;
    if (stepsLeft <= 2) ctx = ctx.user(stepsLeft === 2
      ? "Budget check: ONE more round of tool calls, then write up."
      : "Budget exhausted. Write your findings NOW. Make NO tool calls.");
    const { response, context } = await ctx.respond({ type: "progress", maxTokens: 3000 });
    ctx = context;
    if (response.toolCalls.length === 0) { findings = response.content; break; }
    for (const call of response.toolCalls) {
      const result = await chidori.tool(call.name, call.input);
      toolCalls++;
      await chidori.actors.send("parent", "progress", { id: input.id, tool: call.name });
      ctx = ctx.toolResult(call.id, JSON.stringify(result));
    }
  }
  if (!findings) {
    const { text } = await ctx.user("Stop. Summarize your findings now. No tool calls.")
      .prompt({ type: "progress", maxTokens: 3000 });
    findings = text;
  }
  await chidori.actors.send("parent", "finding", { angle: input.angle, findings, toolCalls });
  return { id: input.id, angle: input.angle, toolCalls };
});
```

`services/desk.ts` — the detached service:

```ts
run(async () => {
  const triaged: { headline: string; verdict: string }[] = [];
  let digests = 0;
  for (;;) {
    const msg = await chidori.signal(["story", "digest_now", "close"],
      { timeoutMs: 24 * 60 * 60 * 1000 });          // daily digest alarm
    if (msg.timedOut || msg.name === "digest_now") {
      digests++;
      await chidori.log("digest", { digest: await chidori.prompt(
        `Write a 3-sentence desk digest of: ${JSON.stringify(triaged)}`) });
      continue;
    }
    if (msg.name === "close") return { triaged: triaged.length, digests };
    const tip = msg.payload as JsonObject;
    const verdict = await chidori.prompt(`Triage this tip in one sentence: ${JSON.stringify(tip)}`);
    triaged.push({ headline: String(tip.headline ?? "?"), verdict });
  }
});
```

## Appendix C: reproduction commands

```bash
export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-...

# the pipeline
chidori run newsroom.ts --model deepseek-v4-flash \
  --input topic="AI coding agents" --tools tools --trusted

# zero-cost whole-tree replay (must repeat --model: Finding 5)
chidori resume newsroom.ts <run-id> --model deepseek-v4-flash

# Finding 1 (crash resume): SIGKILL the run mid-fan-out, then the resume
# above — actors' tool calls are policy-denied (no --trusted on resume),
# parent starves for its receive timeout.

# Finding 2 (flaky restart): tool fetches a server that 500s once then
# 200s; spawn with restart:"resume" — the upstream is hit exactly once.

# Finding 4 (desk wedge): chidori run spawn_desk.ts --trusted  (exits
# while the desk is mid-first-execution), then chidori serve …; the desk
# stays "running" forever with no worker.

# detached desk over HTTP
chidori serve spawn_desk.ts --port 8091 --model deepseek-v4-flash
curl -XPOST :8091/agents/detached/news-desk/send \
  -d '{"name":"story","payload":{"headline":"…"}}'

# branch iterate (env var, not a flag: Finding 5)
CHIDORI_MODEL=deepseek-v4-flash chidori branch-rerun <run-id> <branch-id>
```
