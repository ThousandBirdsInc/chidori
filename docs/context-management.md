# Context Management — Cache-Aware, Composable Prompts

> **Status:** Phases 1, 2, and 3 are **implemented** (see the
> implementation-status section below). The doc was drafted before the QuickJS
> removal (#39); references to a "QuickJS path" / `rust-engine` feature
> describe the pre-#39 tree — `chidori-js` is now the only engine and
> everything here ships on it unconditionally.
> **Related:** `docs/signals.md`, `docs/branching-execution.md`,
> `docs/captured-effects-vfs-crypto-timers.md`, `docs/pure-rust-js-engine-plan.md`.

## Implementation status (Phases 1–3 landed)

**Phase 1 — cache-aware request layout + accounting** (no author API change):

- `src/providers/mod.rs`: `CacheTtl` (`"5m"`/`"1h"`), request-level
  `CacheLayout { system, tools }` on `LlmRequest`, and a per-message
  `Message.cache_control` marker for the conversation head. All additive and
  skip-serialized, so existing checkpoints round-trip unchanged.
- `src/providers/anthropic.rs`: `build_request_body` emits
  `cache_control: {"type":"ephemeral"}` on the marked system block
  (structured-system form), the last tool entry, and each marked message's
  last content block; coalesces marks to Anthropic's 4-breakpoint cap
  (latest wins); sends the `extended-cache-ttl-2025-04-11` beta header only
  when a `1h` TTL is requested; parses `cache_creation_input_tokens` /
  `cache_read_input_tokens` on both the blocking and SSE paths. An unmarked
  request serializes byte-identically to the pre-caching wire format.
- `src/providers/openai.rs`: parses `prompt_tokens_details.cached_tokens`
  (send + stream) and reports `input_tokens` as the fresh share so the two
  providers agree on semantics. No marker emission (OpenAI caching is
  automatic on exact prefixes — which the immutable-prefix design feeds).
- Auto-marking (`host_core::auto_mark_prompt_cache`): system and tools are
  always marked; the conversation head is marked when a follow-up sharing the
  prefix is plausible (tools present or the request is already multi-turn).
  Default on; `cache: false` in prompt options disables per call
  (`host_core::cache_posture_from_options`). The native tool loop
  (`src/runtime/native.rs`) and every `chidori.prompt` route through it.
- Accounting: `TokenUsage` gained optional `cache_creation_tokens` /
  `cache_read_tokens` (skip-if-none); `cost.rs` prices writes at 1.25x and
  reads at 0.1x base input for Anthropic (0.5x reads for OpenAI); `RunSpan`
  stamps `gen_ai.usage.cache_creation_tokens` / `_read_tokens` on prompt
  spans.

**Phase 2 — `chidori.context` + full-prompt recording:**

- The immutable builder lives in the shared JS helpers
  (`src/runtime/typescript/helpers.rs`): each builder call allocates one
  frozen segment node pointing at its parent (structural prefix sharing);
  only `.prompt()`/`.respond()` cross the host boundary, forwarding the
  flattened chain in the prompt effect's options (`__context`).
- `src/runtime/typescript/bindings.rs` flattens segments into
  system/tools/messages plus the explicit `cacheBreakpoint` layout
  (`context_request_parts`), runs the same durable
  `execute_prompt_text`/`execute_prompt_response` path (tool loops included),
  and returns the appended turns so the JS side extends the context with the
  full exchange. `Context.digest()` is a synchronous pure host call
  (`contextDigest`), never recorded.
- Every prompt record's args now carry `request_digest` — a versioned sha256
  over the canonicalized assembled request (model, system, tools, messages,
  cache layout) — closing the §4.4 partial-capture gap. Replay still matches
  on `(seq, function)`; host-promise completed-operation matching explicitly
  ignores the digest (`snapshot.rs::completed_args_match`) so a digest-scheme
  change can never force a completed effect to re-execute.
- SDK types (`sdk/typescript/src/agent.ts`): `Context`, `CacheTtl`,
  `LlmResponseJson`, `PromptOptions.cache`, `chidori.context()`. Example:
  `examples/agents/context_qa.ts`. API reference: `llm.txt`.

**Phase 3 — window compaction + local content-addressed prompt cache:**

- `Context.compact(options?)` (`src/runtime/typescript/helpers.rs`): explicit,
  opt-in window compaction. Splits the chain into the stable head
  (system/tools/doc) and the conversation tail, summarizes everything older
  than the newest `keepTurns` turns (default 2) through a **recorded `prompt`
  host call** (so the summary is durable and replays deterministically), and
  rebuilds the chain as head + one `summary` segment + a fresh
  `cacheBreakpoint` + the kept turns verbatim. `budgetTokens` makes the call a
  pure no-op (same context value, no host call) while `estimateTokens()` is
  within budget, so loops can call it unconditionally; `model`,
  `instructions`, `maxTokens`, and `ttl` tune the summarizer. The host maps
  the `summary` segment to a `<conversation-summary>…</conversation-summary>`
  user turn (`bindings.rs::context_request_parts`). Never automatic — silent
  truncation would change results invisibly (§3 non-goal holds).
- Local content-addressed prompt cache (`src/runtime/prompt_cache.rs`):
  opt-in via `CHIDORI_PROMPT_CACHE_DIR=<dir>`; one JSON entry per
  `request_digest` (the §8.3 digest over the fully assembled request,
  recomputed after model overrides so it keys on the request actually sent).
  Consulted in `execute_prompt_text` / `execute_prompt_response` on the live
  path only — strictly **after** the replay short-circuit and
  completed-host-operation replay decline (§10) — and a hit completes the
  same begin/safepoint/resolve/record sequence as a provider success, with
  the identical recorded result and `token_usage: None` (nothing was billed).
  Successful live responses write through (atomic temp-file + rename).
  Disabled (the default) the module is inert. Both the `chidori.prompt` /
  context paths and the native tool loop get it for free since all route
  through the two executors.
- Budget helpers: `estimateTokens()` (Phase 2) + `compact({ budgetTokens })`
  as the decide-when-to-compact primitive.
- SDK: `CompactOptions` + `Context.compact()` in `sdk/typescript/src/agent.ts`;
  API reference in `llm.txt`.

---

## 1. Summary (TL;DR)

Give agents a first-class way to **manage the context they push into an LLM
prompt** — one that (a) is **cache-aware** so repeated prefixes hit the model
provider's prompt cache and our own replay/dedup caches instead of being re-sent
and re-billed, and (b) **composes as conversational turns** instead of being
hand-rolled by string concatenation.

The unit is **`chidori.context`**: an **immutable, content-addressed,
turn-structured** value that you build up (`.system(...)`, `.tools(...)`,
`.doc(...)`, `.user(...)`, `.assistant(...)`) and then `.prompt()` against.
Because it is immutable and persistent, appending a turn returns a *new* handle
that **structurally shares the prefix** with its parent — so the stable head of a
conversation (system + tools + long reference docs) is laid out once, marked with
**explicit cache breakpoints**, and reused across every subsequent turn, every
tool-use round, and every branch fork. Each segment is hashed, so identical
prefixes dedupe in our own store and produce a stable `cache_control` prefix for
the provider.

Two things are missing today and this doc adds both:

1. **Provider prompt caching** does not exist anywhere in the codebase (verified
   in §4.5). Today every `chidori.prompt` and every turn of the native tool-use
   loop re-sends the entire system prompt, tool schemas, and accumulated history
   at full input-token price. Anthropic (and OpenAI) will cache a marked prefix
   and bill it at a fraction — but only if we emit the cache-control layout. We
   don't.
2. **Composable multi-turn context** is not exposed to agent authors.
   `chidori.prompt(text)` builds a *single* `user` message every call (§4.2); the
   only place that accumulates a real `messages` array is the internal native
   agent loop (`src/runtime/native.rs`), which authors can't drive. To thread a
   conversation today you concatenate strings by hand.

Roughly half the substrate already exists — the `Message`/`ContentBlock` model,
the `LlmRequest` struct, the durable `CallRecord`, the provider HTTP path. The
new work is: a cache-control field threaded through the request, default
auto-marking of stable segments, cache-usage accounting in the trace, the
immutable `Context` builder + its binding, and recording the full assembled
prompt (not just `{text, model}`) in the call log.

---

## 2. Motivation (the "why")

### 2.1 We pay full price for context we send over and over

A tool-use agent today (`src/runtime/native.rs:193-200`) runs a loop: send
`[system, user]`, get an `assistant` turn with tool calls, append the assistant
message + `tool_result` messages, send **the whole growing array again**, repeat.
Every turn re-transmits the system prompt, every tool schema, and the entire
prior conversation. Anthropic and OpenAI both price cached prefix tokens at a
large discount (Anthropic: cache *reads* are ~10% of base input price), but the
discount only applies if the request marks a cache breakpoint. We emit none
(§4.5), so a 10-turn agent with a 20K-token system+tools+docs prefix pays for
~200K input tokens that could have been ~20K + 9×(cache reads). This is the
single biggest cheap win available in the LLM path, and it is invisible to the
agent author — it is purely a request-layout concern.

### 2.2 Authors hand-roll conversation context

The public surface is `chidori.prompt(text, options)` → returns a `string`
(`sdk/typescript/src/agent.ts:151`). There is no `messages` parameter, no
conversation handle, no way to say "continue the previous exchange." An author
who wants a three-turn refinement writes:

```ts
const a1 = await chidori.prompt(`${SYSTEM}\n\nUser: ${q1}`);
const a2 = await chidori.prompt(`${SYSTEM}\n\nUser: ${q1}\nAssistant: ${a1}\nUser: ${q2}`);
```

This is error-prone (role boundaries become string formatting), wastes tokens (no
cache structure — it's all one opaque `user` block), and throws away the
structured `ContentBlock` model the provider layer already speaks internally.

### 2.3 Context is the thing branching and signals operate on

`docs/branching-execution.md` forks an agent to explore strategies; the expensive
shared thing those forks have in common is **the context prefix**. If context is
an immutable, prefix-sharing value, a fork is nearly free and every branch's
first prompt is a cache hit on the shared head. `docs/signals.md` pushes
externally-authored information into a run; the natural destination for a signal's
payload is **a context segment** (`ctx.appendSignal(sig)`), so steering enters the
conversation as a first-class, recorded turn. Context management is the substrate
both features want underneath them. §12 covers the composition.

### 2.4 Why chidori specifically: caching that can't corrupt replay

Caching and determinism look like they're in tension — a cache hit returns a value
the run "didn't compute." Chidori already resolves exactly this tension for every
other effect via the call log: **the recorded result is the source of truth; any
cache is a live-only cost optimization that never changes what's recorded or
replayed** (the same argument the signals mailbox makes, `docs/signals.md` §10).
Provider prompt caching changes *how a request is billed*, not its response;
content-addressed local caching changes *whether we call the provider at all*, but
the recorded `CallRecord.result` is identical either way. So chidori can layer
aggressive caching under a fully deterministic, replayable agent — the
differentiator, the same one that makes signals and branching safe.

---

## 3. Goals / Non-Goals

**Goals**
- A cache-control layout is emitted to providers so stable prompt prefixes
  (system, tools, large docs, the rolling conversation head) are cached and billed
  at the discounted rate. This applies **immediately to the existing native
  tool-use loop** with no author change (auto-marking, §8.1).
- Cache usage (creation vs read tokens) is captured in `TokenUsage` and surfaced
  in the trace and cost accounting, so the win is measurable.
- A first-class **`chidori.context`** value lets authors compose multi-turn
  conversations as structured turns, immutably, with prefix sharing.
- The full assembled prompt (messages + system + tools + cache layout, captured as
  a digest) is recorded in the prompt `CallRecord`, closing the current
  partial-capture gap (§4.4).
- Determinism is preserved exactly: caching is live-only; replay returns the
  recorded result and never re-sends or re-reads a cache.
- Reuse the existing `Message`/`ContentBlock`/`LlmRequest`/provider machinery; do
  not build a parallel prompt path.

**Non-Goals (initially)**
- Automatic, magic context-window management (summarize-when-full) as a default.
  Phase 3 adds an *explicit, opt-in* `.compact()` transform; silent truncation is
  never default — it would change results invisibly.
- A new prompt-templating system. minijinja already exists
  (`src/runtime/template.rs`) and renders *text*; context composition is about
  *turn structure*, a layer above templated text (§11).
- Cross-provider cache portability guarantees. The local content-addressed cache
  (Phase 3) is keyed on our own digest and is provider-independent, but the
  *provider's* cache is theirs (Anthropic 5-min/1-hr TTL, OpenAI automatic).
- Streaming-specific cache semantics beyond what the provider already does.

---

## 4. Background (verified, with file references)

### 4.1 The message / content-block model already exists
`src/providers/mod.rs:13-53`: `ContentBlock` is a `#[serde(tag = "type",
rename_all = "snake_case")]` enum — `Text { text }`, `ToolUse { id, name, input
}`, `ToolResult { tool_use_id, content, is_error }`. `Message { role: String,
content: Vec<ContentBlock> }` with constructors `user_text` and `assistant_blocks`
(`mod.rs:39-53`). This is the structured turn model context composition will build
on; it's already the internal lingua franca.

### 4.2 How a prompt request is assembled today
`LlmRequest { model, messages, system: Option<String>, temperature, max_tokens,
tools: Vec<ToolSchema> }` (`src/providers/mod.rs:73-81`). For a plain
`chidori.prompt(text)` the host builds a **single-message** array
`vec![Message::user_text(text)]` (verified in the binding path; `system` is a
separate optional string, `tools` a separate vec). The **native agent loop**
(`src/runtime/native.rs:193-200`) is the only place a multi-turn `messages` array
accumulates: after each response it pushes `Message::assistant_blocks(...)` then
the tool results, and re-sends the full array next turn. That accumulation is
internal — no author-facing handle exposes it.

### 4.3 The provider HTTP path
`src/providers/anthropic.rs:110-220` (`send`) maps each `Message` to
`json!({ "role", "content": blocks })` via `content_block_to_anthropic_json`,
then builds `body = { model: resolve_alias(model), messages, max_tokens }`, adds
`body["system"]` and `body["tools"]` when present (`anthropic.rs:134-144`). POSTs
to `https://api.anthropic.com/v1/messages` with `anthropic-version: 2023-06-01`.
The response usage struct is `AnthropicUsage { input_tokens, output_tokens }`
(`anthropic.rs:59-63`) — note it does **not** parse the cache token fields the API
returns. OpenAI is parallel (`src/providers/openai.rs`). Both implement the
`LlmProvider` trait (`mod.rs:104-126`) with `send` + a streaming `stream`.

### 4.4 What gets recorded for a prompt call (the partial-capture gap)
`src/runtime/host_core.rs:271-351` (`execute_prompt_text`) and `:353-437`
(`execute_prompt_response`) drive the durable boundary: `next_seq()`,
`try_replay_checked(seq, "prompt")`, `replay_completed_host_operation(...,
PendingHostOperationKind::Prompt, ...)`, else begin host op + `send_prompt_request`
+ `record_call`. The recorded `CallRecord` (`src/runtime/call_log.rs:6-38`) stores
`args` = the JSON the binding passed (`{ text, model, type?, tools?, turn?,
max_turns? }`) and `result` = the response text (text path) or the full
`llm_response_to_json` (tool path, `host_core.rs:477-490`). **`args` does not
contain the system prompt or the assembled `messages` array** — so the durable
record is a *partial fingerprint* of what was actually sent. Replay still works
(it matches on `seq` + function name, not on args content), but the log can't
fully reconstruct the request, and can't key a content-addressed cache. Closing
this is part of Phase 2.

### 4.5 ⚠️ No prompt caching exists anywhere
A search for `cache_control`, `ephemeral`, `prompt_cache`, `cache_creation`,
`cache_read` across `src/` finds **nothing** in the LLM path (the only "cache"
hits are the WASM sandbox artifact cache, `src/runtime/sandbox.rs`, unrelated).
`AnthropicUsage` (`anthropic.rs:59-63`) and `TokenUsage` (`call_log.rs:34-38`)
have only `input_tokens` / `output_tokens`. So: no request marks a cache
breakpoint, and even if a prefix *were* cached server-side we wouldn't observe it
because we discard the `cache_creation_input_tokens` / `cache_read_input_tokens`
fields. This is greenfield.

### 4.6 Token & cost accounting
`CallLog::total_tokens` (`call_log.rs:66-76`) sums `token_usage`;
`total_cost_usd` (`:84-98`) walks `prompt` records, reads `args["model"]`, and
calls `estimate_cost_usd(model, input, output)` (`src/runtime/cost.rs`). Once
cache tokens are captured they can be priced separately (cache writes cost ~1.25×
base, reads ~0.1× base on Anthropic) for accurate cost reporting.

### 4.7 Tracing
`record_call` → `RunSpan::stream_record` streams each call's span by `parent_seq`
(`docs/signals.md` §4.6). A `prompt` span already carries token usage; adding
cache-read/creation counts as span attributes makes cache effectiveness visible in
tael with no new pipeline.

### 4.8 Templates (a different layer)
`src/runtime/template.rs` (minijinja) renders *strings* — `render_string`,
`render_file`, `render` — exposed to JS as `chidori.template()`. It produces the
*text* that goes **inside** a segment; it does not model turn structure or caching.
Context composition sits above it (§11).

---

## 5. Design overview

Three layers, each shippable on its own:

1. **Cache-aware request layout (provider caching).** Add an optional cache-control
   marker to the request model and emit Anthropic `cache_control: {type:
   "ephemeral"}` (and the OpenAI equivalent posture) at stability boundaries. Mark
   the system block, the tools array, and the conversation head **by default**, so
   the existing native loop and every `chidori.prompt` benefit with zero author
   change. Capture cache token counts in `TokenUsage`. This is pure plumbing on
   the substrate that already exists — no new author API.

2. **The `chidori.context` builder (composition).** An **immutable,
   content-addressed, turn-structured** value. Building methods return new handles
   that structurally share the prefix (a persistent cons-list of segments with
   parent pointers + cached digests). `.prompt()` walks the segment chain,
   assembles a cache-aware `LlmRequest`, places cache breakpoints at the marked
   stability boundaries, sends, and returns `{ text, context }` where `context` is
   the parent with the new `assistant` turn appended. This is the author-facing
   feature.

3. **Window management + our own cache (Phase 3).** `.compact(strategy)` is an
   explicit transform that summarizes old turns into one segment (itself a recorded
   host call). A local content-addressed prompt cache, keyed on the full context
   digest from §4.4's fix, can short-circuit the provider entirely on an exact
   repeat — provider-independent, opt-in, and still recorded identically.

The load-bearing correctness idea is in §10: **a context digest and a cache marker
are request metadata; the recorded `result` is unchanged by either, and replay
never consults a cache.** Caching lives entirely on the live path.

---

## 6. API surface

### 6.1 Agent-facing (`chidori.context`)

```ts
import { chidori } from "chidori";

type Role = "system" | "user" | "assistant" | "tool_result";
type CacheTtl = "5m" | "1h";

interface Context {
  // --- builders (immutable: each returns a NEW Context sharing the prefix) ---
  system(text: string): Context;
  tools(names: string[]): Context;           // by tool name, resolved like prompt({tools})
  doc(label: string, text: string): Context; // a large stable reference block
  user(text: string): Context;
  assistant(text: string): Context;          // or assistant blocks (tool-use turns)
  toolResult(id: string, content: string, isError?: boolean): Context;

  // Freeze everything appended so far as a cacheable prefix. Maps to one
  // provider cache breakpoint. Providers cap breakpoints (Anthropic: 4), so
  // the runtime coalesces; explicit calls express author intent.
  cacheBreakpoint(ttl?: CacheTtl): Context;

  // --- execution ---
  // Assemble a cache-aware request from this context, send it, and return the
  // text plus a NEW context with the assistant turn appended.
  prompt(options?: PromptOptions): Promise<{ text: string; context: Context }>;
  // Tool-use variant: returns the structured response + the extended context.
  respond(options?: PromptOptions): Promise<{ response: LlmResponse; context: Context }>;

  // --- introspection ---
  digest(): string;             // content hash of the assembled request (stable)
  estimateTokens(): number;     // local estimate for window budgeting
}

// Entry point. Optionally seed from an existing single-shot prompt's options.
chidori.context(seed?: { system?: string; tools?: string[] }): Context;
```

Design notes:
- **Immutability is the whole point.** `base.user("a")` and `base.user("b")` are
  two independent contexts that share `base`'s segments by reference. This is what
  makes branch forks cheap and cache prefixes stable.
- **`cacheBreakpoint()` is advisory + coalesced.** Authors mark intent; the
  assembler (§8) places at most the provider's max breakpoints at the latest marks
  that still cover the stable prefix. Default auto-marking (§8.1) means most
  authors never call it.
- `chidori.prompt(text, opts)` stays exactly as-is — it becomes sugar for
  `chidori.context(opts).user(text).prompt(opts)` discarding the returned context.
  No breaking change (`docs/branching-execution.md`-style additive constraint).

### 6.2 SDK types
Add `Context`, `Role`, `CacheTtl` and the `context()` method to the `Chidori`
interface in `sdk/typescript/src/agent.ts` (types only; the runtime supplies the
methods, like every other host method). `PromptOptions` (already at
`agent.ts:38-50`) gains an optional `cache?: boolean | { ttl?: CacheTtl }` to
opt a single-shot prompt into/out of auto-marking.

---

## 7. Worked example — a research assistant over a large corpus

**Scenario.** An analyst agent answers a sequence of questions against a fixed
50-page policy corpus. The corpus (≈18K tokens) and the system instructions
(≈1.5K tokens) are **identical for every question**; only the question and the
growing Q&A tail change. This is the textbook prompt-cache shape — and today
chidori would re-bill the full ~20K prefix on every single turn.

### 7.1 The agent

```ts
import { chidori, run } from "chidori";

type Brief = { corpusPath: string; questions: string[] };

run(async (brief: Brief) => {
  const corpus = await chidori.readFile(brief.corpusPath); // VFS, deterministic

  // Build the stable head ONCE and freeze it as a cacheable prefix.
  const base = chidori
    .context()
    .system(
      "You are a policy analyst. Answer ONLY from the provided corpus. " +
      "Cite section numbers. If the corpus is silent, say so."
    )
    .doc("policy-corpus", corpus)   // ~18K tokens, stable across every question
    .cacheBreakpoint("1h");         // <- one provider cache breakpoint here

  const answers: { q: string; a: string }[] = [];
  let ctx = base;

  for (const q of brief.questions) {
    ctx = ctx.user(q);
    const { text, context } = await ctx.prompt({ model: "claude-sonnet" });
    ctx = context;                  // assistant turn appended, prefix still shared
    answers.push({ q, a: text });
    await chidori.log("answered", {
      q,
      digest: ctx.digest().slice(0, 12),
    });
  }

  return { answers };
});
```

What happens on the wire, question by question:

| Turn | Sent prefix (system + corpus) | Billed as |
|---|---|---|
| Q1 | ~19.5K tokens | **cache *creation*** (~1.25× base, once) |
| Q2 | same prefix + Q1/A1 tail | prefix = **cache *read*** (~0.1× base) + small tail |
| Q3 | same prefix + Q1–Q2 tail | prefix = **cache read** + small tail |
| … | … | … |

For 8 questions, the corpus is paid at full rate **once** instead of eight times.
On Anthropic's pricing that is roughly a 70–85% reduction in input-token cost for
this agent — achieved by the `.cacheBreakpoint()` placement and the immutable
prefix sharing, with no change to the answers.

### 7.2 What the trace shows (tael)

```
agent.run research-assistant
├─ host.read_file   policy.md                      18,142 corpus tokens
├─ host.prompt      Q1   in=19,533  cache_creation=19,488  cache_read=0      ← warms cache
├─ host.log         answered  digest=9f2a1c4b…
├─ host.prompt      Q2   in=19,612  cache_creation=0       cache_read=19,488 ← hit
├─ host.prompt      Q3   in=19,701  cache_creation=0       cache_read=19,488 ← hit
└─ …
```

The `cache_read` attribute (Phase 1, §8.4) makes the win **measurable** — without
it, cache effectiveness is invisible. `chidori trace <run>` and `total_cost_usd`
price the creation/read tokens separately for an accurate bill.

### 7.3 The durability payoff

Because `.prompt()` records the full assembled request digest (§4.4 fix) and the
response in the call log, `chidori resume` / `trace` reproduces the exact
conversation — every turn, in order — and the **replay pays zero tokens** (it
returns recorded results; §10). So the agent is simultaneously: cheap live (provider
cache), free on replay (call log), and fully auditable (digest per turn). A raw
"concatenate strings and hope for a cache hit" approach gives none of these
cleanly.

---

## 8. Cache-aware assembly (the core mechanism)

### 8.1 Default auto-marking — the zero-author win
When assembling any `LlmRequest` (both the single-shot `chidori.prompt` path and
the native tool-use loop), the runtime auto-marks cacheable boundaries:
- the **`system`** block (stable for the whole run),
- the **`tools`** array (stable for the whole tool-use loop),
- the **conversation head up to the last stable turn** (everything except the
  newest user turn), coalesced to fit the provider's breakpoint budget.

This requires **no `chidori.context` adoption** — `src/runtime/native.rs`'s loop
and every existing `chidori.prompt` immediately emit a cache layout. Auto-marking
is gated by a default-on `cache` posture that `PromptOptions.cache: false` can
disable per call (e.g. for a one-shot where caching would only ever pay the
write cost).

### 8.2 Request-model changes (`src/providers/`)
- `mod.rs`: add an optional cache marker. Minimal shape: a per-block
  `cache_control: Option<CacheControl>` on `ContentBlock` (matches Anthropic's
  block-level model) **plus** request-level flags for `system`/`tools` caching on
  `LlmRequest`. `CacheControl { ttl: CacheTtl }` serializes to `{ "type":
  "ephemeral" }` (+ `"ttl": "1h"` when extended). Additive; existing constructors
  default to `None` so nothing else changes.
- `anthropic.rs:110-144` (`send`): when a block/section is marked, append
  `"cache_control": {"type":"ephemeral"[, "ttl":"1h"]}` to its emitted JSON; mark
  `system` by wrapping it as a content array with a trailing `cache_control` (the
  API requires the structured-system form for caching); cap at 4 breakpoints. Add
  the `anthropic-beta: extended-cache-ttl-2025-04-11` header **only** when a `1h`
  ttl is requested.
- `openai.rs`: OpenAI prompt caching is automatic on exact prefixes (no
  `cache_control` to emit), so the OpenAI path needs no marking — but it should
  parse and report `prompt_tokens_details.cached_tokens` for symmetry (§8.4).

### 8.3 The assembler
A small `assemble_request(context_or_messages, options) -> LlmRequest` step
(natural home: `src/runtime/host_core.rs`, beside the prompt executors) that:
1. flattens the context segment chain (or takes the native loop's `messages`),
2. lifts `system`/`tools` out,
3. applies auto-marking (§8.1) and any explicit `cacheBreakpoint` marks,
4. computes the **content digest** (sha2 over the canonicalized request — model,
   system, tools, messages, cache layout) for recording (§4.4) and the local
   cache key (Phase 3).

### 8.4 Cache accounting
- `anthropic.rs:59-63`: extend `AnthropicUsage` with
  `cache_creation_input_tokens` and `cache_read_input_tokens` (both
  `#[serde(default)]`, since older/other responses omit them).
- `call_log.rs:34-38`: extend `TokenUsage` with optional
  `cache_creation_tokens` / `cache_read_tokens` (skip-serialize-if-none → old
  logs still deserialize).
- `cost.rs`: price creation (~1.25× base input) and read (~0.1× base) separately
  in `estimate_cost_usd`.
- `RunSpan::stream_record`: stamp the two counts as span attributes (Phase 1
  visibility).

### 8.5 Determinism, nesting, tracing — inherited
The assembled `prompt` call still flows through `execute_prompt_text` /
`execute_prompt_response` (§4.4), so it records via `record_call`, carries
`parent_seq`, and streams its span exactly as today. Cache layout and digest ride
in the (now complete) `args`; nothing about the recording or replay path forks.

---

## 9. The context value (composition mechanics)

### 9.1 Representation
A `Context` is an **immutable singly-linked chain of segments** with parent
pointers and memoized digests:

```
Segment { role, blocks: Vec<ContentBlock>, cache_mark: Option<CacheTtl>,
          parent: Option<Arc<Segment>>, prefix_digest: Digest }
```

`prefix_digest` is `hash(parent.prefix_digest ++ this_segment)`, computed once at
construction. Appending is O(1) (allocate one segment, point at the parent);
`base.user("a")` and `base.user("b")` share every node up to `base`. This is the
persistent-data-structure property that makes (a) branch forks cheap, (b) the
cache prefix digest stable, and (c) our content-addressed store able to dedupe.

### 9.2 Host binding (chidori-js)
In `crates/chidori-js/src/lib.rs::install_chidori_effects`, `chidori.context`
returns a handle (an opaque integer id into a per-run `ContextTable`, mirroring how
host promises are tabled). Builder methods (`system`, `user`, `doc`, …) take the
handle + args, allocate a child segment, and return the new handle — all
synchronous, no host round-trip (pure in-VM structure building). Only `.prompt()`
/ `.respond()` cross into the durable host path via the existing `forward_effect`
bridge, carrying the **flattened segment chain** as the prompt args. This keeps
the builder cheap and the only recorded effect the actual LLM call.

> Implementation choice to settle (§15): the segment chain can live VM-side (a JS
> object, simplest) or host-side in a `ContextTable` (enables host-side digesting
> and the Phase 3 cross-run cache). The doc assumes host-side for the digest/cache
> story; a VM-side MVP is viable if Phase 3 is deferred.

### 9.3 `prompt()` / `respond()`
`ctx.prompt(opts)` → flatten chain → `assemble_request` (§8.3) → reuse
`execute_prompt_text` → on success, return `{ text, context: ctx.assistant(text)
}`. `respond()` returns the structured `LlmResponse` and appends an
`assistant_blocks` segment (+ lets the author append `toolResult` segments),
giving authors the native loop's power explicitly and durably.

---

## 10. Determinism & caching analysis (the crux)

**Claim:** adding provider prompt caching and a local content-addressed cache does
not change any recorded result or any replay.

- **Provider cache changes billing, not output.** A `cache_control` marker tells
  the provider to bill a prefix as a read instead of fresh input; the returned
  content blocks are identical to an uncached send. The recorded
  `CallRecord.result` is the response, so it is invariant under cache hits/misses.
  Only `token_usage` differs (creation vs read split), and that is *recorded as
  observed* — it is descriptive metadata, never a replay match key.
- **Replay never sends a request, so never consults any cache.**
  `try_replay_checked(seq, "prompt")` (`host_core.rs:281-286`) short-circuits to
  the recorded result before `assemble_request` or any provider call. A replayed
  run can run with caching entirely disabled and produce a byte-identical call log.
  **The log is the source of truth; every cache is a live-only optimization** —
  the identical argument `docs/signals.md` §10 makes for the signal mailbox.
- **The local content-addressed cache (Phase 3) is keyed on the full request
  digest** (§8.3) and only ever served on the *live* path, *before* the provider
  call, *after* the replay short-circuit has already declined. So: replay → never
  reaches it; fresh live call with a digest seen before → served from local cache,
  then **recorded as a normal `CallRecord`** exactly as if the provider had
  answered. Two runs that issue an identical prompt get identical recorded
  results; the second just didn't pay the provider. Determinism within each run is
  untouched.
- **The digest closes the §4.4 gap without changing matching.** Replay still
  matches on `(seq, "prompt")`, not on args. The richer `args` (full assembled
  request) is additive — it makes the record self-describing and cache-keyable,
  and is ignored by the existing matcher.

**Non-determinism that is *not* introduced:** cache TTL expiry (a 5-min/1-hr
provider window lapsing between turns) only flips a read back to a creation — a
*cost* difference, recorded faithfully, never a *content* difference. Auto-marking
is a pure function of the request shape, so the same context produces the same
layout every assembly.

---

## 11. Edge cases, risks, relationship to templates

- **Breakpoint budget overflow.** Anthropic allows ≤4 cache breakpoints. The
  assembler coalesces: keep system + tools + the latest qualifying conversation-
  head mark, drop the oldest explicit marks first; `log()` when marks are dropped
  (no silent truncation of *intent*, per the signals "no silent caps" rule).
- **Tiny prefixes.** Caching a prefix below the provider minimum (Anthropic
  caches in ~1K-token increments; very small prefixes won't cache) only ever wastes
  the marker, not correctness. Auto-marking can skip prefixes under a threshold.
- **Mutating a "stable" segment mid-run** breaks the prefix and forces a cache
  re-creation. Immutability makes this impossible *by construction* for a given
  `Context` chain — you get a new chain, and the assembler will simply re-create
  the cache for the changed prefix. Expected and safe.
- **OpenAI has no explicit marker.** Its caching is automatic on exact prefix
  matches, so the immutable-prefix design *still* helps (stable byte-prefix → its
  automatic cache), and we just report `cached_tokens`. No OpenAI-specific marking
  code.
- **Templates vs context.** minijinja (`template.rs`, §4.8) renders the *text*
  inside `.system(...)` / `.doc(...)` / `.user(...)`. Templating + context compose
  cleanly: render to a string, drop it in a segment. Context never re-implements
  templating; templating never models turns or caching. Keep them orthogonal.
- **Digest stability across versions.** The digest must be computed over a
  canonical form (sorted keys, normalized whitespace policy documented) so it's
  stable run-to-run for the Phase 3 cache. Version the canonicalizer; a digest
  scheme change just misses the local cache (a cost event), never corrupts replay.

### 11.1 Composition with branching & signals
- **Branching (`docs/branching-execution.md`).** A fork that inherits a parent
  `Context` shares its segment chain by reference (§9.1): forking N branches costs
  N segment allocations, not N prefix copies, and each branch's first `.prompt()`
  is a provider cache hit on the shared, already-warmed prefix. The reserved
  per-branch `CallLogSequenceRange` keeps each branch's prompt records in range, so
  caching composes with branch determinism for free.
- **Signals (`docs/signals.md`).** A delivered signal's payload can be appended as
  a segment — `ctx = ctx.appendSignal(sig)` (sugar for a `user`/`tool_result`
  segment tagged with `from`) — so externally-pushed, multiplayer information
  enters the conversation as a recorded, cacheable turn. The signal is already in
  the call log (§10 there); the context append is just where it lands in the
  prompt.

---

## 12. Alternatives considered
- **Auto-mark caching only, never expose `chidori.context`.** Captures the cost
  win (§8.1) but leaves authors hand-rolling multi-turn context (§2.2) and leaves
  the partial-capture gap (§4.4). Phase 1 ships exactly this as a standalone win;
  Phases 2–3 are the rest.
- **Expose the native agent loop's `messages` array directly** (let authors pass a
  raw `Message[]`). Rejected as the primary surface: it leaks the wire model, has
  no prefix-sharing or cache-intent expression, and re-creates the
  string-threading footguns at the array level. `Context` is the structured,
  immutable layer over the same `Message`s. (A raw-messages escape hatch is fine as
  future work.)
- **A mutable conversation object (`ctx.push(...)` in place).** Rejected:
  mutation destroys prefix sharing (every branch/fork would deep-copy) and makes
  cache-prefix stability a runtime invariant to police rather than a structural
  guarantee. Immutability is load-bearing, not stylistic.
- **Provider-agnostic "cache hints" only, no Anthropic-specific layout.** Rejected:
  the whole point is to emit the *provider's* required marker. We abstract the
  *author intent* (`cacheBreakpoint`) and specialize per provider in the provider
  module (where `resolve_alias` and wire-format mapping already live).
- **Automatic summarization when the window fills, on by default.** Rejected as a
  default (silently changes outputs); offered as explicit opt-in `.compact()` in
  Phase 3.

---

## 13. Implementation plan (phased)

**Phase 1 — Cache-aware request layout + accounting (no author API change)**
1. `src/providers/mod.rs`: add `CacheControl` + optional cache markers on
   `ContentBlock`/`LlmRequest` (additive; default `None`).
2. `src/providers/anthropic.rs`: emit `cache_control` on marked system/tools/head
   blocks (`send` body build, `:134-144`); structured-system form; ≤4 breakpoints;
   `extended-cache-ttl` header only for `1h`. Parse cache token fields into
   `AnthropicUsage` (`:59-63`).
3. `src/providers/openai.rs`: parse `cached_tokens`; no marker emission.
4. `src/runtime/host_core.rs`: `assemble_request` helper with auto-marking (§8.1,
   §8.3); call it from `execute_prompt_text`/`execute_prompt_response` and from the
   native loop's request build.
5. `src/runtime/native.rs`: route its per-turn request through `assemble_request`
   so the existing tool-use loop caches its system+tools+head immediately.
6. `src/runtime/call_log.rs`: extend `TokenUsage` with cache counts (skip-if-none).
   `src/runtime/cost.rs`: price creation/read separately.
7. `RunSpan::stream_record`: stamp `cache_creation`/`cache_read` span attributes.
8. `sdk/typescript/src/agent.ts`: add `cache?` to `PromptOptions`.

Tests (`--features rust-engine` where engine-specific; provider tests are
engine-agnostic):
- `providers`: a marked request serializes the `cache_control` JSON Anthropic
  expects; an unmarked one is byte-identical to today (no regression).
- `providers`: cache token fields parse from a sample Anthropic body and default
  to 0 when absent.
- `native`: the tool-use loop emits a cached system/tools layout on turns ≥2;
  recorded results are unchanged vs a no-cache run.
- `cost`: creation/read tokens price at the documented multiples.

**Phase 2 — `chidori.context` builder + full-prompt recording**
1. `crates/chidori-js/src/lib.rs`: `chidori.context` + builder methods over a
   `ContextTable` handle (§9.2); `.prompt()`/`.respond()` forward the flattened
   chain.
2. `src/runtime/host_core.rs`: accept a context/messages payload in the prompt
   args; record the **full assembled request digest** in `CallRecord.args`
   (close §4.4); `chidori.prompt(text)` becomes sugar over a one-segment context.
3. `src/runtime/context.rs` (or a new `context_value.rs`): the immutable Segment
   chain + memoized `prefix_digest` (§9.1).
4. `sdk/typescript/src/agent.ts`: `Context`/`Role`/`CacheTtl` types + `context()`.
5. Example: `examples/research-assistant/` — the §7 agent (corpus + question
   loop), run on the rust engine, streaming cache stats to tael.

Tests:
- builder immutability: `base.user("a")` and `base.user("b")` share `base`'s
  digest; neither mutates the other.
- `.prompt()` returns a context whose chain = parent + assistant turn; a 3-turn
  loop produces a stable prefix digest across turns.
- full-request digest is recorded in `args` and is identical on replay; replay
  pays zero tokens and reproduces the conversation.
- determinism: a cached-prefix run and a cache-disabled run produce **identical**
  call logs (only `token_usage` cache split differs).

**Phase 3 — Window management + local content-addressed cache**
1. `.compact(strategy)` — summarize old turns into one segment via a recorded
   `prompt` host call; the summary segment carries a fresh cache breakpoint.
2. A local content-addressed prompt cache keyed on the request digest (opt-in env
   flag), served on the live path *after* the replay short-circuit (§10),
   recorded as a normal `CallRecord`.
3. `estimateTokens()` / budget helpers for authors to decide when to compact.

Tests: compaction is deterministic and recorded; a second run issuing an
identical prompt is served from the local cache yet records an identical result;
window budgeting reports stable estimates.

---

## 14. Verification & rollout
- `cargo check -p chidori` = 0 errors **with and without** `rust-engine`; the
  QuickJS path is untouched by Phase 1 (provider-layer change, shared by both).
- `cargo test -p chidori --lib` green (provider serialization + cost tests).
- `cargo test -p chidori --features rust-engine --lib` green (context builder +
  full-prompt recording + determinism tests).
- Manual cost check: run an 8-question §7 agent against Anthropic with caching on
  vs `cache: false`; confirm `cache_read` tokens appear from turn 2 and
  `total_cost_usd` drops substantially with identical answers.
- Determinism guard: `chidori-js` Test262 baseline unchanged (all additive,
  inert unless an agent opts into `context`/caching).

## 15. Open questions
- **Segment storage:** VM-side (simplest, no Phase 3 cache) vs host-side
  `ContextTable` (enables host digesting + cross-run cache). §9.2.
- **Auto-mark default:** on for all prompts, or only once a context exceeds a
  token threshold where the write cost pays off?
- **TTL default:** `5m` (cheaper writes, fine for tight loops) vs `1h` (better for
  long human-paced or signal-driven runs, needs the beta header)?
- **Digest canonicalization:** exact normalization rules (whitespace, key order,
  float formatting) and how to version them.
- **`respond()` ergonomics:** how much of the native tool-use loop to expose vs
  keep internal — does an author-driven tool loop over `Context` replace
  `native.rs` eventually?

## 16. Future work
- A raw-`Message[]` escape hatch for power users who want full wire control.
- A typed prompt/segment schema registry (declare expected docs/segments; validate
  on build).
- Cross-run cache sharing across a fleet (shared content-addressed store), with the
  digest as the key.
- A tael "context" view: per-run prefix-reuse and cache-hit-rate dashboards keyed
  on segment digests.
- Per-provider cache strategy plugins (Gemini implicit caching, etc.) behind the
  same `cacheBreakpoint` author intent.
- QuickJS binding for `chidori.context` once the rust-engine surface settles.
