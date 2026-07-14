# Context Management — Cache-Aware, Composable Prompts

> Provider prompt caching, the `chidori.context` builder, the
> `chidori.conversation` wrapper, window compaction, and the local
> content-addressed prompt cache — on by default where noted below.
> **Related:** `docs/signals.md`, `docs/branching-execution.md`,
> `docs/captured-effects-vfs-crypto-timers.md`, `docs/architecture.md`.
> API reference: `sdk/typescript/src/agent.ts`, `llm.txt`. Example:
> `examples/agents/context_qa.ts`.

## 1. What this is

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
   change is required to get the discount. (§4)
2. **`chidori.context`** — an immutable, content-addressed, turn-structured value
   you build up (`.system()`, `.tools()`, `.doc()`, `.user()`, `.assistant()`)
   and then `.prompt()` against. Appending a turn returns a *new* handle that
   structurally shares the prefix, so the stable head is laid out once and reused
   across every turn, tool-use round, and branch fork. (§2, §3)
3. **`chidori.conversation`** — a small stateful wrapper over `Context` for the
   common chat shape, plus opt-in **window compaction** (`.compact()`) and an
   opt-in **local content-addressed prompt cache**. (§3.3, §5, §6)

The load-bearing correctness property: **caching is always a live-only cost
optimization.** The recorded call-log result is the source of truth; replay
returns it without sending a request or consulting any cache, so an agent stays
fully deterministic and replayable no matter how aggressively it caches. (§7)

---

## 2. Core concept — immutable, prefix-sharing context

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
  allocations, not N prefix copies (§8);
- **cache prefixes stable** — the same stable head produces the same provider
  cache breakpoint every turn, so it warms once and reads thereafter;
- **content-addressed dedup possible** — `digest()` hashes the assembled
  request (a versioned sha256, `host_core::prompt_request_digest`), and the
  local prompt cache (§6) keys on the same digest.

Builder methods are pure in-VM structure building — no host round-trip. Only
`.prompt()` / `.respond()` (and `.compact()`, which issues a summarization
prompt) cross the durable host boundary, so the only recorded effect is the
actual LLM call.

---

## 3. API surface

All types live in `sdk/typescript/src/agent.ts` (authoring-time declarations; the
runtime injects the concrete `chidori` host object). Full reference in `llm.txt`.

### 3.1 `chidori.context` — the builder

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
  // authors never need this; stable heads are auto-marked (§4.1).
  cacheBreakpoint(ttl?: CacheTtl): Context;

  // --- execution (the only durable host calls) ---
  // Send this context; returns the text plus a NEW context with the assistant
  // turn appended (including any internal tool-use exchange).
  prompt(options?: PromptOptions): Promise<{ text: string; context: Context }>;
  // Single structured turn for author-driven tool loops.
  respond(options?: PromptOptions): Promise<{ response: LlmResponseJson; context: Context }>;

  // --- window management (§5) ---
  compact(options?: CompactOptions): Promise<Context>;

  // --- introspection (pure, never recorded) ---
  digest(options?: PromptOptions): string;   // stable content hash of the assembled request
  estimateTokens(): number;                  // rough local estimate for window budgeting
}

// Entry point. Optionally seed from an existing single-shot prompt's options.
chidori.context(seed?: { system?: string; tools?: string[] }): Context;
```

`chidori.prompt(text, opts)` is unchanged — it is effectively sugar for
`chidori.context(opts).user(text).prompt(opts)` discarding the returned context.

### 3.2 `PromptOptions.cache`

`PromptOptions` gains an optional cache posture:

```ts
cache?: boolean | CacheTtl | { ttl?: CacheTtl };
```

Defaults to on (`"5m"`): the runtime marks the stable request head so providers
bill repeated prefixes at the cached rate. `false` disables marking for that call
(use it for a true one-shot where caching would only ever pay the write cost).
`"1h"` (or `{ ttl: "1h" }`) requests the extended TTL. **Caching never changes a
response — only how it is billed.**

### 3.3 `chidori.conversation` — the stateful wrapper

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
see `ConversationLoopOptions` in `agent.ts`.

---

## 4. Provider prompt caching

### 4.1 Default auto-marking — the zero-author win

When assembling any `LlmRequest` — both the single-shot `chidori.prompt` path and
the native tool-use loop (`crates/chidori/src/runtime/native.rs`) — the runtime
auto-marks cacheable boundaries:

- the **`system`** block (stable for the whole run),
- the **`tools`** array (stable for the whole tool-use loop),
- the **newest message**, which freezes the whole conversation head — marked
  whenever a follow-up sharing the prefix is plausible: the request carries
  tools (so a tool-use loop is likely) or is already multi-turn.

This is `host_core::auto_mark_prompt_cache`
(`crates/chidori/src/runtime/host_core.rs`), gated by a default-on posture that
`PromptOptions.cache: false` disables per call
(`host_core::cache_posture_from_options`). So a 10-turn tool-use agent with a 20K
prefix pays for that prefix once instead of ten times, with no code change.

### 4.2 What goes on the wire

- **Anthropic** (`crates/chidori/src/providers/anthropic.rs`): `build_request_body`
  emits `cache_control: {"type":"ephemeral"}` on the marked system block (using
  the structured-system form the API requires for caching), the last tool entry,
  and each marked message's last content block. Marks are coalesced to
  Anthropic's 4-breakpoint cap (latest wins). The
  `anthropic-beta: extended-cache-ttl-2025-04-11` header is sent **only** when a
  `1h` TTL is requested. An unmarked request serializes byte-identically to the
  pre-caching wire format.
- **OpenAI** (`crates/chidori/src/providers/openai.rs`): caching is automatic on
  exact prefixes, so there is no marker to emit — the immutable-prefix design
  feeds it naturally. The path parses `prompt_tokens_details.cached_tokens` and
  reports `input_tokens` as the fresh share so the two providers agree on
  semantics. OpenAI has no cache-write billing, so `cache_creation_tokens` is
  always 0 there.

### 4.3 Cache accounting

- `TokenUsage` (`crates/chidori/src/runtime/call_log.rs`) carries optional
  `cache_creation_tokens` / `cache_read_tokens` (skip-serialized when absent, so
  old logs still deserialize). Anthropic's `cache_creation_input_tokens` /
  `cache_read_input_tokens` are parsed on both the blocking and SSE paths.
- `crates/chidori/src/runtime/cost.rs` prices cache **writes at 1.25×** base
  input and **reads at 0.1×** base for Anthropic (0.5× reads for OpenAI), so
  `total_cost_usd` reflects the real bill.
- `RunSpan` (`crates/chidori/src/runtime/otel.rs`) stamps
  `gen_ai.usage.cache_creation_tokens` / `_read_tokens` on prompt spans, so cache
  effectiveness is visible in OTEL with no new pipeline.

### 4.4 `cacheBreakpoint()` is advisory and coalesced

Authors call `.cacheBreakpoint(ttl?)` to express intent — "freeze everything up
to here as a cacheable prefix." The assembler places at most the provider's
maximum breakpoints at the latest marks that still cover the stable prefix, and
logs a debug event when older marks are dropped (no silent truncation of
intent). Because
auto-marking already covers the common case, most authors never call it; reach
for it to pin a large `doc()` with a `1h` TTL across a long, human-paced run.

---

## 5. Window compaction — `Context.compact()`

`compact()` is explicit, opt-in window management. It splits the chain into the
stable head (system / tools / docs) and the conversation tail, summarizes
everything older than the newest `keepTurns` turns (default 2) **through a
recorded `prompt` host call**, and rebuilds the chain as: head + one `summary`
segment + a fresh cache breakpoint + the kept turns verbatim. The host maps the
summary segment to a `<conversation-summary>…</conversation-summary>` user turn.

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

Because the summary is produced by a **recorded** prompt call, it is durable and
replays deterministically. Compaction is **never automatic** — silent truncation
would change what the model sees, and therefore results, invisibly. (When you do
want it folded into a chat loop automatically-on-overflow, set
`ConversationOptions.compact`, which runs this same budgeted compaction each turn
— still opt-in, still recorded.)

---

## 6. Local content-addressed prompt cache (opt-in)

Set `CHIDORI_PROMPT_CACHE_DIR=<dir>` to enable a process-local cache
(`crates/chidori/src/runtime/prompt_cache.rs`) keyed on `request_digest` — a
versioned sha256 over the fully assembled request (model, system, tools,
messages, cache layout, max_tokens, temperature), recomputed after any model
override so it keys on the request actually sent.

- It is consulted in `execute_prompt_text` / `execute_prompt_response` on the
  **live path only** — strictly *after* the replay short-circuit and the
  completed-host-operation replay decline (§7).
- A hit completes the same begin/safepoint/resolve/record sequence as a provider
  success, recording the identical result with `token_usage: None` (nothing was
  billed). Two runs that issue an identical prompt get identical recorded
  results; the second just doesn't pay the provider.
- Successful live responses write through atomically (temp file + rename).
- Disabled (the default) the module is inert. Both the `chidori.prompt` / context
  paths and the native tool loop get it for free, since all route through the two
  executors.

---

## 7. Determinism & replay (why caching is safe)

Adding provider prompt caching and the local cache does **not** change any
recorded result or any replay:

- **Provider cache changes billing, not output.** A `cache_control` marker tells
  the provider to bill a prefix as a read instead of fresh input; the returned
  content blocks are identical to an uncached send. The recorded
  `CallRecord.result` is the response, so it is invariant under cache
  hits/misses. Only `token_usage` differs (the creation-vs-read split), and that
  is recorded as observed — descriptive metadata, never a replay match key.
- **Replay never sends a request, so never consults any cache.**
  `try_replay_checked(seq, "prompt")` short-circuits to the recorded result
  before request assembly or any provider call. A replayed run can run with
  caching entirely disabled and produce a byte-identical call log. *The log is
  the source of truth; every cache is a live-only optimization* — the same
  argument the signal mailbox makes (see the determinism argument in
  `docs/signals.md`).
- **The local cache is served only on the live path**, after the replay
  short-circuit has already declined, then recorded as a normal `CallRecord` —
  exactly as if the provider had answered.
- **The digest is self-describing, not a match key.** Every prompt record's args
  carry `request_digest`. Replay still matches on `(seq, function)`, not on args
  content; host-promise completed-operation matching explicitly **ignores** the
  digest (`snapshot.rs::completed_args_match`), so a digest-scheme change can
  never force a completed effect to re-execute. `Context.digest()` is a
  synchronous pure host call (`contextDigest`) and is never recorded.

Non-determinism that is **not** introduced: cache TTL expiry between turns only
flips a read back to a creation — a *cost* difference, recorded faithfully, never
a *content* difference. Auto-marking is a pure function of the request shape, so
the same context produces the same layout every assembly.

---

## 8. Composition with branching & signals

- **Branching (`docs/branching-execution.md`).** In-VM, N continuations built
  from one base `Context` share its segment chain by reference (§2) — N segment
  allocations, not N prefix copies. A `chidori.branch` variant runs its own
  module on a fresh `RuntimeContext` (parent VFS + JSON input), so a `Context`
  handle does not cross that boundary — but a branch that rebuilds the same
  stable head reads the provider cache the parent already warmed. The reserved
  per-branch `CallLogSequenceRange` keeps each branch's prompt records in range,
  so caching composes with branch determinism for free.
- **Signals (`docs/signals.md`).** A delivered signal's payload can be appended
  as a context segment (a `user`/`toolResult` turn), so
  externally-pushed, multiplayer information enters the conversation as a
  recorded, cacheable turn. The signal is already in the call log; the context
  append is just where it lands in the prompt.

Templating composes cleanly too: minijinja (`crates/chidori/src/runtime/template.rs`,
`chidori.template()`) renders the *text* that goes inside `.system()` /
`.doc()` / `.user()`. Context never re-implements templating; templating never
models turns or caching. They are orthogonal layers.

---

## 9. Worked example — research assistant over a large corpus

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
call log (and stamped on the OTEL prompt spans, §4.3) — `input_tokens` is the
fresh share only:

```
prompt Q1   input=45   cache_creation=19,488  cache_read=0       ← warms cache
prompt Q2   input=61   cache_creation=0       cache_read=19,488  ← hit
prompt Q3   input=58   cache_creation=0       cache_read=19,488  ← hit
```

Because each `.prompt()` records the full assembled request digest and response,
`chidori resume` / `trace` reproduces the exact conversation and the **replay
pays zero tokens** (§7). The agent is simultaneously cheap live (provider cache),
free on replay (call log), and fully auditable (digest per turn).

---

## 10. Implementation map

Where each piece lives, for maintainers:

| Concern | Location |
|---|---|
| `CacheTtl`, `CacheLayout`, `Message.cache_control` | `crates/chidori/src/providers/mod.rs` |
| `TokenUsage` cache fields | `crates/chidori/src/runtime/call_log.rs` |
| Anthropic `cache_control` emission, beta header, usage parsing | `crates/chidori/src/providers/anthropic.rs` (`build_request_body`, `cache_control_json`) |
| OpenAI `cached_tokens` parsing | `crates/chidori/src/providers/openai.rs` |
| Auto-marking + posture | `crates/chidori/src/runtime/host_core.rs` (`auto_mark_prompt_cache`, `cache_posture_from_options`) |
| Request digest | `crates/chidori/src/runtime/host_core.rs` (`prompt_request_digest`) |
| Cache pricing | `crates/chidori/src/runtime/cost.rs` |
| Span attributes | `crates/chidori/src/runtime/otel.rs` |
| Immutable segment builder, `compact()` | `crates/chidori/src/runtime/typescript/helpers.rs` |
| Context flattening, `context_request_parts`, `contextDigest` | `crates/chidori/src/runtime/typescript/bindings.rs` |
| Completed-op match (ignores digest) | `crates/chidori/src/runtime/snapshot.rs` (`completed_args_match`) |
| Local content-addressed cache | `crates/chidori/src/runtime/prompt_cache.rs` |
| Native tool-use loop (auto-cached) | `crates/chidori/src/runtime/native.rs` |
| SDK types & example | `sdk/typescript/src/agent.ts`, `examples/agents/context_qa.ts` |

`chidori-js` is the only JavaScript engine — everything here ships
unconditionally; there is no engine feature flag to gate on.

---

## 11. Tests

The guarantees above are pinned by `cargo test -p chidori --lib`:

- `providers/anthropic.rs`: `unmarked_request_body_has_no_cache_control`,
  `marked_request_emits_cache_control_layout`,
  `usage_cache_token_fields_parse_and_default`.
- `runtime/cost.rs`: `test_cache_tokens_price_at_documented_multiples`.
- `runtime/prompt_cache.rs`: `store_then_lookup_roundtrips_and_misses_are_none`,
  `disabled_without_env_flag`.

---

## 12. Limitations & not-yet-supported

- **No raw `Message[]` escape hatch.** `Context` is the structured surface; a
  raw-wire-model path for power users is intentionally not exposed.
- **No cross-run / fleet-shared cache.** The local content-addressed cache
  (§6) is process-local, keyed on our own digest; a shared store across a fleet
  is future work.
- **No typed segment-schema registry.** Segments are untyped text/blocks; there
  is no declare-and-validate layer for expected docs.
- **Provider-specific cache strategies beyond Anthropic/OpenAI** (e.g. Gemini
  implicit caching) are not wired behind `cacheBreakpoint` yet.
- **Compaction is single-strategy** (summarize-older-than-`keepTurns`); there is
  no pluggable compaction policy.
- **Digest canonicalization is versioned but not pluggable** — a scheme change
  just misses the local cache (a cost event), never corrupts replay.
