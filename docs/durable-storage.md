# Durable storage: the run store

Chidori's durability model is the deterministic effect journal
(`docs/replay.md`): every host effect is recorded, and recovery is replaying
the journal. That gives *logical* durability — replayability. This document
covers the layer underneath: **where the journal's bytes live**, what
guarantees each backend gives, and how a run survives losing the machine.

## The layering

```
  agent code (plain TypeScript)
      │  chidori.* host calls
  effect journal (CallRecords)          ← the replay model; unchanged
      │  RunStore trait
  ┌───┴─────────────────────────────┐
  │ FsRunStore (always, primary)    │   .chidori/runs/<run_id>/...
  │ + durable mirror (optional)     │   SQLite file · HTTP relay (one
  │   via TeeRunStore               │   Cloudflare Durable Object per run)
  └─────────────────────────────────┘
```

Everything a run persists — the journal, the snapshot manifest and blob, the
pending host operation, the host-promise table, the signal inbox, branch
stores — flows through one `RunStore` handle
(`crates/chidori/src/runtime/store.rs`). The filesystem layout is always the
primary and is byte-identical to what the framework has always written, so
every existing consumer (the viewer, `chidori trace`, external tooling) keeps
working. A configured durable mirror receives a copy of every write.

This is the same local-fast / remote-durable split Cloudflare built for
Durable Objects' storage (local disk for reads, replicated relay for
durability) — applied to the journal.

## The journal on disk: append-only + checkpoint

Two artifacts per run:

* **`records.jsonl`** — append-only, one JSON `CallRecord` per line. Every
  `record_call` appends O(1) bytes (previously each record rewrote the whole
  `checkpoint.json`, O(history) per call).
* **`checkpoint.json`** — the full-log artifact, rewritten at host-operation
  safepoints (and branch merges). Doubles as the compaction of the
  append-only file: every safepoint rewrite truncates `records.jsonl` to
  match, so neither file grows past one run's history.

Loading unions the two: the last checkpoint wins per-seq, and any tail
records a crash stranded after the last safepoint are recovered from
`records.jsonl`.

## Backends

Selected by `CHIDORI_RUN_STORE`:

| Value | Backend |
|---|---|
| unset / `fs` | Filesystem only (the default — exactly the pre-existing behavior) |
| `sqlite` | Mirror to a shared SQLite database (`CHIDORI_RUN_DB`, default `<run_base>/runs.sqlite3`). One row per record — not the session store's blob-per-session shortcut. |
| `http(s)://…` | Mirror to a remote relay speaking the run-store REST protocol. The reference deployment is one **Cloudflare Durable Object per run** (`integrations/cloudflare-durable-objects/`), which gives every acknowledged write cross-datacenter replication and 30-day point-in-time recovery. `CHIDORI_RUN_STORE_TOKEN` adds bearer auth. |

## Write-error policy: `CHIDORI_DURABILITY`

Journal writes are no longer fire-and-forget:

* **`besteffort`** (default): a failed persistence write is logged and the
  run continues — the pre-store behavior, right for local dev.
* **`strict`**: the first failed journal write **poisons the run** — the next
  live host effect refuses to execute ("acting on the world without a
  recording of it"), filesystem journal writes fsync before acknowledging,
  and the run's completion is gated on a final flush (the output-gate point:
  a result is not surfaced until its journal is durable).

## Recovery after machine loss: hydration

With a mirror configured, the journal survives the machine. On a fresh
machine, every load path (server session loads, `chidori resume`,
`chidori trace`) first calls `RunStoreFactory::hydrate(run_id)`: if the local
run directory has no journal but the mirror knows the run, the run directory
is materialized from the mirror and everything proceeds as if the files had
always been there. `list_runs` unions local run directories with the
mirror's runs, so runs written by a lost node are discoverable.

## Time travel: `--until-seq`

Because the journal is the state, replaying a prefix of it re-drives the
run's logic from any point in its history:

```bash
chidori resume agent.ts <run_id> --until-seq 12
```

replays records 1–12 from cache (zero LLM calls) and continues live from that
frontier. This is *logic-level* time travel — a stronger operation than
restoring a database to a past moment, because the run continues executing
from the restored point.

## Leases: single-writer ownership

`lease.json` (via `acquire_lease` / `release_lease` in `store.rs`) records
which process owns a run, with a TTL. The detached-agent supervisor
(`docs/detached-agents.md`) takes a run's lease before executing and releases
it on hibernate/settle; a second process sharing the same mirror stands down,
and an expired lease (a dead node) transfers on the next wake. Note the
check-and-set is last-writer-wins on the plain filesystem backend — real
multi-writer deployments should use the SQLite or HTTP backends, which
serialize writers.

## What this layer deliberately does not do

* **No semantic journal compaction.** Replay cost is still O(run history);
  the safepoint rewrite compacts the *files*, not the history. Folding old
  history into value checkpoints remains future work
  (`docs/resume-performance.md` §5 discusses the warm-standby direction).
* **No multi-node routing.** Leases arbitrate double-execution; they do not
  route requests to a run's owner. One server (or CLI process) drives a run
  at a time.
* **Branch stores mirror through the parent run's handle** (scoped keys), but
  out-of-band branch *reads* (`chidori branches`) stay filesystem-local —
  hydrate the run first on a fresh machine.
