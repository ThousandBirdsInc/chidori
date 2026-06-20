# Chidori vs. Vercel AI SDK Gap Analysis

This note compares Chidori's current functionality against the Vercel AI SDK
documentation reviewed on 2026-06-17.

The originally requested URL, `https://ai-sdk.dev/v7/docs`, was not reachable
during the review. The live documentation available at `https://ai-sdk.dev/docs`
identified `v6` as the latest docs surface. The comparison below is therefore
against the current public AI SDK docs that were available at review time.

## Summary

Chidori and the Vercel AI SDK overlap on LLM calls, streaming, tool calling,
agent loops, MCP tools, and multi-turn context. They are not direct substitutes.

AI SDK is broader as an application-facing TypeScript toolkit: many providers,
framework UI hooks, structured output helpers, embeddings, reranking, media
generation, provider middleware, and a large frontend/server integration
surface.

Chidori is deeper as a durable agent runtime: host-call capture, deterministic
replay, durable pause/resume, human input, multiplayer signals, branch stores,
recorded HTTP/filesystem effects, runtime policy, and replayable checkpoints are
runtime semantics rather than library conventions.

## Current Overlap

| Capability | Chidori | Vercel AI SDK |
| --- | --- | --- |
| Text generation | `chidori.prompt(...)`, `Context.prompt()` | `generateText` |
| Streaming text | CLI and SSE prompt events with `prompt_type` labels | `streamText`, UI stream protocols |
| Tool calling | TypeScript tools, MCP-backed tools, provider tool calls | `tool(...)`, model tool calling, multi-step tool execution |
| Agent loop | Plain async TypeScript with `conversation()`, `Context.respond()`, examples/templates | `ToolLoopAgent`, `generateText`/`streamText` loops |
| Multi-turn context | `chidori.context()`, `conversation()`, explicit compaction | Message arrays, agent context management, workflow patterns |
| MCP | Stdio MCP subset: initialize, `tools/list`, `tools/call` | Documented MCP integration surface |
| Subagents | `chidori.callAgent(...)`, branch sub-runs | Subagents modeled as tools around `ToolLoopAgent` instances |
| Testing | Recorded call-log replay, static test provider | Mock providers and stream simulation helpers |
| Telemetry | OTEL trace emission and call records | OpenTelemetry integration hooks and lifecycle callbacks |

## Chidori Advantages

These are capabilities where Chidori has a stronger or more opinionated runtime
contract than AI SDK.

### Durable Execution By Default

Every side effect flows through the host boundary and is recorded. Replay uses
the call log to return recorded host-call results with zero LLM calls. This
covers prompts, tools, HTTP, input pauses, signals, memory, templates, and
checkpoints.

AI SDK offers deterministic unit testing through mock providers, but it does not
provide a built-in durable execution journal that can replay real production
runs byte-for-byte.

### Crash And Human Pause Recovery

Chidori sessions can pause on `chidori.input(...)`, policy approval, or
`chidori.signal(...)`, persist state, and resume later through the session API.
This is a runtime-level primitive for long-running human-gated work.

AI SDK supports application-managed persistence and resumable chat streams, but
the application owns durable orchestration semantics.

### Recorded External Effects

Chidori captures base effects such as `fetch`, `node:http`, VFS-backed
workspace operations, crypto, timers, and host tools under policy and replay.
Requests made inside dependencies inherit the same capture and approval model.

AI SDK focuses on model/tool abstractions. It does not make arbitrary HTTP or
filesystem effects replayable by default.

### Branching And Replayable Exploration

`chidori.branch(...)` forks a run into variant sub-runs from an anchored state,
persists branch stores, and can resume or rerun branches independently. Replaying
the parent returns the recorded branch outcomes without re-running the variants.

AI SDK supports workflow patterns and subagents, but does not expose an
equivalent durable branch store or replayable fan-out primitive.

### Runtime Policy And Capability Confinement

Chidori has explicit policy profiles, deterministic `Date`/`Math.random`
policies, import policy, captured networking policy, and workspace mutation
gates. These policies are part of the runtime.

AI SDK assumes the host JavaScript runtime and application enforce these
constraints.

## Chidori Gaps Against AI SDK

These are the main gaps to close if Chidori wants closer functional parity with
the Vercel AI SDK framework.

### 1. Provider Breadth

Current Chidori support is intentionally narrow:

- Native Anthropic provider.
- Native OpenAI / OpenAI-compatible provider.
- LiteLLM-compatible routing as an escape hatch.
- MCP tools for tool integration, not model-provider expansion.

AI SDK has a much broader provider ecosystem, including official packages for
OpenAI, Anthropic, Google, Google Vertex, Azure, Amazon Bedrock, Mistral, Groq,
Cohere, DeepSeek, Cerebras, xAI, Together.ai, Fireworks, DeepInfra, Perplexity,
ElevenLabs, Deepgram, AssemblyAI, and many community providers.

Gap:

- No first-class provider package interface.
- No published provider specification equivalent.
- No plugin-like provider ecosystem.
- Limited native model capability matrix.

### 2. Structured Output

Chidori exposes `format: "json"` and structured tool-call responses, but the
repo explicitly documents no JSON-mode or structured-output plumbing beyond
tool calls.

AI SDK supports structured output through `Output.object(...)`,
`Output.array(...)`, schemas, validation, and streaming partial structured
outputs.

Gap:

- No first-class schema-validated `generateObject`/`Output.object` equivalent.
- No Zod/Valibot/JSON Schema validation pipeline for model outputs.
- No partial structured output stream equivalent to AI SDK's streamed object
  generation.
- No typed structured-output authoring surface in the TypeScript SDK.

### 3. Embeddings And Reranking

Chidori does not currently expose embeddings or reranking as model primitives.
The local roadmap states that embeddings and additional modalities are not yet
implemented.

AI SDK documents embeddings and reranking as first-class core capabilities.

Gap:

- No `embed` / `embedMany` equivalent.
- No rerank API.
- No embedding model provider abstraction.
- Memory/vector storage exists at the host API level, but model-side embedding
  generation is not a first-class API.

### 4. Media Modalities

Chidori currently focuses on text, tool calls, captured effects, and durable
agent execution.

AI SDK documents image generation, transcription, speech, and video generation
surfaces where providers support them.

Gap:

- No first-class image generation API.
- No audio transcription API.
- No speech synthesis API.
- No video generation API.
- No media artifact streaming or provider capability abstraction.

### 5. Frontend UI Toolkit

Chidori exposes CLI streaming, session APIs, SSE events, and TypeScript/Python
HTTP clients.

AI SDK UI provides framework-specific hooks such as `useChat`,
`useCompletion`, and `useObject` for React, Vue, Svelte, Angular, and SolidJS
usage.

Gap:

- No React/Vue/Svelte/Angular/Solid client hooks.
- No drop-in chat state manager.
- No UI message abstraction equivalent to AI SDK UI's `UIMessage` stream model.
- No generative UI helper layer.
- No client-side structured object hook equivalent to `useObject`.

### 6. Stream Protocol Compatibility

Chidori streams host calls and prompt lifecycle events:

- `call`
- `prompt_start`
- `prompt_delta`
- `prompt_end`
- `paused`
- `done`

AI SDK has broader stream helpers and UI-oriented stream protocols for chat,
custom data, metadata, tool use, resumable streams, and UI consumption.

Gap:

- No compatibility layer for AI SDK UI stream protocols.
- No documented bridge from Chidori SSE events to AI SDK UI hooks.
- No standardized typed client event model beyond the current TypeScript SDK
  parser.

### 7. Provider Options And Middleware

Chidori supports basic prompt options such as model, temperature, max tokens,
tools, stream labels, and cache posture.

AI SDK has a richer provider options and language-model middleware surface,
including provider-specific settings, model wrapping, guardrails, RAG-style
middleware, and lifecycle hooks.

Gap:

- No language model middleware API.
- Limited provider-specific option pass-through.
- No reusable model wrapper stack.
- No built-in guardrail/middleware pattern matching AI SDK's integration model.

### 8. Memory Ecosystem

Chidori has a durable `chidori.memory(...)` host API and replay-aware memory
operations.

AI SDK documents provider-defined memory tools and integrations with memory
providers such as Letta, Mem0, Supermemory, and Hindsight.

Gap:

- No packaged memory-provider integrations.
- No provider-defined memory tool mapping.
- No semantic memory provider interface.
- No standard memory injection or retrieval middleware.

### 9. Testing Surface

Chidori's strongest testing mechanism is replaying real recorded runs. That is
valuable for integration and regression testing.

AI SDK has dedicated mock language/embedding models, mock value generators, and
stream simulation helpers designed for unit tests.

Gap:

- No TypeScript SDK test helper package comparable to `ai/test`.
- No mock provider API exposed to agent authors beyond environment-level static
  provider behavior.
- No stream simulation utilities for frontend/client tests.
- TypeScript SDK tests are still listed as product follow-through work.

### 10. Package And Ecosystem Fit

Chidori agents run inside the embedded pure-Rust JavaScript engine. This gives
Chidori control over determinism and captured effects, but it also means normal
Node/npm compatibility is intentionally limited.

AI SDK runs in standard JavaScript application environments and can compose with
the broader npm ecosystem.

Gap:

- No general npm package compatibility inside agents.
- Limited `node:` allowlist.
- No standard bundler/runtime integration story for arbitrary AI SDK ecosystem
  packages inside a Chidori agent.
- No adapter that lets Chidori agents call AI SDK model providers directly
  inside the durable runtime.

## Positioning Implication

Chidori should not try to compete with AI SDK by matching every provider,
frontend framework hook, media endpoint, and middleware feature immediately.
The strongest current position is complementary:

- AI SDK is the application/model integration toolkit.
- Chidori is the durable execution substrate for expensive, long-running,
  replay-sensitive, human-gated, or branch-heavy agents.

The highest-leverage parity work would be the parts that preserve Chidori's
runtime differentiation while reducing adoption friction:

1. Structured output with schema validation.
2. A provider abstraction or adapter story compatible with the AI SDK ecosystem.
3. Frontend stream adapters for AI SDK UI-style chat clients.
4. Embeddings/reranking as durable host calls.
5. A TypeScript test helper package that combines AI SDK-style mocks with
   Chidori replay fixtures.

## References

Local references:

- `README.md` - Chidori positioning and durable host-call model.
- `llm.txt` - TypeScript agent and host API reference.
- `docs/core-concepts.md` - Host functions, context, prompt caching, and
  conversational agents.
- `docs/replay.md` - Replay model and SDK checkpoint flow.
- `docs/context-management.md` - Prompt caching, context composition, and
  compaction.
- `docs/branching-execution.md` - Branching design and branch stores.
- `docs/signals.md` - Multiplayer signal model.
- `docs/TODO.md` - Current product gaps, including providers/modalities and
  TypeScript SDK tests.

External references reviewed:

- `https://ai-sdk.dev/docs/introduction`
- `https://ai-sdk.dev/docs/ai-sdk-core/overview`
- `https://ai-sdk.dev/docs/ai-sdk-core/generating-structured-data`
- `https://ai-sdk.dev/docs/ai-sdk-core/tool-calling`
- `https://ai-sdk.dev/docs/ai-sdk-core/model-context-protocol`
- `https://ai-sdk.dev/docs/ai-sdk-core/testing`
- `https://ai-sdk.dev/docs/ai-sdk-core/telemetry`
- `https://ai-sdk.dev/docs/ai-sdk-ui/overview`
- `https://ai-sdk.dev/docs/agents/overview`
- `https://ai-sdk.dev/docs/agents/memory`
- `https://ai-sdk.dev/docs/agents/subagents`
