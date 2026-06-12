# Pure-Rust JS Engine + Replay-Based Durable Execution — Implementation Plan

> **⚠️ Historical — superseded.** This plan tracked building the pure-Rust engine
> *alongside* QuickJS behind a `rust-engine` cargo feature. That migration is
> **complete**: QuickJS was removed in #39, the feature flag is gone, and
> `chidori-js` is now the **only** JS engine in the tree (the default and only
> path). The "default OFF / QuickJS untouched" framing below is historical. For
> the current engine and conformance story see
> [`docs/conformance.md`](./conformance.md).

Status: **implemented · migration complete** · Last updated: 2026-05-31

## Implementation status

The engine and replay runtime are built in `crates/chidori-js`. It began behind
a `rust-engine` cargo feature (default OFF, alongside the QuickJS/C path); as of
#39 that feature is removed and `chidori-js` is the sole engine.

| Phase | Scope | Status |
| --- | --- | --- |
| **P0** | Seam & scaffold, toggle, runner param | ✅ crate scaffolded; `SnapshotCapableJsEngine` adapter (`src/runtime/rust_engine.rs`); `CHIDORI_JS_ENGINE` toggle; `test262-runner --engine rust\|quickjs`. *Deferred:* the behavior-preserving rewrite of the 33 C `extern "C"` callbacks behind a shared `HostFn` (the Rust path has its own clean seam in `host.rs`; migrating the C callbacks is mechanical follow-up gated by the existing tests). |
| **P1** | Run-to-completion core | ✅ oxc AST → bytecode → stack VM; object model (shapes/prototypes, deterministic key order); Rc-based GC; core builtins. Tests: `tests/smoke.rs` (arithmetic, scope, closures incl. closure-in-loop, classes+`extends`+`super`, destructuring, exceptions, JSON, spread). |
| **P2** | Async + host boundary | ✅ Promises + combinators, async/await, generators (incl. `.next(v)`/`yield*`), microtask queue, `run_jobs_until_blocked`, host-op create/resolve/reject, `BlockedOnHostOperation`. Tests: `tests/async_gen.rs`. |
| **P3** | Replay durability | ✅ journal format + deterministic host-op keying + restore-by-replay. Tests: `tests/replay.rs` (record→replay identity; suspend→persist→restore→resume). |
| **P4** | Modify-and-resume | ✅ forward-edit resume works end-to-end; edit-conflict policy = **fail-loud** divergence detection via ordered journal consumption. Tests: `tests/replay.rs`. |
| **P5** | Conformance & builtins long tail | ⏳ harness wired over both engines (`--engine`). **Full Test262 (language + built-ins) on the Rust engine now completes in ~5 min and scores 91.69% of executed (36,490 pass / 3,307 fail / 7,468 skip) as of 2026-06-04 — see the dated update at the end of this cell; the narrative below is from the 84.65% iteration.** (This iteration: 73.64% → 79.24%, +2,170 tests, all from data-driven targeting of the failure survey — class methods made non-enumerable, sloppy/strict honored per test variant instead of forcing strict, `Symbol.species` getters, private names (`#x`) made invisible to reflection, `WeakMap`/`WeakSet` implemented from scratch (91%/93%), a real `$262.detachArrayBuffer`, and top-level `var`/`function` declarations made own properties of `globalThis` (clearing the `asyncTest`/`$DONE` harness gate).) Per-area highlights: **language 72.8%**; **built-ins** `BigInt` ~99%, `Reflect` ~93%, `Object` ~93%, `Number` ~90%, `Date` ~88%, `String` ~86%, `Math` ~82%, `Set` ~78%, `Proxy` ~70% (0→ from scratch), `JSON` ~77%, `Array` ~72%, `Map` ~70%, `DataView` ~68%, `TypedArray` ~62%, `RegExp` ~54% (`\p{}` Unicode-property-escape tables are the big remaining gap), `BigInt64Array`/`BigUint64Array` **100%**, `WeakMap` ~91%, `WeakSet` ~93%, `Promise` ~55%. Landed across the effort: `Function`/`eval`, parameter defaults, iterator-protocol destructuring, async generators + `for await`, private fields/methods, semantic early-errors as `SyntaxError`; full descriptor-validation `Object`, ES2023 Array methods, correct `Number`/`Math`/`JSON`; a complete **TypedArray/ArrayBuffer/DataView** surface on an engine-core exotic-indexed-access layer (`typed_array.rs` + VM hooks); the **RegExp `Symbol.*` protocol** with `String` dispatch; **ES2024 Set methods**; ISO-8601 `Date`. **Recent (this iteration):** real **BigInt** (`num-bigint`-backed `Value::BigInt`, literals, operators/comparisons, `BigInt` builtin + `asIntN`/`asUintN`, `BigInt64Array`/`BigUint64Array`, DataView BigInt accessors) — replaces the old `f64` approximation; a **Proxy** exotic-object layer (all 13 traps dispatched through the central VM ops + `Object`/`Reflect`); `Reflect.construct`/`IsConstructor` newTarget validation (lifts `not-a-constructor` tests suite-wide); **const-reassignment → TypeError** + `++`/`--` ToNumeric; and **compile-time regex-literal validation** (invalid patterns are now parse-phase `SyntaxError`s; `\p{}` is `u`-flag-aware), which moved RegExp 45→54%; **class methods/accessors made non-enumerable**; **sloppy vs. strict honored per test variant** (the engine parses scripts as sloppy and the runner prepends `"use strict"` for the strict variant, instead of forcing module/strict on everything); **`Symbol.species` getters** on `Array`/`Map`/`Set`/`Promise`/`RegExp`/`ArrayBuffer`/`%TypedArray%` (unified on `realm.symbol_species`); **private names (`#x`) made invisible to reflection** (`getOwnPropertyNames`/`Reflect.ownKeys`/`hasOwnProperty`/`getOwnPropertyDescriptor`); **`WeakMap`/`WeakSet`** (strong-ref, since collection is unobservable in Test262); a real **`$262.detachArrayBuffer`** in the runner (sets the buffer's backing store to `None`, which the TypedArray/DataView code treats as detached) so the ~330 `$DETACHBUFFER` tests exercise real behavior; and **top-level `var`/`function` declarations made own properties of `globalThis`** (the `<script>` scope routes them through `DeclareGlobal`/`StoreGlobal` instead of cells, so `globalThis.x`/`hasOwnProperty` observe them with no cell↔property divergence; `let`/`const`/`class` stay lexical) — this cleared the `asyncTest`/`$DONE` harness gate; a real `URIError` intrinsic so `decodeURI`/`decodeURIComponent` reject malformed input with the right type (decode 38%→96%); and a **memory-leak fix** — the reference-counting GC cannot reclaim the realm's `Rc` cycles (ctor↔prototype, global→builtins, closures→captured objects), so each short-lived `Vm` leaked ~0.4 MB; `Vm::dispose()` now walks the object graph and breaks those cycles (the conformance runner calls it per test), cutting the leak ~50× and bounding peak memory (an earlier full run OOM'd a 64 GB machine). String/array builtins also cap eager allocations (`repeat`/`padStart`/`padEnd`, dense arrays, and array-like `length` in `TypedArray(obj)`/`TypedArray.from`/`.set`) so a hostile input like `"a".repeat(2**33)` or `new Int8Array({length: 2**53})` throws `RangeError` instead of allocating — the latter had been looping/allocating to ~28 GB before the cap. **Lexical hoisting:** `let`/`const`/`class` simple bindings are pre-declared at scope entry, so forward references (a closure naming a `const` declared later in the same scope — e.g. the `nativeFunctionMatcher.js` harness, or `const f = () => g(); const g = …`) resolve to the binding instead of the global object; this moved the engine from 78.76% → 79.16%. **Full TDZ:** a `Value::Uninitialized` marker (`InitCellTdz`) is stored in hoisted `let`/`const`/`class` cells until their initializer runs; `LoadCell`/`LoadUpvalue` throw a `ReferenceError` on it (so use-before-init, incl. self-reference like `let z = z` and cross-closure access, throws as the spec requires; `let x;` with no initializer clears it to `undefined`) — 79.16% → 79.24%. Added **computed `super[expr]` property reads and calls** (static `super.prop`/`super.method()` already worked); super *assignment* (`super.x = v`) remains a small edge. **Dynamic `import()`** now evaluates its specifier and returns a real (rejected, since module loading is unsupported) `Promise` instead of `undefined`, so `import(x).then(…)`/`.catch(…)` work — `language/expressions/dynamic-import` went to ~48% (it had been crashing on `.then` of `undefined`). **Object accessor/prototype surface (79.34% → 79.54%):** the four Annex B `Object.prototype.__defineGetter__`/`__defineSetter__`/`__lookupGetter__`/`__lookupSetter__` (now 54/54, routed through spec `DefinePropertyOrThrow`/`[[GetOwnProperty]]` with full Proxy-trap dispatch); `Object.prototype.__proto__` installed as a real accessor (14/15); a shared **`OrdinarySetPrototypeOf`** with cycle + extensibility checks now backs `Object.setPrototypeOf`, `Reflect.setPrototypeOf`, and the `__proto__` setter (this also fixed an infinite-loop/timeout where `Reflect.setPrototypeOf` could create a prototype cycle); and **`Object.prototype.toString`** made proxy-aware (`IsArray`/`IsCallable` see through to the target) and given the `RegExp` builtin tag — `Object/prototype` 88% → 95%. **`%GeneratorFunction%` / `%AsyncFunction%` / `%AsyncGeneratorFunction%` intrinsic family (79.54% → 79.75%):** added the three function-kind prototypes (distinct from `%Generator%`/`%AsyncGenerator%`, which are the *instance* prototypes), wired into the function-object `[[Prototype]]` chain in `make_closure` by kind, each with a dynamic constructor (compiling `(function* anonymous(args){body})` like `Function`, reachable only via `Object.getPrototypeOf(function*(){}).constructor`), `Symbol.toStringTag`, and two-way `constructor` links; generator/async-generator functions now also get a spec-correct `.prototype` (own proto = `%Generator%`/`%AsyncGenerator%`, no `constructor`), and generator *instances* inherit from that `.prototype` rather than `%Generator%` directly — `GeneratorFunction` 38%→95%, `AsyncFunction`/`AsyncGeneratorFunction` →100%, `GeneratorPrototype` 48%→70%, `Function` dir 69%→79%. **Generator-completion state fix:** `generator_resume` optimistically set the generator state to `Executing` (via `mem::replace`) before dispatching, but the already-`Completed` branch returned without restoring it — so a *second* `.next()`/`.throw()`/`.return()` on any finished generator threw a spurious "generator is already running" `TypeError`. Restoring `Completed` on that path (sync + async) fixed `GeneratorPrototype` 70%→92% (and the many tests whose trailing `iter.next()` tripped it). **Known gap (next major target):** `return`/`break`/`continue` that exit a `try` do **not** run its `finally` (finally is compiled inline only on the normal + exception-landing paths; non-local exits emit a bare `Op::Return`/jump that bypasses it) — and generator `.return()` on a generator suspended inside `try/finally` likewise skips finally; both want a VM completion-register model. Exception-through-finally and `yield`-in-finally already work. **Destructuring-default named evaluation (79.80% → 81.49%, +656 tests):** an anonymous function/class/arrow used as a destructuring *default* (`[x = function(){}] = []`, `var {x = () => {}} = {}`, and the assignment-target form `[x = class{}] = []`) now takes the binding's name, matching the existing parameter-default behavior — the single largest fix this iteration (the `dstr/*-fn-name-*` tests span async-generator/generator/function/arrow/async-function/class). Done in `compile bind_pattern_kind` (AssignmentPattern) and `assign_maybe_default` by routing the default through `compile_named_expr` when the target is a plain identifier. **Async-iteration protocol fix:** `get_async_iterator` (for `for await` / async `yield*`) now throws a `TypeError` when `@@asyncIterator` is present-but-not-callable instead of silently probing `@@iterator`, and only falls back to the sync iterator when `@@asyncIterator` is absent/null (spec GetMethod). **Async `yield*` delegation (81.49% → 82.55%, +411 tests):** the `yield*` desugaring was sync-only (always `GetIterator` + no await); in an async-generator body it now uses `GetAsyncIterator` (so `@@asyncIterator` is consulted and a non-callable one throws `TypeError` without probing `@@iterator`) and `Await`s each `next()` result before reading `done`/`value` — pushed `language/{statements,expressions}/async-generator` from ~76% to ~88%, with cascading gains elsewhere. **Destructuring correctness batch (82.55% → 83.52%, +377 tests):** (1) **array-assignment destructuring** `[a, b] = x` now uses the iterator protocol (`Symbol.iterator` + `IteratorClose`-less step), not numeric-index access — so it works for any iterable (`new Set(...)`, strings) not just indexable array-likes (assignment 67%→82%, for-of 76%→82%); (2) **object rest excludes already-bound keys** — `var {a, ...rest} = o` and `({a, ...rest} = o)` were copying `a` into `rest` (the old `compile_object_rest` admittedly "copy all"); now the taken keys are `delete`d after the spread, and the *assignment*-form object rest (previously a no-op) is handled; (3) **named evaluation** reaches through the parenthesized-cover grammar (`[x = (function(){})]` names the fn, `(0, function(){})` does not) and applies to object-assignment defaults (`({x = function(){}} = o)`). **Typed arrays + resizable ArrayBuffer (83.52% → 83.75%):** TypedArray iteration (`values`/`keys`/`entries`/`for-of`) now reads `array[i]` live each step instead of snapshotting (mutation-during-iteration tests); added `TypedArray.prototype.with`; and implemented **resizable `ArrayBuffer`** — `new ArrayBuffer(len, {maxByteLength})`, `resize()`, the `maxByteLength`/`resizable` getters, and `transfer()`/`transferToFixedLength()` (modeled with a hidden `[[ArrayBufferMaxByteLength]]` own property to avoid restructuring the `Internal::ArrayBuffer` variant) — `ArrayBuffer/prototype/resize` 0→100%, ArrayBuffer dir 62%→73%. **Length-tracking TypedArray views** are now implemented (a `length_tracking` flag on `TypedArrayData` set when an auto-length view is created on a resizable buffer; `ta_eff_length` computes `(bufferByteLength − byteOffset) / elementSize` live, and the `length`/`byteLength` getters + `ta_get`/`ta_set`/`ta_write` bounds use it) — so a view's length follows `resize()`. The remaining resizable-buffer long tail is the `makePassthrough` detach/shrink-mid-operation checks and DataView-on-resizable. **Generic Array methods:** the mutating methods + `slice` (`push`/`pop`/`shift`/`unshift`/`splice`/`reverse`/`fill`, plus `slice`) operated only on the dense backing vec and threw/no-op'd on array-like receivers (`Array.prototype.push.call({length:0}, …)`); they are now spec-generic — `ToObject` + length/indexed `get`/`set`/`delete` — with a dense fast-path preserved, so `.call`/`.apply` on plain objects (and subclasses) works. Remaining: `copyWithin` (still dense-only). The generic loops are bounded by `MAX_DENSE_ARRAY` (a hostile `{length: 2**53}` receiver throws `RangeError` rather than allocating/looping unbounded). **Array iteration-method holes:** `forEach`/`some`/`every`/`map`/`filter`/`reduce`/`reduceRight` now skip holes on array-like receivers — a `present_elements` helper visits only indices where `HasProperty(O, k)` is true (dense arrays have no holes, so they visit all). **Computed class fields (83.98% → 84.43%, +173):** instance + static fields with a *computed* key (`[1 + 1] = 2`, `[sym] = …`) were stored under `""` because the `synthesize_constructor` (no-explicit-ctor) and static-field codegen paths ignored `field.computed` and used `property_key_name` (empty for a non-literal key expr); both now evaluate the key with `ToPropertyKey` like the explicit-constructor path — clearing the broad `cpn-class-*-computed-property-name-*` cluster across class statements/expressions (class dir 88%→90%). **Private brand check + destructuring:** reading a private field on a foreign object (`obj.#x` where `obj` is not an instance) now throws a `TypeError` via a new `Op::PrivateGet` that brand-checks (`HasProperty(obj, "#name")`) before reading, instead of silently returning `undefined`; and `[obj.#x] = […]` (a private field as a destructuring assignment target, `AssignmentTarget::PrivateFieldExpression`) is now supported. Still open in this area: private *writes* (`obj.#x = v` brand check via `member_assign`) and derived-constructor `this`-TDZ (`this`/implicit-return before `super()` should throw `ReferenceError` — our model pre-creates `this` rather than having `super()` create it, so this needs the spec construction model). **Harness:** each Rust-engine test runs on its own 256 MB-stack worker thread joined with a wall-clock **per-test timeout** (`TEST262_TIMEOUT_MS`, default 10 s) plus **cooperative cancellation** (`vm.interrupt`, an atomic flag polled every 256 ops that latches the op budget to 0) — so a pathological test (e.g. the `CharacterClassEscapes`/`property-escapes` tests that build ~1.1M-char strings via O(n²) `+=`) can no longer stall the suite or leak a CPU core. Other crash-safety: opcode budget, regex step budget, array-alloc caps, `catch_unwind` net. Remaining long tail, highest-value first: **Array iteration-method hole semantics** — `forEach`/`some`/`every`/`map`/`filter`/`reduce` materialize the receiver via the `elements()` snapshot and invoke the callback for *every* index `0..len`, but the spec skips indices where `HasProperty(O, k)` is false (holes / absent array-like indices) and reads each present index live via `Get`; rewriting the generic path to `HasProperty`-gate + live `Get` would clear the large `Array/prototype/*` "testResult !== true" cluster (~96); **non-local completion** (a Frame completion-register so `return`/`break`/`continue` run `finally`, generator `.return()` runs finally, and `for-of`/destructuring call `IteratorClose` on abrupt completion — see the try/finally + iterator-close note above, ~150 tests); **resizable `ArrayBuffer`** (`resize`/`maxByteLength`/length-tracking views; ~150); RegExp `\p{}` Unicode-property tables (~440, needs Unicode data); deeper Promise/async ordering and async-iterator unwrapping; `Symbol.species` *usage* in builtin methods; array-index property-descriptor attributes; `Intl`, `Temporal`, `Atomics`/`SharedArrayBuffer`; and diffuse individual value/throw-semantics. **Update 2026-06-04 — non-local completion (partial):** implemented the Frame completion-register (`Completion` enum + `Frame.pending_completion` in `vm.rs`; `do_completion` in `exec.rs` replacing `unwind`; `Op::CompletionJump`; single-landing-pad `compile_try_with_finally`). `return`/`break`/`continue`/throw now run enclosing `finally` blocks (chaining through nesting), and `for-of` calls `IteratorClose` (iterator `return()`) on abrupt exit with correct throw-vs-return error precedence. `statements/try` 82.8%→90.4%, `for-of` 87.4%→**91.3%** (declaration + assignment forms), loops/labeled ~96%. Extended the same completion machinery to: array **destructuring** `IteratorClose` (declaration + assignment; `done` cell + `emit_iter_step_tracked`), and generator `.return()` running enclosing `finally` (`pending_return` + `resume_frame_return`) — **`GeneratorPrototype` 48%→100%**, generators(language) ~95%. (Function-param destructuring close came free — params route through `bind_pattern`.) Also fixed private-method **calls** to brand-check (`obj.#m()` on a non-instance throws `TypeError`; `PrivateGet` not `GetProp` in the call path) — correct/smoke-verified but Test262-neutral alone (needs the per-instance private-element model). **Full suite 91.69% (36,490 pass / 3,307 fail).** Still open: async-generator `.return()` finally, niche try `cptn-*` completion-value tests, per-instance private brand model, derived-ctor `this`-TDZ. |
| **P6** | Value checkpoints | ✅ `durableStep(fn)` memoizes plain-value results — re-run is skipped on replay. Test: `tests/replay.rs::durable_step_memoizes`. |

Known v1 gaps (documented inline in source, all P5 long-tail): RegExp Unicode
property escapes `\p{}` (need Unicode property tables) and the backtracking
matcher's step budget on very large inputs, full UTF-16 string indexing
(currently code-point indexed), `Date` formatting, `Intl`,
`Temporal`, `Atomics`/`SharedArrayBuffer`, `WeakRef`/`FinalizationRegistry`
(intentionally unsupported per the determinism contract), and dense-only arrays
(large allocations throw rather than going sparse). BigInt is now a real
`num-bigint`-backed primitive (no longer `f64`-approximated). Runaway execution
is bounded by an optional opcode budget (`Vm::op_budget`), a regex step budget, a
dense-array allocation cap, and — in the conformance runner — a per-test
wall-clock timeout with cooperative cancellation (`Vm::interrupt`).


## Decision record

We will build an in-tree **pure-Rust JavaScript engine** and a **deterministic-replay**
durable-execution model on top of it, selectable at runtime alongside the existing
C/QuickJS engine, **without breaking the existing code path**.

Settled decisions (and the reasoning, so we don't relitigate):

1. **Pure Rust, no C.** No `chidori-quickjs-sys` linkage in the new path. No `cc` build.
2. **No `boa_engine` dependency.** We study Boa's architecture (bytecode VM, object
   model) as a reference but vendor nothing and depend on nothing from it.
3. **Durability = deterministic replay, not VM-image snapshot.** Required because we
   must support **modify-and-resume** (edit a suspended program, continue from its
   execution point). A frozen VM image is a program counter into specific bytecode;
   editing the source invalidates it. Replay re-executes (possibly edited) source while
   feeding recorded host results from a journal, so it is the *only* model compatible
   with edit-and-continue. This consequently supersedes "parity with the C snapshot
   format" — we are building a capability the C engine cannot provide.
4. **Independent, Rust-native durable format.** The durable artifact is our own journal
   format; it does not need to interop with C-engine snapshot bytes. Deployments run
   all-C or all-Rust; snapshots never cross engines.

Deferred decisions (designed-for, decided later):

- **Edit-conflict policy** when an edit touches already-executed code so the journal no
  longer matches (Temporal-style version markers vs. fail-loud). The journal will be
  keyed/addressable enough to support either; we pick during P4.
- **Value checkpointing** to bound resume cost on long histories (serialize plain JS
  *values* — not continuations — at safe points so replay starts from the last
  checkpoint, not the top). Replay-from-top ships first; checkpoints are an optimization.
- **GC strategy** beyond an initial simple collector (see Engine §).

## Why this is large but tractable

Writing a conformant JS engine is a multi-engineer, multi-month effort. Two things keep
it bounded:

- **We reuse `oxc` for parsing.** It's already a dependency (`transformer`, `codegen`,
  `semantic`). The engine consumes the `oxc_ast` directly, so we write **no lexer and no
  parser** — the single largest chunk of a JS engine is already done and battle-tested.
- **We never serialize VM state.** Continuation serialization — the hardest feature in
  QuickJS/Boa-class engines — is off the table. The engine needs in-memory async/await,
  generators, and a microtask queue, but durability lives one layer up in the journal.

What remains to build: a **bytecode compiler** (oxc AST → our bytecode), a **register/stack
VM** with in-memory frame suspension for generators/async, an **object model**
(shapes/prototypes/property maps), a **GC**, and **builtins** to the conformance level
our agent programs need. The long pole is builtins + async correctness, not the VM core.

## Architecture

```
 agent source (TS)
      │  oxc: strip types  (existing)
      ▼
 JavaScript (oxc AST)
      │  NEW: bytecode compiler  (oxc_ast → chidori-js bytecode)
      ▼
 chidori-js VM  ──────────────►  host-fn seam (engine-agnostic)  ──►  host_core / policies
   • object model                         ▲
   • microtask queue                      │ same Rust host logic the C path uses
   • in-memory async/gen suspension       │
      │                                   │
      ▼                                   │
 SnapshotCapableJsEngine impl  ◄──────────┘
   • snapshot() -> serialized effect journal (+ bundle content-hash)
   • restore()  -> rehydrate: re-eval bundle, replay journal
   • run_jobs_until_blocked() -> drain microtasks; block on first unresolved host op
```

### Crates

- `crates/chidori-js` — the pure-Rust engine. Bytecode compiler, VM, object model, GC,
  builtins, microtask scheduler. Depends on `oxc` for the AST; nothing C.
- `crates/chidori-js` also exposes the **replay runtime**: journal, host-op addressing,
  suspend/resume, restore-by-replay.
- No changes to `crates/chidori-quickjs*`. The C path is untouched by construction.

### The engine seam (already exists)

`SnapshotCapableJsEngine` (`src/runtime/snapshot.rs:1268`) is the integration point:

```rust
pub trait SnapshotCapableJsEngine: Sized {
    fn snapshot(&mut self) -> Result<Vec<u8>>;
    fn restore(snapshot: &[u8]) -> Result<Self>;
    fn resolve_host_promise(&mut self, id: HostOperationId, value: Value) -> Result<()>;
    fn reject_host_promise(&mut self, id: HostOperationId, error: String) -> Result<()>;
    fn run_jobs_until_blocked(&mut self) -> Result<JsRunState>;
}
```

The C engine implements it today. The Rust engine adds a second impl. High-level
consumers (`engine.rs`, `server.rs`) already route through the trait, so the toggle is a
construction-site choice, not a rewrite. **Note:** `restore(&[u8])` for the replay engine
needs the *code bundle*, not just the journal — the journal references the bundle by
content hash, and restore re-evaluates the bundle before replaying. We thread the bundle
through the engine constructor/registry rather than the trait signature.

### The host-function seam (the one refactor to existing code)

The real coupling is in `src/runtime/typescript/snapshot.rs` (~7.2k lines): **33
`unsafe extern "C"` callbacks** registered as raw `JSCFunction` pointers
(`native_runtime_*`). Their *bodies* already marshal `serde_json::Value` and delegate to
`host_core` — i.e. the host logic is engine-agnostic; only the ABI wrapper is QuickJS.

Plan: introduce an engine-agnostic host-fn interface, e.g.

```rust
type HostFn = dyn Fn(&mut HostCtx, &[Value]) -> Result<Value, HostError>;
```

- C path: a thin adapter wraps each `HostFn` as a `JSCFunction` (existing behavior;
  guarded by current tests — this refactor must be behavior-preserving for C).
- Rust path: registers each `HostFn` directly as a native callable on the engine.

The 33 callback bodies move almost verbatim; the marshalling layer is what changes. This
is the only edit to existing files of consequence.

### Toggle

- `CHIDORI_JS_ENGINE=quickjs|rust` (default `quickjs`) selects the impl at construction.
- Cargo feature `rust-engine` gates the new crate out of release builds until it's ready.
- No behavior change for existing deployments until they opt in.

## The replay durable model

**Journal.** Durable state is an ordered list of **host-effect records**, each addressed
by a deterministic key (call-site id + per-site invocation index, or an explicit
author-supplied key). A record holds the resolved/rejected result of one host operation.

**Execution.**
1. Compile bundle → bytecode. Evaluate top-level. Call the agent export.
2. JS runs synchronously, scheduling microtasks, until it `await`s a host op.
3. The host op creates a pending promise tagged with a `HostOperationId`. The engine
   drains the microtask queue to quiescence.
4. If progress is blocked solely on an unresolved host op →
   `run_jobs_until_blocked() == BlockedOnHostOperation(id)`. The runtime persists the
   journal and suspends.
5. Host produces a result → `resolve_host_promise(id, value)` appends to the journal and
   resumes draining.

**Restore (same or new process).**
1. Re-evaluate the bundle (by content hash) → fresh VM. **No VM state is loaded.**
2. Re-run from the top. Each host call, instead of executing, returns its **recorded**
   result from the journal (matched by key).
3. When execution reaches a host call with **no** journal entry (the pending frontier),
   the engine suspends there — reconstructing the exact logical continuation by
   re-execution.

**Modify-and-resume.**
- Edit affects only code **after** the suspension frontier → journal prefix matches
  exactly → clean resume into new code. This is the common case and works out of the box.
- Edit affects code **before** the frontier → host-call sequence/keys may diverge from
  the journal → handled by the deferred edit-conflict policy (version markers or
  fail-loud). The journal's keying is designed to detect divergence precisely rather than
  silently corrupt state.

## Determinism requirements (the correctness backbone)

Replay is only correct if re-execution reproduces the same state. Every non-deterministic
source must be captured-and-replayed or eliminated:

- **Time** — `Date.now()`, `new Date()`, `performance.now()` routed through the host clock
  and journaled. (Your captured-effects timers work covers part of this.)
- **Randomness** — `Math.random`, `crypto.getRandomValues` seeded from / captured via the
  host. (Captured-effects crypto covers part of this.)
- **All I/O** — fs/VFS, network/http, prompts, tools, sub-agent calls — already host ops;
  must all be journaled.
- **Iteration / ordering** — object property order and `Map`/`Set` iteration must be
  deterministic and address-independent (insertion-ordered, never hash/pointer-ordered).
  This is an engine design constraint, satisfied by construction.
- **Async scheduling** — the microtask queue must be strictly FIFO-deterministic (it is,
  by construction — single-threaded, no wall-clock-based scheduling).
- **GC-observable behavior** — `WeakRef`/`FinalizationRegistry` are non-deterministic by
  spec; restrict or forbid in durable agents (lint/deny at compile).

This list is the determinism contract; it should live next to
`docs/captured-effects-vfs-crypto-timers.md` since that work is its foundation.

## Engine internals (chidori-js)

- **Front end:** `oxc_parser` → `oxc_ast`. No hand-written lexer/parser.
- **Compiler:** lower oxc AST → a compact bytecode. Register-or-stack machine (lean
  toward a stack VM first for simplicity; Boa/QuickJS are references). Generators/async
  compile to resumable state machines so a frame can suspend **in memory** at a yield/await
  and resume — *never serialized*.
- **Object model:** prototype chain + "shapes"/hidden-class style property maps for
  predictable iteration order and reasonable perf. Strings as ropes/interned atoms later;
  simple `Rc<str>` first.
- **GC:** start with reference counting + a basic cycle collector (QuickJS's model), or a
  simple mark-sweep over an arena. Correctness first; tune later. Determinism: GC timing
  must not be program-observable.
- **Builtins:** implement the subset agent programs use, expand to conformance targets.
  Priority order: `Object/Array/String/Number/Math/JSON/Map/Set/Promise/async+generators/
  RegExp/Error/Symbol/TypedArray`. `BigInt`/full `Intl`/`Date` formatting are long-tail.
- **Numerics:** `f64` + a Rust `BigInt` crate (e.g. pure-Rust bignum) instead of libbf.

## Test & parity strategy

- **Conformance:** reuse `crates/test262-runner` (`docs/conformance.md`). Run *both*
  engines; define the required pass-set (agent programs don't need 100% of Test262).
  Track the Rust engine's pass-rate as the headline progress metric.
- **Differential execution:** run the existing agent suite on C and Rust engines; assert
  identical outputs. Divergences are bugs in the Rust engine or undeclared non-determinism.
- **Replay tests (new):**
  - suspend → persist journal → restore → resume → identical result.
  - **modify-and-resume (forward edit):** suspend, edit post-frontier code, resume, assert
    new code path taken and pre-frontier effects unchanged.
  - divergence detection: edit pre-frontier code, assert the conflict policy fires (once
    P4 lands).

## Phasing

- **P0 — Seam & scaffold (non-breaking, mergeable, no behavior change).**
  - New `crates/chidori-js` skeleton behind `rust-engine` feature.
  - Refactor the 33 host callbacks behind the engine-agnostic `HostFn` seam; C adapter
    preserves current behavior (existing tests are the gate).
  - `CHIDORI_JS_ENGINE` toggle wired; Rust impl is a stub returning "unimplemented".
  - test262-runner parameterized over engine.
- **P1 — Run-to-completion core.** oxc AST → bytecode → VM. Object model, GC, core
  builtins. Target: synchronous agent programs run start→finish; measured on test262.
- **P2 — Async + host boundary.** Promises, async/await, generators, microtask queue;
  `run_jobs_until_blocked`; host-promise create/resolve/reject; `BlockedOnHostOperation`.
- **P3 — Replay durability.** Journal format, deterministic host-op keying, restore-by-
  replay. Suspend/persist/restore/resume identity tests pass. Wire the determinism
  contract (time/random/IO/ordering).
- **P4 — Modify-and-resume.** Forward-edit resume working end-to-end; implement and choose
  the edit-conflict policy; divergence detection tests.
- **P5 — Conformance & builtins long tail.** Push test262 pass-rate to the agreed bar;
  RegExp/BigInt/Date/Intl as needed.
- **P6 — (optional) Value checkpoints.** Bound resume cost for long histories.
- **Pn — Flip default per environment** once parity + replay gates hold.

## Risks / open questions

- **Conformance long tail** is the schedule risk: RegExp semantics, async ordering edge
  cases, and `Date`/`Intl` are deep. Mitigated by reusing oxc (front end) and scoping the
  required pass-set to agent needs rather than all of Test262.
- **Replay cost** on long histories until P6 checkpoints land.
- **Determinism leaks** are the correctness risk: any uncaptured non-determinism corrupts
  resume silently. The differential suite + a strict determinism contract are the defense.
- **Edit-conflict semantics** (P4) are genuinely hard (Temporal-class problem); we ship a
  conservative fail-loud default before any auto-versioning.
- **`restore` needs the bundle**, so the durable record must pin the code by content hash
  and the runtime must be able to fetch the (original or edited) bundle on resume.
