---
title: "Memory"
description: "chidori.memory: a persistent cross-run key-value store — namespacing, on-disk anchoring, replay semantics."
---

# Memory — persistent key-value storage across runs

> `chidori.memory` is a small, namespaced, JSON key-value store that persists
> **across runs** — the place for what an agent learns in one session and
> should remember in the next. **Related:** [Core Concepts](./core-concepts.md),
> [Replay & Resume](./replay.md), [Value Checkpoints](./value-checkpoints.md),
> [Durable Storage](./durable-storage.md). API reference:
> [Host API](./host-api.md). Example:
> `examples/release-notes-concierge/agent.ts` (the house-style pattern).

## What this is

Runs are durable, but a run's journal belongs to *that run*. `chidori.memory`
is the store that outlives the run: JSON values keyed by string, anchored to
the agent on disk, readable by every subsequent session of the same agent.

```ts
await chidori.memory.set("house-style", learned);
const style = await chidori.memory.get("house-style");   // null when absent
const items = await chidori.memory.list({ prefix: "user_" });
await chidori.memory.delete("house-style");              // → true if it existed
await chidori.memory.clear();                             // empty this namespace
```

The canonical pattern (from `examples/release-notes-concierge`): at the end of
a session, distill what the human's feedback taught the agent and `set` it;
at the start of the next session, `get` it and fold it into the system prompt.

## API

Every method takes a trailing `options` object; the options are exactly
`namespace` (default `"default"`) and, on `list` only, `prefix`.

- `set(key, value, options?)` → `null`. `value` is any JSON-compatible value.
- `get(key, options?)` → the stored value, or `null` when the key is absent.
- `delete(key, options?)` → resolves to whether the key existed: `true` if it
  did, else `false`.
- `list(options?)` → an array of `{ key, value }` entries; with `prefix`, only
  entries whose keys start with that prefix.
- `clear(options?)` → `null`; empties the namespace (the file remains, as
  `{}`).

Namespaces isolate stores: `get("k", { namespace: "per-user" })` never sees
the default namespace's `k`. Each namespace is its own file,
`.chidori/memory/<namespace>.json`, anchored to the workspace root (below).
Namespace names are sanitized for the filesystem — any character outside
`[A-Za-z0-9_-]` becomes `_`.

## Where it lives on disk

Each namespace is one pretty-printed JSON object at:

```
<root>/.chidori/memory/<namespace>.json
```

`<root>` resolves in precedence order:

1. **`CHIDORI_MEMORY_DIR`** — explicit override, wins outright.
2. **The run's workspace root** — the agent file's directory under
   `run`/`resume`/`serve` and for detached agents, or
   `CHIDORI_WORKSPACE_ROOT` when set.
3. **The process cwd** — last-resort fallback for bare embeddings with no
   known root. (Unlike `chidori.workspace`, memory never hard-fails on a
   missing root.)

So memory is **anchored to the agent, like runs and workspace files**:
running the same agent from a different working directory sees the same
store, and two different agent directories are two independent stores.

## Record vs. replay

Every memory action is a journaled `memory` host call. Live, the action
executes against the JSON file (a whole-file load → mutate → save for writes)
and its result is journaled; on replay, the journaled result is returned and
the store is **not touched** — a replayed `get` returns the value as it was at
recording time even if the file has changed since, and a replayed `set` does
not re-write the file ([Replay & Resume](./replay.md)). Only live continuation
past the recorded frontier hits the store again.

Memory calls are never policy-gated: they behave the same under the
`supervised` profile (the bare `chidori run` default), the `untrusted`
profile, and `--trusted` — see the [CLI reference](./cli.md).

## Concurrency

Writes are whole-file read-modify-write with **no cross-process locking**.
Within a single run, host calls execute one at a time, so an agent's own
actions never interleave. But concurrent writers sharing one store — parallel
actors, a detached-agent fleet, or two processes anchored at the same root —
can interleave load/save and lose updates (last write wins at file
granularity). Keep memory for low-contention state (preferences, distilled
lessons, per-user notes under distinct keys or namespaces); use
[signals/mailboxes](./signals.md) or [actor messages](./actors.md) for
cross-agent coordination.

## Memory vs. its neighbors

This table is the canonical boundary map for Chidori's state surfaces — the
other state pages link here rather than restating it.

| Store | Scope | For |
|---|---|---|
| `chidori.memory` | The agent, across all runs | What the agent has learned; small JSON state |
| `chidori.workspace` | The project directory, across runs | Deliverable files (documents, code) — policy-gated |
| `chidori.step` | One run's journal | Memoizing expensive pure compute within a run ([Value Checkpoints](./value-checkpoints.md)) |
| Run journal | One run | Every journaled host call; replay/resume ([Replay & Resume](./replay.md)) |
| Run store | The `.chidori/runs/` tree, plus an optional durable mirror | Where the journal's bytes physically live — backends, hydration, machine-loss survival ([Durable Storage](./durable-storage.md)) |
