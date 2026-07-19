---
title: "Observing with Tael"
---

# Observing runs with Tael

Chidori emits standard OTLP spans for every run — one parent span per run, one
child span per host call (`prompt`, `tool`, `http`, `branch`, …), nested by the
same parent/child structure the call log records. Any OTLP backend works
(Jaeger, Tempo, Honeycomb, Datadog); this guide uses
[**tael**](https://github.com/ThousandBirds/app-tael), the AI-agent-native
observability CLI, because the two products share a design goal: **a tael trace
and a Chidori run are two views of the same object.**

## One env var

```bash
# Terminal 1: tael server (OTLP gRPC on :4317, REST API on :7701)
tael serve

# Terminal 2: any chidori command, pointed at it
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
chidori run examples/agents/worker.ts \
  --input task="Reverse the word 'chidori'." --tools examples/tools
```

That's the whole integration. No collector, no SDK, no code changes.

## Three queries to try

```bash
# Every span of the run, with gen_ai.* token/model attributes on prompt spans
tael query traces --last 10m --format table

# Filter to one run — chidori.run_id is stamped on every span
tael query traces --attribute chidori.run_id=<run-id>

# The full waterfall: run span → host calls → JS function spans,
# plus the Chidori correlation footer (run id, checkpoint path, branches)
tael get trace <trace-id> --format table
```

`tael live` opens a TUI waterfall of the same data.

## What Chidori stamps on spans

| Attribute | Where | Meaning |
|---|---|---|
| `chidori.run_id` | every span | Join key: `chidori resume <agent.ts> <run_id>` replays this exact run |
| `chidori.checkpoint_path` | run span | The replayable artifact on disk (`.chidori/runs/<run id>/`) |
| `chidori.branch_id` / `chidori.branch_label` | spans inside a `chidori.branch` variant | Which fan-out variant a call executed in |
| `chidori.prompt.request_digest` | prompt spans | Content-addressed key for "the same prompt across runs" |
| `gen_ai.request.model`, `gen_ai.usage.input_tokens` / `output_tokens` / `cache_creation_tokens` / `cache_read_tokens` | prompt spans | OTEL GenAI semantic conventions — model, tokens, prompt-cache effectiveness |
| `tool.name`, `tool.arguments_json`, `tool.status`, `tool.latency_ms` | tool spans | Tael's typed tool-call fields |
| `signal.name`, `signal.from.*` | signal spans | Multiplayer provenance |
| `chidori.capability.*` | run span | Captured-effect surfaces the agent touched |

## The round trip

The correlation is bidirectional, and it's the point:

**Trace → run.** `tael get trace <id>` prints the run id and checkpoint path.
From there:

```bash
chidori resume <agent.ts> <run-id>            # replay it, $0, milliseconds
chidori resume <agent.ts> <run-id> --ci       # regression mode: exit 0 = byte-identical, 3 = drift
chidori branches <run-id>                     # its branch fan-outs
chidori branch-rerun <run-id> <branch-id>     # re-run one variant from the anchor
chidori checkpoint export <run-id>            # portable .tar.gz of the run
```

**Run → trace.** From any chidori run id:

```bash
tael query traces --attribute chidori.run_id=<run-id>   # its spans
tael comment list <trace-id>                            # annotations, issues, eval cases
tael experiment compare <run-id>                        # a chidori.branch A/B as an experiment
```

## Branch fan-outs as experiments

A `chidori.branch` fork renders as one subtree per variant, each span stamped
with its `chidori.branch_label`. Tael's experiment comparison reads those
labels directly — a branch A/B **is** an experiment, no extra instrumentation:

```bash
chidori run examples/branching/agent.ts --input topic="postmortem"
tael experiment compare <run-id> --format table
# Variant        Traces  Spans  Errors  Error %  Avg ms ...
# draft-direct   1       12     0       0.00     840.2
# outline-first  1       15     1       6.70     1204.9
```

## Golden cases that are checkpoints

`tael eval case add --from-trace <id>` promotes a failure into a regression
case. When the trace carries `chidori.run_id`, tael records the run id and
checkpoint path on the case — so the case's fixture is not a *description* of
the failed run, it is the failed run itself:

```bash
# Promote: the case records chidori_run_id + chidori_checkpoint_path
tael eval case add --from-trace <trace-id> --suite my-agent \
  --case-id timeout-001 --failure-mode tool_error

# Regression (exact): replay every case byte-for-byte at $0
tael eval run cases.jsonl --suite my-agent \
  --cmd 'chidori resume agent.ts {case_id} --ci'

# Live re-test (semantic): re-run a branch variant against current source
tael eval run cases.jsonl --suite my-agent \
  --cmd 'chidori branch-rerun {case_id} <branch-id>'
```

`chidori resume --ci` prints a machine-readable JSON report and exits 0 when
the replay matched the checkpoint exactly, 3 on divergence (with the first
mismatching call in the report), 1 on error. Strict mode also compares the
arguments the agent passes *now* against what the checkpoint recorded, so a
changed prompt or tool call fails loudly instead of returning a stale cached
result.

Archive a fixture without knowing the runs layout:

```bash
chidori checkpoint export <run-id>              # -> <run-id>.chidori-run.tar.gz
chidori checkpoint import <archive> --dir ci/   # restore under ci/.chidori/runs/
```

## The full loop

These pieces compose into a self-improvement harness — observe failures in
tael, fork controlled experiments with `chidori.branch`, validate against
checkpoint-backed eval suites, guard with `tael signal trend`. The runnable
end-to-end demo lives at
[`examples/self-harness-loop/`](../examples/self-harness-loop/).

## Notes

- Spans stream during the run (each ships as its call completes) and are
  emitted for **live** execution only — a resume never duplicates a prior
  turn's spans.
- Set `OTEL_SERVICE_NAME` to override the default `chidori` service name.
- Set `CHIDORI_OTEL_DEBUG=1` to surface exporter errors on stderr.
- JS-level function spans (one per agent-code function activation) nest under
  the host-call tree automatically.
