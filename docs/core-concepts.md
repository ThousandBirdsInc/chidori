# Core concepts: agents & host functions

An agent is a `.ts` file that exports an async `agent(input, chidori)` function.
The runtime provides a fixed set of **host functions** for side effects through
the `chidori` object — agents never touch the outside world directly, so the
runtime sees and records everything. See [`llm.txt`](../llm.txt) for the full
API reference.

## Host functions

| Function | Purpose |
|---|---|
| `chidori.prompt(text, { type, ... })` | Send to an LLM, return string or parsed JSON; streamed prompt events carry the optional type |
| `chidori.context()` | Immutable multi-turn prompt builder with prefix sharing and provider prompt caching |
| `chidori.template(strOrPath, vars)` | Render a Jinja2 template with minijinja |
| `chidori.tool(name, args)` | Invoke a registered tool |
| `chidori.callAgent(path, input)` | Call a sub-agent |
| `chidori.parallel(fns)` | Run functions concurrently |
| `chidori.branch(variants)` | Fork the run into per-strategy sub-runs from the current state; returns every outcome for comparison ([design](./branching-execution.md)) |
| `chidori.input(msg, options)` | Human-in-the-loop — pauses execution |
| `chidori.signal(name, options)` | Multiplayer — pause at a named listen point until an outside party (human or agent) delivers `{ name, payload, from }`; drains a durable mailbox if one is queued; `timeoutMs` resolves to a `{ timedOut: true }` sentinel after the deadline |
| `chidori.pollSignal(name)` | Non-blocking signal check — consume a queued signal of this name or resolve to `null` |
| `chidori.signalAny(names, options)` | Fan-in — pause until ANY of the named signals is delivered; the result's `name` says which fired |
| `chidori.http(url, options)` | Make an HTTP request |
| `chidori.memory(action, ...)` | Persistent storage (key-value + vector) |
| `chidori.log(msg, data)` | Structured logging |
| `chidori.checkpoint(label, meta)` | Record an explicit call-log marker for trace/replay |
| `chidori.step(name, fn)` | Durable value checkpoint — run pure compute once, journal the result, never re-pay it on replay/resume |
| `chidori.retry(fn, options)` | Retry with backoff |
| `chidori.tryCall(fn)` | Capture errors without raising |

See also [`docs/signals.md`](./signals.md) for the multiplayer signal model and
[`docs/value-checkpoints.md`](./value-checkpoints.md) for `chidori.step`.

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
`chidori.parallel(...)` branches and `chidori.callAgent(...)` sub-agents. See
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
