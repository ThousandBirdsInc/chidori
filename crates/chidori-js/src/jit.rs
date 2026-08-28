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
//! 1. transmuting the finalized code pointers to typed function pointers,
//! 2. calling them in [`NativeKernel::run`],
//! 3. freeing each module's executable memory on drop,
//! 4. the element shims reconstructing the caller-owned activation tables
//!    from the [`JitCtx`] the live native run was invoked with,
//! 5. the one-time [`dense_layout_ok`] self-check of `Value`'s `#[repr(u8)]`
//!    layout contract.
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

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, AbiParam, Block, FuncRef, InstBuilder, MemFlagsData};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

use crate::bytecode::{CmpOp, KMath, KOp, Kernel, KWIN};
use crate::exec::{number_arith_raw, ArithKind};

/// Return code of a native kernel run: the cooperative interrupt latched on a
/// back-edge poll. Any non-negative return is the code index of the
/// `Exit`/`Ret` op the program reached (for a recursive kernel, success is
/// `0` with the result in `JitCtx::scratch`).
pub(crate) const INTERRUPTED: i64 = -1;
/// Return code of a native RECURSIVE kernel run: the depth budget ran out —
/// the (pure) activation is abandoned and the caller re-runs generically,
/// which raises the spec RangeError from the exact frame it belongs to.
pub(crate) const REC_ABANDONED: i64 = -2;

/// The compiled signature:
/// `(regs: *mut f64, interrupt: *const u8, ctx: *mut JitCtx) -> i64`.
/// `regs` is the caller's kernel register window (≥ [`KWIN`] slots);
/// `interrupt` points at the one-byte cooperative-interrupt flag (a shared
/// never-set `false` when the VM has none installed, so the compiled code
/// needs no null check); `ctx` carries the activation tables ([`JitCtx`]).
type NativeFn = unsafe extern "C" fn(*mut f64, *const u8, *mut JitCtx) -> i64;

/// One pinned flat-ASCII string base (per kernel sslot): the byte pointer
/// and length the entry guard validated. Strings are immutable and pinned
/// for the activation, so compiled code hoists these at entry and reads
/// bytes directly (`StrLen`/`CharCodeAt` are total — no bail exists).
#[repr(C)]
pub(crate) struct SStr {
    pub ptr: *const u8,
    pub len: u64,
}

/// A DIRECT element view over the base pinned in an oslot, when one exists —
/// every field an activation constant (resizes, detaches, element-kind and
/// property-map changes, and — for the dense form, granted only to kernels
/// with no element stores — length changes all require calls, which kernel
/// regions exclude):
///
/// - [`ElemView::TA_F64`]: an f64 typed array; `ptr` is raw element storage
///   (`buffer bytes + byte_offset`), `len` the effective element count. An
///   element access is a plain 8-byte little-endian load/store — exactly
///   `decode`/`encode` for `TAKind::F64`.
/// - [`ElemView::DENSE`]: an unshadowed dense array in a kernel that
///   provably performs no element store/push/pop; `ptr` is the `Vec<Value>`
///   storage, `len` its length. A read checks the slot's `#[repr(u8)]` tag
///   ([`Value::JIT_NUMBER_TAG`]) and loads the f64 payload — a non-`Number`
///   slot (hole included) takes the op's bail edge, exactly like the
///   interpreter's fast-path miss. Granted only after
///   [`dense_layout_ok`]'s live layout self-check.
/// - [`ElemView::NONE`] (`kind == 0`): no direct view — the access goes
///   through the helper shims, which re-run the interpreter's own fast-path
///   core per access.
#[repr(C)]
pub(crate) struct ElemView {
    pub ptr: *mut u8,
    pub len: u64,
    pub kind: u64,
}

impl ElemView {
    pub(crate) const NONE: u64 = 0;
    pub(crate) const TA_F64: u64 = 1;
    pub(crate) const DENSE: u64 = 2;

    fn none() -> ElemView {
        ElemView {
            ptr: std::ptr::null_mut(),
            len: 0,
            kind: ElemView::NONE,
        }
    }
}

/// One-time self-check of the `#[repr(u8)]` dense-slot contract
/// ([`Value::JIT_NUMBER_TAG`] / [`Value::JIT_NUMBER_PAYLOAD_OFFSET`])
/// against a live value: belt-and-braces under the guaranteed RFC 2195
/// layout — if it ever failed, dense views are simply never granted and
/// every dense access takes the helper shims.
fn dense_layout_ok() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let magic = f64::from_bits(0x0123_4567_89AB_CDEF);
        let probe = crate::value::Value::Number(magic);
        let base = (&probe as *const crate::value::Value).cast::<u8>();
        // SAFETY: reading the tag byte and the payload bytes of the live
        // `Number` variant — both initialized for this variant (padding
        // bytes between them are never read).
        #[expect(unsafe_code, reason = "verify the dense-slot layout contract")]
        let (tag, payload) = unsafe {
            (
                base.read(),
                base.add(crate::value::Value::JIT_NUMBER_PAYLOAD_OFFSET)
                    .cast::<f64>()
                    .read_unaligned(),
            )
        };
        tag == crate::value::Value::JIT_NUMBER_TAG && payload.to_bits() == magic.to_bits()
    })
}

/// Per-activation context handed to compiled kernels as their third
/// parameter. Built on the caller's stack around each native run; every
/// pointer is valid for exactly the duration of the call (the caller keeps
/// the owning `JsString`s/`JsObject`s alive across it).
#[repr(C)]
pub(crate) struct JitCtx {
    /// Pinned string table, one [`SStr`] per kernel sslot (null iff none).
    pub sstr: *const SStr,
    /// The activation's pinned object bases (the caller's `objs` cache),
    /// one per kernel oslot — the element helper shims index this.
    pub objs: *const crate::value::JsObject,
    pub n_objs: u64,
    /// Direct element views, one [`ElemView`] per oslot (null iff no
    /// oslots).
    pub ta: *const ElemView,
    /// The realm's canonical `Array.prototype`, for the push receiver check
    /// (null unless the kernel uses push/pop).
    pub array_proto: *const crate::value::JsObject,
    /// Helper out-param: element loads / lengths / push results land here
    /// when the shim reports success — and a recursive kernel's result on a
    /// `0` return.
    pub scratch: f64,
    /// RECURSIVE kernels: the remaining call-depth budget
    /// (`max_call_depth - call_depth`), decremented down the native
    /// recursion; a self-call with none left flags `abandon = 1`.
    pub depth: i64,
    /// RECURSIVE kernels: 0 = running; 1 = depth budget exhausted; 2 = the
    /// cooperative interrupt latched. Set deep in the recursion and checked
    /// after every self-call, unwinding every native frame immediately.
    pub abandon: u64,
    /// RECURSIVE kernels: the shared self-call poll counter (the
    /// interrupt-check cadence lives across frames, like the interpreter's).
    pub poll: u64,
}

impl JitCtx {
    /// The empty context for kernels with no oslots/sslots (every function
    /// kernel): eligibility guarantees compiled code never dereferences the
    /// tables.
    pub(crate) fn empty() -> JitCtx {
        JitCtx {
            sstr: std::ptr::null(),
            objs: std::ptr::null(),
            n_objs: 0,
            ta: std::ptr::null(),
            array_proto: std::ptr::null(),
            scratch: 0.0,
            depth: 0,
            abandon: 0,
            poll: 0,
        }
    }
}

/// Build the direct-view table entry for one pinned oslot object (see
/// [`ElemView`]). `allow_dense` = the kernel provably performs no element
/// store/push/pop, so a dense array's storage pointer and length are
/// activation constants.
pub(crate) fn elem_view(o: &crate::value::JsObject, allow_dense: bool) -> ElemView {
    if !cfg!(target_endian = "little") {
        return ElemView::none();
    }
    {
        let b = o.borrow();
        match &b.internal {
            crate::value::Internal::Array(arr) if allow_dense && b.own_is_empty() => {
                if dense_layout_ok() {
                    // Read-only raw view over the Vec<Value> storage: derived
                    // from a shared borrow, read (never written) by native
                    // code while the caller's `objs` cache keeps the array
                    // alive and nothing can reallocate it (no in-kernel
                    // stores by the `allow_dense` grant, no calls at all).
                    return ElemView {
                        ptr: arr.as_ptr() as *mut u8,
                        len: arr.len() as u64,
                        kind: ElemView::DENSE,
                    };
                }
                return ElemView::none();
            }
            crate::value::Internal::TypedArray(t)
                if matches!(t.kind, crate::value::TAKind::F64) =>
            {
                // Fall through below (needs the buffer borrow_mut, which
                // this object borrow must not overlap for aliased views).
            }
            _ => return ElemView::none(),
        }
    }
    let b = o.borrow();
    let crate::value::Internal::TypedArray(t) = &b.internal else {
        return ElemView::none();
    };
    let len = crate::typed_array::ta_eff_length(t);
    if len == 0 {
        return ElemView::none();
    }
    let byte_offset = t.byte_offset;
    let mut buf = t.buffer.borrow_mut();
    let crate::value::Internal::ArrayBuffer(Some(bytes)) = &mut buf.internal else {
        return ElemView::none();
    };
    // The raw pointer is used only while the caller's `objs` cache keeps the
    // buffer alive and nothing else can run (kernel regions contain no
    // calls); the `RefMut` is dropped before the native call, so no Rust
    // reference aliases the accesses.
    ElemView {
        ptr: bytes[byte_offset..].as_mut_ptr(),
        len: len as u64,
        kind: ElemView::TA_F64,
    }
}

/// Polled by compiled code when the VM has no interrupt flag installed.
/// Private and never written, so the load always sees `false`.
static NO_INTERRUPT: AtomicBool = AtomicBool::new(false);

// Tier observability (the `chidori-js-jit --jit-stats` report and the
// structural tests): counts are advisory only and never influence execution.
static STAT_COMPILED: AtomicU64 = AtomicU64::new(0);
static STAT_DECLINED: AtomicU64 = AtomicU64::new(0);
static STAT_RUNS: AtomicU64 = AtomicU64::new(0);
static STAT_ELEM_SHIM: AtomicU64 = AtomicU64::new(0);

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
    /// Element accesses that took a helper shim (no direct view) — the
    /// observability handle for "is this loop on the raw path?".
    pub elem_shim_calls: u64,
}

/// Snapshot the process-wide tier counters.
pub fn stats() -> JitStats {
    JitStats {
        compiled: STAT_COMPILED.load(Ordering::Relaxed),
        declined: STAT_DECLINED.load(Ordering::Relaxed),
        native_runs: STAT_RUNS.load(Ordering::Relaxed),
        elem_shim_calls: STAT_ELEM_SHIM.load(Ordering::Relaxed),
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
    /// The pinned-callee protos this code inlined (empty for kernels with no
    /// `CallKernel`): compiled against the COMPILING activation's resolved
    /// callees, so every later activation identity-checks its own resolution
    /// against these — a mismatch (the same call site pinning a different
    /// function) runs that activation on the interpreter tier.
    callees: Vec<Rc<crate::bytecode::FuncProto>>,
    /// Slots the compiled code addresses in the register buffer: window 0
    /// plus one KWIN-strided window per inlined callee.
    min_regs: usize,
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
    pub(crate) fn run(
        &self,
        regs: &mut [f64],
        interrupt: Option<&AtomicBool>,
        ctx: &mut JitCtx,
    ) -> i64 {
        // The compiled code indexes window 0 (bounded by KWIN at
        // translation) plus one KWIN-strided window per inlined callee —
        // exactly the buffer the interpreter tier sizes for the same kernel.
        assert!(
            regs.len() >= self.0.min_regs,
            "kernel register buffer under-sized"
        );
        let flag: &AtomicBool = interrupt.unwrap_or(&NO_INTERRUPT);
        STAT_RUNS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `entry` was compiled for exactly the signature `NativeFn`
        // from a verified Cranelift function; the module owning its memory is
        // alive (`self.0.module`). It reads/writes only `regs[0..n_regs]`
        // (indices validated `< n_regs ≤ KWIN` at translation; the buffer is
        // ≥ KWIN slots, asserted above), loads the single byte at `flag`
        // (an `AtomicBool` is one byte with the same layout as `u8`; the
        // relaxed racy read matches the interpreter's `Relaxed` poll), and
        // touches `ctx`'s tables, whose every pointer the caller keeps valid
        // for the duration of this call, sized one entry per oslot/sslot of
        // the kernel this code was compiled from.
        #[expect(unsafe_code, reason = "call into JIT-compiled kernel code")]
        unsafe {
            (self.0.entry)(
                regs.as_mut_ptr(),
                flag as *const AtomicBool as *const u8,
                ctx as *mut JitCtx,
            )
        }
    }
}

/// The compiled form of `k`, compiling it on first use against the resolved
/// pinned callees of the compiling activation (`callee_bfs` — empty for
/// kernels without `CallKernel`). `None` = the kernel is not JIT-eligible,
/// the host ISA is unsupported, or THIS activation resolved different
/// callees than the code was compiled against — the caller proceeds on the
/// interpreter tier. On `Some`, the caller builds the activation's
/// [`JitCtx`] and invokes [`NativeKernel::run`].
pub(crate) fn native_for_loop(
    k: &Kernel,
    callee_bfs: &[(Rc<crate::value::BytecodeFunction>, u32)],
) -> Option<NativeKernel> {
    let native = k.native.get_or_init(|| {
        let specs: Vec<(Rc<crate::bytecode::FuncProto>, u32)> = callee_bfs
            .iter()
            .map(|(bf, wb)| (bf.proto.clone(), *wb))
            .collect();
        match compile(k, &specs) {
            Some(n) => {
                STAT_COMPILED.fetch_add(1, Ordering::Relaxed);
                Some(n)
            }
            None => {
                STAT_DECLINED.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    });
    let nk = native.as_ref()?;
    // Identity guard: this activation's resolved callees must be the very
    // protos the code inlined (closure INSTANCES may differ — their upvalue
    // snapshots live in the register buffer — but the compiled bodies must
    // match exactly).
    if nk.0.callees.len() != callee_bfs.len()
        || !nk
            .0
            .callees
            .iter()
            .zip(callee_bfs)
            .all(|(compiled, (bf, _))| Rc::ptr_eq(compiled, &bf.proto))
    {
        return None;
    }
    Some(nk.clone())
}

/// [`native_for_loop`] for contexts with no pinned callees (the function-
/// kernel seam; function kernels contain no `CallKernel` by construction).
pub(crate) fn native_for(k: &Kernel) -> Option<NativeKernel> {
    native_for_loop(k, &[])
}

/// The compiled form of a SELF-ONLY recursive function kernel (the
/// `run_fn_kernel_rec` seam), compiling on first use. Recursive kernels are
/// reached exclusively through the windowed-executor path, so the same
/// per-kernel cache slot holds this compilation.
pub(crate) fn native_for_rec(k: &Kernel) -> Option<NativeKernel> {
    k.native
        .get_or_init(|| match compile_rec(k) {
            Some(n) => {
                STAT_COMPILED.fetch_add(1, Ordering::Relaxed);
                Some(n)
            }
            None => {
                STAT_DECLINED.fetch_add(1, Ordering::Relaxed);
                None
            }
        })
        .clone()
}

/// Convenience for the FUNCTION-kernel seam (no oslots/sslots by
/// construction): compile-or-fetch and run with the empty context.
pub(crate) fn maybe_run(
    k: &Kernel,
    regs: &mut [f64],
    interrupt: Option<&AtomicBool>,
) -> Option<i64> {
    let native = native_for(k)?;
    let mut ctx = JitCtx::empty();
    Some(native.run(regs, interrupt, &mut ctx))
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
/// Cold tail of the inlined ToInt32 (see `Translator::toint32`): NaN, ±Inf,
/// and magnitudes at or beyond 2^63, where a saturating i64 conversion's low
/// 32 bits diverge from the spec's mod-2^32. Returns the exact int32 value
/// as an f64 (always in [-2^31, 2^31), so the caller's conversion back is
/// exact).
extern "C" fn h_toint32(x: f64) -> f64 {
    crate::vm::to_int32(x) as f64
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

// ---- element shims: the interpreter's own fast-path cores, reached from
// native code through the activation's [`JitCtx`]. Each returns 1 for
// success (result, where there is one, in `ctx.scratch`) and 0 for "take the
// op's bail edge". The `unsafe` here is one pattern repeated: reconstruct
// the caller's context and object table from the raw pointers it built
// immediately around the native run — valid for exactly that call, sized
// one entry per kernel oslot (`oslot` itself validated at translation).

/// # Safety
/// `ctx` must be the [`JitCtx`] the active native run was invoked with (see
/// [`NativeKernel::run`]'s safety comment).
#[expect(unsafe_code, reason = "the element shims' shared context contract")]
unsafe fn ctx_parts<'a>(ctx: *mut JitCtx) -> (&'a mut JitCtx, &'a [crate::value::JsObject]) {
    // SAFETY: per the function contract — the caller's stack-built context
    // and its object table, alive across the native call this shim serves.
    // The slice does not overlap `ctx` itself.
    #[expect(unsafe_code, reason = "reconstruct the activation's tables")]
    unsafe {
        let objs = if (*ctx).objs.is_null() {
            &[]
        } else {
            std::slice::from_raw_parts((*ctx).objs, (*ctx).n_objs as usize)
        };
        (&mut *ctx, objs)
    }
}

extern "C" fn h_elem_load(ctx: *mut JitCtx, oslot: i64, idx: f64) -> i8 {
    STAT_ELEM_SHIM.fetch_add(1, Ordering::Relaxed);
    // SAFETY: shim invoked only from a live native run (see `ctx_parts`).
    #[expect(unsafe_code, reason = "element shim over the activation tables")]
    let (ctx, objs) = unsafe { ctx_parts(ctx) };
    match crate::exec::kernel_elem_load(objs, oslot as usize, idx) {
        Some(n) => {
            ctx.scratch = n;
            1
        }
        None => 0,
    }
}

extern "C" fn h_elem_store(ctx: *mut JitCtx, oslot: i64, idx: f64, val: f64) -> i8 {
    // SAFETY: as `h_elem_load`.
    #[expect(unsafe_code, reason = "element shim over the activation tables")]
    let (_, objs) = unsafe { ctx_parts(ctx) };
    i8::from(crate::exec::kernel_elem_store(
        objs,
        oslot as usize,
        idx,
        val,
    ))
}

extern "C" fn h_elem_len(ctx: *mut JitCtx, oslot: i64) -> i8 {
    // SAFETY: as `h_elem_load`.
    #[expect(unsafe_code, reason = "element shim over the activation tables")]
    let (ctx, objs) = unsafe { ctx_parts(ctx) };
    match crate::exec::kernel_elem_len(objs, oslot as usize) {
        Some(n) => {
            ctx.scratch = n;
            1
        }
        None => 0,
    }
}

extern "C" fn h_array_push(ctx: *mut JitCtx, oslot: i64, val: f64) -> i8 {
    // SAFETY: as `h_elem_load`; `array_proto` is set (to the realm's
    // canonical, which outlives the call) whenever the kernel contains a
    // push — null-checked anyway, declining into the bail edge.
    #[expect(unsafe_code, reason = "element shim over the activation tables")]
    let (ctx, objs, proto) = unsafe {
        let (c, o) = ctx_parts(ctx);
        if c.array_proto.is_null() {
            return 0;
        }
        let p = &*c.array_proto;
        (c, o, p)
    };
    match crate::exec::kernel_array_push(objs, oslot as usize, proto, val) {
        Some(len) => {
            ctx.scratch = len;
            1
        }
        None => 0,
    }
}

extern "C" fn h_array_pop(ctx: *mut JitCtx, oslot: i64) -> i8 {
    // SAFETY: as `h_elem_load`.
    #[expect(unsafe_code, reason = "element shim over the activation tables")]
    let (ctx, objs) = unsafe { ctx_parts(ctx) };
    match crate::exec::kernel_array_pop(objs, oslot as usize) {
        Some(n) => {
            ctx.scratch = n;
            1
        }
        None => 0,
    }
}

/// Parameter/return atoms of a helper shim's C ABI.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Abi {
    F64,
    I64,
    I8,
    Ptr,
}

type Shim1 = extern "C" fn(f64) -> f64;
type Shim2 = extern "C" fn(f64, f64) -> f64;

/// `(symbol name, address, params, return)` for every registered helper.
/// Names are namespaced to keep the JIT symbol table from ever shadowing a
/// real symbol.
fn helper_table() -> [(&'static str, *const u8, &'static [Abi], Abi); 22] {
    fn p1(f: Shim1) -> *const u8 {
        f as usize as *const u8
    }
    fn p2(f: Shim2) -> *const u8 {
        f as usize as *const u8
    }
    const F1: &[Abi] = &[Abi::F64];
    const F2: &[Abi] = &[Abi::F64, Abi::F64];
    const ELEM2: &[Abi] = &[Abi::Ptr, Abi::I64];
    const ELEM3: &[Abi] = &[Abi::Ptr, Abi::I64, Abi::F64];
    const ELEM4: &[Abi] = &[Abi::Ptr, Abi::I64, Abi::F64, Abi::F64];
    [
        ("cjit_mod", p2(h_mod), F2, Abi::F64),
        ("cjit_pow", p2(h_pow), F2, Abi::F64),
        ("cjit_bitand", p2(h_bitand), F2, Abi::F64),
        ("cjit_bitor", p2(h_bitor), F2, Abi::F64),
        ("cjit_bitxor", p2(h_bitxor), F2, Abi::F64),
        ("cjit_shl", p2(h_shl), F2, Abi::F64),
        ("cjit_shr", p2(h_shr), F2, Abi::F64),
        ("cjit_ushr", p2(h_ushr), F2, Abi::F64),
        ("cjit_bitnot", p1(h_bitnot), F1, Abi::F64),
        ("cjit_round", p1(h_round), F1, Abi::F64),
        ("cjit_sign", p1(h_sign), F1, Abi::F64),
        ("cjit_fround", p1(h_fround), F1, Abi::F64),
        ("cjit_min2", p2(h_min2), F2, Abi::F64),
        ("cjit_max2", p2(h_max2), F2, Abi::F64),
        ("cjit_pow2", p2(h_pow), F2, Abi::F64),
        ("cjit_imul2", p2(h_imul2), F2, Abi::F64),
        ("cjit_toint32", p1(h_toint32), F1, Abi::F64),
        (
            "cjit_elem_load",
            h_elem_load as extern "C" fn(*mut JitCtx, i64, f64) -> i8 as usize as *const u8,
            ELEM3,
            Abi::I8,
        ),
        (
            "cjit_elem_store",
            h_elem_store as extern "C" fn(*mut JitCtx, i64, f64, f64) -> i8 as usize as *const u8,
            ELEM4,
            Abi::I8,
        ),
        (
            "cjit_elem_len",
            h_elem_len as extern "C" fn(*mut JitCtx, i64) -> i8 as usize as *const u8,
            ELEM2,
            Abi::I8,
        ),
        (
            "cjit_array_push",
            h_array_push as extern "C" fn(*mut JitCtx, i64, f64) -> i8 as usize as *const u8,
            ELEM3,
            Abi::I8,
        ),
        (
            "cjit_array_pop",
            h_array_pop as extern "C" fn(*mut JitCtx, i64) -> i8 as usize as *const u8,
            ELEM2,
            Abi::I8,
        ),
    ]
}

/// `2^63` — the magnitude bound under which `fcvt_to_sint_sat.i64` +
/// truncation computes ToInt32's mod-2^32 exactly (see
/// [`Translator::toint32`]).
const TOINT32_FAST_BOUND: f64 = 9_223_372_036_854_775_808.0;
/// `2^53` — the integer-exactness bound of the interpreter's `js_mod` fast
/// path, mirrored by [`Translator::emit_mod`].
const MOD_FAST_BOUND: f64 = 9_007_199_254_740_992.0;

/// Whether `%` by the compile-time constant `k` can take the inlined integer
/// path unconditionally on the right operand: integral, within the exact-i64
/// bound, nonzero — the `bi` half of `js_mod`'s fast-path guard, decided at
/// translation instead of per iteration.
fn mod_const_rhs(k: f64) -> Option<i64> {
    let ki = k as i64;
    (ki as f64 == k && k.abs() <= MOD_FAST_BOUND && ki != 0).then_some(ki)
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

/// Whether a pinned callee's function kernel can be INLINED at its
/// `CallKernel` sites: the compiled subset minus `Exit` (a frameless callee
/// has nothing to exit to), non-recursive, cell-write-free (per-call cell
/// flushes are the interpreter tier's), and boolean-return-free (the caller
/// guard already requires Number returns; checked again here).
fn callee_eligible(ck: &Kernel) -> bool {
    ck.rec.is_none()
        && ck.uv_writes.is_empty()
        && eligible(ck, &[])
        && !ck
            .code
            .iter()
            .any(|op| matches!(op, KOp::Exit { .. } | KOp::Ret { boolean: true, .. }))
}

/// Whether a SELF-ONLY recursive function kernel can compile to a native
/// recursive function ([`compile_rec`]): no mutual-recursion partners, no
/// cell writes, a frameless scalar body (function kernels have no
/// oslots/sslots/props by construction — checked anyway), and every op in
/// the subset with `SelfCall` allowed (callee 0 only) and `Exit` rejected.
fn rec_eligible(k: &Kernel) -> bool {
    k.rec.as_deref().is_some_and(|rec| rec.globals.is_empty())
        && k.uv_writes.is_empty()
        && k.oslots.is_empty()
        && k.sslots.is_empty()
        && k.props_used.is_empty()
        && eligible_inner(k, &[], true)
}

/// Whether every op in `k` is in the subset this backend compiles, with
/// every register index `< n_regs`, every branch target in range, every
/// fall-through / fused skip landing on a real op, and every `CallKernel`
/// aimed at an inlinable resolved callee. A `false` pins the kernel to the
/// interpreter tier — never an error.
fn eligible(k: &Kernel, callees: &[(Rc<crate::bytecode::FuncProto>, u32)]) -> bool {
    eligible_inner(k, callees, false)
}

/// `allow_selfcall` selects the RECURSIVE-body dialect: `SelfCall` (callee
/// 0) becomes legal and `Exit` illegal (a frameless recursive body has
/// nothing to exit to).
fn eligible_inner(
    k: &Kernel,
    callees: &[(Rc<crate::bytecode::FuncProto>, u32)],
    allow_selfcall: bool,
) -> bool {
    let n_regs = k.n_regs as usize;
    let len = k.code.len();
    if n_regs > KWIN || len == 0 {
        return false;
    }
    let r = |i: u16| (i as usize) < n_regs;
    let t = |i: u16| (i as usize) < len;
    let o = |i: u16| (i as usize) < k.oslots.len();
    let st = |i: u16| (i as usize) < k.sslots.len();
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
        KOp::Exit { .. } => !allow_selfcall,
        KOp::Ret { .. } => true,
        KOp::SelfCall {
            dst, base, callee, ..
        } => {
            allow_selfcall
                && callee == 0
                && r(dst)
                && (base as usize + k.args_used as usize) <= n_regs
                && next_ok(pc, 1)
        }
        KOp::LoadElem {
            dst,
            obj,
            idx,
            bail,
        } => r(dst) && o(obj) && r(idx) && t(bail) && next_ok(pc, 1),
        KOp::StoreElem {
            obj,
            idx,
            val,
            bail,
        } => o(obj) && r(idx) && r(val) && t(bail) && next_ok(pc, 1),
        KOp::LoadElemAdd {
            dst,
            obj,
            idx,
            bail,
            d2,
            a2,
            b2,
        } => r(dst) && o(obj) && r(idx) && t(bail) && r(d2) && r(a2) && r(b2) && next_ok(pc, 2),
        KOp::LoadElemArith {
            dst,
            obj,
            idx,
            bail,
            d2,
            a2,
            b2,
            ..
        } => r(dst) && o(obj) && r(idx) && t(bail) && r(d2) && r(a2) && r(b2) && next_ok(pc, 2),
        KOp::LoadLen { dst, obj, bail } => r(dst) && o(obj) && t(bail) && next_ok(pc, 1),
        KOp::LenBrCmp {
            dst,
            obj,
            bail,
            a,
            b,
            target,
            ..
        } => r(dst) && o(obj) && t(bail) && r(a) && r(b) && t(target) && next_ok(pc, 2),
        KOp::ArrayPush {
            obj,
            val,
            dst,
            bail,
        } => o(obj) && r(val) && r(dst) && t(bail) && next_ok(pc, 1),
        KOp::ArrayPop { obj, dst, bail } => o(obj) && r(dst) && t(bail) && next_ok(pc, 1),
        KOp::StrLen { dst, str } => r(dst) && st(str) && next_ok(pc, 1),
        KOp::CharCodeAt { dst, str, idx } => r(dst) && st(str) && r(idx) && next_ok(pc, 1),
        KOp::CallKernel {
            dst,
            fslot,
            base,
            argc,
        } => {
            r(dst)
                && next_ok(pc, 1)
                && (base as usize + argc as usize) <= n_regs
                && callees
                    .get(fslot as usize)
                    .and_then(|(proto, _)| proto.fn_kernel.as_ref())
                    .is_some_and(callee_eligible)
        }
        // Outside the compiled subset: localized property ops (rewritten
        // away at kernel build — surviving ones are the tier bug the
        // interpreter arm reports). The whole kernel stays on the
        // interpreter tier.
        KOp::LoadProp { .. } | KOp::StoreProp { .. } => false,
    })
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// Compile `k` to native code against the resolved pinned callees, or
/// `None` when it is not eligible or the host ISA is unsupported. Pure:
/// reads only the kernel's (and inlined callee kernels') code.
fn compile(k: &Kernel, callees: &[(Rc<crate::bytecode::FuncProto>, u32)]) -> Option<NativeKernel> {
    if !eligible(k, callees) {
        return None;
    }
    // A kernel with pinned-callee slots must be compiled against exactly its
    // resolved callees (an activation with a declined callee resolution
    // never reaches compile with a short list, but stay defensive).
    if callees.len() != k.callee_slots.len() {
        return None;
    }
    // Probe host support up front: `JITBuilder::with_flags` panics on an
    // unsupported host, and a decline must stay a decline.
    cranelift_native::builder().ok()?;
    let mut builder =
        JITBuilder::with_flags(&[("opt_level", "speed")], default_libcall_names()).ok()?;
    for (name, addr, _, _) in helper_table() {
        builder.symbol(name, addr);
    }
    let mut module = JITModule::new(builder);
    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
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
        Translator::new(builder, &mut module, k, callees).translate(frontend_config)?;
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
        callees: callees.iter().map(|(p, _)| p.clone()).collect(),
        min_regs: KWIN * (1 + callees.len()),
    })))
}

/// Compile a SELF-ONLY recursive function kernel ([`rec_eligible`]) into a
/// real native recursive function plus a standard-signature wrapper:
///
/// - `rec(args…, upvals…, depth, interrupt, ctx) -> f64` — the body, with
///   `SelfCall` a direct native call (depth-guarded and interrupt-polled at
///   the interpreter's exact points; a flagged abandon unwinds every frame
///   through `ctx.abandon`).
/// - `kernel(regs, interrupt, ctx) -> i64` — the exported entry: loads the
///   pre-guarded window-0 arguments/upvalue snapshots from the register
///   buffer, calls `rec` with `ctx.depth` (the remaining call-depth budget
///   the seam computed), and returns `0` with the raw result in
///   `ctx.scratch`, or [`REC_ABANDONED`] / [`INTERRUPTED`].
fn compile_rec(k: &Kernel) -> Option<NativeKernel> {
    if !rec_eligible(k) {
        return None;
    }
    cranelift_native::builder().ok()?;
    let mut builder =
        JITBuilder::with_flags(&[("opt_level", "speed")], default_libcall_names()).ok()?;
    for (name, addr, _, _) in helper_table() {
        builder.symbol(name, addr);
    }
    let mut module = JITModule::new(builder);
    let ptr_ty = module.target_config().pointer_type();
    // Window slots by role, in the fixed order the signature uses.
    let mut arg_slots: Vec<(usize, u16)> = Vec::new();
    let mut upval_slots: Vec<usize> = Vec::new();
    for (r, slot) in k.locals.iter().enumerate() {
        match slot {
            crate::bytecode::KSlot::Arg(a) => arg_slots.push((r, *a as u16)),
            crate::bytecode::KSlot::Upvalue(_) => upval_slots.push(r),
            crate::bytecode::KSlot::Local(_) => {}
        }
    }
    let mut rec_sig = module.make_signature();
    for _ in 0..arg_slots.len() + upval_slots.len() {
        rec_sig.params.push(AbiParam::new(types::F64));
    }
    rec_sig.params.push(AbiParam::new(types::I64));
    rec_sig.params.push(AbiParam::new(ptr_ty));
    rec_sig.params.push(AbiParam::new(ptr_ty));
    rec_sig.returns.push(AbiParam::new(types::F64));
    let rec_id = module
        .declare_function("rec", Linkage::Local, &rec_sig)
        .ok()?;
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    sig.params.push(AbiParam::new(ptr_ty));
    sig.params.push(AbiParam::new(ptr_ty));
    sig.returns.push(AbiParam::new(types::I64));
    let wrap_id = module
        .declare_function("kernel", Linkage::Export, &sig)
        .ok()?;
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut mctx = module.make_context();
    // The recursive body.
    mctx.func.signature = rec_sig;
    {
        let frontend_config = module.target_config();
        let b = FunctionBuilder::new(&mut mctx.func, &mut fb_ctx);
        Translator::new_rec(b, &mut module, k, rec_id, &arg_slots, &upval_slots)
            .translate(frontend_config)?;
    }
    module.define_function(rec_id, &mut mctx).ok()?;
    module.clear_context(&mut mctx);
    // The wrapper.
    mctx.func.signature = sig;
    {
        let frontend_config = module.target_config();
        let mut b = FunctionBuilder::new(&mut mctx.func, &mut fb_ctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let (regs_ptr, int_ptr, ctx_ptr) = {
            let p = b.block_params(entry);
            (p[0], p[1], p[2])
        };
        let rec_ref = module.declare_func_in_func(rec_id, b.func);
        let mut args = Vec::with_capacity(arg_slots.len() + upval_slots.len() + 3);
        for &(r, _) in &arg_slots {
            args.push(b.ins().load(
                types::F64,
                MemFlagsData::trusted(),
                regs_ptr,
                (8 * r) as i32,
            ));
        }
        for &r in &upval_slots {
            args.push(b.ins().load(
                types::F64,
                MemFlagsData::trusted(),
                regs_ptr,
                (8 * r) as i32,
            ));
        }
        let depth = b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            ctx_ptr,
            std::mem::offset_of!(JitCtx, depth) as i32,
        );
        args.push(depth);
        args.push(int_ptr);
        args.push(ctx_ptr);
        let call = b.ins().call(rec_ref, &args);
        let ret = b.inst_results(call)[0];
        let ab = b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            ctx_ptr,
            std::mem::offset_of!(JitCtx, abandon) as i32,
        );
        let ok_b = b.create_block();
        let fail_b = b.create_block();
        b.ins().brif(ab, fail_b, &[], ok_b, &[]);
        b.switch_to_block(ok_b);
        b.ins().store(
            MemFlagsData::trusted(),
            ret,
            ctx_ptr,
            std::mem::offset_of!(JitCtx, scratch) as i32,
        );
        let zero = b.ins().iconst(types::I64, 0);
        b.ins().return_(&[zero]);
        b.switch_to_block(fail_b);
        let is_intr = b.ins().icmp_imm_s(IntCC::Equal, ab, 2);
        let intr_code = b.ins().iconst(types::I64, INTERRUPTED);
        let aband_code = b.ins().iconst(types::I64, REC_ABANDONED);
        let code = b.ins().select(is_intr, intr_code, aband_code);
        b.ins().return_(&[code]);
        b.seal_all_blocks();
        b.finalize(frontend_config);
    }
    module.define_function(wrap_id, &mut mctx).ok()?;
    module.clear_context(&mut mctx);
    module.finalize_definitions().ok()?;
    let code = module.get_finalized_function(wrap_id);
    // SAFETY: as in `compile` — the wrapper was defined with exactly
    // `NativeFn`'s signature.
    #[expect(unsafe_code, reason = "type the finalized JIT entry point")]
    let entry: NativeFn = unsafe { std::mem::transmute(code) };
    Some(NativeKernel(Rc::new(Compiled {
        module: Some(module),
        entry,
        callees: Vec::new(),
        min_regs: KWIN,
    })))
}

/// One oslot's entry-hoisted direct-view state (see `Translator::ta_views`).
#[derive(Clone, Copy)]
struct OslotView {
    ptr: cranelift_codegen::ir::Value,
    /// The element count as an f64 (`.length` reads).
    len_f64: cranelift_codegen::ir::Value,
    /// `umin(len, 2^32 - 1)`: a single unsigned `index < bound` compare is
    /// then `dense_index`'s full range condition (negative indices wrap to
    /// huge unsigned values and fail it too).
    bound: cranelift_codegen::ir::Value,
    /// `kind == TA_F64` / `kind == DENSE` / `kind != NONE`, as i8 tests.
    is_ta: cranelift_codegen::ir::Value,
    is_dense: cranelift_codegen::ir::Value,
    is_direct: cranelift_codegen::ir::Value,
}

struct Translator<'a> {
    b: FunctionBuilder<'a>,
    module: &'a mut JITModule,
    k: &'a Kernel,
    /// One block per code index of the code currently being emitted (the
    /// op's single entry point): the top-level kernel's blocks in
    /// `translate`, swapped for a fresh per-site set while a pinned callee
    /// kernel is being inlined (`emit_call_kernel`).
    cur_blocks: Vec<Block>,
    /// Register-file base the u16 op indices resolve against: 0 for the
    /// top-level kernel, the callee's variable window during inlining.
    reg_base: usize,
    /// `Some(cont)` while inlining a callee: `Ret` jumps here with its value
    /// instead of exiting the function.
    inline_ret: Option<Block>,
    /// The pinned callees this kernel is being compiled against (resolved at
    /// the compiling activation; later activations identity-check them).
    callees: Vec<InlineCallee>,
    /// `Some` while building a recursive body (see [`RecMode`]).
    rec: Option<RecMode>,
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
    ctx_ptr: cranelift_codegen::ir::Value,
    ptr_ty: cranelift_codegen::ir::Type,
    /// Per-oslot direct element view, hoisted from the [`JitCtx`] table at
    /// entry — all activation constants, including the derived values every
    /// element access needs (the f64 length for `.length` reads, the
    /// index bound clamped to `dense_index`'s 2^32-1 ceiling, and the kind
    /// tests), so the per-access sequence pays none of it. Kind
    /// [`ElemView::NONE`] routes that oslot's element ops to the helper
    /// shims.
    ta_views: Vec<OslotView>,
    /// Per-sslot pinned-string view (byte ptr, length), hoisted at entry.
    sviews: Vec<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value)>,
    /// Lazily imported helper functions, by symbol name.
    helpers: HashMap<&'static str, FuncRef>,
    /// Back-edge trampolines created while emitting the current op, filled
    /// with the poll sequence after the op's terminator is placed.
    pending_backedges: Vec<(Block, usize)>,
}

/// One pinned callee prepared for inlining: the proto whose `fn_kernel` is
/// inlined at every `CallKernel` site targeting it, the base of its variable
/// window in `Translator::vars`, and the base of its f64 slot window in the
/// caller's register buffer (where the entry guard put its upvalue
/// snapshot).
#[derive(Clone)]
struct InlineCallee {
    proto: Rc<crate::bytecode::FuncProto>,
    var_base: usize,
}

/// The RECURSIVE-body emission mode (see [`compile_rec`]): the function
/// being built is the self-callable inner function, not the standard
/// register-buffer wrapper — `Ret` returns its value directly, and
/// `SelfCall` becomes a real native call through `self_ref`.
#[derive(Clone)]
struct RecMode {
    self_ref: FuncRef,
    /// This frame's remaining depth budget (an i64 parameter).
    depth: cranelift_codegen::ir::Value,
    /// The kernel-arg index of each argument parameter, in signature order —
    /// a self-call passes `window[base + index]` for each.
    arg_indices: Vec<u16>,
    /// The upvalue parameters, re-passed verbatim on every self-call (cells
    /// cannot change during an activation).
    upval_params: Vec<cranelift_codegen::ir::Value>,
    /// Sets `ctx.abandon = 1` (depth exhausted) and returns 0.0.
    abandon_depth: Block,
    /// Returns 0.0 with `ctx.abandon` already set (post-call unwind).
    ret_zero: Block,
}

impl<'a> Translator<'a> {
    fn new(
        mut b: FunctionBuilder<'a>,
        module: &'a mut JITModule,
        k: &'a Kernel,
        callee_specs: &[(Rc<crate::bytecode::FuncProto>, u32)],
    ) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let (regs_ptr, int_ptr, ctx_ptr) = {
            let params = b.block_params(entry);
            (params[0], params[1], params[2])
        };
        let ptr_ty = module.target_config().pointer_type();
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
        // Callee variable windows: one F64 variable per callee kernel
        // register. Upvalue slots load their once-per-activation snapshot
        // from the caller's register buffer (where the entry guard put it);
        // Arg slots are assigned at each call site; Local slots are pure
        // scratch (translation proved store-before-read), defined here only
        // so every use is dominated.
        let mut callees = Vec::with_capacity(callee_specs.len());
        for (proto, win_base) in callee_specs {
            let ck = proto.fn_kernel.as_ref().expect("checked by eligibility");
            let var_base = vars.len();
            for (r, slot) in ck.locals.iter().enumerate() {
                let var = b.declare_var(types::F64);
                let v = match slot {
                    crate::bytecode::KSlot::Upvalue(_) => b.ins().load(
                        types::F64,
                        MemFlagsData::trusted(),
                        regs_ptr,
                        (8 * (*win_base as usize + r)) as i32,
                    ),
                    _ => b.ins().f64const(0.0),
                };
                b.def_var(var, v);
                vars.push(var);
            }
            for _ in ck.locals.len()..ck.n_regs as usize {
                let var = b.declare_var(types::F64);
                let v = b.ins().f64const(0.0);
                b.def_var(var, v);
                vars.push(var);
            }
            callees.push(InlineCallee {
                proto: proto.clone(),
                var_base,
            });
        }
        let poll = b.declare_var(types::I32);
        let zero = b.ins().iconst(types::I32, 0);
        b.def_var(poll, zero);
        // Hoist the activation-constant tables: per-oslot direct-view
        // (ptr, len) pairs and per-sslot pinned-string (ptr, len) pairs.
        // Both are immutable for the whole activation (see [`TaView`] /
        // [`SStr`]), so entry loads suffice.
        let mut ta_views = Vec::with_capacity(k.oslots.len());
        if !k.oslots.is_empty() {
            let tab = b.ins().load(
                ptr_ty,
                MemFlagsData::trusted(),
                ctx_ptr,
                std::mem::offset_of!(JitCtx, ta) as i32,
            );
            for i in 0..k.oslots.len() {
                let base = (i * std::mem::size_of::<ElemView>()) as i32;
                let ptr = b.ins().load(ptr_ty, MemFlagsData::trusted(), tab, base);
                let len = b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), tab, base + 8);
                let kind = b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), tab, base + 16);
                let len_f64 = b.ins().fcvt_from_sint(types::F64, len);
                let ceiling = b.ins().iconst(types::I64, 4_294_967_295);
                let bound = b.ins().umin(len, ceiling);
                let is_ta = b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, kind, ElemView::TA_F64 as i64);
                let is_dense = b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, kind, ElemView::DENSE as i64);
                let is_direct = b.ins().icmp_imm_s(IntCC::NotEqual, kind, 0);
                ta_views.push(OslotView {
                    ptr,
                    len_f64,
                    bound,
                    is_ta,
                    is_dense,
                    is_direct,
                });
            }
        }
        let mut sviews = Vec::with_capacity(k.sslots.len());
        if !k.sslots.is_empty() {
            let tab = b.ins().load(
                ptr_ty,
                MemFlagsData::trusted(),
                ctx_ptr,
                std::mem::offset_of!(JitCtx, sstr) as i32,
            );
            for i in 0..k.sslots.len() {
                let base = (i * std::mem::size_of::<SStr>()) as i32;
                let ptr = b.ins().load(ptr_ty, MemFlagsData::trusted(), tab, base);
                let len = b
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), tab, base + 8);
                sviews.push((ptr, len));
            }
        }
        let blocks: Vec<Block> = (0..k.code.len()).map(|_| b.create_block()).collect();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I64);
        let intr_block = b.create_block();
        b.ins().jump(blocks[0], &[]);
        Translator {
            b,
            module,
            k,
            cur_blocks: blocks,
            reg_base: 0,
            inline_ret: None,
            callees,
            rec: None,
            exit_block,
            intr_block,
            vars,
            poll,
            regs_ptr,
            int_ptr,
            ctx_ptr,
            ptr_ty,
            ta_views,
            sviews,
            helpers: HashMap::new(),
            pending_backedges: Vec::new(),
        }
    }

    /// Constructor for a RECURSIVE body (see [`compile_rec`]): the function
    /// under construction is the self-callable inner function — window
    /// registers come from PARAMETERS (arguments, then upvalue snapshots;
    /// locals are scratch) rather than the register buffer, and the landing
    /// blocks (depth-abandon, interrupt-abandon, post-call unwind) are
    /// pre-filled here. Signature:
    /// `(args…, upvals…, depth: i64, interrupt: ptr, ctx: ptr) -> f64`.
    fn new_rec(
        mut b: FunctionBuilder<'a>,
        module: &'a mut JITModule,
        k: &'a Kernel,
        rec_id: cranelift_module::FuncId,
        arg_slots: &[(usize, u16)],
        upval_slots: &[usize],
    ) -> Self {
        let self_ref = module.declare_func_in_func(rec_id, b.func);
        let ptr_ty = module.target_config().pointer_type();
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params: Vec<cranelift_codegen::ir::Value> = b.block_params(entry).to_vec();
        let n_args = arg_slots.len();
        let n_upvals = upval_slots.len();
        let depth = params[n_args + n_upvals];
        let int_ptr = params[n_args + n_upvals + 1];
        let ctx_ptr = params[n_args + n_upvals + 2];
        let n_regs = k.n_regs as usize;
        let mut init: Vec<Option<cranelift_codegen::ir::Value>> = vec![None; n_regs];
        for (j, &(r, _)) in arg_slots.iter().enumerate() {
            init[r] = Some(params[j]);
        }
        for (j, &r) in upval_slots.iter().enumerate() {
            init[r] = Some(params[n_args + j]);
        }
        let zero = b.ins().f64const(0.0);
        let mut vars = Vec::with_capacity(n_regs);
        for slot_init in init {
            let var = b.declare_var(types::F64);
            b.def_var(var, slot_init.unwrap_or(zero));
            vars.push(var);
        }
        let poll = b.declare_var(types::I32);
        let z32 = b.ins().iconst(types::I32, 0);
        b.def_var(poll, z32);
        let blocks: Vec<Block> = (0..k.code.len()).map(|_| b.create_block()).collect();
        let abandon_depth = b.create_block();
        let abandon_intr = b.create_block();
        let ret_zero = b.create_block();
        b.ins().jump(blocks[0], &[]);
        let abandon_off = std::mem::offset_of!(JitCtx, abandon) as i32;
        b.switch_to_block(abandon_depth);
        let one = b.ins().iconst(types::I64, 1);
        b.ins()
            .store(MemFlagsData::trusted(), one, ctx_ptr, abandon_off);
        let z = b.ins().f64const(0.0);
        b.ins().return_(&[z]);
        b.switch_to_block(abandon_intr);
        let two = b.ins().iconst(types::I64, 2);
        b.ins()
            .store(MemFlagsData::trusted(), two, ctx_ptr, abandon_off);
        let z = b.ins().f64const(0.0);
        b.ins().return_(&[z]);
        b.switch_to_block(ret_zero);
        let z = b.ins().f64const(0.0);
        b.ins().return_(&[z]);
        let arg_indices = arg_slots.iter().map(|&(_, a)| a).collect();
        let upval_params = params[n_args..n_args + n_upvals].to_vec();
        Translator {
            b,
            module,
            k,
            cur_blocks: blocks,
            reg_base: 0,
            inline_ret: None,
            callees: Vec::new(),
            rec: Some(RecMode {
                self_ref,
                depth,
                arg_indices,
                upval_params,
                abandon_depth,
                ret_zero,
            }),
            // Unused in rec mode: `Exit` is rejected by eligibility, and no
            // register-buffer loads exist — but the fields must hold FILLED
            // blocks / live values.
            exit_block: ret_zero,
            intr_block: abandon_intr,
            vars,
            poll,
            regs_ptr: ctx_ptr,
            int_ptr,
            ctx_ptr,
            ptr_ty,
            ta_views: Vec::new(),
            sviews: Vec::new(),
            helpers: HashMap::new(),
            pending_backedges: Vec::new(),
        }
    }

    fn abi_ty(&self, a: Abi) -> cranelift_codegen::ir::Type {
        match a {
            Abi::F64 => types::F64,
            Abi::I64 => types::I64,
            Abi::I8 => types::I8,
            Abi::Ptr => self.ptr_ty,
        }
    }

    fn helper(&mut self, name: &'static str) -> Option<FuncRef> {
        if let Some(&f) = self.helpers.get(name) {
            return Some(f);
        }
        let (_, _, params, ret) = helper_table().into_iter().find(|(n, _, _, _)| *n == name)?;
        let mut sig = self.module.make_signature();
        for p in params {
            sig.params.push(AbiParam::new(self.abi_ty(*p)));
        }
        sig.returns.push(AbiParam::new(self.abi_ty(ret)));
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .ok()?;
        let f = self.module.declare_func_in_func(id, self.b.func);
        self.helpers.insert(name, f);
        Some(f)
    }

    /// Call a registered helper with explicit arguments (the element shims).
    fn callh(
        &mut self,
        name: &'static str,
        args: &[cranelift_codegen::ir::Value],
    ) -> Option<cranelift_codegen::ir::Value> {
        let f = self.helper(name)?;
        let call = self.b.ins().call(f, args);
        Some(self.b.inst_results(call)[0])
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
        self.b.use_var(self.vars[self.reg_base + r as usize])
    }

    fn set(&mut self, r: u16, v: cranelift_codegen::ir::Value) {
        self.b.def_var(self.vars[self.reg_base + r as usize], v);
    }

    /// ToInt32 as native code. The `|x| < 2^63` fast path is one saturating
    /// i64 conversion + truncation: within that range the conversion is the
    /// exact `trunc(x)`, whose low 32 bits ARE ToInt32's mod-2^32 result.
    /// Outside it — NaN, ±Inf, |x| ≥ 2^63, where saturation's low bits
    /// diverge — the cold path calls the shared `to_int32` core (returning
    /// the exact int32 as an f64 in [-2^31, 2^31), so converting back is
    /// exact). `fabs(NaN) < bound` is false, routing NaN to the cold path.
    fn toint32(&mut self, x: cranelift_codegen::ir::Value) -> Option<cranelift_codegen::ir::Value> {
        let bound = self.b.ins().f64const(TOINT32_FAST_BOUND);
        let ax = self.b.ins().fabs(x);
        let in_range = self.b.ins().fcmp(FloatCC::LessThan, ax, bound);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, types::I32);
        self.b.ins().brif(in_range, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let wide = self.b.ins().fcvt_to_sint_sat(types::I64, x);
        let narrow = self.b.ins().ireduce(types::I32, wide);
        self.b.ins().jump(join, &[narrow.into()]);
        self.b.switch_to_block(slow);
        let f = self.call1("cjit_toint32", x)?;
        let wide2 = self.b.ins().fcvt_to_sint_sat(types::I64, f);
        let narrow2 = self.b.ins().ireduce(types::I32, wide2);
        self.b.ins().jump(join, &[narrow2.into()]);
        self.b.switch_to_block(join);
        Some(res)
    }

    /// An i32 back to the f64 register world.
    fn i32_to_f64(&mut self, v: cranelift_codegen::ir::Value) -> cranelift_codegen::ir::Value {
        self.b.ins().fcvt_from_sint(types::F64, v)
    }

    /// JS `%` with `js_mod`'s exact fast/slow split. The inlined path fires
    /// under `js_mod`'s own integer guard — both operands round-trip through
    /// i64, both within ±2^53, divisor nonzero (checked at translation when
    /// the divisor is a constant) — and computes `srem` with the identical
    /// `-0`-carries-the-dividend's-sign fix. Everything else calls the shared
    /// `js_mod` core, so NaN/Inf/zero/fractional cases are the interpreter's
    /// own code.
    fn emit_mod(
        &mut self,
        a: cranelift_codegen::ir::Value,
        b: cranelift_codegen::ir::Value,
        bk: Option<f64>,
    ) -> Option<cranelift_codegen::ir::Value> {
        // A constant divisor outside the integer fast path (fractional, 0,
        // huge) can never take it: skip the guard entirely.
        if let Some(k) = bk {
            if mod_const_rhs(k).is_none() {
                return self.call2("cjit_mod", a, b);
            }
        }
        let bound = self.b.ins().f64const(MOD_FAST_BOUND);
        let ai = self.b.ins().fcvt_to_sint_sat(types::I64, a);
        let fa = self.b.ins().fcvt_from_sint(types::F64, ai);
        let a_int = self.b.ins().fcmp(FloatCC::Equal, fa, a);
        let aa = self.b.ins().fabs(a);
        let a_small = self.b.ins().fcmp(FloatCC::LessThanOrEqual, aa, bound);
        let mut ok = self.b.ins().band(a_int, a_small);
        let bi = match bk.and_then(mod_const_rhs) {
            Some(ki) => self.b.ins().iconst(types::I64, ki),
            None => {
                let bi = self.b.ins().fcvt_to_sint_sat(types::I64, b);
                let fb = self.b.ins().fcvt_from_sint(types::F64, bi);
                let b_int = self.b.ins().fcmp(FloatCC::Equal, fb, b);
                let ab = self.b.ins().fabs(b);
                let b_small = self.b.ins().fcmp(FloatCC::LessThanOrEqual, ab, bound);
                let b_nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, bi, 0);
                ok = self.b.ins().band(ok, b_int);
                ok = self.b.ins().band(ok, b_small);
                ok = self.b.ins().band(ok, b_nonzero);
                bi
            }
        };
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, types::F64);
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        // No trap: |operands| ≤ 2^53 and divisor ≠ 0 by the guard.
        let r = self.b.ins().srem(ai, bi);
        let rz = self.b.ins().icmp_imm_s(IntCC::Equal, r, 0);
        let fr = self.b.ins().fcvt_from_sint(types::F64, r);
        let zero = self.b.ins().f64const(0.0);
        let signed_zero = self.b.ins().fcopysign(zero, a);
        let picked = self.b.ins().select(rz, signed_zero, fr);
        self.b.ins().jump(join, &[picked.into()]);
        self.b.switch_to_block(slow);
        let h = self.call2("cjit_mod", a, b)?;
        self.b.ins().jump(join, &[h.into()]);
        self.b.switch_to_block(join);
        Some(res)
    }

    /// `regs[a] <kind> b` with the interpreter's exact semantics. `bk` is
    /// `b`'s compile-time constant when the op is a K-variant, letting the
    /// ToInt32/guard work fold at translation. IEEE kinds and the whole
    /// ToInt32 bitwise family are native instructions; `%` inlines its
    /// integer fast path; `**` keeps the shared `math_pow` core (its spec
    /// special cases are not IEEE).
    fn arith(
        &mut self,
        kind: ArithKind,
        a: cranelift_codegen::ir::Value,
        b: cranelift_codegen::ir::Value,
        bk: Option<f64>,
    ) -> Option<cranelift_codegen::ir::Value> {
        // The right operand as int32: folded when constant.
        macro_rules! b32 {
            () => {
                match bk {
                    Some(k) => {
                        let ki = crate::vm::to_int32(k);
                        self.b.ins().iconst(types::I32, i64::from(ki))
                    }
                    None => self.toint32(b)?,
                }
            };
        }
        Some(match kind {
            ArithKind::Sub => self.b.ins().fsub(a, b),
            ArithKind::Mul => self.b.ins().fmul(a, b),
            ArithKind::Div => self.b.ins().fdiv(a, b),
            ArithKind::Mod => self.emit_mod(a, b, bk)?,
            ArithKind::Pow => self.call2("cjit_pow", a, b)?,
            ArithKind::BitAnd => {
                let (ia, ib) = (self.toint32(a)?, b32!());
                let r = self.b.ins().band(ia, ib);
                self.i32_to_f64(r)
            }
            ArithKind::BitOr => {
                let (ia, ib) = (self.toint32(a)?, b32!());
                let r = self.b.ins().bor(ia, ib);
                self.i32_to_f64(r)
            }
            ArithKind::BitXor => {
                let (ia, ib) = (self.toint32(a)?, b32!());
                let r = self.b.ins().bxor(ia, ib);
                self.i32_to_f64(r)
            }
            // Shift counts: `ToUint32(rhs) & 31` — the same low 5 bits as
            // `ToInt32(rhs) & 31`, and Cranelift additionally masks the
            // count to the type width, so the band is belt-and-braces.
            ArithKind::Shl => {
                let (ia, ib) = (self.toint32(a)?, b32!());
                let cnt = self.b.ins().band_imm_u(ib, 31i64);
                let r = self.b.ins().ishl(ia, cnt);
                self.i32_to_f64(r)
            }
            ArithKind::Shr => {
                let (ia, ib) = (self.toint32(a)?, b32!());
                let cnt = self.b.ins().band_imm_u(ib, 31i64);
                let r = self.b.ins().sshr(ia, cnt);
                self.i32_to_f64(r)
            }
            // `>>>` interprets the shifted lhs as unsigned: ToUint32(lhs)
            // has the same 32 bits as ToInt32(lhs), so shift logically and
            // zero-extend before the float conversion.
            ArithKind::UShr => {
                let (ia, ib) = (self.toint32(a)?, b32!());
                let cnt = self.b.ins().band_imm_u(ib, 31i64);
                let r = self.b.ins().ushr(ia, cnt);
                let wide = self.b.ins().uextend(types::I64, r);
                self.b.ins().fcvt_from_sint(types::F64, wide)
            }
        })
    }

    /// ToBoolean on a scalar register: true unless `0`, `-0`, or NaN —
    /// exactly `fcmp ord_ne x, 0.0`.
    fn truthy(&mut self, x: cranelift_codegen::ir::Value) -> cranelift_codegen::ir::Value {
        let zero = self.b.ins().f64const(0.0);
        self.b.ins().fcmp(FloatCC::OrderedNotEqual, x, zero)
    }

    /// Element READ (`KOp::LoadElem` semantics): direct 8-byte load for an
    /// f64-typed-array view, the interpreter's shared fast-path core through
    /// the helper shim for everything else; any fast-path failure jumps to
    /// the op's bail edge (an Exit stub), exactly like the interpreter arm.
    fn emit_elem_load(
        &mut self,
        pc: usize,
        obj: u16,
        idx: cranelift_codegen::ir::Value,
        bail: u16,
    ) -> Option<cranelift_codegen::ir::Value> {
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let ta_b = self.b.create_block();
        let chk_dense = self.b.create_block();
        let dense_b = self.b.create_block();
        let helper = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, types::F64);
        self.b.ins().brif(view.is_ta, ta_b, &[], chk_dense, &[]);
        self.b.switch_to_block(chk_dense);
        self.b.ins().brif(view.is_dense, dense_b, &[], helper, &[]);
        // f64 typed array: bounds-checked 8-byte load.
        self.b.switch_to_block(ta_b);
        let (ok, ii) = self.elem_index_ok(idx, view.bound);
        let load_b = self.b.create_block();
        self.b.ins().brif(ok, load_b, &[], bail_b, &[]);
        self.b.switch_to_block(load_b);
        let eight = self.b.ins().iconst(types::I64, 8);
        let off = self.b.ins().imul(ii, eight);
        let addr = self.b.ins().iadd(view.ptr, off);
        let v = self.b.ins().load(types::F64, MemFlagsData::new(), addr, 0);
        self.b.ins().jump(join, &[v.into()]);
        // Dense array (read-only kernels): bounds check, then the slot's
        // repr(u8) tag must be `Number` (a hole or any other variant bails,
        // exactly like the interpreter's fast-path miss), then the payload.
        self.b.switch_to_block(dense_b);
        let (ok, ii) = self.elem_index_ok(idx, view.bound);
        let tag_b = self.b.create_block();
        self.b.ins().brif(ok, tag_b, &[], bail_b, &[]);
        self.b.switch_to_block(tag_b);
        let stride = self.b.ins().iconst(
            types::I64,
            std::mem::size_of::<crate::value::Value>() as i64,
        );
        let off = self.b.ins().imul(ii, stride);
        let slot = self.b.ins().iadd(view.ptr, off);
        let tag = self
            .b
            .ins()
            .load(types::I8, MemFlagsData::trusted(), slot, 0);
        let is_num = self.b.ins().icmp_imm_s(
            IntCC::Equal,
            tag,
            i64::from(crate::value::Value::JIT_NUMBER_TAG),
        );
        let payload_b = self.b.create_block();
        self.b.ins().brif(is_num, payload_b, &[], bail_b, &[]);
        self.b.switch_to_block(payload_b);
        let v = self.b.ins().load(
            types::F64,
            MemFlagsData::trusted(),
            slot,
            crate::value::Value::JIT_NUMBER_PAYLOAD_OFFSET as i32,
        );
        self.b.ins().jump(join, &[v.into()]);
        // Everything else: the interpreter's shared core through the shim.
        self.b.switch_to_block(helper);
        let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
        let st = self.callh("cjit_elem_load", &[self.ctx_ptr, oslot, idx])?;
        let ok_b = self.b.create_block();
        self.b.ins().brif(st, ok_b, &[], bail_b, &[]);
        self.b.switch_to_block(ok_b);
        let v = self.scratch();
        self.b.ins().jump(join, &[v.into()]);
        self.b.switch_to_block(join);
        Some(res)
    }

    /// Element WRITE (`KOp::StoreElem` semantics): direct store on an f64
    /// view (in-place only — the view's length is fixed, so an append is out
    /// of bounds and bails, exactly as a typed array's OOB store must), the
    /// shared core via the shim otherwise.
    fn emit_elem_store(
        &mut self,
        pc: usize,
        obj: u16,
        idx: cranelift_codegen::ir::Value,
        val: cranelift_codegen::ir::Value,
        bail: u16,
    ) -> Option<()> {
        // A dense view is never granted to a kernel containing stores, so
        // the direct arm here is the f64 typed array only.
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let direct = self.b.create_block();
        let helper = self.b.create_block();
        let join = self.b.create_block();
        self.b.ins().brif(view.is_ta, direct, &[], helper, &[]);
        self.b.switch_to_block(direct);
        let (ok, ii) = self.elem_index_ok(idx, view.bound);
        let store_b = self.b.create_block();
        self.b.ins().brif(ok, store_b, &[], bail_b, &[]);
        self.b.switch_to_block(store_b);
        let eight = self.b.ins().iconst(types::I64, 8);
        let off = self.b.ins().imul(ii, eight);
        let addr = self.b.ins().iadd(view.ptr, off);
        self.b.ins().store(MemFlagsData::new(), val, addr, 0);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(helper);
        let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
        let st = self.callh("cjit_elem_store", &[self.ctx_ptr, oslot, idx, val])?;
        self.b.ins().brif(st, join, &[], bail_b, &[]);
        self.b.switch_to_block(join);
        Some(())
    }

    /// The shared index test of the direct element paths — `dense_index`'s
    /// exact conditions (integral, `0 ≤ i < 2^32-1`) plus the view bound —
    /// returning the test and the index as an i64. Two conditions total:
    /// the f64 round-trip proves integrality (NaN fails it — saturation
    /// gives 0, and `0.0 == NaN` is false), and one UNSIGNED compare
    /// against the entry-clamped bound covers non-negativity, the length,
    /// and the 2^32-1 ceiling at once (a negative index wraps to a huge
    /// unsigned value).
    fn elem_index_ok(
        &mut self,
        idx: cranelift_codegen::ir::Value,
        bound: cranelift_codegen::ir::Value,
    ) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
        let ii = self.b.ins().fcvt_to_sint_sat(types::I64, idx);
        let fi = self.b.ins().fcvt_from_sint(types::F64, ii);
        let integral = self.b.ins().fcmp(FloatCC::Equal, fi, idx);
        let in_bounds = self.b.ins().icmp(IntCC::UnsignedLessThan, ii, bound);
        let ok = self.b.ins().band(integral, in_bounds);
        (ok, ii)
    }

    /// `.length` (`KOp::LoadLen` semantics): the view length directly, the
    /// shared core via the shim otherwise; failure bails.
    fn emit_len(&mut self, pc: usize, obj: u16, bail: u16) -> Option<cranelift_codegen::ir::Value> {
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let direct = self.b.create_block();
        let helper = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, types::F64);
        self.b.ins().brif(view.is_direct, direct, &[], helper, &[]);
        self.b.switch_to_block(direct);
        let v = view.len_f64;
        self.b.ins().jump(join, &[v.into()]);
        self.b.switch_to_block(helper);
        let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
        let st = self.callh("cjit_elem_len", &[self.ctx_ptr, oslot])?;
        let ok_b = self.b.create_block();
        self.b.ins().brif(st, ok_b, &[], bail_b, &[]);
        self.b.switch_to_block(ok_b);
        let v = self.scratch();
        self.b.ins().jump(join, &[v.into()]);
        self.b.switch_to_block(join);
        Some(res)
    }

    /// Read the helper out-param.
    fn scratch(&mut self) -> cranelift_codegen::ir::Value {
        self.b.ins().load(
            types::F64,
            MemFlagsData::trusted(),
            self.ctx_ptr,
            std::mem::offset_of!(JitCtx, scratch) as i32,
        )
    }

    /// A status-returning shim whose success value lands in `dst` via
    /// `ctx.scratch` and whose failure takes the bail edge (`ArrayPush`/
    /// `ArrayPop`).
    fn emit_shim_to_dst(
        &mut self,
        pc: usize,
        name: &'static str,
        args: &[cranelift_codegen::ir::Value],
        dst: u16,
        bail: u16,
    ) -> Option<()> {
        let bail_b = self.dest(pc, bail as usize);
        let st = self.callh(name, args)?;
        let ok_b = self.b.create_block();
        self.b.ins().brif(st, ok_b, &[], bail_b, &[]);
        self.b.switch_to_block(ok_b);
        let v = self.scratch();
        self.set(dst, v);
        Some(())
    }

    /// The block a branch from `pc` to `target` should jump to: the target's
    /// block directly for a forward branch; for a back-edge, a trampoline
    /// that runs the interpreter's poll sequence (count taken back-edges,
    /// every 256th check the interrupt byte) before reaching the target.
    fn dest(&mut self, pc: usize, target: usize) -> Block {
        if target > pc {
            return self.cur_blocks[target];
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
                .brif(masked, self.cur_blocks[target], &[], check, &[]);
            self.b.switch_to_block(check);
            let flag = self
                .b
                .ins()
                .load(types::I8, MemFlagsData::trusted(), self.int_ptr, 0);
            self.b
                .ins()
                .brif(flag, self.intr_block, &[], self.cur_blocks[target], &[]);
        }
    }

    fn translate(
        mut self,
        frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Option<()> {
        let k = self.k;
        let code: &[KOp] = &k.code;
        for (pc, &op) in code.iter().enumerate() {
            let block = self.cur_blocks[pc];
            self.b.switch_to_block(block);
            if let Some(skip) = self.emit_op(op, pc)? {
                let next = self.cur_blocks[pc + skip];
                self.b.ins().jump(next, &[]);
            }
            self.flush_backedges();
        }
        self.finish(frontend_config)
    }

    /// Emit one op into the current block. Returns the fall-through skip
    /// (1, or 2 past a fused op's landing pad), `Some(None)` when the op
    /// placed its own terminator, or `None` to decline the whole
    /// translation. Shared between the top-level kernel and inlined callee
    /// kernels (`emit_call_kernel` swaps `cur_blocks`/`reg_base`/
    /// `inline_ret` around it).
    fn emit_op(&mut self, op: KOp, pc: usize) -> Option<Option<usize>> {
        let mut fallthrough = Some(1usize);
        match op {
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
                let v = self.arith(kind, x, y, None)?;
                self.set(dst, v);
            }
            KOp::ArithK { kind, dst, a, k } => {
                let x = self.get(a);
                let kk = self.b.ins().f64const(k);
                let v = self.arith(kind, x, kk, Some(k))?;
                self.set(dst, v);
            }
            KOp::Neg { dst, src } => {
                let x = self.get(src);
                let v = self.b.ins().fneg(x);
                self.set(dst, v);
            }
            KOp::BitNot { dst, src } => {
                let x = self.get(src);
                let ix = self.toint32(x)?;
                let nx = self.b.ins().bnot(ix);
                let v = self.i32_to_f64(nx);
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
                let v = self.arith(kind, x, y, None)?;
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
                let v = self.arith(kind, x, kk, Some(k))?;
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
                let v = match kind {
                    // Cranelift fmin/fmax carry wasm's semantics — NaN
                    // poisons, -0 < +0 — which are exactly the
                    // `math_min2`/`math_max2` cores' rules.
                    KMath::Min2 => self.b.ins().fmin(x, y),
                    KMath::Max2 => self.b.ins().fmax(x, y),
                    // `**`'s spec special cases live in `math_pow`.
                    KMath::Pow2 => self.call2("cjit_pow2", x, y)?,
                    KMath::Imul2 => {
                        let (ix, iy) = (self.toint32(x)?, self.toint32(y)?);
                        let r = self.b.ins().imul(ix, iy);
                        self.i32_to_f64(r)
                    }
                    // Unary kinds are excluded by `eligible` (arity 2).
                    _ => return None,
                };
                self.set(dst, v);
            }
            KOp::Exit { .. } => {
                // Top-level only (callee eligibility rejects `Exit`).
                if self.inline_ret.is_some() {
                    return None;
                }
                let pcv = self.b.ins().iconst(types::I64, pc as i64);
                self.b.ins().jump(self.exit_block, &[pcv.into()]);
                fallthrough = None;
            }
            KOp::Ret { src, .. } => {
                fallthrough = None;
                if let Some(cont) = self.inline_ret {
                    // Inlined callee: the return value flows to the call
                    // site's continuation (Number-only — boolean-
                    // returning callees are rejected by the activation
                    // guard the compiled code runs under).
                    let v = self.get(src);
                    self.b.ins().jump(cont, &[v.into()]);
                } else if self.rec.is_some() {
                    // Recursive body: a real native return (raw f64 —
                    // booleans travel as 0.0/1.0; the seam constructs
                    // the typed result from the family's `ret_bool`).
                    let v = self.get(src);
                    self.b.ins().return_(&[v]);
                } else {
                    // Top-level (function kernels): exit at this op's
                    // index; the caller constructs the typed result.
                    let pcv = self.b.ins().iconst(types::I64, pc as i64);
                    self.b.ins().jump(self.exit_block, &[pcv.into()]);
                }
            }
            KOp::LoadElem {
                dst,
                obj,
                idx,
                bail,
            } => {
                let i = self.get(idx);
                let v = self.emit_elem_load(pc, obj, i, bail)?;
                self.set(dst, v);
            }
            KOp::StoreElem {
                obj,
                idx,
                val,
                bail,
            } => {
                let (i, v) = (self.get(idx), self.get(val));
                self.emit_elem_store(pc, obj, i, v, bail)?;
            }
            // Fused `s += a[i]` / `a[i] <op> …`: the element load's exact
            // semantics (and bail edge), then the arithmetic tail, then
            // the 2-slot skip past the landing pad.
            KOp::LoadElemAdd {
                dst,
                obj,
                idx,
                bail,
                d2,
                a2,
                b2,
            } => {
                let i = self.get(idx);
                let v = self.emit_elem_load(pc, obj, i, bail)?;
                self.set(dst, v);
                let (x2, y2) = (self.get(a2), self.get(b2));
                let v2 = self.b.ins().fadd(x2, y2);
                self.set(d2, v2);
                fallthrough = Some(2);
            }
            KOp::LoadElemArith {
                dst,
                obj,
                idx,
                bail,
                kind,
                d2,
                a2,
                b2,
            } => {
                let i = self.get(idx);
                let v = self.emit_elem_load(pc, obj, i, bail)?;
                self.set(dst, v);
                let (x2, y2) = (self.get(a2), self.get(b2));
                let v2 = self.arith(kind, x2, y2, None)?;
                self.set(d2, v2);
                fallthrough = Some(2);
            }
            KOp::LoadLen { dst, obj, bail } => {
                let v = self.emit_len(pc, obj, bail)?;
                self.set(dst, v);
            }
            // Fused `i < a.length` header: LoadLen's semantics (and bail
            // edge), then BrCmp's compare-and-branch, then the 2-slot
            // fall-through past the landing pad.
            KOp::LenBrCmp {
                dst,
                obj,
                bail,
                cmp,
                a,
                b,
                if_true,
                target,
            } => {
                let v = self.emit_len(pc, obj, bail)?;
                self.set(dst, v);
                let (x, y) = (self.get(a), self.get(b));
                let c = self.b.ins().fcmp(cmp_cc(cmp), x, y);
                let taken = self.dest(pc, target as usize);
                let fall = self.cur_blocks[pc + 2];
                if if_true {
                    self.b.ins().brif(c, taken, &[], fall, &[]);
                } else {
                    self.b.ins().brif(c, fall, &[], taken, &[]);
                }
                fallthrough = None;
            }
            KOp::ArrayPush {
                obj,
                val,
                dst,
                bail,
            } => {
                let v = self.get(val);
                let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
                let args = [self.ctx_ptr, oslot, v];
                self.emit_shim_to_dst(pc, "cjit_array_push", &args, dst, bail)?;
            }
            KOp::ArrayPop { obj, dst, bail } => {
                let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
                let args = [self.ctx_ptr, oslot];
                self.emit_shim_to_dst(pc, "cjit_array_pop", &args, dst, bail)?;
            }
            // Pinned-string reads: TOTAL over the entry-hoisted view
            // (flat ASCII, immutable) — no bail exists, mirroring the
            // interpreter arms exactly (saturating index conversion:
            // NaN→0, truncate; out-of-range yields NaN).
            KOp::StrLen { dst, str } => {
                let (_, slen) = self.sviews[str as usize];
                let v = self.b.ins().fcvt_from_sint(types::F64, slen);
                self.set(dst, v);
            }
            KOp::CharCodeAt { dst, str, idx } => {
                let (sptr, slen) = self.sviews[str as usize];
                let i = self.get(idx);
                let p = self.b.ins().fcvt_to_sint_sat(types::I64, i);
                let nonneg = self
                    .b
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, p, 0);
                let in_bounds = self.b.ins().icmp(IntCC::SignedLessThan, p, slen);
                let ok = self.b.ins().band(nonneg, in_bounds);
                let load_b = self.b.create_block();
                let oob_b = self.b.create_block();
                let join = self.b.create_block();
                let res = self.b.append_block_param(join, types::F64);
                self.b.ins().brif(ok, load_b, &[], oob_b, &[]);
                self.b.switch_to_block(load_b);
                let addr = self.b.ins().iadd(sptr, p);
                let byte = self.b.ins().load(types::I8, MemFlagsData::new(), addr, 0);
                let wide = self.b.ins().uextend(types::I32, byte);
                let v = self.b.ins().fcvt_from_sint(types::F64, wide);
                self.b.ins().jump(join, &[v.into()]);
                self.b.switch_to_block(oob_b);
                let nan = self.b.ins().f64const(f64::NAN);
                self.b.ins().jump(join, &[nan.into()]);
                self.b.switch_to_block(join);
                self.set(dst, res);
            }
            // A pinned-callee call: copy the arguments into the
            // callee's variable window and run its kernel INLINE — the
            // callee identity is an activation constant the compiled
            // code's users re-verify, so the inlined body is exactly the
            // kernel the interpreter's `run_callee_window` would run.
            KOp::CallKernel {
                dst,
                fslot,
                base,
                argc: _,
            } => {
                self.emit_call_kernel(dst, fslot, base)?;
            }
            // A direct self-recursive call (recursive bodies only; see
            // `compile_rec`): the interpreter's per-call depth guard and
            // shared poll, then a real native call to this very
            // function, then the abandon-unwind check.
            KOp::SelfCall {
                dst,
                base,
                argc: _,
                callee,
            } => {
                if callee != 0 {
                    return None;
                }
                let rec = self.rec.clone()?;
                self.emit_self_call(&rec, dst, base)?;
            }
            // Excluded by `eligible`.
            KOp::LoadProp { .. } | KOp::StoreProp { .. } => return None,
        }
        Some(fallthrough)
    }

    /// One self-recursive call site (recursive bodies; see the `SelfCall`
    /// arm): depth guard → shared poll → native self-call → abandon check.
    fn emit_self_call(&mut self, rec: &RecMode, dst: u16, base: u16) -> Option<()> {
        // Depth: the interpreter abandons when the NEXT frame would exceed
        // the budget — i.e. when this frame's remaining budget is < 1.
        let depth_ok = self.b.create_block();
        let out_of_depth = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, rec.depth, 1);
        self.b
            .ins()
            .brif(out_of_depth, rec.abandon_depth, &[], depth_ok, &[]);
        self.b.switch_to_block(depth_ok);
        // Shared poll counter (ctx-resident so the cadence spans frames,
        // like the interpreter's activation-wide counter).
        let poll_off = std::mem::offset_of!(JitCtx, poll) as i32;
        let p = self
            .b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), self.ctx_ptr, poll_off);
        let p2 = self.b.ins().iadd_imm_u(p, 1i64);
        self.b
            .ins()
            .store(MemFlagsData::trusted(), p2, self.ctx_ptr, poll_off);
        let masked = self.b.ins().band_imm_u(p2, 0xFFi64);
        let check = self.b.create_block();
        let call_b = self.b.create_block();
        self.b.ins().brif(masked, call_b, &[], check, &[]);
        self.b.switch_to_block(check);
        let flag = self
            .b
            .ins()
            .load(types::I8, MemFlagsData::trusted(), self.int_ptr, 0);
        // intr_block in rec mode sets `abandon = 2` and returns.
        self.b.ins().brif(flag, self.intr_block, &[], call_b, &[]);
        self.b.switch_to_block(call_b);
        // Arguments from the call site's contiguous registers, upvalues
        // re-passed, one less depth.
        let mut call_args = Vec::with_capacity(rec.arg_indices.len() + rec.upval_params.len() + 3);
        for &a in &rec.arg_indices {
            call_args.push(self.get(base + a));
        }
        call_args.extend_from_slice(&rec.upval_params);
        let next_depth = self.b.ins().iadd_imm_s(rec.depth, -1i64);
        call_args.push(next_depth);
        call_args.push(self.int_ptr);
        call_args.push(self.ctx_ptr);
        let call = self.b.ins().call(rec.self_ref, &call_args);
        let ret = self.b.inst_results(call)[0];
        // A flagged abandon/interrupt anywhere below unwinds every frame.
        let ab = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.ctx_ptr,
            std::mem::offset_of!(JitCtx, abandon) as i32,
        );
        let cont = self.b.create_block();
        self.b.ins().brif(ab, rec.ret_zero, &[], cont, &[]);
        self.b.switch_to_block(cont);
        self.set(dst, ret);
        Some(())
    }

    /// Inline one pinned-callee call site (see the `CallKernel` arm).
    fn emit_call_kernel(&mut self, dst: u16, fslot: u16, base: u16) -> Option<()> {
        let callee = self.callees.get(fslot as usize)?.clone();
        let ck = callee.proto.fn_kernel.as_ref()?;
        // Per-call argument copy — exactly the interpreter's window setup.
        for (r, slot) in ck.locals.iter().enumerate() {
            if let crate::bytecode::KSlot::Arg(a) = slot {
                let v = self.get(base + *a as u16);
                let var = self.vars[callee.var_base + r];
                self.b.def_var(var, v);
            }
        }
        // Swap in the callee's emission context and lay its body down at
        // this site; every `Ret` jumps to `cont` with the return value.
        let cont = self.b.create_block();
        let ret = self.b.append_block_param(cont, types::F64);
        let saved_blocks = std::mem::replace(
            &mut self.cur_blocks,
            (0..ck.code.len()).map(|_| self.b.create_block()).collect(),
        );
        let saved_base = std::mem::replace(&mut self.reg_base, callee.var_base);
        let saved_ret = self.inline_ret.replace(cont);
        let first = self.cur_blocks[0];
        self.b.ins().jump(first, &[]);
        for (pc2, &op) in ck.code.iter().enumerate() {
            let block = self.cur_blocks[pc2];
            self.b.switch_to_block(block);
            if let Some(skip) = self.emit_op(op, pc2)? {
                let next = self.cur_blocks[pc2 + skip];
                self.b.ins().jump(next, &[]);
            }
            self.flush_backedges();
        }
        self.cur_blocks = saved_blocks;
        self.reg_base = saved_base;
        self.inline_ret = saved_ret;
        self.b.switch_to_block(cont);
        self.set(dst, ret);
        Some(())
    }

    fn finish(
        mut self,
        frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Option<()> {
        // Recursive bodies pre-filled their landing blocks in `new_rec`.
        if self.rec.is_some() {
            self.b.seal_all_blocks();
            self.b.finalize(frontend_config);
            return Some(());
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
        let fall = self.cur_blocks[pc + 1];
        if if_true {
            self.b.ins().brif(cond, taken, &[], fall, &[]);
        } else {
            self.b.ins().brif(cond, fall, &[], taken, &[]);
        }
    }
}
