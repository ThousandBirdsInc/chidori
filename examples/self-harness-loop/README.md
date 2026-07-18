# The Self-Harness Loop

A runnable implementation of the self-improvement loop from Lilian Weng's
["Harness Engineering for Self-Improvement"](https://lilianweng.github.io/posts/2026-07-04-harness/)
— weakness mining, harness proposal, validation — built from only shipped
`chidori` and [`tael`](https://github.com/ThousandBirds/app-tael) commands plus
one ~100-line reflector agent.

| Weng's stage | This demo |
|---|---|
| **Weakness mining** | tael: error trace → `issue create` → `eval case add` (fixture = the checkpoint) |
| **Harness proposal** | chidori: `reflector.ts` reads the trajectory from tael, writes a revised strategy |
| **Validation** | chidori `branch` A/B from the anchored state → `tael experiment compare` → `resume --ci` at $0 |

The claim worth internalizing: the golden test case produced by this loop is
**not a description of a failed run — it is the failed run itself**, replayable
byte-for-byte at $0, forkable from its exact anchor state.

## Setup

```bash
# Terminal 1: tael (OTLP :4317, REST :7701)
tael serve

# Terminal 2: from the chidori repo root
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317

# No API key needed for the walkthrough — the canned-response provider
# exercises the full loop. (With a real key, drop this and the reflector's
# diagnosis becomes a real model proposal.)
export CHIDORI_TEST_LLM_RESPONSE="Root cause: single-attempt tool call with no retry on a transiently failing backend. Bounded fix: retry with backoff (max 3 attempts)."
```

Run everything below **from the chidori repo root** (branch strategy paths are
repo-root-relative).

## Step 1 — Run + observe: the failure

The worker answers tasks by searching a knowledge base. The bundled
`flaky_search` tool times out on first attempts — and the worker's naive
strategy makes exactly one:

```bash
chidori run examples/self-harness-loop/worker.ts \
  --input task="deployment rollback procedure"
# Error: JavaScript exception: flaky_search: upstream timeout after 5000ms (attempt 1)
```

The run failed, but nothing is lost: the trace streamed to tael (with an error
span) and the checkpoint persisted under
`examples/self-harness-loop/.chidori/runs/<run-id>/`.

```bash
chidori_run_id=$(ls -t examples/self-harness-loop/.chidori/runs/ | head -1)
tael query traces --attribute chidori.run_id=$chidori_run_id --status error --format table
#  tool.call        | error
#  agent.run worker | error
trace_id=<the trace id from that output>
```

## Step 2 — Weakness mining

Classify the failure and promote it into a golden case. Because the trace
carries `chidori.run_id`, the case records the checkpoint as its fixture:

```bash
tael issue create --from-trace $trace_id --failure-mode tool_error --impact high \
  --summary "flaky_search times out on first attempt; worker has no retry"

tael eval case add --from-trace $trace_id --suite self-harness-demo \
  --case-id $chidori_run_id --failure-mode tool_error \
  --expected-behavior "worker retries transient search failures"
# Eval case self-harness-demo/<run-id> added from trace <trace-id>
# Fixture: chidori run <run-id> — replay with `chidori resume <agent.ts> <run-id> --ci` ($0)
```

(The case id **is** the chidori run id, so `{case_id}` in eval commands
substitutes straight into `chidori resume`.)

## Step 3 — Harness proposal: the reflector

The reflector is itself a Chidori agent. It pulls the failed trajectory from
tael's REST API, asks the model for a diagnosis and a bounded harness edit, and
writes the proposed strategy into the workspace as a branch-ready module:

```bash
chidori run examples/self-harness-loop/reflector.ts \
  --input trace_id=$trace_id --input tael_url=http://localhost:7701
# {
#   "diagnosis": "Root cause: single-attempt tool call with no retry ...",
#   "error_spans": 2,
#   "proposed_strategy": "strategies/retry_with_backoff.ts",
#   ...
# }
```

`strategies/retry_with_backoff.ts` now exists next to the incumbent
`strategies/naive.ts`. The reflector's every step — the tael fetch, the
diagnosis prompt, the workspace write — is durable and recorded; the
*improvement process itself* is replayable.

## Step 4 — Controlled experiment

Fork from a shared anchored prefix into both strategies — one variable, same
state:

```bash
chidori run examples/self-harness-loop/experiment.ts \
  --input task="deployment rollback procedure"
# winner: retry_with_backoff
#   naive              → failed
#   retry_with_backoff → completed
experiment_run=$(ls -t examples/self-harness-loop/.chidori/runs/ | head -1)
```

Each variant's spans carry `chidori.branch_label`, so tael scores the A/B with
no extra instrumentation:

```bash
tael experiment compare $experiment_run --format table
# | Variant            | Traces | Spans | Errors | Error % | ...
# | naive              | 1      | 2     | 1      | 50.00   |
# | retry_with_backoff | 1      | 4     | 1      | 25.00   |   <- one retry, then success
```

## Step 5 — Validation

The winner's checkpoint is the regression fixture. Replay it — byte-for-byte,
$0, milliseconds:

```bash
chidori resume examples/self-harness-loop/experiment.ts $experiment_run --ci
# { "status": "match", "calls_replayed": 10, "live_cost_usd": 0.0, ... }   exit 0
```

Or run it as a tael eval suite (case id = run id):

```bash
echo "{\"case_id\": \"$experiment_run\"}" > cases.jsonl
tael eval run cases.jsonl --suite self-harness-demo \
  --cmd 'chidori resume examples/self-harness-loop/experiment.ts {case_id} --ci >/dev/null'
# eval run run_...: complete
```

`--ci` is strict: if the agent's behavior drifts — different call, different
arguments, extra or missing calls — the replay exits 3 with the first
mismatching call in a JSON report. Try it:

```bash
sed -i '' 's/anchoring shared prefix/anchoring modified prefix/' examples/self-harness-loop/experiment.ts
chidori resume examples/self-harness-loop/experiment.ts $experiment_run --ci
# { "status": "diverged", ... "Replay divergence at seq 1 (`log`) ..." }   exit 3
git checkout examples/self-harness-loop/experiment.ts
```

## Step 6 — Guard

The failure mode is now monitored and the fix is regression-locked:

```bash
tael signal trend tool_error --format table    # frequency by day — watch it drop
chidori checkpoint export $experiment_run --dir examples/self-harness-loop
# -> <run-id>.chidori-run.tar.gz — commit it; CI replays it forever at $0
```

Restore anywhere with `chidori checkpoint import <archive>` and the same
`resume --ci` gate.

## Try the loop's output without running anything

A recorded experiment run is checked in at
`regression/experiment-run.chidori-run.tar.gz` — the demo's own regression
fixture. Replay it at $0, no API key, from the repo root:

```bash
chidori checkpoint import examples/self-harness-loop/regression/experiment-run.chidori-run.tar.gz --dir /tmp/demo
chidori resume examples/self-harness-loop/experiment.ts \
  b18522eb-be61-4136-80bf-084931b8fcd3 --ci --dir /tmp/demo
# { "status": "match", ... "live_cost_usd": 0.0 }
```

(Run from the repo root — the branch variants' strategy sources are
repo-root-relative paths.)

## What this is and isn't

This demo supplies the **loop**, not the mind. The reflector's proposal is a
template — real harness-proposal quality is the frontier model's job (swap the
canned response for a real provider and prompt it for the actual code). What
the substrate guarantees is everything around the proposal: the failure is
replayable evidence, the experiment is controlled (same anchor, one variable),
the validation is deterministic, and the regression guard costs nothing to run.

## Files

| File | Role |
|---|---|
| `worker.ts` | The production agent with the weakness (single-attempt tool call) |
| `tools/flaky_search.ts` | The seeded failure: first attempts time out, retries succeed |
| `reflector.ts` | Reads the failed trajectory from tael, proposes + writes a strategy |
| `strategies/naive.ts` | The incumbent (failing) strategy, as a branch variant |
| `strategies/retry_with_backoff.ts` | Written by the reflector in step 3 |
| `experiment.ts` | `chidori.branch` A/B of the two strategies from one anchor |
