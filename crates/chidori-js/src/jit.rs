//! Cranelift baseline JIT for the typed kernel tier (`jit` feature only).
//!
//! This is the tier `docs/js-performance-roadmap.md` §4 sketched as "a product
//! decision, not an engineering default", built exactly under the constraints
//! that section established: **baseline-only, non-speculative, deterministic
//! activation trigger, interpreter helpers for every slow path** — and `unsafe`
//! confined to the smallest possible surface (see *Safety* below).
//!
//! ## What it compiles
//!
//! Not bytecode. The kernel tier (`kernel.rs`) already solves the two hard
//! problems a JS JIT faces — proving a region monomorphic and guarding entry —
//! and its output is a typed, unboxed `f64` register program
//! ([`Kernel`](crate::bytecode::Kernel)). This module translates the **scalar
//! subset** of that program ([`KOp`](crate::bytecode::KOp) moves, constants,
//! arithmetic, comparisons, branches, the fused superinstructions, `Math`
//! intrinsics, `Exit`/`Ret`) into native code via `cranelift-jit`, one native
//! function per kernel, compiled on the kernel's first activation with the
//! tier enabled and cached on the kernel ([`Kernel::native`]).
//!
//! Anything outside the scalar subset — element access, `.length`, pinned
//! `push`/`pop`/`charCodeAt`, pinned-callee and recursive calls — declines
//! translation *as a whole kernel*: those kernels keep running on the safe
//! interpreter tier unchanged. No native↔interpreter transitions exist inside
//! a kernel activation, so there is no OSR, no deopt, and no frame
//! reconstruction — the two rejection reasons `interpreter-optimization.md`
//! §2 weighs heaviest simply do not arise.
//!
//! ## The contract with the interpreter tier
//!
//! A native kernel is a drop-in replacement for the interpreter's dispatch
//! loop *between* the entry guard and the exit materialization, nothing more:
//!
//! - **Entry**: the caller has already run the full activation guard and
//!   loaded the register file (`regs[0..n_regs]`). The native function loads
//!   every register from that buffer.
//! - **Exit**: the native function stores every register back to the buffer
//!   and returns the index of the [`KOp::Exit`]/[`KOp::Ret`] op it reached
//!   (or [`INTERRUPTED`]). The caller then performs the identical write-back /
//!   shape materialization / return-value construction the interpreter tier
//!   performs — from the same buffer state the interpreter loop would have
//!   left.
//! - **Semantics are shared, not re-implemented.** Only bit-exact IEEE
//!   operations are inlined (`+ - * /`, negation, ordered comparisons,
//!   `abs`/`floor`/`ceil`/`trunc`/`sqrt`). Everything with JS-specific
//!   semantics — `%`, `**`, the `ToInt32` bitwise family, `Math.round`/
//!   `sign`/`fround`/`min`/`max`/`imul` — is a call back into the *same*
//!   `number_arith_raw` / `builtins::numbers` cores the interpreter uses, so
//!   results are bit-identical by construction (NaN, `-0`, shift masking —
//!   all of it).
//! - **Interrupts**: taken backward branches increment a poll counter and
//!   check the cooperative-interrupt flag every 256th time — the interpreter
//!   loop's exact cadence. An observed interrupt exits through the same
//!   store-everything path with [`INTERRUPTED`], and the caller runs the same
//!   latch-and-unwind as an interpreter-tier poll hit.
//!
//! Determinism: translation eligibility is a pure function of the kernel
//! (itself a pure function of the source), the compile trigger is the first
//! activation (deterministic engine state, not wall-clock), and the compiled
//! code computes bit-identical results — so record and replay execute
//! identically with the tier on, and a journal recorded with it off replays
//! with it on (and vice versa). `tests/jit.rs` runs the differential corpus
//! both ways and asserts byte-identical outcomes.
//!
//! ## Safety
//!
//! The crate's `forbid(unsafe_code)` drops to `deny(unsafe_code)` under this
//! feature (see `lib.rs`); every `unsafe` block in the crate lives in this
//! file, each individually `#[expect]`-scoped and commented:
//!
//! 1. transmuting the finalized code pointer to a typed function pointer,
//! 2. calling that pointer in [`NativeKernel::run`],
//! 3. freeing the module's executable memory on drop.
//!
//! The native code itself touches exactly two allocations, both owned by the
//! caller for the duration of the call: the `f64` register buffer (every
//! compiled index is validated `< n_regs ≤ KWIN` at translation, and the
//! caller passes a buffer of at least `KWIN` slots) and the one-byte
//! interrupt flag. It performs no allocation, no recursion, and calls only
//! the registered `extern "C"` helper shims below.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{types, AbiParam, Block, FuncRef, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

use crate::bytecode::{CmpOp, KMath, KOp, Kernel, KWIN};
use crate::exec::{number_arith_raw, ArithKind};

/// Return code of a native kernel run: the cooperative interrupt latched on a
/// back-edge poll. Any non-negative return is the code index of the
/// `Exit`/`Ret` op the program reached.
pub(crate) const INTERRUPTED: i64 = -1;

/// The compiled signature: `(regs: *mut f64, interrupt: *const u8) -> i64`.
/// `regs` is the caller's kernel register window (≥ [`KWIN`] slots);
/// `interrupt` points at the one-byte cooperative-interrupt flag (a shared
/// never-set `false` when the VM has none installed, so the compiled code
/// needs no null check).
type NativeFn = unsafe extern "C" fn(*mut f64, *const u8) -> i64;

/// Polled by compiled code when the VM has no interrupt flag installed.
/// Private and never written, so the load always sees `false`.
static NO_INTERRUPT: AtomicBool = AtomicBool::new(false);

// Tier observability (the `chidori-js-jit --jit-stats` report and the
// structural tests): counts are advisory only and never influence execution.
static STAT_COMPILED: AtomicU64 = AtomicU64::new(0);
static STAT_DECLINED: AtomicU64 = AtomicU64::new(0);
static STAT_RUNS: AtomicU64 = AtomicU64::new(0);

/// Process-wide tier counters (see [`stats`]).
#[derive(Clone, Copy, Debug)]
pub struct JitStats {
    /// Kernels successfully translated to native code.
    pub compiled: u64,
    /// Kernels that declined translation (an op outside the scalar subset,
    /// or an unsupported host); these keep running on the interpreter tier.
    pub declined: u64,
    /// Native kernel activations executed.
    pub native_runs: u64,
}

/// Snapshot the process-wide tier counters.
pub fn stats() -> JitStats {
    JitStats {
        compiled: STAT_COMPILED.load(Ordering::Relaxed),
        declined: STAT_DECLINED.load(Ordering::Relaxed),
        native_runs: STAT_RUNS.load(Ordering::Relaxed),
    }
}

/// The per-kernel compile-once cache slot ([`Kernel::native`]): empty until
/// the first activation with the tier enabled; then `Some(None)` for a kernel
/// whose translation declined (stay on the interpreter tier forever) or
/// `Some(Some(_))` for compiled code.
pub type NativeCache = OnceCell<Option<NativeKernel>>;

/// A kernel's compiled native code plus the [`JITModule`] that owns the
/// executable memory backing it. Cheaply cloneable (`Rc`); the memory is
/// freed when the last clone drops, and the function pointer is only
/// reachable through [`NativeKernel::run`], so it cannot outlive the memory.
#[derive(Clone)]
pub struct NativeKernel(Rc<Compiled>);

struct Compiled {
    /// `Some` until drop. Boxed into an `Option` only so `Drop` can move it
    /// out for `free_memory(self)`.
    module: Option<JITModule>,
    entry: NativeFn,
}

impl Drop for Compiled {
    fn drop(&mut self) {
        if let Some(module) = self.module.take() {
            // SAFETY: `entry` is the only pointer into this module's memory
            // and it is unreachable once `self` drops (it is never copied
            // out; `run` borrows `self`), so no thread can be executing or
            // about to execute the freed code.
            #[expect(unsafe_code, reason = "release the JIT module's executable memory")]
            unsafe {
                module.free_memory();
            }
        }
    }
}

impl std::fmt::Debug for NativeKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NativeKernel({:p})", self.0.entry as *const u8)
    }
}

impl NativeKernel {
    /// Run the compiled kernel over `regs` (the caller's loaded register
    /// window). Returns the code index of the `Exit`/`Ret` reached, or
    /// [`INTERRUPTED`]; either way every register has been stored back to
    /// `regs`, exactly as the interpreter tier's loop would have left them.
    pub(crate) fn run(&self, regs: &mut [f64], interrupt: Option<&AtomicBool>) -> i64 {
        // The compiled code indexes `regs` up to `n_regs - 1`, which
        // translation bounded by KWIN; both callers pass KWIN-sized windows.
        assert!(regs.len() >= KWIN, "kernel register window under-sized");
        let flag: &AtomicBool = interrupt.unwrap_or(&NO_INTERRUPT);
        STAT_RUNS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `entry` was compiled for exactly the signature `NativeFn`
        // from a verified Cranelift function; the module owning its memory is
        // alive (`self.0.module`). It reads/writes only `regs[0..n_regs]`
        // (indices validated `< n_regs ≤ KWIN` at translation; the buffer is
        // ≥ KWIN slots, asserted above) and loads the single byte at `flag`
        // (an `AtomicBool` is one byte with the same layout as `u8`; the
        // relaxed racy read matches the interpreter's `Relaxed` poll).
        #[expect(unsafe_code, reason = "call into JIT-compiled kernel code")]
        unsafe {
            (self.0.entry)(regs.as_mut_ptr(), flag as *const AtomicBool as *const u8)
        }
    }
}

/// Run kernel `k` natively over `regs` if it compiles (compiling it on first
/// use). `None` = this kernel is not JIT-eligible (or the host ISA is
/// unsupported) — the caller proceeds on the interpreter tier. `Some(code)`
/// = the kernel ran natively to completion; `code` is the index of the
/// `Exit`/`Ret` op reached, or [`INTERRUPTED`].
pub(crate) fn maybe_run(
    k: &Kernel,
    regs: &mut [f64],
    interrupt: Option<&AtomicBool>,
) -> Option<i64> {
    let native = k.native.get_or_init(|| match compile(k) {
        Some(n) => {
            STAT_COMPILED.fetch_add(1, Ordering::Relaxed);
            Some(n)
        }
        None => {
            STAT_DECLINED.fetch_add(1, Ordering::Relaxed);
            None
        }
    });
    native.as_ref().map(|n| n.run(regs, interrupt))
}

// ---------------------------------------------------------------------------
// Helper shims: the JS-semantics operations compiled code calls back into.
// Every shim delegates to the SAME core the interpreter tier uses, so native
// results are bit-identical by construction. All are total over f64 (no
// panics, no allocation).
// ---------------------------------------------------------------------------

extern "C" fn h_mod(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::Mod)
}
extern "C" fn h_pow(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::Pow)
}
extern "C" fn h_bitand(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::BitAnd)
}
extern "C" fn h_bitor(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::BitOr)
}
extern "C" fn h_bitxor(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::BitXor)
}
extern "C" fn h_shl(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::Shl)
}
extern "C" fn h_shr(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::Shr)
}
extern "C" fn h_ushr(a: f64, b: f64) -> f64 {
    number_arith_raw(a, b, ArithKind::UShr)
}
extern "C" fn h_bitnot(x: f64) -> f64 {
    !crate::vm::to_int32(x) as f64
}
extern "C" fn h_round(x: f64) -> f64 {
    crate::builtins::numbers::math_round(x)
}
extern "C" fn h_sign(x: f64) -> f64 {
    crate::builtins::numbers::math_sign(x)
}
extern "C" fn h_fround(x: f64) -> f64 {
    crate::builtins::numbers::math_fround(x)
}
extern "C" fn h_min2(a: f64, b: f64) -> f64 {
    crate::builtins::numbers::math_min2(a, b)
}
extern "C" fn h_max2(a: f64, b: f64) -> f64 {
    crate::builtins::numbers::math_max2(a, b)
}
extern "C" fn h_imul2(a: f64, b: f64) -> f64 {
    crate::builtins::numbers::math_imul2(a, b)
}

type Shim1 = extern "C" fn(f64) -> f64;
type Shim2 = extern "C" fn(f64, f64) -> f64;

/// `(symbol name, address, arity)` for every registered helper. Names are
/// namespaced to keep the JIT symbol table from ever shadowing a real symbol.
fn helper_table() -> [(&'static str, *const u8, u8); 16] {
    fn p1(f: Shim1) -> *const u8 {
        f as usize as *const u8
    }
    fn p2(f: Shim2) -> *const u8 {
        f as usize as *const u8
    }
    [
        ("cjit_mod", p2(h_mod), 2),
        ("cjit_pow", p2(h_pow), 2),
        ("cjit_bitand", p2(h_bitand), 2),
        ("cjit_bitor", p2(h_bitor), 2),
        ("cjit_bitxor", p2(h_bitxor), 2),
        ("cjit_shl", p2(h_shl), 2),
        ("cjit_shr", p2(h_shr), 2),
        ("cjit_ushr", p2(h_ushr), 2),
        ("cjit_bitnot", p1(h_bitnot), 1),
        ("cjit_round", p1(h_round), 1),
        ("cjit_sign", p1(h_sign), 1),
        ("cjit_fround", p1(h_fround), 1),
        ("cjit_min2", p2(h_min2), 2),
        ("cjit_max2", p2(h_max2), 2),
        ("cjit_pow2", p2(h_pow), 2),
        ("cjit_imul2", p2(h_imul2), 2),
    ]
}

/// Helper symbol for an [`ArithKind`] that is not inlined, or `None` for the
/// four bit-exact IEEE kinds emitted as native instructions.
fn arith_helper(kind: ArithKind) -> Option<&'static str> {
    match kind {
        ArithKind::Sub | ArithKind::Mul | ArithKind::Div => None,
        ArithKind::Mod => Some("cjit_mod"),
        ArithKind::Pow => Some("cjit_pow"),
        ArithKind::BitAnd => Some("cjit_bitand"),
        ArithKind::BitOr => Some("cjit_bitor"),
        ArithKind::BitXor => Some("cjit_bitxor"),
        ArithKind::Shl => Some("cjit_shl"),
        ArithKind::Shr => Some("cjit_shr"),
        ArithKind::UShr => Some("cjit_ushr"),
    }
}

/// JS numeric comparison ([`knum_cmp`](crate::exec) semantics) as a Cranelift
/// float condition. Ordered conditions are false on NaN (JS `<`,`<=`,`>`,`>=`,
/// `==`); `NotEqual` is unordered-or-unequal (JS `NaN != x` is true). Loose
/// and strict compare identically on two Numbers.
fn cmp_cc(cmp: CmpOp) -> FloatCC {
    match cmp {
        CmpOp::Eq | CmpOp::StrictEq => FloatCC::Equal,
        CmpOp::Ne | CmpOp::StrictNe => FloatCC::NotEqual,
        CmpOp::Lt => FloatCC::LessThan,
        CmpOp::Gt => FloatCC::GreaterThan,
        CmpOp::Le => FloatCC::LessThanOrEqual,
        CmpOp::Ge => FloatCC::GreaterThanOrEqual,
    }
}

// ---------------------------------------------------------------------------
// Eligibility
// ---------------------------------------------------------------------------

/// Whether every op in `k` is in the scalar subset this backend compiles,
/// with every register index `< n_regs`, every branch target in range, and
/// every fall-through / fused skip landing on a real op. A `false` pins the
/// kernel to the interpreter tier — never an error.
fn eligible(k: &Kernel) -> bool {
    let n_regs = k.n_regs as usize;
    let len = k.code.len();
    if n_regs > KWIN || len == 0 {
        return false;
    }
    let r = |i: u16| (i as usize) < n_regs;
    let t = |i: u16| (i as usize) < len;
    // `skip` past a fused op's landing pad must land on a real op; plain ops
    // must have a fall-through successor unless they are terminators.
    let next_ok = |pc: usize, skip: usize| pc + skip < len;
    k.code.iter().enumerate().all(|(pc, op)| match *op {
        KOp::Mov { dst, src } => r(dst) && r(src) && next_ok(pc, 1),
        KOp::Const { dst, .. } => r(dst) && next_ok(pc, 1),
        KOp::Add { dst, a, b } => r(dst) && r(a) && r(b) && next_ok(pc, 1),
        KOp::AddK { dst, a, .. } => r(dst) && r(a) && next_ok(pc, 1),
        KOp::Arith { dst, a, b, .. } => r(dst) && r(a) && r(b) && next_ok(pc, 1),
        KOp::ArithK { dst, a, .. } => r(dst) && r(a) && next_ok(pc, 1),
        KOp::Neg { dst, src } | KOp::BitNot { dst, src } => r(dst) && r(src) && next_ok(pc, 1),
        KOp::Mov2 { d1, s1, d2, s2 } => r(d1) && r(s1) && r(d2) && r(s2) && next_ok(pc, 2),
        KOp::ArithAdd {
            dst,
            a,
            b,
            d2,
            a2,
            b2,
            ..
        } => r(dst) && r(a) && r(b) && r(d2) && r(a2) && r(b2) && next_ok(pc, 2),
        KOp::ArithKAdd {
            dst, a, d2, a2, b2, ..
        } => r(dst) && r(a) && r(d2) && r(a2) && r(b2) && next_ok(pc, 2),
        KOp::AddKBr { dst, a, target, .. } => r(dst) && r(a) && t(target),
        KOp::Br { target } => t(target),
        KOp::BrCmp { a, b, target, .. } => r(a) && r(b) && t(target) && next_ok(pc, 1),
        KOp::BrCmpK { a, target, .. } => r(a) && t(target) && next_ok(pc, 1),
        KOp::BrFalsy { src, target } | KOp::BrTruthy { src, target } => {
            r(src) && t(target) && next_ok(pc, 1)
        }
        KOp::CmpSet { dst, a, b, .. } => r(dst) && r(a) && r(b) && next_ok(pc, 1),
        KOp::BoolNot { dst, src } => r(dst) && r(src) && next_ok(pc, 1),
        KOp::Math1 { kind, dst, src } => kind.arity() == 1 && r(dst) && r(src) && next_ok(pc, 1),
        KOp::Math2 { kind, dst, a, b } => {
            kind.arity() == 2 && r(dst) && r(a) && r(b) && next_ok(pc, 1)
        }
        KOp::Exit { .. } | KOp::Ret { .. } => true,
        // Outside the scalar subset: element/length/property access, pinned
        // string reads, pinned push/pop, pinned-callee and recursive calls.
        // The whole kernel stays on the interpreter tier.
        KOp::LoadElem { .. }
        | KOp::StoreElem { .. }
        | KOp::LoadElemAdd { .. }
        | KOp::LoadElemArith { .. }
        | KOp::LoadLen { .. }
        | KOp::LenBrCmp { .. }
        | KOp::ArrayPush { .. }
        | KOp::ArrayPop { .. }
        | KOp::StrLen { .. }
        | KOp::CharCodeAt { .. }
        | KOp::LoadProp { .. }
        | KOp::StoreProp { .. }
        | KOp::CallKernel { .. }
        | KOp::SelfCall { .. } => false,
    })
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// Compile `k` to native code, or `None` when it is not eligible or the host
/// ISA is unsupported. Pure: reads only the kernel's code.
fn compile(k: &Kernel) -> Option<NativeKernel> {
    if !eligible(k) {
        return None;
    }
    // Probe host support up front: `JITBuilder::with_flags` panics on an
    // unsupported host, and a decline must stay a decline.
    cranelift_native::builder().ok()?;
    let mut builder =
        JITBuilder::with_flags(&[("opt_level", "speed")], default_libcall_names()).ok()?;
    for (name, addr, _) in helper_table() {
        builder.symbol(name, addr);
    }
    let mut module = JITModule::new(builder);
    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    sig.params.push(AbiParam::new(ptr_ty));
    sig.returns.push(AbiParam::new(types::I64));
    let func_id = module
        .declare_function("kernel", Linkage::Export, &sig)
        .ok()?;
    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    let mut fb_ctx = FunctionBuilderContext::new();
    {
        let frontend_config = module.target_config();
        let builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        Translator::new(builder, &mut module, k).translate(frontend_config)?;
    }
    module.define_function(func_id, &mut ctx).ok()?;
    module.clear_context(&mut ctx);
    module.finalize_definitions().ok()?;
    let code = module.get_finalized_function(func_id);
    // SAFETY: `code` is the finalized entry of a function defined above with
    // exactly `NativeFn`'s signature (two pointer params, one i64 return,
    // the module's default call convention, which matches `extern "C"` on
    // every supported host).
    #[expect(unsafe_code, reason = "type the finalized JIT entry point")]
    let entry: NativeFn = unsafe { std::mem::transmute(code) };
    Some(NativeKernel(Rc::new(Compiled {
        module: Some(module),
        entry,
    })))
}

struct Translator<'a> {
    b: FunctionBuilder<'a>,
    module: &'a mut JITModule,
    k: &'a Kernel,
    /// One block per code index (the op's single entry point).
    blocks: Vec<Block>,
    /// Shared epilogue: stores every register back and returns its i64 param.
    exit_block: Block,
    /// Interrupt landing: routes to `exit_block` with [`INTERRUPTED`].
    intr_block: Block,
    /// F64 variable per kernel register.
    vars: Vec<Variable>,
    /// I32 taken-backward-branch counter (the interpreter's poll cadence).
    poll: Variable,
    regs_ptr: cranelift_codegen::ir::Value,
    int_ptr: cranelift_codegen::ir::Value,
    /// Lazily imported helper functions, by symbol name.
    helpers: HashMap<&'static str, FuncRef>,
    /// Back-edge trampolines created while emitting the current op, filled
    /// with the poll sequence after the op's terminator is placed.
    pending_backedges: Vec<(Block, usize)>,
}

impl<'a> Translator<'a> {
    fn new(mut b: FunctionBuilder<'a>, module: &'a mut JITModule, k: &'a Kernel) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let (regs_ptr, int_ptr) = {
            let params = b.block_params(entry);
            (params[0], params[1])
        };
        let n_regs = k.n_regs as usize;
        let mut vars = Vec::with_capacity(n_regs);
        for r in 0..n_regs {
            let var = b.declare_var(types::F64);
            let v = b.ins().load(
                types::F64,
                MemFlagsData::trusted(),
                regs_ptr,
                (8 * r) as i32,
            );
            b.def_var(var, v);
            vars.push(var);
        }
        let poll = b.declare_var(types::I32);
        let zero = b.ins().iconst(types::I32, 0);
        b.def_var(poll, zero);
        let blocks: Vec<Block> = (0..k.code.len()).map(|_| b.create_block()).collect();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I64);
        let intr_block = b.create_block();
        b.ins().jump(blocks[0], &[]);
        Translator {
            b,
            module,
            k,
            blocks,
            exit_block,
            intr_block,
            vars,
            poll,
            regs_ptr,
            int_ptr,
            helpers: HashMap::new(),
            pending_backedges: Vec::new(),
        }
    }

    fn helper(&mut self, name: &'static str) -> Option<FuncRef> {
        if let Some(&f) = self.helpers.get(name) {
            return Some(f);
        }
        let (_, _, arity) = helper_table().into_iter().find(|(n, _, _)| *n == name)?;
        let mut sig = self.module.make_signature();
        for _ in 0..arity {
            sig.params.push(AbiParam::new(types::F64));
        }
        sig.returns.push(AbiParam::new(types::F64));
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .ok()?;
        let f = self.module.declare_func_in_func(id, self.b.func);
        self.helpers.insert(name, f);
        Some(f)
    }

    fn call1(
        &mut self,
        name: &'static str,
        x: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        let f = self.helper(name)?;
        let call = self.b.ins().call(f, &[x]);
        Some(self.b.inst_results(call)[0])
    }

    fn call2(
        &mut self,
        name: &'static str,
        x: cranelift_codegen::ir::Value,
        y: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        let f = self.helper(name)?;
        let call = self.b.ins().call(f, &[x, y]);
        Some(self.b.inst_results(call)[0])
    }

    fn get(&mut self, r: u16) -> cranelift_codegen::ir::Value {
        self.b.use_var(self.vars[r as usize])
    }

    fn set(&mut self, r: u16, v: cranelift_codegen::ir::Value) {
        self.b.def_var(self.vars[r as usize], v);
    }

    /// `regs[a] <kind> regs-value b` with the interpreter's exact semantics:
    /// IEEE kinds inline, everything else through the shared helper.
    fn arith(
        &mut self,
        kind: ArithKind,
        a: cranelift_codegen::ir::Value,
        b: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        Some(match kind {
            ArithKind::Sub => self.b.ins().fsub(a, b),
            ArithKind::Mul => self.b.ins().fmul(a, b),
            ArithKind::Div => self.b.ins().fdiv(a, b),
            _ => {
                let name = arith_helper(kind).expect("non-IEEE kind has a helper");
                self.call2(name, a, b)?
            }
        })
    }

    /// ToBoolean on a scalar register: true unless `0`, `-0`, or NaN —
    /// exactly `fcmp ord_ne x, 0.0`.
    fn truthy(&mut self, x: cranelift_codegen::ir::Value) -> cranelift_codegen::ir::Value {
        let zero = self.b.ins().f64const(0.0);
        self.b.ins().fcmp(FloatCC::OrderedNotEqual, x, zero)
    }

    /// The block a branch from `pc` to `target` should jump to: the target's
    /// block directly for a forward branch; for a back-edge, a trampoline
    /// that runs the interpreter's poll sequence (count taken back-edges,
    /// every 256th check the interrupt byte) before reaching the target.
    fn dest(&mut self, pc: usize, target: usize) -> Block {
        if target > pc {
            return self.blocks[target];
        }
        let tramp = self.b.create_block();
        self.pending_backedges.push((tramp, target));
        tramp
    }

    /// Fill the back-edge trampolines queued while emitting an op. Called
    /// once the op's own terminator is placed (a block must be complete
    /// before switching away).
    fn flush_backedges(&mut self) {
        while let Some((tramp, target)) = self.pending_backedges.pop() {
            self.b.switch_to_block(tramp);
            let p = self.b.use_var(self.poll);
            let p2 = self.b.ins().iadd_imm_u(p, 1i64);
            self.b.def_var(self.poll, p2);
            let masked = self.b.ins().band_imm_u(p2, 0xFFi64);
            let check = self.b.create_block();
            // 255 of 256 times: straight to the target.
            self.b
                .ins()
                .brif(masked, self.blocks[target], &[], check, &[]);
            self.b.switch_to_block(check);
            let flag = self
                .b
                .ins()
                .load(types::I8, MemFlagsData::trusted(), self.int_ptr, 0);
            self.b
                .ins()
                .brif(flag, self.intr_block, &[], self.blocks[target], &[]);
        }
    }

    fn translate(
        mut self,
        frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Option<()> {
        let code = &self.k.code;
        for pc in 0..code.len() {
            self.b.switch_to_block(self.blocks[pc]);
            // Fall-through skip (1, or 2 past a fused op's landing pad);
            // `None` = the op placed its own terminator.
            let mut fallthrough = Some(1usize);
            match code[pc] {
                KOp::Mov { dst, src } => {
                    let v = self.get(src);
                    self.set(dst, v);
                }
                KOp::Const { dst, k } => {
                    let v = self.b.ins().f64const(k);
                    self.set(dst, v);
                }
                KOp::Add { dst, a, b } => {
                    let (x, y) = (self.get(a), self.get(b));
                    let v = self.b.ins().fadd(x, y);
                    self.set(dst, v);
                }
                KOp::AddK { dst, a, k } => {
                    let x = self.get(a);
                    let kk = self.b.ins().f64const(k);
                    let v = self.b.ins().fadd(x, kk);
                    self.set(dst, v);
                }
                KOp::Arith { kind, dst, a, b } => {
                    let (x, y) = (self.get(a), self.get(b));
                    let v = self.arith(kind, x, y)?;
                    self.set(dst, v);
                }
                KOp::ArithK { kind, dst, a, k } => {
                    let x = self.get(a);
                    let kk = self.b.ins().f64const(k);
                    let v = self.arith(kind, x, kk)?;
                    self.set(dst, v);
                }
                KOp::Neg { dst, src } => {
                    let x = self.get(src);
                    let v = self.b.ins().fneg(x);
                    self.set(dst, v);
                }
                KOp::BitNot { dst, src } => {
                    let x = self.get(src);
                    let v = self.call1("cjit_bitnot", x)?;
                    self.set(dst, v);
                }
                KOp::Mov2 { d1, s1, d2, s2 } => {
                    let v1 = self.get(s1);
                    self.set(d1, v1);
                    let v2 = self.get(s2);
                    self.set(d2, v2);
                    fallthrough = Some(2);
                }
                KOp::ArithAdd {
                    kind,
                    dst,
                    a,
                    b,
                    d2,
                    a2,
                    b2,
                } => {
                    let (x, y) = (self.get(a), self.get(b));
                    let v = self.arith(kind, x, y)?;
                    self.set(dst, v);
                    let (x2, y2) = (self.get(a2), self.get(b2));
                    let v2 = self.b.ins().fadd(x2, y2);
                    self.set(d2, v2);
                    fallthrough = Some(2);
                }
                KOp::ArithKAdd {
                    kind,
                    dst,
                    a,
                    k,
                    d2,
                    a2,
                    b2,
                } => {
                    let x = self.get(a);
                    let kk = self.b.ins().f64const(k);
                    let v = self.arith(kind, x, kk)?;
                    self.set(dst, v);
                    let (x2, y2) = (self.get(a2), self.get(b2));
                    let v2 = self.b.ins().fadd(x2, y2);
                    self.set(d2, v2);
                    fallthrough = Some(2);
                }
                KOp::AddKBr { dst, a, k, target } => {
                    let x = self.get(a);
                    let kk = self.b.ins().f64const(k);
                    let v = self.b.ins().fadd(x, kk);
                    self.set(dst, v);
                    let d = self.dest(pc, target as usize);
                    self.b.ins().jump(d, &[]);
                    fallthrough = None;
                }
                KOp::Br { target } => {
                    let d = self.dest(pc, target as usize);
                    self.b.ins().jump(d, &[]);
                    fallthrough = None;
                }
                KOp::BrCmp {
                    cmp,
                    a,
                    b,
                    if_true,
                    target,
                } => {
                    let (x, y) = (self.get(a), self.get(b));
                    let c = self.b.ins().fcmp(cmp_cc(cmp), x, y);
                    self.branch_on(c, if_true, pc, target as usize);
                    fallthrough = None;
                }
                KOp::BrCmpK {
                    cmp,
                    a,
                    k,
                    if_true,
                    target,
                } => {
                    let x = self.get(a);
                    let kk = self.b.ins().f64const(k);
                    let c = self.b.ins().fcmp(cmp_cc(cmp), x, kk);
                    self.branch_on(c, if_true, pc, target as usize);
                    fallthrough = None;
                }
                KOp::BrFalsy { src, target } => {
                    let x = self.get(src);
                    let c = self.truthy(x);
                    self.branch_on(c, false, pc, target as usize);
                    fallthrough = None;
                }
                KOp::BrTruthy { src, target } => {
                    let x = self.get(src);
                    let c = self.truthy(x);
                    self.branch_on(c, true, pc, target as usize);
                    fallthrough = None;
                }
                KOp::CmpSet { cmp, dst, a, b } => {
                    let (x, y) = (self.get(a), self.get(b));
                    let c = self.b.ins().fcmp(cmp_cc(cmp), x, y);
                    let one = self.b.ins().f64const(1.0);
                    let zero = self.b.ins().f64const(0.0);
                    let v = self.b.ins().select(c, one, zero);
                    self.set(dst, v);
                }
                KOp::BoolNot { dst, src } => {
                    let x = self.get(src);
                    let c = self.truthy(x);
                    let one = self.b.ins().f64const(1.0);
                    let zero = self.b.ins().f64const(0.0);
                    let v = self.b.ins().select(c, zero, one);
                    self.set(dst, v);
                }
                KOp::Math1 { kind, dst, src } => {
                    let x = self.get(src);
                    let v = match kind {
                        KMath::Abs => self.b.ins().fabs(x),
                        KMath::Floor => self.b.ins().floor(x),
                        KMath::Ceil => self.b.ins().ceil(x),
                        KMath::Trunc => self.b.ins().trunc(x),
                        KMath::Sqrt => self.b.ins().sqrt(x),
                        KMath::Round => self.call1("cjit_round", x)?,
                        KMath::Sign => self.call1("cjit_sign", x)?,
                        KMath::Fround => self.call1("cjit_fround", x)?,
                        // Binary kinds are excluded by `eligible` (arity 1).
                        KMath::Min2 | KMath::Max2 | KMath::Pow2 | KMath::Imul2 => return None,
                    };
                    self.set(dst, v);
                }
                KOp::Math2 { kind, dst, a, b } => {
                    let (x, y) = (self.get(a), self.get(b));
                    let name = match kind {
                        KMath::Min2 => "cjit_min2",
                        KMath::Max2 => "cjit_max2",
                        KMath::Pow2 => "cjit_pow2",
                        KMath::Imul2 => "cjit_imul2",
                        // Unary kinds are excluded by `eligible` (arity 2).
                        _ => return None,
                    };
                    let v = self.call2(name, x, y)?;
                    self.set(dst, v);
                }
                KOp::Exit { .. } | KOp::Ret { .. } => {
                    let pcv = self.b.ins().iconst(types::I64, pc as i64);
                    self.b.ins().jump(self.exit_block, &[pcv.into()]);
                    fallthrough = None;
                }
                // Excluded by `eligible`.
                KOp::LoadElem { .. }
                | KOp::StoreElem { .. }
                | KOp::LoadElemAdd { .. }
                | KOp::LoadElemArith { .. }
                | KOp::LoadLen { .. }
                | KOp::LenBrCmp { .. }
                | KOp::ArrayPush { .. }
                | KOp::ArrayPop { .. }
                | KOp::StrLen { .. }
                | KOp::CharCodeAt { .. }
                | KOp::LoadProp { .. }
                | KOp::StoreProp { .. }
                | KOp::CallKernel { .. }
                | KOp::SelfCall { .. } => return None,
            }
            if let Some(skip) = fallthrough {
                let next = self.blocks[pc + skip];
                self.b.ins().jump(next, &[]);
            }
            self.flush_backedges();
        }
        // Interrupt landing: exit with the sentinel (registers still stored —
        // the caller's latch-and-unwind reads them, like an interpreter poll
        // hit).
        self.b.switch_to_block(self.intr_block);
        let sentinel = self.b.ins().iconst(types::I64, INTERRUPTED);
        self.b.ins().jump(self.exit_block, &[sentinel.into()]);
        // Shared epilogue: store every register back, return the exit code.
        self.b.switch_to_block(self.exit_block);
        let code_v = self.b.block_params(self.exit_block)[0];
        for r in 0..self.vars.len() {
            let v = self.b.use_var(self.vars[r]);
            self.b
                .ins()
                .store(MemFlagsData::trusted(), v, self.regs_ptr, (8 * r) as i32);
        }
        self.b.ins().return_(&[code_v]);
        self.b.seal_all_blocks();
        self.b.finalize(frontend_config);
        Some(())
    }

    /// Emit `if cond == if_true { goto target (via back-edge poll) } else
    /// { fall through }` — the shared shape of every conditional branch.
    fn branch_on(
        &mut self,
        cond: cranelift_codegen::ir::Value,
        if_true: bool,
        pc: usize,
        target: usize,
    ) {
        let taken = self.dest(pc, target);
        let fall = self.blocks[pc + 1];
        if if_true {
            self.b.ins().brif(cond, taken, &[], fall, &[]);
        } else {
            self.b.ins().brif(cond, fall, &[], taken, &[]);
        }
    }
}
