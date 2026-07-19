---
title: "Overview"
description: "What Chidori is, and how these docs are organized."
---

# Chidori documentation

Chidori is the agent framework where every run is durable, replayable, and
resumable by default. You write agents as plain async TypeScript; every side
effect — every LLM call, tool call, and HTTP request — flows through the
runtime as a recorded **host call**, so any run can be checkpointed to disk,
replayed for byte-identical output with zero LLM calls, and resumed from any
pause, even in a new process after a crash.

## Where to start

Read these in order if you're new:

1. [Getting Started](./getting-started.md) — install, first agent, first replay.
2. [Core Concepts](./core-concepts.md) — host calls, the call log, safepoints.
3. [Replay & Resume](./replay.md) — record, replay, resume, divergence rules.

Then pick up the rest of **Using Chidori** as you need it: running modes,
signals, branching, actors, memory, storage, sandboxing, and deployment.

## How these docs are organized

- **Using Chidori** — guides for agent authors and operators.
- **Engineering Notes** — internal design records for contributors. Status
  headers inside each file are authoritative; several document retired or
  superseded work.
- **Usability Reviews** — six rounds of hands-on reviews that shaped the
  developer experience.
- **Posts** — longer-form writing about the ideas behind the framework.

## Other references

- [`llm.txt`](../llm.txt) — the complete API reference, optimized for LLMs
  generating agents.
- [TypeScript SDK](../sdk/typescript/README.md) and
  [Python SDK](../sdk/python/README.md) — HTTP clients with no native
  bindings.
- [Examples](../examples/) — runnable agents, from hello-world to
  multi-agent war rooms.
