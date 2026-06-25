# An experimental closure-threading "JIT" for `chidori-js`

> **Status:** experimental, **on by default**, gated by the full conformance /
> replay / differential suite. Lives on branch
> `claude/chidori-js-jit-compiler-btc3ec`. This is a deliberate experiment that
> runs *against* the recommendation in
> [`docs/interpreter-optimization.md`](./interpreter-optimization.md) — read that
> first; §2 and §11.5 there explain why a JIT is normally the wrong tool for this
> engine. This document records what a JIT *can* look like here without breaking
> the engine's invariants, and what it measurably buys.

---

## 1. TL;DR

`chidori-js` runs JavaScript by lowering an `oxc` AST to stack-machine bytecode
(`compiler.rs`) and interpreting it with a switch-dispatch loop
(`exec.rs::run_frame` → `step`). A conventional **native-code JIT** is rejected
in `interpreter-optimization.md` for three standing reasons: it needs `unsafe`
executable memory (or a heavyweight backend like Cranelift), it fights the
deterministic-replay contract with compile/deopt timing nondeterminism, and a
measured agent-replay benchmark shows **JS execution is well under 1 % of an
agent's live wall-clock**, so native codegen's end-to-end payoff is negligible.

All three reasons still hold. So this branch does **not** build a native JIT. It
builds a **closure-threading execution backend** — the most a "JIT" can be here
while keeping every load-bearing invariant intact:

- **Zero `unsafe`.** It is ordinary safe Rust: a `Vec` of boxed closures, one per
  bytecode op. No executable memory, no transmuted function pointers.
- **No new dependencies.** `std` only.
- **Byte-identical deterministic replay.** The JIT is a *pure performance side
  effect*. A runtime toggle (`Vm::jit_enabled`) selects backend, and the test
  suite asserts the two backends produce identical output, errors, and host-call
  journals (`tests/jit.rs`, plus the whole existing suite running JIT-on by
  default — including `tests/replay.rs` record→replay byte-identity).

It compiles each function once, on first activation, into a closure thread cached
on its `FuncProto`. The hot ops (loads, cell/local access, arithmetic,
comparisons, branches) run inline with operands pre-decoded into the closure; the
long tail (calls, property access, generators, async, classes, `super`, `with`,
exceptions, iteration) delegates to the unchanged `step`, so it can never drift
from the reference interpreter.

---

## 2. Why "closure-threading," and why it's the honest ceiling here

`interpreter-optimization.md` §4 lays out the dispatch-optimization family and
the Rust constraint: classic **direct/token threading** needs computed `goto`,
**subroutine threading** needs guaranteed tail calls, and **stable, `unsafe`-free
Rust has neither**. That's why that doc pursues *fewer/cheaper dispatches*
(fusion, loop cleanup) rather than literal threading.

Closure-threading is the one member of that family that *is* expressible in safe
Rust. Instead of an array of opcodes dispatched through one central `match`, the
program becomes an array of **closures**, each capturing its decoded operands.
Dispatch is a call through a per-ip function pointer rather than a re-entry of the
giant `match`. It does not need computed goto or TCO because the *driver loop*
still owns control flow (each closure returns a `Ctl`, the loop advances `ip`) —
so the native stack does not grow per op.

This is genuinely "compiling to native code": the closures are compiled by
`rustc` ahead of time, and the per-function "compile" step selects and
parameterizes them. It is not *speculative* native codegen (no type guards, no
deopt) — and that's the point: speculation + deopt is exactly the JIT machinery
`interpreter-optimization.md` §2 calls out as the determinism-breaking,
bug-prone part. Skipping it is what keeps replay byte-identical.

What it can and cannot win:

- **Removes:** per-op operand decoding (operands are pre-captured), and, for the
  specialized ops, the trip through the central `match` and the construction of a
  throwaway `Result<Ctl, Value>` payload. Some per-op work is also hoisted to
  compile time — e.g. a cell's `stable_cells` membership is resolved once at
  compile instead of a `Vec::contains` on every `InitCell` (12 % of executed ops
  in the Phase-0 survey).
- **Does not remove:** the operand-stack traffic itself (this is still a stack
  VM, not a register VM — that's `interpreter-optimization.md` Phase 4), nor the
  cost of the heavy ops, which still flow through `step`.

So this is the interpreter-dispatch ceiling reachable *without* a new instruction
encoding, `unsafe`, or speculation. It overlaps with — and composes on top of —
the fusion work already landed (Phase 2): fused superinstructions like
`CmpBranchFalse` and `LoadCellConst` are themselves specialized in the thread.

---

## 3. Design

All code is in `crates/chidori-js/src/jit.rs` (+ small seams elsewhere).

### 3.1 The compiled form

```
type OpFn    = Box<dyn Fn(&mut Vm, &mut Frame) -> Result<Ctl, Value>>;
struct JitThread { ops: Vec<OpFn>, specialized: u32, fallback: u32 }
```

`JitThread.ops` is **index-parallel** to `proto.code`: exactly one closure per
bytecode op, at the same index. Because indices are preserved, every jump target
(absolute code offset) carries over unchanged — no remapping, no second encoding,
no interaction with the fusion pass's offset remap.

Each closure returns the **same** `Result<Ctl, Value>` the switch interpreter's
`step` returns for that op. That is the key to a minimal, low-risk integration:
the surrounding `run_frame` driver is untouched and handles a closure's result
with byte-identical control flow whether it came from a closure or from `step`.

### 3.2 Specialized vs. delegated ops

`lower(proto, op)` returns either a *specialized* closure or a *fallback*. The
specialized set is the hot core the Phase-0 survey identified
(`interpreter-optimization.md` §11.2): constants/literals, `LoadArg`,
locals (`LoadLocal`/`StoreLocal`), cells (`LoadCell`/`StoreCell`/`InitCell`/
`InitCellTdz`/`LoadCellConst`, with the identical TDZ check), upvalues, stack
manipulation (`Pop`/`Dup`/`Swap`/`Rot3`), all arithmetic/bitwise/unary ops, all
eight comparisons, the branches (`Jump`/`JumpIf*`/`CmpBranch{False,True}`/peek
jumps), and `typeof`.

Every specialized arm is a **transcription of the corresponding `step` arm that
reuses the identical helper** — `op_add`, `bin_arith`, `less_than`,
`loose_equals`, `strict_equals`, `unary_arith`, `to_boolean`, `to_number`,
`to_numeric`, `const_val`. Reusing the same helper means coercion order, thrown
error type/site, and `±0`/`NaN`/`BigInt`/`valueOf` ordering are identical *by
construction*, not by careful re-implementation.

Everything else — calls, `new`, property get/set, object/array literals,
generators, `await`/`yield`, `with`, private elements, `super`, exceptions, the
iterator protocol, modules, `eval` — lowers to a single fallback closure that
clones the op and calls `vm.step(frame, &op)`. The long tail therefore *is* the
reference interpreter; it cannot diverge.

### 3.3 Caching & lifetime

The thread is cached on the proto:

```
struct FuncProto { …, jit: JitCache /* RefCell<Option<Rc<JitThread>>> */ }
```

`JitCache::get_or_compile` compiles on first activation and clones out the `Rc`,
**releasing the borrow before returning** — so direct recursion (e.g. `fib`
calling `fib`) never double-borrows the cell. Each proto (top-level and nested)
owns an independent cache and compiles lazily on its own first call. The cache is
a pure side effect: never serialized, never observed by the journal, rebuilt from
scratch on every fresh `Vm`. (A `FuncProto` is immutable and shared by `Rc`; the
cache is its only interior-mutable field.)

### 3.4 Integration

`run_frame` is the single chokepoint for *all* execution — ordinary calls,
generator/async resume, module evaluation, promise reactions. The only change to
it: after cloning the per-frame `proto` `Rc`, fetch the proto's thread (when
`jit_enabled`) and source each op's result from it:

```rust
let stepped = match &jit {
    Some(thread) => (thread.ops[ip])(self, &mut frame),
    None         => self.step(&mut frame, &proto.code[ip]),
};
match stepped { /* … unchanged Ctl handling … */ }
```

Everything else in `run_frame` — the op budget, the amortized interrupt poll, the
`pending_throw`/`pending_return` resume handling, the module-capture hook, the
suspend variants, frame recycling — is byte-identical. Because the integration is
at the single dispatch point that the whole engine funnels through, every caller
(sync calls, generators, async, modules) gets the JIT for free, and the
suspend/resume machinery is unaffected: a frame's representation (`ip`, `stack`,
`locals`, `cells`) is unchanged, so a frame suspended under the JIT resumes
identically (under either backend).

---

## 4. Determinism & the toggle (why "on by default" is safe)

The determinism contract (`interpreter-optimization.md` §6) requires that an
optimization change *timing and internal representation only* — never results,
error identity, enumeration order, microtask/promise ordering, or host-call
sequencing. This JIT satisfies it the same way the fusion pass does, and proves
it the same way:

- **`Vm::jit_enabled`** selects the backend at runtime. It is the differential
  oracle: the same program run both ways must produce identical observable
  behavior and an identical journal.
- **`tests/jit.rs::jit_matches_interpreter`** runs a broad corpus (numeric loops,
  every operator, coercion + `toPrimitive`, BigInt, closures/upvalues, TDZ,
  objects/arrays/Map/Set, strings/templates, try/catch/finally, switch, labeled
  break/continue, optional chaining, for-of/for-in, generators, destructuring,
  classes + private fields + `super`, mid-expression throws, async/await +
  microtask ordering, `arguments`) through **both** backends and asserts the
  `(threw, console, error)` triples are byte-identical.
- **The entire existing suite runs JIT-on by default** and is green, including
  `tests/replay.rs` (record→replay byte-identity, suspend→persist→resume),
  `tests/async_gen.rs`, `tests/gc.rs`, and the DOM suites.

Because the unspecialized long tail literally calls `step`, and the specialized
ops reuse `step`'s own helpers, the surface where divergence could even *occur*
is small and directly differential-tested.

---

## 5. Results

> **Measurement caveat (unchanged from `interpreter-optimization.md` §7.6):** the
> cloud dev/CI container cannot reliably resolve wall-clock deltas below ~10–15 %;
> re-running the *same* binary swings several percent. So the load-bearing result
> here is the **deterministic dispatch proxy**, with wall-clock reported only as
> indicative and min-of-N.

### 5.1 Deterministic dispatch proxy

`cargo run -q --release --example jit_stats -p chidori-js` reports, per workload,
how many ops across the whole proto tree are specialized vs. delegated.
`specialized` is the count of central-`match` dispatches removed per pass over
the code — reproducible and environment-independent.

Measured here (whole proto tree, as-emitted; `cargo run --release --example
jit_stats`):

| workload | specialized | fallback | specialized % |
| --- | ---: | ---: | ---: |
| arith_loop | 41 | 10 | 80.4 % |
| fib_recursive | 37 | 16 | 69.8 % |
| property_access | 50 | 20 | 71.4 % |
| array_push_sum | 62 | 16 | 79.5 % |
| array_hof | 61 | 24 | 71.8 % |
| string_build | 38 | 11 | 77.6 % |
| closures | 62 | 22 | 73.8 % |

**~70–80 % of every workload's ops are specialized** (run inline, off the central
`match`). The data-movement + arithmetic + compare/branch core that the Phase-0 survey put
at the bulk of executed ops (`interpreter-optimization.md` §11.2: ~40 % data
movement alone) is specialized; the remaining fallback ops are the heavy,
rarely-hot operations (calls, property access, allocation) where dispatch cost is
already dwarfed by the op's own work.

### 5.2 Indicative wall-clock (min-of-N, JIT on vs. off)

Measured here (`jit_stats`, min-of-N, ms/run; criterion view: `cargo bench -p
chidori-js -- jit_vs_interp`):

| workload | jit | interp | speedup |
| --- | ---: | ---: | ---: |
| arith_loop | 5.88 | 6.54 | **1.11×** |
| fib_recursive | 71.0 | 71.9 | 1.01× |
| property_access | 8.98 | 9.24 | 1.03× |
| array_push_sum | 5.28 | 5.51 | 1.04× |
| array_hof | 3.41 | 3.44 | 1.01× |
| string_build | 2.21 | 2.33 | 1.06× |
| closures | 5.68 | 6.04 | 1.06× |

The JIT is faster on every workload, but most deltas (1–6 %) sit **at or below
this environment's ~10–15 % noise floor** and cannot be claimed as real here. The
one that clears it most is `arith_loop` (**1.11×**) — the most dispatch-bound
workload (80 % specialized, a tight arithmetic loop with almost no per-op work to
hide dispatch behind), which is exactly where removing dispatch should show up.
`fib_recursive` barely moves (1.01×) because its time is dominated by call/frame
setup (heavy ops on the fallback path), not dispatch. These numbers are
directionally consistent with closure-threading theory; resolving the small wins
quantitatively needs a quiet, frequency-pinned machine (§7.6).

### 5.3 Honest read

Per the agent-replay measurement (`interpreter-optimization.md` §11.5), JS
execution is <1 % of an agent's *live* wall-clock, so **this changes live agent
latency negligibly** — exactly as that doc predicts for any JS-execution
optimization. Where a dispatch win actually lands is the **zero-host replay/test
path** (~97 % interpreter) and **compute-heavy steps**: faster replay and CI, and
faster tight numeric loops. That, not live latency, is the honest justification —
the same conclusion the no-JIT analysis reached, now with a JIT in hand to
measure rather than assume.

---

## 6. Limitations & possible next steps

- **Still a stack VM.** The closures push/pop the operand stack exactly as the
  bytecode dictates; the ~40 %-data-movement traffic is *dispatched* more cheaply
  but not *eliminated*. Eliminating it needs a register encoding
  (`interpreter-optimization.md` Phase 4) — orthogonal to this and composable.
- **Boxed-closure indirection.** Each op is a `Box<dyn Fn>`: one heap allocation
  per op at compile time and an indirect call per dispatch. The indirect call is
  the same shape of branch as the central `match`, so part of the predicted win
  is decode/operand savings rather than branch-prediction wins; this is why the
  proxy (dispatches removed) is the honest headline, not a branch-mispredict
  claim.
- **No basic-block fusion (yet).** The biggest closure-threading win — compiling a
  straight-line basic block into *one* closure that keeps intermediates in locals
  and bypasses the operand stack — is not done here, because doing it while
  preserving exact mid-block throw/coercion semantics is delicate. It is the
  natural follow-up if the proxy/timing justify it.
- **Compile cost & memory.** Compiling allocates one boxed closure per op on first
  activation. For run-once code this is pure overhead; it pays off only across
  repeated activations (loops, recursion, repeated calls, replay). A
  call-count threshold (compile only after N activations) would avoid the
  cold-code cost — a standard "tiering" refinement left out here for simplicity.

---

## 7. Where things live

- `crates/chidori-js/src/jit.rs` — the closure-threading compiler, the per-op
  `lower`, the `JitThread`/`JitCache` types.
- `crates/chidori-js/src/exec.rs` — `run_frame` dispatch integration; `step`,
  `Ctl`, `const_val`, `bin_arith` exposed `pub(crate)` for reuse.
- `crates/chidori-js/src/bytecode.rs` — `FuncProto::jit` cache field.
- `crates/chidori-js/src/vm.rs` — `Vm::jit_enabled` toggle.
- `crates/chidori-js/tests/jit.rs` — differential (toggle-equivalence) +
  structural tests.
- `crates/chidori-js/benches/execution.rs` — `jit_vs_interp` criterion group.
- `crates/chidori-js/examples/jit_stats.rs` — deterministic dispatch proxy +
  indicative timing.

---

## 8. References

- [`docs/interpreter-optimization.md`](./interpreter-optimization.md) — the no-JIT
  analysis, the Phase-0 op-frequency survey, the agent-replay <1 % result, and the
  ~10–15 % noise-floor caveat that frame this experiment.
- [`docs/replay.md`](./replay.md) — the deterministic-replay durability model the
  toggle-equivalence test protects.
- [`docs/conformance.md`](./conformance.md) — the Test262 gate.
