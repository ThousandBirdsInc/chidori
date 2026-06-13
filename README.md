<p align="center">
  <img src=".github/chidori-banner.svg" alt="Chidori — checkpoint · replay · resume: durable TypeScript agents on a Rust core" width="800" />
</p>

<h1 align="center">Chidori</h1>

<p align="center">
  <b>An agent framework where TypeScript agents checkpoint, replay, and resume by default.</b>
</p>

<p align="center">
  Agents are plain async TypeScript; every side effect flows through the runtime as a recorded
  <b>host call</b>. So a finished run can be saved to disk, replayed for identical output with
  <b>zero LLM calls</b>, and resumed from any pause — all on one Rust binary with an embedded
  pure-Rust JavaScript engine and TypeScript + Python SDKs.
</p>

<p align="center">
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="GitHub Last Commit" src="https://img.shields.io/github/last-commit/ThousandBirdsInc/chidori" /></a>
<a href="https://crates.io/crates/chidori"><img alt="crates.io version" src="https://img.shields.io/crates/v/chidori" /></a>
<a href="https://pypi.org/project/chidori/"><img alt="PyPI version" src="https://img.shields.io/pypi/v/chidori" /></a>
<a href="https://www.npmjs.com/package/@1kbirds/chidori"><img alt="npm version" src="https://img.shields.io/npm/v/%401kbirds%2Fchidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/blob/main/LICENSE"><img alt="License Apache-2.0" src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" /></a>
</p>

<p align="center">
  <a href="#️-quick-start"><b>⚡️ Quick Start</b></a> ·
  <a href="#-what-you-can-build"><b>🧰 What You Can Build</b></a> ·
  <a href="#-documentation"><b>📚 Documentation</b></a> ·
  <a href="DESIGN.md"><b>📐 Design</b></a> ·
  <a href="https://discord.gg/CJwKsPSgew"><b>💬 Discord</b></a>
</p>

> **About v3.** Chidori began as a reactive runtime exploring how to build durable, debuggable agents. v3 is a ground-up rewrite that distills those ideas into a smaller, sharper core: a single Rust binary, TypeScript agent authoring, and replay as the foundation for tests, debugging, resume, and human-in-the-loop workflows. Earlier versions of Chidori live in the git history and on prior tags.

## 📖 About

Chidori is an agent framework where every side effect an agent performs flows
through the runtime as a recorded **host call**. That single boundary is what
makes runs durable: because the runtime sees everything, it can log, cache,
replay, pause, and resume any run — deterministically, with zero LLM calls on
replay.

- **Agents are TypeScript.** Native async control flow, type-safe inputs, tool calls, and imports with full editor tooling — no template DSL.
- **Durable execution.** Every side effect goes through a host function the runtime can log, cache, and replay, so a run survives crashes and restarts and resumes exactly where it left off.
- **Deterministic checkpoint & replay.** Save a session's call log to disk and replay it later for identical output with zero LLM calls — the foundation for tests, debugging, and resume.
- **Human-in-the-loop.** Pause an agent for approval or input, persist the checkpoint, and resume from the call log later — even in a new process.
- **Event-driven agents.** Agents can run as HTTP servers that react to webhooks and other events.
- **Rust core, TypeScript and Python SDKs.** The runtime is a single binary; SDKs talk to it over HTTP with no native bindings.

The whole model fits in one picture — agents never touch the world directly, so the runtime
sees (and records) everything:

<p align="center">
  <img src=".github/host-call-loop.svg" alt="Animation: a TypeScript agent calls host functions on the Chidori runtime; the runtime performs each side effect against the world (LLMs, HTTP, tools) and records it in the call log" width="860" />
</p>

## ⚡️ Quick Start

### 1. Write an agent

```ts
// agents/summarizer.ts
import type { Chidori } from "chidori";

export async function agent(input: { document: string }, chidori: Chidori) {
  const summary = await chidori.prompt(
    "Summarize in 3 bullets:\n" + input.document,
    { type: "summary" },
  );
  const actionItems = await chidori.prompt(
    "Extract action items:\n" + summary,
    { type: "actions" },
  );
  return { summary, actionItems };
}
```

### 2. Run it

```bash
# Set up LLM provider (uses LiteLLM in this example)
export LITELLM_API_URL=http://localhost:4401/v1
export LITELLM_API_KEY=sk-litellm-master-key

# Or use providers directly
# export ANTHROPIC_API_KEY=sk-ant-...
# export OPENAI_API_KEY=sk-...

cargo build
./target/debug/chidori run agents/summarizer.ts \
  --input document="Rust is a systems programming language..."
```

### 3. Try the example agents

```bash
# Interactive example picker
./target/debug/chidori demo

# Minimal agent — no LLM calls needed
./target/debug/chidori run examples/agents/hello.ts --input name=Colton

# Local TypeScript tool — no LLM calls needed
./target/debug/chidori run examples/agents/tool_use.ts \
  --input query=chidori --tools examples/tools
```

For a guided walkthrough — inspecting a run, the demo picker, and the
human-in-the-loop pause/resume loop — see
[**Getting started & demos**](./docs/getting-started.md).

## 🧰 What You Can Build

- **Durable, resumable agents** — runs survive crashes and restarts and resume exactly where they paused. See [How replay works](./docs/replay.md).
- **Deterministic tests & cheap debugging** — check in a checkpoint and replay it with zero LLM calls to assert behavior or step through a failure locally.
- **Human-in-the-loop workflows** — pause for approval or input with `chidori.input(...)`, persist the checkpoint, resume later in a new process.
- **Multiplayer & event-driven agents** — react to webhooks, or pause on named [signals](./docs/signals.md) until a human or another agent delivers a payload.
- **Branching exploration** — fork a run into per-strategy sub-runs and compare every outcome ([branching execution](./docs/branching-execution.md)).
- **Cost-efficient prompting** — structural [prompt caching](./docs/context-management.md) re-bills stable prefixes at the cached rate, and replay pays zero tokens.

Agents reach all of this through a fixed set of host functions on the `chidori`
object — see [**Core concepts**](./docs/core-concepts.md) for the full list and
[`llm.txt`](./llm.txt) for the complete API reference.

## 📚 Documentation

| Topic | What's there |
|---|---|
| [Getting started & demos](./docs/getting-started.md) | Demo picker, inspecting a run, human-in-the-loop walkthrough, example agents |
| [Core concepts & host API](./docs/core-concepts.md) | Host function reference, streaming prompt progress, prompt caching |
| [Running modes](./docs/running-modes.md) | One-shot CLI, HTTP server + session API, event-driven agents |
| [How replay works](./docs/replay.md) | Record/checkpoint/replay model and SDK replay |
| [Architecture & project structure](./docs/architecture.md) | High-level component map and repository layout |
| [JavaScript conformance (Test262)](./docs/conformance.md) | Running the pure-Rust JS engine against the TC39 suite |
| [Sandbox & security model](./docs/sandbox-model.md) | Deny-by-default policy, capability injection, resource limits |
| [Context management & caching](./docs/context-management.md) | Immutable contexts, compaction, cost accounting |
| [Signals & multiplayer](./docs/signals.md) | Named listen points, mailboxes, fan-in |
| [Design rationale](./DESIGN.md) · [Roadmap](./TODO.md) | Full design notes and implementation roadmap |
| [Python SDK](./sdk/python/README.md) · [TypeScript SDK](./sdk/typescript/README.md) | HTTP clients with no native bindings |

## 💬 Community

Questions, ideas, or want to contribute? Join us on
[Discord](https://discord.gg/CJwKsPSgew).

## License

Apache-2.0 — see [LICENSE](./LICENSE).
