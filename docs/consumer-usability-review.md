---
title: "Round 1: Linear Path"
description: "Round 1: building a real agent on 3.6.0 \u2014 the linear path."
---

# Consumer usability review: building a real agent on Chidori

**Date:** 2026-07-16 · **Chidori:** 3.6.0, built from source at `ea0e70e` ·
**Perspective:** a developer who found the repo, wanted to build something
real, and does **not** use Anthropic or OpenAI — their provider is DeepSeek
(an OpenAI-compatible API).

This is deliberately not a code review. It is a log of what it feels like to
*consume* the framework: install it, point it at a non-default provider,
build a non-trivial agent, and lean on the marquee features (replay, crash
resume, human-in-the-loop) the README sells.

## What was built

An **HN Research Analyst** (~110 lines of agent + two ~50-line tools): given
a topic, it plans search queries with the model, runs an author-driven tool
loop (`context().tools().respond()`) against two real HTTP tools hitting the
Hacker News Algolia API, synthesizes a Markdown briefing, pauses on
`chidori.input()` for human approval, and publishes to the workspace. It
exercises `prompt`, `context`/`respond`/`toolResult`, `tool`, captured
`fetch`, `input`, `log`, and `workspace.write` — plus `chidori resume`,
`chidori trace`, `chidori serve`, and the session HTTP API. Full source in
the appendix.

**Provider note:** the DeepSeek key initially had no billing balance
(`402 Insufficient Balance` on a direct `curl`), so the first round of
testing validated routing to the real endpoint and then substituted a
scripted OpenAI-compatible mock for the model itself. The key was later
funded and the full exercise re-ran against live DeepSeek
(`deepseek-v4-flash`, a reasoning model) — see
[Round 2](#round-2-the-same-agent-against-live-deepseek). All findings
below held in both rounds; Round 2 added several new ones.

---

## The verdict up front

The core promise is real. The parts of Chidori that are hardest to build —
and the reason to pick it — worked, first try, under deliberately hostile
testing:

- **`kill -9` mid-LLM-call, resume in a new process: exactly one provider
  call re-billed.** 13 journaled calls replayed instantly; only the
  interrupted prompt re-executed. This is the README's central claim and it
  held under SIGKILL, not just a polite shutdown.
- **Replay of a 16-call run: byte-identical output, zero provider calls.**
- **The resume guards are excellent.** Editing the agent source and resuming
  refuses with a message that names the opt-in flag; forcing it with
  `--allow-source-change` when the edit touched an already-recorded call
  fails loudly with a recorded-vs-current argument diff. Nothing silently
  re-executed a side effect.
- **Server-mode human-in-the-loop survived a hard server kill** between
  pause and resume.
- The agent authoring model is genuinely pleasant: plain `async` TypeScript,
  the whole agent in one readable file, `chidori trace` giving a clean
  timeline with per-call durations and token counts.

What stands between that engine and adoption is almost entirely **first-day
surface friction**: provider onboarding outside the two blessed vendors,
stale published types, and a cluster of undocumented environment variables
that turn marquee features into error messages. All of it is fixable
cheaply relative to what's already built. Ranked below by how much adoption
each one costs.

---

## Blocker 1: the provider story assumes a vendor the user may not have

The single most likely reason a first-day user bounces. The ecosystem
reality is that most teams evaluate agent frameworks against an
OpenAI-*compatible* endpoint that is not OpenAI — DeepSeek, Groq, Together,
Fireworks, vLLM, Ollama. For that user:

1. **There is no documented path.** The README offers OpenRouter login,
   `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and — only inside a comment in a
   code block — `LITELLM_API_URL`. Nothing says the LiteLLM pair is actually
   a *generic* OpenAI-compatible escape hatch. It worked perfectly against
   `https://api.deepseek.com`, but discovering that required reading
   `providers/mod.rs`. An env var named after a specific proxy product reads
   as "for LiteLLM users only."
2. **The OpenAI provider's base URL is hardcoded** (`providers/openai.rs`).
   `OPENAI_BASE_URL` is a de-facto ecosystem standard that dozens of tools
   honor; a DeepSeek/Groq/Ollama user will try it, watch it silently do
   nothing, and conclude the framework doesn't support them.
3. **The default model is `claude-sonnet-4-6` and the only global override,
   `CHIDORI_MODEL`, is documented nowhere** — not in the README, not in any
   `docs/*.md`, not in `llm.txt`. It was found by grepping
   `runtime/context.rs`. Without it, every `chidori.prompt()` from a
   DeepSeek user's agent asks the catch-all provider for a Claude model.
   Relatedly: `chidori chat` has `--model` but `chidori run` does not.
4. **Errors misattribute the provider.** The failed DeepSeek call surfaced
   as `OpenAI API error (402 Payment Required)` — technically the OpenAI
   *protocol* provider, but a user pointing at DeepSeek now has an error
   naming a company they don't use.
5. **Unknown models price as free.** `chidori trace` printed
   `Est cost: $0.000000` for `deepseek-v4-flash`. For a framework whose
   pitch is partly cost accounting, an unpriced model should say `unknown`,
   not `$0` — someone will paste that into a budget doc.

None of this is architectural. The catch-all provider already works; it
needs a vendor-neutral name (e.g. `CHIDORI_OPENAI_COMPAT_URL`, keeping the
old one as alias), `OPENAI_BASE_URL` support, a documented `CHIDORI_MODEL`,
a `--model` flag on `run`, and one README subsection titled "Any
OpenAI-compatible provider (DeepSeek, Groq, Ollama, vLLM…)". That
subsection is probably worth more new users than any feature on the
roadmap.

## Blocker 2: the published types fight the runtime

`npm install -D @1kbirds/chidori` (3.6.0) then `tsc --strict`, following the
README's own setup:

- The worker loop — written exactly like the shipped `examples/agents/worker.ts`
  and `llm.txt` — fails to compile: the published `LlmResponseJson` declares
  `tool_calls` while the runtime actually returns `toolCalls` (verified in
  `host_core.rs`; the in-repo SDK source is already correct). Worse than the
  error is the compiler's suggestion — *"Did you mean `tool_calls`?"* —
  which, if followed, **compiles clean and then breaks at runtime**: the
  agent reads `undefined`, sees "no tool calls," and silently returns
  without doing any work. A first-day user cannot tell whether the docs, the
  types, or the runtime is lying; the actual answer (stale npm publish) is
  invisible to them. This alone justifies publishing the SDK in lockstep
  with the binary and adding a CI check that the published types compile
  against the shipped examples.
- Declaring agent input as an `interface` fails the
  `TInput extends AgentJson` constraint with an inscrutable error
  (interfaces have no index signature — a classic TS trap). A `type` alias
  works. Every doc example happens to use inline literals, so nothing warns
  about this; one sentence in the SDK README ("use `type`, not `interface`,
  for input shapes") would save real debugging time.

## Blocker 3: `CHIDORI_WORKSPACE_ROOT` is a trap door under two marquee features

`chidori run` gives the agent a workspace root implicitly — `workspace.write`
just works, files appear under the project. Then:

- **`chidori resume` of that same run fails**:
  `chidori.workspace requires CHIDORI_WORKSPACE_ROOT or a runtime workspace
  root`. The flagship "replay any run byte-for-byte" breaks for any agent
  that writes a file, unless the user divines an env var that appears in no
  documentation (only `llm.txt` mentions it, parenthetically).
- **`chidori serve` never provides one**, so a workspace-using agent cannot
  complete under the session API at all until the operator sets the env var.
- The failure mode compounds: a paused session resumed against a
  misconfigured server ran to the `workspace.write`, threw, and the session
  became permanently `failed` — the human's approval apparently consumed.
  The data *was* recoverable (the answer had been journaled before the
  crash, and `POST /sessions/{id}/replay` completed the work under a new
  session id), but nothing tells you that; `resume` just says
  `Session is not paused`.

Three cheap fixes: default the workspace root on `resume` the same way `run`
does; make `serve` either default it or refuse to start a workspace-using
agent with a clear message; and when `resume` hits a failed-but-replayable
session, say so ("this session failed after its pause was answered; POST
/replay to re-drive it").

## Blocker 4: `input()` silently ignores its declared default at EOF

The approval gate declared `{ choices: ["publish", "discard"], default:
"publish" }`. Run non-interactively with stdin at EOF, `input()` returned
an **empty string** — not the declared default — and the agent silently
took the discard branch. Policy gates correctly fail *closed and loudly* in
non-TTY contexts; `input()` fails *open and silently* with a value the
author explicitly said should not be the fallback. In CI this is a wrong
branch taken with no error anywhere. Either honor `default` on EOF or fail
the run like the policy layer does.

## Friction 5: every runtime error points at line 1 of the agent

Every uncaught error — provider 402, policy refusal, workspace
misconfiguration, replay divergence — renders a beautiful miette frame
pointing at the same place: the `run(async …)` line. In a 110-line agent
with five prompts and two tools, "your agent failed at `run(`" is no
information at all; in a 1,000-line agent it would be actively painful. The
divergence errors prove the runtime knows the failing seq and call; the
frames just don't use the real call site. (This clearly improved recently —
#132 added stack traces — but at review time the top frame shown to the
user was still the registration site. Since fixed by the per-op position
table; see the note in the fixes section below.)

## Friction 6: CLI/server asymmetries a consumer trips on, in order

- `chidori serve` has no `--tools`; it silently loads `tools/` next to the
  agent file. The convention is fine — but it's documented nowhere, and the
  asymmetry with `chidori run --tools` means the first serve attempt of a
  tool-using agent is a head-scratcher.
- `run` asks for approval per gated effect (good), but the ask includes no
  "allow all for this run" option, so a 10-tool-call research run is ten
  keypresses; the docs push you straight to `--trusted`, which is
  all-or-nothing.
- Every invocation on this (containerized) Linux box prints
  `isolate worker: sandbox: landlock not enforced: no kernel support` to
  stderr — reasonable once, noise on every run, and slightly alarming as
  the first line a new user ever sees.
- `examples/tools/web_search.ts` is a stub returning `results: []`, but
  `llm.txt`'s `prompt` example passes `tools: ["web_search"]` as if it were
  real. An agent generated from that reference searches the void. The HN
  tools in the appendix took ten minutes to write; the bundled examples
  deserve one real HTTP-backed tool.

## Round 2: the same agent against live DeepSeek

Once the key was funded, the identical agent ran unmodified against
`deepseek-v4-flash`. The good news first:

- **It just worked, and worked well.** A 68-second run: 5 model turns, 13
  real tool calls across both HN tools, a genuinely publication-quality
  briefing, a 52-call journal. The OpenAI-compat provider handled a
  *reasoning* model's tool calling without any framework-side accommodation.
- **Replay proof got stronger.** Replaying the 52-call run with a
  deliberately **invalid** API key succeeded — byte-identical output. Zero
  provider calls is not a claim, it's enforced by construction.
- **Streaming works** through the OpenAI-compat path: clean per-token
  `prompt_delta` events. (The CLI's `--stream` prints raw JSONL event
  objects — great for piping, not a human-facing rendering.)

New findings only a real model could surface:

1. **Truncation is silent, and reasoning models make it likely.**
   The briefing was cut off mid-sentence: `deepseek-v4-flash` spends the
   completion budget on hidden reasoning before visible output, so the
   agent's `maxTokens: 1200` produced ~760 visible tokens and a hard stop.
   The provider reported `finish_reason: length` — but `chidori.prompt()`
   returns a bare string, so the author has **no way to see the stop
   reason** short of dropping to `context().respond()`. Nothing logged a
   warning; the truncated briefing sailed through the approval gate into
   the workspace. A `stopReason` on some richer return form (or at minimum
   a runtime warning log when a prompt stops on `length`) would have
   caught this.
2. **Sampling parameters aren't journaled.** The recorded `prompt` args
   contain `model`, `text`, `type`, and `request_digest` — but not the
   `maxTokens` that shaped the response (it *is* sent on the wire;
   verified in `providers/openai.rs`). Consequences: `chidori trace`
   can't show why a response stopped short, and the argument-level
   divergence check on resume is blind to an edit that changes
   `maxTokens`/`temperature` — it replays cached results as if nothing
   changed. (`request_digest` is explicitly ignored in divergence
   comparisons per `docs/replay.md`, so it doesn't backstop this either.)
3. **`reasoning_content` is dropped on the floor.** Reasonable as a
   default — it keeps journals lean — but there is no opt-in to see or
   record it, and users of reasoning models (an ecosystem-wide trend) will
   ask for it when debugging why a model burned 400 tokens before
   answering.
4. **The `$0.000000` cost line got worse with real numbers behind it:**
   `Tokens: 15087 in / 4443 out · Est cost: $0.000000` is now a concrete
   lie about a run that cost real money.

## What was *not* evaluated

Prompt-cache economics (DeepSeek's cache discount isn't in the cost
tables), `branch`, actors, detached agents, durable storage backends, and
the Python SDK. The docs for those read well, but this review can't vouch
for them.

## Status: fixes shipped

Every issue above was addressed on this branch except one, in the same series
of commits as this document:

- **Provider onboarding** — `CHIDORI_OPENAI_COMPAT_URL`/`_KEY` is the
  documented vendor-neutral pair (`LITELLM_*` stays as a legacy alias);
  `OPENAI_BASE_URL` is honored; errors name the endpoint actually configured
  ("OpenAI-compatible endpoint api.deepseek.com" instead of "OpenAI");
  `chidori run`/`resume` accept `--model`; `CHIDORI_MODEL` and the provider
  matrix are documented in the README and `llm.txt`.
- **Cost display** — unpriced models report `Est cost: unknown (no pricing
  data for: …)` in `trace` and `stats` instead of `$0.000000`.
- **Workspace root** — `resume`, `serve`, and the branch commands now provide
  the same implicit project-directory workspace root as `run`
  (`CHIDORI_WORKSPACE_ROOT` still overrides); a failed session's resume
  error now points at `POST /sessions/{id}/replay` as the recovery path.
- **`input()` at EOF** — an empty answer resolves to the declared `default`;
  EOF with no default fails loudly instead of silently returning `""`.
- **CLI/server asymmetries** — the `run`/`serve` tools asymmetry was resolved
  by removing the flag and the `tools/`-directory mechanism entirely: tools
  are now defined in-VM with `defineTool` and passed as handles, so there is
  nothing to load and no flag to forget; the approval prompt gained an
  `[a]ll further calls to this target` answer; sandbox degradation notes
  (the landlock line) print only under `--verbose` /
  `CHIDORI_ISOLATE_VERBOSE`, with `CHIDORI_ISOLATE_REQUIRE_SANDBOX`
  unchanged for enforcement.
- **Prompt metadata** — a response cut off at the `maxTokens` cap prints a
  truncation warning naming the seq and cap; `max_tokens`/`temperature` are
  journaled in prompt args (`trace` shows them; editing them is now a
  visible divergence, while old checkpoints without the keys still replay —
  the argument comparison tolerates keys absent from the recorded side);
  `reasoning_content` from reasoning models is captured on
  `LlmResponse.reasoning`, journaled, and exposed on `respond()`.
- **Example tool** — the `examples/tools/web_search.ts` stub went away with
  the `tools/` mechanism; `llm.txt`'s tool section now shows a real
  fetch-backed `defineTool` example instead of a stub, and the bundled
  examples (`examples/release-notes-concierge`, `examples/war-room`) define
  real HTTP-backed tools inline with `defineTool`.
- **Types** — the `interface`-vs-`type` input gotcha and the
  SDK-must-match-binary rule are documented in the SDK README. Republishing
  the npm package in lockstep with the binary release remains a
  release-process action this branch can't perform.

A second audit pass closed the residual gaps: the `chidori demo` provider
detection recognizes `CHIDORI_OPENAI_COMPAT_URL`; the init-template README,
deployment doc, and every CLI hint teach the vendor-neutral pair instead of
the LiteLLM alias; **detached agents** get the same implicit project-directory
workspace root as run/serve/resume; `llm.txt` warns about the
`interface`-vs-`type` gotcha (it is the reference AI code generators read);
and regression tests pin the new behaviors (target-wide approval cache,
tolerant-but-loud journal argument comparison, priced-vs-unknown cost).

**Fixed since this review: error frames anchored at `run(`.** At the time
of the investigation the engine's stack frames carried each function's
*definition site* (`FuncProto.source_line`) and the bytecode had no
per-instruction line table — pointing a frame at the failing `await` meant
adding a pc→span table to compiled functions and teaching the unwinder to
read the current pc. That engine work has since landed (#135): a per-op
position table (`FuncProto::pos`, index-parallel to the code and remapped by
every code-shortening pass) is threaded through both interpreter tiers, and
the unwinder records each frame at the position the throw crossed it — the
throwing statement for the innermost frame, the awaiting call for outer
frames. Host-raised failures (policy denials, provider errors, workspace
errors, unknown tools) now anchor at the gated call rather than the `run(`
registration line, with the definition site remaining only as the fallback
for synthetic protos. Regression tests pin the behavior in
`crates/chidori-js/tests/errors.rs` and
`crates/chidori/src/runtime/rust_engine.rs`
(`policy_denied_effect_frame_anchors_at_the_gated_call_not_run`).

## Closing

The hard part of this framework — the journal, the replay semantics, the
divergence guards, surviving SIGKILL with one re-billed call — is done and
it works. The things that will actually stop adoption are a stale npm
publish, four or five undocumented environment variables, and a provider
onboarding page that doesn't exist. That's a week of polish guarding a
moat. Ship the polish.

---

## Appendix: the agent

`agent.ts`:

```ts
/// <reference types="@1kbirds/chidori/agent-env" />
import { chidori, run, type AgentJson, type JsonObject } from "chidori:agent";

type Input = {
  topic: string;
  maxSteps?: number;
};

run(async (input: Input) => {
  const topic = input.topic;
  const maxSteps = input.maxSteps ?? 10;
  if (!topic) throw new Error("Provide --input topic=...");

  await chidori.log("Starting research", { topic });

  // 1. Plan search queries
  const plan = await chidori.prompt(
    `You are planning Hacker News research on the topic: "${topic}".\n` +
      `Propose 3 distinct search queries that would surface the most\n` +
      `informative stories and debates. Reply as a JSON array of strings,\n` +
      `nothing else.`,
    { type: "progress", format: "json", maxTokens: 200 },
  );
  const queries: string[] = Array.isArray(plan) ? plan : [topic];
  await chidori.log("Planned queries", { queries });

  // 2. Autonomous research loop
  let ctx = chidori
    .context()
    .system(
      "You are a research analyst investigating what the Hacker News " +
        "community thinks about a topic. Use hn_search to find stories " +
        "(try the suggested queries, but adapt based on what you find) and " +
        "hn_thread to read the discussions that look most substantive " +
        "(high points / many comments). Read at least 2 threads before " +
        "concluding. When you have enough material, reply with a final " +
        "answer summarizing your raw findings and NO tool calls.",
    )
    .tools(["hn_search", "hn_thread"])
    .user(
      `Topic: ${topic}\nSuggested queries: ${JSON.stringify(queries)}\n` +
        `Research this topic and report your raw findings.`,
    );

  const trail: { tool: string; input: JsonObject }[] = [];
  let findings = "";

  for (let step = 0; step < maxSteps; step++) {
    const { response, context } = await ctx.respond({ type: "progress" });
    ctx = context;

    if (!response.toolCalls || response.toolCalls.length === 0) {
      findings = response.content;
      break;
    }
    for (const call of response.toolCalls) {
      const result: AgentJson = await chidori.tool(call.name, call.input);
      trail.push({ tool: call.name, input: call.input });
      ctx = ctx.toolResult(call.id, JSON.stringify(result));
    }
  }
  if (!findings) {
    findings = "(research loop hit maxSteps; findings may be partial)";
  }
  await chidori.log("Research loop finished", { toolCalls: trail.length });

  // 3. Synthesize the briefing
  const briefing = await chidori.prompt(
    `Write a crisp research briefing in Markdown titled ` +
      `"HN Briefing: ${topic}".\n\nSections:\n` +
      `## Community pulse — 2-3 sentences on overall sentiment\n` +
      `## Key threads — the specific stories/debates found, with points/comment counts\n` +
      `## Strongest arguments — the best points made on each side\n` +
      `## Analyst's take — one paragraph of your own read\n\n` +
      `Base it ONLY on these raw findings:\n\n${findings}`,
    { type: "final", maxTokens: 1200 },
  );

  // 4. Human approval gate
  const verdict = await chidori.input(
    `Briefing drafted (${briefing.length} chars). Publish to workspace?`,
    { type: "approval", choices: ["publish", "discard"], default: "publish" },
  );

  if (verdict === "publish") {
    const slug = topic.toLowerCase().replace(/[^a-z0-9]+/g, "-");
    const path = `briefings/${slug}.md`;
    await chidori.workspace.write(path, briefing, { language: "markdown" });
    return { published: path, toolCalls: trail.length, briefing };
  }
  return { published: null, toolCalls: trail.length, briefing };
});
```

`tools/hn_search.ts`:

```ts
import type { Chidori, ToolDefinition } from "chidori:agent";

export const tool: ToolDefinition = {
  name: "hn_search",
  description:
    "Search Hacker News stories via the Algolia API. Returns the top stories " +
    "matching the query with title, points, comment count, date, and objectID " +
    "(use objectID with hn_thread to read the discussion).",
  parameters: {
    type: "object",
    properties: {
      query: { type: "string", description: "Search query" },
      sortBy: {
        type: "string",
        enum: ["relevance", "date"],
        description: "Sort by relevance (default) or most recent",
      },
    },
    required: ["query"],
  },
};

export async function run(
  args: { query: string; sortBy?: string },
  chidori: Chidori,
) {
  const endpoint = args.sortBy === "date" ? "search_by_date" : "search";
  const url =
    `https://hn.algolia.com/api/v1/${endpoint}?tags=story&hitsPerPage=8&query=` +
    encodeURIComponent(args.query);
  await chidori.log("hn_search", { query: args.query, url });
  const res = await fetch(url);
  if (!res.ok) {
    return { error: `Algolia returned HTTP ${res.status}` };
  }
  const data = await res.json();
  const hits = (data.hits ?? []).map((h: any) => ({
    objectID: h.objectID,
    title: h.title,
    url: h.url ?? null,
    points: h.points,
    numComments: h.num_comments,
    createdAt: h.created_at,
  }));
  return { query: args.query, hits };
}
```

(`tools/hn_thread.ts` is analogous: fetches
`https://hn.algolia.com/api/v1/items/{objectID}`, strips HTML from the top
12 comments, returns story metadata + comment texts.)

### Reproduction commands

```bash
# provider (any OpenAI-compatible endpoint)
export LITELLM_API_URL=https://api.deepseek.com
export LITELLM_API_KEY=sk-...
export CHIDORI_MODEL=deepseek-v4-flash        # undocumented, see Blocker 1

chidori run agent.ts --input topic="local-first software" --tools tools --trusted

# zero-cost replay (workspace agents also need the undocumented root, see Blocker 3)
CHIDORI_WORKSPACE_ROOT=. chidori resume agent.ts <run-id>

# crash recovery: SIGKILL the process mid-LLM-call, then the same resume —
# observed: 16 calls replayed, exactly 1 provider call re-executed

# human-in-the-loop over HTTP
CHIDORI_WORKSPACE_ROOT=$PWD chidori serve agent.ts --port 8090 --trusted
curl -s :8090/sessions -d '{"input":{"topic":"webassembly on the server"}}'
curl -s :8090/sessions/<id>/resume -d '{"response":"publish"}'
```
