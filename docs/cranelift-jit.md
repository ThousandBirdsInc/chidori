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
  in kernels that provably never store/push/pop. Everything else
  element-shaped — BigInt64/BigUint64 arrays, dense writes, `push`/`pop`,
  `.length` on non-viewed bases — goes through `extern "C"` shims that call
  the *same* extracted fast-path cores the interpreter arms use
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

## Measured (2026-08, this container): vs Node 22 and Bun 1.3

The tier's goal is **Node/Bun-level performance on kernel-shaped code**.
Cross-engine wall-clock, scaled versions of the repo's canonical workloads
(`benches/common/workloads.rs`), min-of-3 full-process runs minus each
engine's empty-script baseline, byte-identical outputs verified on every row.
`chidori-int` is the identical binary with `--no-jit`:

| workload | chidori-jit | chidori-int | node 22 | bun 1.3 | jit/node | jit/bun |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| fib_recursive (fib 32) | 20 ms | 133 ms | 33 ms | 16 ms | **0.6×** | 1.3× |
| property_access (20M) | 17 ms | 322 ms | 29 ms | 15 ms | **0.6×** | 1.1× |
| string_scan (charCodeAt hash) | 101 ms | 317 ms | 164 ms | 243 ms | **0.6×** | **0.4×** |
| typed_array (Float64Array dot/transform) | 94 ms | 613 ms | 105 ms | 52 ms | **0.9×** | 1.8× |
| bitwise_prng (50M LCG) | 344 ms | 3927 ms | 310 ms | 479 ms | **1.1×** | **0.7×** |
| closures (adder in a loop, 20M) | 27 ms | 432 ms | 24 ms | 18 ms | **1.1×** | 1.5× |
| dense_push (20×2M pushes) | 1142 ms | 1243 ms | 945 ms | 300 ms | **1.2×** | 3.8× |
| array_sum (push + 20 sum passes) | 313 ms | 832 ms | 114 ms | 86 ms | 2.7× | 3.6× |
| arith_loop (50M, `%`-heavy) | 243 ms | 922 ms | 84 ms | 74 ms | 2.9× | 3.3× |
| dense_sum (200M dense reads) | 1331 ms | 3967 ms | 264 ms | 215 ms | 5.0× | 6.2× |
| array_hof (map/filter/reduce) | 153 ms | 139 ms | 34 ms | 20 ms | 4.5× | 7.8× |
| string_build (rope `+=`) | 611 ms | 523 ms | 125 ms | 247 ms | 4.9× | 2.5× |
| mixed_helpers (object glue) | 637 ms | 642 ms | 57 ms | 73 ms | 11× | 8.7× |
| object_literals (5M allocations) | 1499 ms | 1488 ms | 35 ms | 35 ms | 43× | 42× |

Since that table was taken, the recursion-family extension moved
`mutual_recursion` (function-scoped `isEven`/`isOdd` + captured-binding
`gcd`, the standalone-suite workload) from ~110× behind Node — it ran the
generic call path — to ~1.1× on total wall-clock (interpreter tier ~1.7×):
both engine columns now run the family tiers.

Reading the table honestly, three regimes:

1. **At or beyond Node (7 of 14)** — everything the kernel tier fully
   compiles: recursion, property loops, string scans, typed arrays, bitwise,
   inlined closures, pushes. Four rows are *faster than Node*; two are
   faster than Bun. This is the regime agent compute (checksum loops, PRNGs,
   tokenizers, numeric kernels, comparators) lives in.
2. **2–5× (kernel-shaped, known cause)** — dense-array reads and
   `%`-in-loop shapes pay per-access exactness checks (index integrality,
   bounds, slot tags) that V8 eliminates with induction-variable and range
   analysis. Closing this needs typed loop-counter reasoning in the kernel
   translator (prove `i` integral and bounded by the header check, then
   drop the per-access tests) — mechanical, deterministic, and the
   documented next step.
3. **4–43× (allocation/shape-bound, out of this tier's scope)** —
   `object_literals`, `mixed_helpers`, `string_build`, `array_hof` spend
   their time in allocation, property-map traffic, string building, and
   builtin iteration machinery, not kernel execution (jit ≈ interp on every
   one). Reaching V8 there means escape analysis, inline allocation, and
   shape-specialized builtins — a different, much larger project the
   kernel JIT deliberately does not start.

## Limitations / next steps

- **Per-access exactness checks on dense reads** (regime 2 above): the next
  win is induction-variable typing in the kernel translator so the JIT can
  drop integrality/bounds tests the loop header already proves.
- **Cell-writing pinned callees** keep the interpreter tier (per-call cell
  flushes), as do mixed-return-type recursion families.
- **Dense direct views are read-only kernels only**; writing kernels reach
  dense arrays through the shim cores (a store can grow/reallocate).
- **The allocation-bound regime** (3 above) is explicitly out of scope for
  this tier.
- **One `JITModule` per kernel**; compile cost is paid on first activation
  (~ms-scale). An activation-count threshold is the standard refinement if
  run-once kernels show up in profiles.

## Where things live

- `crates/chidori-js/src/jit.rs` — eligibility, the Cranelift translator, the
  helper shims, the compile-once cache, the three `unsafe` blocks.
- `crates/chidori-js/src/exec.rs` — the four seams (`run_kernel_op_impl` for
  loop kernels across the stack and register tiers, `run_fn_kernel` for
  frameless calls, `run_fn_kernel_rec` for self-recursion,
  `exec_prepared_kernel` for prepared callbacks) plus the shared element
  fast-path cores (`kernel_elem_load`/`store`/`len`,
  `kernel_array_push`/`pop`) both tiers call.
- `crates/chidori-js/src/bytecode.rs` — `Kernel::native` (the cache slot).
- `crates/chidori-js/src/vm.rs` — `Vm::jit_enabled`.
- `crates/chidori-js/src/bin/chidori_js_jit.rs` — the `chidori-js-jit`
  binary.
- `crates/chidori-js/tests/jit.rs` — the differential + structural gate.
