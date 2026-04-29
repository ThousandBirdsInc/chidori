# app-agent Python SDK

Pure-stdlib HTTP client for a running `app-agent serve` instance. No native
bindings, no third-party dependencies.

## Install

```bash
pip install -e ./sdk/python
```

## Usage

```python
from app_agent import AgentClient, Checkpoint

client = AgentClient("http://localhost:8080")

# Run an agent
session = client.run({"document": "Rust is a systems language."})
print(session.output)

# Save and replay a checkpoint — zero LLM calls on replay
checkpoint = session.checkpoint()
checkpoint.save("/tmp/session.json")

replayed = client.replay(Checkpoint.load("/tmp/session.json"))
assert replayed.output == session.output

# Paused sessions (when the agent calls `input()`)
paused = client.run({"action": "delete-prod"})
if paused.status == "paused":
    print("prompt:", paused.pending_prompt)
    final = client.resume(paused.id, "yes")

# Live streaming: yields one event per host function call, then `done`
for evt in client.stream({"document": "hi"}):
    if evt["type"] == "call":
        print("call:", evt["record"]["function"])
    elif evt["type"] == "done":
        print("done:", evt["status"], evt["output"])
```

Mirrors the TypeScript SDK (`sdk/typescript/`) method-for-method. See the
top-level `examples/sdk_demo.py` for a longer walkthrough and the server's
`README.md` for the HTTP session API this client wraps.

## Testing

```bash
cargo build                     # make sure target/debug/app-agent is up to date
python3 -m unittest sdk/python/tests/test_session_api.py -v
```

The integration tests spin up a real `app-agent serve` subprocess per
config (default / auth / concurrency / cors) and drive it through this
SDK against an in-process stdlib mock LLM server. No real provider
traffic; no third-party dependencies.
