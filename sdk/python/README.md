# chidori Python SDK

Pure-stdlib HTTP client for a running `chidori serve` instance. No native
bindings, no third-party dependencies.

## Install

```bash
pip install -e ./sdk/python
```

## Usage

```python
from chidori import AgentClient, Checkpoint

client = AgentClient("http://localhost:8080")

# Run an agent
session = client.run({"document": "Rust is a systems language."})
print(session.output)

# Save and replay a checkpoint — zero LLM calls on replay
checkpoint = session.checkpoint()
checkpoint.save("/tmp/session.json")

replayed = client.replay(Checkpoint.load("/tmp/session.json"))
assert replayed.output == session.output

# Durable TypeScript runs may include snapshot metadata in the checkpoint.
# The manifest is safe to inspect; raw VM snapshot bytes remain server-side.
if checkpoint.snapshot_manifest:
    print(checkpoint.snapshot_manifest["abi"]["engine_fork"])

manifest = client.get_snapshot_manifest(session.id)
print(manifest["policy"]["typescript_imports"])

# Paused sessions (when the agent calls `input()`)
paused = client.run({"action": "delete-prod"})
if paused.status == "paused":
    print("prompt:", paused.pending_prompt)
    final = client.resume(paused.id, "yes")

# Multiplayer signals (when the agent calls `chidori.signal` / `pollSignal`):
# deliver {name, payload?, from?} to a run. A run paused-waiting on this name
# resolves + resumes (returns a Session); otherwise the signal is enqueued into
# the durable mailbox (returns a SignalQueued with the assigned delivery_seq).
from chidori import Session, SignalQueued
paused = client.run({"topic": "data-retention policy"})
print("awaiting signal:", paused.pending_signal_name)  # -> "review"
result = client.signal(
    paused.id, "review",
    payload={"decision": "approve", "notes": "LGTM"},
    from_={"kind": "human", "id": "mara"},
)
if isinstance(result, SignalQueued):
    print("queued at delivery_seq", result.delivery_seq)  # 202
else:
    print(result.status, result.output)  # 200 — resumed Session

# Live streaming: yields host calls, prompt stream events, then `done`
for evt in client.stream({"document": "hi"}):
    if evt["type"] == "call":
        print("call:", evt["record"]["function"])
    elif evt["type"] == "prompt_delta":
        print("delta:", evt["delta"])
    elif evt["type"] == "done":
        print("done:", evt["status"], evt["output"])
```

Snapshot-aware checkpoints include the replay call log plus optional manifest
metadata. Durable resume is exposed through `client.resume(session_id,
response)` for paused sessions, recovering through persisted host-promise
metadata and the replay journal. Replay **is** the resume mechanism by design —
the QuickJS live-VM snapshot path was removed in #39, not merely deferred, so
the manifest carries journal/scaffold metadata rather than serialized VM bytes.

Mirrors the TypeScript SDK (`sdk/typescript/`) method-for-method. See the
top-level `examples/sdk_demo.py` for a longer walkthrough and the server's
`README.md` for the HTTP session API this client wraps.

## Testing

```bash
cargo build                     # make sure target/debug/chidori is up to date
python3 -m unittest sdk/python/tests/test_session_api.py -v
```

The integration tests spin up a real `chidori serve` subprocess per
config (default / auth / concurrency / cors) and drive it through this
SDK against an in-process stdlib mock LLM server. No real provider
traffic; no third-party dependencies.
