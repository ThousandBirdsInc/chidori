# JS execution performance: the road toward JIT-class throughput

> **Status:** profiling review complete (2026-07-02); the optimization push it
> scoped has **landed on this branch** — deterministic fast hasher, quick-wins
> batch, key-verified inline caches, superinstruction round 2, and the
> call-path slimming (cell pool + `Rc`-shared `BytecodeFunction`). Measured
> results in **§6**; the remaining roadmap (register bytecode, full
> cells→locals, shapes) is unchanged below. This document is the successor to the
> per-phase plan in [`docs/interpreter-optimization.md`](./interpreter-optimization.md)
> (Phases 0–2 landed there) and the retired closure-threading experiment in
> [`docs/jit.md`](./jit.md). It re-scopes the goal from "make the interpreter
> less wasteful" to **"close as much of the gap to JIT runtimes as the
> engine's invariants allow"** — and it is grounded in a new callgrind
> (instruction-exact) profile of the cross-runtime benchmark suite rather
> than wall-clock on a noisy container.
>
> The three invariants are unchanged and non-negotiable: **zero `unsafe`**,
> **no new heavyweight dependencies**, **byte-identical deterministic replay**
> ([`docs/replay.md`](./replay.md)). Every item below states how it satisfies
> them.

---

## 1. Where we are (measured 2026-07-02)

### 1.1 Cross-runtime gap

`node crates/chidori-js/benchmarks/run.mjs` (5 runs, median, execution-only =
subprocess wall-clock minus per-runtime startup), on an otherwise-idle dev
container (4 cores):

| workload | chidori | node (V8) | bun (JSC) | gap vs fastest |
| --- | ---: | ---: | ---: | ---: |
| arith_loop | 342 ms | 2.2 ms | 4.0 ms | 155× |
| array_hof | 362 ms | 21 ms | 15 ms | 24× |
| array_push_sum | 590 ms | 14 ms | 18 ms | 41× |
| closures | 624 ms | 0.6 ms † | 4.0 ms | (†) |
| fib_recursive | 1.38 s | 7.0 ms | 6.9 ms | 199× |
| json_roundtrip | 223 ms | 38 ms | 26 ms | 8.5× |
| property_access | 904 ms | 0.6 ms † | 3.2 ms | (†) |
| sort | 1.73 s | 110 ms | 99 ms | 17× |
| string_build | 440 ms | 3.6 ms | 3.4 ms | 128× |

† A sub-millisecond "execution" time means V8's optimizer effectively
*deleted* the workload (loop-invariant hoisting / dead-store elimination on a
loop whose result folds), so those ratios measure the JIT's ability to not do
the work, not engine speed doing it. Cross-checked directly: node's *total*
process time for property_access (~37 ms) is below its own ~42 ms startup
baseline. Read those rows as "unboundedly behind on eliminable loops."

**Honest summary:** chidori-js executes these workloads **~10–40× slower**
than JIT runtimes where the work is irreducible (json, sort, array traversal)
and **~100–200×** slower on tight numeric/call loops, which is exactly where
speculative JITs shine. Startup is chidori's one clear win (~4 ms vs node's
~42 ms). For context: QuickJS — the reference "fast pure interpreter" — sits
roughly 10–30× behind V8 on these same shapes; the realistic ceiling for an
interpreter-only chidori-js is that band, and §4 covers what crossing it
would actually take.

(Wall-clock methodology note: an earlier run of this table taken while a
`cargo` build was saturating the container's cores inflated node/bun times
3–15× and understated every gap. On shared hardware, cross-runtime ratios
are only meaningful from an idle machine — one more reason the roadmap's
load-bearing numbers are callgrind instruction counts, which contention
cannot touch.)

### 1.2 Where the instructions actually go (callgrind)

Wall-clock on this container has a ~10–15% noise floor
(`interpreter-optimization.md` §7.6), so the load-bearing numbers here are
**callgrind instruction counts** — deterministic, environment-independent,
and reproducible to the instruction. Totals are for one full workload run.

**property_access (11.0 G instructions) — hashing IS the workload:**

| share | where | what |
| ---: | --- | --- |
| **48.7%** | SipHash (`sip::Hasher::write` 19.8% + `hash_one` 16.1%) + `IndexMap::get_index_of` (12.7%) | every `o.a` get/set SipHashes the key string and probes the property `IndexMap` |
| 17.7% | `step` | dispatch + op bodies |
| 6.0% | `run_frame` | loop bookkeeping |
| 4.9% | `set_prop_mode` | write-path walk |
| 4.2% | `Vec::push_mut` | operand-stack pushes |

**fib_recursive (12.8 G) — the price of a call:**

| share | where | what |
| ---: | --- | --- |
| 21.3% | `step` | dispatch + op bodies |
| **16.7%** | `malloc` + `_int_free` + `free` | one `Rc<RefCell<Value>>` heap allocation **per binding per call** (`make_frame`), plus frame vec churn |
| 8.6% | `run_frame` | loop bookkeeping |
| ~7.8% | SipHash + `IndexMap` probe | resolving the global `fib` by string hash **on every call** |
| 4.9% | `make_frame` | frame setup |
| 2.8% + 1.9% + 1.3% | `drop_in_place<Frame>`, `Rc::drop_slow`, `Value::clone` | refcount churn |
| 1.3% | `slice_contains` | `stable_cells` `Vec::contains` on every `InitCell` |
| 1.1% | `BytecodeFunction::clone` | per-call clone |

**arith_loop (3.2 G) — dispatch-bound, as Phase 0 predicted:**

| share | where | what |
| ---: | --- | --- |
| **52.3%** | `step` (37.7%) + `run_frame` (14.7%) | pure dispatch |
| 8.8% | `Vec::push_mut` | operand-stack traffic |
| 8.2% | `bin_arith` | the actual arithmetic |
| 5.9% | libm `fmod` | the workload's `%` operator |
| 5.4% | `drop_in_place<Value>` | stack-slot drops |

Three structural taxes explain nearly all of the gap:

1. **Hash-table property access** — no shapes, no caches, and (until this
   change) a DoS-hardened hasher: ~49% of property-heavy execution.
2. **Every binding is a heap cell** — `compiler.rs` documents this as the v1
   trade ("every source-level binding is a heap **cell** … an allocation per
   binding"): ~25% of call-heavy execution is allocator + refcount traffic.
   `frame.locals` and `LoadLocal`/`StoreLocal` exist in the VM but the
   compiler never emits them (`num_locals: 0`).
3. **Stack-machine switch dispatch** — >50% of tight-loop execution, the
   known ceiling from `interpreter-optimization.md` §4.

---

## 2. Landed with this review: deterministic fast hasher (`src/fxhash.rs`)

`IndexMap`'s default hasher is `RandomState`/SipHash — a *keyed, DoS-resistant*
hash that is exactly wrong for an engine-internal property table. Replaced
with an in-tree implementation of the Fx hash function (rustc's own table
hasher; one rotate/xor/multiply per 8-byte chunk, ~30 lines, zero new
dependencies) for: object property maps (`ObjectData::props`), `Map`/`Set`/
`WeakMap`/`WeakSet` backing stores.

**Measured (callgrind, deterministic):**

| workload | before | after | Δ instructions |
| --- | ---: | ---: | ---: |
| property_access | 10.98 G | 7.75 G | **−29.4%** |
| fib_recursive | 12.83 G | 12.24 G | −4.6% |

Wall-clock moved less than the instruction count on this container
(property_access roughly −10%, sort and string_build directionally similar) —
the remaining probe is more memory-bound, and the shared container's noise
floor blurs the rest. The instruction count is the honest, reproducible
metric here.

**Why this is safe:**

- *Determinism:* hashes decide only bucket placement inside `IndexMap`;
  iteration order is insertion order and lookups are settled by `Eq`. The
  hash function is unobservable to JS and to the replay journal. Unlike
  `RandomState` (per-process random seed!), Fx is fully deterministic —
  bucket layouts are now *identical across runs by construction*, which is
  strictly more deterministic than before.
- *Security:* SipHash's seed defends tables whose keys an adversary chooses
  against collision-flooding. Property keys come from the agent program and
  its data; the engine already bounds runaway execution with the uncatchable
  op budget and the host interrupt/timeout. A flooding attack degrades a run
  the same way `while(1)` does, and the same defenses answer it. V8, JSC,
  and SpiderMonkey all make this same trade.
- *Gates:* full crate suite (incl. `tests/replay.rs` record→replay
  byte-identity) and the Test262 gate, green.

---

## 3. The interpreter roadmap, re-ranked by measured share

Ordered by (measured cost × implementation risk). Each item is a pure
performance side effect: same results, same errors, same enumeration order,
same journal — gated by Test262, the replay byte-identity suite, and the
callgrind instruction-count proxy.

### 3.1 Property-key atoms with precomputed hashes (attacks the remaining ~24% of property-heavy)

After the hasher swap, `get_index_of` still re-hashes the key string and
byte-compares it on every access. Property names are known at compile time
(they sit in `FuncProto.consts`); the fix is a compile-time **atom table**:

- Intern property-name strings once per engine; a `PropertyKey` then carries
  `(Rc<str>, precomputed u64 hash)`; equality tries pointer-equality first.
- `GetProp`/`SetProp` reference the atom directly from the const table —
  no `JsString` clone, no `PropertyKey` construction, no re-hash per access.
- Determinism: an atom table is a cache keyed by string *content*; identical
  content → identical atom. Nothing address-dependent leaks: hashes are
  content-derived (Fx, unseeded).

Expected: kills most of the remaining 24% `get_index_of` + the per-access key
materialization visible in `step`. Low risk — no semantic surface at all.

### 3.2 Cells → locals: stop heap-allocating every binding (attacks ~25% of call-heavy)

The compiler's own header calls this the deferred v1 trade. The VM already
has pooled `frame.locals: Vec<Value>` and `LoadLocal`/`StoreLocal` ops —
unused. The work is compiler-side:

- Pre-pass per function body: collect names referenced by nested closures
  (the `FnCtx` comment already reserves the concept). Bindings **not**
  captured — the overwhelming majority — lower to `locals` slots; captured
  ones stay cells. Frames containing direct `eval` (or `with`) keep
  everything in cells, as today.
- Params stay cells only when a mapped `arguments` object aliases them or a
  closure captures them.
- Per-iteration `let` semantics: a fresh *cell* per iteration is only needed
  when the loop body captures the binding; otherwise a local slot is reused —
  same observable behavior, zero allocations.

Expected: removes the malloc/free + `Rc`/`RefCell` traffic that is ~25% of
fib-style execution and much of the `closures` workload's 49× gap; every
`LoadCell` (16.9% of all executed ops in the Phase-0 survey) that becomes
`LoadLocal` drops a pointer-chase + borrow-check + refcount-safe clone to an
indexed `Vec` read. This is the single biggest interpreter win available.
Medium risk (compiler rewrite of binding resolution), fully covered by
Test262's closure/TDZ/arguments suites + the differential harness.

### 3.3 Global inline cache (attacks fib's ~8% + every cross-function call)

`LoadGlobal` resolves `fib` by string hash on **every recursive call**. The
global object is one known object; give each `LoadGlobal` site a slot cache:

- Cache `(slot_index)` validated by a **global-object mutation counter**
  (bump on any insert/delete/reconfigure of globals — not on value writes,
  which go through the slot). Hit → direct indexed read from the `IndexMap`;
  miss → today's path, then refill.
- Determinism: deterministic by construction — the counter is engine state
  driven only by program behavior, identical across record/replay. (No
  pointer identity involved; if per-object identity is ever needed for
  object-property ICs, mint **allocation-order object ids** — also
  deterministic — rather than addresses.)
- Never serialized; rebuilt per `Vm` like the existing caches.

Expected: turns every global read (function references above all) from
hash+probe into `counter check + Vec index`. Low risk, small surface.

### 3.4 Superinstruction continuation + per-op precomputation (cheap, incremental)

The Phase-2 fusion infrastructure is landed and cheap to extend:

- `GetPropConst`/`SetPropConst` ops that carry the resolved atom (with §3.1)
  — removes the const-table indirection in the hottest property idiom.
- `stable_cells` membership: resolve at compile time into a per-cell flag on
  the op (`InitCell` vs `InitCellStable`) instead of `Vec::contains` per
  execution (1.3% of fib for a linear scan!).
- Integer-`%` fast path in `arith`: both operands integral f64 in safe range
  → compute directly instead of libm `fmod` (5.9% of arith_loop).
- Loop-idiom fusions from the Phase-0 pair table still unmined:
  `LoadCell;LoadCell;<binop>`, increment patterns (`i = i + 1` as a single
  `IncCell`-class op).

### 3.5 Register bytecode (Phase 4 — now with a justified trigger)

Unchanged assessment from `interpreter-optimization.md`: the biggest
dispatch-side win (arith_loop is 52% dispatch + 8.8% stack pushes + 5.4%
stack drops), and the biggest rewrite. Two changes to its standing since:

- §3.2 (cells→locals) is a **prerequisite done right**: once bindings live in
  indexed frame slots, the distance to "ops address slots directly" (that is
  what a register VM is) shrinks substantially — much of the compiler-side
  analysis is shared.
- The decision input is now instruction-exact: re-run the callgrind proxy
  after 3.1–3.4; if dispatch+stack-shuffle still dominates compute-heavy
  workloads by >40%, the rewrite pays.

### 3.6 Frame diet (small, riskless)

`Frame` carries rarely-used fields (`dispose_scopes`, `enumerators`,
`with_scope`, `eval_vars`, completion machinery) inline; boxing the rare ones
(`Option<Box<RareFrameState>>`) shrinks per-call initialization and the
`drop_in_place<Frame>` cost (2.8% of fib). Likewise `BytecodeFunction::clone`
per call (1.1%) can become an `Rc` bump.

### 3.7 Out of scope here, tracked elsewhere

- **Warm-realm reuse** — `engine_new` ≈ 3.6 ms dominates short scripts;
  flagged INVESTIGATE in `interpreter-optimization.md` §11.4 and partly
  addressed by the resume caches ([`docs/resume-performance.md`](./resume-performance.md)).
- **Object-shape (hidden-class) layer + property ICs** — the full V8-style
  answer to property access. Deliberately *sequenced after* §3.1/§3.3: atoms
  + global ICs capture most of the measured cost at a fraction of the risk
  (shape transitions, `delete`, proto mutation, dictionary-mode fallback are
  where engines grow their worst bugs). Revisit with fresh callgrind data;
  if `get_index_of` still dominates property-heavy code, shapes are the next
  step and the determinism story is the same as ICs (shapes are
  content/insertion-order-derived, never serialized).

---

## 4. The JIT question, answered honestly

The ask: *"if we could add a JIT without `unsafe`, it would be reasonable."*

**A native-code JIT without `unsafe` does not exist.** Emitting machine code
at runtime requires mapping writable-then-executable memory and calling
through a synthesized function pointer — both outside safe Rust's semantics
by definition, no matter who writes the code. The options, ranked by how
close they get:

| option | unsafe? | deps | verdict |
| --- | --- | --- | --- |
| **Cranelift baseline JIT** (`cranelift-jit`) | none in *our* crate (`forbid(unsafe_code)` stays); the `unsafe` lives in the dependency | pure Rust, no C — but a compiler backend (~large, slow to build, its own CVE surface) | The only real JIT path. See below. |
| **JS → wasm → wasmtime/pulley** | same delegation, more of it | wasmtime is a much larger dependency than Cranelift alone | Strictly worse than direct Cranelift for this use. |
| **Closure-threading** (safe Rust) | zero | zero | **Built, measured 1.01–1.11×, retired** ([`docs/jit.md`](./jit.md)). Dispatch was never the whole problem. |
| **Basic-block closure compilation** (safe Rust) | zero | zero | The unexplored safe headroom: compile straight-line blocks to single closures keeping intermediates in a scratch buffer, `Ctl` only at block boundaries. Est. 1.3–2× on dispatch-bound code, delicate mid-block throw semantics. Only worth revisiting *after* §3.2/§3.5, which shrink the same costs with less machinery. |

**On determinism:** the old blanket objection ("a JIT fights replay") is
narrower than `interpreter-optimization.md` §2 states, and the retired
experiment proved the pattern: a **non-speculative** baseline tier (no type
guards, no deopt) that compiles a function on its **Nth activation** (an op-
count trigger — deterministic engine state, not wall-clock) and reuses the
interpreter's own helpers computes byte-identical results with a byte-
identical journal, record and replay alike. Determinism is an engineering
constraint on JIT design, not a disqualifier.

**The honest cost-benefit:** a Cranelift tier is the "no heavyweight deps"
invariant traded away, plus the largest correctness surface this engine
would ever take on (frame reconstruction at OSR points, unwinding into JS
try/finally, the op-budget contract inside native code) — for a payoff that
lands almost entirely on compute-heavy replay/test throughput, because JS
is <1% of live agent wall-clock (`interpreter-optimization.md` §11.5).

**Recommendation:**

1. Land §3.1–3.4 (atoms, cells→locals, global ICs, fusion batch). These are
   safe-Rust, dep-free, and attack the *measured* 25–50% shares. Re-profile.
   Expected composite: **3–6×** on the benchmark suite, taking the gap on
   irreducible workloads from ~10–40× to roughly **3–10×** and tight loops
   from ~100–200× to ~30–60× — the QuickJS band, i.e. parity with the best
   pure interpreters, with startup (~4 ms vs node's ~42 ms) already won.
2. Decide register bytecode (§3.5) on the post-3.x callgrind numbers.
3. Treat a Cranelift tier as a **product decision, not an engineering
   default**: it becomes rational only if a workload emerges where JS compute
   dominates wall-clock at scale (mass replay fleets, compute-heavy agent
   steps) *and* the 5–10× interpreter band is demonstrably not enough. If
   that day comes, the design constraints are already established: baseline-
   only, non-speculative, deterministic activation-count tier-up, interpreter
   helpers for every slow path, `unsafe` confined to the dependency, and the
   whole thing behind the same toggle-equivalence gate the closure-threading
   experiment used.

---

## 5. Gates (unchanged, per item)

Every roadmap item ships only when all four hold:

1. **Test262 gate** (`scripts/test262.sh --gate`): zero regressions.
2. **Replay byte-identity** (`tests/replay.rs`): identical journal + output,
   caches cold vs. warm, optimization on vs. off.
3. **Differential harness** (`tests/fusion.rs` pattern): optimized vs.
   fallback path, byte-identical output and errors over the corpus.
4. **Callgrind instruction-count proxy**: the claimed share actually drops;
   wall-clock is reported but never load-bearing on shared hardware.

---

## 6. Results of the 2026-07-02 optimization push (landed)

Five commits on this branch implement §2, §3.1 (the cheap half), §3.3, §3.4,
§3.6, and an allocation-free approximation of §3.2. All of it is safe Rust,
zero new dependencies, and gated green on both crates' full suites (98 + 603
tests, including record→replay byte-identity, the fusion differential corpus,
and the new `tests/ic.rs` stale-hint corpus cross-checked against Node).

1. **Deterministic Fx hasher** (§2) for property maps and Map/Set stores.
2. **Quick wins:** O(1) `stable_cells` flags; integer fast path for `%`
   (libm `fmod` was 5.9% of arith_loop); `Rc` pointer-equality fast path in
   `JsString::eq`.
3. **Key-verified inline caches** on `GetProp`/`SetProp`/`LoadGlobal`
   (`FuncProto::ic`): a slot-index hint per op site, verified against the key
   stored at that slot on every use — a stale hint is a miss, never a wrong
   answer, so no invalidation protocol exists. Own-data-property reads/writes
   and global reads skip hashing entirely on hits.
4. **Superinstruction round 2:** the fusion pass matches variable-length
   windows and runs to a fixed point. A whole `i < N` loop test
   (`CmpCellConstBranchFalse`), a statement-position `i++` (`IncCellStmt`,
   the 6-op window), `cell <op> const` operands (`AddCellConst`/
   `ArithCellConst`), and the per-iteration `let` copy (`LoadCellInit`) are
   each ONE dispatch. The canonical counting loop: 21 → 12 dispatches per
   iteration.
5. **Call-path slimming:** binding cells are pooled (recycled only at
   `Rc::strong_count == 1` — provably unreachable, so reuse is unobservable);
   `FunctionInner::Bytecode` holds `Rc<BytecodeFunction>`, making the
   per-call clone a refcount bump instead of one or two Vec allocations.
6. **Cells→locals localization (§3.2, landed)** — `localize.rs` rewrites
   provably-uncaptured bindings to flat `frame.locals` slots at compile
   finish. Captured/stable/aliased cells keep their ORIGINAL indices (child
   protos' upvalue references stay valid, no patching); dynamic-resolution
   functions (direct `eval`, `with`, materialized `arguments`) bail whole.
   Every superinstruction gained a local mirror, so localized loops keep
   their one-dispatch tests/updates. En route, `mapped_param_cells` — which
   pinned every sloppy function's parameters as stable heap cells — is now
   built only when the function can actually materialize `arguments`;
   `fib`-style leaf calls now allocate **zero** cells. Gated by a
   capture-focused differential corpus (`tests/localize.rs`) across all four
   {fuse}×{localize} combinations.

Round 3 (same session) added four more commits:

7. **Rope strings** — `s += chunk` was O(total²): string_build's profile was
   96.5% memcpy. `JsString` gained a rope arm: each `+` over well-formed
   operands is one O(1) node; bytes are copied exactly once on first
   observation. `.length` and the size guard stay O(1) via stored totals;
   flatten and drop are both iterative. **string_build: 5.00 G → 0.18 G
   instructions (27×)** — and prompt-building via `+=` is the canonical
   agent pattern.
8. **Dense-array element fast paths** — every `a[i]` converted the Number
   key to a heap-allocated string (grisu formatting!) and reparsed it.
   `GetPropDynamic`/`SetPropDynamic` and the array iteration builtins
   (map/filter/forEach/reduce, the array iterator) now access unshadowed
   in-bounds dense elements directly. array_push_sum −26%, array_hof −17%.
9. **Prototype-level inline caches** — `IcEntry` carries an independent
   `proto_slot` + holder: a data property on the receiver's DIRECT
   prototype (the `arr.push` / class-method pattern) is cached, verified by
   holder pointer identity (which doubles as realm isolation) and
   no-own-props shadowing. The own-hit path never touches the holder.
10. **Owned-args calls** — the interpreter's call ops move their pooled
    argument buffer into the callee frame instead of re-copying.

### 6.1 Instruction counts (callgrind — deterministic, the load-bearing metric)

Whole-workload totals, branch start → after rounds 1–3:

| workload | before | after | Δ |
| --- | ---: | ---: | ---: |
| string_build | 5.00 G | 0.18 G | **−96%** |
| property_access | 10.98 G | 3.95 G | **−64%** |
| arith_loop | 3.17 G | 2.28 G | **−28%** |
| fib_recursive | 12.83 G | 8.59 G | **−33%** |
| array_push_sum | 5.04 G¹ | 3.73 G | −26%¹ |
| array_hof | 3.03 G¹ | 2.52 G | −17%¹ |

¹ vs. the post-round-2 measurement (no branch-start baseline was taken).
The zero-host agent-replay example is instruction-identical before/after
round 3 (11.05 G) — its cost is host-effect glue, not the paths these
rounds touched.

malloc/free (16.7% of fib at branch start) and SipHash (48.7% of
property_access) have left the profiles' top ranks entirely.

### 6.2 Wall-clock (idle container, 5-run median, execution-only)

Against the same-day pre-push idle-machine run (§1.1 methodology):

| workload | before | after | speedup |
| --- | ---: | ---: | ---: |
| property_access | 904 ms | 310 ms | **2.9×** |
| arith_loop | 342 ms | 199 ms | 1.7× |
| fib_recursive | 1.38 s | 900 ms | 1.5× |
| closures | 624 ms | 451 ms | 1.4× |
| array_push_sum | 590 ms | 436 ms | 1.4× |
| array_hof | 362 ms | 276 ms | 1.3× |
| sort | 1.73 s | 1.38 s | 1.3× |
| json_roundtrip | 223 ms | 183 ms | 1.2× |
| string_build | 440 ms | 382 ms | 1.2× |

Geometric mean ≈ **1.5×** across the suite (2.9× where property access
dominates), on top of the hasher win the same day. The zero-host agent-replay
path (`examples/agent_replay`) went 21.0 → 18.5 ms (−12%) — it is
host-effect glue rather than tight loops, as §11.5 of the interpreter doc
predicts.

### 6.3 What's next (assessment after round 3 — now addressed by §6.4)

§3.2 (cells→locals) and the property/array cache work are **landed**. Hash
probing (`get_index_of`) no longer appears in any workload's top profile
ranks — the shapes question (§3.7) is answered for now. What the profiles
show after round 3:

- **sort (14.1 G, the biggest remaining)**: ~25% pure call ceremony for the
  tiny comparator — `Frame` is 408 bytes and its construction, pool
  round-trips, and drop dominate each comparison. The lever is a **Frame
  diet** (box the rarely-used fields) and/or a leaf-call fast path that
  skips unused frame machinery. *(→ landed as the frame POOL + direct-call
  path, §6.4.)*
- **register bytecode** (§3.5) remains the dispatch-side end-game for the
  arith/fib class (step + run_frame are >50% there).
- A discovered pre-existing conformance gap: sealed-array appends are not
  rejected (`Object.seal(a); a[len] = v` writes). Tracked for a separate
  fix validated by Test262.

## 6.4 Round 4 (2026-07-02, follow-up session): the call-ceremony round

Callgrind confirmed §6.3's read: after round 3, ~30–40% of sort/fib-class
execution was **per-call ceremony** — frame construction (`make_frame_owned`
6.3%), frame drop (3.9%), buffer-pool round-trips (`recycle_frame` +
`recycle_value_vec` 7%), ~400-byte `Frame` moves (memcpy 2.8%), and the
layered call dispatch (~6%). This round attacks exactly that; all safe Rust,
zero new dependencies, byte-identical benchmark checksums.

1. **Frame pool** — frames are now recycled WHOLE as `Box<Frame>`
   (`Vm::frame_pool`): a synchronously-finished frame is scrubbed (every
   value-bearing field cleared, so the pool never extends a value's lifetime
   — the cell pool's discipline) and parked; the next call re-initializes
   fields in place. The operand-stack/locals/cells/args buffer *capacities*
   stay attached to the frame across calls (the per-call
   `take_value_vec`/`recycle_value_vec` round-trips for three buffers are
   gone), `run_frame` takes the box (a pointer move, not a ~400-byte
   memcpy), and a suspension keeps its box (no `Box::new` per await/yield).
   A pooled frame's `func` slot holds a shared placeholder
   (`Vm::dummy_bf`) so no `BytecodeFunction` outlives its frame.

2. **Direct call path** (`Op::Call`/`Op::CallMethodless` →
   `Vm::call_direct`): the callee is peeked on the operand stack; a plain
   (sync, non-generator, non-class-ctor) bytecode function skips the
   generic dispatch layers and its arguments MOVE straight from the
   caller's operand stack into the pooled callee frame — no intermediate
   pooled `Vec`, no second copy, and the popped function value is reused as
   `func_obj` without refcount traffic. Everything else (native, bound,
   proxy, async, generator, not-callable) takes the generic path unchanged.
   The generic `call_valuevec` itself was collapsed to a single object
   borrow (callable check + dispatch extraction were two).

3. **Interpreter-loop slimming** — the per-op budget/interrupt checks
   hoisted behind one `counting` bool sampled at frame entry (both are only
   ever installed before execution starts); the common case pays one
   predicted branch instead of two `Option` loads through `self` per op.

4. **`merge_sort` scratch buffer** — the recursion allocated TWO fresh
   `Vec` clones per node (O(n log n) allocations + refcount churn). Now one
   scratch buffer for the whole sort; the left run MOVES into it and merges
   back in place (zero `Value` clones). Identical recursion structure and
   stable-merge order, so the comparator sees the exact same call sequence.

5. **Sort loop fast paths** — `Array.prototype.sort`'s snapshot loop now
   uses the existing `has_get_elem` dense fast path (round 3) instead of
   per-index `HasProperty`/`Get` with a heap-allocated string key, and
   write-back overwrites existing dense elements in place under the same
   gate as `Op::SetPropDynamic` (`props.is_empty()` + in-bounds non-hole).
   The undefined-partition pass moves values instead of cloning.

6. **`func_obj` gating** — the per-call `Rc` round-trip is skipped unless
   `proto.uses_arguments` (its only consumer is `arguments.callee`).

### 6.4.1 Instruction counts (callgrind, whole workload, deterministic)

| workload | round-3 baseline | after | Δ |
| --- | ---: | ---: | ---: |
| sort | 14.09 G | 11.58 G | **−17.8%** |
| fib_recursive | 8.59 G | 7.45 G | **−13.3%** |
| closures | 4.47 G | 4.00 G | **−10.4%** |
| array_hof | 2.52 G | 2.38 G | −5.4% |
| property_access | 3.95 G | 3.85 G | −2.6% |
| arith_loop | 2.28 G | 2.23 G | −2.5% |
| array_push_sum | 3.73 G | 3.67 G | −1.7% |
| string_build | 0.18 G | 0.18 G | 0% |

Benchmark RESULT checksums are byte-identical before/after every change.
Wall-clock (5-run median, same shared container, indicative only per §1.1):
sort 1.09 s → 0.93 s, fib 696 → 572 ms, closures 350 → 311 ms.
An incidental robustness gain: heap-boxed frames raised the native-stack
recursion ceiling from ~1160 to ~1460 JS frames (see the known issue below).

### 6.4.2 Robustness follow-ups (landed with this round)

Both known issues flagged during this round's review are **fixed**:

- **Deep recursion now throws instead of aborting the process.** The
  pre-existing failure: `max_call_depth` (2000) exceeded what the default
  8 MB native stack supports, so `rec(1500)`-style recursion killed the
  process with an uncatchable native stack overflow (main aborted at ~1160
  frames; the frame pool had already raised that to ~1460). Root cause:
  `step`'s single ~190-arm match carried a **4 KB stack frame** (LLVM's
  imperfect stack coloring unions the arms' locals), so every JS call cost
  ~5.5 KB of native stack. Fix, two-sided:
  - `step` is split: hot ops stay inline (~2.7 KB frame), everything else
    delegates to an `#[inline(never)] step_cold`. Each op has exactly ONE
    implementation. A plain JS→JS call's native footprint is now ~3 KB, so
    the 2000-frame guard fires (catchable RangeError) comfortably inside
    8 MB. Probe-verified: `rec(1900)` returns, unbounded recursion throws
    `RangeError`, spread-call recursion included. Callgrind cost of the
    split: ≈0.5% geomean (fib +1.1% worst) — the price of the abort→throw
    conversion.
  - The chidori server/CLI ran agent JS on `tokio::spawn_blocking` threads
    with tokio's **2 MiB** default stacks (abort at ~350 frames!). All
    tokio runtimes are now built via `scheduler::new_tokio_runtime()` with
    16 MiB threads (`JS_THREAD_STACK_BYTES`, matching the branch worker
    threads' existing choice).
  - Remaining sharp edge (accepted): recursion THROUGH `direct eval`
    (`perform_direct_eval` holds its own 4 KB frame) stacks ~10 KB/frame
    and can still hit the native limit before the depth guard on small
    stacks. Pathological; unchanged from before.
- **Sealed-array appends are rejected.** `Object.seal(a); a[a.len] = v`
  (and `push`/`unshift`, in-bounds hole writes, `preventExtensions`-only
  receivers) wrote through the dense-array Set path, which never consulted
  `extensible` when CREATING an element. `ordinary_define_own` now rejects
  creation on a non-extensible dense array — silently in sloppy mode,
  TypeError in strict — matching Node/spec exactly. One Test262 test flips
  to passing (baseline refreshed).

## 6.5 Typed loop kernels (landed): unboxed register execution for numeric loops

The answer to "can we selectively fast-path compute-heavy loops in a
limited context" — a deterministic, safe-Rust, zero-dependency middle tier
between the interpreter and a real JIT. Design informed directly by the
retired closure-threading experiment (docs/jit.md): removing *dispatch*
alone bought 1.01–1.11×, so this tier removes the **boxing** instead — the
`Value` clone/match/drop and operand-stack traffic around every add and
compare.

**How it works** (`kernel.rs`, `Op::LoopKernel`, `Vm::run_kernel_op`):

- At compile finish (after localize + fuse), back-edges identify loop
  regions. A region qualifies only if every op is on a numeric allowlist —
  loads/stores of localized `frame.locals` slots, `Number` constants,
  arithmetic/comparisons/branches and their fused forms; anything else
  (calls, property access, cells, TDZ init, try handlers, suspension)
  disqualifies it entirely. The region's stack bytecode is translated by
  abstract interpretation into a flat register program over unboxed `f64`s
  (canonical stack slots become registers; compare ops must feed branches —
  a materialized boolean disqualifies). The loop-header op is REPLACED by
  `Op::LoopKernel` (indices/jump targets everywhere are untouched); the
  original header op is preserved as the kernel's `fallback`.
- At runtime the kernel enters only when (a) no op budget is installed
  (per-op accounting stays exact — the conformance runner's budgeted runs
  execute fully generically) and (b) every mapped local currently holds a
  `Number`. JS numeric ops are CLOSED over numbers, so after the guard no
  non-number can appear mid-kernel — there is no deopt map because there is
  nothing to deopt. A failed guard executes the fallback op and the generic
  interpreter takes that iteration; the kernel retries at the next
  back-edge arrival (so a `let x;` warming to a number on iteration 1
  enters the kernel from iteration 2). Loop exits write the registers back
  and resume the interpreter at the exit ip. The cooperative interrupt flag
  is polled on kernel back-edges at the interpreter's cadence.
- Arithmetic calls the SAME `number_arith_raw`/`js_mod`/`to_int32` helpers
  as the interpreter's Number×Number fast paths — results are bit-identical
  by construction (NaN, -0, shift masking, `%` sign, `>>>`).

**Kernel v2 (same branch): dense-array access + nested loops.** The
translator's virtual stack became TYPED — number registers vs. array-base
entries (a two-phase walk first discovers which locals are used as bases) —
which generalizes the tier to the `s += a[i]` class:

- `a[i]` reads, `a[i] = v` in-place writes, and `a.length` translate to
  `LoadElem`/`StoreElem`/`LoadLen` kernel ops. Each access RE-CHECKS its
  full fast-path condition at runtime (unshadowed dense array, integral
  in-bounds index, non-hole `Number` element — the same conditions as
  `Op::SetPropDynamic`'s existing fast path) and otherwise **bails
  precisely**: registers write back and the generic interpreter resumes AT
  the access op, its operand stack reconstructed from a per-kernel shape
  table (numbers from registers, bases from the pinned object slots). A
  bail is a slow iteration, never a wrong answer — holes that read through
  the prototype, accessor elements, frozen/sealed arrays, string elements,
  float/negative/OOB indices, and mid-loop growth all take the exact spec
  path (differentially pinned in the corpus and cross-checked against
  Node).
- Base locals are pinned at entry (stores to them reject at translation),
  aliased bases work (per-access borrows), and `a[b[i]]` nests.
- An inner loop's `Op::LoopKernel` header translates as its preserved
  fallback op, so nested numeric loops collapse into ONE outer kernel
  (the per-iteration `let j` reset — a dead `undefined` store — is elided
  when provably re-stored before any read within the block).

**Measured (callgrind, whole workload, deterministic):**

| workload | before | after | Δ |
| --- | ---: | ---: | ---: |
| arith_loop | 2.23 G | 0.48 G | **−78% (4.6×)** |
| array_sum (new workload) | 10.67 G | 3.76 G | **−65% (2.8×)** |
| array_push_sum | 3.67 G | 2.72 G | **−26%** (its sum loop kernels) |
| sort / fib / closures / property / string / json | — | — | unchanged |

Wall-clock arith_loop ~245 ms → ~44 ms (5.6×); a mixed array/nested
workload (dot products + 2D walk) 686 ms → 193 ms (3.6×). The gap to V8 on
pure numeric loops drops from ~100× to ~20×. `fib` (calls), `sort`
(comparator calls), and property/string workloads are untouched by design —
kernels only fire where the loop body is local numerics and dense-array
element access.

**Gates:** the differential corpus (70+ programs) + 300-case deterministic
fuzz (`tests/kernels.rs`) require byte-identical behavior kernels-on vs
kernels-off across break/continue/labels, NaN/-0/precision edges, guard
bails, late entry, nested loops, array holes/accessors/freeze/aliasing/
growth/reassignment, and op-budget interaction; structural tests pin the
canonical, array, and nested loops to actually kernelize; full suites +
Test262 gate green (kernels are additionally OFF under the runner's op
budget, so conformance runs the generic path — the corpus carries
kernel-specific coverage). The pass is disabled under the `op-histogram`
feature (it would hide per-op counts).

**Kernel v3 (same branch): `Math.*` intrinsics + in-body `const`/`let`.**

- The compiler's Math method-call pattern (`LoadGlobal("Math"); Dup;
  GetProp(name); Swap; args…; Call(n)`) translates to direct kernel ops for
  `abs floor ceil round trunc sign sqrt fround` (unary) and
  `min max pow imul` (binary, exact-arity only). Every kind calls the SAME
  core function its builtin uses (`builtins::numbers`), so results are
  bit-identical — including `Math.round`'s half-up negatives, `min/max`
  NaN-poisoning and ±0 ordering, and `imul`'s int32 wrap. The **entry
  guard** identity-checks the global `Math` binding (a plain data property
  holding the canonical object — accessors/replacements decline) and each
  used method (methods are writable; a monkeypatched `Math.max` makes the
  kernel decline and the patch runs generically, observably). `Math.PI`-
  class value constants are non-writable AND non-configurable on the
  canonical object, so with the object identity guarded they fold to
  literal constants at translation. Unsupported methods/arities
  (`hypot`, `log`, variadic `max`) reject the region as before.
- In-body `const x = …` / `let y = …` emit a TDZ-init op
  (`InitLocalTdz`) that previously rejected the region — the single
  biggest eligibility hole in practice. It is now ELIDED under the same
  proof as the dead `undefined` store: the local must be re-stored before
  any read, branch, or branch target; a genuine conditional-TDZ-read
  region stays generic so the ReferenceError comes from the spec path
  (pinned in the corpus).

Measured on a Math-heavy loop (clamp + `imul` hash over 2M iterations, the
DSP/aggregation shape): **2058 ms → 199 ms (10.3×)**, node at 57 ms — the
gap on this class drops from ~36× to ~3.5×. Benchmark-suite counts are
otherwise unchanged (checksums identical); the corpus grew Math edge cases
(NaN/±0 ordering, half-up rounding, monkeypatching, wholesale `Math`
replacement, accessor-on-globalThis, patch-between-activations) plus
in-body-declaration cases, and the fuzz generator now routes values
through Math intrinsics.

**Kernel v4 (same branch): dense appends, materialized booleans, and
captured loop bounds.**

- **Appends & hole fills**: `StoreElem` now performs an exact
  one-past-the-end append (`a[a.length] = v`, `arr.push`-free building)
  and in-bounds hole fills — both CREATE a property, so they additionally
  require the array extensible and under the dense-storage bound;
  otherwise they bail to the generic path, which owns the sloppy-silent /
  strict-TypeError / RangeError semantics. This unlocks the fill-by-append
  and `new Array(n)` fill idioms.
- **Booleans as first-class kernel values**: the virtual stack and the
  local map are statically TYPED. A stored comparison (`const hi = x > 5`),
  `!x`, `true`/`false` literals, and loop-carried flags become Bool
  registers holding exactly 0.0/1.0; the guard requires `Value::Bool` and
  write-back restores it — `typeof ok` never sees a number. Coercing
  consumers (arithmetic, conditions, Math args) read the raw register —
  identical to `ToNumber`/`ToBoolean` on a boolean — while array
  indices/elements REFUSE bools (`a[true]` is the property `"true"`), and
  strict (in)equality between statically mixed bool/number operands folds
  to its constant (the generic `strict_equals` never compares across
  types). Local types are discovered to a FIXPOINT (a boolean store types
  the local; the next translation run reloads it as Bool); genuinely
  mixed-type locals keep the loop generic.
- **Captured loop bounds**: a read-only-in-region UPVALUE (`const N`
  captured from the enclosing scope — the classic module-level bound)
  snapshots into a register at entry, guarded `Number`. Sound because
  kernel regions contain no calls: nothing can write the cell
  mid-activation. In-region upvalue writes still reject.

Measured: array_sum drops further, 3.74 G → 2.03 G instructions (−81%
total from its 10.67 G pre-kernel baseline — the `new Array(N)` fill loop
was bailing per-iteration on holes). A run-scanning workload (append-build
+ boolean-flag scan, 500k elements ×4) goes **2065 ms → 204 ms (10.1×)**.
Other suite counts unchanged (±layout noise), checksums byte-identical.

**Kernel v5 (same branch): FUNCTION kernels — frameless tiny callees.**

The sort/HOF profile is ~55% comparator-call ceremony (frame init/recycle,
operand-stack moves, `Value` clones/drops) around an 8-op body. A function
whose ENTIRE body is on the kernel allowlist now also compiles to a register
program (`FuncProto::fn_kernel`), and the call paths (`call_direct`,
`call_bytecode_vec`, the slice-based `call_bytecode`) execute it FRAMELESS:
no frame, no operand stack, no pool traffic — arguments load straight into
registers and `Return` yields the result value (`KOp::Ret`, typed
Number/Bool).

- **Entry guard, per call**: every consumed argument present and a `Number`
  (a missing/extra/string argument declines THAT call, generically);
  captured upvalues hold `Number`s (read-only snapshots — no calls can run
  inside a kernelized body, so nothing writes a cell mid-execution); no op
  budget; no trace sink (it must see an enter/exit per call); `Math`
  canonicals verified as in v3. Callers apply the depth guard before the
  hook, so the max-call-depth RangeError fires identically on both paths.
- **fn-mode translation rules** on top of the loop allowlist: `LoadArg`
  reads become guarded argument registers; locals are pure register scratch
  — there is no guard to type them, so every read must be DOMINATED by a
  real store, tracked as a sorted `init` set merged under the same
  must-match rule as the virtual-stack shape (a genuine TDZ read or
  use-before-init rejects; the generic path owns the error). Element access
  and `a.length` reject outright (a bail needs a frame to resume into). The
  declared-function prologue (`this`/`new.target` materialized into locals)
  translates via an OPAQUE stack entry whose only legal consumer is a store
  to a never-read local — arrows and declared functions both kernelize.
  Bodies with loops work (the interior `Op::LoopKernel` translates as its
  fallback, and kernel back-edges poll the interrupt flag as usual).

Measured (callgrind, RESULT checksums byte-identical across the suite):
**sort 11.56 G → 6.21 G (−46%)**, **closures 4.03 G → 2.31 G (−43%)** (its
callbacks capture numeric upvalues — the guarded-snapshot rule covers them),
**array_hof 2.38 G → 1.72 G (−28%)**, arith_loop/array_sum −3.4% each;
fib_recursive +0.07% (the `fn_kernel.is_some()` probe on a never-eligible
callee — ~1 instruction per call), property_access unchanged. Wall-clock
sort ~1.4 s → ~0.6 s on the dev box.

**Gates:** corpus grew ~20 function-kernel programs (comparators incl.
boolean-returning and ternary, map/filter/reduce/every/findIndex callbacks,
upvalue capture with number and string cells, missing/extra/non-number
arguments, monkeypatched Math, −0/NaN pins, `.call`/`.apply` entry,
recursion staying generic, loops inside kernelized bodies); structural pins
require the canonical tiny functions to carry `fn_kernel` and
frame-dependent bodies (`arguments`, property access, calls, allocation,
implicit-undefined return) to NEVER carry one; the op-budget test now also
covers a call-heavy program (function kernels are OFF under a budget, like
loop kernels).

**Kernel v6 (same branch): SELF-RECURSIVE function kernels.**

fib-class functions are pure scalar bodies whose only off-allowlist ops are
the recursive call sites. `LoadGlobal` of the function's OWN name now
translates (fn mode) to a speculative "self" entry, and the plain-call
pattern over it fuses to `KOp::SelfCall`: the executor
(`Vm::run_fn_kernel_rec`) runs the whole recursion as stacked REGISTER
WINDOWS over one grown `Vec<f64>` with an explicit (return-pc, dst,
window) stack — zero frames, zero `Value`s, zero operand stacks for the
entire call tree.

- **Guard** (on top of v5's): the global the callee resolves through must
  be a plain data property holding the VERY closure being invoked (pointer
  identity) — a rebound/shadowed/accessor'd name declines and the generic
  `LoadGlobal` observably resolves whatever the program set up. Checked
  once per top-level entry: nothing inside a kernel can write globals.
- **Depth fidelity**: self-calls track depth against the interpreter's
  limit (`call_depth + window count`); an overflow ABANDONS the activation
  and returns "guard declined" — sound because function kernels are pure
  (registers only) — so the caller's generic rerun recurses to the same
  depth and raises the exact spec RangeError from the exact frame.
  Interrupts poll on self-calls and back-edges as usual.
- **Static safety rails**: every self-call must supply every argument
  index the body consumes (a short call would need the generic `undefined`
  parameter), and a recursive kernel must return NUMBERS only (a boolean
  would land in a caller register statically typed Num, diverging under
  `typeof`/strict-eq). Argument expressions must be statically-Number
  registers. Mutual recursion and named-expression self-reference (a
  lexical binding, not a global) stay generic.

Measured: **fib_recursive 7.51 G → 0.81 G instructions (−89%, 9.3×)**;
wall-clock fib(30) ~78 ms vs node 22's ~110 ms total on the same box — the
first workload where chidori beats node outright. Every other suite count
is at layout-noise level (±0.15%), checksums byte-identical. Corpus grew
fib/gcd/Ackermann (nested self-call arguments), rebinding-mid-program,
boolean-return and mutual-recursion negatives, per-call declines, and a
dedicated depth-overflow differential (`max_call_depth = 64` on a big-stack
thread) pinning the abandon-and-rerun RangeError path.

### 6.5.1 What's next (new baseline)

- Remaining kernel candidates: `String.prototype.charCodeAt`-class reads,
  loop bounds via own-frame CELLS (captured accumulators), typed-array
  element access (a natural fit — elements are statically numeric), and
  argument-typed ARRAY parameters for function kernels (`(a, i) => a[i]`
  needs arg object slots + a bail-free access story).
- Kernel-tier extensions with clear shapes: MUTUAL recursion (guard a
  small set of global bindings instead of one), self-calls through local/
  captured bindings (`const f = n => … f(…)`), and boolean-returning
  recursion (type the result register Bool).
- **step dispatch remains the wall for non-kernel code**: `step` +
  `run_frame` are 25–43% of call-heavy workloads. Register bytecode (§3.5)
  is the remaining structural lever, and kernels shrink its risk: the
  translator's typed-stack machinery is exactly the analysis a register
  allocator needs.

Re-run the callgrind sweep before choosing; the noise-floor and idle-machine
caveats in §1.1 stand.

## 7. References

- [`docs/interpreter-optimization.md`](./interpreter-optimization.md) —
  Phases 0–2 (measurement, hot-loop cleanup, fusion), the noise-floor
  protocol, and the agent-replay "<1% of live wall-clock" result.
- [`docs/jit.md`](./jit.md) — the retired closure-threading experiment.
- [`docs/resume-performance.md`](./resume-performance.md) — the resume-cost
  caches (transpile/proto/regexp) that motivated "measure, then cache".
- [`docs/replay.md`](./replay.md) — the determinism contract every item here
  is gated against.
- `crates/chidori-js/benchmarks/` — the cross-runtime harness used for §1.1.
