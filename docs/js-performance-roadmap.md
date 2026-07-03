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

### 6.4.3 What's next (new baseline)

- **step dispatch is now the wall**: `step` + `run_frame` are 25–43% of
  every workload (fib 42%, arith 50%+, sort 25%) with the call ceremony
  halved. Register bytecode (§3.5) is the remaining structural lever.

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
