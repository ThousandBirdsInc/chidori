---
title: "Durable Storage"
description: "The run store: append-only journal, SQLite and Durable Object mirrors, hydration, strict durability, leases, and time travel."
---

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
* **`checkpoint.json`** — the full-log artifact, rewritten at **compaction
  points**: pause, settle, branch merges, and the first safepoint after a
  resume replay (whose replayed + synthetic records bypass the append path —
  the context tracks this as the checkpoint-dirty flag). Steady-state
  per-effect safepoints persist only the manifest + pending artifacts: the
  O(1) append already made the record durable, so rewriting the whole
  checkpoint per host call would cost O(history²) bytes per run for nothing.
  Each rewrite doubles as compaction of the append-only file: it truncates
  `records.jsonl` to match, so neither file grows past one run's history.

Loading unions the two: the last checkpoint wins per-seq, and any tail
records appended after the last compaction — the steady-state case, not just
crash recovery — are recovered from `records.jsonl`.

The **host-promise table** follows the same append+compact discipline. Each
state change (begin/resolve/reject) writes one small per-operation blob
(`host_promises/<id>.json`) — O(1) on every backend — instead of rewriting
the whole `host_promises.json` table per host call. Compaction points (pause,
settle, a server-side delivery) fold the blobs into the table file and delete
them; readers union both, per-op blobs winning by id. The per-op blob is what
keeps the crash-between-resolve-and-record dedup guarantee: a resolved effect
whose journal record never landed is still recognized on resume and not
re-executed. Recognition requires the recorded arguments to match the
re-executed call's (ignoring the derived `request_digest`); a mismatch is a
hard replay-divergence error rather than a silent live re-execution
(`CHIDORI_REPLAY_LAX=1` restores the old tolerate-and-re-execute behavior). The manifest's embedded copy of the table has the same freshness
contract as `checkpoint.json` (compaction-time snapshot; runtime resume never
reads it).

## Backends

Selected by `CHIDORI_RUN_STORE`:

| Value | Backend |
|---|---|
| unset / `fs` | Filesystem only (the default — exactly the pre-existing behavior) |
| `sqlite` | Mirror to a shared SQLite database (`CHIDORI_RUN_DB`, default `<run_base>/runs.sqlite3`). One row per record — not the session store's blob-per-session shortcut. |
| `s3://bucket[/prefix]` | Mirror to any **S3-compatible object store** — AWS S3, Cloudflare R2, GCS interop, Backblaze, MinIO, LocalStack. No server-side code to deploy: point `CHIDORI_RUN_STORE_ENDPOINT` at the store (default `https://s3.<region>.amazonaws.com`), supply the standard `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (or `CHIDORI_RUN_STORE_*` overrides), and requests are SigV4-signed in-process (no AWS SDK). Each journal append is one object PUT (`runs/<id>/records/<seq>.json`); safepoint checkpoints compact the tail objects. Bucket versioning gives point-in-time recovery for free. **This is the recommended default mirror.** |
| `http(s)://…` | Mirror to a remote relay speaking the run-store REST protocol. The reference deployment is one **Cloudflare Durable Object per run** (`integrations/cloudflare-durable-objects/`), which gives every acknowledged write cross-datacenter replication, 30-day point-in-time recovery, and a **serialized writer per run** (the platform enforces a single instance per id — the strongest lease story). `CHIDORI_RUN_STORE_TOKEN` adds bearer auth. |

Choosing between the remote backends: reach for `s3://` when you want durability
with zero deployment surface (most users); reach for the Durable Object relay
when you want platform-enforced single writers and the lowest write
confirmation latency, or as the beachhead for future multi-node routing.

## Write-error policy: `CHIDORI_DURABILITY`

Journal writes are no longer fire-and-forget:

* **`besteffort`** (default): a failed persistence write is logged and the
  run continues — the pre-store behavior, right for local dev.
* **`strict`**: the first failed journal write **poisons the run** — the next
  live host effect refuses to execute ("acting on the world without a
  recording of it"), filesystem journal writes fsync before acknowledging,
  and the run's completion is gated on a final flush (the output-gate point:
  a result is not surfaced until its journal is durable).

The durability mode also decides how remote-mirror appends are paced. Under
`besteffort`, HTTP/S3 record appends are **pipelined**: each append is
enqueued on the mirror's single FIFO relay thread and the agent continues
immediately instead of blocking one network round-trip per host call
(ordering against later checkpoint writes and loads is preserved by the
FIFO; in-flight requests are bounded, so a slow mirror applies backpressure
rather than growing an unbounded queue). Failures surface at the next
`flush()` barrier — pause, settle, output gate — where besteffort logs and
continues, exactly as its per-append handling always did. Under `strict`,
every append stays synchronous: acknowledged by the mirror before the next
effect runs.

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

## Repairing a failed run: `--retry-failed`

A run that failed mid-flight leaves its journal ending in the failed
record(s), so replaying it replays the failure. Repair used to mean
hand-computing an `--until-seq` frontier just before the failure —
error-prone, and easy to get wrong in a way that forfeits `verify`.
`--retry-failed` does it first-class:

```bash
chidori resume agent.ts <run_id> --retry-failed
```

strips the trailing failed record(s) from the journal — cascading to any
nested effects the failing call consumed, the same crash-frontier rule the
actor `restart: "resume"` path uses — replays every record before the
failure from cache, and re-executes the failed call live
(`retry-failed: stripped N failed record(s) (seqs X..Y), replaying M records
then executing live` on stderr names the split). On success the run settles
normally and the repaired journal is coherent: `chidori verify` passes on it.

Tolerance is scoped to the retried call only: the stripped tail re-executes
live, so a different args/result on the retry needs no opt-in, while the
surviving prefix still replays under the normal divergence rules
(`--allow-source-change` keeps its usual meaning). The flag refuses a run
whose journal has no trailing failure — a completed run needs nothing, a
paused run wants plain `resume` — and is mutually exclusive with
`--until-seq`.

## Leases: single-writer ownership

`lease.json` (via `acquire_lease` / `release_lease` in `store.rs`) records
which process owns a run, with a TTL. The detached-agent supervisor
(`docs/detached-agents.md`) takes a run's lease before executing and releases
it on hibernate/settle; a second process sharing the same mirror stands down,
and an expired lease (a dead node) transfers on the next wake. Note the
check-and-set is last-writer-wins on the plain filesystem backend **and on
S3-compatible object stores** (no compare-and-swap is used) — deployments
that need enforced single writers should use the SQLite backend (one
connection serializes writers) or the Durable Object relay (the platform
guarantees one instance per run).

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
