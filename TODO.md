# TODO — App Agent Framework

## Status at a glance

| Phase | Area | Status |
|---|---|---|
| **1** | Rust core runtime, Starlark eval, host functions, CLI, templates, providers, tools | ✅ Working |
| **2a** | Session API, replay-based checkpointing, Python SDK, event-driven `serve` | ✅ Working |
| **2b** | WASM sandbox (`exec()`), `parallel()`, sub-agents, human-in-the-loop, retry/try_call | 🚧 Not started |
| **3** | Visual editor (Starlark AST ↔ node graph) | 🚧 Not started |
| **4** | Memory backends, observability/cost, ecosystem packages | 🚧 Partial |

See [README.md](./README.md) for what works today, [DESIGN.md](./DESIGN.md) for design rationale.

---

## Phase 1: Rust Core Runtime — ✅ DONE

### Project Setup
- [x] Initialize Rust project with `cargo init`
- [x] Single-crate structure with modules: `runtime`, `providers`, `tools`, `server`
- [x] Core dependencies: `starlark`, `tokio`, `clap`, `serde`, `reqwest`, `minijinja`, `tracing`, `axum`
- [ ] Set up CI (build + clippy + tests)
- [ ] Clean up dead-code warnings (unused methods flagged by compiler)

### Starlark Evaluator
- [x] Integrate `starlark-rust` 0.13
- [x] `.star` file loading and parsing
- [x] `agent()` function discovery and invocation with keyword args
- [x] `config()` built-in for module-scope agent configuration
- [x] Wire up `--input` CLI args as `agent()` function parameters
- [x] Error reporting: Starlark traceback with line numbers propagates through to CLI/HTTP

### Host Functions — `#[starlark_module]`
- [x] `prompt()` — send text to LLM, return string or parsed JSON
  - [x] `model`, `temperature`, `max_tokens`, `system`, `format` kwargs
  - [x] `format="json"` parses response as dict/list (strips markdown fences)
  - [x] `tools` kwarg for LLM function-calling (tool-use loop)
  - [x] `max_turns` kwarg for autonomous tool-use iteration limit
- [x] `template()` — render Jinja2 inline string or `.jinja` file
- [x] `config()` — agent-level defaults
- [x] `env()` — read environment variables
- [x] `log()` — structured logging
- [x] `tool()` — invoke a registered tool by name
- [x] `http()` — make HTTP requests with headers/body/params
- [x] `memory()` — persistent key-value (JSON file backend). Vector storage still TODO.
- [x] `exec()` — run WebAssembly in a bounded wasmer sandbox (Phase 2b)
- [x] `call_agent()` — call sub-agents (renamed from `agent()` to avoid shadowing user's own `def agent(...)`)
- [ ] `parallel()` — concurrent execution (Phase 2b)
- [x] `input()` — human-in-the-loop (Phase 2b)
- [x] `retry()` — retry with backoff (Phase 2b)
- [x] `try_call()` — capture errors (Phase 2b)

### Template Engine
- [x] `template()` host function — inline Jinja2 rendering via minijinja
- [x] Template file loading resolves paths relative to project base directory
- [x] Template includes and inheritance (`{% extends %}`, `{% include %}`)
- [x] Built-in filters work out of the box (`tojson`, `upper`, `join`, etc.)
- [x] Trim/lstrip blocks enabled for cleaner output
- [ ] Prompt directory auto-discovery from `prompts/` (currently requires explicit path)

### LLM Provider Clients
- [x] Anthropic Messages API client (`reqwest`)
- [x] OpenAI Chat Completions API client
- [x] LiteLLM / OpenAI-compatible catch-all provider via `LITELLM_API_URL` + `LITELLM_API_KEY`
- [x] Model routing: resolve model name → provider (Anthropic for `claude*`, OpenAI for `gpt*`/`o1`/`o3`, LiteLLM as catch-all)
- [x] API key resolution from environment variables
- [ ] Streaming support (SSE parsing) for both providers — token-level streaming from provider back through Starlark. Blocked on async-from-sync plumbing; per-call streaming via `/sessions/stream` is shipped as a middle ground.
- [x] Tool use / function calling request/response handling (Anthropic + OpenAI)
- [ ] Provider configuration loading from `config/providers.star` (only env vars today)

### Tool System
- [x] Tool registry: load `.star` files from `tools/` directory
- [x] Parse tool function signatures and docstrings → JSON schema
- [x] `app-agent tools --dir <path>` lists discovered tools with params
- [x] Make tools available to `tool()` host function (tools can transitively call host fns)
- [x] Make tools available to `prompt()` via `tools` kwarg (LLM function-calling)
- [ ] Inline tool definition support (tools defined within agent `.star` files)

### CLI
- [x] `app-agent run <file.star>` — run an agent with `--input` args
- [x] `app-agent check <file.star>` — parse and validate without executing
- [x] `app-agent tools` — list discovered tools with signatures
- [x] `app-agent serve <file.star>` — run as HTTP server (see Phase 2a)
- [x] `--trace` flag outputs call log as JSON
- [x] `--verbose` flag enables structured tracing to stderr
- [x] `--input key=value`, `--input key=@file.txt`, `--input '{"json": "object"}'` all work
- [x] `app-agent resume <file> <run-id>` — resume from a disk checkpoint
- [x] `app-agent trace <run-id>` — pretty-print a saved trace
- [x] `app-agent stats` — aggregate tokens + cost across runs
- [x] Exit codes: 0 success, 1 agent/runtime error, 2 parse/config error (check-only)

### Tracing & Call Log
- [x] Every host function call logged with name, args, result, duration, token usage, timestamp
- [x] Call log serialized as structured JSON
- [x] Token usage summary at end of run (`--trace`)
- [x] Tests passing: `cargo test` (5 unit tests on template + JSON extraction)

---

## Phase 2a: Sessions, Replay, Python SDK — ✅ DONE

### HTTP Server (`serve`)
- [x] `app-agent serve <file.star> --port 8080`
- [x] Built on `axum`
- [x] Each request runs agent on a blocking thread with its own tokio runtime (avoids nested-runtime panic)
- [x] Health endpoint: `GET /health`
- [x] Fallback handler: any request → `agent(event)` as a structured event dict
- [x] Event dict shape: `method`, `path`, `headers`, `query`, `body` (body auto-parsed as JSON)
- [x] Agent response mapping: `{status, headers, body}` dict → HTTP response, or any value → 200 JSON

### Session API
- [x] `POST /sessions` — create a session, run the agent, return result + call log
- [x] `GET /sessions` — list all sessions
- [x] `GET /sessions/{id}` — get session result
- [x] `GET /sessions/{id}/checkpoint` — get the full call log
- [x] `POST /sessions/{id}/replay` — replay from a session's own checkpoint
- [x] `POST /sessions` with `replay_from` field — replay from an arbitrary call log
- [x] UUID-based session IDs
- [x] In-memory session storage

### Replay-Based Checkpointing
- [x] `RuntimeContext::with_replay(call_log)` — pre-load call log for replay mode
- [x] `Engine::run_with_replay()` — replay entry point
- [x] `prompt()` checks `ctx.try_replay(seq)` before making live calls
- [x] Cached results returned instantly (milliseconds instead of seconds)
- [x] Output parity: replayed sessions produce byte-identical output to originals
- [x] Verified end-to-end: 3 live sessions, saved checkpoints, replayed all 3 with matching output
- [x] Persist call log to disk after each host function call (`.app-agent/runs/<id>/checkpoint.json`)
- [x] Divergence detection: replay errors cleanly when the host-function sequence at a given seq doesn't match the checkpoint (e.g. agent code changed). `log()` also participates so logging edits don't silently skew the stream.
- [ ] Checkpoint storage directory (default `.app-agent/runs/`)
- [ ] Partial replay: fast-forward to N, execute from N+1 onward (works but not exposed as first-class API)

### Python SDK
- [x] `sdk/python/app_agent/` — pure stdlib, no dependencies
- [x] `AgentClient` — HTTP client for the server
- [x] `Session` — dataclass with status, output, error, call log
- [x] `Checkpoint` — save/load to JSON files
- [x] `client.run(input)` — create and run a session
- [x] `client.replay(checkpoint)` — replay from a checkpoint
- [x] `client.list_sessions()`, `client.get_session(id)`
- [x] `session.checkpoint().save(path)` / `Checkpoint.load(path)`
- [x] Working demo: [`examples/sdk_demo.py`](./examples/sdk_demo.py)
- [x] SDK packaging (pyproject.toml for `pip install -e ./sdk/python`)
- [x] TypeScript SDK (mirror the Python one) — `sdk/typescript/`, zero runtime deps, uses global `fetch`. Ships `AgentClient` with `run`, `replay`, `resume`, `getSession`, `listSessions`, `getCheckpoint`, and a streaming `stream(input)` async generator that parses SSE from `POST /sessions/stream`. `tsc --noEmit` clean; verified end-to-end against a live server.
- [x] **SDK streaming** — both TS and Python SDKs now expose `client.stream(input)` over `POST /sessions/stream`. TS SDK uses a `TextDecoder` + `ReadableStream` reader + async generator; Python SDK is a plain generator using `urllib`'s line iterator with a small stdlib-only SSE frame parser (no sseclient dep). Python SDK is now at full API parity with TS: `health`, `run`, `replay`, `resume`, `get_session`, `list_sessions`, `get_checkpoint`, `stream`, plus `Session.checkpoint`/`replay`/`ok` and the `pending_seq`/`pending_prompt` pause fields. Verified by `test_stream_emits_call_then_done` in the integration suite.

---

## Phase 2b: WASM Sandbox & Advanced Composition — 🚧 NOT STARTED

### WASM Sandbox (`exec()`)
- [x] Add `wasmer` crate dependency (+ `wasmer-middlewares` for metering)
- [x] `exec()` host function: accept WAT/WASM source, function name, args, fuel, memory_pages
- [x] Fuel-based timeout enforcement (Cranelift + metering middleware; every operator costs 1)
- [x] Bounded linear memory via custom `CappedTunables` (caps any declared memory at `memory_pages` * 64 KiB)
- [x] Numeric (i32/i64/f32/f64) args and returns, serialized through JSON for the call log + replay
- [x] Replay support: `exec()` participates in the replay cache and divergence detection
- [x] **Language layer v2 (infix miniscript):** `sandbox-runtime/` grew from a 3.3 KB postfix calculator into a 20 KB recursive-descent interpreter with `let/in`, `if/then/else`, integers + booleans, `+ - * / %`, comparisons, `&& || !`, line comments. `exec_expr(source, vars={}, fuel=…)` passes host-supplied vars by prepending `let name = value in …` chains. Runs under wasmer with Cranelift+metering fuel limits (32 linear-memory pages to fit the Rust-compiled guest's stack + 256 KiB bump heap).
- [x] **Host-function bridge:** sandboxed modules can import `host.log(ptr, len)` and the host reads guest linear memory, decodes UTF-8, and forwards to `tracing::info!` / user callback. Exercised by both a unit test and the agent-level `wasm_demo.star`.
- [x] WASM module caching: `(source_hash, memory_pages) → (Engine, Module)` in a global `Mutex<HashMap>`. Fuel is reset per-call via `set_remaining_points`, so the cached artifact works across calls with different budgets.
- [x] **Python language layer via RustPython.** New `sandbox-python/` subcrate compiles `rustpython-vm` (compiler feature, no wasmbind/stdio) to `wasm32-wasip1`, producing a 7.6 MB WASI binary embedded via `include_bytes!`. Host exposes `exec_python(source, fuel=…)` that runs real Python 3: defs, recursion, comprehensions, exceptions, string ops. The program assigns its answer to a top-level `result` variable and the sandbox returns `repr(result)`.
- [x] **JavaScript language layer via Boa.** New `sandbox-js/` subcrate compiles `boa_engine` (default features off — no float16/xsum/intl/temporal/wasm-bindgen) to `wasm32-wasip1`, producing a 3.4 MB WASI binary. Host exposes `exec_js(source, fuel=…)` that runs real JS: arrow functions, recursion, array methods (`map`/`reduce`/`Array.from`), template literals, `throw`. Final-expression semantics — returns `String(value)` of the last expression. **Cold 6.2s → warm 0.22s** with the on-disk Module cache. Reuses the same WASI preview 1 shim as Python; Boa only imports 9 of the 18 WASI functions (no fd/path operations), so the shim is already a superset.
- [x] **Shared `run_wasi_guest` helper** factors out the boilerplate (artifact load, store, env, 18 imports, memory wiring, fuel, `_start`, proc_exit/fuel/trap demux, `ERR:` prefix handling) so `exec_python` and `exec_js` are each a one-liner delegating to the helper with their own (binary, memory_pages, label) triple.
- [x] **WASI preview 1 shim (hand-rolled).** Rather than pulling in `wasmer-wasix` (which drags reqwest, tokio networking, virtual-fs, webc, and ~100 more crates), `src/runtime/sandbox.rs` implements 18 WASI preview-1 functions directly on top of the existing wasmer `Function::new_typed_with_env` machinery: `args_*`, `environ_*`, `clock_time_get` (fixed for determinism), `fd_close`, `fd_fdstat_get`, `fd_read` (stdin preloaded with source), `fd_write` (fd 1 → stdout capture buffer, fd 2 → stderr), `fd_filestat_get`/`fd_prestat_*`/`path_*` (return errors — zero preopens), `poll_oneoff` (NOTSUP), `proc_exit` (raises a `ProcExit` user error the orchestrator distinguishes from real traps), `random_get` (xorshift64 with fixed seed for determinism), `sched_yield` (success noop).
- [x] **On-disk compiled-artifact cache.** `load_or_compile` now walks in-memory cache → disk cache (`.app-agent/wasm-cache/v01-<key>.cwasm`) → fresh compile, and persists newly-compiled Modules via `Module::serialize` (~55 MB for RustPython). Disk load uses the `unsafe Module::deserialize` path, which we justify with: files live under our own private cache dir, filenames include a `DISK_CACHE_VERSION` tag that bumps on compiler/tunables/wasmer changes, and a corrupt file falls through to a fresh compile. **Measured end-to-end on `python_sandbox_demo.star`: 18.7s cold (Cranelift + disk serialize), 0.5s warm (deserialize) — ~37× speedup.**
- [ ] **Follow-up: fuel budget for Python.** Python tests run at 200 M instructions; a tighter language-specific default + a per-op multiplier (parsing dominates) is a nice-to-have.

### Parallel Execution
- [x] `parallel()` host function accepting a list of lambdas (sequential today — see follow-ups)
- [ ] **Follow-up: true concurrency.** Today each branch runs sequentially because the Starlark evaluator is single-threaded and the lambdas are bound to the parent evaluator's heap — they can't cross thread boundaries. Real parallelism needs one of:
  - (a) Run each branch in its own `Module` + `Evaluator` on a dedicated thread, serializing arguments/results across the boundary (requires re-parsing the branch's body or restructuring `parallel()` to take a spec like `parallel([{"kind": "prompt", ...}, ...])` instead of lambdas).
  - (b) Fan out at the host-call level only: detect when a branch is a one-shot `prompt()`/`tool()`/`http()` and issue those concurrently while keeping Starlark itself sequential. Narrower but preserves the lambda API.
  - (c) Wait for upstream `starlark-rust` to grow a thread-safe evaluator story.
- [ ] Each parallel branch gets its own call log sequence range
- [ ] Merge parallel branch logs into the main log on completion
- [ ] Error handling: propagate first error (or collect all)
- [ ] Replay support: parallel branches replay deterministically (works today only because execution is sequential)

### Agent Composition
- [x] `call_agent()` host function: resolve path → `.star` file and invoke
- [x] Execute sub-agent with its own evaluation context
- [x] Sub-agent calls flow into the parent's call log (flat, not nested — simpler for replay)
- [x] Sub-agents inherit parent's provider config (shared HostState)
- [ ] Expose sub-agents as tools for LLM function-calling

### Human-in-the-Loop
- [x] `input()` host function
- [x] Interactive mode (CLI `run` command): reads one line from stdin
- [x] Server mode: `input()` suspends, session goes to `paused` with pending prompt
- [x] `POST /sessions/{id}/resume` endpoint with user response → injects response into call log and replays
- [ ] Timeout handling: fail or use default after N seconds

### Error Handling
- [x] `retry()` host function with `max_attempts` + `backoff` strategies (`constant`, `linear`, `exponential`)
- [x] `try_call()` host function — execute lambda, return `{value, error}` struct
- [ ] Global `on_error` config: `stop` (default), `skip`, `retry`

---

## Phase 3: Visual Editor — ❌ DESCOPED

Not planned. Keep the roadmap below for reference only; don't schedule this
work until the decision is revisited.


### Editor Core
- [ ] Web app scaffolding (React / TypeScript)
- [ ] Integrate node graph library (React Flow / xyflow)
- [ ] Canvas: pan, zoom, selection, undo/redo
- [ ] Node palette with draggable node types
- [ ] Properties panel for editing selected nodes
- [ ] Wire drawing with type validation

### Starlark AST ↔ Node Graph

#### Code → Visual (Parser)
- [ ] Parse `.star` file into AST (via `starlark-rust` exposed as a WASM module or server endpoint)
- [ ] Walk AST: `agent()` params → Input nodes
- [ ] Walk AST: host function calls → Action nodes
- [ ] Walk AST: variable assignments → wires
- [ ] Walk AST: `if`/`else` → Branch nodes
- [ ] Walk AST: `for` loops → Loop nodes
- [ ] Walk AST: `parallel()` → Parallel container nodes
- [ ] Walk AST: `return` → Output node
- [ ] Unrecognized expressions → Transform nodes (escape hatch)
- [ ] Layout: topological sort, left-to-right

#### Visual → Code (Generator)
- [ ] AST builder: each node type → Starlark AST fragment
- [ ] Wire resolution: infer variable names from port connections
- [ ] Pretty-printer with canonical Starlark formatting
- [ ] Comment preservation via node metadata
- [ ] Transform nodes emit raw code as-is

#### Round-trip Validation
- [ ] Test: visual → code → visual equals original graph
- [ ] Test: hand-written `.star` → visual → code is semantically equivalent
- [ ] Test: complex expressions survive round-trip via Transform nodes

### Node Types
- [ ] Prompt, Template, Tool, Exec, Sub-agent, HTTP, Human Input, Memory
- [ ] Branch, Loop, Parallel, Retry, Try
- [ ] Input, Output, Transform, Constant

### Code Panel
- [ ] Read-only `.star` code view that live-updates as canvas changes
- [ ] Starlark syntax highlighting
- [ ] Click-to-select: line ↔ node bidirectional

### File Operations
- [ ] Open `.star` → parse to visual
- [ ] Save canvas → generate `.star`
- [ ] Watch file for external changes
- [ ] New agent from blank canvas

### Editor Server (`edit`)
- [ ] `app-agent edit <file.star> --port 3000`
- [ ] Serve editor static assets from embedded binary
- [ ] WebSocket sync between canvas and `.star` file
- [ ] Live preview: mock execution reusing the replay mechanism with synthetic results

---

## Phase 4: Production & Ecosystem — 🚧 PARTIAL

### HTTP Server polish
- [x] Basic serve command working
- [x] Session API working
- [x] SSE streaming: `POST /sessions/stream` emits `event: call` per host function and `event: done` with the final output. Note: streams *per-call*, not per-token (token-level streaming from providers would require async-through-Starlark plumbing — tracked as a follow-up).
- [x] **Concurrent session limits + queueing.** `AppState` grew a `run_semaphore: Arc<Semaphore>` (size from `APP_AGENT_MAX_CONCURRENT_SESSIONS`, default 8) plus an `acquire_timeout` (from `APP_AGENT_ACQUIRE_TIMEOUT_MS`, default 30 000). `create_session` and `stream_session` `acquire_owned` before running and hold the permit for the entire blocking agent run. Saturated requests wait up to the timeout, then return **503** with `Retry-After: 1` and a JSON body `{error: "server busy…", acquire_timeout_ms: N}`. Verified against a slow sleep-agent: max=2 / timeout=200ms / 1s agent → 3rd and 4th concurrent requests hit 503 cleanly.
- [x] **Auth middleware.** `axum::middleware::from_fn(auth_middleware)` gated on the `APP_AGENT_API_KEY` env var. Default-off (local dev unchanged); when set, every route except `/health` requires `Authorization: Bearer $APP_AGENT_API_KEY`. Mismatches return **401** with `WWW-Authenticate: Bearer` and a JSON error body.
- [x] **CORS layer.** `build_cors_layer()` reads `APP_AGENT_CORS_ORIGINS`: unset → no CORS headers (same-origin only), `*` → permissive with `Any` origin/methods/headers, comma-separated list → explicit allow-list. Sits in the router as a `tower_http::cors::CorsLayer`.
- [x] Startup banner now prints the active concurrency cap, auth state, and CORS config so ops can verify settings without inspecting env vars.

### Memory Backends
- [x] `memory()` host function (get/set/delete/list/clear)
- [x] JSON-file backend (one file per namespace under `.app-agent/memory/`)
- [x] Memory namespace isolation per agent (via `namespace=` kwarg)
- [x] Goes through replay cache for deterministic replays
- [ ] SQLite backend for better concurrency / larger datasets
- [ ] Vector search backend (Qdrant or in-memory FAISS-like)
- [ ] Embedding model integration (OpenAI text-embedding-3-small, Voyage)

### Observability & Cost
- [x] Per-run token usage tracking (input + output counts)
- [x] Per-run duration tracking
- [x] Cost estimation based on model pricing (`src/runtime/cost.rs`, surfaced in `--trace`)
- [x] Rate limiting: configurable requests-per-minute per provider via `APP_AGENT_{ANTHROPIC,OPENAI,LITELLM}_RPM` env vars (async token bucket)
- [x] `app-agent stats` — aggregate usage across runs (reads `.app-agent/runs/*/checkpoint.json`)
- [ ] Dashboard: web UI for viewing traces, run history, cost breakdown

### Local Model Support
- [x] OpenAI-compatible endpoint support via `LITELLM_API_URL` (works with LM Studio, vLLM, Ollama's OpenAI mode)
- [ ] Dedicated Ollama provider client (native `/api/generate` endpoint)

### Ecosystem Packages
- [x] `web_search` tool — Tavily backend, pure Starlark tool using `http()`, `TAVILY_API_KEY` from env. Returns `{query, answer, results: [{title, url, snippet, score}]}`. Brave/SerpAPI backends still open.
- [x] `fetch_url` tool — HTTP fetch + cheap HTML-to-text pass (script/style stripping, tag removal, whitespace collapse). Not a real readability parser; good enough to feed page content into an LLM prompt. Lives at `examples/tools/fetch_url.star`; use with `app-agent run … --tools examples/tools`.
- [x] `read_file`, `write_file`, `list_dir` as first-class host functions (not tools). Sandboxing (restrict to project base) is a follow-up.
- [x] `shell()` **host function** — whitelisted by `APP_AGENT_SHELL_ALLOW` env var (default-closed; `*` for "allow anything"). Uses tokio's `Command` with `kill_on_drop` + `tokio::time::timeout` for cancellation; basename-matched so `/usr/bin/ls` still resolves to `ls` in the allow list; empty env by default (no PATH / AWS_* leakage) — caller opts in via `env={...}`. Returns `{stdout, stderr, exit_code, timed_out}`. Participates in replay cache + divergence detection. Verified end-to-end (success, listing, whitelist block, timeout) plus 3 unit tests on the whitelist parser.
- [ ] `database` tool (SQL query + read-only mode)
- [ ] Agent registry: publish/share `.star` agents

### Developer Tooling
- [ ] VS Code extension: `.star` syntax highlighting (Starlark dialect variant)
- [ ] VS Code extension: `.jinja` prompt template preview
- [ ] VS Code extension: run agent + debug from editor
- [ ] LSP server for Starlark completion (leveraging `starlark-rust`'s LSP features)
- [ ] Pre-commit hook: `app-agent check` on changed `.star` files

---

## Known Issues

- [ ] Dead-code warnings: `ToolDef.source`, `ToolDef.source_path`, `ToolRegistry.get`, `CallLog.records`, etc. flagged as unused — should either be wired up or removed
- [ ] Anthropic provider's `content_type` field is unused — either use it to filter content blocks or remove
- [ ] `ProviderRegistry` is re-created per request inside `spawn_blocking` because `reqwest::Client` is bound to its owning tokio runtime. Cheap but wasteful — consider a `reqwest::blocking::Client` alternative or pooling.
- [x] `parse_tool_file` in `tools/mod.rs` now matches on `def <filename_stem>(...)` instead of grabbing the first `def`, so files with private helper defs (e.g. `_strip_tags`) work correctly. A full Starlark AST walk is still a later upgrade.
- [x] **Session API integration tests** — `sdk/python/tests/test_session_api.py`, 13 tests covering run/checkpoint/replay/list, pause+resume via `input()`, auth middleware (401 missing / 401 wrong / 200 correct / health open), concurrency semaphore 503 saturation, and CORS preflight. Each class starts its own `app-agent serve` subprocess with a fresh env (auth / concurrency / cors configs isolated). A stdlib-only `MockLlm` HTTP server sits in front of the runtime via `LITELLM_API_URL` so no real provider traffic happens; hit counts assert that replay does NOT re-call the LLM. Discovered and fixed a real gap while writing: Python SDK was missing `resume()` and `pending_prompt` on `Session` (TS SDK already had it).
- [ ] `config()` silently ignores unknown keys — should warn or error.
