# Architecture & project structure

A high-level map of the runtime. For the full design rationale, durable VM
snapshot model, and roadmap, see [`DESIGN.md`](./DESIGN.md) and
[`TODO.md`](./TODO.md).

## Architecture

```
┌─────────────────────────────────────────────────────┐
│   User code (.ts files, .jinja prompts, SDKs)        │
└────────────────────────┬────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────┐
│               Rust Core Runtime                      │
│                                                      │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │ TypeScript  │ │ Host Function│ │  Call log /  │  │
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
- **Call-log / replay engine** records every host call and replays the journal for deterministic, zero-LLM-call resume.
- **LLM providers** (Anthropic, OpenAI, LiteLLM-compatible) are swappable via `reqwest`.
- **Template engine** uses `minijinja` for Jinja2 prompt templates.
- **HTTP server** (`axum`) powers the `serve` command and session API.

Agents run on Chidori's embedded **pure-Rust JavaScript engine**
(`crates/chidori-js`, oxc parser → bytecode → stack VM, zero `unsafe`, no C) —
the only JS engine in the tree. Its language conformance is gated against
Test262; see [`docs/conformance.md`](./conformance.md).

## Project structure

```
chidori/
├── crates/
│   ├── chidori/            # The `chidori` CLI crate (runtime, server, providers)
│   │   ├── src/
│   │   │   ├── main.rs         # CLI entry point
│   │   │   ├── server.rs       # HTTP server (serve + session API)
│   │   │   ├── runtime/
│   │   │   │   ├── engine.rs       # agent dispatch + runtime persistence
│   │   │   │   ├── typescript/     # TypeScript runtime, bindings, tools, transpile
│   │   │   │   ├── host_core.rs    # language-neutral durable host behavior
│   │   │   │   ├── context.rs      # Runtime context (call log + replay)
│   │   │   │   ├── call_log.rs     # Checkpoint data structures
│   │   │   │   └── template.rs     # minijinja integration
│   │   │   ├── providers/
│   │   │   │   ├── mod.rs          # Provider registry, model routing
│   │   │   │   ├── anthropic.rs    # Anthropic Messages API
│   │   │   │   └── openai.rs       # OpenAI-compatible (incl. LiteLLM)
│   │   │   └── tools/
│   │   │       └── mod.rs          # Tool discovery + JSON schema generation
│   │   └── tests/             # CLI integration tests
│   ├── chidori-js/         # Pure-Rust JS engine (oxc → bytecode → VM), the only engine
│   └── test262-runner/     # Test262 conformance harness + baseline gate
├── sdk/
│   ├── typescript/         # TypeScript SDK (zero-dependency HTTP client)
│   └── python/chidori/     # Python SDK (pure stdlib, no deps)
├── examples/
│   ├── agents/             # Example .ts agents
│   ├── prompts/            # Example .jinja templates
│   ├── tools/              # Example tools
│   └── sdk_demo.py         # Python SDK demo
├── docs/
│   ├── DESIGN.md           # Architecture & design rationale
│   └── TODO.md             # Implementation roadmap
└── llm.txt                 # Complete API reference for LLM-assisted development
```
