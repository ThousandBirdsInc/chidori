---
title: "FAQ"
description: "Common questions: Python support, Node, providers, how Chidori compares, data locality, securing the server, and what to do when things go wrong."
---

# FAQ

## Choosing Chidori

### Can I write agents in Python?

No — **agents are TypeScript**, executed by the runtime's embedded
pure-Rust JavaScript engine. The [Python SDK](../sdk/python/README.md) (and
the [TypeScript SDK](../sdk/typescript/README.md)) are **HTTP clients** for
driving a running `chidori serve` instance from your application: create
sessions, resume paused runs, deliver signals, fetch checkpoints, replay.
You don't need either SDK to write or run agents.

### Do I need Node.js?

No. The runtime is one Rust binary with an embedded JavaScript engine — no
Node, no Deno, no V8. Even npm packages install without Node:
`chidori add <pkg>` uses a content-addressed store with SHA-512
verification ([Package Management](./package-management.md)).

### Which model providers work?

Anthropic (`ANTHROPIC_API_KEY`), OpenAI (`OPENAI_API_KEY`, redirectable
via `OPENAI_BASE_URL`), any OpenAI-compatible endpoint — DeepSeek, Groq,
Ollama, vLLM, LiteLLM — via `CHIDORI_OPENAI_COMPAT_URL`, and a zero-setup
OpenRouter fallback via `chidori model-login`. All can coexist; requests
route by model name. Details:
[Providers & model selection](./host-api.md#providers--model-selection).

### How is this different from graph frameworks (LangGraph-style) or durable execution engines (Temporal-style)?

Chidori sits where the two meet. Versus **graph/DSL agent frameworks**:
agents are plain async TypeScript — native `if`/`for`/`try`, real imports —
not node graphs, and durability is the default rather than an add-on.
Versus **durable execution engines**: the LLM-native primitives (prompts,
tools, context, caching) are built in, replay is byte-identical with
**zero** model calls (not a re-execution that calls the model again), and
the runtime is one binary rather than a server + workers + queue. The
[comparison table in the README](../README.md#️-how-chidori-compares) puts
these side by side, and the
[AI SDK gap analysis](./ai-sdk-gap-analysis.md) is an honest
feature-by-feature comparison against Vercel's AI SDK.

### Where are my graphs and activity definitions?

There aren't any. Orchestration is ordinary control flow in your handler,
and every `await chidori.*` call is already a durable, replayable
safepoint — you don't annotate steps or define activities. If you're
reaching for a fan-out node, see
[Common Patterns](./patterns.md) for what to use instead
(`util.parallel`, `branch`, actors).

### I'm pointing an AI coding assistant at this. What should it read?

[`llm.txt`](../llm.txt) — a single, complete, LLM-optimized API reference,
designed to be read in full and sufficient to generate correct agents and
tools without crawling the source.

## Building agents

### What does a replay cost?

Nothing. Replay re-executes your TypeScript, but every host call — every
prompt, tool call, HTTP request — returns its recorded result from the call
log. No provider is contacted and no tokens are billed. `chidori verify`
enforces this posture (no provider configured, deny-all policy) and asserts
byte-identical output ([Replay & Resume](./replay.md)).

### What happens if I edit my agent after recording a run?

Resume is divergence-checked: it rejects incompatible source hashes rather
than silently replaying stale code. To deliberately replay a recorded run
against edited code, pass `--allow-source-change` — edit-and-resume, still
divergence-checked ([divergence rules](./replay.md)). In CI, `chidori verify` fails when
behavior drifts from the recording — which is exactly what makes recordings
useful as tests.

### Can agents use tools from MCP servers?

Yes — configure servers via `CHIDORI_MCP_*` and invoke their tools by name
with `chidori.tool(name, args)`, or pass the names in a prompt's `tools`
array alongside your own `defineTool` handles
([Host API](./host-api.md#tools-and-sub-agents)).

## Running in production

### What data leaves my machine?

By default, only what your agent explicitly does: LLM calls go to the
provider you configured, and `fetch`/tool calls go where you point them
(policy-gated). Everything else is local files next to your agent: run
journals under `.chidori/runs/`, memory under `.chidori/memory/`, server
sessions in `.chidori/sessions.sqlite3`. Telemetry is opt-in — OTLP export
only happens if you configure it
([Observing with Tael](./observing-with-tael.md)).

### How do I secure a served agent?

Set `CHIDORI_API_KEY` — bearer auth on everything except `GET /health`. It
accepts a comma-separated list for zero-downtime key rotation, and SDK
clients pass the same key (`{ apiKey }` / `api_key=`). Remember `serve` is
deny-by-default for powerful effects: granting them in production is an
explicit policy decision, not a flag you copy from a tutorial.
[Deployment](./deployment.md) has the full checklist, including recipes for
a plain VM, Fly.io, and Kubernetes.

### Do sessions survive a server restart?

Yes, by default: sessions persist in SQLite next to the agent
(`CHIDORI_DB_PATH` overrides; `:memory:` opts out), and `chidori serve`
re-arms every registered detached agent at boot
([Durable Storage](./durable-storage.md),
[Detached Agents](./detached-agents.md)).

## When things go wrong

### My prompt call fails with no provider configured

Set a provider key ([which ones work](#which-model-providers-work)), run
`chidori model-login` for the zero-setup fallback, or set
`CHIDORI_TEST_LLM_RESPONSE="(test reply)"` to smoke-test with a static
response and no network at all.

### My run fails in CI but works at my terminal

`chidori run` **asks** for approval before powerful effects (network, tool
calls, workspace writes) — and with no terminal to ask at, it **fails
closed**. Pass `--trusted` in scripts and CI, or configure an explicit
policy ([Running Modes](./running-modes.md)).

### My agent hangs waiting for input in a script

Under `chidori run`, `input()` reads stdin; at end-of-file it resolves to
the declared `default`, and **fails the run if there is no default** rather
than silently returning an empty string. Give interactive gates a
`default`, or run the agent under `chidori serve`, where `input()` pauses
the session for `POST /sessions/{id}/resume`.

### Resume refuses to run my edited agent

That's the divergence check doing its job — see
[What happens if I edit my agent after recording a run?](#what-happens-if-i-edit-my-agent-after-recording-a-run)

### A tool can't reach my local service

Tool `fetch` is SSRF-guarded: requests to localhost and private ranges are
refused even under `--trusted`. Allow specific hosts with
`CHIDORI_HTTP_ALLOW_HOSTS=127.0.0.1` (comma-separated hosts, IPs, or
CIDRs). Provider endpoints like `CHIDORI_OPENAI_COMPAT_URL` are not
affected ([Host API](./host-api.md#chidoritoolname-args)).
