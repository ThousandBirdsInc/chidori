---
title: "Memory"
description: "chidori.memory: a persistent cross-run key-value store \u2014 namespacing, on-disk anchoring, replay semantics."
---

# Memory — persistent key-value storage across runs

> `chidori.memory` is a small, namespaced, JSON key-value store that persists
> **across runs** — the place for what an agent learns in one session and
> should remember in the next. **Related:** `docs/core-concepts.md`,
> `docs/replay.md`, `docs/value-checkpoints.md`. API reference: `llm.txt`.
> Implementation: `crates/chidori/src/runtime/memory.rs`,
> `crates/chidori/src/runtime/typescript/bindings.rs` (`memory_base`).
> Example: `examples/release-notes-concierge/agent.ts` (the house-style
> pattern).

## 1. What this is

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

## 2. API

Every method takes a trailing `options` object accepting `namespace`
(default `"default"`); `list` also accepts `prefix`.

- `set(key, value, options?)` → `null`. `value` is any JSON-compatible value.
- `get(key, options?)` → the stored value, or `null` when the key is absent.
- `delete(key, options?)` → `true` if the key existed, else `false`.
- `list(options?)` → `[{ key, value }, …]`; with `prefix`, only keys starting
  with that prefix.
- `clear(options?)` → `null`; empties the namespace (the file remains, as
  `{}`).

Namespaces isolate stores: `get("k", { namespace: "per-user" })` never sees
the default namespace's `k`. Namespace names are sanitized for the filesystem
— any character outside `[A-Za-z0-9_-]` becomes `_`.

## 3. Where it lives on disk

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

## 4. Record vs. replay

Every memory action is a durable `memory` host call in the run's journal:

- **Live**, the action executes against the JSON file (a whole-file
  load → mutate → save for writes) and its result is recorded.
- **On replay** (`chidori resume`, `chidori verify`, server replay), the
  recorded result is returned and the store is **not touched** — no file is
  read or written. A replayed `get` returns the value as it was at recording
  time even if the file has changed since; a replayed `set` does not re-write
  the file. Only live continuation past the recorded frontier (crash
  recovery's new work) hits the store again.

Memory is a *pure* effect for policy purposes: it is never policy-gated, so
it works identically under `--trusted`, ask-mode, and the `untrusted`
profile.

## 5. Concurrency

Writes are whole-file read-modify-write with **no cross-process locking**.
Within a single run, host calls execute one at a time, so an agent's own
actions never interleave. But concurrent writers sharing one store — parallel
actors, a detached-agent fleet, or two processes anchored at the same root —
can interleave load/save and lose updates (last write wins at file
granularity). Keep memory for low-contention state (preferences, distilled
lessons, per-user notes under distinct keys or namespaces); use
[signals/mailboxes](./signals.md) or [actor messages](./actors.md) for
cross-agent coordination.

## 6. Memory vs. its neighbors

| Store | Scope | For |
|---|---|---|
| `chidori.memory` | The agent, across all runs | What the agent has learned; small JSON state |
| `chidori.workspace` | The project directory, across runs | Deliverable files (documents, code) — policy-gated |
| `chidori.step` | One run's journal | Memoizing expensive pure compute within a run ([value checkpoints](./value-checkpoints.md)) |
| Run journal | One run | Every recorded effect; replay/resume ([replay](./replay.md)) |
