---
title: "Durable Storage"
description: "The run store: append-only journal, SQLite and Durable Object mirrors, hydration, strict durability, leases, and time travel."
---

# Durable storage: the run store

Chidori's durability model is the deterministic effect journal: every host
call is recorded, and recovery is replaying the journal
([Replay & Resume](./replay.md)). That gives *logical* durability —
replayability. This document covers the layer underneath: **where the
journal's bytes live**, what guarantees each backend gives, and how a run
survives losing the machine.

## The layering

```
  agent code (plain TypeScript)
      │  chidori.* host calls
  effect journal (records.jsonl)        ← the replay model; unchanged
      │
  ┌───┴─────────────────────────────┐
  │ filesystem (always, primary)    │   .chidori/runs/<run_id>/...
  │ + durable mirror (optional)     │   SQLite file · s3:// bucket ·
  └─────────────────────────────────┘   http(s):// relay
```

Everything a run persists flows through one run-store handle:

* the journal (`records.jsonl`) and `checkpoint.json`, its compacted artifact;
* the snapshot manifest and blob;
* the pending host call and the host-promise table;
* the signal inbox — `signals/inbox.json` inside the run directory
  ([Detached Agents](./detached-agents.md));
* branch stores;
* the source history — `history/`, the git-like record of the agent's
  implementation alongside the journal ([Source History](./source-history.md));
* and, as a sibling of the run directories, the detached-agent registry —
  `.chidori/runs/agents/<name>.json`
  ([Detached Agents](./detached-agents.md)).

The filesystem layout is always the primary and is byte-identical to what the
framework has always written, so every existing consumer (the viewer,
`chidori trace`, external tooling) keeps working. A configured durable mirror
receives a copy of every write.

This is the same local-fast / remote-durable split Cloudflare built for
Durable Objects' storage (local disk for reads, replicated relay for
durability) — applied to the journal.

## The journal on disk: append-only + `checkpoint.json`

Two artifacts per run:

* **`records.jsonl`** — the journal: append-only, one JSON record per line,
  one host call each. Appending a record costs O(1) bytes.
* **`checkpoint.json`** — the compacted whole-journal artifact, rewritten at
  **compaction points**: pause, settle, branch merges, and the first safepoint
  after a resume replay. Steady-state per-effect safepoints persist only the
  manifest + pending artifacts: the O(1) append already made the record
  durable, so rewriting the whole artifact per host call would cost
  O(history²) bytes per run for nothing. Each rewrite doubles as compaction of
  the append-only file: it truncates `records.jsonl` to match, so neither file
  grows past one run's history.

Loading unions the two: the last `checkpoint.json` wins per record, and any
tail records appended after the last compaction — the steady-state case, not
just crash recovery — are recovered from `records.jsonl`.

The **host-promise table** follows the same append+compact discipline. Each
state change (begin/resolve/reject) writes one small per-operation blob
(`host_promises/<id>.json`) — O(1) on every backend — instead of rewriting the
whole table per host call. Compaction points fold the blobs into the table
file and delete them; readers union both, per-op blobs winning by id. The
per-op blob is what keeps the crash-between-resolve-and-record dedup
guarantee: a resolved effect whose journal record never landed is still
recognized on resume and not re-executed. Recognition requires the recorded
arguments to match the re-executed call's (ignoring the derived
`request_digest`); a mismatch is a hard replay-divergence error rather than a
silent live re-execution (`CHIDORI_REPLAY_LAX=1` restores the old
tolerate-and-re-execute behavior).

## Backends

Selected by `CHIDORI_RUN_STORE`:

| Value | Backend |
|---|---|
| unset / `fs` | Filesystem only (the default — exactly the pre-existing behavior) |
| `sqlite` | Mirror to a shared SQLite database (`CHIDORI_RUN_DB`, default `<run_base>/runs.sqlite3`). One row per journal record. |
| `s3://bucket[/prefix]` | Mirror to any **S3-compatible object store** — AWS S3, Cloudflare R2, GCS interop, Backblaze, MinIO, LocalStack. No server-side code to deploy: point `CHIDORI_RUN_STORE_ENDPOINT` at the store (default `https://s3.<region>.amazonaws.com`), supply the standard `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (or `CHIDORI_RUN_STORE_*` overrides), and requests are SigV4-signed in-process (no AWS SDK). Each journal append is one object PUT (`runs/<id>/records/<seq>.json`); `checkpoint.json` rewrites at compaction points fold the tail objects. Bucket versioning gives point-in-time recovery for free. |
| `http(s)://…` | Mirror to a remote relay speaking the run-store REST protocol. Two reference deployments: one **Cloudflare Durable Object per run** (`integrations/cloudflare-durable-objects/`), which gives every acknowledged write cross-datacenter replication, 30-day point-in-time recovery, and a **serialized writer per run** (the platform enforces a single instance per id — the strongest lease story); or the **self-hosted cell store** (`chidori cell-store`, below), which delivers the same one-database-per-run, single-writer model on your own machines. `CHIDORI_RUN_STORE_TOKEN` adds bearer auth to both. |

Rule of thumb: `sqlite` on a durable disk you back up; `s3://` when the
machine is ephemeral (containers, managed hosts); `chidori cell-store` when
you want enforced single writers on your own infrastructure; the Durable
Object relay when you want the strongest failover guarantees and don't mind
depending on Cloudflare. [Deployment](./deployment.md) applies the same rule
to concrete hosting recipes.

## Self-hosted cell store: `chidori cell-store`

An alternative to the Durable Object relay that runs on your own
infrastructure, implementing the core of the design behind Deno's
[celld](https://github.com/denoland/celld) ("self-hosted, distributed Durable
Objects"). It serves the exact run-store REST protocol, so it is a drop-in
`CHIDORI_RUN_STORE` target. The cell store speaks plain HTTP — keep it on a
private network behind your firewall; authentication is the
`CHIDORI_RUN_STORE_TOKEN` bearer token:

```bash
# One node, replicating to any S3-compatible bucket (S3, R2, MinIO, …):
chidori cell-store --bucket s3://chidori-cells --listen 0.0.0.0:9700

# Point Chidori at it, exactly like the Durable Object worker
# (plain HTTP on the private network; bearer auth via the token):
export CHIDORI_RUN_STORE="http://storehost:9700"
export CHIDORI_RUN_STORE_TOKEN="…"   # same knob, enforced by the node
chidori serve agent.ts
```

The celld model, applied to runs:

* **Every run is its own SQLite database** (a *cell*) on node-local disk —
  runs shard by construction, one run's failure can't corrupt another's.
* **The bucket is the fleet's source of truth.** Cells replicate to it as
  immutable snapshots on the `--sync-secs` cadence (and at hibernate and
  shutdown); ownership records live next to them. Nodes are replaceable.
* **Object-storage compare-and-swap owns cells.** Exactly one node owns a
  cell at a time with no membership protocol, failure detector, or consensus
  service: ownership epoch *N* is a create-only PUT (`If-None-Match: *`) of
  the cell's `meta/N.json` — atomic create means exactly one winner per
  epoch. Leases expire; takeover is winning the next epoch and restoring the
  database from the current snapshot. A fenced ex-owner (its epoch
  superseded) drops its copy and answers 409 with the live owner's identity.
* **Idle cells hibernate to nearly nothing** (`--idle-secs`): a final
  replication, published unowned *without resetting the epoch*, database
  closed, memory dropped. The next request — on any node sharing the bucket —
  claims the next epoch immediately and restores it.

Durability shape: local disk stays the fast primary (a node restart reclaims
its own cells losslessly, unpublished writes included); the bucket is the
copy that survives losing the machine, fresh to within one sync window. Run
several nodes against one bucket for failover.

**Routing: a 409 you can follow.** A client still talks to one node, and a
cell owned by another live node is refused rather than proxied — full
request routing is out of scope for the same reason celld's V8 application
runtime is: a cell here holds the run's code but never runs it (the engine
lives in the Chidori process), so "wrong node, here is the owner" is a
complete answer to a client that only reads and appends bytes. What the
refusal now carries is an *address*. Start a node with
`--advertise http://host:9700` and that URL is stamped into every ownership
record it writes, next to the epoch, so the current owner's address is as
durable as its identity; the 409 body gains `owner_url` alongside `owner` and
`lease_expires_at`. A client relay that gets such a 409 retries the same
request — same path, same body, same bearer token — against the owner,
**exactly once**: if the owner also refuses, that fence stands rather than
starting a chase. Both halves are optional and compatible in either
direction: meta records and 409 bodies without the field parse fine (a node
with no `--advertise`, or an older one, just doesn't hand out an address),
and an older client ignores a field it doesn't know.

Lease traffic is deliberately exempt. A 409 about a run's `lease.json` **is**
the ownership verdict ([Leases](#leases-single-writer-ownership)), so it is
never re-aimed: a node that lost its cell stands down exactly as before
instead of arbitrating ownership through the node that took it over.
Following applies to the read/append traffic addressed at a store, not to who
may run the run.

Nodes that execute the runs they own — the fleet scaling workers rather than
just storage — need more than that. The prerequisite of a suspended agent any
node can pick up cheaply exists: see
[VM images](./resume-performance.md#7-landed-vm-images), which make resume
cost track a run's live state instead of its history. Agent source is no
longer a gap either: a node that lacks an agent's project tree materializes
it from the run's own durable source history (or, for older runs, the
snapshot bundle) and runs from there — see
[Detached agents](./detached-agents.md). The remaining gaps are per-node
config and secrets distribution, multi-tenant isolation with a warm pool,
and routing beyond this one-hop follow: nothing here places work on a node,
picks an owner by load, or drains one for maintenance — ownership is still
decided by whoever asks first after a lease lapses.

## Write-error policy: `CHIDORI_DURABILITY`

Journal writes are not fire-and-forget:

* **`besteffort`** (default): a failed persistence write is logged and the
  run continues — right for local dev.
* **`effect`**: **durable at the effect, priced at the effect.** Remote
  appends stay pipelined (the thing that makes `strict` expensive against a
  remote store), but each effectful host call runs a durability barrier
  around its pending-intent write *before* the effect executes and around
  its result record *after* it completes. A crash therefore never leaves an
  executed effect with no durable trace — either the intent is durable
  (recovery sees a pending operation that may have fired) or the result is —
  while pure records (steps, logs) never pay a per-write round-trip.
  Failed writes poison the run and filesystem writes fsync, exactly as
  under `strict`.
* **`strict`**: the first failed journal write **poisons the run** — the next
  live host call refuses to execute ("acting on the world without a
  recording of it"), filesystem journal writes fsync before acknowledging,
  and the run's completion is gated on a final flush (the output-gate point:
  a result is not surfaced until its journal is durable).

The durability mode also decides how remote-mirror appends are paced. Under
`besteffort`, HTTP/S3 record appends are **pipelined**: each append is
enqueued on the mirror's single FIFO relay thread and the agent continues
immediately instead of blocking one network round-trip per host call
(ordering against later `checkpoint.json` writes and loads is preserved by the
FIFO; in-flight requests are bounded, so a slow mirror applies backpressure
rather than growing an unbounded queue). Failures surface at the next flush
barrier — pause, settle, output gate — where besteffort logs and continues,
exactly as its per-append handling always did. Under `effect`, appends stay
pipelined but every effectful call drains the pipeline at its two barriers
(intent before, result after), so barrier frequency scales with effects, not
with journal records. Under `strict`, every append stays synchronous:
acknowledged by the mirror before the next effect runs.

## Recovery after machine loss: hydration

With a mirror configured, the journal survives the machine. On a fresh
machine, every load path (server session loads, `chidori resume`,
`chidori trace`) first tries **hydration**: if the local run directory has no
journal but the mirror knows the run, the run directory is materialized from
the mirror and everything proceeds as if the files had always been there. Run
listings union local run directories with the mirror's runs, so runs written
by a lost node are discoverable.

## Inspecting and exporting a run

Two CLI commands look at this layer directly, and they do different things:
`chidori snapshot <run_id>` pretty-prints the run's snapshot manifest, while
`chidori checkpoint export <run_id>` packs the whole run directory into a
portable tar.gz (`chidori checkpoint import` unpacks it under another
machine's `.chidori/runs/`). See the [CLI reference](./cli.md).

## Time travel: `--until-seq`

Because the journal is the state, replaying a prefix of it re-drives the
run's logic from any point in its history:

```bash
chidori resume agent.ts <run_id> --until-seq 12
```

replays records 1–12 from the journal (zero LLM calls) and continues live
from that frontier. This is *logic-level* time travel — a stronger operation
than restoring a database to a past moment, because the run continues
executing from the restored point.

## Repairing a failed run: `--retry-failed`

A run that failed mid-flight leaves its journal ending in the failed
record(s), so replaying it replays the failure. Repair used to mean
hand-computing an `--until-seq` frontier just before the failure —
error-prone, and easy to get wrong in a way that forfeits `chidori verify`.
`--retry-failed` does it first-class:

```bash
chidori resume agent.ts <run_id> --retry-failed
```

strips the trailing failed record(s) from the journal — cascading to any
nested effects the failing call consumed, the same crash-frontier rule the
[actor](./actors.md) `restart: "resume"` path uses — replays every record
before the failure, and re-executes the failed call live
(`retry-failed: stripped N failed record(s) (seqs X..Y), replaying M records
then executing live` on stderr names the split). On success the run settles
normally and the repaired journal is coherent: `chidori verify` passes on it.

Tolerance is scoped to the retried call only: the stripped tail re-executes
live, so a different args/result on the retry needs no opt-in, while the
surviving prefix still replays under the normal divergence rules
(`--allow-source-change` keeps its usual meaning — see
[Replay & Resume](./replay.md)). The flag refuses a run whose journal has no
trailing failure — a completed run needs nothing, a paused run wants plain
`resume` — and is mutually exclusive with `--until-seq`.

## Leases: single-writer ownership

`lease.json` records which process owns a run, with a TTL. The detached-agent
supervisor ([Detached Agents](./detached-agents.md)) takes a run's lease
before executing and releases it on hibernate/settle; a second process
sharing the same mirror stands down, and an expired lease (a dead node)
transfers on the next wake.

**Server resumes take the lease too.** `POST /sessions/{id}/resume`, a
`/signal` that resolves the pending pause, and `/approve` each acquire the
run's lease for the duration of the leg and release it when the leg settles
or re-pauses. Two `chidori serve` processes pointed at the same run store
therefore cannot both accept a resume of the same paused run: the second
writer gets **409 Conflict** with `lease_holder` and `lease_expires_at` in
the body, before any durable state is touched. The same lease excludes a
concurrent `chidori resume` of that run from the CLI. The TTL is
`CHIDORI_RUN_LEASE_TTL_SECS` (default 600); a dead holder's lease lapses at
its expiry and the next writer takes the run over. On a backend that cannot
serve the lease at all the server logs a warning and proceeds (the same
advisory posture the CLI uses) — the guarantee column in the table below
says how strong the arbitration is per backend.

**The lease is fleet state, so it lives in the shared backend.** When a
durable mirror is configured, lease reads and writes address the mirror
directly rather than the local filesystem copy. Reading the local primary
first would give every machine its own private `lease.json` and defeat the
whole mechanism: node A would keep seeing its own stale local lease while
node B owned the run in the mirror. With no mirror, the filesystem store is
itself the coordination target, so single-machine behavior is unchanged.

**Acquisition is a compare-and-swap, not a read-then-write.** The runtime
reads the current lease bytes, decides (free / mine / expired / held), then
swaps *against exactly the bytes it read*. A lost swap means someone else
wrote first: the caller re-reads and re-decides instead of clobbering the
winner. How atomic that swap really is depends on the backend:

| Backend | Lease guarantee |
|---|---|
| `fs` | **Advisory.** The default read-compare-write has no atomic primitive behind it; two processes can interleave. Fine for one machine, one process. |
| `sqlite` | **Enforced.** The swap runs in one `BEGIN IMMEDIATE` transaction, so concurrent writers serialize — including writers in other processes sharing the file. |
| `s3://` | **Advisory.** Object stores are last-writer-wins on overwrite; the create-only conditional PUT the cell store uses for *cell ownership* does not generalize to overwriting an existing lease across every S3-compatible implementation. |
| `chidori cell-store` | **Enforced.** The swap rides HTTP conditional headers (`If-Match` / `If-None-Match`) that the node evaluates inside the cell's own lock — one serialization point per run. |
| Durable Object relay | **Enforced.** Same conditional headers, evaluated inside the run's Durable Object, of which the platform runs exactly one. |

A fenced node — one whose cell another node has taken over — gets a `409`
from the store naming the live owner. The runtime turns that into the
ordinary "held by someone else" outcome, so every existing standdown path
(the detached-agent supervisor's wait-for-expiry loop, `chidori resume`'s
refusal, `chidori chat`'s error) fires unchanged rather than the run being
poisoned by a mirror-write failure.

## What this layer deliberately does not do

* **No semantic journal compaction.** Replay cost is still O(run history);
  the safepoint rewrite compacts the *files*, not the history.
  [Value checkpoints](./value-checkpoints.md) (`chidori.step`) let an agent
  memoize expensive pure compute explicitly; automatic folding of old history
  into value checkpoints is not supported. (Engineering note on the
  warm-standby direction: [resume performance](./resume-performance.md) on
  GitHub.)
* **No multi-node routing.** Leases arbitrate double-execution; they do not
  route requests to a run's owner. One server (or CLI process) drives a run
  at a time. (The cell store's `--advertise` + one-hop follow redirects a
  *store* client to the owning node; it does not move execution.)
* **Branch stores mirror through the parent run's handle** (scoped keys), but
  out-of-band branch *reads* (`chidori branches`) stay filesystem-local —
  hydrate the run first on a fresh machine.

## Limits

Stated so the guarantees above aren't read as stronger than they are:

* **`s3://` leases are advisory.** Not every S3-compatible store supports the
  conditional overwrite the lease swap would need, so on `s3://` the lease is
  last-writer-wins. Concurrent writers against the same `s3://` mirror are
  not coordinated — stop the old instance before starting the new.
* **Fencing is observed at lease boundaries.** A node that loses a cell
  mid-run learns about it at its next lease renewal (per supervisor
  iteration) and stands down then. Its in-flight journal appends between
  those points surface as generic mirror-write failures — logged under
  `besteffort`, run-poisoning under `strict`.
* **The cell store still does not route, it redirects.** A cell owned by
  another live node is refused with a `409` naming the owner — never proxied.
  With `--advertise` the refusal also carries the owner's URL and the client
  retries there once, which is redirection, not request routing: no node
  forwards traffic, nothing places work on a node or picks an owner by load,
  and lease arbitration never follows. Single ownership is solved at the
  storage layer; picking *which* node should serve a given run is still the
  operator's job — one `chidori serve` per agent, as
  [Deployment](./deployment.md) describes.
* **Cell-store bucket freshness is the sync cadence.** An acknowledged write
  is durable on the owning node's disk (`synchronous=FULL`) but reaches the
  bucket on the `--sync-secs` tick, at hibernate, and at graceful shutdown.
  Losing a store node's disk outright can therefore lose up to one sync
  window.
* **Snapshot GC is best-effort.** A publish retires the superseded snapshot
  and pre-takeover epoch metas after swinging the pointer. An interrupted GC
  leaves orphaned objects (never a dangling pointer) — a bucket lifecycle
  rule on the `state/` prefix is the pragmatic backstop.
