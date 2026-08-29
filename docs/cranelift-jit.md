---
title: "Cranelift Kernel JIT (experimental)"
description: "The opt-in `jit` feature: a Cranelift baseline tier that compiles the typed kernel programs to native code, behind a separate binary."
---

# The Cranelift kernel JIT (`jit` feature, experimental)

> **Status: EXPERIMENTAL, OFF BY DEFAULT.** This is the tier
> [`docs/js-performance-roadmap.md`](./js-performance-roadmap.md) §4 said to
> treat as "a product decision, not an engineering default" — built under
> exactly the design constraints that section established, but kept out of
> every default build. Enabling it requires **both** the `jit` cargo feature
> (which trades the crate's `forbid(unsafe_code)` down to `deny(unsafe_code)`
> and pulls in the Cranelift compiler backend) **and** the `Vm::jit_enabled`
> runtime switch. The shipped library, CLI, and wasm builds are byte-for-byte
> unaffected.

## The alternative entry point

The tier ships as its own binary so the default `chidori-js` artifacts never
carry it:

```bash
# The JIT-enabled runner (same file-runner contract as `examples/run.rs`):
cargo run --release -p chidori-js --features jit --bin chidori-js-jit -- script.js

# The identical build with the tier off — the quickest A/B on any script:
cargo run --release -p chidori-js --features jit --bin chidori-js-jit -- --no-jit script.js

# Tier observability:
cargo run --release -p chidori-js --features jit --bin chidori-js-jit -- --jit-stats script.js
```

`required-features = ["jit"]` in `crates/chidori-js/Cargo.toml` means the
binary simply does not exist in a default build. Library embedders opt in the
same way the binary does: build with the feature, set `vm.jit_enabled = true`.

## What it compiles (and why this seam)

**Not bytecode.** The typed kernel tier (`kernel.rs`,
[`docs/js-performance-roadmap.md`](./js-performance-roadmap.md) §6.5) already
did the two hard parts of a JS JIT — proving a region monomorphic at
translation time and guarding entry at activation time — and its output is an
unboxed `f64` register program. The retired closure-threading experiment
([`docs/jit.md`](./jit.md)) showed dispatch alone was never the cost; the
kernels removed the boxed-`Value` traffic and got the real win in safe Rust.
What the interpreter still pays on a kernel is the `KOp` dispatch loop itself.
This tier compiles that away: `src/jit.rs` translates kernel programs into
native functions via `cranelift-jit`, compiled on a kernel's first activation
with the tier enabled and cached on the kernel (`Kernel::native`). The
compiled subset now covers essentially the whole kernel language:

- **Scalars**: moves, constants, arithmetic, comparisons, branches, the
  fused superinstructions, `typeof`-free boolean logic. Only bit-exact IEEE
  operations are inlined (`+ - * /`, negation, ordered comparisons, `abs`/
  `floor`/`ceil`/`trunc`/`sqrt`, `Math.min`/`max` via wasm-semantics
  `fmin`/`fmax`); everything JS-specific — the cold tail of `ToInt32` (the
  hot `|x| < 2^63` path is one inlined saturating conversion), `%`'s
  non-integer cases (the integer fast path is inlined with `js_mod`'s exact
  guards), `**`, `Math.round`/`sign`/`fround`/`imul`'s cores — calls back
  into the same `number_arith_raw`/`builtins::numbers` functions the
  interpreter uses.
- **Elements**: every numeric typed-array kind (i8/u8/u8-clamped/i16/u16/
  i32/u32/f32/f64) reads and writes raw storage directly in IR — a
  little-endian load/store at the element width plus the kind's exact
  `decode`/`encode` conversion (integer stores inline the codec's
  `to_int`-plus-wrapping-cast as one saturating conversion, a finiteness
  select, and a narrow store; f32 is an exact IEEE demote/promote; the
  clamped kind reads directly but stores through the shim, its clamp not
  being a wrap). Bounds + `dense_index` conditions fold into one unsigned
  compare against an entry-clamped bound. Each compiled kernel carries the
  direct sequence for the ONE kind the compiling activation pinned per
  oslot (baked at translation); an activation pinning a different kind
  fails that one-compare guard and takes the shims — no new decline, no
  recompile. Dense arrays get a read-only direct view (slot tag + payload
  loads over `Value`'s `#[repr(u8)]` layout, verified by a live self-check)
  in kernels that provably never store/push/pop — and the granting scan
  now upgrades an all-Number hole-free array to a tag-check-free view
  (`DENSE_NUM`: one bounds compare, one payload load per read; the O(len)
  scan is repaid by the first pass). Everything else element-shaped —
  BigInt64/BigUint64 arrays, dense writes, `push`/`pop`, `.length` on
  non-viewed bases — goes through `extern "C"` shims that call the *same*
  extracted fast-path cores the interpreter arms use
  (`kernel_elem_load`/`store`/`len`, `kernel_array_push`/`pop`), with the
  op's exact bail edge on a miss.
- **Pinned strings**: `StrLen`/`CharCodeAt` over the entry-hoisted
  flat-ASCII view — total, no bail, NaN on out-of-range exactly like the
  interpreter.
- **Pinned callees** (`CallKernel`): the resolved callee's function kernel
  is INLINED at each call site — the `adder`-closure-in-a-loop pattern
  compiles to the same loop V8 would make of it. The code is compiled
  against the compiling activation's resolved callee protos and every later
  activation identity-checks its own resolution (`Rc::ptr_eq`); a mismatch
  runs that activation on the interpreter tier. Closure *instances* may
  differ freely — their upvalue snapshots travel through the register
  buffer.
- **Batch array HOFs** (`map`/`filter`/`forEach`/`reduce`/`some`/`every`/
  `find`/`findIndex`): when the receiver is a dense array or numeric typed
  array and the callback is a pure function kernel, the builtin runs the
  WHOLE loop as one native activation — a fixed synthetic loop kernel per
  mode (`jit::batch_kernel`)
  with the callback inlined at its standard pinned-callee slot, cached per
  callback kernel and identity-checked per activation like any other
  pinned callee. `map` writes results straight into the hole-filled
  species array's slots (a writable dense view granted only here — tag +
  payload stores over the `#[repr(u8)]` layout); `filter` pushes kept
  elements through the shared push core; `reduce` threads a Number
  accumulator in a register. Soundness rests on one property: no user code
  runs between batch elements, so the per-call re-checks of the prepared
  path (canonical `Math`, all-Number upvalues) hold once for the whole run
  — the sort specialization's argument. Any element the kernel cannot
  handle (a hole, a non-Number, a shadowed base) bails before completing
  its op and the builtin's generic loop resumes at that exact index; the
  search modes exit through a dedicated FOUND edge at the hit index;
  boolean-returning predicates are admitted wherever the result only feeds
  a truthiness branch (`filter`, `forEach`, and the searches — never
  `map`/`reduce`, whose results materialize), and the interrupt is polled
  on the loop back-edge at the interpreter's cadence.
- **Int-typed registers** (`jit_ty`): a per-kernel analysis proves
  registers INTEGER-VALUED and range-bounded — entry-checked accumulators,
  `%`-re-bounded hash/checksum chains, ToInt32-family results, integer
  typed-array elements, and loop counters bounded by a dominating
  `i < len` guard (a small flow-sensitive range dataflow with
  compare-fact tracking) — and the function then carries TWO bodies: the
  plain float body, and an int body whose typed registers live in i64
  (native `iadd`/`imul`/`srem`/`icmp`, single-compare element indexing, raw
  integer element loads/stores, no per-op float↔int conversions). Runtime
  entry checks (integral, in-band, nonnegative where required, never `-0`)
  pick the body once per activation, so an activation whose live values
  don't fit runs exactly today's float code — nothing regresses. Every
  admitted operation is one where i64 and IEEE-double semantics provably
  coincide (sums far below 2^53, products of proven-nonnegative ranges,
  `%` with nonnegative dividend and positive divisor — and nothing that
  could produce `-0`, NaN, or a fraction). Read-only `%` divisors bake the
  compiling activation's VALUE into the check (`const MOD = 65521`
  becomes a literal), so Cranelift's divide-by-constant strength reduction
  replaces the hardware `srem` with the multiply sequence V8 uses; a
  different later value fails the equality and takes the float body.
- **Recursion families** (`SelfCall`): a RESOLVED family — the invoked
  closure plus every partner reached through its recursive call graph, via
  global bindings or captured (function-scoped) bindings, self and mutual
  alike — compiles to one native function per member, calling each other
  directly (plus a standard-signature wrapper), with the interpreter's
  exact per-call depth guard — exhaustion abandons the pure activation
  through a context flag that unwinds every native frame, and the generic
  rerun raises the spec RangeError — and an every-256-calls interrupt poll
  through the activation context. Member upvalue snapshots load from a
  per-activation table; the compiled code is keyed to the resolved member
  protos AND the callee mapping, identity-checked per activation (a
  reassigned cell or global runs that activation on the windowed executor).
  `isEven`/`isOdd` defined inside a function and capturing each other — the
  shape agent helper code actually takes — compiles to two mutually-calling
  native functions.

Kernels containing anything outside this (cell-writing callees, surviving
property ops, mixed-return-type families) decline translation **as a whole**
and
keep running on the interpreter tier. There is still no OSR, no deopt, and no
frame reconstruction anywhere: bail edges jump to the kernel's own `Exit`
stubs, exactly as the interpreter's fast-path misses do. The native function
remains a drop-in replacement for the dispatch loop *between* the existing
entry guard and the existing exit materialization:

- **Entry** — the caller has already run the full activation guard and loaded
  the register buffer; native code loads every register (and the
  activation-constant tables: string views, element views, pinned-callee
  upvalue windows) from it and the per-activation `JitCtx`.
- **Exit** — native code stores every register back and returns the index
  of the `Exit`/`Ret` op it reached (or an interrupt/abandon sentinel). The
  caller then runs the *same* write-back, operand-shape materialization,
  futility accounting, and return-value construction the interpreter tier
  runs. (In `run_kernel_op_impl` this is literal: the interpreter loop is
  entered at the returned `Exit` index, so the exit path is shared
  instruction-for-instruction.)
- **Interrupts** — taken backward branches count toward a poll and check the
  cooperative-interrupt flag every 256th take, the interpreter's exact
  cadence; recursive kernels poll on the shared per-activation counter at
  the interpreter's per-call points.

## Determinism

The roadmap's constraints, point by point:

- **Baseline-only, non-speculative.** No type guards beyond the kernel tier's
  existing entry guard, no deopt. Eligibility is a pure function of the
  kernel, which is a pure function of the source.
- **Deterministic tier-up.** Compilation happens on the first activation with
  the tier enabled — engine state, not wall-clock.
- **Bit-identical results.** The differential gate (`tests/jit.rs`) runs a
  corpus over every scalar op, both `Math` families, boolean registers,
  localized property registers, function kernels (plain, boolean-returning,
  cell-writing, recursive), guard bails, and the decline boundary — with
  `jit_enabled` on and off — and asserts byte-identical `(threw, console,
  error)` triples. A journal recorded with the tier off replays with it on,
  and vice versa.

The one observable the tier does not preserve exactly is *wall-clock timing*
of cooperative interrupts (native code polls at the same cadence but runs the
ops faster), which is already timing-dependent under the interpreter.

## Safety

- The crate drops `forbid(unsafe_code)` to `deny(unsafe_code)` **only under
  this feature** (`lib.rs`). Default builds keep the hard `forbid`, so the
  sandbox claim in [`docs/sandbox-model.md`](./sandbox-model.md) is unchanged
  for everything shipped today.
- All `unsafe` in the crate lives in `src/jit.rs`, every block
  `#[expect]`-scoped and commented: typing and calling the finalized code
  pointers, freeing each module's executable memory on drop, the element
  shims reconstructing the caller-owned activation tables from the live
  run's `JitCtx`, and the one-time self-check of `Value`'s `#[repr(u8)]`
  dense-slot layout.
- The native code touches exactly two allocations, both owned by the caller
  for the duration of the call: the register buffer (every compiled index is
  validated `< n_regs ≤ KWIN` at translation; the buffer is asserted ≥ `KWIN`
  slots) and the one-byte interrupt flag. No allocation, no recursion, no
  calls except the registered `extern "C"` shims.
- Cranelift is pure Rust (no C), but it is a full compiler backend with its
  own CVE surface — the reason this stays opt-in.

## Measured (2026-08-28, this container): vs Node 22 and Bun 1.3

The tier's goal is **Node/Bun-level performance on kernel-shaped code**.
The numbers below are the standalone cross-engine suite
(`crates/chidori-js/benchmarks/run.mjs`, the CI perf-table workloads):
execution-only wall-clock (subprocess median minus each engine's
empty-script startup baseline), identical `RESULT=` checksums verified on
every row. `chidori-int` is the identical engine with the tier off:

| workload | chidori-jit | chidori-int | node 22 | bun 1.3 | jit/node |
| --- | ---: | ---: | ---: | ---: | ---: |
| checksum (Adler-32 / Uint8Array) | **28.5 ms** | 230.4 ms | 44.5 ms | 39.6 ms | **0.6×** |
| arith_loop (`%`-heavy) | **2.7 ms** | 21.4 ms | 3.9 ms | 4.7 ms | **0.7×** |
| closures (adder in a loop) | **2.9 ms** | 20.5 ms | 3.2 ms | 4.9 ms | **0.9×** |
| property_access | **2.0 ms** | 13.3 ms | 3.4 ms | 3.9 ms | **0.6×** |
| array_push_sum | **16.7 ms** | 25.7 ms | 21.0 ms | 18.8 ms | **0.8×** |
| array_hof (map/filter/reduce chain) | 23.9 ms | 44.1 ms | 25.8 ms | 17.5 ms | **0.9×** |
| typed_array (all-kinds dot/transform/mix) | 17.4 ms | 30.3 ms | 21.5 ms | 10.6 ms | **0.8×** |
| fib_recursive (fib 30) | 8.4 ms | 51.3 ms | 7.4 ms | 8.4 ms | 1.1× |
| sort (comparator kernels) | 145.3 ms | 161.2 ms | 104.7 ms | 92.8 ms | 1.4× |
| string_build (rope `+=`) | 8.1 ms | 7.7 ms | 5.4 ms | 4.1 ms | 1.5× |
| string_scan (charCodeAt + charAt mix) | 10.0 ms | 10.0 ms | 6.5 ms | 5.1 ms | 1.5× |
| array_sum | 77.4 ms | 110.9 ms | 48.8 ms | 60.3 ms | 1.6× |
| json_roundtrip | 61.1 ms | 114.3 ms | 28.7 ms | 24.9 ms | 2.1× |
| mutual_recursion (family tiers) | 18.1 ms | 62.3 ms | 8.3 ms | 5.3 ms | 2.2× |
| mixed_helpers (object glue) | 183.4 ms | 191.8 ms | 20.0 ms | 26.2 ms | 9.2× |

Reading the table honestly, three regimes:

1. **At or beyond Node (9 of 15; the FASTEST engine outright on 5)** —
   everything the kernel tier fully compiles: arithmetic, recursion,
   closures, typed arrays of every numeric kind, the array-HOF batches,
   and the integer loops the int-typed register tier targets. `checksum`
   went from 6–8× behind Node on the interpreter to 1.6× FASTER than
   both Node and Bun; a pure `charCodeAt` hash loop (the tokenizer inner
   loop, isolated) measures ~3× faster than Node and ~6× faster than the
   interpreter tier. This is the regime agent compute (checksum loops,
   PRNGs, tokenizers, numeric kernels, per-element callbacks) lives in.
2. **1.4–2.2× (known causes, shrinking)** — `array_sum` alternates dense
   pushes with reads; `sort` pays per-comparison call framing;
   `string_scan` is now dominated by its `charAt`/`fromCharCode` build
   glue (the hash half runs on the int tier); `json_roundtrip` HALVED
   this round (the compact-mode fast serializer reads plain subtrees
   straight off the slots; the parser skips per-parse key tables and
   dec2flt for small ints) and is now allocation-bound;
   `mutual_recursion`'s remaining gap is per-outer-call family validation
   (~130 ns), after caching the compiled code on the parked family
   removed the per-call identity re-checks.
3. **~9× (`mixed_helpers` — object-shaped straight-line code)** — the one
   row still out of the kernel tier's reach. Interpreter-tier work keeps
   trimming it — a for-in shape guard that skips the per-key liveness
   walk and serves the body's `obj[k]` from a verified (object, key,
   slot) cursor hint; a per-shape for-in key plan that removes the chain
   walk, the per-key index parse and the plan allocation; a word-compare
   fast path for inline-string equality; and consuming dying operands
   instead of cloning them — together **−13% instructions** on this row,
   all of it shipping in the default `forbid(unsafe_code)` binary too.

   What remains is the interpreter itself, and it is spread thin rather
   than pooled behind one fixable hot spot: register dispatch ~31%,
   `Value` clone/drop ~12%, frame setup/teardown ~8% (≈740 instructions
   per JS call), cold-op bodies ~5%, constant materialization ~4%,
   property ICs ~3%. A baseline JIT over the register bytecode was
   scoped and measured against this profile and **rejected**: it removes
   dispatch overhead but must still execute every arm body and every
   helper call, so the honest ceiling is ~10% — not a regime change, for
   a very large and risky compiler. The costs that actually separate this
   row from V8 are representational: a 24-byte boxed `Value` with `Rc`
   refcounting on every move, and a heap frame per call. Closing it means
   an unboxed/NaN-boxed value model and shape-guarded inline property
   access — a value-representation project, not a JIT bolt-on.

## Limitations / next steps

- **Induction-variable typing: done** (the int-typed register tier above) —
  index integrality tests and float↔int conversion traffic are gone from
  int-provable loops; dense reads over pre-scanned all-Number arrays no
  longer pay the slot tag check either (`DENSE_NUM`). `charCodeAt` now
  carries a bail edge, so counter-indexed hashes over pinned ASCII strings
  type as ints (out-of-range bails to the generic NaN). Recursion families
  cache their compiled code ON the parked family, so the per-outer-call
  cost is the dynamic re-validation alone. Remaining int-tier gaps:
  function-kernel `Local` scratch and batch kernels are untyped, and
  `Neg`/`Sign`/non-positive divisors decline to the float body.
- **Cell-writing pinned callees** keep the interpreter tier (per-call cell
  flushes), as do mixed-return-type recursion families.
- **Dense direct views are read-only kernels only**; writing kernels reach
  dense arrays through the shim cores (a store can grow/reallocate). The
  one exception is driver-granted: the batch `map` output view, whose
  target is a freshly created hole-filled array nothing else can reach and
  no push can move.
- **The allocation-bound regime** (3 above) is explicitly out of scope for
  this tier.
- **One `JITModule` per kernel**; compile cost is paid on first activation
  (~ms-scale). An activation-count threshold is the standard refinement if
  run-once kernels show up in profiles.

## Where things live

- `crates/chidori-js/src/jit.rs` — eligibility, the Cranelift translator, the
  helper shims, the compile-once cache, the three `unsafe` blocks.
- `crates/chidori-js/src/jit_ty.rs` — the int-typing analysis (safe code
  only): flow-sensitive ranges, guard facts, entry checks, divisor bakes.
- `crates/chidori-js/src/exec.rs` — the five seams (`run_kernel_op_impl` for
  loop kernels across the stack and register tiers, `run_fn_kernel` for
  frameless calls, `run_fn_kernel_rec` for self-recursion,
  `exec_prepared_kernel` for prepared callbacks, `hof_batch` for the batch
  array HOFs) plus the shared element fast-path cores
  (`kernel_elem_load`/`store`/`len`, `kernel_array_push`/`pop`) both tiers
  call. `builtins/array.rs` calls the batch seam from `forEach`/`map`/
  `filter`/`reduce`.
- `crates/chidori-js/src/bytecode.rs` — `Kernel::native` (the cache slot).
- `crates/chidori-js/src/vm.rs` — `Vm::jit_enabled`.
- `crates/chidori-js/src/bin/chidori_js_jit.rs` — the `chidori-js-jit`
  binary.
- `crates/chidori-js/tests/jit.rs` — the differential + structural gate.
