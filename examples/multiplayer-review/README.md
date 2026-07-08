# Multiplayer review — signals, the durable mailbox, deterministic replay

A runnable example of **`chidori.signal` / `chidori.pollSignal`** (see
[`docs/signals.md`](../../docs/signals.md)): one agent run stays open to
**multiple participants** — humans *and* other agents — who push information
into it mid-flight, while the whole session stays **deterministically
replayable**.

The agent ([`policy_doc.ts`](./policy_doc.ts)) turns a brief into a policy
document. Three participants collaborate on **one live run**:

- a **human editor** (Mara) reviews drafts and asks for changes,
- a **compliance-checker agent** pushes an approve/changes verdict,
- a **human lead** (Sam) re-scopes the doc mid-run.

None of these are agent-initiated `input()` questions. The reviewers **push** a
`review` signal when they have it; the lead **steers** with a `steer` signal
whenever he wants. The agent only consumes those pushes at the points it
declares safe:

```ts
// BLOCKS — nothing to do until a review lands; the run idles cheaply on disk.
const review = await chidori.signal<Review>("review");

// NON-BLOCKING — steering is optional; check the mailbox, move on if empty.
const steer = await chidori.pollSignal<Steer>("steer");
```

`writeDraft`/`revise` are local helpers so the example runs offline with no LLM
provider; swap them for `chidori.prompt(...)` to get real model spans.

## Serve the agent

```bash
cargo run -- serve examples/multiplayer-review/policy_doc.ts --port 8080
# or, with the from-source release build:
./target/release/chidori serve examples/multiplayer-review/policy_doc.ts --port 8080
```

Create a run (it drafts, then **pauses** at `signal("review")`):

```bash
curl -s -H 'Content-Type: application/json' \
  -XPOST localhost:8080/sessions \
  -d '{"input":{"topic":"data-retention policy"}}'
# -> { "id": "<RUN>", "status": "paused", "pending_signal_name": "review", ... }
```

Grab the id into `$RUN`. A session view of a signal pause carries
`pending_signal_name: "review"` (it is `null` for a plain `input()` pause), so a
caller knows *which* named signal to deliver.

## Deliver signals (multiplayer)

> The endpoint requires `Content-Type: application/json`. `curl -d` defaults to
> form-encoding, so the header is mandatory.

**The lead steers — while the agent is mid-flight and not yet listening.** The
run is paused on `review`, not `steer`, so this lands in the **durable mailbox**
and is consumed at the next `pollSignal("steer")`. The endpoint returns
`202 Accepted` with the assigned `delivery_seq`:

```bash
curl -s -H 'Content-Type: application/json' \
  -XPOST localhost:8080/sessions/$RUN/signal \
  -d '{
    "name": "steer",
    "payload": { "priority": "high", "scope": "EU + UK only" },
    "from": { "kind": "human", "id": "sam" }
  }'
# 202 -> { "id": "<RUN>", "status": "queued", "name": "steer", "delivery_seq": 1 }
```

**The compliance agent delivers a "changes" verdict.** The run *is* paused
waiting on `review`, so this **resolves the pause and resumes** the run — which
revises (draining Sam's queued `steer` at the `pollSignal`) and pauses again on
the next `review`. The endpoint returns `200 OK` with the advanced session view:

```bash
curl -s -H 'Content-Type: application/json' \
  -XPOST localhost:8080/sessions/$RUN/signal \
  -d '{
    "name": "review",
    "payload": { "decision": "changes", "notes": "Tighten the data-retention section." },
    "from": { "kind": "agent", "id": "compliance-bot" }
  }'
# 200 -> { "status": "paused", "pending_signal_name": "review", ... }
```

**The human editor approves.** The agent publishes and the run completes:

```bash
curl -s -H 'Content-Type: application/json' \
  -XPOST localhost:8080/sessions/$RUN/signal \
  -d '{
    "name": "review",
    "payload": { "decision": "approve", "notes": "LGTM" },
    "from": { "kind": "human", "id": "mara" }
  }'
# 200 -> status "completed", output:
# { "status": "published", "rounds": 2,
#   "approvedBy": { "kind": "human", "id": "mara" }, "draft": "..." }
```

Delivering to a **completed / failed / cancelled** run returns `409 Conflict`
(no inbox write); an empty `name` is `400`; an unknown session is `404`.

### From the SDKs instead of curl

```python
from chidori import AgentClient, Session, SignalQueued
client = AgentClient("http://localhost:8080")
paused = client.run({"topic": "data-retention policy"})           # status "paused"
queued = client.signal(paused.id, "steer", {"priority": "high"})  # SignalQueued (202)
done   = client.signal(paused.id, "review",
                       {"decision": "approve", "notes": "LGTM"},
                       from_={"kind": "human", "id": "mara"})       # Session (200)
```

```ts
import { AgentClient, isSignalQueued } from "@1kbirds/chidori";
const client = new AgentClient("http://localhost:8080");
const res = await client.signal(run, { name: "review", payload: { decision: "approve", notes: "LGTM" } });
if (isSignalQueued(res)) console.log("queued at", res.delivery_seq);
else console.log(res.status, res.output);
```

## What the trace shows

Each signal is a recorded host call, so the multiplayer session is one ordered
trace with every participant attributed by `from`:

```bash
RUN_DIR=examples/multiplayer-review/.chidori/runs
RUN_ID=$(ls -t "$RUN_DIR" | head -1)
cargo run -- trace "$RUN_ID" --dir examples/multiplayer-review
```

```
Calls: 10
  #1   log          writing draft
  #2   log          draft round 1 ready
  #3   signal       {"name":"review"}     <- idled here; resolved by compliance-bot (changes)
  #4   log          review received
  #5   poll_signal  {"name":"steer"}      <- drained Sam's queued steer (delivery_seq 1)
  #6   log          scope changed mid-run
  #7   log          revising draft
  #8   log          draft round 2 ready
  #9   signal       {"name":"review"}     <- resolved by mara (approve)
  #10  log          review received
```

The `signal` and `poll_signal` calls freeze `{payload, from}` into the call log,
so *who reviewed each draft* and *when the lead re-scoped* are a durable audit,
not lost in a chat channel.

## It replays deterministically

```bash
cargo run -- resume examples/multiplayer-review/policy_doc.ts "$RUN_ID" \
  --dir examples/multiplayer-review
```

`resume` (any replay) reproduces the **identical** run: the compliance verdict,
Sam's steering, and Mara's approval are replayed from their recorded
`CallRecord`s — no human is re-contacted and the compliance agent is not re-run.
The inbox is a live-only convenience; the call log is the source of truth, so a
replay with an empty mailbox still reproduces every consumed signal.

## Notes

- The full signal surface is documented in
  [`docs/signals.md`](../../docs/signals.md): the blocking named signal,
  durable per-run mailbox, `/signal` delivery endpoint, and deterministic
  replay; `pollSignal`, the fan-in `chidori.signal(["review", "steer"])`,
  `timeoutMs` (resolves to a `{timedOut: true}` sentinel after the deadline),
  and sender provenance as OTEL span attributes; and live in-memory
  delivery — a signal sent to a run streaming over `/sessions/stream` lands in
  the running agent's mailbox in-memory and resumes a matching pause
  in-process, the response reporting `"delivered_live"`.
