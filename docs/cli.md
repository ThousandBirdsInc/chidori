---
title: "CLI Reference"
description: "Every chidori subcommand — run, serve, resume, verify, trace, chat, branches, packages — and the approval postures that govern them."
---

# CLI reference

One binary, no runtime dependencies. This page lists every subcommand with
its job and the doc that covers it in depth.

## Scaffolding and exploring

| Command | What it does |
|---|---|
| `chidori init [dir] --template docs\|chat\|worker` | Scaffold a starter project (agent + README; the `docs` template bundles a docs corpus to chat with). Omit `--template` to pick interactively. |
| `chidori demo` | Interactive picker over the runnable examples, including no-key demos. |
| `chidori check <agent.ts>` | Validate an agent file without running it. |
| `chidori model-login` | Zero-setup OpenRouter fallback — prompts work without configuring a provider key. |

## Running

| Command | What it does |
|---|---|
| `chidori run <agent.ts> --input key=value` | One-shot run. `--input` takes `key=value` pairs or a JSON object; `--model` sets the run's default model; `--stream` emits NDJSON progress events; `--trace` prints the call log as it grows. |
| `chidori chat [agent.ts]` | Interactive multi-turn REPL backed by `conversation()`. With no file, chats with the model directly (`--system`, `--model`); with a conversational agent file, chats through it. Each turn is a durable host call; prior turns replay for free, so only the newest message reaches the provider. `--resume <session_id>` reprints the transcript for $0 and continues the same session. |
| `chidori serve <agent.ts> --port 8080` | HTTP session server: sessions, pause/resume, signals, SSE streaming ([Running Modes](./running-modes.md), [Deployment](./deployment.md)). |
| `chidori serve --port 8080` | Fleet-only server (no agent file): hosts detached agents; sessions must name an agent. |

## Replay, resume, and testing

| Command | What it does |
|---|---|
| `chidori resume <agent.ts> <run_id>` | Replay a recorded run byte-for-byte with zero model calls; the run's recorded model applies automatically (`--model` overrides). A crashed run replays to the frontier of its log and continues live. |
| `chidori resume … --trusted` | Crash recovery of a trusted tool-using run — same posture flags as `run`; continuation journals into the same run dir. |
| `chidori resume … --allow-source-change` | Edit-and-resume: replay against edited code, divergence-checked ([divergence rules](./replay.md)). |
| `chidori verify <agent.ts> <run_id>` | Checkpoint-as-test: replay with **no provider** and a **deny-all policy**; asserts completion with byte-identical output. Exit 0 = pass. Built for CI. Journaled workspace writes do re-materialize on disk (same bytes, fresh mtime). |
| `chidori trace <run_id>` | Print a run's call log — every prompt, tool call, and effect, with token counts and cost (including prompt-cache read/write totals). |
| `chidori stats` | Usage and cost totals, including prompt-cache read/write tokens. |
| `chidori snapshot <run_id>` | Print `runtime.snapshot.json` metadata (never raw VM snapshot bytes). |

Run journals live under `.chidori/runs/<run_id>/` next to the agent file;
pass `--dir <path>` when tracing from elsewhere.

## Branches

| Command | What it does |
|---|---|
| `chidori branches <run-id>` | List a run's persisted branch stores. |
| `chidori branch-resume <run-id> <branch-id> --value "…"` | Answer a paused `input()` inside a branch. |
| `chidori branch-rerun <run-id> <branch-id>` | Re-run a branch's (possibly edited) `source.ts` from its fork-time anchor. |

See [Branching Execution](./branching-execution.md).

## Packages

| Command | What it does |
|---|---|
| `chidori add <pkg>` | Add an npm dependency — content-addressed store, SHA-512 verification, JSONL lockfile, no Node. |
| `chidori install` | Install dependencies from the lockfile. |
| `chidori remove <pkg>` | Remove a dependency. |

See [Package Management](./package-management.md).

## Approval postures

The posture decides what happens when an agent reaches a *powerful* effect —
network access, `chidori.tool` calls, workspace mutations. LLM prompts and
pure compute are never gated.

| Context | Default behavior |
|---|---|
| `chidori run` (interactive) | **Ask**: y/a/N approval at the terminal per gated effect (`a` allows that target for the rest of the run). |
| `chidori run` (no terminal: scripts, CI) | **Fail closed** — pass `--trusted` or configure a policy. |
| `chidori serve` | **Deny by default** (`untrusted` profile) unless `--trusted` or explicit `CHIDORI_POLICY*` configuration; read-only workspace introspection stays allowed. `--untrusted` forces the deny profile over any env configuration. |

Full model: [Running Modes](./running-modes.md) and the
[Sandbox Model](./sandbox-model.md).
