# Getting started & demos

This walks through running Chidori's example agents, inspecting a durable run,
and exercising the human-in-the-loop pause/resume loop. For the two-minute
version, see [Quick Start in the README](../README.md#️-quick-start).

## Try the demo picker

The easiest way to explore Chidori is the interactive demo picker:

```bash
cargo build
./target/debug/chidori demo
```

`chidori demo` shows a numbered list of runnable examples, including demos that
do not need an LLM provider and demos that exercise prompt tracing or streaming
when provider environment variables are configured. Choose **Hello agent** for
the fastest no-key path.

That demo runs a TypeScript agent, records a durable host-call log, and returns
JSON. The direct command is:

```bash
./target/debug/chidori run examples/agents/hello.ts --input name=Colton
```

Expected output:

```json
{
  "greeting": "Hello, Colton!"
}
```

What this demonstrates:

- `examples/agents/hello.ts` exports `agent(input, chidori)`.
- The agent calls `chidori.log(...)`, so the runtime records a host call.
- The agent returns plain JSON, which is what CLI, server, and SDK users receive.
- A checkpoint is written under `examples/agents/.chidori/runs/<run_id>/` for
  trace/replay workflows.

You can inspect the most recent run:

```bash
RUN_ID=$(ls -t examples/agents/.chidori/runs | head -1)
./target/debug/chidori trace "$RUN_ID" --dir examples/agents
./target/debug/chidori snapshot "$RUN_ID" --dir examples/agents
```

## Human-in-the-loop demo

This demo shows the session API pausing on `chidori.input(...)` and resuming
from the persisted call log:

<p align="center">
  <img src="../.github/pause-resume.svg" alt="Animation: an agent runs until input() pauses it, the session is persisted to disk, and when a human responds the runtime replays the call log to the pause point and continues live from there" width="860" />
</p>

Start the server:

```bash
./target/debug/chidori serve examples/agents/input_pause.ts --port 8080
```

In another terminal, create a session:

```bash
curl -s http://localhost:8080/sessions \
  -H "Content-Type: application/json" \
  -d '{"input":{"request":"ship the TypeScript runtime"}}'
```

The response will have `"status":"paused"`, an `"id"`, and
`"pending_prompt":"Approve this request?"`. Resume it with:

```bash
SESSION_ID=<paste id from the previous response>

curl -s http://localhost:8080/sessions/$SESSION_ID/resume \
  -H "Content-Type: application/json" \
  -d '{"response":"yes"}'
```

The completed response includes:

```json
{
  "output": {
    "request": "ship the TypeScript runtime",
    "approved": true
  }
}
```

That flow is the core Chidori loop: TypeScript code runs until a durable host
operation pauses, Chidori persists the run, and resume re-executes the agent
against the persisted call log to continue from where it paused.

## Example agents

See [`examples/`](../examples):

- [`agents/hello.ts`](../examples/agents/hello.ts) — minimal agent, no LLM
- [`agents/summarizer.ts`](../examples/agents/summarizer.ts) — LLM summary pipeline
- [`agents/context_qa.ts`](../examples/agents/context_qa.ts) — cache-aware multi-turn Q&A via `chidori.context`
- [`agents/streaming_progress.ts`](../examples/agents/streaming_progress.ts) — labelled prompt progress streams
- [`agents/webhook.ts`](../examples/agents/webhook.ts) — event-driven HTTP handler
- [`agents/tool_use.ts`](../examples/agents/tool_use.ts) — tool call example
- [`sdk_demo.py`](../examples/sdk_demo.py) — Python SDK with checkpointing + replay
- [`prompts/analysis.jinja`](../examples/prompts/analysis.jinja) — shared prompt template
- [`tools/web_search.ts`](../examples/tools/web_search.ts) — simple tool definition
- [`legacy-starlark/`](../examples/legacy-starlark) — archived Starlark examples kept for migration reference
