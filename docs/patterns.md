---
title: "Common Patterns"
description: "Task-oriented recipes: approval gates, tool loops, fan-out, multiplayer review, scheduled agents, and checkpoint tests — which primitive fits which job."
---

# Common patterns

Chidori has a small number of primitives that compose into most agent
shapes. This page maps jobs to primitives; each recipe links to the doc that
covers it in depth, and to a runnable example where one exists.

| I want to… | Reach for | Details |
|---|---|---|
| Ask an LLM, maybe with tools | `chidori.prompt` + `defineTool` | [Host API](./host-api.md#llm-calls) |
| Build a chat assistant | `chidori.conversation` | [Core Concepts](./core-concepts.md#conversational-agents) |
| Answer webhooks / arbitrary HTTP | `chidori serve` catch-all route | [below](#an-agent-behind-a-webhook) |
| Gate an action on human approval | `chidori.input` with `details` | [below](#approval-gates-that-show-their-work) |
| Wait for an outside party (human or agent) | `chidori.signal` | [Signals](./signals.md) |
| Run several prompts concurrently | `chidori.util.parallel` | [below](#fan-out-drafts-concurrently) |
| Compare whole strategies, not just prompts | `chidori.branch` | [Branching Execution](./branching-execution.md) |
| Run long-lived concurrent workers with supervision | `chidori.actors` | [Actors](./actors.md) |
| Run a service that outlives any one run | `chidori.agents` + `chidori.alarm` | [Detached Agents](./detached-agents.md) |
| Remember things across runs | `chidori.memory` | [Memory](./memory.md) |
| Avoid re-paying expensive compute on resume | `chidori.step` | [Value Checkpoints](./value-checkpoints.md) |
| Pin agent behavior in CI | `chidori verify` | [below](#checkpoint-as-test) |

## Approval gates that show their work

Never make a human approve blind: pass the artifact under review as
`details`. The CLI prints it above the prompt; a paused session exposes it
as `pending_details` next to `pending_prompt`.

```ts
const draft = await chidori.prompt("Draft the announcement.", { type: "draft" });

const verdict = await chidori.input("Ship this announcement?", {
  type: "approval",
  choices: ["yes", "no"],
  default: "no",
  details: draft,
});

if (verdict.toLowerCase() !== "yes") return { shipped: false };
```

Under `chidori serve`, the `input()` suspends the session to disk — no
process waits while the human decides. Days later,
`POST /sessions/{id}/resume` picks the run up exactly where it paused.
Example: [`examples/agents/input_pause.ts`](../examples/agents/input_pause.ts).

## An agent behind a webhook

Under `chidori serve`, any route other than the `/sessions/*` API is folded
into `{ event: { method, path, headers, query, body } }` and run as the
agent's input; returning `{ status, body, headers? }` shapes the HTTP
response:

```ts
import { chidori, run } from "chidori:agent";

run(async (input: { event: { method: string; path: string; body?: unknown } }) => {
  const { event } = input;
  if (event.method !== "POST" || event.path !== "/hooks/pr") {
    return { status: 404, body: { error: "not found" } };
  }
  const triage = await chidori.prompt(
    `Triage this pull request event:\n${JSON.stringify(event.body)}`,
    { type: "final" },
  );
  return { status: 200, body: { triage } };
});
```

Branch on `input.event` early, as above: **every** request runs the whole
agent, so return a cheap `4xx` for non-events before any model call. A run
that **pauses** (on `input()`, a signal, or a policy approval) is stored as
a real session and answered `202` with the session view, so the caller can
resume or signal it later — a webhook can open a long-lived, durable
workflow. Deep doc: [Running Modes](./running-modes.md) (which also covers
*outbound* requests from a handler —
[`examples/agents/webhook.ts`](../examples/agents/webhook.ts)).

## Give the model tools, keep the loop

`prompt()` with `tools` runs the whole provider tool-use loop for you.
Hand-roll with `context().respond()` / `toolResult()` only when you need
per-step control:

```ts
const answer = await chidori.prompt(input.question, {
  tools: [searchNotes, wikiSearch], // defineTool handles and/or registry names
  maxTurns: 6,
});
```

Every model turn and tool invocation is journaled, so the loop shows up in
`chidori trace` — and replays for $0. Examples:
[`examples/agents/tool_use.ts`](../examples/agents/tool_use.ts) (built-in
loop), [`examples/agents/worker.ts`](../examples/agents/worker.ts)
(author-driven loop).

## Fan out drafts, concurrently

```ts
const [a, b, c] = await chidori.util.parallel(
  [
    () => chidori.prompt("Draft: crisp and technical", { type: "draft" }),
    () => chidori.prompt("Draft: warm and story-led", { type: "draft" }),
    () => chidori.prompt("Draft: contrarian angle", { type: "draft" }),
  ],
  { concurrency: 3 },
);
const winner = await chidori.prompt(
  `Pick the strongest draft and say why:\n\nA:\n${a}\n\nB:\n${b}\n\nC:\n${c}`,
  { type: "final" },
);
```

`util.parallel` is in-VM control flow — only the prompts inside are durable
calls. When the *strategies* differ (different code, not just different
prompts), use `chidori.branch` instead: each variant runs its own module
from the current state, and the whole fan-out replays as one recorded call.
Example: [`examples/branching/`](../examples/branching/).

## Multiplayer review

A run can wait on named signals from several parties — humans or other
agents — with a durable mailbox absorbing whatever arrives early:

```ts
// Wait for either reviewer; the result's `name` says who fired.
const first = await chidori.signal(["design-review", "security-review"], {
  timeoutMs: 24 * 60 * 60 * 1000,
});
if (first.timedOut) return { escalated: true };
```

Deliver with `POST /sessions/{id}/signal` — the server resolves a matching
pause, pushes to a live streaming run, or queues into the mailbox. Example:
[`examples/multiplayer-review/`](../examples/multiplayer-review/), deep doc:
[Signals](./signals.md).

## A service that sleeps between events

A detached agent owns its own run, journal, and mailbox, and **outlives the
run that spawned it**. Waiting costs nothing: `signal()` hibernates with no
thread and no VM, and `alarm()` is the durable "wake me every N hours"
timer:

```ts
// services/inbox-triager.ts
import { chidori, run } from "chidori:agent";

run(async () => {
  for (;;) {
    const msg = await chidori.signal(["email", "shutdown"], {
      timeoutMs: 6 * 60 * 60 * 1000, // or use chidori.alarm for pure maintenance ticks
    });
    if (msg.timedOut) { await runMaintenance(); continue; }
    if (msg.name === "shutdown") return { stopped: true };
    await triage(msg.payload);
  }
});
```

Spawn it once with `chidori.agents.spawn(..., { name: "inbox-triager" })`;
send to it from any run or via `POST /agents/detached/{name}/send`. The
fleet re-arms at boot, so a server restart loses nothing. Deep doc:
[Detached Agents](./detached-agents.md).

## Don't re-pay expensive compute

Replay re-executes your TypeScript. That's usually free — but a genuinely
expensive *pure* computation (parsing a big corpus, building an index)
would re-run on every resume. Journal its value once:

```ts
const index = await chidori.step("build-index", () => buildIndex(corpus));
```

Resume returns the recorded value without re-running the callback. The
callback must be pure, synchronous compute — host effects inside a step
throw. Deep doc: [Value Checkpoints](./value-checkpoints.md), example:
[`examples/agents/value_checkpoint.ts`](../examples/agents/value_checkpoint.ts).

## Checkpoint-as-test

A recorded run is a complete, deterministic specification of your agent's
behavior. Commit one and assert against it in CI:

```bash
chidori run agent.ts --input question="smoke test"   # record once
git add .chidori/runs/<run_id>                        # commit the recording
chidori verify agent.ts <run_id>                      # in CI: exit 0 = no drift
```

`verify` replays with no provider configured and a deny-all policy — if the
agent's prompts, tool calls, or control flow drift from the recording, it
fails. A full integration test that costs $0 and runs in milliseconds.
[Observing with Tael](./observing-with-tael.md) builds its golden regression
cases on the same mechanism.

## Keep long conversations inside the window

Compaction is explicit — it changes what the model sees, so it's an author
decision, and it's a recorded prompt call, so it replays deterministically:

```ts
const chat = chidori.conversation({
  system: PERSONA,
  compact: { budgetTokens: 8000 }, // no-op until the tail exceeds budget
});
```

Or on a raw context: `ctx = await ctx.compact({ budgetTokens: 8000 })`.
Deep doc: [Context Management](./context-management.md).
