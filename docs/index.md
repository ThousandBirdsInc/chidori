---
title: "Overview"
description: "What Chidori is, the one mechanism behind it, and how these docs are organized."
---

# Chidori documentation

> **Docs track the next release.** These pages describe the code on the
> default branch, which may include features not yet in the release you
> installed. When running a tagged release, read the docs *at that tag*
> (`https://github.com/ThousandBirdsInc/chidori/tree/vX.Y.Z/docs`) and trust
> `chidori <command> --help` over any page. Features that shipped after the
> latest release are marked with a "since" note where the skew bites.

Chidori is the agent framework where every run is durable, replayable, and
resumable by default. You write agents as plain async TypeScript — real
`if`/`for`/`try`, real imports, no graph DSL — and run them on a single Rust
binary with no Node, no Deno, and no V8. (The TypeScript and Python *SDKs*
are optional HTTP clients for driving a served runtime from your app —
agents themselves are always TypeScript. [More FAQ →](./faq.md))

## The one mechanism

Everything Chidori does follows from a single boundary: an agent never
touches the world directly. Every side effect — every LLM call, tool call,
HTTP request, file write, human question — flows through the runtime as a
recorded **host call**. Because the runtime sees and records everything, it
can log it, cache it, replay it, pause on it, and resume from it:

| Because every effect is recorded… | …you get |
|---|---|
| The journal is a deterministic record | **Replay any run with zero LLM calls** — byte-identical output, no tokens billed ([Replay & Resume](./replay.md)) |
| Runs persist at every host safepoint | **Crash recovery** — kill the process mid-run, resume in a new one ([Durable Storage](./durable-storage.md)) |
| A pause is just a host call with no answer yet | **Humans in the loop without a live process** — suspend to disk, resume days later ([Signals](./signals.md)) |
| A recording fully specifies behavior | **Recorded runs as CI tests** — `chidori verify` asserts zero drift for $0 ([Replay & Resume](./replay.md#replay-as-test)) |
| Code changes are events too | **Implementation history** — a git-like chain of every source version that ran, diffable, anchored to the journal ([Source History](./source-history.md)) |

## Where to start

1. [Getting Started](./getting-started.md) — install the binary, scaffold
   and run your first agent.
2. [Your First Agent](./your-first-agent.md) — a fifteen-minute tutorial:
   write an agent, pause it for approval, replay it for $0, check it into
   CI.
3. [Core Concepts](./core-concepts.md) — the host-function surface and the
   mental model.
4. [Common Patterns](./patterns.md) — which primitive fits which job.

Then pick up the rest of **Using Chidori** as you need it, and keep the
[Host API Reference](./host-api.md) and [CLI Reference](./cli.md) at hand.
Evaluating against other frameworks? Start with the
[FAQ](./faq.md#choosing-chidori) and the
[comparison table](../README.md#️-how-chidori-compares).

## How these docs are organized

- **Using Chidori** — guides for agent authors and operators, roughly in
  reading order.
- **Reference** — the complete host API and CLI, for lookup.
- **Internals** — how the engine and runtime work, for the curious and for
  contributors.
- **Posts** — longer-form writing about the ideas behind the framework.

Deeper engineering notes and the historical usability reviews live in the
repository under [`docs/`](./README.md) rather than on this site.

## Other references

- [`llm.txt`](../llm.txt) — the complete API reference, optimized for LLMs
  generating agents.
- [TypeScript SDK](../sdk/typescript/README.md) and
  [Python SDK](../sdk/python/README.md) — HTTP clients with no native
  bindings.
- [Examples](../examples/) — runnable agents, from hello-world to
  multi-agent war rooms.
- [Discord](https://discord.gg/CJwKsPSgew) — questions, ideas,
  contributions.
