---
title: "Your First Agent"
description: "A hands-on tutorial: write a durable agent from scratch, pause it for approval, replay it for $0, and check it into CI."
---

# Your first agent

[Getting Started](./getting-started.md) runs Chidori's pre-built examples.
This tutorial has you **write your own agent** and walks it through the full
durability loop: run it, trace its call log, pause it for a human, replay it
with zero LLM calls, and turn the recording into a CI test. Plan for about
fifteen minutes.

You need the `chidori` binary ([install](./getting-started.md)) and,
ideally, a provider key such as `ANTHROPIC_API_KEY`
([other providers](./host-api.md#providers--model-selection)).

> **No API key?** Set `CHIDORI_TEST_LLM_RESPONSE="(test reply)"` and every
> prompt call returns that static string instead of calling a provider — so
> the model won't actually exercise the tool, but the durability mechanics
> this tutorial is about (journaling, pause, replay, verify) behave
> identically.

## 1. Write the agent

An agent is one ordinary TypeScript file. Create a fresh directory and save
this as `research.ts`:

```ts
import { chidori, run, defineTool } from "chidori:agent";

const NOTES = [
  "2026-05-04 standup: replay divergence bug traced to unseeded RNG in retry helper.",
  "2026-05-11 standup: prompt cache hit rate at 87% after context() refactor.",
  "2026-05-18 standup: actors supervision tree shipped; joins fold logs correctly.",
];

// A tool is just a function with a documented signature. `run` executes in
// the agent's own VM, so closures over NOTES work, and every invocation is
// journaled for the trace.
const searchNotes = defineTool({
  name: "search_notes",
  description: "Keyword search over the team's standup notes.",
  parameters: {
    type: "object",
    properties: { query: { type: "string", description: "Search keyword" } },
    required: ["query"],
  },
  run: async ({ query }: { query: string }) =>
    NOTES.filter((n) => n.toLowerCase().includes(query.toLowerCase())),
});

run(async (input: { question: string }) => {
  await chidori.log("Researching", { question: input.question });

  // One call runs the whole provider tool-use loop: the model calls
  // search_notes, the runtime executes it and feeds results back, up to
  // maxTurns, then returns the final text.
  const answer = await chidori.prompt(
    `Answer from the standup notes, citing dates: ${input.question}`,
    { tools: [searchNotes], maxTurns: 4, type: "final" },
  );

  // Pause for a human. `details` carries the artifact under review, so the
  // approval is never blind.
  const ship = await chidori.input("Publish this answer?", {
    type: "approval",
    choices: ["yes", "no"],
    default: "no",
    details: answer,
  });

  return { answer, published: ship.toLowerCase() === "yes" };
});
```

Three things to notice before running it:

- **Every side effect goes through a host call.** `chidori.log`,
  `chidori.prompt`, `chidori.input` — each is recorded in the run's call
  log. The `if`s, `await`s, and string munging between them are plain
  TypeScript.
- **The tool needs no registration.** `defineTool` wraps a function in the
  `name`/`description`/`parameters` the model reads; you pass the handle
  straight into `prompt()`.
- **Type the input with an object type, not an `interface`** — interfaces
  fail the handler's JSON constraint with a confusing error.

## 2. Run it

```bash
chidori run research.ts --input question="what happened with the prompt cache?"
```

The model runs the tool loop — calling `search_notes`, reading the results,
composing an answer — and then the run **stops and asks you**:

```text
Publish this answer? [yes/no]
```

The answer text prints above the prompt — that's `details`. Type `yes`. The
run completes and prints its JSON output, something like:

```json
{
  "answer": "…the 2026-05-11 note reports an 87% prompt cache hit rate…",
  "published": true
}
```

> `chidori run` asks y/a/N approval before *powerful* effects — network
> access, `chidori.tool` calls, workspace writes. This agent's tool is pure
> in-VM compute, so the only pause you see is your own `input()` gate. See
> [Running Modes](./running-modes.md) for postures and `--trusted`.

## 3. Read the record

Every run journals to `.chidori/runs/<run_id>/` next to the agent file. Look
at what was recorded:

```bash
RUN_ID=$(ls -t .chidori/runs | head -1)
chidori trace "$RUN_ID"
```

The trace is the run's complete story: the `log` call, each model turn and
`search_notes` invocation inside the prompt loop, your `input()` answer, and
the token counts and cost of every prompt. This call log — not a framework
abstraction — is what makes everything in the next two steps possible.

## 4. Replay it for $0

```bash
chidori resume research.ts "$RUN_ID"
```

The agent code re-executes from the top — but every host call returns its
recorded result from the call log instead of touching the world. No model is
called, no tokens are billed, nobody is asked to approve anything, and the
output is byte-identical to step 2. This is the same mechanism that powers
crash recovery: a run that dies halfway replays to the frontier of its log
and **continues live from there** ([how replay works](./replay.md)).

Two variants worth knowing now:

- `chidori resume research.ts <run_id> --allow-source-change` — replay
  against *edited* agent code, divergence-checked. Fix a bug three runs deep
  without re-paying the first three runs.
- A paused server-mode session resumes the same way, minutes or days later,
  in a fresh process ([Signals](./signals.md)).

## 5. Turn the recording into a test

```bash
chidori verify research.ts "$RUN_ID"
```

`verify` replays the run with **no provider configured and a deny-all
policy** and asserts it completes with byte-identical output. Exit code 0
means the agent's behavior hasn't drifted from the recording. Commit
`.chidori/runs/<run_id>/` to git and this is a full integration test of your
agent — prompts, tool loop, approval gate and all — that costs $0 and runs in
milliseconds in CI. See [Value Checkpoints](./value-checkpoints.md) for
bounding replay cost as runs grow.

## Where next

- [Core Concepts](./core-concepts.md) — the full host-function surface and
  the mental model behind it.
- [Common Patterns](./patterns.md) — approval gates, fan-out, multiplayer
  review, scheduled agents: which primitive fits which job.
- [Host API Reference](./host-api.md) — every `chidori.*` method, option by
  option.
- `chidori serve research.ts --port 8080` turns this same file into an HTTP
  session API where `input()` pauses become resumable sessions —
  [Running Modes](./running-modes.md).
