# Trace Emitter — Design for Starlark-Level Instrumentation

**Status:** pre-implementation design doc.
**Companion doc:** `util-trace-webgl/docs/design-vision.md` — the viewer
that consumes the events this emitter produces. Read §3 (event schema)
and §4 (framework-readiness gap analysis) there first. This doc exists
because that §4 concluded the sub-expression events the viewer wants
require upstream work in this repo, and nobody has decided how.

This doc decides how.

---

## 1. Goal

Emit the full event schema specified in `util-trace-webgl/docs/design-vision.md
§3.1` from the running Starlark agent:

| Event kind | Meaning |
|---|---|
| `call_enter` / `call_exit` | Host-function invocation |
| `value` | Intermediate sub-expression (`2+3 → 5`) |
| `bind` | Variable assigned |
| `branch` | Conditional arm taken |
| `iter` | Loop iteration started/ended |
| `effect` | Side effect (log, stdout, mutation) |
| `error` | Raise/fail |

Every event carries `t`, `seq`, `run_id`, `span_id`, `parent`, `kind`,
`source: SourceRef`, `attrs`. Payload values are `ValueRef` — inline
for small primitives, remote `{id, preview, bytes}` for large payloads
persisted out-of-band.

The emitter must be:

- **Opt-in.** Gated by an env flag (`AGENT_TRACE=1`) so production
  agents pay zero cost when disabled.
- **Additive.** Does not alter existing `CallRecord`, checkpoint, or
  OTel behavior. Users who don't turn on tracing see no change.
- **Upgradable.** Call-level coverage ships first; sub-expression
  coverage lands later without reshaping the wire format.

---

## 2. What starlark-rust 0.13 actually offers us

Audited from `~/.cargo/registry/src/.../starlark-0.13.0/src/`:

### 2.1 The one hook we can use today

`Evaluator::before_stmt_for_dap()` — takes a
`BeforeStmtFunc<'a, 'e>` and calls it before each statement with a
`FileSpanRef` and `&mut Evaluator`. File reference:
`starlark-0.13.0/src/eval/runtime/evaluator.rs:436`.

```rust
// Source: starlark-0.13.0/src/eval/runtime/evaluator.rs:433-438
/// This function is used by DAP, and it is not public API.
// TODO(nga): pull DAP into the crate, and hide this function.
#[doc(hidden)]
pub fn before_stmt_for_dap(&mut self, f: BeforeStmtFunc<'a, 'e>) {
    self.before_stmt(f)
}
```

Two risk flags to name upfront:

- **`#[doc(hidden)]`.** The function is technically pub, practically
  unstable. The `TODO(nga)` says the crate owner intends to make it
  crate-private once DAP is internalized. On any starlark-rust bump
  that lands that TODO, we break.
- **Statement-granularity only.** The hook fires once per statement.
  It does **not** fire for sub-expression evaluations (`2+3`, list
  construction, comprehension body elements). Anything inside a
  single statement is opaque to this hook.

Both properties are load-bearing for the plan below.

### 2.2 What else exists (and doesn't help)

| API | Why it's not the answer |
|---|---|
| `Evaluator::enable_profile(ProfileMode::Statement)` | Writes timing to a file. No programmatic callback. Uses `before_stmt` internally — the raw hook is strictly more flexible. |
| `set_print_handler` / `set_soft_error_handler` | Intercepts `print()` and deprecation warnings. Useful for `effect` events from `print()`, nothing more. |
| `Evaluator::extra` | Host-state carry-through. This is how `RuntimeContext` already travels; we'll use it for the trace sink too. Not a hook — just a data slot. |
| AST (`AstModule`, `AstStmt`, `AstExpr` in `starlark_syntax-0.13.0`) | All public. `FileSpan` on every node. No Visitor trait — manual recursion required. Relevant for Option B below. |

### 2.3 What doesn't exist

- No `before_expr` / `after_expr` callback.
- No `on_bind(name, value)`.
- No `on_branch(cond, taken)` / `on_iter(loop, element)`.
- No `Debugger` trait.
- No public way to introspect the evaluator's value stack between
  instructions.

Those are the capabilities the Phase-I events (`value`, `bind`,
`branch`, `iter`) require at full fidelity. Upstream doesn't offer
them, so we need to either live without them, derive them, or add them.

### 2.4 How the framework wires starlark today

`src/runtime/engine.rs:241-268`:

```rust
// First pass: evaluate the module.
let mut eval = Evaluator::new(&module);
eval.extra = Some(&host_state);
eval.eval_module(ast, &globals)?;

// Second pass: call the agent function.
let mut eval2 = Evaluator::new(&module2);
eval2.extra = Some(&host_state);
let result = eval2.eval_function(agent_fn.value(), &[], &kwargs_refs);
```

Two `Evaluator` instances: one for module top-level, one for
`agent()`. Both receive `HostState` (which wraps `RuntimeContext`)
through `extra`. **Any hook we install must be installed on both
evaluators.** This is the single insertion point for all trace
plumbing.

---

## 3. Three options

We evaluated three paths. Each is a real, buildable plan; the
recommendation in §5 combines them.

### 3.1 Option A — `before_stmt` hook + source map

Install a `before_stmt_for_dap` callback on both evaluators. The
callback fires per statement with a `FileSpanRef`; use it to observe
control flow *at statement granularity only*. Complement with a
**source map** — a one-time AST walk at run-start that builds
`(file, byte_range) → statement kind, node metadata`. The callback
looks up the current statement in the map and decides what events to
emit.

**What we get:**

- `call_enter` / `call_exit` — already accurate from existing
  host-function records; fold them into the new event stream without
  needing the hook.
- `bind` — when the callback sees statement kind "assignment", it
  emits a `bind` event with the source span. The *value* is not
  directly accessible from `FileSpanRef` + `&mut Evaluator`, but the
  bound name is in the AST, and we can read back the value from
  `Evaluator::module().get(name)` on the *next* `before_stmt` firing
  (which is "after" for our purposes).
- `branch` — the AST map tells us a branch exists at line L. When
  the callback fires and `current_span` is inside the `then` arm,
  emit `branch { taken: "then" }`; when inside `else`, emit `{ taken:
  "else" }`. Guard for fire-at-most-once per entry.
- `iter` — the AST map identifies for-loops. On first callback
  inside the loop body, emit `iter { phase: "start", index: 0 }`.
  Detect loop re-entry by tracking the statement sequence: if we see
  the first-body-statement span twice in a row without a
  post-loop-span between, it's iteration N+1.
- `source` byte-range — **solved for every event** because the
  source map covers the whole module.
- `parent` — track call depth by maintaining a stack in
  `RuntimeContext`; each host-call push/pop updates it, each
  statement event inherits the current top.

**What we don't get:**

- `value` events for sub-expressions. `total = sum(x*r for x in xs)`
  emits *one* `bind` event, not one per multiplication. Internal
  values are invisible.
- Bind values where the callback can't observe the post-state
  (closures, non-local mutations inside a comprehension).

**Cost estimate:** ~800-1200 LOC in this repo. Breakdown:

- `src/runtime/trace/mod.rs` — event types, sink trait, event bus.
  ~200 LOC.
- `src/runtime/trace/source_map.rs` — AST walk + span→metadata map.
  ~300 LOC.
- `src/runtime/trace/stmt_hook.rs` — the `before_stmt` callback,
  state machine for branch/loop detection. ~300 LOC.
- `src/runtime/trace/emit.rs` — translate `CallRecord` into
  `call_enter` / `call_exit` + route to sink. ~150 LOC.
- `src/runtime/trace/sink.rs` — JSONL-to-disk sink, in-memory sink,
  SSE sink. ~200 LOC.
- Wiring in `engine.rs` and `context.rs` — ~100 LOC.
- Tests. ~300 LOC.

**Risk:** the `#[doc(hidden)]` concern (§2.1). Mitigation: pin the
`starlark` dependency to `=0.13.0` and gate bumps on re-verifying the
hook. Add a CI check that fails the build if `before_stmt_for_dap`
disappears. When the hook eventually is hidden, we'll have a fork
path ready anyway (Option C).

**Time:** 2-3 engineer-weeks.

### 3.2 Option B — AST reconstruction from the call log

No evaluator hook at all. At run-end, take the call log plus the
original source; walk the AST, simulating evaluation for the
**pure** parts (literals, binops on known values, name lookups) and
plugging in recorded values for the impure parts (host-call results).
Synthesize `value` / `bind` / `branch` / `iter` events post-hoc.

**What we get:**

- Sub-expression `value` events *only* for expressions whose operands
  are either literals or host-call results we observed. `2+3` we can
  trace; `x+y` where both came from earlier pure computations we can
  trace; `f(x)` where `f` is user-defined we *can't* without
  simulating `f`'s body.
- Covers simple cases well; degrades on comprehensions, lambdas,
  nested user-defined calls — exactly the shapes `kitchen_sink.star`
  exercises (§5 below).

**What we don't get:**

- Faithfulness. Any time user-defined code runs, we're guessing. A
  guess that diverges from what Starlark actually did is worse than
  no event — it silently lies to the viewer.
- Streaming. This is a post-hoc pass; no live events.

**Cost:** 1500-2500 LOC (a mini-Starlark evaluator for the pure
sublanguage). Time: 4-6 engineer-weeks.

**Verdict:** **Rejected** as a primary strategy. The fidelity
degradation is exactly backwards — it fails hardest on the cases
users most want to debug (complex comprehensions, lambda-heavy
code). Kept in reserve as a Phase-I fallback, gated on a config flag
that labels events as `synthetic: true` so the viewer can visually
distinguish them.

### 3.3 Option C — Fork starlark-rust, add expression hooks

Take a branch of `facebookexperimental/starlark-rust` at tag
`0.13.0`, add ~100-150 LOC of hook infrastructure, publish as
`starlark-with-trace` (or a path dep in our workspace).

**What we add:**

```rust
// starlark-rust/src/eval/runtime/evaluator.rs — new field:
pub(crate) trace_hook: Option<Box<dyn TraceHook + 'a>>,

// starlark-rust/src/eval/runtime/trace.rs — new module:
pub trait TraceHook {
    fn on_expr(&mut self, span: FileSpanRef, kind: ExprKind, result: Value) -> Result<()>;
    fn on_bind(&mut self, span: FileSpanRef, name: &str, value: Value) -> Result<()>;
    fn on_branch(&mut self, span: FileSpanRef, cond: Value, arm: &str) -> Result<()>;
    fn on_iter(&mut self, span: FileSpanRef, loop_span: FileSpanRef, index: u64, element: Value) -> Result<()>;
}
```

Insertion sites in the bytecode interpreter
(`src/eval/bc/instr_impl.rs`):

- Arithmetic opcodes (`BcInstr::Add`, `Sub`, `Mul`, `Div`, `Mod`,
  comparison ops, …) → `on_expr` with the ExprKind discriminator.
- Slot assignment (`BcInstr::AssignLocal`, `AssignSlot`) → `on_bind`.
- Branch (`BcInstr::JumpIf*`) → `on_branch` based on condition
  result.
- Loop entry (`BcInstr::ForLoopBegin`, `ForLoopIter`) → `on_iter`.
- Function call (`BcInstr::Call`, `CallFrozen`) — we already have
  call hooks from the host-function side, but fork gives us
  user-defined call observation too.

The hook is `Option<Box<dyn TraceHook>>`; when `None`, the branch
predicts out (zero cost). When `Some`, a virtual dispatch per
instrumented instruction.

**What we get:**

- The full §3.1 schema at genuine fidelity.
- Live streaming (the hook can forward to an `mpsc` channel).
- Source spans for every event (the bytecode instruction carries
  the span already; we pass it through).
- User-defined calls as `call_enter` / `call_exit` too, if we want
  — beyond the host-call-only coverage that exists today.

**Cost:**

- Fork + implement hooks: ~150 LOC upstream + ~50 LOC for the
  TraceHook impl on our side. ~1 engineer-week.
- Ongoing: rebase the fork on each starlark-rust release. Meta
  releases roughly quarterly; the delta is small enough that rebases
  should be mechanical. Budget: ~2 engineer-days per quarter.
- Perf: expected 15-25% slowdown on hot paths *with hook enabled*;
  zero overhead when disabled (the `Option<Box<dyn _>>::is_none`
  branch predicts trivially). Verified by enabling
  `ProfileMode::Statement` today — which uses the same hook style —
  and measuring.

**Risk:**

- Meta may accept an upstream PR instead, removing the fork. We
  should try — file the PR in parallel with shipping the fork.
- The fork diverges from upstream feature-set if Meta lands
  something major. Mitigation: keep the fork delta small.

**Verdict:** The **only** option that delivers the full schema at
full fidelity. We should commit to it, but stage it behind Option A.

---

## 4. Event coverage matrix

| Event kind | `CallRecord` alone (today) | Option A | Option B | Option C |
|---|---|---|---|---|
| `call_enter` / `call_exit` | ✅ full | ✅ full | ✅ full | ✅ full |
| `effect` (host `log()`/`print()`) | ✅ full | ✅ full | ✅ full | ✅ full |
| `error` (host-call raise) | ✅ full | ✅ full | ✅ full | ✅ full |
| `error` (Starlark fail/panic) | ❌ | 🟡 span only | 🟡 span only | ✅ full |
| `bind` | ❌ | ✅ full (name + value) | 🟡 names only, values guessed | ✅ full |
| `branch` | ❌ | ✅ arm taken, ✅ source | 🟡 guessed from call-log drift | ✅ arm + condition value |
| `iter` | ❌ | ✅ index + span | 🟡 guessed | ✅ index + element |
| `value` (sub-expr) | ❌ | ❌ | 🟡 pure expressions only | ✅ full |
| `source: SourceRef` | ❌ | ✅ all events | ✅ all events | ✅ all events |
| `parent` span stack | ❌ | ✅ tracked in RuntimeContext | ✅ | ✅ |
| Live streaming | ✅ (today, call-level) | ✅ | ❌ post-hoc | ✅ |

✅ full · 🟡 partial/degraded · ❌ not available

**Upshot:** Option A covers everything *except* sub-expression
`value` events. Option C adds those. B is not worth doing as a
primary path.

---

## 5. What `kitchen_sink.star` actually needs

The canonical fixture exercises (line numbers from
`examples/agents/kitchen_sink.star`):

- Comprehensions (list, dict, multi-clause) — lines 185-191.
- Lambdas — lines 279-280.
- Nested `def` — lines 339-349.
- Loops with `break`/`continue` — lines 353-360.
- `if`/`elif`/`else` and ternary — lines 84-93, 182.
- String formatting, dict/list construction, arithmetic — throughout.

**Option A coverage on this fixture:**
- 100% of statements observable.
- 100% of host calls (already covered).
- Branch/loop events fire correctly per statement-level AST walk.
- Bind events for every `x = ...` at statement scope.
- **Invisible:** anything inside `[x*2 for x in xs]`, inside a
  lambda body, or inside a nested inline expression. The bind on
  the result is visible; the intermediate multiplications are not.

For a first viewer ship, this is enough. The interesting
debugging cases ("why did my comprehension produce the wrong list")
are exactly the ones Option A can't handle — but the daily case
("what happened in what order in this agent run") it handles fully.

Decision: **Option A is the MVP; Option C is the target.**

---

## 6. Architecture

```
src/runtime/trace/
├── mod.rs          # Public API, Event type, feature gate
├── event.rs        # Event struct, EventKind enum, ValueRef
├── source_map.rs   # AstModule -> { span -> StmtMeta } index
├── stmt_hook.rs    # before_stmt callback + branch/loop state machine
├── emit.rs         # CallRecord <-> call_enter/call_exit adapter
├── value_ref.rs    # inline/remote value serialization + size threshold
├── stack.rs        # call-depth tracking for `parent` span
└── sink/
    ├── mod.rs      # TraceSink trait
    ├── jsonl.rs    # .app-agent/runs/{id}/events.jsonl writer
    ├── channel.rs  # tokio::mpsc sink for SSE/WS streaming
    └── null.rs     # zero-cost no-op sink (default when disabled)
```

### 6.1 Public API surface (inside `app-agent-framework`)

```rust
// src/runtime/trace/mod.rs

/// Install the trace hook on an Evaluator. No-op if tracing disabled.
pub fn attach<'a, 'e>(
    eval: &mut Evaluator<'_, 'a, 'e>,
    sink: Arc<dyn TraceSink>,
    source_map: Arc<SourceMap>,
    ctx: Arc<RuntimeContext>,
) -> Result<()>;

/// Build the source map once per run from the parsed agent module.
pub fn build_source_map(ast: &AstModule) -> Result<SourceMap>;

/// Pick a sink based on env/config.
pub fn sink_for(ctx: &RuntimeContext, run_dir: &Path) -> Arc<dyn TraceSink>;

pub trait TraceSink: Send + Sync {
    fn emit(&self, event: &Event);
}
```

### 6.2 Integration in `engine.rs`

```rust
// Inside run_agent(), after ast is parsed and before Evaluator::new:
let trace_enabled = env::var("AGENT_TRACE").is_ok();
let (sink, source_map) = if trace_enabled {
    let sm = Arc::new(trace::build_source_map(&ast)?);
    let sk = trace::sink_for(&ctx, &run_dir);
    (sk, sm)
} else {
    (trace::null_sink(), trace::empty_source_map())
};

// Module pass:
let mut eval = Evaluator::new(&module);
eval.extra = Some(&host_state);
if trace_enabled {
    trace::attach(&mut eval, sink.clone(), source_map.clone(), ctx.clone())?;
}
eval.eval_module(ast, &globals)?;

// Agent-call pass: same wiring with a fresh Evaluator.
```

This is ~20 lines of new code in `engine.rs`. Everything else lives
under `trace/` as its own module, deletable as a unit.

### 6.3 Source map representation

```rust
pub struct SourceMap {
    file: Arc<str>,
    // Every statement in the module, keyed by start byte.
    stmts: BTreeMap<usize, StmtMeta>,
    // Branch/loop structures: span range -> metadata.
    structures: Vec<Structure>,
}

pub struct StmtMeta {
    span: FileSpan,
    kind: StmtKind,       // Assign, Expr, If, For, Def, Return, ...
    bound_names: Vec<String>,  // names the stmt binds (for Assign)
    container: Option<ContainerRef>, // enclosing if/for, and which arm
}

pub enum Structure {
    Branch { span: FileSpan, then_range: Range<usize>, else_range: Option<Range<usize>> },
    Loop { span: FileSpan, body_range: Range<usize> },
    Function { span: FileSpan, body_range: Range<usize> },
}
```

Built by recursive descent on `AstModule::top_level` + statement
bodies. The recursion is ~200 LOC; no visitor trait needed.

### 6.4 Stmt hook state machine

The callback is `FnMut(FileSpanRef, &mut Evaluator)`. On each firing:

1. Look up `span.start_byte` in `source_map.stmts` — get `StmtMeta`.
2. **Deferred post-hooks for previous statement.** If the previous
   statement was an assignment, read back the bound names via
   `eval.module().get(name)` and emit `bind` with the now-current
   value. (We can't do this during the statement because we don't
   know when it's done; firing at the next statement is the natural
   "after" marker.)
3. **Structure transitions.** Walk from the previous `StmtMeta`'s
   `container` chain to the current one:
   - Entering a branch arm → emit `branch { taken: <arm> }`.
   - Entering a loop body → emit `iter { phase: "start", index: N }`
     (N tracked in a side-stack keyed on loop span).
   - Leaving a loop body → emit `iter { phase: "end" }`.
4. Record current statement as "previous" for the next firing.
5. On evaluator teardown, flush any pending post-hook for the final
   statement.

This is state-machine-heavy but bounded — ~300 LOC with tests.

### 6.5 ValueRef + large-value storage

```rust
pub enum ValueRef {
    Inline { repr: String },
    Remote { id: String, preview: String, bytes: u64 },
}

impl ValueRef {
    pub fn from_json(v: &serde_json::Value, run_dir: &Path) -> Self {
        let repr = v.to_string();
        if repr.len() <= INLINE_THRESHOLD {
            return Self::Inline { repr };
        }
        let id = blake3::hash(repr.as_bytes()).to_hex().to_string();
        let preview = repr.chars().take(120).collect();
        let bytes = repr.len() as u64;
        // Write-once: blake3 id is content-addressed.
        let path = run_dir.join("values").join(&id);
        if !path.exists() {
            std::fs::write(&path, &repr).ok();
        }
        Self::Remote { id, preview, bytes }
    }
}

const INLINE_THRESHOLD: usize = 4 * 1024; // 4 KB
```

Content-addressed by `blake3` — if the same large value is produced
twice (common: an LLM response referenced across multiple calls),
we store it once. The viewer fetches via the new
`GET /runs/:id/values/:value_id` endpoint.

### 6.6 Sinks

- **`NullSink`** — default when `AGENT_TRACE` unset. Zero-cost no-op.
- **`JsonlSink`** — appends one JSON event per line to
  `.app-agent/runs/{run_id}/events.jsonl`. Thread-safe via a single
  `Mutex<BufWriter<File>>`. Flushed on run end.
- **`ChannelSink`** — fans out to a `tokio::sync::mpsc` that the
  HTTP server subscribes to for `/runs/:id/stream`.
- **`TeeSink`** — composes the above so a live run can both persist
  and stream.

---

## 7. Wire format

```rust
#[derive(Serialize, Deserialize)]
pub struct Event {
    pub t: u64,                 // µs since run start
    pub seq: u64,               // monotonic
    pub run_id: String,
    pub span_id: String,        // "{run_id}:{seq}"
    pub parent: Option<String>,
    pub kind: EventKind,
    pub source: Option<SourceRef>,
    pub attrs: serde_json::Value,
}

pub struct SourceRef {
    pub file: String,
    pub start: usize,
    pub end: usize,
}

pub enum EventKind {
    CallEnter, CallExit,
    Value, Bind, Branch, Iter,
    Effect, Error,
}
```

Matches `util-trace-webgl/docs/design-vision.md §3.1` exactly.
`attrs` is kind-specific and matches the table in that section.

---

## 8. HTTP surface added to the framework

New endpoints in `src/server.rs`, scoped under `/runs`:

| Method | Path | Returns |
|---|---|---|
| `GET` | `/runs` | `[{ id, started_at, name, status }]` |
| `GET` | `/runs/:id/events` | The full `events.jsonl` as an array |
| `GET` | `/runs/:id/stream` | WebSocket of events as they're emitted (live runs only) |
| `GET` | `/runs/:id/source` | The frozen `.star` source for that run |
| `GET` | `/runs/:id/values/:value_id` | Raw large-value body |

Implementation: a new `src/server/runs.rs` module parallel to
session routes. Reads from `.app-agent/runs/{id}/` for static
endpoints; subscribes to `ChannelSink` broadcasts for `/stream`.

---

## 9. Performance

`AGENT_TRACE` unset → null sink → zero runtime cost. The hook is
never installed.

`AGENT_TRACE=1` with Option A hook installed:

- Statement callback fires once per statement — upstream already
  accepts this cost for `ProfileMode::Statement` users.
- Source-map lookup is `BTreeMap::get` on `usize` key — O(log n),
  fast.
- Event emission: `serde_json::to_writer` over a buffered writer.
  Expect ~1-3 µs per event.
- Total overhead estimate: **10-25% slower** than un-traced run,
  dominated by JSON serialization (can be optimized later with a
  binary sink).

Option C adds a bytecode-level virtual dispatch. Expect another
~5-10% on top when sub-expression hooks fire.

All benchmarks gated behind a `cargo bench` target TBD.

---

## 10. Phase plan

Maps onto `util-trace-webgl` phases (which were written with this
doc's options in mind):

| Framework phase | Delivers | Unblocks viewer phase |
|---|---|---|
| **F0** — Trace module skeleton, `null` + `jsonl` sinks, `AGENT_TRACE` flag, reshape `CallRecord` → `call_enter`/`call_exit` via `emit.rs` | Option A minus hook. File-drop-ready events. | Phase A (MVP) |
| **F1** — Run-scoped HTTP (`/runs`, `/runs/:id/events`, `/runs/:id/source`) + `run_id` on events | REST replay surface | Phase B, C |
| **F2** — Parent-stack tracking for `parent` span field | Nested call drill-in | Phase D |
| **F3** — Install `before_stmt_for_dap` hook + source map + state machine. Delivers `bind`, `branch`, `iter` events. | Full Option A | Phase H (source linking); partially unblocks I |
| **F4** — Large-value store + `/runs/:id/values/:id` endpoint + `ValueRef::Remote` | Large-payload debugging | Phase F |
| **F5** — `ChannelSink` + `/runs/:id/stream` WebSocket | Live mode | Phase G |
| **F6** — Fork starlark-rust, add `TraceHook`. Emit `value` events. | Full Option C | Phase I |

F0-F2 can land in weeks. F3 is the big middle phase (~2-3 weeks).
F4-F5 are small. F6 is the "when we commit to sub-expressions"
moment.

---

## 11. Alternatives considered and rejected

- **Use `ProfileMode::Statement` and parse its file output.**
  Cost: equivalent to F3. Benefit: avoids `#[doc(hidden)]`. Drawback:
  profile output is text-only, not programmatic; we'd be
  string-parsing the profiler's table. Also: still statement-level
  only, so no win over the hook. **Rejected.**

- **Replace starlark-rust with a different Starlark
  implementation** (bazel's Go implementation wrapped in a Rust
  binding, or writing our own). Cost: months. Benefit: full control
  over hooks. Drawback: loses the crate's maturity, every host
  function needs rewriting. **Rejected** unless starlark-rust goes
  unmaintained.

- **Do nothing; rely on OTel export.** Cost: zero. Benefit: users
  with Honeycomb/Tempo can visualize host calls there. Drawback:
  OTel attrs can't carry structured values, no sub-expression
  coverage ever. **Rejected** as a stopping point but kept as
  parallel export (the `otel.rs` module stays).

- **Emit everything through OTel spans** instead of our own wire
  format. Cost: moderate. Benefit: one format. Drawback: forces the
  viewer to speak OTel, and OTel's flat attr model is a bad fit
  (see `util-trace-webgl/docs/design-vision.md §3.5`). **Rejected.**

---

## 12. Open questions

1. **Hook stability.** When does `before_stmt_for_dap` get hidden?
   Mitigation plan in §3.1; the concrete answer needs a read of
   starlark-rust's issue tracker and maybe a question to `@nga`.
2. **Binding value read-back race.** On `before_stmt` firing for
   statement N+1, reading back the value bound in statement N
   assumes the module's public slot reflects it. Holds for
   module-scope bindings; does it hold for function-scope? Needs a
   test.
3. **Comprehension/lambda bodies.** Option A misses them entirely.
   Accept, or put them on a watchlist for Option C?
4. **Event buffering / backpressure.** If the sink is a channel and
   the consumer is slow, do we block the evaluator or drop events?
   Proposal: bounded channel with drop-oldest, plus a counter the
   viewer can surface ("312 events dropped").
5. **Value repr for non-JSON Starlark values** (sets, function
   values, frozen module refs). Fall back to Starlark's `repr()`;
   mark with a `repr: true` flag so the viewer doesn't try to
   JSON-parse.
6. **Env flag scope.** Does `AGENT_TRACE` apply to embedded uses of
   the framework (sandbox sub-processes, SDK callers)? Default
   proposal: yes — runtime library reads the flag once at run
   start.

---

## 13. Decision needed

**Green-light F0-F3 (Option A).** That's ~3 engineer-weeks and
gets the viewer from "useless without tracing" to "useful for the
daily debugging case." The commitment on the `#[doc(hidden)]` hook
is real but bounded — mitigation in §3.1 handles it.

**F6 (Option C fork)** is the larger decision: commit to
maintaining a fork of starlark-rust in exchange for the full schema.
Recommended to defer until users on F3's output tell us whether the
missing sub-expression coverage is the thing blocking them, or a
nice-to-have. Let usage data drive the fork decision.

---

## 14. References

- Companion doc: `util-trace-webgl/docs/design-vision.md` — the
  viewer that consumes these events. §3 for wire format, §4 for
  the gap analysis that motivated this doc, §8 for the viewer's
  phase plan mapping onto F0-F6 above.
- `starlark-rust` 0.13.0 source paths cited throughout. Key files:
  `src/eval/runtime/evaluator.rs` (the hook surface),
  `src/eval/runtime/before_stmt.rs` (the `BeforeStmtFunc` types),
  `starlark_syntax-0.13.0/src/syntax/ast.rs` and `.../module.rs`
  (the AST API we walk for the source map).
- `src/runtime/engine.rs:241-268` — the two `Evaluator` creation
  sites where the hook gets installed.
- `src/runtime/host_functions.rs` — the 37 host functions whose
  existing `CallRecord` emission we wrap into `call_enter` /
  `call_exit` events.
- `src/runtime/otel.rs` — pattern for env-flag-gated optional
  instrumentation; the trace module mirrors its shape.
- `examples/agents/kitchen_sink.star` — the coverage fixture
  (§5).
