---
title: "Core Concepts"
description: "Agents, host functions, and the mental model: the chidori.* surface, tools, streaming, and prompt caching."
---

# Core concepts: agents & host functions

An agent is a `.ts` file that imports `{ chidori, run }` from the virtual
`chidori:agent` module and registers its handler with `run(async (input) => …)`.
(The legacy form — an exported async `agent(input, chidori)` function — is still
accepted.) The runtime provides a fixed set of **host functions** for side
effects through the `chidori` object — agents never touch the outside world
directly, so the runtime sees and records everything. See
[`llm.txt`](../llm.txt) for the full API reference.

## Host functions

| Function | Purpose |
|---|---|
| `chidori.prompt(text, { type, ... })` | Send to an LLM, return string or parsed JSON; streamed prompt events carry the optional type |
| `chidori.context()` | Immutable multi-turn prompt builder with prefix sharing and provider prompt caching |
| `chidori.conversation(options)` | Stateful chat-assistant wrapper over `context()` — `say(message)` per turn, or `loop()` for an interactive `input()` dialogue |
| `chidori.template(strOrPath, vars)` | Render a Jinja2 template (inline string or `.jinja`/`.j2` file) with minijinja — undefined variables fail loudly ([details](./template.md)) |
| `chidori.tool(name, args)` | Invoke a registered tool |
| `chidori.callAgent(path, input)` | Call a sub-agent |
| `chidori.util.parallel(fns)` | Run functions concurrently (in-VM helper — records nothing itself) |
| `chidori.branch(variants)` | Fork the run into per-strategy sub-runs from the current state; returns every outcome for comparison ([details](./branching-execution.md)) |
| `chidori.actors.spawn(source, input, options)` | Start an agent module as a supervised, addressable, concurrent actor process with a durable mailbox and restart policy; returns a handle with `send`/`join`/`stop`/`status` ([details](./actors.md)) |
| `chidori.actors.send(to, name, payload)` | Deliver a named message to an actor (pid or registered name) or to `"parent"` (the sender's spawner); never blocks |
| `chidori.receive(names, options)` | Blocking in-place message consumption from the caller's mailbox (fan-in via an array; `timeoutMs` resolves to the timeout sentinel) |
| `chidori.actors.join(target, options)` / `chidori.actors.stop(target, options)` | Settle an actor (owner-only): wait for its supervision loop, fold its records into this run's log, return `{ status, output?, error?, restarts }` |
| `chidori.actors.status(target)` / `chidori.actors.lookup(name)` | Lifecycle snapshot / name-registry lookup (a handle, or `null`) |
| `chidori.input(msg, options)` | Human-in-the-loop — pauses execution |
| `chidori.signal(name, options)` | Multiplayer — pause at a named listen point until an outside party (human or agent) delivers `{ name, payload, from }`; drains a durable mailbox if one is queued; `timeoutMs` resolves to a `{ timedOut: true }` sentinel after the deadline |
| `chidori.pollSignal(name)` | Non-blocking signal check — consume a queued signal of this name or resolve to `null` |
| `chidori.signal(names[], options)` | Fan-in — pass an array to pause until ANY of the named signals is delivered; the result's `name` says which fired |
| `chidori.memory.set/get/delete/list/clear` | Persistent key-value storage, namespaced on disk under the agent's `.chidori/memory/` (anchored to the workspace root, like runs; `CHIDORI_MEMORY_DIR` overrides) ([details](./memory.md)) |
| `chidori.workspace.{list,read,write,delete,manifest}` | Shared workspace files under the run's workspace root — policy-gated, recorded like every other effect |
| `chidori.log(msg, data)` | Structured logging |
| `chidori.mark(label, data)` | Record a labelled trace marker in the call log (the durable *value* checkpoint is `chidori.step`) |
| `chidori.step(name, fn)` | Durable value checkpoint — run pure compute once, journal the result, never re-pay it on replay/resume |
| `chidori.util.retry(fn, options)` | Retry with backoff (in-VM helper) |
| `chidori.util.tryCall(fn)` | Capture errors without raising (in-VM helper) |

**A tool is just a function with a documented signature.** There is no tool
type to implement and no registry to populate — a tool is an ordinary
function plus the `name`, `description`, and JSON-schema `parameters` that
tell the model when and how to call it. `defineTool` staples that signature
onto the function and hands you back a plain object you define inline or
import from any module — no special directory, no registration step:

```ts
import { chidori, run, defineTool } from "chidori:agent";

const search = defineTool({
  name: "search_commits",
  description: "Keyword search over the release window.",
  parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
  run: async ({ query }) => commits.filter((c) => c.subject.includes(query)), // closures work
});

const section = await chidori.prompt("Investigate the theme.", {
  tools: [search],
  maxTurns: 8,
});
```

The handle's `run` executes in the agent's own VM: closures over agent state,
ordinary imports, and every captured effect (fetch, workspace, `node:fs`)
work exactly as they do in the agent body — which is also what makes each
invocation deterministic on replay. Each model turn is a durable `respond()`
call and each invocation is journaled as a `mark("tool:<name>")` record, so
the loop appears in `chidori trace` like any other work.

**The tool loop is built in.** `chidori.prompt(text, { tools: [search],
maxTurns: 8 })` runs a complete provider tool-use loop — the model calls tools,
the runtime executes them and feeds results back, up to `maxTurns` — and
returns the final text; every inner call is journaled like any other effect. A
`tools` array may also carry string NAMES for tools sourced from outside the
agent (MCP servers, Rust-native tools), freely mixed with `defineTool`
handles. Hand-roll the loop with
`context().respond()` / `toolResult()` only when you need per-step control
(inspecting each call, streaming progress between steps, custom budgets — see
[`examples/agents/worker.ts`](../examples/agents/worker.ts)).

**Approval gates can show their artifact.** `chidori.input(prompt, { details })`
carries the thing under review (a draft, a diff); the CLI prints it above the
prompt and a paused session exposes it as `pending_details`, so a human never
approves blind.

There is no `chidori.http`. Networking is done with the **standard web/Node
APIs** — `fetch` (plus `Headers`/`Request`/`Response`) and the
`node:http`/`node:https` client modules — which the runtime replaces with
captured versions backed by a single policy-gated host op. Because the capture
lives at the base networking layer, every request inherits the same security
policy (allow / ask / deny), approval-pause, and deterministic record/replay —
including requests made deep inside a dependency:

```ts
const res = await fetch("https://example.com/search", {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ q: "chidori" }),
});
const data = await res.json();
```

See also [`docs/signals.md`](./signals.md) for the multiplayer signal model,
[`docs/actors.md`](./actors.md) for the actor process model,
[`docs/value-checkpoints.md`](./value-checkpoints.md) for `chidori.step`, and
[`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md)
for the captured networking/filesystem/crypto/timer model.

## Streaming prompt progress

Agents can label prompt output streams with `type` so UIs can filter incremental
progress separately from final answers:

```ts
const status = await chidori.prompt("Say what work is starting", { type: "progress" });
const answer = await chidori.prompt("Write the final answer", { type: "final" });
```

When using `--stream` or `POST /sessions/stream`, prompt calls emit
`prompt_start`, `prompt_delta`, and `prompt_end` events with `stream_id`,
`seq`, and `prompt_type`. This also works for prompts inside
`chidori.util.parallel(...)` fan-outs and `chidori.callAgent(...)` sub-agents. See
[`examples/agents/streaming_progress.ts`](../examples/agents/streaming_progress.ts).

## Prompt caching

Every prompt automatically marks its stable head (system prompt, tool schemas,
conversation prefix) for the provider's prompt cache, so a tool loop or
multi-turn conversation re-bills its prefix at the cached rate (~10% of base
input on Anthropic) instead of full price each turn. Disable per call with
`cache: false`. For long-lived contexts, build them once with
`chidori.context()` — an immutable, prefix-sharing conversation builder — and
the cache hits become structural:

```ts
const base = chidori.context().system(INSTRUCTIONS).doc("corpus", corpus).cacheBreakpoint("1h");
let ctx = base.user(firstQuestion);
const { text, context } = await ctx.prompt();
```

Cache effectiveness is measurable: prompt records and OTEL spans carry
`cache_creation`/`cache_read` token counts, and `total_cost_usd` prices them
at the provider's cached rates. Caching never changes results — replay returns
recorded results and pays zero tokens either way. See
[`docs/context-management.md`](./context-management.md).

When a conversation outgrows the window, `await ctx.compact({ budgetTokens })`
is the explicit (never automatic) escape valve: it folds the older turns into
one durable summary segment via a recorded prompt call — so it replays
deterministically — and keeps the stable head and newest turns verbatim under
a fresh cache breakpoint. And setting `CHIDORI_PROMPT_CACHE_DIR=<dir>` opts
into a local content-addressed response cache: an exact repeat of a prompt,
even across runs, is served locally without calling the provider and still
recorded as a normal call-log entry.

## Conversational agents

A chat assistant is the most common agent shape, so `chidori.conversation()`
wraps `context()` for it directly. It owns the running dialogue — the system
prompt is frozen once as the cacheable prefix, and each `say(message)` appends
the user turn, makes one durable `prompt` host call, and threads the assistant
turn back in — so you don't re-plumb `ctx = (await ctx.user(m).prompt()).context`
by hand:

```ts
const chat = chidori.conversation({
  system: "You are a concise, friendly assistant.",
  compact: { budgetTokens: 8000 }, // opt-in window management, per turn
});

const a = await chat.say("Hi, who are you?");
const b = await chat.say("What can you help with?");
```

Every turn is still one recorded host call, so the whole conversation replays
for $0 and each turn after the first reads the shared prefix at the cached rate.
For an interactive dialogue, `chat.loop()` reads each human message via
`chidori.input()` — terminal stdin under `chidori run`, a paused session resume
under `chidori serve` — and replies until the user exits:

```ts
const transcript = await chat.loop({ prompt: "you>" }); // type "exit" to end
```

Drop to `chat.context` whenever you need the lower-level API (manual `compact`,
`digest`, forking), and use `chat.respond(message)` for author-driven tool
loops. See [`examples/agents/conversation.ts`](../examples/agents/conversation.ts).

To chat with the model directly — no agent file — run `chidori chat` (`--system`,
`--model`). It is a thin REPL over `conversation()`: each turn
is durable and streams its reply token-by-token, and the prior turns replay for
free, so only your newest message reaches the provider.
