# Chidori run store on Cloudflare Durable Objects

A small Worker that gives every Chidori run **actor-grade storage**: one
Durable Object per run holds the run's journal and durable artifacts in
SQLite-backed storage, so every write Chidori acknowledges has been replicated
across data centers (Cloudflare's Storage Relay Service: a write is confirmed
only after a majority of geographically distributed followers hold it, then
batched into object storage) and is covered by 30-day point-in-time recovery.

Chidori's replay model is untouched — durability is still the deterministic
effect journal. This backend changes *where the journal sleeps at night*:
local disk stays the fast primary; the Durable Object mirror is the copy that
survives losing the machine.

## Deploy

```bash
cd integrations/cloudflare-durable-objects
npx wrangler deploy
npx wrangler secret put CHIDORI_RUN_STORE_TOKEN   # optional but recommended
```

## Point Chidori at it

```bash
export CHIDORI_RUN_STORE="https://chidori-run-store.<account>.workers.dev"
export CHIDORI_RUN_STORE_TOKEN="<the same token>"
chidori serve agent.ts            # or chidori run agent.ts
```

Every run now tees its journal, snapshot manifest, signal inbox, and
detached-agent registry to the Worker. Reads stay local; after machine loss,
`chidori resume <file> <run_id>` (or the server loading a session) hydrates
the run directory back out of the mirror automatically.

Optionally:

```bash
export CHIDORI_DURABILITY=strict   # a failed mirror write poisons the run
                                   # instead of being logged and tolerated
```

## Protocol

The Worker speaks the `HttpRunStore` REST protocol defined in
`crates/chidori/src/runtime/store.rs` (records + blobs per run, a run index,
and the detached-agent registry). Any server implementing that protocol works
as a mirror — the Durable Object deployment is the reference implementation.
