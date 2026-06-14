# Interpreter performance: optimizing `chidori-js` without a JIT

> **Status:** Phase 0 (measurement) **done** — see §11. Phase 1 (hot-loop
> cleanup) **partially landed** — see §12. Phases 2–4 proposed. This document
> scopes a body of work to make the `chidori-js` bytecode interpreter materially
> faster **without** adding a JIT, while preserving the engine's three
> load-bearing invariants: **zero `unsafe`**, **no new heavyweight
> dependencies**, and **byte-identical deterministic replay**.
>
> **Related:** [`docs/pure-rust-js-engine-plan.md`](./pure-rust-js-engine-plan.md)
> (historical engine plan), [`docs/conformance.md`](./conformance.md) (Test262
> gate), [`docs/architecture.md`](./architecture.md), [`docs/replay.md`](./replay.md),
> [`docs/value-checkpoints.md`](./value-checkpoints.md).

---

## 1. Summary (TL;DR)

`chidori-js` is a pure-Rust **bytecode interpreter**: `oxc` parses to an AST,
`compiler.rs` lowers it to a stack-machine bytecode, and `exec.rs`/`vm.rs` run a
switch-dispatch loop over that bytecode. A **JIT** (compile hot bytecode to
native machine code at runtime) would be the conventional way to go faster, but
for Chidori it is the wrong tool: it requires `unsafe` executable-memory
management, a code-generation backend (a large dependency or a hand-rolled
assembler), and it introduces timing nondeterminism that fights the engine's
deterministic-replay contract — all to speed up a workload (LLM agents) that is
overwhelmingly bottlenecked on network/LLM latency, not on JS execution.

This document proposes the **interpreter-level** alternative. The realistic,
high-value wins that respect all three invariants are, in priority order:

1. **Hot-loop cleanup** — flatten the current double dispatch and move per-op
   bookkeeping off the common path.
2. **Superinstructions / op fusion** — fold common opcode sequences (e.g.
   `LoadLocal; LoadConst; Add`) into single fused ops, cutting both dispatch
   count and operand-stack traffic.
3. **Inline caches** — cache shape→slot resolutions at property-access sites.
4. **(Larger, optional) register-based bytecode** — eliminate a large fraction
   of stack-shuffling ops entirely.

Crucially, **true "threaded" dispatch (computed-goto / guaranteed tail calls) is
not cleanly available in stable, `unsafe`-free Rust**, so the goal is framed as
*"reduce the number of dispatches and the work per dispatch"* rather than
*"thread the interpreter"* in the literal sense.

The work is gated end-to-end by the existing **Test262 conformance baseline**
and the **replay byte-identity** tests: a performance change that alters any
observable behavior is a bug, not a tradeoff.

---

## 2. Background: why not a JIT?

A JIT keeps the same front end (parser → bytecode) but replaces the *back end*
for hot code: it profiles which functions/loops run often, emits native machine
code for them, and jumps into it. It computes exactly the same results as the
interpreter — the dividing line is **how work is dispatched**, not semantics.

For Chidori specifically, a JIT collides with the project's stated invariants:

1. **`unsafe` / C-free is a headline property.** A JIT must allocate executable
   memory (W^X / `mmap` with `PROT_EXEC`), write raw bytes into it, and call
   through a transmuted function pointer. That is irreducibly `unsafe`. The
   alternative — a backend like Cranelift — is a large dependency that runs
   against the crate's "pure Rust, vendor nothing" stance (see the decision
   record in `docs/pure-rust-js-engine-plan.md`).
2. **Speculation + deoptimization is the hard, bug-prone part.** JIT speed comes
   from speculating on types/shapes; when an assumption breaks the engine must
   *deoptimize* — abandon native code mid-execution and reconstruct the exact
   interpreter frame. This is where most JIT correctness and security bugs live.
3. **It fights deterministic replay.** Chidori's durability model is
   re-execution against a recorded host-call journal for **byte-identical**
   output (`docs/replay.md`). A JIT introduces nondeterministic *timing* — when
   a function compiles, when it deopts — which can perturb observable ordering
   (GC, microtask interleaving). Keeping a JIT's observable behavior bit-identical
   across record and replay is a constraint production JITs never face.
4. **Rust does not give threaded dispatch for free.** Classic direct/token
   threading needs computed `goto` or guaranteed tail-call optimization. Stable
   safe Rust has neither (`become`/explicit-tail-call is unstable; label-as-value
   does not exist). So the biggest "free" interpreter speedups in C interpreters
   are not directly portable here.
5. **The workload rarely justifies it.** A single `await chidori.llm(...)` dwarfs
   all the JS an agent will run between effects. JS-execution time must first be
   shown to register in a profile of a representative run (see Phase 0).

**Conclusion:** invest in interpreter-level optimizations that keep zero
`unsafe`, add no dependencies, and do not perturb determinism.

---

## 3. Current architecture (what we're optimizing)

All paths below are in `crates/chidori-js/src/`.

- **Front end:** `compiler.rs` (~6.7k LOC) lowers the `oxc` AST to bytecode;
  `bytecode.rs` defines `enum Op` (constants, locals/cells/upvalues/globals,
  arithmetic, property access, calls, control flow, etc.).
- **VM core:** `vm.rs` holds the `Vm`, `Frame`, `Flow` (`Return`/`Throw`/`Suspend`),
  and the suspension/promise/generator machinery. No VM state is ever serialized;
  durability lives one layer up in the journal (`replay.rs`).
- **Dispatch loop:** `exec.rs::run_frame` is the hot loop. It is a **switch-threaded
  interpreter**, and today it is a *double* dispatch:

```rust
// exec.rs::run_frame (abridged)
loop {
    // 1. op-budget decrement (every op)
    // 2. interrupt poll (amortized: every 256 ops)
    // 3. frame.pending_throw  check (Option::take, every op)
    // 4. frame.pending_return check (Option::take, every op)
    let ip = frame.ip;
    frame.ip = ip + 1;
    match self.step(&mut frame, &proto.code[ip]) {   // inner `match op { .. }`
        Ok(Ctl::Next)      => continue,
        Ok(Ctl::Jump(t))   => { frame.ip = t; continue; }
        Ok(Ctl::Return(v)) => { /* module hook */ done!(Flow::Return(v)); }
        Ok(Ctl::Await(v))  => ...,
        Ok(Ctl::Yield(v))  => ...,
        Err(e)             => ...,
    }
}
```

`step(&mut self, frame, op) -> Result<Ctl, Value>` is a large `match op { .. }`.
So per opcode the engine pays for: loop bookkeeping (1–4 above), an indirect read
of `proto.code[ip]`, the inner `match op` jump, construction of a
`Result<Ctl, Value>`, its return, and a re-match on `Ctl` in the outer loop.

**Cost model.** The dominant micro-cost on tiny ops (`Add`, `LoadLocal`) is
branch misprediction on the single central `match` in `step`: the predictor sees
one indirect branch for the whole interpreter and mispredicts constantly
(~15–20 cycles each). For real-world JS, **property access** (shape lookup) is
typically the larger aggregate cost. The team already does this class of tuning —
e.g. cloning the `FuncProto` `Rc` once per frame to avoid a per-op `Op::clone`
(noted in `run_frame` as having been ~5% of a call-heavy run).

The bytecode is **stack-based**: many ops exist only to move values between the
operand stack and locals/cells, which is pure dispatch overhead a register model
would remove.

---

## 4. What "threaded" means, and the Rust constraint

The dispatch-optimization family, from least to most invasive:

- **Switch threading** — one central switch. *This is what we have.*
- **Direct/token threading** — each op handler jumps *directly* to the next
  handler, so the predictor gets one branch site *per opcode* and learns
  correlations ("`LoadLocal` is usually followed by `LoadConst`"). The classic
  20–40% win in C interpreters (CPython computed-goto). **Needs computed goto.**
- **Subroutine threading** — each op is a function; the program is a list of
  function pointers called in sequence. **Needs guaranteed tail calls** to avoid
  growing the native stack.
- **Superinstructions / op fusion** — fold common op *sequences* into one op.
  Cuts dispatch count *and* stack traffic. **Pure compiler+VM work; no dispatch
  trickery; works in safe Rust.**

The catch: direct/subroutine threading need computed `goto` or guaranteed TCO,
**neither of which is available in stable, `unsafe`-free Rust** (`become` is
unstable; raw function-pointer threading reintroduces `unsafe`). Therefore this
plan does **not** pursue literal threaded dispatch. It pursues the safe-Rust
subset that captures most of the benefit: **fewer dispatches (fusion), cheaper
dispatch (loop flattening), and fewer lookups (inline caches).**

---

## 5. Proposed work (phased)

Each phase is independently shippable and independently gated. Phases 1–3 are the
recommended scope; Phase 4 is a larger optional follow-up.

### Phase 0 — Measurement baseline (prerequisite) ✅ done — results in §11

**Goal:** establish where cycles actually go before changing anything, and make
regressions visible. (The only engine touch is a feature-gated, off-by-default
instrument that compiles out of the shipping build — see §11.)

- Extend `benches/execution.rs` (criterion) so the `interp` group isolates the
  dispatch loop from the front end across the existing workloads
  (`arith_loop`, `fib_recursive`, property get/set, arrays, strings, closures).
- Add a **representative-agent** benchmark: a recorded agent run replayed from a
  journal (zero LLM calls), measuring the share of wall-clock spent in JS
  execution vs. host/journal/serialization. This answers "does JS execution even
  register?" and sets the bar for whether Phases 2–4 are worth it.
- Capture a profile (e.g. `perf`/`samply`) of the hottest micro-benchmark to
  confirm the branch-mispredict and property-lookup hypotheses in §3.
- Record committed baseline numbers in this doc (a results table) so later phases
  can quote deltas.

**Exit criteria:** baseline numbers committed; a documented decision on whether
the per-phase wins clear a "worth the complexity" bar.

### Phase 1 — Hot-loop cleanup (flatten dispatch, de-overhead the common path) — partially landed (§12)

**Goal:** remove avoidable per-op work without changing the bytecode.

- **Flatten the double dispatch.** The common `Ctl::Next` path currently builds
  and returns a `Result<Ctl, Value>` that the outer loop immediately re-matches.
  Restructure so the most frequent ops update `frame.ip`/stack and signal
  "continue" with the cheapest possible control transfer (e.g. inline the hottest
  op bodies into the loop, or have `step` mutate `frame` and return a compact
  status that avoids materializing `Ctl::Next`/`Return` payloads on the hot path).
- **Move rare checks off the hot path.** `pending_throw` / `pending_return` are
  only set right after a completion or a generator resume; their `Option::take`
  on every iteration can move to the slow paths that set them (or be guarded by a
  single cheap flag). Keep the interrupt poll amortized as it is.
- **Keep the op-budget semantics identical** (it must remain uncatchable and
  terminating); only its placement may change.

**Risk:** low. No bytecode or semantics change. Pure internal refactor.

### Phase 2 — Superinstructions / op fusion

**Goal:** fewer dispatches and less operand-stack traffic for the most common
sequences.

- Add a **peephole fusion pass** over emitted bytecode (in `compiler.rs`, after
  lowering) that recognizes high-frequency sequences and rewrites them to fused
  ops. Candidate fusions (validate against Phase 0 op-frequency data — do not
  guess):
  - `LoadLocal(x); LoadConst(k); <BinOp>` → `BinOpLocalConst{ x, k, op }`
  - `LoadLocal(x); LoadLocal(y); <BinOp>` → `BinOpLocalLocal`
  - `LoadConst/LoadLocal; <branch>` for loop conditions
  - increment/compare patterns from `for` loops (`i < n`, `i++`).
- Add the corresponding handlers in `exec.rs`. Each fused handler must produce
  **exactly** the result and side effects (including thrown errors, `ToNumber`
  coercions, and ordering) of the sequence it replaces.
- Fusion is opt-in per pattern and gated behind the conformance suite; an
  unfused fallback always remains for any sequence not matched.

**Risk:** medium. Each fused op is a new opportunity for a subtle semantic
divergence (e.g. coercion order, exception type/site). Mitigated by Test262 +
differential testing (§6).

### Phase 3 — Inline caches for property access

**Goal:** make the dominant real-world cost (shape lookups on `GetProp`/`SetProp`)
cheaper by caching the resolved shape→slot mapping at each access site.

- Attach a small monomorphic/polymorphic inline cache to property-access ops
  (keyed on the access site). On a cache hit (same object shape as last seen),
  read/write the slot directly; on a miss, fall back to the full lookup and
  refill the cache.
- **Determinism boundary (critical):** the cache must be a *pure performance side
  effect*. It may only change *how fast* a lookup resolves, never *what* it
  resolves to, nor the order/identity of any observable operation. It must never
  be serialized, must be reset on `Vm` setup, and must be excluded from anything
  the journal observes. This is almost certainly safe (it mirrors what the slow
  path computes) but must be asserted by the replay byte-identity tests (§6).
- Start monomorphic (single shape) and only extend to polymorphic if Phase 0
  data shows it pays.

**Risk:** medium. The cache-invalidation logic (shape transitions, prototype
mutation, `delete`, `__proto__` changes) must be correct or it produces *wrong
results*, not just slow ones. Heavily gated by Test262's `Object`/`Reflect`/`Proxy`
suites and the differential harness.

### Phase 4 — Register-based bytecode (larger, optional follow-up)

**Goal:** eliminate a large fraction of `Load`/`Store` stack-shuffling ops by
moving to a register/local-indexed bytecode (Lua 5 model). This is the
interpreter-side change that most closely approaches "what a baseline JIT would
buy" — *without* code generation, `unsafe`, or determinism risk.

- This is effectively a rewrite of the `compiler.rs` back end and a second
  instruction encoding. It is **out of scope for the initial landing** and listed
  here only to record the ceiling of the interpreter-only approach. Revisit only
  if Phases 1–3 leave a measured, workload-relevant gap.

**Risk:** high (scope). Deferred behind explicit go/no-go after Phases 1–3.

---

## 6. Determinism & replay considerations (non-negotiable)

Every change here must satisfy the engine's determinism contract
(`docs/replay.md`, `docs/conformance.md`):

- **Observable behavior is invariant.** Results, thrown error types/messages,
  property enumeration order, microtask/promise ordering, and host-call
  sequencing must be byte-for-byte identical before and after each phase. These
  optimizations change *timing and internal representation only*.
- **No new nondeterminism sources.** No wall-clock-, address-, or
  iteration-count-dependent decisions may leak into observable output. (This is
  exactly why a JIT's compile/deopt timing is disqualifying and an inline cache —
  which only memoizes a deterministic lookup — is acceptable.)
- **Caches and fused state are never serialized.** Inline caches and any
  Phase-2/3 auxiliary state are rebuilt from scratch on each `Vm` and excluded
  from the journal. A record→replay run must produce an identical journal and
  identical output whether or not caches warmed.
- **Fail-loud on divergence.** The existing ordered-journal divergence detection
  (`replay.rs`) is the backstop: if any optimization perturbs host-call ordering,
  replay must fail loudly rather than silently diverge.

---

## 7. Testing plan

Layered, with each phase required to pass all layers before merge.

### 7.1 Conformance gate (correctness, primary)

- Run the full **Test262** suite via `scripts/test262.sh --gate` against the
  committed baseline. **Zero regressions** is the merge bar for every phase
  (`docs/conformance.md`). Inline caches especially must hold the
  `Object`/`Reflect`/`Proxy`/`Array` suites; fusion must hold the language
  operator/coercion suites.
- Re-record the baseline only for *intentional* conformance *gains* (never to
  paper over a regression).

### 7.2 Engine unit/integration tests (correctness)

- The existing `tests/smoke.rs`, `tests/async_gen.rs`, and `tests/replay.rs` must
  remain green unchanged.
- Add targeted unit tests for each new fused op asserting it equals the unfused
  sequence on edge cases: operand-type mixes (number/string/BigInt/object with
  `valueOf`/`Symbol.toPrimitive`), coercion-order observability, NaN, ±0,
  exceptions thrown mid-sequence, and TDZ/uninitialized-cell interactions.
- Add inline-cache invalidation tests: shape transitions, `delete`, prototype
  swaps (`__proto__`, `Object.setPrototypeOf`), accessor vs data properties,
  proxies, and megamorphic sites.

### 7.3 Replay byte-identity tests (determinism, primary)

- Extend `tests/replay.rs`: for a corpus of programs, assert
  **record→replay** produces an identical journal *and* identical output with
  caches cold vs. warm, and with fusion on vs. off.
- Add a **toggle-equivalence** test: a hidden build/test flag that disables each
  optimization (unfused fallback, cache-bypass) must yield byte-identical output
  and journals to the optimized path on the whole corpus. This is the strongest
  guarantee that the optimizations are pure performance side effects.

### 7.4 Differential testing (correctness, fuzz)

- Add a **differential harness** that runs randomly generated / corpus programs
  through (a) the optimized interpreter and (b) the unfused / cache-bypassed
  interpreter, and asserts identical results, thrown errors, and journals. Seed
  it with Test262 fragments and the benchmark workloads; optionally wire a small
  fuzzer (e.g. `cargo-fuzz`-style, kept in-tree, no new runtime deps).
- Where feasible, cross-check a subset against Node/Bun using the existing
  `benchmarks/run.mjs` cross-runtime harness for *result* parity (not timing).

### 7.5 Performance benchmarks (the actual goal)

- `cargo bench -p chidori-js` (criterion) `interp` group: report per-workload
  deltas vs. the Phase 0 baseline. Each phase states its expected win and must
  not regress any workload.
- The **representative-agent replay benchmark** from Phase 0: report the
  end-to-end share of time in JS execution before/after, to keep the work honest
  about real-world impact.
- Add a CI **performance smoke** (informational, non-gating initially) that flags
  large interpreter-loop regressions on PRs.
- **Measurement environment matters.** Phase 1 found that the cloud dev/CI
  container cannot reliably resolve deltas below ~10–15%: re-running the *same
  unchanged binary* produced criterion "regressions" of +3% to +8% and absolute
  times that drifted upward run-over-run (e.g. `fib_recursive` 74→84→90 ms across
  three identical runs). Consequences: (a) per-phase wins smaller than the noise
  floor (Phase 1 is one) **cannot** be validated by wall-clock here — run on a
  quiet, frequency-pinned, single-tenant machine; (b) the CI perf-smoke must
  compare against a **same-session** baseline (measure HEAD and HEAD~ back to
  back in one job), never a stored historical baseline; (c) for small changes,
  prefer a **deterministic proxy** (executed-op count, branch count, allocations)
  over wall-clock — these are reproducible and environment-independent.

### 7.6 Memory & safety

- Confirm no `unsafe` is introduced (`#![forbid(unsafe_code)]`-style check or
  grep gate in CI).
- Re-run the conformance runner's leak/OOM guards (the GC cycle-breaking
  `Vm::dispose` path) to confirm caches/fused state don't leak across `Vm`s.

---

## 8. Rollout & sequencing

1. **Phase 0** lands first and alone (benchmarks + agent profile + committed
   baseline + the go/no-go decision).
2. **Phase 1** (loop cleanup) — lowest risk, validates the gating harness.
3. **Phase 2** (fusion) — driven by Phase 0 op-frequency data.
4. **Phase 3** (inline caches) — only if property access registers in the agent
   profile.
5. **Phase 4** (register bytecode) — explicit go/no-go after 1–3; likely deferred.

Each phase is a separate PR, each gated by §7.1 (Test262), §7.3 (replay
byte-identity), and §7.5 (no benchmark regressions).

---

## 9. Explicitly out of scope

- **A JIT** (baseline or optimizing), for the reasons in §2.
- **Literal threaded dispatch** (computed-goto / subroutine threading), because
  stable safe Rust lacks computed `goto` and guaranteed tail calls (§4).
- **Any `unsafe` or new heavyweight dependency.** If a proposed optimization
  needs either, it is out of scope by definition.
- **Speculative type specialization with deoptimization** — that is JIT-shaped
  complexity and determinism risk without the JIT's payoff.

---

## 10. Open questions

- Does JS execution register meaningfully against host/LLM/journal time in a
  representative agent run? (Phase 0 answers this and may shrink the scope to
  Phase 1 only.)
- Which fused-op set maximizes win per added handler? (Driven by Phase 0
  op-frequency histograms, not intuition.)
- Monomorphic vs. polymorphic inline caches: does the agent workload have enough
  shape diversity at hot sites to justify polymorphism?
- Is a register bytecode (Phase 4) ever worth its rewrite cost given the workload,
  or is the stack VM permanently "good enough" after Phases 1–3?

---

## 11. Phase 0 results (measured 2026-06-14)

Phase 0 is **complete**. Tooling landed:

- A feature-gated dynamic opcode-frequency instrument (`src/opstats.rs` + one
  `#[cfg(feature = "op-histogram")]` call site in `exec.rs::run_frame`). The
  feature is **OFF by default**, so the shipping engine is byte-identical and
  pays nothing (confirmed: `tests/smoke.rs` and `tests/replay.rs` pass unchanged
  on the default build, including record→replay byte-identity).
- An analyzer that reports static (as-emitted) and dynamic (as-executed) opcode
  and adjacent-pair histograms: `cargo run --release --example opstats -p
  chidori-js --features op-histogram`.

> **Caveat:** the numbers below are from one developer machine and are a
> *relative* baseline for tracking deltas across phases, not an absolute spec.
> Re-run on the target machine before quoting wins.

### 11.1 Timing baseline (criterion, default release build)

Median per the existing `benches/execution.rs` workloads. `compile` = front end
only; `interp` = VM loop only (compiled once); `eval` = end-to-end incl. realm
setup; `engine_new` = realm/builtin construction alone.

| workload | compile | interp | eval |
| --- | ---: | ---: | ---: |
| arith_loop | 4.4 µs | 6.3 ms | 7.0 ms |
| fib_recursive | 5.4 µs | 74.6 ms | 73.9 ms |
| property_access | 7.0 µs | 7.9 ms | 8.8 ms |
| array_push_sum | 6.4 µs | 5.3 ms | 6.0 ms |
| array_hof | 9.0 µs | 3.1 ms | 4.0 ms |
| string_build | 4.5 µs | 2.2 ms | 4.8 ms |
| closures | 8.4 µs | 5.9 ms | 9.3 ms |
| **engine_new** | — | — | **3.6 ms** |

**Findings.**
1. **The front end is not the bottleneck.** Compilation is microseconds; the
   interpreter loop is milliseconds — 3 orders of magnitude apart. Optimization
   effort belongs in the VM loop, not the parser/compiler.
2. **Realm setup (`engine_new`, ~3.6 ms) is a large fixed cost** — it rivals or
   exceeds the *entire* interpreter time of the lighter workloads. For short
   agent steps (a little JS between host calls), realm construction can dominate
   interpreter-loop time. This is a previously-unscoped lever worth its own
   investigation (cache/clone a warm realm) and may beat Phases 1–3 for the
   short-script regime.
3. The interpreter loop dominates only for compute-heavy code (fib ≈ 75 ms).

### 11.2 Dynamic opcode frequency (execution-weighted, 4.43M executed ops)

Top executed opcodes (combined across workloads):

| % | opcode | | % | opcode |
| ---: | --- | --- | ---: | --- |
| 16.9% | `LoadCell` | | 3.8% | `Call` |
| 12.2% | `InitCell` | | 3.8% | `LoadArg` |
| 9.4% | `LoadConst` | | 3.7% | `Return` |
| 4.6% | `JumpIfFalse` | | 3.6% | `LoadUndefined` |
| 4.6% | `Lt` | | 3.6% | `LoadThis`/`LoadNewTarget`/`BindThisSloppy` |
| 4.1% | `Sub` | | 3.6% | `LoadUpvalue` |
| 3.1% | `Add` | | | |

**~40% of all executed opcodes are pure data movement** (`LoadCell`,
`InitCell`, `LoadConst`, `LoadArg`, `LoadUpvalue`, `LoadUndefined`). This is the
operand-stack-shuffle overhead that fusion (Phase 2) and a register bytecode
(Phase 4) directly target. `InitCell` at 12.2% is notably high — per-iteration
`let` bindings in `for` loops and the call/param prologue mint fresh cells.

### 11.3 Top fusion candidates (adjacent executed pairs)

| % | pair | fuse to |
| ---: | --- | --- |
| 8.9% | `LoadCell ; LoadConst` | `LoadCellConst` |
| 5.0% | `InitCell ; LoadCell` | prologue superinstruction |
| 4.6% | `Lt ; JumpIfFalse` | `JumpIfNotLt` (compare-and-branch) |
| 4.5% | `LoadConst ; Lt` | folds into `LoadCellConst ; Lt` |
| 3.8% | `LoadArg ; InitCell` | param-bind superinstruction |
| 3.6% | `LoadConst ; Sub` | `SubConst` |
| 3.4% | `Sub ; Call` | — |

The canonical loop-condition idiom `LoadCell(i); LoadConst(N); Lt; JumpIfFalse`
shows up as a *chain* of high-frequency pairs (8.9% + 4.5% + 4.6%). Fusing
load+binop (`LoadCell ; LoadConst ; <binop>`) and compare-and-branch
(`Lt ; JumpIfFalse`) would remove a large, concrete slice of dispatches and
operand-stack traffic in exactly the hot loops.

### 11.4 Go / no-go decision

| Phase | Decision | Rationale |
| --- | --- | --- |
| **1 — hot-loop cleanup** | **GO** | Lowest risk; validates the gating harness; the per-op `Result<Ctl,_>` round-trip and per-op `Option::take`s are real overhead on the data-movement ops that make up ~40% of execution. |
| **2 — fusion** | **GO** | Data shows clear high-frequency pairs (load+binop, compare-and-branch) and a 40%-data-movement profile. Target the §11.3 candidates, validated by the survey rather than intuition. |
| **3 — inline caches** | **CONDITIONAL** | Property ops did *not* crack the top-15 in this loop/arith-heavy mix, so the win is unproven *for this workload*. Gate on the agent profile (below): pursue only if property access registers there. |
| **4 — register bytecode** | **DEFER** | The 40%-data-movement figure is the strongest argument for it, but it's a back-end rewrite; revisit only if Phases 1–2 leave a measured, workload-relevant gap. |
| **Bonus — warm-realm reuse** | **INVESTIGATE** | `engine_new` ≈ 3.6 ms is unexpectedly large and dominates short scripts; worth scoping independently of the dispatch work. |

### 11.5 Remaining Phase 0 item (carried forward)

The **representative-agent replay benchmark** (JS-execution share of an agent
run replayed from a committed journal) is **not yet implemented** — it needs a
checked-in journal fixture and pulls in the larger `chidori` crate's host
runtime, out of scope for this `chidori-js`-local pass. It is the gate for
Phase 3 and for sizing the whole effort. *A-priori* signal: even the heaviest
micro-workload here (~75 ms) is small next to typical per-LLM-call latency
(hundreds of ms to seconds), so JS execution is likely a *minority* of agent
wall-clock — which is the core argument for the whole "no JIT" stance. **This
must be measured, not assumed, before committing to Phases 3–4.**

---

## 12. Phase 1 results (hot-loop cleanup, 2026-06-14)

**Landed.** Hoisted the per-iteration `pending_throw` / `pending_return` checks
out of `run_frame`'s dispatch loop (`exec.rs`). These two `Option::take`s ran on
*every* opcode, but the fields are set **only** by `resume_frame_throw` /
`resume_frame_return` immediately before `run_frame` (a generator `.return(v)`
or an awaited rejection delivered at resume) and are never re-set inside the
loop — so they can only ever fire on the first iteration. The hoist handles them
once, before the loop, removing two branches from the hot path per executed op.

**Correctness — fully validated.** The whole `chidori-js` suite is green,
including the paths that exercise the hoisted fields: `tests/async_gen.rs`
(generators / `await`), `tests/replay.rs` (**record→replay byte-identity**,
suspend→persist→restore→resume), `tests/gc.rs`, the DOM suites, and
`tests/smoke.rs`. No `unsafe`, no new deps, no bytecode change. The one
acknowledged difference — the op-budget counter decrements one fewer time on the
rare resume-with-injection path — is not observable to JS (the budget is a
coarse, uncatchable safety bound, not a spec-visible quantity).

**Performance — below the noise floor here; not separately claimed.** Hoisting
two predictable-branch `Option::take`s is expected to be a sub-few-percent win,
and the dev/CI container's noise floor (~10–15%, see §7.6) is far larger:
re-running the unchanged binary swung +3–8% and times drifted upward across
identical runs. So this environment **cannot** confirm or deny the delta. The
change is kept on its own merits: it is a strict reduction in per-iteration work
*and* a clarity win — it codifies in one place the invariant that `pending_*`
are resume-only signals. The measurable dispatch wins come from Phase 2.

**Scoping note — the "flatten the double dispatch" half is folded into Phase 2.**
The other Phase 1 idea (collapse the `step → Result<Ctl, Value>` round-trip the
outer loop immediately re-matches) was deliberately *not* done as a standalone
step. Doing it safely requires either (a) a sweeping change to `step`'s return
type touching hundreds of return sites, or (b) duplicating trivial-op logic into
a loop fast-path — and (b) creates exactly the kind of two-sources-of-truth
divergence risk the determinism contract warns against. Phase 2's superinstruction
work restructures dispatch *by construction* (new fused ops handled in the loop),
so the flatten lands there with a single source of truth and direct test
coverage, rather than as a risky isolated refactor of the engine's hottest code.

---

## 13. References

- [`docs/pure-rust-js-engine-plan.md`](./pure-rust-js-engine-plan.md) — engine
  decision record (pure Rust, no `boa_engine`, replay-not-snapshot).
- [`docs/conformance.md`](./conformance.md) — Test262 harness and CI gate.
- [`docs/replay.md`](./replay.md) — deterministic-replay durability model.
- [`docs/architecture.md`](./architecture.md) — runtime map.
- `crates/chidori-js/src/exec.rs` — `run_frame` dispatch loop and `step`.
- `crates/chidori-js/src/bytecode.rs` — `enum Op`.
- `crates/chidori-js/src/compiler.rs` — AST → bytecode lowering.
- `crates/chidori-js/benches/execution.rs` — interpreter micro-benchmarks.
