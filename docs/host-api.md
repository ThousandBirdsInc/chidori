---
title: "Host API Reference"
description: "Every chidori.* host function — signatures, options, and semantics — plus agent shape rules, tool definition, providers, and runtime policy."
---

# Host API reference

Reference for the `chidori` host object and the agent module surface. Each
method here is a **durable host call** unless marked as an in-VM helper:
recorded in the call log live, returned from the log on replay. The
LLM-optimized rendition of this reference ships in the repo as
[`llm.txt`](../llm.txt) — point your codegen tools at that.

For orientation first, read [Core Concepts](./core-concepts.md).

## Agent shape

An agent is a `.ts` file that imports from the virtual `chidori:agent` module
and registers its handler with `run(...)`:

```ts
import { chidori, run } from "chidori:agent";

run(async (input: { document: string }) => {
  const summary = await chidori.prompt(
    "Summarize in three bullets:\n\n" + input.document,
    { type: "final" },
  );
  return { summary };
});
```

Rules:

- Import `{ chidori, run }` from `chidori:agent` and call `run(handler)` at
  the top level. (Legacy fallback: `export async function agent(input,
  chidori)` is still accepted when `run(...)` wasn't called.)
- Type the input with an inline object type or a `type` alias, never an
  `interface` — interfaces have no implicit index signature, so they fail
  the handler's `AgentJson` constraint with a confusing type error.
- Return JSON-compatible values only.
- Use `chidori.*` for LLMs, tools, input, signals, memory, templates,
  workspace files, and logging. HTTP goes through the standard `fetch`,
  which the runtime captures.
- Prefer deterministic code. Durable runs use fixed `Date` and seeded
  `Math.random` policies by default.
- Local TypeScript imports are governed by runtime policy. Dynamic imports
  are rejected.

## LLM calls

### `chidori.prompt(text, options?)`

```ts
const text = await chidori.prompt("Write a concise answer", {
  type: "final",
  model: "claude-sonnet",
  maxTokens: 500,
  temperature: 0.2,
});
```

Returns the reply text. With `tools` set, `prompt()` runs a **complete
provider tool-use loop** internally — call tools, feed back results, repeat
up to `maxTurns` — and returns the final text.

| Option | Meaning |
|---|---|
| `type` | Label for streamed prompt output — `"progress"`, `"draft"`, `"subagent"`, `"final"`, … |
| `model` | Provider model override. Unset prompts use the run's default model (`--model` / `CHIDORI_MODEL`, falling back to `claude-sonnet-4-6`); the resolved default is recorded in the run manifest so `resume`/`branch-rerun` re-run under the same model automatically. |
| `system` | System prompt for this call. |
| `maxTokens` | Output token cap. Reasoning models spend the same budget on hidden reasoning first — budget generously. A truncated reply prints a warning to stderr. |
| `maxTurns` | Cap on provider tool-use turns for the built-in tool loop. |
| `temperature` | Sampling temperature. |
| `tools` | Tools available to the loop: registered tool **names** (MCP/native registry) and/or `defineTool(...)` **handles**, freely mixed. Handle bodies run in the agent's own VM; each invocation is journaled as a `mark("tool:<name>")` record. |
| `format` | `"json"` parses the reply as JSON (a single wrapping markdown fence is tolerated). Unparseable output **throws** by default so truncation can't masquerade as a structured result. |
| `strict` | Applies to `format: "json"`. `true` (default) throws on unparseable output; `false` falls back to the raw string. |
| `cache` | Prompt-cache posture. Defaults to on (`"5m"`): the stable request head (system, tools, conversation prefix) is marked so providers bill repeated prefixes at the cached rate. `false` disables for this call; `"1h"` requests the extended TTL. Caching never changes a response. |

Use `context().respond()` instead when you need the structured `stopReason`,
token counts, or `reasoning` yourself, or per-step control of a tool loop.

When streaming is enabled, prompt events carry `prompt_type`, `stream_id`,
and `seq` so UIs can filter progress streams from final-answer streams — see
[Streaming](#streaming).

### `chidori.context()`

```ts
const base = chidori
  .context()
  .system("You are a policy analyst.")
  .doc("policy-corpus", corpusText) // large stable reference block
  .cacheBreakpoint("5m");           // freeze the head as a cacheable prefix

let ctx = base;
for (const q of questions) {
  ctx = ctx.user(q);
  const { text, context } = await ctx.prompt({ type: "final" });
  ctx = context; // assistant turn appended; the prefix stays shared
}
```

An immutable, turn-structured prompt context. Builder methods (`system`,
`tools`, `doc`, `user`, `assistant`, `toolResult`, `cacheBreakpoint`) each
return a **new** context sharing the parent's segments, so `base.user("a")`
and `base.user("b")` are independent forks of the same prefix. Building is
pure in-VM work; only `prompt()` / `respond()` perform a durable host call.
The stable head is auto-marked for provider prompt caching.

| Method | Returns | Purpose |
|---|---|---|
| `prompt(options?)` | `{ text, context }` | Send; get the answer plus the context extended with the assistant turn (including any tool-use exchange). |
| `respond(options?)` | `{ response, context }` | One structured turn for author-driven tool loops (`response.toolCalls`, `response.blocks`; reasoning models also expose `response.reasoning`). |
| `digest()` | string | Stable content hash of the assembled request; also recorded in each prompt's call-log args as `request_digest`. |
| `estimateTokens()` | number | Rough local size estimate for window budgeting. |
| `compact(options?)` | `Promise<Context>` | Explicit, opt-in window compaction — see below. |

`compact()` summarizes the older conversation turns into **one durable
summary segment** (a recorded `prompt` host call, so it replays
deterministically) and returns a new context: stable head + summary + fresh
cache breakpoint + the newest `keepTurns` turns (default 2) verbatim.
`budgetTokens` makes it a pure no-op while `estimateTokens()` is within
budget, so loops can call it unconditionally; `model` / `instructions` /
`maxTokens` / `ttl` tune the summarizer. Compaction is never automatic — it
changes what the model sees, so it is always an author decision.

See [`examples/agents/context_qa.ts`](../examples/agents/context_qa.ts) and
[Context Management](./context-management.md).

### `chidori.conversation(options?)`

```ts
const chat = chidori.conversation({
  system: "You are a concise, friendly assistant.",
  tools: [search],                 // defineTool handles, on every turn
  compact: { budgetTokens: 8000 }, // opt-in per-turn window management
});

const reply = await chat.say("Hi, who are you?"); // one durable prompt call
await chat.say("What can you help with?");        // prefix read at cached rate

chat.length;    // number of completed exchanges
chat.history(); // [{ role, text }, ...]
chat.context;   // the underlying immutable Context
```

A stateful chat-assistant wrapper over `context()` — the most common agent
shape. The system/tools head is frozen once as a cacheable prefix; each
`say(message)` appends the user turn, makes one durable `prompt` host call,
and threads the assistant turn back in. The whole conversation is recorded,
replays for $0, and reads the shared prefix at the cached rate each turn.

| Method | Purpose |
|---|---|
| `say(message, options?)` | Send a user message, return the assistant reply text; the dialogue advances in place. `options` are per-turn `PromptOptions`. |
| `respond(message, options?)` | Like `say()` but returns the structured response (`toolCalls`, `blocks`) for author-driven tool loops; append results with `chat.context.toolResult(...)`, then `say()`. |
| `loop(options?)` | Drive an interactive dialogue: read each human message via `chidori.input()` (terminal stdin under `chidori run`, a paused-session resume under `chidori serve`), reply with `say()`, repeat until an exit word (`"exit"`/`"quit"`) or `until` returns true. Options: `prompt`, `inputOptions`, `exit`, `maxTurns`, `skipEmpty`, `turn`, `onReply`, `until`. |

`conversation(options)` accepts `system`, `tools`, default `type` / `model` /
`maxTokens` / `temperature` / `cache`, `cacheTtl`, and `compact` (a
`CompactOptions` applied before each turn — a no-op until the tail exceeds
budget). See [`examples/agents/conversation.ts`](../examples/agents/conversation.ts).

Setting `CHIDORI_PROMPT_CACHE_DIR=<dir>` opts into a **local**
content-addressed prompt cache keyed on the assembled `request_digest`: an
exact repeat of a prompt — even from a different run — is served locally
without calling the provider, then recorded as a normal call-log entry with
the identical result and no token usage. Live-path only: replay always
short-circuits to the call log first.

## Humans and other agents

### `chidori.input(prompt, options?)`

```ts
const answer = await chidori.input("Approve this request?", {
  type: "approval",
  choices: ["yes", "no"],
  default: "no",
  details: draft, // the artifact under review — shown to the human
});
```

Pause for a human. `details` carries the thing being approved (a draft, a
diff, a report): the CLI prints it above the prompt, and a paused session
exposes it as `pending_details` alongside `pending_prompt` — approval gates
are never blind. It is display-only and never part of the durable record.

- Under `chidori serve`, `input()` **pauses the session**; resume with
  `POST /sessions/{id}/resume` or `AgentClient.resume(id, response)`.
- Under `chidori run`, `input()` reads one line from stdin. An empty answer —
  blank enter, or EOF in a non-interactive run — resolves to the declared
  `default`; EOF with no `default` fails the run rather than silently
  returning an empty string.

### `chidori.signal(name | names[], options?)` / `chidori.pollSignal(name)`

```ts
// Pause at a named listen point until an outside party (human or agent)
// delivers { name, payload, from } via POST /sessions/{id}/signal. A durable
// per-run mailbox absorbs signals that arrive before the agent listens.
const review = await chidori.signal("review");

// With timeoutMs, resolves to { timedOut: true } after the deadline.
const r = await chidori.signal("review", { timeoutMs: 60000 });
if (r.timedOut) { /* nobody answered */ }

// Non-blocking: consume a queued signal or get null (recorded, replayable).
const steer = await chidori.pollSignal("steer");

// Fan-in: pause until ANY listed name fires; result.name says which.
const fired = await chidori.signal(["review", "steer"]);
```

Every consumed signal is recorded in the call log, so multiplayer sessions
replay deterministically. Signals delivered to a run streaming over
`POST /sessions/stream` are pushed into the live agent's mailbox in-memory
and resume a matching pause in-process. See [Signals](./signals.md).

### `chidori.alarm(ms)`

```ts
const fired = await chidori.alarm(24 * 60 * 60 * 1000); // → { timedOut: true }
```

A durable timer on the signal machinery: the run (or detached agent)
hibernates and is woken at the deadline, **surviving process restarts** — the
deadline is persisted and re-armed at boot. In a detached agent this is the
idiomatic "do maintenance every N hours even with no traffic" primitive.

## Tools and sub-agents

### `defineTool(...)` and the `tools` option

A tool is a plain object made with `defineTool`: JSON-compatible metadata
(`name`, `description`, JSON-schema `parameters`) wrapped around an async
`run(args, chidori)` function. Define it inline or import it from any module
— there is no `tools/` directory and no registration step — and pass the
handle in the `tools` prompt option.

```ts
import { chidori, run, defineTool } from "chidori:agent";

// `fetch` inside a tool body is the captured fetch: policy-gated,
// journaled, and replayed for $0.
const wikiSearch = defineTool({
  name: "wiki_search",
  description: "Search Wikipedia and return the top matching titles and URLs.",
  parameters: {
    type: "object",
    properties: { query: { type: "string", description: "Search query" } },
    required: ["query"],
  },
  run: async (args: { query: string }) => {
    const url =
      "https://en.wikipedia.org/w/api.php?action=opensearch&format=json" +
      "&limit=5&search=" + encodeURIComponent(args.query);
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`wiki_search failed: HTTP ${resp.status}`);
    const [, titles, , urls] = (await resp.json()) as [string, string[], string[], string[]];
    return titles.map((title, i) => ({ title, url: urls[i] }));
  },
});

run(async (input: { question: string }) => {
  const answer = await chidori.prompt(input.question, {
    tools: [wikiSearch],
    maxTurns: 4,
  });
  return { answer };
});
```

The `run` body executes in the agent's own VM: closures over agent state
work, and its side effects are the same captured effects the agent already
has. Each invocation is journaled as a `mark("tool:<name>")` record.

### `chidori.tool(name, args)`

For tools sourced from **outside** the agent — MCP-server tools (configured
via `CHIDORI_MCP_*`) and Rust-native tools registered by an embedding
application — dispatched by name:

```ts
const result = await chidori.tool("docs_search", { query: "snapshot runtime" });
```

A tool's `fetch` is SSRF-guarded by default: requests to hosts that resolve
to non-public addresses (localhost, RFC-1918 ranges) are refused even under
`--trusted`. Tools that talk to local services need
`CHIDORI_HTTP_ALLOW_HOSTS=127.0.0.1` (comma-separated hosts, IPs, or CIDRs;
`*` disables the guard). Provider endpoints
(`CHIDORI_OPENAI_COMPAT_URL=http://localhost:11434`) are **not** affected —
the guard covers only agent/tool-initiated http effects.

### `chidori.callAgent(path, input)`

```ts
const child = await chidori.callAgent("child.ts", { topic: "snapshots" });
```

Sub-agents share the parent runtime context and call log. Runtime dispatch
accepts TypeScript `.ts` sub-agents only.

## Concurrency and multi-agent

### `chidori.util.parallel(fns, options?)` — in-VM helper

```ts
const [a, b] = await chidori.util.parallel([
  () => chidori.prompt("Draft option A", { type: "draft" }),
  () => chidori.prompt("Draft option B", { type: "draft" }),
]);
```

`Promise.all` semantics; `options.concurrency` caps in-flight tasks.
Everything under `chidori.util` is pure JavaScript control flow and records
nothing itself — only the durable calls made inside the tasks appear in the
journal.

### `chidori.branch(variants, options?)`

```ts
const outcomes = await chidori.branch([
  { label: "outline-first", source: "strategies/outline_first.ts", input: { research } },
  { label: "draft-direct", source: "strategies/draft_direct.ts", input: { research } },
]);
const best = outcomes.filter((o) => o.status === "completed").reduce(pick);
```

Fork the run into one sub-run per variant from the current anchored state
(the parent's VFS plus each variant's explicit `input`). Each branch runs
its own source module on a fresh context whose records occupy a reserved,
disjoint sequence range nested under the `branch` call, and returns
`{ label, branchId, status, output?, pendingPrompt?, error? }`. The whole
fan-out is **one recorded durable call**: replay returns the outcomes from
the call log without re-running the branches. Variants run in waves of
`options.concurrency` worker threads (default 1 — sequential); outcome order
always follows variant order. Nested `chidori.branch` inside a branch is
rejected.

Persisted branches are independently operable after the parent moves on:

```bash
chidori branches <run-id>                                 # list branch stores
chidori branch-resume <run-id> <branch-id> --value "blue" # answer a paused input()
chidori branch-rerun <run-id> <branch-id>                 # re-run edited source.ts
```

See [Branching Execution](./branching-execution.md).

### `chidori.actors.*` — supervised concurrent processes

```ts
const worker = await chidori.actors.spawn("workers/researcher.ts", { topic }, {
  name: "researcher",  // optional registry name for lookup / send
  restart: "resume",   // "never" (default) | "clean" | "resume"
  maxRestarts: 3,
  backoffMs: 500,      // doubles per attempt
});

await worker.send("focus", { region: "EU" });               // never blocks
await chidori.actors.send("researcher", "focus", { region: "EU" });

const msg = await chidori.receive("draft"); // { name, payload, from }
const any = await chidori.receive(["draft", "cancel"], { timeoutMs: 60000 });

const outcome = await worker.join(); // fold records into this run's log
await worker.stop();                 // cooperative stop, then join
await worker.status();               // { pid, status, restarts, mailbox, waitingFor? }
await chidori.actors.lookup("researcher"); // a handle, or null
```

Actors run their own source module on an isolated VM, concurrently on their
own thread, with a durable mailbox. Restart strategies: `clean` re-runs from
scratch; `resume` replays the actor's accumulated log minus the trailing
failed records, so completed work returns from cache and only the failing
call retries. Actor death is observable via a `"__chidori.down__"` message;
actors form supervision trees (depth ≤ 4, ≤ 128 actors per run); join/stop
are owner-only. Full semantics: [Actors](./actors.md).

### `chidori.agents.*` — detached durable agents

```ts
const svc = await chidori.agents.spawn("services/inbox-triager.ts", {}, {
  name: "inbox-triager", // registry name that outlives this run
  restart: "resume",     // "never" | "clean" | "resume" (default)
  model: "deepseek-chat",
});

await svc.send("email", { from: "a@x.com" }); // durable delivery; wakes a hibernating agent
await svc.status();
await svc.join({ timeoutMs: 30000 });
await svc.stop();
await chidori.agents.lookup("inbox-triager");
```

A detached agent is its own durable run and **outlives the spawner**: its own
run id and journal, a registered name, a durable mailbox, and a
hibernate/wake lifecycle — `chidori.signal(name)` inside the agent holds no
thread and no VM while waiting. The fleet survives process restarts:
`chidori serve` re-arms every registered agent at boot. Requires
persistence. See [Detached Agents](./detached-agents.md).

## State and effects

### `fetch` / `node:http` — there is no `chidori.http`

```ts
const response = await fetch("https://example.com/webhook", {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ ok: true }),
});
const data = await response.json();
```

The runtime replaces the standard networking APIs with captured versions
backed by one policy-gated host op. Because the capture lives at the base
networking layer, every request — even one made inside a dependency — is
policy-checked, logged, and replayed from the call log when available.

### `chidori.template(strOrPath, vars)`

```ts
const prompt = await chidori.template("prompts/summary.jinja", {
  document: input.document,
});
```

Jinja rendering for reusable prompt text; inline templates are also
supported; undefined variables fail loudly. See [Prompt Templates](./template.md).

### `chidori.memory.*`

```ts
await chidori.memory.set("draft", { text: "..." });
const draft = await chidori.memory.get("draft");
const keys = await chidori.memory.list();
await chidori.memory.delete("draft");
await chidori.memory.clear();
```

Persistent, namespaced key-value storage across runs, anchored to the
agent's workspace root at `.chidori/memory/<namespace>.json`
(`CHIDORI_MEMORY_DIR` overrides). Logged and replay-aware. See
[Memory](./memory.md).

### `chidori.workspace.*`

```ts
const entries = await chidori.workspace.list({ completeOnly: true });
const text = await chidori.workspace.read("notes/draft.md");
const entry = await chidori.workspace.write("notes/draft.md", "...", { language: "markdown" });
await chidori.workspace.delete("notes/draft.md", "superseded");
const manifest = await chidori.workspace.manifest();
```

Durable file store rooted at the project directory (the agent file's dir)
under `run`, `resume`, `serve`, and detached agents alike;
`CHIDORI_WORKSPACE_ROOT` overrides. Entries carry `{ path, status, sha256,
bytes }`. Every action is policy-gated (`workspace:write` /
`workspace:delete` are refused under `untrusted`) and recorded. `remove` is
an alias for `delete`.

### `chidori.step(name, fn)`

```ts
const plan = await chidori.step("plan", () => buildPlanDeterministically(input));
```

A **durable value checkpoint**: `fn` runs once and its JSON-serializable
result is journaled; replay and resume return the recorded value (or
re-throw the recorded error) without re-running `fn`. Wrap expensive
deterministic computation in a step so resuming a long run does not re-pay
it. The callback must be pure, synchronous compute: host effects, captured
randomness, filesystem writes, timers, and async callbacks throw inside a
step. See [Value Checkpoints](./value-checkpoints.md).

### `chidori.log(msg, data?)` / `chidori.mark(label, data?)`

```ts
await chidori.log("Fetched candidates", { count: 3 });
await chidori.mark("after-draft", { tokens: 120 });
```

`log` records structured progress for debugging. `mark` records a labelled
call-log marker — an annotation for the trace, nothing more (the durable
*value* checkpoint is `chidori.step`).

### `chidori.util.retry(fn, options?)` / `chidori.util.tryCall(fn)` — in-VM helpers

```ts
const value = await chidori.util.retry(
  () => fetch("https://example.com").then((r) => r.json()),
  { attempts: 3 },
);

const result = await chidori.util.tryCall(() => chidori.tool("maybe_fails", {}));
if (!result.ok) {
  await chidori.log("Tool failed", { error: result.error });
}
```

`RetryOptions` also accepts `delayMs` and `backoff`, but the helper retries
immediately — no delay is applied between attempts.

### `chidori.appData.*`

```ts
await chidori.appData.write("insert into notes (body) values ($1)", ["hi"]);
const rows = await chidori.appData.query("select * from notes", []);
```

Host-brokered writes/queries against a run-bound app-data cluster
(generative UI). Params are bound server-side, never string-concatenated;
the guest never holds a DB credential. Journaled like `http`. Requires a
host-side `CHIDORI_APP_DATA` binding; without one, calls return
`{ appDataError: { kind: "no_cluster", ... } }`.

### `chidori.renderDOM()`

```ts
document.body.appendChild(document.createElement("div"));
const batch = chidori.renderDOM();
```

Agents get a virtual `document` / `window`. `renderDOM()` flushes the
pending DOM mutation batch as a journaled `dom_render` effect. See
[DOM Runtime Prototype](./dom-runtime-prototype.md).

## Streaming

```bash
chidori run examples/agents/streaming_progress.ts --stream
```

`--stream` changes only how progress is reported (NDJSON events on stdout);
the final `done` event carries `run_id` and `status`. Over HTTP, use
`POST /sessions/stream` (SSE):

```ts
for await (const event of client.stream({ topic: "snapshots" })) {
  if (event.type === "prompt_delta" && event.prompt_type === "progress") {
    process.stdout.write(event.delta);
  }
}
```

| Event | Meaning |
|---|---|
| `call` | A host call record. |
| `prompt_start` / `prompt_delta` / `prompt_end` | Prompt stream lifecycle; deltas carry incremental token text. |
| `paused` | The run paused at a `signal()` listen point and stays live; a delivered signal (or the timeout) resumes it on the same stream. |
| `done` | Run completed, failed, or paused. |

Prompt labels work inside sub-agents and parallel branches because prompt
events are emitted through the shared runtime context.

## Providers & model selection

Providers register from environment variables (all can coexist; requests
route by model name, first match wins):

| Variable | Provider |
|---|---|
| `ANTHROPIC_API_KEY` | Anthropic (`claude-*` models). |
| `OPENAI_API_KEY` | OpenAI; `OPENAI_BASE_URL` redirects it at any OpenAI-compatible endpoint and widens it to match all model names. |
| `CHIDORI_OPENAI_COMPAT_URL` + `CHIDORI_OPENAI_COMPAT_KEY` | Any OpenAI-compatible endpoint (DeepSeek, Groq, Ollama, vLLM, LiteLLM…), matching all model names. `/v1` and bare hosts both work. |
| `chidori model-login` | Zero-setup OpenRouter fallback. |

The default model for prompts that don't set `model` in code is
`CHIDORI_MODEL` (or `--model` on `run`/`resume`), falling back to
`claude-sonnet-4-6`. The resolved default is recorded in each run's
manifest, so `resume`, `branch-resume`/`branch-rerun`, and server
resume/replay routes re-run under the run's own model with no flags.
Detached agents likewise carry their model in their registry descriptor.

Cost estimation covers Anthropic/OpenAI models out of the box; teach it
other models with `CHIDORI_PRICING` (JSON, model prefix → USD per MTok):

```bash
CHIDORI_PRICING='{"deepseek-v4-flash":{"input_per_mtok":0.28,"output_per_mtok":0.42,"cache_read_multiplier":0.1}}'
```

For local smoke tests without provider credentials, set
`CHIDORI_TEST_LLM_RESPONSE` to a static response string — this registers a
catch-all test provider and avoids external network calls.

## Runtime policy

Durable TypeScript runs record policy in the snapshot manifest:

| Policy | Values |
|---|---|
| `typescript_imports` | `none`, `relative`, or `project` |
| `date` | `disabled`, `fixed`, or `host` |
| `random` | `disabled`, `seeded`, or `host` |
| `maps_sets` | `reject` or `serialize` |

Environment overrides:

```bash
CHIDORI_TS_IMPORTS=relative
CHIDORI_TS_DATE=fixed
CHIDORI_TS_RANDOM=seeded
CHIDORI_SNAPSHOT_MAPS_SETS=reject
```

Durable snapshot runs reject host clock and host randomness. Resume rejects
incompatible source hashes, policy, or ABI before trusting snapshot metadata
— see [Replay & Resume](./replay.md) and the
[Sandbox Model](./sandbox-model.md).
