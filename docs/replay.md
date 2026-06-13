# How replay works

<p align="center">
  <img src="../.github/record-replay.svg" alt="Animation: an original run executes prompt, tool, and http calls while recording each one into a numbered call log; the call log becomes a JSON checkpoint; replay re-runs the same code answering every host call from the log — identical output, zero LLM calls" width="860" />
</p>

TypeScript durable runs use deterministic runtime policy plus cached host-call
results. Given the same inputs, compatible source hashes, and the same cached
results for host calls, agent control flow is expected to produce the same
outputs.

1. **Original run:** Every `prompt()`, `tool()`, `http()` call is logged with seq number + result.
2. **Checkpoint:** The call log is a JSON array — save it to disk, send it over the wire, commit it to git.
3. **Replay:** Re-run the agent with the call log pre-loaded. Each host function call checks the log for its seq number — hit returns the cached result instantly, miss executes normally.

This means you can:
- **Debug without spending money:** save a failing session, replay locally with breakpoints.
- **Run deterministic tests:** check in a checkpoint, assert the agent's behavior hasn't changed.
- **Resume after crashes:** the runtime can persist checkpoints after each call; on restart, replay picks up where it left off.
- **Pause for human approval:** `input()` suspends execution; when the human responds, the agent replays to that point and continues.

## Replaying from an SDK

Both SDKs talk to a running `chidori serve` instance over HTTP — no native
bindings, no install. The Python SDK is pure stdlib:

```python
import sys
sys.path.insert(0, "sdk/python")

from chidori import AgentClient, Checkpoint

client = AgentClient("http://localhost:8080")

# Create a session (runs the agent with live LLM calls)
session = client.run({"document": "Rust is a systems language."})
print(session.output)
# {"summary": "...", "action_items": "..."}

# Save a checkpoint to disk
checkpoint = session.checkpoint()
checkpoint.save("/tmp/session.json")
```

Later, replay the session from disk — **zero LLM calls**:

```python
from chidori import AgentClient, Checkpoint

client = AgentClient("http://localhost:8080")
cp = Checkpoint.load("/tmp/session.json")

# Replay: re-executes the agent but returns cached host-call results
replayed = client.replay(cp)
assert replayed.output == session.output  # identical output
```

See [`sdk/python/README.md`](../sdk/python/README.md) and
[`sdk/typescript/README.md`](../sdk/typescript/README.md) for the full SDK
surface.
