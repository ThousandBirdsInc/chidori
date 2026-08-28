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
This tier compiles that away: `src/jit.rs` translates the **scalar subset** of
`KOp` (moves, constants, arithmetic, comparisons, branches, the fused
superinstructions, `Math` intrinsics, `Exit`/`Ret`) into one native function
per kernel via `cranelift-jit`, compiled on the kernel's first activation with
the tier enabled and cached on the kernel (`Kernel::native`).

A kernel containing anything outside that subset — element access, `.length`,
pinned `push`/`pop`/`charCodeAt`, pinned-callee calls, recursion — declines
translation **as a whole** and keeps running on the interpreter tier. That
choice is load-bearing: with no native↔interpreter transition inside an
activation there is no OSR, no deopt, and no frame reconstruction — the
correctness surface the roadmap called "the largest this engine would ever
take on" is simply not taken on. The native function is a drop-in replacement
for the dispatch loop *between* the existing entry guard and the existing exit
materialization:

- **Entry** — the caller has already run the full activation guard and loaded
  the register buffer; the native code loads every register from it.
- **Exit** — the native code stores every register back and returns the index
  of the `Exit`/`Ret` op it reached (or an interrupt sentinel). The caller
  then runs the *same* write-back, operand-shape materialization, futility
  accounting, and return-value construction the interpreter tier runs. (In
  `run_kernel_op_impl` this is literal: the interpreter loop is entered at the
  returned `Exit` index, so the exit path is shared instruction-for-
  instruction.)
- **Semantics are shared, not re-implemented** — only bit-exact IEEE
  operations are inlined (`+ - * /`, negation, ordered comparisons, `abs`/
  `floor`/`ceil`/`trunc`/`sqrt`); `%`, `**`, the `ToInt32` bitwise family,
  and `Math.round`/`sign`/`fround`/`min`/`max`/`imul` call back into the same
  `number_arith_raw`/`builtins::numbers` cores the interpreter uses. NaN,
  `-0`, shift masking, `ToInt32` far outside the i64 range — identical by
  construction.
- **Interrupts** — taken backward branches count toward a poll and check the
  cooperative-interrupt flag every 256th take, the interpreter's exact
  cadence; an observed interrupt unwinds through the same latch (registers
  written back, `op_budget` zeroed, the same RangeError).

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
- All `unsafe` in the crate lives in `src/jit.rs`: three `#[expect]`-scoped
  blocks — typing the finalized code pointer, calling it, and freeing the
  module's executable memory on drop.
- The native code touches exactly two allocations, both owned by the caller
  for the duration of the call: the register buffer (every compiled index is
  validated `< n_regs ≤ KWIN` at translation; the buffer is asserted ≥ `KWIN`
  slots) and the one-byte interrupt flag. No allocation, no recursion, no
  calls except the registered `extern "C"` shims.
- Cranelift is pure Rust (no C), but it is a full compiler backend with its
  own CVE surface — the reason this stays opt-in.

## Measured (2026-08, this container)

Release build, min-of-5 wall-clock, `chidori-js-jit` vs the same binary with
`--no-jit` (so kernels, fusion, localization, and the register tier are
identical on both sides — the delta is purely native-vs-interpreted kernel
execution). Outputs byte-identical in every run. The usual caveat from
[`docs/interpreter-optimization.md`](./interpreter-optimization.md) §7.6
applies (shared cloud hardware, ~10–15 % noise floor) — but these deltas
clear it comfortably:

| workload (hot loop shape) | jit | interp | speedup |
| --- | ---: | ---: | ---: |
| `s += i * 2.5 - s * 1e-7` (50M, all-IEEE, fully inlined) | 108 ms | 752 ms | **7.0×** |
| `s += i % 7 + (i * 3 - 1) / 2` (20M, `%` helper call) | 139 ms | 509 ms | **3.7×** |
| `Math.sqrt(j) + Math.abs(...)` (5M, inlined intrinsics) | 38 ms | 122 ms | **3.2×** |
| LCG PRNG `(seed * k + c) >>> 0` (20M, all helper calls) | 864 ms | 1390 ms | **1.6×** |

The gradient is the design speaking: the more of the loop that stays in
inlined IEEE ops, the closer the kernel runs to native arithmetic; helper-call
density (the bitwise family) sets the current floor. Inlining `ToInt32` in
Cranelift IR is the obvious next win if PRNG-shaped workloads matter.

Where this lands in practice is unchanged from the roadmap's honest read: JS
execution is <1 % of live agent wall-clock, so this buys little for live
agents — the payoff is compute-heavy steps and the zero-host replay/test
path. That, and having the measured answer to "what would a real JIT buy?" on
hand rather than assumed.

## Limitations / next steps

- **Scalar kernels only.** Element/`.length`/string/pinned-callee kernels
  decline; extending the subset means teaching the translator the bail-exit
  discipline (registers written back, resume at the access op) — mechanical
  but a larger unsafe-adjacent surface, so it should be paid for by data.
- **Loop kernels and plain function kernels only.** The recursive-kernel
  windowed executor and the prepared-callback path (`exec_prepared_kernel`)
  keep the interpreter tier.
- **One `JITModule` per kernel.** Simple ownership (the module dies with the
  kernel); a shared module would amortize better if kernel counts grow.
- **Compile cost** is paid on first activation (~ms-scale per kernel). A
  run-once kernel eats it for nothing; an activation-count threshold is the
  standard refinement if it shows up.

## Where things live

- `crates/chidori-js/src/jit.rs` — eligibility, the Cranelift translator, the
  helper shims, the compile-once cache, the three `unsafe` blocks.
- `crates/chidori-js/src/exec.rs` — the two seams: `run_kernel_op_impl` (loop
  kernels, stack and register tiers alike) and `run_fn_kernel` (frameless
  function calls).
- `crates/chidori-js/src/bytecode.rs` — `Kernel::native` (the cache slot).
- `crates/chidori-js/src/vm.rs` — `Vm::jit_enabled`.
- `crates/chidori-js/src/bin/chidori_js_jit.rs` — the `chidori-js-jit`
  binary.
- `crates/chidori-js/tests/jit.rs` — the differential + structural gate.
