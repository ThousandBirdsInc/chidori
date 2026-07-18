# Interactive pipeline — long-running, human-in-the-loop, streamed to tael

A runnable example of a **long-running agent that pauses for human feedback in
your terminal (a REPL)** and **streams OpenTelemetry spans to [tael](../../../app-tael)
while it runs** — so you watch the trace fill in across the session instead of
all at once at the end.

It shows the current authoring convention: **import the host object and define
the entrypoint with `run(handler)`** — no second `chidori` parameter, no magic
`agent` export.

```ts
import { chidori, run } from "chidori:agent";

run(async (input) => {
  await chidori.log("hello", { input });
  const answer = await chidori.input("continue?");
  return { answer };
});
```

The runtime strips the `from "chidori:agent"` import and supplies `chidori` + `run`
at execution time. The agent ([`interactive_pipeline.ts`](./interactive_pipeline.ts))
runs several review stages; each stage delegates a batch to the `review_batch`
tool — an import-defined tool (`defineTool`, right in the agent file) whose
body runs in the agent's own VM — and then stops at a **checkpoint** for you to
type a decision.

## Run it

From the repo root (the convenience wrapper auto-points at tael when it's up on
`:4317`):

```bash
examples/interactive-pipeline/run.sh

# …or directly
chidori run examples/interactive-pipeline/interactive_pipeline.ts \
  -i '{"pipeline":"triage","stages":5,"itemsPerStage":4}' --trusted

# …or from source
cargo run -- run examples/interactive-pipeline/interactive_pipeline.ts \
  -i '{"pipeline":"triage","stages":5,"itemsPerStage":4}' --trusted
```

(`--trusted` runs this in-repo code without ask-by-default approval prompts.
The `review_batch` tool and the `chidori.log`/`chidori.input` calls aren't
gated effects, so this example runs the same with or without the flag; it's
here for consistency with the other examples. See
[`docs/running-modes.md`](../../docs/running-modes.md).)

At each checkpoint the agent prints a prompt to your terminal and **blocks on
stdin** (`chidori.input`). Type one of:

- `continue` (or any free-text note) → advance to the next stage
- `rerun` → redo the current stage
- `stop` → end the run now

The run lasts as long as your interactive session — that's the "long duration":
the human is the pacing function.

## Stream spans to tael

Start tael (it listens for OTLP/gRPC on `127.0.0.1:4317` by default), then point
chidori at it via the standard env var and run:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
export OTEL_SERVICE_NAME=interactive-pipeline        # optional (defaults to "chidori")

chidori run examples/interactive-pipeline/interactive_pipeline.ts \
  -i '{"pipeline":"triage","stages":5,"itemsPerStage":4}' --trusted
```

`run.sh` does this for you, but only when tael is actually listening on `:4317`.

What you'll see in tael — each host call streams as a span, in order, as the
run progresses:

```
agent.run interactive_pipeline
├─ host.log    stage 1/N: begin
├─ host.log    review_batch: scanned item 1/4   ← the tool's own calls (in-VM)
├─ host.log    review_batch: scanned item 2/4
├─ host.log    review_batch: scanned item 3/4
├─ host.log    review_batch: flagged item 3
├─ host.input  Stage 1/N checkpoint    ← the run idles here, waiting on you
├─ host.log    stage 1: operator said 'continue'
├─ host.log    stage 2/N: begin
└─ …
```

(An import-defined tool runs in the agent's own VM, so its host calls are
recorded as the run's own calls — one flat stream — rather than nested under a
separate `tool.call` span. The tool's work is still fully in the trace.)

- The per-stage work is delegated to the `review_batch` tool, which logs each
  item **internally**. Because a tool runs inside the same run (sharing the
  runtime context), its host calls are recorded as children — so they **nest
  under the `tool.call` span** (real OTEL `parent_span_id`), one level below the
  top-level agent calls. That's the nesting.
- Spans **arrive incrementally as calls complete** (not batched at the end): a
  stage's `tool.call` and its nested logs show up, then the run idles on the
  `host.input` span while it waits for you, then the next stage streams in after
  you answer. (A nested log is recorded before its parent tool completes, so it's
  buffered briefly and then ships nested under the tool — automatically.)

> If tael isn't running, an unreachable OTLP endpoint is dropped silently — the
> agent runs exactly the same, just without exporting. Tracing is purely
> env-driven; the agent code is unaware of it.

## Inspect / replay the run (durability)

Every run is checkpointed under `examples/interactive-pipeline/.chidori/runs/<run-id>/`.

```bash
RUNS=examples/interactive-pipeline/.chidori/runs
RUN_ID=$(ls -t "$RUNS" | head -1)

# Pretty-print the recorded call log (the spans, as data).
chidori trace "$RUN_ID" --dir examples/interactive-pipeline

# Resume: replays your earlier answers from the journal (no re-prompting) and
# continues from where it left off.
chidori resume examples/interactive-pipeline/interactive_pipeline.ts "$RUN_ID" \
  --dir examples/interactive-pipeline
```

On resume, the replayed calls are **not** re-exported to tael (a resume doesn't
duplicate the prior turn's spans) — only newly executed calls stream out.

## Notes

- Default input: `{"pipeline":"triage","stages":5,"itemsPerStage":4}`. Bump
  `stages` / `itemsPerStage` for a longer run with more spans.
- Uses `chidori.log`, `chidori.input`, and `chidori.tool` — no API keys or
  external services. Swap a `chidori.log` for `chidori.prompt(...)` if you have
  an LLM provider configured and want real model spans (`gen_ai.*`) in tael.
- The `run(handler)` entrypoint + `import { chidori } from "chidori:agent"`
  convention is the current (and only) authoring form.
