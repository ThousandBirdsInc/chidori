---
title: "Context Management"
description: "Immutable prompt contexts, provider prompt caching, explicit window compaction, and cost accounting."
---

# Context Management — Cache-Aware, Composable Prompts

> Provider prompt caching, the `chidori.context` builder, the
> `chidori.conversation` wrapper, window compaction, and the local
> content-addressed prompt cache — on by default where noted below.
> **Related:** [Memory](./memory.md), [Value Checkpoints](./value-checkpoints.md),
> [Signals](./signals.md), [Branching Execution](./branching-execution.md).
> API reference: [Host API](./host-api.md) (`llm.txt` is the LLM-consumable
> copy). Example: `examples/agents/context_qa.ts`.

## What this is

Chidori gives agents a first-class way to **manage the context they push into an
LLM prompt** — one that is:

- **cache-aware**, so repeated prefixes hit the provider's prompt cache (and our
  own replay/dedup caches) instead of being re-sent and re-billed, and
- **composable as conversational turns** instead of hand-rolled by string
  concatenation.

There are three layers, each usable on its own:

1. **Automatic provider prompt caching.** Every `chidori.prompt` and every turn
   of the native tool-use loop marks its stable head (system + tools +
   conversation prefix) with provider cache breakpoints by default. No author
   change is required to get the discount. See
   [Provider prompt caching](#provider-prompt-caching).
2. **`chidori.context`** — an immutable, content-addressed, turn-structured value
   you build up (`.system()`, `.tools()`, `.doc()`, `.user()`, `.assistant()`)
   and then `.prompt()` against. Appending a turn returns a *new* handle that
   structurally shares the prefix, so the stable head is laid out once and reused
   across every turn, tool-use round, and branch fork.
3. **`chidori.conversation`** — a small stateful wrapper over `Context` for the
   common chat shape, plus opt-in **window compaction** (`.compact()`) and an
   opt-in **local content-addressed prompt cache**.

The load-bearing correctness property: **caching is always a live-only cost
optimization.** The journaled result — the journal is the run's call log — is
the source of truth; replay returns it without sending a request or consulting
any cache, so an agent stays fully deterministic and replayable no matter how
aggressively it caches. See [Caching and replay](#caching-and-replay).

---

## Core concept: immutable, prefix-sharing context

A `Context` is an **immutable singly-linked chain of segments**. Each builder
call (`.system()`, `.user()`, `.doc()`, …) allocates one new frozen segment node
pointing at its parent and returns a new handle — the parent is never mutated:

```ts
const base = chidori.context().system("You are concise.");
const a = base.user("question A");   // independent
const b = base.user("question B");   // independent — shares `base` by reference
```

`a` and `b` share every node up to `base`. This persistent-data-structure
property is what makes:

- **forks cheap** — building N continuations from one base costs N segment
  allocations, not N prefix copies (see
  [Composition with branching and signals](#composition-with-branching-and-signals));
- **cache prefixes stable** — the same stable head produces the same provider
  cache breakpoint every turn, so it warms once and reads thereafter;
- **content-addressed dedup possible** — `digest()` hashes the assembled
  request (a versioned sha256), and the
  [local prompt cache](#the-local-prompt-cache) keys on the same digest.

Builder methods are pure in-VM structure building — no host call. Only
`.prompt()` / `.respond()` (and `.compact()`, which issues a summarization
prompt) make host calls, so the only journaled effect is the actual LLM call.

---

## API surface

The types below are the authoring-time declarations; the runtime injects the
concrete `chidori` host object. Full reference: [Host API](./host-api.md).

### `chidori.context` — the builder

```ts
type CacheTtl = "5m" | "1h";

interface Context {
  // --- builders (immutable: each returns a NEW Context sharing the prefix) ---
  system(text: string): Context;
  tools(names: string[]): Context;             // by name, resolved like prompt({ tools })
  doc(label: string, text: string): Context;   // a large stable reference block
  user(text: string): Context;
  assistant(text: string): Context;
  toolResult(id: string, content: string, isError?: boolean): Context;

  // Freeze everything appended so far as a cacheable prefix (one provider
  // cache breakpoint). Coalesced to the provider's cap — latest wins. Most
  // authors never need this; stable heads are auto-marked by default.
  cacheBreakpoint(ttl?: CacheTtl): Context;

  // --- execution (the only durable host calls) ---
  // Send this context; returns the text plus a NEW context with the assistant
  // turn appended (including any internal tool-use exchange).
  prompt(options?: PromptOptions): Promise<{ text: string; context: Context }>;
  // Single structured turn for author-driven tool loops.
  respond(options?: PromptOptions): Promise<{ response: LlmResponseJson; context: Context }>;

  // --- window management ---
  compact(options?: CompactOptions): Promise<Context>;

  // --- introspection (pure, never journaled) ---
  digest(options?: PromptOptions): string;   // stable content hash of the assembled request
  estimateTokens(): number;                  // rough local estimate for window budgeting
}

// Entry point. Optionally seed from an existing single-shot prompt's options.
chidori.context(seed?: { system?: string; tools?: string[] }): Context;
```

`chidori.prompt(text, opts)` is unchanged — it is effectively sugar for
`chidori.context(opts).user(text).prompt(opts)` discarding the returned context.

### `PromptOptions.cache`

`PromptOptions` gains an optional cache posture:

```ts
cache?: boolean | CacheTtl | { ttl?: CacheTtl };
```

Defaults to on (`"5m"`): the runtime marks the stable request head so providers
bill repeated prefixes at the cached rate. `false` disables marking for that call
(use it for a true one-shot where caching would only ever pay the write cost).
`"1h"` (or `{ ttl: "1h" }`) requests the extended TTL. **Caching never changes a
response — only how it is billed.**

### `chidori.conversation` — the stateful wrapper

For the most common shape, a multi-turn chat, `chidori.conversation()` owns the
running context (system + tools frozen as a cacheable prefix) so you write
`chat.say(message)` instead of threading `ctx = (await
ctx.user(m).prompt()).context` by hand. Every turn is still one durable
`prompt`/`respond` host call that replays for free.

```ts
interface Conversation {
  readonly context: Context;        // drop to the lower-level API any time
  readonly length: number;          // completed user+assistant exchanges
  history(): ConversationTurn[];    // transcript as { role, text }
  say(message: string, options?: PromptOptions): Promise<string>;
  respond(message: string, options?: PromptOptions): Promise<LlmResponseJson>;
  loop(options?: ConversationLoopOptions): Promise<ConversationTurn[]>;
}

chidori.conversation(options?: ConversationOptions): Conversation;
```

```ts
const chat = chidori.conversation({ system: "You are concise." });
const a = await chat.say("Hi, who are you?");
const b = await chat.say("What can you help with?");
// or drive it interactively against chidori.input():
const transcript = await chat.loop({ prompt: "you>" });
```

`ConversationOptions` carries per-turn defaults (`model`, `maxTokens`,
`temperature`, `cache`, `cacheTtl`) and an opt-in `compact?: CompactOptions` —
when set, each turn first runs the same budgeted `Context.compact()` (a no-op
until the tail exceeds budget). `Conversation.loop()` reads a human message via
`chidori.input()` each turn and ends on an exit word or an `until` predicate;
see `ConversationLoopOptions` in the [Host API](./host-api.md).

---

## Provider prompt caching

### Default auto-marking — the zero-author win

When assembling any LLM request — both the single-shot `chidori.prompt` path and
the native tool-use loop — the runtime auto-marks cacheable boundaries:

- the **`system`** block (stable for the whole run),
- the **`tools`** array (stable for the whole tool-use loop),
- the **newest message**, which freezes the whole conversation head — marked
  whenever a follow-up sharing the prefix is plausible: the request carries
  tools (so a tool-use loop is likely) or is already multi-turn.

Auto-marking is on by default and disabled per call by
`PromptOptions.cache: false`. So a 10-turn tool-use agent with a 20K prefix pays
for that prefix once instead of ten times, with no code change.

### What goes on the wire

- **Anthropic**: the runtime emits `cache_control: {"type":"ephemeral"}` on the
  marked system block (using the structured-system form the API requires for
  caching), the last tool entry, and each marked message's last content block.
  Marks are coalesced to Anthropic's 4-breakpoint cap (latest wins). The
  `anthropic-beta: extended-cache-ttl-2025-04-11` header is sent **only** when a
  `1h` TTL is requested. An unmarked request serializes byte-identically to the
  pre-caching wire format.
- **OpenAI**: caching is automatic on exact prefixes, so there is no marker to
  emit — the immutable-prefix design feeds it naturally. The runtime parses
  `prompt_tokens_details.cached_tokens` and reports `input_tokens` as the fresh
  share so the two providers agree on semantics. OpenAI has no cache-write
  billing, so `cache_creation_tokens` is always 0 there.

### Cache accounting

- Each prompt record's `token_usage` carries optional `cache_creation_tokens` /
  `cache_read_tokens` (absent on journals recorded before caching existed, which
  still load fine). Anthropic's `cache_creation_input_tokens` /
  `cache_read_input_tokens` are parsed on both the blocking and streaming paths.
- Cost accounting prices cache **writes at 1.25×** base input and **reads at
  0.1×** base for Anthropic (0.5× reads for OpenAI), so `total_cost_usd`
  reflects the real bill.
- Prompt spans are stamped with `gen_ai.usage.cache_creation_tokens` /
  `_read_tokens`, so cache effectiveness is visible in OTEL with no new pipeline
  ([Observing with Tael](./observing-with-tael.md)).

### `cacheBreakpoint()` is advisory and coalesced

Authors call `.cacheBreakpoint(ttl?)` to express intent — "freeze everything up
to here as a cacheable prefix." The assembler places at most the provider's
maximum breakpoints at the latest marks that still cover the stable prefix, and
logs a debug event when older marks are dropped (no silent truncation of
intent). Because auto-marking already covers the common case, most authors never
call it; reach for it to pin a large `doc()` with a `1h` TTL across a long,
human-paced run.

---

## Window compaction

`compact()` is explicit, opt-in window management. It splits the chain into the
stable head (system / tools / docs) and the conversation tail, summarizes
everything older than the newest `keepTurns` turns (default 2) **through a
journaled `prompt` host call**, and rebuilds the chain as: head + one `summary`
segment + a fresh cache breakpoint + the kept turns verbatim. The runtime maps
the summary segment to a `<conversation-summary>…</conversation-summary>` user
turn.

```ts
interface CompactOptions {
  keepTurns?: number;     // newest turns kept verbatim (default 2)
  budgetTokens?: number;  // skip (pure no-op) while estimateTokens() ≤ budget
  model?: string;         // summarizer model
  instructions?: string;  // summarizer system prompt (faithful-brief default)
  maxTokens?: number;     // summarizer output cap
  cache?: boolean | CacheTtl | { ttl?: CacheTtl };
  ttl?: CacheTtl;         // TTL of the cache breakpoint on the summary (default "5m")
}
```

`budgetTokens` makes the call a pure no-op (same context value, **no host call**)
while `estimateTokens()` is within budget, so a loop can call it unconditionally:

```ts
for (const question of questions) {
  ctx = await ctx.compact({ budgetTokens: 8000 }); // no-op until the tail grows
  ctx = ctx.user(question);
  const { text, context } = await ctx.prompt();
  ctx = context;
}
```

Because the summary is produced by a **journaled** prompt call, it is durable
and replays deterministically. Compaction is **never automatic** — silent
truncation would change what the model sees, and therefore results, invisibly.
(When you do want it folded into a chat loop automatically-on-overflow, set
`ConversationOptions.compact`, which runs this same budgeted compaction each
turn — still opt-in, still journaled.)

---

## The local prompt cache

Set `CHIDORI_PROMPT_CACHE_DIR=<dir>` to enable a directory-backed,
content-addressed prompt cache keyed on the request digest — a versioned sha256
over the fully assembled request (model, system, tools, messages, cache layout,
max_tokens, temperature), recomputed after any model override so it keys on the
request actually sent.

- **The cache is shared across runs and processes** that point at the same
  directory: one file per digest, written atomically (temp file + rename), so
  concurrent runs are safe. A fleet shares the cache exactly when it shares the
  directory (for example, a mounted volume).
- It is consulted on the **live path only** — replay short-circuits to the
  journaled result first and never reads any cache. A read or parse failure is
  a miss, never an error.
- A hit completes the same journaling sequence as a provider success, recording
  the identical result with `token_usage: None` (nothing was billed). Two runs
  that issue an identical prompt get identical journaled results; the second
  just doesn't pay the provider.
- Disabled (the default), the cache is inert. Both the `chidori.prompt` /
  context paths and the native tool-use loop get it for free.

---

## Caching and replay

Replay re-executes an agent against its journal and returns every journaled
result without sending a request — see [Replay & Resume](./replay.md) for the
model. What matters here is that caching never changes a journaled result:

- **Provider cache changes billing, not output.** A `cache_control` marker tells
  the provider to bill a prefix as a read instead of fresh input; the returned
  content blocks are identical to an uncached send, so the journaled result is
  invariant under cache hits and misses. Only `token_usage` differs (the
  creation-vs-read split), and that is recorded as observed — descriptive
  metadata, never a replay match key.
- **The local cache is served only on the live path**, then journaled as a
  normal record — exactly as if the provider had answered.
- **The digest is self-describing, not a match key.** Every prompt record's args
  carry `request_digest`, but replay matching explicitly ignores it — so a
  digest-scheme change can only cause a local-cache miss (a cost event), never
  force a completed effect to re-execute. `Context.digest()` is pure and never
  journaled.
- **Cache TTL expiry between turns only flips a read back to a creation** — a
  *cost* difference, recorded faithfully, never a *content* difference.
  Auto-marking is a pure function of the request shape, so the same context
  produces the same layout every assembly.

---

## Composition with branching and signals

- **Branching** ([Branching Execution](./branching-execution.md)). In-VM, N
  continuations built from one base `Context` share its segment chain by
  reference — N segment allocations, not N prefix copies. A `chidori.branch`
  variant runs its own module in a fresh VM (parent VFS + JSON input), so a
  `Context` handle does not cross that boundary — but a branch that rebuilds the
  same stable head reads the provider cache the parent already warmed. Each
  branch's prompt records land in the branch's own reserved journal range, so
  caching composes with branch determinism for free.
- **Signals** ([Signals](./signals.md)). A delivered signal's payload can be
  appended as a context segment (a `user`/`toolResult` turn), so
  externally-pushed, multiplayer information enters the conversation as a
  journaled, cacheable turn. The signal is already in the journal; the context
  append is just where it lands in the prompt.

Templating composes cleanly too: [`chidori.template`](./template.md) renders the
*text* that goes inside `.system()` / `.doc()` / `.user()`. Context never
re-implements templating; templating never models turns or caching. They are
orthogonal layers.

---

## Worked example: research assistant over a large corpus

An analyst agent answers a sequence of questions against a fixed corpus. The
system instructions and the corpus are identical for every question, so they are
laid out once as a cache-marked prefix; only the question and the growing Q&A
tail change. Source: `examples/agents/context_qa.ts`.

```ts
import { chidori, run } from "chidori:agent";

run(async (input: { corpus: string; questions: string[] }) => {
  // The stable head, built ONCE and frozen as a cacheable prefix.
  const base = chidori
    .context()
    .system(
      "You are a policy analyst. Answer ONLY from the provided corpus. " +
        "Cite section numbers. If the corpus is silent, say so.",
    )
    .doc("policy-corpus", input.corpus)
    .cacheBreakpoint("5m");

  const answers: { question: string; answer: string }[] = [];
  let ctx = base;
  for (const question of input.questions) {
    // Explicit window management: a pure no-op until the Q&A tail exceeds
    // ~8K estimated tokens, then the older turns fold into one summary segment.
    ctx = await ctx.compact({ budgetTokens: 8000 });
    ctx = ctx.user(question);
    const { text, context } = await ctx.prompt({ type: "final" });
    ctx = context; // assistant turn appended; the corpus prefix stays shared
    answers.push({ question, answer: text });
    await chidori.log("answered", {
      question,
      contextDigest: ctx.digest().slice(0, 12),
    });
  }

  return { answers };
});
```

Run it:

```
chidori run examples/agents/context_qa.ts \
  --input '{"corpus": "Section 1: All deploys require review. Section 2: Rollbacks are automatic.", "questions": ["Who approves deploys?", "What happens on a bad deploy?"]}'
```

On the wire, question by question:

| Turn | Sent prefix (system + corpus) | Billed as |
|---|---|---|
| Q1 | full prefix | **cache *creation*** (~1.25× base, once) |
| Q2 | same prefix + Q1/A1 tail | prefix = **cache *read*** (~0.1× base) + small tail |
| Q3 | same prefix + Q1–Q2 tail | prefix = **cache read** + small tail |
| … | … | … |

The corpus is paid at full rate **once** instead of once per question — roughly a
70–85% reduction in input-token cost on Anthropic pricing, with identical
answers. The split is recorded on each prompt record's `token_usage` in the
journal (and stamped on the OTEL prompt spans — see
[Cache accounting](#cache-accounting)) — `input_tokens` is the fresh share only:

```
prompt Q1   input=45   cache_creation=19,488  cache_read=0       ← warms cache
prompt Q2   input=61   cache_creation=0       cache_read=19,488  ← hit
prompt Q3   input=58   cache_creation=0       cache_read=19,488  ← hit
```

Because each `.prompt()` journals the full assembled request digest and
response, `chidori resume` / `chidori trace` reproduce the exact conversation
without re-billing a token ([Caching and replay](#caching-and-replay) above).
The agent is simultaneously cheap live (provider cache), free on replay
(journal), and fully auditable (digest per turn).

---

## Limitations

- **No raw `Message[]` escape hatch.** `Context` is the structured surface; a
  raw-wire-model path for power users is intentionally not exposed.
- **The local prompt cache is shared exactly as far as its directory.** Runs and
  processes pointing at the same `CHIDORI_PROMPT_CACHE_DIR` share it; there is
  no fleet-wide shared cache beyond that — a fleet shares the cache only if it
  shares that directory or volume.
- **No typed segment-schema registry.** Segments are untyped text/blocks; there
  is no declare-and-validate layer for expected docs.
- **Not supported: provider-specific cache strategies beyond Anthropic/OpenAI**
  (e.g. Gemini implicit caching) behind `cacheBreakpoint`.
- **Compaction is single-strategy** (summarize-older-than-`keepTurns`); there is
  no pluggable compaction policy.
- **Digest canonicalization is versioned but not pluggable** — a scheme change
  just misses the local cache (a cost event), never corrupts replay.
