<div align="center">

# &nbsp; Chidori (v3) &nbsp;

**An agent framework where TypeScript agents can checkpoint, replay, and resume by default.**

<p>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="GitHub Last Commit" src="https://img.shields.io/github/last-commit/ThousandBirdsInc/chidori" /></a>
<a href="https://crates.io/crates/chidori"><img alt="crates.io version" src="https://img.shields.io/crates/v/chidori" /></a>
<a href="https://pypi.org/project/chidori/"><img alt="PyPI version" src="https://img.shields.io/pypi/v/chidori" /></a>
<a href="https://www.npmjs.com/package/chidori"><img alt="npm version" src="https://img.shields.io/npm/v/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/blob/main/LICENSE"><img alt="License Apache-2.0" src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" /></a>
</p>
<br />
</div>

Star us on GitHub! Join us on [Discord](https://discord.gg/CJwKsPSgew).

> **About v3.** Chidori began as a reactive runtime exploring how to build durable, debuggable agents. v3 is a ground-up rewrite that distills those ideas into a smaller, sharper core: a single Rust binary, TypeScript agent authoring, and replay as the foundation for tests, debugging, resume, and human-in-the-loop workflows. Earlier versions of Chidori live in the git history and on prior tags.

## Contents
- [📖 About](#-about)
- [⚡️ Quick Start](#️-quick-start)
- [▶️ Try The Demo](#️-try-the-demo)
- [🧩 Core Concepts](#-core-concepts)
- [🚦 Running Modes](#-running-modes)
- [🐍 Python SDK](#-python-sdk)
- [⏪ How Replay Works](#-how-replay-works)
- [🧪 Examples](#-examples)
- [✅ JavaScript Conformance (Test262)](#-javascript-conformance-test262)
- [🏗 Architecture](#-architecture)
- [📦 Project Structure](#-project-structure)

## 📖 About

- **Agents are TypeScript.** Native async control flow, typed inputs, imports, and editor tooling with no template DSL.
- **Deterministic execution.** Every side effect goes through a host function the runtime can log, cache, and replay.
- **Zero-cost checkpointing.** Save a session's call log to disk, replay it later for identical output with zero LLM calls.
- **Event-driven agents.** Agents can run as HTTP servers that react to webhooks and other events.
- **Rust core, TS and Python SDKs.** The runtime is a single binary. SDKs talk to it over HTTP without native bindings.

## ⚡️ Quick Start

### 1. Write an agent

```ts
// agents/summarizer.ts
import type { Chidori } from "chidori";

export async function agent(input: { document: string }, chidori: Chidori) {
  const summary = await chidori.prompt(
    "Summarize in 3 bullets:\n" + input.document,
    { type: "summary" },
  );
  const actionItems = await chidori.prompt(
    "Extract action items:\n" + summary,
    { type: "actions" },
  );
  return { summary, actionItems };
}
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
./target/debug/chidori run agents/summarizer.ts \
  --input document="Rust is a systems programming language..."
```

### 3. Try the example agents

```bash
# Interactive example picker
./target/debug/chidori demo

# Minimal agent — no LLM calls needed
./target/debug/chidori run examples/agents/hello.ts --input name=Colton

# Local TypeScript tool — no LLM calls needed
./target/debug/chidori run examples/agents/tool_use.ts \
  --input query=chidori --tools examples/tools

# Summarizer with trace
./target/debug/chidori run examples/agents/summarizer.ts \
  --input document="Rust is great." --trace

# Parallel host work
./target/debug/chidori run examples/agents/parallel.ts \
  --input '{"topic": "runtime snapshots"}'

# Event-driven webhook handler
./target/debug/chidori serve examples/agents/webhook.ts --port 8080
```

## ▶️ Try The Demo

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

### Human-In-The-Loop Demo

This demo shows the session API pausing on `chidori.input(...)` and resuming
from the persisted VM state.

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
operation pauses, Chidori persists the run, and resume continues from the saved
state.

## 🧩 Core Concepts

An agent is a `.ts` file that exports an async `agent(input, chidori)` function. The runtime provides a fixed set of **host functions** for side effects through the `chidori` object:

| Function | Purpose |
|---|---|
| `chidori.prompt(text, { type, ... })` | Send to an LLM, return string or parsed JSON; streamed prompt events carry the optional type |
| `chidori.template(strOrPath, vars)` | Render a Jinja2 template with minijinja |
| `chidori.tool(name, args)` | Invoke a registered tool |
| `chidori.callAgent(path, input)` | Call a sub-agent |
| `chidori.parallel(fns)` | Run functions concurrently |
| `chidori.input(msg, options)` | Human-in-the-loop — pauses execution |
| `chidori.execJs(...)`, `chidori.execPython(...)`, `chidori.execWasm(...)` | Run generated code in a sandbox |
| `chidori.http(url, options)` | Make an HTTP request |
| `chidori.memory(action, ...)` | Persistent storage (key-value + vector) |
| `chidori.log(msg, data)` | Structured logging |
| `chidori.env(name)` | Read environment variables |
| `chidori.retry(fn, options)` | Retry with backoff |
| `chidori.tryCall(fn)` | Capture errors without raising |

See [`llm.txt`](./llm.txt) for the full API reference.

### Streaming Prompt Progress

Agents can label prompt output streams with `type` so UIs can filter incremental
progress separately from final answers:

```ts
const status = await chidori.prompt("Say what work is starting", { type: "progress" });
const answer = await chidori.prompt("Write the final answer", { type: "final" });
```

When using `--stream` or `POST /sessions/stream`, prompt calls emit
`prompt_start`, `prompt_delta`, and `prompt_end` events with `stream_id`,
`seq`, and `prompt_type`. This also works for prompts inside
`chidori.parallel(...)` branches and `chidori.callAgent(...)` sub-agents. See
[`examples/agents/streaming_progress.ts`](./examples/agents/streaming_progress.ts).

## 🚦 Running Modes

### 1. One-shot CLI

```bash
chidori demo                                  # pick from runnable examples
chidori run agents/my_agent.ts --input key=value
chidori run agents/my_agent.ts --input '{"complex": "input"}'
chidori check agents/my_agent.ts            # validate without running
chidori tools --dir tools/                   # list available tools
```

### 2. HTTP Server (event-driven + session API)

```bash
chidori serve agents/my_agent.ts --port 8080
```

Exposes:
- `GET  /health` — health check
- `ANY  /*` — any request is passed to `agent(event)` as an event dict
- `POST /sessions` — create a session and run the agent with given input
- `GET  /sessions` — list all sessions
- `GET  /sessions/{id}` — get session result
- `GET  /sessions/{id}/checkpoint` — get the call log and snapshot manifest metadata
- `GET  /sessions/{id}/snapshot` — inspect snapshot manifest metadata without raw VM bytes
- `POST /sessions/{id}/resume` — resume a paused `input()` or approval session
- `POST /sessions/{id}/replay` — replay from a session's checkpoint
- `POST /sessions/{id}/cancel` — cancel a running or stored session
- `POST /sessions/stream` — run a session with SSE call and prompt progress events

### 3. Event-Driven Agents

An agent can handle incoming HTTP events:

```ts
// agents/webhook.ts
import type { Chidori } from "chidori";

export async function agent(
  input: { url: string; payload?: Record<string, unknown> },
  chidori: Chidori,
) {
  const response = await chidori.http(input.url, {
    method: "POST",
    body: input.payload ?? { source: "chidori" },
  });
  return { status: response.status, body: response.body };
}
```

```bash
chidori serve agents/webhook.ts --port 8080

curl -X POST http://localhost:8080/github \
  -H "Content-Type: application/json" \
  -d '{"action": "opened", "pull_request": {"title": "Add login"}}'
```

## 🐍 Python SDK

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

# Replay: re-executes the agent but returns cached host-call results
replayed = client.replay(cp)
assert replayed.output == session.output  # identical output
```

## ⏪ How Replay Works

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

## 🧪 Examples

See [`examples/`](./examples):

- [`agents/hello.ts`](./examples/agents/hello.ts) — minimal agent, no LLM
- [`agents/summarizer.ts`](./examples/agents/summarizer.ts) — LLM summary pipeline
- [`agents/streaming_progress.ts`](./examples/agents/streaming_progress.ts) — labelled prompt progress streams
- [`agents/webhook.ts`](./examples/agents/webhook.ts) — event-driven HTTP handler
- [`agents/tool_use.ts`](./examples/agents/tool_use.ts) — tool call example
- [`sdk_demo.py`](./examples/sdk_demo.py) — Python SDK with checkpointing + replay
- [`prompts/analysis.jinja`](./examples/prompts/analysis.jinja) — shared prompt template
- [`tools/web_search.ts`](./examples/tools/web_search.ts) — simple tool definition
- [`legacy-starlark/`](./examples/legacy-starlark) — archived Starlark examples kept for migration reference

## ✅ JavaScript Conformance (Test262)

Chidori runs agents in an embedded QuickJS runtime. To check that runtime
against the same yardstick Bun and Node use, we run [**Test262**](https://github.com/tc39/test262) —
the official TC39 ECMAScript conformance suite.

```bash
# Vendor the suite (shallow clone of tc39/test262) and run language + built-ins.
# First run clones ~56k files; subsequent runs reuse the checkout.
scripts/test262.sh

# Run a subset:
scripts/test262.sh test/built-ins/Array
scripts/test262.sh --filter Promise

# Full machine-readable report + per-failure detail:
scripts/test262.sh --json target/test262-report.json --verbose
```

The runner prints a pass/fail/skip summary:

```
Test262 (chidori/QuickJS bare context)
  pass 39178  fail 202  skip 7885  =>  99.49% of executed
```

It drives the **bare ECMAScript context** (no `chidori` host object), so the
number is pure language conformance — directly comparable to how Bun and Node
report it. Modules and dynamic `import()` run by default; features the engine
does not implement (and that Bun/Node also skip) are reported as `skip`, never
hidden.

You can also run the runner directly (after `cargo build --release -p test262-runner`):

```bash
target/release/test262-runner --test262 vendor/test262 --help
```

See [`docs/conformance.md`](./docs/conformance.md) for how it works, the honest
skip policy, and the remaining engine-level gaps.

### In-tree pure-Rust engine (experimental)

Alongside the embedded QuickJS runtime, the tree ships an **independent,
pure-Rust JavaScript engine** (`crates/chidori-js`) — oxc parser → bytecode →
stack VM, with deterministic-replay durable execution and **no C and no
`boa_engine`**. It is selectable with `--engine rust`:

```bash
target/release/test262-runner --test262 vendor/test262 --engine rust
```

It is younger than the QuickJS path (which scores **99.5%** above), but already
runs the full language + built-ins suite. Each test executes on its own
worker thread with a wall-clock timeout and cooperative cancellation, so the
whole suite completes in **~5 minutes** instead of hanging on pathological cases.

**Latest run: `84.65%` of executed — 32,775 pass / 5,944 fail / 8,546 skip.**
(Language **86.9%**, Built-ins **80.9%**.)

#### Language — 80.1% (17,686 / 22,089 executed)

| Category | Pass | Fail | Skip | Pass-rate |
|---|--:|--:|--:|:--|
| `expressions` | 8,380 | 2,143 | 515 | ████████░░ 80% |
| `statements` | 7,283 | 1,828 | 226 | ████████░░ 80% |
| `literals` | 411 | 40 | 83 | █████████░ 91% |
| `arguments-object` | 206 | 57 | 0 | ████████░░ 78% |
| `function-code` | 159 | 58 | 0 | ███████░░░ 73% |
| `block-scope` | 143 | 2 | 0 | ██████████ 99% |
| `types` | 99 | 12 | 2 | █████████░ 89% |
| `white-space` | 62 | 5 | 0 | █████████░ 93% |
| `computed-property-names` | 40 | 8 | 0 | ████████░░ 83% |
| `destructuring` | 15 | 4 | 0 | ████████░░ 79% |
| `rest-parameters` | 10 | 1 | 0 | █████████░ 91% |
| `directive-prologue` | 40 | 22 | 0 | ██████░░░░ 65% |
| `global-code` | 27 | 15 | 0 | ██████░░░░ 64% |
| `identifier-resolution` | 8 | 6 | 0 | ██████░░░░ 57% |
| `eval-code` | 142 | 200 | 5 | ████░░░░░░ 42% |
| lexical & syntax¹ | 660 | 0 | 1 | ██████████ 100% |
| `module-code` / `export` / `import` / `source-text` | 1 | 2 | 727 | _modules not yet supported_ |

¹ `asi`, `comments`, `identifiers`, `keywords`, `line-terminators`,
`punctuators`, `reserved-words`, `future-reserved-words`, `statementList`.

#### Built-ins — 76.6% (12,745 / 16,630 executed)

**Core objects & primitives**

| Category | Pass | Fail | Skip | Pass-rate |
|---|--:|--:|--:|:--|
| `Object` | 3,042 | 361 | 8 | █████████░ 89% |
| `Array` | 2,148 | 822 | 111 | ███████░░░ 72% |
| `String` | 1,050 | 163 | 10 | █████████░ 87% |
| `RegExp` | 834 | 692 | 353 | █████░░░░░ 55% |
| `Date` | 561 | 22 | 11 | ██████████ 96% |
| `Function` | 341 | 155 | 13 | ███████░░░ 69% |
| `Number` | 322 | 17 | 1 | ██████████ 95% |
| `Math` | 302 | 25 | 0 | █████████░ 92% |
| `JSON` | 115 | 27 | 23 | ████████░░ 81% |
| `BigInt` | 75 | 1 | 1 | ██████████ 99% |
| `Symbol` | 68 | 9 | 21 | █████████░ 88% |
| `NativeErrors` | 76 | 12 | 6 | █████████░ 86% |
| `Boolean` | 49 | 1 | 1 | ██████████ 98% |
| `Error` | 38 | 17 | 38 | ███████░░░ 69% |
| `AggregateError` | 21 | 3 | 1 | █████████░ 88% |

**Collections & iterators**

| Category | Pass | Fail | Skip | Pass-rate |
|---|--:|--:|--:|:--|
| `Set` | 332 | 49 | 2 | █████████░ 87% |
| `Map` | 144 | 25 | 35 | █████████░ 85% |
| `WeakSet` | 78 | 6 | 1 | █████████░ 93% |
| `WeakMap` | 92 | 9 | 40 | █████████░ 91% |
| `ArrayIteratorPrototype` | 23 | 4 | 0 | █████████░ 85% |
| `MapIteratorPrototype` | 10 | 1 | 0 | █████████░ 91% |
| `SetIteratorPrototype` | 10 | 1 | 0 | █████████░ 91% |
| `GeneratorPrototype` | 29 | 32 | 0 | █████░░░░░ 48% |
| `GeneratorFunction` | 7 | 14 | 2 | ███░░░░░░░ 33% |
| `StringIteratorPrototype` | 5 | 2 | 0 | ███████░░░ 71% |
| `RegExpStringIteratorPrototype` | 4 | 13 | 0 | ██░░░░░░░░ 24% |
| `Iterator` | 5 | 0 | 509 | _mostly helper proposals (skipped)_ |

**Typed arrays & binary data**

| Category | Pass | Fail | Skip | Pass-rate |
|---|--:|--:|--:|:--|
| `TypedArray` | 957 | 474 | 15 | ███████░░░ 67% |
| `TypedArrayConstructors` | 496 | 164 | 78 | ████████░░ 75% |
| `DataView` | 406 | 104 | 51 | ████████░░ 80% |
| `ArrayBuffer` | 67 | 115 | 39 | ████░░░░░░ 37% |

`TypedArrayConstructors` includes `Int8Array`…`Float64Array` plus the
`BigInt64Array`/`BigUint64Array` pair, which pass **100%**.

**Async, reflection & meta**

| Category | Pass | Fail | Skip | Pass-rate |
|---|--:|--:|--:|:--|
| `Reflect` | 143 | 10 | 0 | █████████░ 93% |
| `Proxy` | 193 | 80 | 38 | ███████░░░ 71% |
| `Promise` | 392 | 247 | 38 | ██████░░░░ 61% |
| `AsyncFunction` | 9 | 8 | 1 | █████░░░░░ 53% |
| `AsyncGeneratorPrototype` | 17 | 31 | 0 | ████░░░░░░ 35% |
| `AsyncGeneratorFunction` | 7 | 14 | 2 | ███░░░░░░░ 33% |
| `AsyncFromSyncIteratorPrototype` | 9 | 29 | 0 | ██░░░░░░░░ 24% |
| `AsyncIteratorPrototype` | 0 | 4 | 9 | ░░░░░░░░░░ 0% |

**Globals, coercion & URI**

| Category | Pass | Fail | Skip | Pass-rate |
|---|--:|--:|--:|:--|
| `parseFloat` / `isNaN` / `isFinite` | 84 | 0 | 0 | ██████████ 100% |
| `parseInt` | 53 | 2 | 0 | █████████░ 96% |
| `global` | 27 | 2 | 0 | █████████░ 93% |
| `eval` | 9 | 1 | 0 | █████████░ 90% |
| `encodeURI` / `encodeURIComponent` | 46 | 16 | 0 | ███████░░░ 74% |
| `decodeURI` / `decodeURIComponent` | 107 | 4 | 0 | ██████████ 96% |
| `NaN` / `Infinity` / `undefined` (value props) | 7 | 13 | 0 | ████░░░░░░ 35% |
| `ThrowTypeError` | 0 | 13 | 1 | ░░░░░░░░░░ 0% |

**Not yet implemented** (reported as `skip`, never hidden): ES modules
(`import`/`export`), `Temporal`, `Intl`, `Atomics` / `SharedArrayBuffer`,
`WeakRef` / `FinalizationRegistry` (intentionally unsupported under the
deterministic-replay contract), `ShadowRealm`, and the
`DisposableStack` / `SuppressedError` proposals.

**Reading the table.** Strong areas (≥ 85%) include `Object`, `String`, `Date`,
`Number`, `Math`, `BigInt`, `Set`/`Map`/`WeakMap`/`WeakSet`, `Reflect`, and the
typed-array constructors. The biggest open gaps are `RegExp` (Unicode property
escapes `\p{}` need Unicode tables), `ArrayBuffer` (resizable buffers), and the
async-iterator/generator surfaces — see the long-tail list in
[`docs/pure-rust-js-engine-plan.md`](./docs/pure-rust-js-engine-plan.md).

## 🏗 Architecture

```
┌─────────────────────────────────────────────────────┐
│   User code (.ts files, .jinja prompts, SDKs)        │
└────────────────────────┬────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────┐
│               Rust Core Runtime                      │
│                                                      │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │ TypeScript  │ │ Host Function│ │  Snapshot /  │  │
│  │  Runtime    │ │ Registry     │ │   Replay     │  │
│  └─────────────┘ └──────────────┘ └──────────────┘  │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │  LLM Client │ │  Template    │ │  HTTP Server │  │
│  │  (providers)│ │  (minijinja) │ │  (axum)      │  │
│  └─────────────┘ └──────────────┘ └──────────────┘  │
└──────────────────────────────────────────────────────┘
```

- **TypeScript runtime** transpiles `.ts` agents and exposes a deterministic `chidori` host API.
- **Host functions** are the only way agents touch the outside world.
- **Snapshot/checkpoint engine** records host calls and persists runtime metadata for resume.
- **LLM providers** (Anthropic, OpenAI, LiteLLM-compatible) are swappable via `reqwest`.
- **Template engine** uses `minijinja` for Jinja2 prompt templates.
- **HTTP server** (`axum`) powers the `serve` command and session API.

See [`DESIGN.md`](./DESIGN.md) for the full architecture and design rationale, and [`TODO.md`](./TODO.md) for the implementation roadmap.

## 📦 Project Structure

```
chidori/
├── src/
│   ├── main.rs             # CLI entry point
│   ├── server.rs           # HTTP server (serve + session API)
│   ├── runtime/
│   │   ├── engine.rs       # agent dispatch + runtime persistence
│   │   ├── typescript/     # TypeScript runtime, bindings, tools, transpile
│   │   ├── host_core.rs    # language-neutral durable host behavior
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
│   ├── agents/             # Example .ts agents
│   ├── prompts/            # Example .jinja templates
│   ├── tools/              # Example tools
│   ├── legacy-starlark/    # Archived .star examples
│   └── sdk_demo.py         # Python SDK demo
├── DESIGN.md               # Architecture & design rationale
├── TODO.md                 # Implementation roadmap
└── llm.txt                 # Complete API reference for LLM-assisted development
```
