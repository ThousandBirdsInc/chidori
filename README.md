# Chidori v3

The third generation of [Chidori](https://github.com/ThousandBirdsInc/chidori) — a YAML-free AI agent framework where agents are written as **Starlark** scripts, a deterministic Python dialect that enables checkpointing, replay, and visual editing.

> **About v3.** Chidori began as a reactive runtime exploring how to build durable, debuggable agents. v3 is a ground-up rewrite that distills those ideas into a smaller, sharper core: a single Rust binary, Starlark instead of bespoke cells, and replay as the foundation for everything else (tests, debugging, resume, human-in-the-loop). Earlier versions of Chidori live in the git history and on prior tags.

- **Agents look like Python.** Native control flow, variables, list comprehensions — no template DSL.
- **Deterministic execution.** Every side effect goes through a host function the runtime can log, cache, and replay.
- **Zero-cost checkpointing.** Save a session's call log to disk, replay it later for identical output with zero LLM calls.
- **Event-driven agents.** Agents can run as HTTP servers that react to webhooks and other events.
- **Rust core, Python SDK.** The runtime is a single binary. The Python SDK talks to it over HTTP — no `pip install`, no native bindings.

## Quick Start

### 1. Write an agent

```python
# agents/summarizer.star
config(model = "claude-sonnet")

def agent(document):
    summary = prompt("Summarize in 3 bullets:\n" + document)
    actions = prompt("Extract action items:\n" + summary)
    return {"summary": summary, "action_items": actions}
```

### 2. Run it

```bash
# Set up LLM provider (uses LiteLLM in this example)
export LITELLM_API_URL=http://localhost:4401/v1
export LITELLM_API_KEY=sk-litellm-master-key

# Or use providers directly
# export ANTHROPIC_API_KEY=sk-ant-...
# export OPENAI_API_KEY=sk-...

cargo build
./target/debug/chidori run agents/summarizer.star \
  --input document="Rust is a systems programming language..."
```

### 3. Try the example agents

```bash
# Minimal agent — no LLM calls needed
./target/debug/chidori run examples/agents/hello.star --input name=Colton

# Summarizer with trace
./target/debug/chidori run examples/agents/summarizer.star \
  --input document="Rust is great." --trace

# Template-based agent
./target/debug/chidori run examples/agents/template_demo.star \
  --input '{"items": ["alpha", "beta", "gamma"]}'

# Event-driven webhook handler
./target/debug/chidori serve examples/agents/webhook.star --port 8080
```

## Core Concepts

An agent is a `.star` file with a `def agent(...)` function. The runtime provides a fixed set of **host functions** for side effects — everything else is pure Starlark:

| Function | Purpose |
|---|---|
| `prompt(text, ...)` | Send to an LLM, return string or parsed JSON |
| `template(str_or_path, ...)` | Render a Jinja2 template with minijinja |
| `tool(name, ...)` | Invoke a registered tool |
| `agent(name, ...)` | Call a sub-agent |
| `parallel(fns)` | Run functions concurrently |
| `input(msg, ...)` | Human-in-the-loop — pauses execution |
| `exec(code, ...)` | Run AI-generated code in a WASM sandbox |
| `http(method, url, ...)` | Make an HTTP request |
| `memory(action, ...)` | Persistent storage (key-value + vector) |
| `log(msg, ...)` | Structured logging |
| `env(name)` | Read environment variables |
| `retry(fn, ...)` | Retry with backoff |
| `try_call(fn)` | Capture errors without raising |

See [`llm.txt`](./llm.txt) for the full API reference.

## Running Modes

### 1. One-shot CLI

```bash
chidori run agents/my_agent.star --input key=value
chidori run agents/my_agent.star --input '{"complex": "input"}'
chidori check agents/my_agent.star          # validate without running
chidori tools --dir tools/                   # list available tools
```

### 2. HTTP Server (event-driven + session API)

```bash
chidori serve agents/my_agent.star --port 8080
```

Exposes:
- `GET  /health` — health check
- `ANY  /*` — any request is passed to `agent(event)` as an event dict
- `POST /sessions` — create a session and run the agent with given input
- `GET  /sessions` — list all sessions
- `GET  /sessions/{id}` — get session result
- `GET  /sessions/{id}/checkpoint` — get the call log (for replay)
- `POST /sessions/{id}/replay` — replay from a session's checkpoint

### 3. Event-Driven Agents

An agent can handle incoming HTTP events:

```python
# agents/webhook.star
config(model = "claude-sonnet")

def agent(event):
    if event["path"] == "/github":
        body = event["body"]
        summary = prompt("Summarize this GitHub event:\n" + repr(body))
        return {"status": 200, "body": {"summary": summary}}

    return {"status": 404, "body": {"error": "Unknown path"}}
```

```bash
chidori serve agents/webhook.star --port 8080

curl -X POST http://localhost:8080/github \
  -H "Content-Type: application/json" \
  -d '{"action": "opened", "pull_request": {"title": "Add login"}}'
```

## Python SDK

The Python SDK is a pure-stdlib HTTP client that talks to a running `chidori serve` instance. No `pip install`, no native bindings.

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

# Replay: re-executes Starlark but returns cached LLM results
replayed = client.replay(cp)
assert replayed.output == session.output  # identical output
```

## How Replay Works

Starlark is deterministic. Given the same inputs and the same cached results for host function calls, the agent's control flow is guaranteed to produce the same outputs.

1. **Original run:** Every `prompt()`, `tool()`, `http()` call is logged with seq number + result.
2. **Checkpoint:** The call log is a JSON array — save it to disk, send it over the wire, commit it to git.
3. **Replay:** Re-run the agent with the call log pre-loaded. Each host function call checks the log for its seq number — hit returns the cached result instantly, miss executes normally.

This means you can:
- **Debug without spending money:** save a failing session, replay locally with breakpoints.
- **Run deterministic tests:** check in a checkpoint, assert the agent's behavior hasn't changed.
- **Resume after crashes:** the runtime can persist checkpoints after each call; on restart, replay picks up where it left off.
- **Pause for human approval:** `input()` suspends execution; when the human responds, the agent replays to that point and continues.

## Examples

See [`examples/`](./examples):

- [`agents/hello.star`](./examples/agents/hello.star) — minimal agent, no LLM
- [`agents/summarizer.star`](./examples/agents/summarizer.star) — 2-step LLM pipeline
- [`agents/template_demo.star`](./examples/agents/template_demo.star) — Jinja2 prompt templates
- [`agents/webhook.star`](./examples/agents/webhook.star) — event-driven HTTP handler
- [`sdk_demo.py`](./examples/sdk_demo.py) — Python SDK with checkpointing + replay
- [`prompts/analysis.jinja`](./examples/prompts/analysis.jinja) — shared prompt template
- [`tools/greet.star`](./examples/tools/greet.star) — simple tool definition

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  User code (.star files, .jinja prompts, Python SDK) │
└────────────────────────┬────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────┐
│               Rust Core Runtime                      │
│                                                      │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │  Starlark   │ │ Host Function│ │  Checkpoint  │  │
│  │  Evaluator  │ │ Registry     │ │  / Replay    │  │
│  └─────────────┘ └──────────────┘ └──────────────┘  │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │  LLM Client │ │  Template    │ │  HTTP Server │  │
│  │  (providers)│ │  (minijinja) │ │  (axum)      │  │
│  └─────────────┘ └──────────────┘ └──────────────┘  │
└──────────────────────────────────────────────────────┘
```

- **Starlark evaluator** (`starlark-rust` crate) parses `.star` files and executes them.
- **Host functions** (`#[starlark_module]`) are the only way agents touch the outside world.
- **Checkpoint/replay engine** intercepts host calls for deterministic replay.
- **LLM providers** (Anthropic, OpenAI, LiteLLM-compatible) are swappable via `reqwest`.
- **Template engine** uses `minijinja` for Jinja2 prompt templates.
- **HTTP server** (`axum`) powers the `serve` command and session API.

See [`DESIGN.md`](./DESIGN.md) for the full architecture and design rationale, and [`TODO.md`](./TODO.md) for the implementation roadmap.

## Project Structure

```
chidori/
├── src/
│   ├── main.rs             # CLI entry point
│   ├── server.rs           # HTTP server (serve + session API)
│   ├── runtime/
│   │   ├── engine.rs       # Starlark evaluator + agent() invocation
│   │   ├── host_functions.rs  # prompt, template, config, log, env
│   │   ├── context.rs      # Runtime context (call log + replay)
│   │   ├── call_log.rs     # Checkpoint data structures
│   │   └── template.rs     # minijinja integration
│   ├── providers/
│   │   ├── mod.rs          # Provider registry, model routing
│   │   ├── anthropic.rs    # Anthropic Messages API
│   │   └── openai.rs       # OpenAI-compatible (incl. LiteLLM)
│   └── tools/
│       └── mod.rs          # Tool discovery + JSON schema generation
├── sdk/
│   └── python/chidori/     # Python SDK (pure stdlib, no deps)
├── examples/
│   ├── agents/             # Example .star agents
│   ├── prompts/            # Example .jinja templates
│   ├── tools/              # Example tools
│   └── sdk_demo.py         # Python SDK demo
├── DESIGN.md               # Architecture & design rationale
├── TODO.md                 # Implementation roadmap
└── llm.txt                 # Complete API reference for LLM-assisted development
```

## Current Status

**Phase 1 (Core Runtime)** — ✅ Working
- Starlark evaluator with host functions (`prompt`, `template`, `config`, `log`, `env`)
- LLM providers: Anthropic, OpenAI, LiteLLM/OpenAI-compatible
- CLI: `run`, `check`, `tools`, `serve`
- Template engine (minijinja)
- Structured tracing & token accounting
- Tool auto-discovery

**Phase 2 (Sessions + Replay)** — ✅ Working
- Session API (create, list, get, checkpoint, replay)
- Replay-based checkpointing in the engine
- Python SDK with checkpoint save/load
- Event-driven agents over HTTP

**Phase 3 (Visual Editor)** — 🚧 Not started. See [`DESIGN.md`](./DESIGN.md).

**Phase 4 (Production)** — 🚧 Partial (serve works, memory/SDK packaging TBD).
