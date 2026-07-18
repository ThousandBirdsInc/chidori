# How replay works

<p align="center">
  <img src="../.github/record-replay.svg" alt="Animation: an original run executes prompt, tool, and http calls while recording each one into a numbered call log; the call log becomes a JSON checkpoint; replay re-runs the same code answering every host call from the log — identical output, zero LLM calls" width="860" />
</p>

TypeScript durable runs use deterministic runtime policy plus cached host-call
results. Given the same inputs, compatible source hashes, and the same cached
results for host calls, agent control flow is expected to produce the same
outputs.

1. **Original run:** Every `prompt()`, `tool()`, `fetch()` call is logged with seq number + result.
2. **Checkpoint:** The call log is a JSON array — save it to disk, send it over the wire, commit it to git.
3. **Replay:** Re-run the agent with the call log pre-loaded. Each host function call checks the log for its seq number — hit returns the cached result instantly, miss executes normally.

Replay is guarded, not best-effort:

- **Source verification:** every resume surface (the server's resume/approve
  routes *and* `chidori resume`) verifies the agent's entry + module source
  fingerprints against the run's snapshot manifest before replaying, and
  refuses on mismatch — cached results are never paired with changed code
  *silently*. (Runs persisted before manifests existed skip with a warning.)
- **Edit-and-resume is an explicit opt-in:** pass `--allow-source-change` to
  `chidori resume` (or `"allow_source_change": true` on the server's
  resume/signal/approve routes) to replay a recorded run against edited code.
  The divergence checks below still guard the journaled prefix — an edit that
  changes an already-recorded call fails loudly, an edit past the pause point
  resumes cleanly. ABI/policy mismatches are environment drift, not edits,
  and always refuse.
- **Divergence checks compare arguments, not just names:** a replayed call
  must match the recorded call's function *and* arguments (the derived
  `request_digest` field is ignored). A completed async host operation whose
  recorded arguments differ from the re-executed call's is a hard divergence
  error instead of a silent live re-execution of the side effect.
- **Escape hatch:** `CHIDORI_REPLAY_LAX=1` downgrades argument-level
  divergence to a warning (serving the cached result / re-executing live,
  the historical behavior). Function-name mismatches are always fatal.

`chidori resume` carries the run's own configuration so recovery needs no
flag archaeology:

- **The model travels with the run.** The run's resolved default model is
  recorded in its manifest; `resume` (and `branch-resume`/`branch-rerun`,
  and the server's resume/replay/approve routes) default to it. A bare
  `chidori resume agent.ts <run-id>` replays a `--model`-started run
  byte-for-byte; an explicit `--model`/`CHIDORI_MODEL` still overrides —
  and a divergence error that stems from a model mismatch says so, naming
  both models, instead of blaming "changed code".
- **Trust mirrors `run`.** `resume` accepts `--trusted` / `--untrusted` so
  live continuation past the replay frontier (crash recovery) executes under
  the same posture the original `chidori run --trusted` had. Without
  `--trusted`, gated effects re-ask at the terminal exactly like `run`.
- **Continuation is journaled.** Live records past the frontier persist
  into the same run directory, so a resume that itself crashes resumes from
  the *new* frontier — and the run's lease (`lease.json`) refuses a second
  concurrent driver of the same run dir.

This means you can:
- **Debug without spending money:** save a failing session, replay locally with breakpoints.
- **Run deterministic tests:** check in a run directory, and `chidori verify
  <agent.ts> <run_id>` asserts it still replays cleanly: no provider
  configured, deny-all policy, no writes to the run directory, output must be
  identical to the recorded one and every call must come from the journal
  (top-level workspace effects re-materialize their recorded artifacts —
  workspace state is real disk, not journal-served).
  Exit 0 on pass — a full integration test that costs $0 and runs in
  milliseconds, built for CI. A full run directory is heavy (the runtime
  snapshot blob alone can run to tens of MB), so don't commit it raw:
  `chidori export <run_id> --fixture tests/fixtures` copies just the four
  artifacts `verify` reads (`records.jsonl`, `runtime.snapshot.json`,
  `output.json`, `input.json`) into `tests/fixtures/<run_id>/` — typically
  a few KB. Commit that, and point verify at it with
  `chidori verify agent.ts <run_id> --runs-dir tests/fixtures`. Export
  refuses runs whose journal isn't a complete verifiable record (still
  leased by a live process, paused at a pending operation, or never
  completed).
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
