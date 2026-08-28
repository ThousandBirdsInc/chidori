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
/// - [`ElemView::TA_F64`] / the other `TA_*` kinds: a numeric typed array;
///   `ptr` is raw element storage (`buffer bytes + byte_offset`), `len` the
///   effective element count. An element access is a little-endian
///   load/store at the kind's width plus the kind's conversion — exactly
///   `decode`/`encode` for that `TAKind`. Compiled code carries the direct
///   sequence for ONE kind per oslot (the compiling activation's, baked at
///   translation); an activation pinning a different kind fails the baked
///   kind-equality test and takes the helper shims. `TA_U8C`
///   (Uint8ClampedArray) reads directly (identical to `TA_U8`) but always
///   stores through the shim (the clamp is not wraparound).
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
    pub(crate) const TA_I8: u64 = 3;
    pub(crate) const TA_U8: u64 = 4;
    pub(crate) const TA_U8C: u64 = 5;
    pub(crate) const TA_I16: u64 = 6;
    pub(crate) const TA_U16: u64 = 7;
    pub(crate) const TA_I32: u64 = 8;
    pub(crate) const TA_U32: u64 = 9;
    pub(crate) const TA_F32: u64 = 10;
    /// A dense view whose grant SCANNED the storage and found every slot a
    /// `Number` (no holes): reads skip the per-access tag check entirely —
    /// one bounds compare, one payload load. Granted under exactly the
    /// [`ElemView::DENSE`] conditions (read-only kernels, so the scan's
    /// finding holds for the whole activation); a dirty scan falls back to
    /// the tag-checked `DENSE` view. The scan is O(len) once per
    /// activation, repaid by the first pass over the array.
    pub(crate) const DENSE_NUM: u64 = 12;
    /// WRITABLE dense-array view — granted ONLY by the batch HOF driver
    /// (`Vm::hof_batch`) over the freshly-created, hole-filled `map` output
    /// array, which nothing else can reach mid-batch. A store writes the
    /// slot's `#[repr(u8)]` tag ([`Value::JIT_NUMBER_TAG`]) and f64 payload
    /// directly — overwriting a `Hole` (no drop needed, no heap payload)
    /// with a `Number`, exactly what the interpreter-core write would
    /// produce. Never granted by [`elem_view`]; reads and every other view
    /// kind treat it as "no direct arm" and take the shims.
    pub(crate) const DENSE_W: u64 = 11;

    /// The view-kind code for a typed array's element kind — `None` for the
    /// BigInt kinds, which never get a direct view (their elements are not
    /// f64-representable).
    pub(crate) fn ta_code(kind: crate::value::TAKind) -> Option<u64> {
        use crate::value::TAKind;
        Some(match kind {
            TAKind::I8 => ElemView::TA_I8,
            TAKind::U8 => ElemView::TA_U8,
            TAKind::U8Clamped => ElemView::TA_U8C,
            TAKind::I16 => ElemView::TA_I16,
            TAKind::U16 => ElemView::TA_U16,
            TAKind::I32 => ElemView::TA_I32,
            TAKind::U32 => ElemView::TA_U32,
            TAKind::F32 => ElemView::TA_F32,
            TAKind::F64 => ElemView::TA_F64,
            TAKind::I64 | TAKind::U64 => return None,
        })
    }

    pub(crate) fn none() -> ElemView {
        ElemView {
            ptr: std::ptr::null_mut(),
            len: 0,
            kind: ElemView::NONE,
        }
    }
}

/// Element width of a typed-array view-kind code — `None` for `NONE`/`DENSE`
/// (i.e. "is this a typed-array code" and its byte size in one).
fn ta_code_bytes(code: u64) -> Option<i64> {
    match code {
        ElemView::TA_I8 | ElemView::TA_U8 | ElemView::TA_U8C => Some(1),
        ElemView::TA_I16 | ElemView::TA_U16 => Some(2),
        ElemView::TA_I32 | ElemView::TA_U32 | ElemView::TA_F32 => Some(4),
        ElemView::TA_F64 => Some(8),
        _ => None,
    }
}

/// One-time self-check of the `#[repr(u8)]` dense-slot contract
/// ([`Value::JIT_NUMBER_TAG`] / [`Value::JIT_NUMBER_PAYLOAD_OFFSET`])
/// against a live value: belt-and-braces under the guaranteed RFC 2195
/// layout — if it ever failed, dense views are simply never granted and
/// every dense access takes the helper shims.
pub(crate) fn dense_layout_ok() -> bool {
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
    /// RECURSIVE kernels: the resolved family's flattened upvalue-snapshot
    /// table (`RecFamily::uv_flat`, member-major) — compiled members load
    /// their upvalue registers from here at entry, at offsets fixed at
    /// compile time. Null outside family runs.
    pub rec_uv: *const f64,
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
            rec_uv: std::ptr::null(),
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
                    // A one-pass scan upgrades an all-Number array to the
                    // tag-check-free view.
                    let all_num = arr
                        .iter()
                        .all(|v| matches!(v, crate::value::Value::Number(_)));
                    return ElemView {
                        ptr: arr.as_ptr() as *mut u8,
                        len: arr.len() as u64,
                        kind: if all_num {
                            ElemView::DENSE_NUM
                        } else {
                            ElemView::DENSE
                        },
                    };
                }
                return ElemView::none();
            }
            crate::value::Internal::TypedArray(t) if ElemView::ta_code(t.kind).is_some() => {
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
    let Some(code) = ElemView::ta_code(t.kind) else {
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
        kind: code,
    }
}

/// The view-kind code `elem_view` would grant `o` — without materializing
/// the view (no buffer borrow, no pointers). Used at compile time to bake
/// the compiling activation's per-oslot element kind into the generated
/// code (see [`OslotView`]).
pub(crate) fn elem_kind_code(o: &crate::value::JsObject, allow_dense: bool) -> u64 {
    if !cfg!(target_endian = "little") {
        return ElemView::NONE;
    }
    let b = o.borrow();
    match &b.internal {
        crate::value::Internal::Array(_) if allow_dense && b.own_is_empty() => {
            if dense_layout_ok() {
                ElemView::DENSE
            } else {
                ElemView::NONE
            }
        }
        crate::value::Internal::TypedArray(t) => {
            ElemView::ta_code(t.kind).unwrap_or(ElemView::NONE)
        }
        _ => ElemView::NONE,
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
static STAT_INT_TYPED: AtomicU64 = AtomicU64::new(0);

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
    /// Kernels whose compilation carries an INT-TYPED body alongside the
    /// float body (`jit_ty::analyze` found typeable registers).
    pub int_typed: u64,
}

/// Snapshot the process-wide tier counters.
pub fn stats() -> JitStats {
    JitStats {
        compiled: STAT_COMPILED.load(Ordering::Relaxed),
        declined: STAT_DECLINED.load(Ordering::Relaxed),
        native_runs: STAT_RUNS.load(Ordering::Relaxed),
        elem_shim_calls: STAT_ELEM_SHIM.load(Ordering::Relaxed),
        int_typed: STAT_INT_TYPED.load(Ordering::Relaxed),
    }
}

/// The per-kernel compile-once cache slot ([`Kernel::native`]): empty until
/// the first activation with the tier enabled; then `Some(None)` for a kernel
/// whose translation declined (stay on the interpreter tier forever) or
/// `Some(Some(_))` for compiled code.
pub type NativeCache = OnceCell<Option<NativeKernel>>;

/// Per-function-kernel cache of the synthetic BATCH kernels built AROUND it
/// (`Kernel::batch`, on the CALLBACK's kernel): one slot per [`BatchMode`].
/// Each cached instance is the mode's fixed program ([`batch_kernel`]); what
/// makes it per-callback is its own `native` cache, whose compiled code
/// inlines and identity-checks exactly this callback's proto. Like `native`,
/// a pure performance side effect: never serialized, never observable.
pub type BatchCache = [OnceCell<Rc<Kernel>>; BATCH_MODE_COUNT];

/// Number of [`BatchMode`] variants (the `BatchCache` arity).
pub const BATCH_MODE_COUNT: usize = 7;

/// Which array HOF loop a synthetic batch kernel drives (see
/// [`batch_kernel`] and `Vm::hof_batch`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BatchMode {
    /// `forEach`: call `cb(v, i)` per element, no output.
    ForEach = 0,
    /// `map`: call `cb(v, i)`, write the result to output slot `i`
    /// (a [`ElemView::DENSE_W`] direct write into the hole-filled result).
    Map = 1,
    /// `filter`: call `cb(v, i)`, push `v` onto the output when truthy.
    Filter = 2,
    /// `reduce`: `acc = cb(acc, v, i)` per element.
    Reduce = 3,
    /// `some`: call `cb(v, i)`, exit FOUND on the first truthy result.
    Some = 4,
    /// `every`: call `cb(v, i)`, exit FOUND on the first FALSY result (the
    /// counterexample).
    Every = 5,
    /// `find`/`findIndex`: as `Some` — the caller turns the found index
    /// into the element or the index. (Both spec loops visit holes via
    /// `Get`; a hole bails the element load, and the generic loop resumes
    /// there with the exact hole semantics.)
    Find = 6,
}

impl BatchMode {
    pub(crate) fn idx(self) -> usize {
        self as usize
    }
    /// Whether the callback's result only ever feeds a truthiness branch or
    /// is discarded — the modes that admit boolean-returning callbacks
    /// (see [`callee_eligible`]).
    pub(crate) fn bool_ret_ok(self) -> bool {
        matches!(
            self,
            BatchMode::ForEach
                | BatchMode::Filter
                | BatchMode::Some
                | BatchMode::Every
                | BatchMode::Find
        )
    }
    /// Number of oslots the mode's kernel binds: input only, or input +
    /// output.
    pub(crate) fn n_oslots(self) -> usize {
        match self {
            BatchMode::Map | BatchMode::Filter => 2,
            _ => 1,
        }
    }
    /// Arguments each `CallKernel` site passes — the highest argument index
    /// a batchable callback may consume is `argc - 1`.
    pub(crate) fn argc(self) -> u16 {
        match self {
            BatchMode::Reduce => 3,
            _ => 2,
        }
    }
    /// The code index of the mode's COMPLETION `Exit` (loop ran to the
    /// end); every `Exit` that is neither this nor [`Self::found_pc`] is a
    /// bail, resuming the generic loop at the current index.
    pub(crate) fn done_pc(self) -> usize {
        match self {
            BatchMode::ForEach => 5,
            BatchMode::Map => 6,
            BatchMode::Filter => 7,
            BatchMode::Reduce => 6,
            BatchMode::Some | BatchMode::Every | BatchMode::Find => 6,
        }
    }
    /// EARLY-EXIT modes: the `Exit` taken when the search hit (`some`'s
    /// truthy, `every`'s falsy, `find`'s truthy) at the current index.
    pub(crate) fn found_pc(self) -> Option<usize> {
        match self {
            BatchMode::Some | BatchMode::Every | BatchMode::Find => Some(8),
            _ => None,
        }
    }
}

/// Batch-kernel register layout (window 0): the loop index, the length
/// bound, a push-result scratch, the call result, the accumulator
/// (`reduce`), then the contiguous argument window at [`BREG_ARGS`].
pub(crate) const BREG_I: u16 = 0;
pub(crate) const BREG_LEN: u16 = 1;
const BREG_SCRATCH: u16 = 2;
const BREG_RES: u16 = 3;
pub(crate) const BREG_ACC: u16 = 4;
const BREG_ARGS: u16 = 5;

/// Build the synthetic loop kernel that runs one array HOF entirely as
/// native code: `while (i < len) { v = input[i] (bail: not a clean dense/
/// typed-array element); r = cb(...); <mode's consume>; i += 1 }`. The
/// program is fixed per mode — the callback binds through the standard
/// pinned-callee slot (`CallKernel` fslot 0), compiled against and
/// identity-checked per activation like any other pinned callee — and is
/// only ever run NATIVELY by `Vm::hof_batch` (never by the interpreter
/// loop, whose exit materialization needs a frame): the driver maps each
/// `Exit` index to "done" or "resume the generic loop at `regs[BREG_I]`".
/// Every bail happens BEFORE its op completes, and batchable callbacks are
/// pure register programs, so the generic redo of the current index — which
/// may re-run the callback — is unobservable (same result, no side
/// effects).
pub(crate) fn batch_kernel(mode: BatchMode) -> Kernel {
    use crate::bytecode::{KCallee, KCalleeSrc, Op};
    let exit = |_: usize| KOp::Exit {
        resume_ip: 0,
        shape: 0,
    };
    let header = KOp::BrCmp {
        cmp: CmpOp::Lt,
        a: BREG_I,
        b: BREG_LEN,
        if_true: false,
        target: mode.done_pc() as u16,
    };
    let call = KOp::CallKernel {
        dst: match mode {
            BatchMode::Reduce => BREG_ACC,
            _ => BREG_RES,
        },
        fslot: 0,
        base: BREG_ARGS,
        argc: mode.argc(),
    };
    let next = |back_target: u16| KOp::AddKBr {
        dst: BREG_I,
        a: BREG_I,
        k: 1.0,
        target: back_target,
    };
    let code: Vec<KOp> = match mode {
        BatchMode::ForEach => vec![
            header,
            KOp::LoadElem {
                dst: BREG_ARGS,
                obj: 0,
                idx: BREG_I,
                bail: 6,
            },
            KOp::Mov {
                dst: BREG_ARGS + 1,
                src: BREG_I,
            },
            call,
            next(0),
            exit(5), // done
            exit(6), // element bail
        ],
        BatchMode::Map => vec![
            header,
            KOp::LoadElem {
                dst: BREG_ARGS,
                obj: 0,
                idx: BREG_I,
                bail: 7,
            },
            KOp::Mov {
                dst: BREG_ARGS + 1,
                src: BREG_I,
            },
            call,
            KOp::StoreElem {
                obj: 1,
                idx: BREG_I,
                val: BREG_RES,
                bail: 8,
            },
            next(0),
            exit(6), // done
            exit(7), // element bail
            exit(8), // output-store bail
        ],
        BatchMode::Filter => vec![
            header,
            KOp::LoadElem {
                dst: BREG_ARGS,
                obj: 0,
                idx: BREG_I,
                bail: 8,
            },
            KOp::Mov {
                dst: BREG_ARGS + 1,
                src: BREG_I,
            },
            call,
            KOp::BrFalsy {
                src: BREG_RES,
                target: 6,
            },
            KOp::ArrayPush {
                obj: 1,
                val: BREG_ARGS,
                dst: BREG_SCRATCH,
                bail: 9,
            },
            next(0),
            exit(7), // done
            exit(8), // element bail
            exit(9), // push bail
        ],
        BatchMode::Reduce => vec![
            header,
            KOp::LoadElem {
                dst: BREG_ARGS + 1,
                obj: 0,
                idx: BREG_I,
                bail: 7,
            },
            KOp::Mov {
                dst: BREG_ARGS,
                src: BREG_ACC,
            },
            KOp::Mov {
                dst: BREG_ARGS + 2,
                src: BREG_I,
            },
            call,
            next(0),
            exit(6), // done
            exit(7), // element bail
        ],
        BatchMode::Some | BatchMode::Every | BatchMode::Find => vec![
            header,
            KOp::LoadElem {
                dst: BREG_ARGS,
                obj: 0,
                idx: BREG_I,
                bail: 7,
            },
            KOp::Mov {
                dst: BREG_ARGS + 1,
                src: BREG_I,
            },
            call,
            // `every` searches for the first FALSY result; the others for
            // the first truthy one.
            if mode == BatchMode::Every {
                KOp::BrFalsy {
                    src: BREG_RES,
                    target: 8,
                }
            } else {
                KOp::BrTruthy {
                    src: BREG_RES,
                    target: 8,
                }
            },
            next(0),
            exit(6), // done: exhausted without a hit
            exit(7), // element bail
            exit(8), // FOUND at regs[BREG_I]
        ],
    };
    Kernel {
        code: code.into_boxed_slice(),
        locals: Box::new([]),
        bool_locals: Box::new([]),
        // Frame-local indices are meaningless here (the driver builds the
        // object table directly); only the slot COUNT matters to
        // translation.
        oslots: vec![0u32; mode.n_oslots()].into_boxed_slice(),
        sslots: Box::new([]),
        uses_char_code: false,
        shapes: Box::new([Box::new([]) as Box<[crate::bytecode::KShapeSlot]>]),
        futile: std::cell::Cell::new(0),
        native: NativeCache::new(),
        batch: BatchCache::default(),
        math_used: Box::new([]),
        props_used: Box::new([]),
        callee_slots: Box::new([KCallee {
            source: KCalleeSrc::Oslot(0),
            min_argc: mode.argc(),
        }]),
        n_regs: BREG_ARGS + mode.argc(),
        rec: None,
        ret_bool: false,
        args_used: 0,
        uv_writes: Box::new([]),
        stores_elems: matches!(mode, BatchMode::Map),
        loads_len: false,
        uses_array_push: matches!(mode, BatchMode::Filter),
        uses_array_pop: false,
        fallback: Box::new(Op::Nop),
    }
}

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
    /// FAMILY compilations ([`compile_family`]): the callee map the members
    /// were compiled against, identity-checked with `callees` per
    /// activation. Empty otherwise.
    family_map: Vec<Vec<u8>>,
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
    elem_kinds: &[u64],
    entry_regs: &[f64],
) -> Option<NativeKernel> {
    let native = k.native.get_or_init(|| {
        let specs: Vec<(Rc<crate::bytecode::FuncProto>, u32)> = callee_bfs
            .iter()
            .map(|(bf, wb)| (bf.proto.clone(), *wb))
            .collect();
        match compile(k, &specs, elem_kinds, entry_regs) {
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
pub(crate) fn native_for(k: &Kernel, entry_regs: &[f64]) -> Option<NativeKernel> {
    native_for_loop(k, &[], &[], entry_regs)
}

/// The compiled form of a synthetic BATCH kernel (see [`batch_kernel`] and
/// `Vm::hof_batch`): [`native_for_loop`]'s compile-or-fetch and per-
/// activation callee identity guard, with the batch dialect's boolean-
/// returning-callee relaxation where the mode tolerates it.
pub(crate) fn native_for_batch(
    k: &Kernel,
    callee_bfs: &[(Rc<crate::value::BytecodeFunction>, u32)],
    elem_kinds: &[u64],
    bool_ret_callees: bool,
) -> Option<NativeKernel> {
    let native = k.native.get_or_init(|| {
        let specs: Vec<(Rc<crate::bytecode::FuncProto>, u32)> = callee_bfs
            .iter()
            .map(|(bf, wb)| (bf.proto.clone(), *wb))
            .collect();
        match compile_inner(k, &specs, elem_kinds, &[], bool_ret_callees, true) {
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

/// The compiled form of a RESOLVED recursion family, entered through
/// `k0` — the ENTRY member's kernel, whose cache slot holds the
/// compilation (recursive kernels are reached exclusively through the
/// windowed-executor path; a family entered through a different member
/// compiles under THAT member's kernel). Compiled on first use against the
/// resolving activation's family; a later activation whose resolution
/// differs — other member closures' protos, or a different callee mapping —
/// declines to the windowed executor.
pub(crate) fn native_for_family(k0: &Kernel, fam: &crate::exec::RecFamily) -> Option<NativeKernel> {
    let native = k0.native.get_or_init(|| match compile_family(fam) {
        Some(n) => {
            STAT_COMPILED.fetch_add(1, Ordering::Relaxed);
            Some(n)
        }
        None => {
            STAT_DECLINED.fetch_add(1, Ordering::Relaxed);
            None
        }
    });
    let nk = native.as_ref()?;
    if nk.0.callees.len() != fam.funcs.len()
        || !nk
            .0
            .callees
            .iter()
            .zip(&fam.funcs)
            .all(|(p, bf)| Rc::ptr_eq(p, &bf.proto))
        || nk.0.family_map != fam.callee_map
    {
        return None;
    }
    Some(nk.clone())
}

/// Convenience for the FUNCTION-kernel seam (no oslots/sslots by
/// construction): compile-or-fetch and run with the empty context.
pub(crate) fn maybe_run(
    k: &Kernel,
    regs: &mut [f64],
    interrupt: Option<&AtomicBool>,
) -> Option<i64> {
    let native = native_for(k, regs)?;
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
    STAT_ELEM_SHIM.fetch_add(1, Ordering::Relaxed);
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

/// The signed-integer condition for a compare whose BOTH operands live in
/// the Int domain: identical outcomes to the float compare on the same
/// integer values (the domain admits no NaN and no `-0`).
fn cmp_icc(cmp: CmpOp) -> IntCC {
    match cmp {
        CmpOp::Eq | CmpOp::StrictEq => IntCC::Equal,
        CmpOp::Ne | CmpOp::StrictNe => IntCC::NotEqual,
        CmpOp::Lt => IntCC::SignedLessThan,
        CmpOp::Gt => IntCC::SignedGreaterThan,
        CmpOp::Le => IntCC::SignedLessThanOrEqual,
        CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
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
/// `bool_ret_ok` relaxes the last rule for BATCH kernels whose call result
/// only ever feeds a truthiness branch (`filter`'s predicate, `forEach`'s
/// discarded result): a boolean `Ret` lands as its exact 0.0/1.0 register
/// value, whose truthiness IS `ToBoolean` of the Bool — the result is never
/// materialized as a `Value`, so the type difference cannot be observed.
fn callee_eligible(ck: &Kernel, bool_ret_ok: bool) -> bool {
    ck.rec.is_none()
        && ck.uv_writes.is_empty()
        && eligible(ck, &[])
        && !ck.code.iter().any(|op| match op {
            KOp::Exit { .. } => true,
            KOp::Ret { boolean: true, .. } => !bool_ret_ok,
            _ => false,
        })
}

/// Whether one RESOLVED-FAMILY member's kernel can compile into the
/// family's native functions ([`compile_family`]): no cell writes, a
/// frameless scalar body (function kernels have no oslots/sslots/props by
/// construction — checked anyway), and every op in the subset with
/// `SelfCall` allowed against the member's `n_callees`-entry callee row and
/// `Exit` rejected.
fn rec_member_eligible(k: &Kernel, n_callees: usize) -> bool {
    k.uv_writes.is_empty()
        && k.oslots.is_empty()
        && k.sslots.is_empty()
        && k.props_used.is_empty()
        && eligible_inner(k, &[], Some(n_callees), false)
}

/// Whether every op in `k` is in the subset this backend compiles, with
/// every register index `< n_regs`, every branch target in range, every
/// fall-through / fused skip landing on a real op, and every `CallKernel`
/// aimed at an inlinable resolved callee. A `false` pins the kernel to the
/// interpreter tier — never an error.
fn eligible(k: &Kernel, callees: &[(Rc<crate::bytecode::FuncProto>, u32)]) -> bool {
    eligible_inner(k, callees, None, false)
}

/// `rec_callees` selects the RECURSIVE-body dialect: `Some(n)` makes
/// `SelfCall` legal against an `n`-entry callee row and `Exit` illegal (a
/// frameless recursive body has nothing to exit to). `bool_ret_callees`
/// admits boolean-returning callees (see [`callee_eligible`]) — BATCH
/// kernels only.
fn eligible_inner(
    k: &Kernel,
    callees: &[(Rc<crate::bytecode::FuncProto>, u32)],
    rec_callees: Option<usize>,
    bool_ret_callees: bool,
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
        KOp::Exit { .. } => rec_callees.is_none(),
        KOp::Ret { .. } => true,
        KOp::SelfCall {
            dst,
            base,
            callee,
            argc,
        } => {
            rec_callees.is_some_and(|n| (callee as usize) < n)
                && r(dst)
                && (base as usize + argc as usize) <= n_regs
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
        KOp::CharCodeAt {
            dst,
            str,
            idx,
            bail,
        } => r(dst) && st(str) && r(idx) && t(bail) && next_ok(pc, 1),
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
                    .is_some_and(|ck| callee_eligible(ck, bool_ret_callees))
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
fn compile(
    k: &Kernel,
    callees: &[(Rc<crate::bytecode::FuncProto>, u32)],
    elem_kinds: &[u64],
    entry_regs: &[f64],
) -> Option<NativeKernel> {
    compile_inner(k, callees, elem_kinds, entry_regs, false, false)
}

/// [`compile`] with the batch dialect (`batch`): the boolean-returning-
/// callee relaxation where the mode tolerates it (see [`callee_eligible`])
/// and NO int-typing pass — a batch kernel's register file is driver-
/// populated, outside the locals-map layout the typing's entry rules read.
fn compile_inner(
    k: &Kernel,
    callees: &[(Rc<crate::bytecode::FuncProto>, u32)],
    elem_kinds: &[u64],
    entry_regs: &[f64],
    bool_ret_callees: bool,
    batch: bool,
) -> Option<NativeKernel> {
    if !eligible_inner(k, callees, None, bool_ret_callees) {
        return None;
    }
    let typing = if batch {
        None
    } else {
        crate::jit_ty::analyze(k, elem_kinds, entry_regs).map(Rc::new)
    };
    if typing.is_some() {
        STAT_INT_TYPED.fetch_add(1, Ordering::Relaxed);
    }
    if std::env::var_os("CJIT_TY_DEBUG").is_some() {
        eprintln!("--- kernel ({} ops) ---", k.code.len());
        for (pc, op) in k.code.iter().enumerate() {
            eprintln!("  {pc:3}: {op:?}");
        }
        match &typing {
            None => eprintln!("  typing: none"),
            Some(t) => eprintln!(
                "  typing: {:?}\n  checks: {:?}\n  temps: {:?}",
                t.ty, t.checks, t.temps
            ),
        }
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
        Translator::new(builder, &mut module, k, callees, elem_kinds, typing)
            .translate(frontend_config)?;
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
        family_map: Vec::new(),
        min_regs: KWIN * (1 + callees.len()),
    })))
}

/// Compile a RESOLVED recursion family ([`crate::exec::RecFamily`]) into
/// one native function per member plus a standard-signature wrapper:
///
/// - `rec<j>(args…, depth, interrupt, ctx) -> f64` — member `j`'s body,
///   with every `SelfCall` a direct native call to the member its callee id
///   resolved to (depth-guarded and interrupt-polled at the interpreter's
///   exact points; a flagged abandon unwinds every frame through
///   `ctx.abandon`). Upvalue registers load from the activation's
///   `JitCtx::rec_uv` table at compile-fixed offsets.
/// - `kernel(regs, interrupt, ctx) -> i64` — the exported entry: loads the
///   pre-guarded window-0 arguments from the register buffer, calls `rec0`
///   with `ctx.depth` (the remaining call-depth budget the seam computed),
///   and returns `0` with the raw result in `ctx.scratch`, or
///   [`REC_ABANDONED`] / [`INTERRUPTED`].
///
/// The compiled code is keyed to the RESOLVED family: member protos and the
/// callee map are stored on the [`Compiled`] and identity-checked by every
/// later activation ([`native_for_family`]); a family resolving differently
/// (a reassigned cell or global) runs that activation on the windowed
/// executor.
fn compile_family(fam: &crate::exec::RecFamily) -> Option<NativeKernel> {
    let n = fam.funcs.len();
    let kernels: Vec<&Kernel> = fam
        .funcs
        .iter()
        .map(|f| f.proto.fn_kernel.as_ref())
        .collect::<Option<_>>()?;
    for (j, k) in kernels.iter().enumerate() {
        if !rec_member_eligible(k, fam.callee_map[j].len()) {
            return None;
        }
    }
    cranelift_native::builder().ok()?;
    let mut builder =
        JITBuilder::with_flags(&[("opt_level", "speed")], default_libcall_names()).ok()?;
    for (name, addr, _, _) in helper_table() {
        builder.symbol(name, addr);
    }
    let mut module = JITModule::new(builder);
    let ptr_ty = module.target_config().pointer_type();
    // Per-member window-slot roles, in the fixed orders the signatures and
    // the upvalue table use (the same `locals` iteration the resolver's
    // `arg_slots`/`uv_snaps` were built from).
    let mut arg_slots: Vec<Vec<(usize, u16)>> = Vec::with_capacity(n);
    let mut upval_slots: Vec<Vec<usize>> = Vec::with_capacity(n);
    for k in kernels.iter() {
        let mut aslots = Vec::new();
        let mut uslots = Vec::new();
        for (r, slot) in k.locals.iter().enumerate() {
            match slot {
                crate::bytecode::KSlot::Arg(a) => aslots.push((r, *a as u16)),
                crate::bytecode::KSlot::Upvalue(_) => uslots.push(r),
                crate::bytecode::KSlot::Local(_) => {}
            }
        }
        arg_slots.push(aslots);
        upval_slots.push(uslots);
    }
    // The flattened upvalue table's per-member bases (member-major, matching
    // `RecFamily::uv_flat`).
    let mut uv_bases = Vec::with_capacity(n);
    let mut acc = 0usize;
    for u in upval_slots.iter() {
        uv_bases.push(acc);
        acc += u.len();
    }
    let callee_args: Rc<Vec<Vec<u16>>> = Rc::new(
        arg_slots
            .iter()
            .map(|s| s.iter().map(|&(_, a)| a).collect())
            .collect(),
    );
    let mut member_ids = Vec::with_capacity(n);
    for (j, aslots) in arg_slots.iter().enumerate() {
        let mut sig = module.make_signature();
        for _ in 0..aslots.len() {
            sig.params.push(AbiParam::new(types::F64));
        }
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(types::F64));
        member_ids.push(
            module
                .declare_function(&format!("rec{j}"), Linkage::Local, &sig)
                .ok()?,
        );
    }
    let mut wrap_sig = module.make_signature();
    wrap_sig.params.push(AbiParam::new(ptr_ty));
    wrap_sig.params.push(AbiParam::new(ptr_ty));
    wrap_sig.params.push(AbiParam::new(ptr_ty));
    wrap_sig.returns.push(AbiParam::new(types::I64));
    let wrap_id = module
        .declare_function("kernel", Linkage::Export, &wrap_sig)
        .ok()?;
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut mctx = module.make_context();
    // Member bodies.
    for j in 0..n {
        let mut sig = module.make_signature();
        for _ in 0..arg_slots[j].len() {
            sig.params.push(AbiParam::new(types::F64));
        }
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(types::F64));
        mctx.func.signature = sig;
        {
            let frontend_config = module.target_config();
            let b = FunctionBuilder::new(&mut mctx.func, &mut fb_ctx);
            let cmap: Vec<u8> = fam.callee_map[j].clone();
            Translator::new_family_member(
                b,
                &mut module,
                kernels[j],
                &member_ids,
                cmap,
                callee_args.clone(),
                &arg_slots[j],
                &upval_slots[j],
                uv_bases[j],
            )
            .translate(frontend_config)?;
        }
        module.define_function(member_ids[j], &mut mctx).ok()?;
        module.clear_context(&mut mctx);
    }
    // The wrapper.
    mctx.func.signature = wrap_sig;
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
        let rec_ref = module.declare_func_in_func(member_ids[0], b.func);
        let mut args = Vec::with_capacity(arg_slots[0].len() + 3);
        for &(r, _) in &arg_slots[0] {
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
        callees: fam.funcs.iter().map(|f| f.proto.clone()).collect(),
        family_map: fam.callee_map.clone(),
        min_regs: KWIN,
    })))
}

/// One oslot's entry-hoisted direct-view state (see `Translator::ta_views`).
#[derive(Clone, Copy)]
struct OslotView {
    ptr: cranelift_codegen::ir::Value,
    /// The element count as an f64 (`.length` reads).
    len_f64: cranelift_codegen::ir::Value,
    /// The element count as the raw i64 (int-typed `.length` reads).
    len_i64: cranelift_codegen::ir::Value,
    /// `umin(len, 2^32 - 1)`: a single unsigned `index < bound` compare is
    /// then `dense_index`'s full range condition (negative indices wrap to
    /// huge unsigned values and fail it too).
    bound: cranelift_codegen::ir::Value,
    /// `kind == baked` / `kind == DENSE` / `kind != NONE`, as i8 tests.
    /// `is_ta` compares against the BAKED typed-array kind code (below) —
    /// constant false when the compiling activation had no typed-array view
    /// for this oslot.
    is_ta: cranelift_codegen::ir::Value,
    is_dense: cranelift_codegen::ir::Value,
    /// `kind == DENSE_NUM`: the pre-scanned all-Number dense view (reads
    /// skip the tag check).
    is_dnum: cranelift_codegen::ir::Value,
    is_direct: cranelift_codegen::ir::Value,
    /// The compiling activation's [`ElemView`] kind code for this oslot: the
    /// ONE typed-array kind this code carries a direct load/store sequence
    /// for. `NONE`/`DENSE` = no baked typed-array arm.
    baked: u64,
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
    /// The INT-TYPING result for window-0 registers (`jit_ty::analyze`),
    /// when any register typed `Int`: the function then carries TWO bodies —
    /// the float body (exactly the untyped emission) and an int body whose
    /// `Int` registers live in `ivars` — selected once at entry by the
    /// typing's runtime checks. `None` = single float body, as ever.
    typing: Option<Rc<crate::jit_ty::RegTyping>>,
    /// I64 variables for `Int`-typed window-0 registers (index-parallel to
    /// window 0; `None` for float registers).
    ivars: Vec<Option<Variable>>,
    /// Whether ops are currently being emitted into the INT body (swapped
    /// alongside `cur_blocks`; always false in the float body, inlined
    /// callee windows, and rec mode).
    in_int_body: bool,
    /// The entry dispatch block `new` parks between the hoists and the
    /// body: `translate` fills it with the typing's entry checks (or a
    /// plain jump). `None` in rec mode.
    dispatch: Option<Block>,
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

/// The RECURSIVE-body emission mode (see [`compile_family`]): the function
/// being built is one member of a resolved recursion family, not the
/// standard register-buffer wrapper — `Ret` returns its value directly, and
/// `SelfCall` becomes a real native call to the member its callee id
/// resolved to.
#[derive(Clone)]
struct RecMode {
    /// Every family member's function, importable from this one; a call
    /// site picks `member_fns[cmap[callee]]`.
    member_fns: Vec<FuncRef>,
    /// This member's callee row (`RecFamily::callee_map[j]`).
    cmap: Vec<u8>,
    /// Per MEMBER: the kernel-arg index of each of its parameters, in
    /// signature order — a call to member `t` passes `window[base + a]` for
    /// each `a` in `callee_args[t]`.
    callee_args: Rc<Vec<Vec<u16>>>,
    /// This frame's remaining depth budget (an i64 parameter).
    depth: cranelift_codegen::ir::Value,
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
        elem_kinds: &[u64],
        typing: Option<Rc<crate::jit_ty::RegTyping>>,
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
                // The one typed-array kind this oslot's direct sequences are
                // emitted for: the compiling activation's (`elem_kinds`).
                // Later activations pinning any other kind fail `is_ta` and
                // take the helper shims.
                let baked = elem_kinds.get(i).copied().unwrap_or(ElemView::NONE);
                let is_ta = if ta_code_bytes(baked).is_some() || baked == ElemView::DENSE_W {
                    b.ins().icmp_imm_s(IntCC::Equal, kind, baked as i64)
                } else {
                    b.ins().iconst(types::I8, 0)
                };
                let is_dense = b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, kind, ElemView::DENSE as i64);
                let is_dnum = b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, kind, ElemView::DENSE_NUM as i64);
                let is_direct = b.ins().icmp_imm_s(IntCC::NotEqual, kind, 0);
                ta_views.push(OslotView {
                    ptr,
                    len_f64,
                    len_i64: len,
                    bound,
                    is_ta,
                    is_dense,
                    is_dnum,
                    is_direct,
                    baked,
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
        // I64 variables for the int body's typed registers.
        let mut ivars: Vec<Option<Variable>> = vec![None; n_regs];
        if let Some(t) = &typing {
            for (r, slot) in ivars.iter_mut().enumerate() {
                if t.ty[r] == crate::jit_ty::RegTy::Int {
                    *slot = Some(b.declare_var(types::I64));
                }
            }
        }
        let blocks: Vec<Block> = (0..k.code.len()).map(|_| b.create_block()).collect();
        let exit_block = b.create_block();
        b.append_block_param(exit_block, types::I64);
        let intr_block = b.create_block();
        // `translate` fills the dispatch with the typing's entry checks (or
        // a plain jump to the float body).
        let dispatch = b.create_block();
        b.ins().jump(dispatch, &[]);
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
            typing,
            ivars,
            in_int_body: false,
            dispatch: Some(dispatch),
        }
    }

    /// Constructor for a RECURSIVE body (see [`compile_rec`]): the function
    /// under construction is the self-callable inner function — window
    /// registers come from PARAMETERS (arguments, then upvalue snapshots;
    /// locals are scratch) rather than the register buffer, and the landing
    /// blocks (depth-abandon, interrupt-abandon, post-call unwind) are
    /// pre-filled here. Signature:
    /// `(args…, upvals…, depth: i64, interrupt: ptr, ctx: ptr) -> f64`.
    /// Constructor for one FAMILY MEMBER's body (see [`compile_family`]):
    /// window registers come from PARAMETERS (this member's consumed
    /// arguments) and the activation's flattened upvalue-snapshot table
    /// (`JitCtx::rec_uv`, this member's slice at `uv_flat_base`); locals are
    /// scratch. Signature: `(args…, depth: i64, interrupt: ptr, ctx: ptr)
    /// -> f64`. The landing blocks (depth-abandon, interrupt-abandon,
    /// post-call unwind) are pre-filled here.
    #[expect(
        clippy::too_many_arguments,
        reason = "one-call-site constructor threading the family tables"
    )]
    fn new_family_member(
        mut b: FunctionBuilder<'a>,
        module: &'a mut JITModule,
        k: &'a Kernel,
        member_ids: &[cranelift_module::FuncId],
        cmap: Vec<u8>,
        callee_args: Rc<Vec<Vec<u16>>>,
        arg_slots: &[(usize, u16)],
        upval_slots: &[usize],
        uv_flat_base: usize,
    ) -> Self {
        let member_fns: Vec<FuncRef> = member_ids
            .iter()
            .map(|id| module.declare_func_in_func(*id, b.func))
            .collect();
        let ptr_ty = module.target_config().pointer_type();
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let params: Vec<cranelift_codegen::ir::Value> = b.block_params(entry).to_vec();
        let n_args = arg_slots.len();
        let depth = params[n_args];
        let int_ptr = params[n_args + 1];
        let ctx_ptr = params[n_args + 2];
        let n_regs = k.n_regs as usize;
        let mut init: Vec<Option<cranelift_codegen::ir::Value>> = vec![None; n_regs];
        for (j, &(r, _)) in arg_slots.iter().enumerate() {
            init[r] = Some(params[j]);
        }
        if !upval_slots.is_empty() {
            let uv_tab = b.ins().load(
                ptr_ty,
                MemFlagsData::trusted(),
                ctx_ptr,
                std::mem::offset_of!(JitCtx, rec_uv) as i32,
            );
            for (j, &r) in upval_slots.iter().enumerate() {
                let v = b.ins().load(
                    types::F64,
                    MemFlagsData::trusted(),
                    uv_tab,
                    (8 * (uv_flat_base + j)) as i32,
                );
                init[r] = Some(v);
            }
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
        Translator {
            b,
            module,
            k,
            cur_blocks: blocks,
            reg_base: 0,
            inline_ret: None,
            callees: Vec::new(),
            rec: Some(RecMode {
                member_fns,
                cmap,
                callee_args,
                depth,
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
            typing: None,
            ivars: Vec::new(),
            in_int_body: false,
            dispatch: None,
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

    /// The current body's type for register `r`: `Int` only inside the int
    /// body, for window-0 registers the typing marked (inlined callee
    /// windows are always float).
    fn rty(&self, r: u16) -> crate::jit_ty::RegTy {
        if self.in_int_body && self.reg_base == 0 {
            if let Some(t) = &self.typing {
                return t.ty[r as usize];
            }
        }
        crate::jit_ty::RegTy::Float
    }

    fn is_int(&self, r: u16) -> bool {
        self.rty(r) == crate::jit_ty::RegTy::Int
    }

    /// Register read as f64 — an `Int` register converts (exact: every
    /// value the typing admits is far below 2^53).
    fn get(&mut self, r: u16) -> cranelift_codegen::ir::Value {
        if self.is_int(r) {
            let iv = self.b.use_var(self.ivars[r as usize].expect("Int reg"));
            self.b.ins().fcvt_from_sint(types::F64, iv)
        } else {
            self.b.use_var(self.vars[self.reg_base + r as usize])
        }
    }

    /// A register whose entry check baked an exact value (a read-only `%`
    /// divisor): every read is that constant.
    fn baked_const(&self, r: u16) -> Option<i64> {
        if !self.in_int_body || self.reg_base != 0 {
            return None;
        }
        let t = self.typing.as_ref()?;
        t.checks.iter().find_map(|&(cr, chk)| {
            if cr == r {
                if let crate::jit_ty::EntryCheck::Exact(v) = chk {
                    return Some(v);
                }
            }
            None
        })
    }

    /// Register read as i64. Caller must have checked `is_int`. A baked
    /// divisor reads as its constant so Cranelift's divide-by-constant
    /// strength reduction can see it.
    fn get_int(&mut self, r: u16) -> cranelift_codegen::ir::Value {
        if let Some(v) = self.baked_const(r) {
            return self.b.ins().iconst(types::I64, v);
        }
        self.b.use_var(self.ivars[r as usize].expect("Int reg"))
    }

    /// Register write from an f64 value — an `Int` destination converts
    /// (exact by the typing's def rules: the value is a bounded integer).
    fn set(&mut self, r: u16, v: cranelift_codegen::ir::Value) {
        if self.is_int(r) {
            let iv = self.b.ins().fcvt_to_sint_sat(types::I64, v);
            self.b.def_var(self.ivars[r as usize].expect("Int reg"), iv);
        } else {
            self.b.def_var(self.vars[self.reg_base + r as usize], v);
        }
    }

    /// Register write from an i64 value. Caller must have checked `is_int`.
    fn set_int(&mut self, r: u16, v: cranelift_codegen::ir::Value) {
        self.b.def_var(self.ivars[r as usize].expect("Int reg"), v);
    }

    /// An operand for the ToInt32 bitwise family as an i32: an `Int`
    /// register truncates directly (`ToInt32` of an integer IS its low 32
    /// bits), anything else runs the shared inline ToInt32.
    fn operand_i32(&mut self, r: u16) -> Option<cranelift_codegen::ir::Value> {
        if self.is_int(r) {
            let iv = self.get_int(r);
            Some(self.b.ins().ireduce(types::I32, iv))
        } else {
            let v = self.get(r);
            self.toint32(v)
        }
    }

    /// Compare two registers with JS numeric semantics: both `Int` →
    /// signed integer compare (identical to the f64 compare on the same
    /// integer values — no NaN, no `-0` in the Int domain), else float.
    fn cmp_regs(
        &mut self,
        cmp: CmpOp,
        a: u16,
        b: u16,
    ) -> cranelift_codegen::ir::Value {
        if self.is_int(a) && self.is_int(b) {
            let (x, y) = (self.get_int(a), self.get_int(b));
            self.b.ins().icmp(cmp_icc(cmp), x, y)
        } else {
            let (x, y) = (self.get(a), self.get(b));
            self.b.ins().fcmp(cmp_cc(cmp), x, y)
        }
    }

    /// As [`Self::cmp_regs`] with a constant rhs.
    fn cmp_reg_const(
        &mut self,
        cmp: CmpOp,
        a: u16,
        k: f64,
    ) -> cranelift_codegen::ir::Value {
        if self.is_int(a) && k.fract() == 0.0 && k.abs() <= 4_503_599_627_370_496.0 {
            let x = self.get_int(a);
            self.b.ins().icmp_imm_s(cmp_icc(cmp), x, k as i64)
        } else {
            let x = self.get(a);
            let kk = self.b.ins().f64const(k);
            self.b.ins().fcmp(cmp_cc(cmp), x, kk)
        }
    }

    /// ToBoolean of a register: an `Int` register is truthy iff nonzero
    /// (no NaN/-0 in the Int domain), else the float test.
    fn truthy_reg(&mut self, r: u16) -> cranelift_codegen::ir::Value {
        if self.is_int(r) {
            let x = self.get_int(r);
            self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0)
        } else {
            let x = self.get(r);
            self.truthy(x)
        }
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

    /// Element READ (`KOp::LoadElem` semantics): a direct bounds-checked
    /// load-and-convert for a typed-array view of the BAKED kind (see
    /// [`OslotView::baked`]), the interpreter's shared fast-path core
    /// through the helper shim for everything else; any fast-path failure
    /// jumps to the op's bail edge (an Exit stub), exactly like the
    /// interpreter arm.
    fn emit_elem_load(
        &mut self,
        pc: usize,
        obj: u16,
        idx_reg: u16,
        bail: u16,
    ) -> Option<cranelift_codegen::ir::Value> {
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let chk_dense = self.b.create_block();
        let dense_b = self.b.create_block();
        let chk_dnum = self.b.create_block();
        let dnum_b = self.b.create_block();
        let helper = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, types::F64);
        // Baked typed array: bounds-checked load at the kind's width, then
        // the kind's exact `decode` conversion. Emitted only when the
        // compiling activation had a typed-array view (otherwise `is_ta` is
        // constant false and the arm would be dead weight).
        if ta_code_bytes(view.baked).is_some() {
            let ta_b = self.b.create_block();
            self.b.ins().brif(view.is_ta, ta_b, &[], chk_dense, &[]);
            self.b.switch_to_block(ta_b);
            let (ok, ii) = self.elem_index(idx_reg, view.bound);
            let load_b = self.b.create_block();
            self.b.ins().brif(ok, load_b, &[], bail_b, &[]);
            self.b.switch_to_block(load_b);
            let v = self.ta_load(view, ii);
            self.b.ins().jump(join, &[v.into()]);
        } else {
            self.b.ins().jump(chk_dense, &[]);
        }
        // Pre-scanned all-Number dense view: one bounds compare, one
        // payload load — the grant's scan already proved every slot's tag.
        self.b.switch_to_block(chk_dense);
        self.b.ins().brif(view.is_dnum, dnum_b, &[], chk_dnum, &[]);
        self.b.switch_to_block(dnum_b);
        let (ok, ii) = self.elem_index(idx_reg, view.bound);
        let num_b = self.b.create_block();
        self.b.ins().brif(ok, num_b, &[], bail_b, &[]);
        self.b.switch_to_block(num_b);
        let stride_n = self.b.ins().iconst(
            types::I64,
            std::mem::size_of::<crate::value::Value>() as i64,
        );
        let off_n = self.b.ins().imul(ii, stride_n);
        let slot_n = self.b.ins().iadd(view.ptr, off_n);
        let vn = self.b.ins().load(
            types::F64,
            MemFlagsData::trusted(),
            slot_n,
            crate::value::Value::JIT_NUMBER_PAYLOAD_OFFSET as i32,
        );
        self.b.ins().jump(join, &[vn.into()]);
        self.b.switch_to_block(chk_dnum);
        self.b.ins().brif(view.is_dense, dense_b, &[], helper, &[]);
        // Dense array (read-only kernels): bounds check, then the slot's
        // repr(u8) tag must be `Number` (a hole or any other variant bails,
        // exactly like the interpreter's fast-path miss), then the payload.
        self.b.switch_to_block(dense_b);
        let (ok, ii) = self.elem_index(idx_reg, view.bound);
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
        let idx_f = self.get(idx_reg);
        let st = self.callh("cjit_elem_load", &[self.ctx_ptr, oslot, idx_f])?;
        let ok_b = self.b.create_block();
        self.b.ins().brif(st, ok_b, &[], bail_b, &[]);
        self.b.switch_to_block(ok_b);
        let v = self.scratch();
        self.b.ins().jump(join, &[v.into()]);
        self.b.switch_to_block(join);
        Some(res)
    }

    /// [`Self::emit_elem_load`] with an INT-typed destination (the typing
    /// grants this only over a baked integer typed-array kind): the direct
    /// arm loads the element RAW (no float conversion); every other view —
    /// a mismatched kind, a dense array, no view — takes the op's bail
    /// edge, exactly as the interpreter's fast-path miss would, since only
    /// the baked kind guarantees an integer value for the i64 register.
    fn emit_elem_load_int(
        &mut self,
        pc: usize,
        obj: u16,
        idx_reg: u16,
        bail: u16,
    ) -> Option<cranelift_codegen::ir::Value> {
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let ta_b = self.b.create_block();
        let load_b = self.b.create_block();
        self.b.ins().brif(view.is_ta, ta_b, &[], bail_b, &[]);
        self.b.switch_to_block(ta_b);
        let (ok, ii) = self.elem_index(idx_reg, view.bound);
        self.b.ins().brif(ok, load_b, &[], bail_b, &[]);
        self.b.switch_to_block(load_b);
        Some(self.ta_load_int(view, ii))
    }

    /// Element WRITE (`KOp::StoreElem` semantics): direct convert-and-store
    /// on a typed-array view of the baked kind (in-place only — the view's
    /// length is fixed, so an append is out of bounds and bails, exactly as
    /// a typed array's OOB store must), the shared core via the shim
    /// otherwise. `Uint8ClampedArray` has no direct store arm (its clamp is
    /// not the integer kinds' wraparound): its stores always take the shim.
    fn emit_elem_store(
        &mut self,
        pc: usize,
        obj: u16,
        idx_reg: u16,
        val_reg: u16,
        bail: u16,
    ) -> Option<()> {
        // A dense view is never granted to a kernel containing stores, so
        // the direct arm here is the baked typed-array kind only.
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let helper = self.b.create_block();
        let join = self.b.create_block();
        let direct_ok = (ta_code_bytes(view.baked).is_some() && view.baked != ElemView::TA_U8C)
            || view.baked == ElemView::DENSE_W;
        if direct_ok {
            let direct = self.b.create_block();
            self.b.ins().brif(view.is_ta, direct, &[], helper, &[]);
            self.b.switch_to_block(direct);
            let (ok, ii) = self.elem_index(idx_reg, view.bound);
            let store_b = self.b.create_block();
            self.b.ins().brif(ok, store_b, &[], bail_b, &[]);
            self.b.switch_to_block(store_b);
            if view.baked == ElemView::DENSE_W {
                // Writable dense slot (batch `map` output): tag byte +
                // payload, the `Value::Number` layout the read path checks.
                let val = self.get(val_reg);
                let stride = self.b.ins().iconst(
                    types::I64,
                    std::mem::size_of::<crate::value::Value>() as i64,
                );
                let off = self.b.ins().imul(ii, stride);
                let slot = self.b.ins().iadd(view.ptr, off);
                let tag = self.b.ins().iconst(
                    types::I64,
                    i64::from(crate::value::Value::JIT_NUMBER_TAG),
                );
                self.b.ins().istore8(MemFlagsData::trusted(), tag, slot, 0);
                self.b.ins().store(
                    MemFlagsData::trusted(),
                    val,
                    slot,
                    crate::value::Value::JIT_NUMBER_PAYLOAD_OFFSET as i32,
                );
            } else if self.is_int(val_reg) {
                let iv = self.get_int(val_reg);
                self.ta_store_int(view, ii, iv);
            } else {
                let val = self.get(val_reg);
                self.ta_store(view, ii, val);
            }
            self.b.ins().jump(join, &[]);
        } else {
            self.b.ins().jump(helper, &[]);
        }
        self.b.switch_to_block(helper);
        let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
        let idx_f = self.get(idx_reg);
        let val_f = self.get(val_reg);
        let st = self.callh("cjit_elem_store", &[self.ctx_ptr, oslot, idx_f, val_f])?;
        self.b.ins().brif(st, join, &[], bail_b, &[]);
        self.b.switch_to_block(join);
        Some(())
    }

    /// The address of baked-kind element `ii` of `view`'s storage.
    fn ta_addr(
        &mut self,
        view: OslotView,
        ii: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let size = ta_code_bytes(view.baked).expect("caller checked baked TA kind");
        let size_v = self.b.ins().iconst(types::I64, size);
        let off = self.b.ins().imul(ii, size_v);
        self.b.ins().iadd(view.ptr, off)
    }

    /// The baked kind's exact `decode`: a little-endian load at the element
    /// width, sign-/zero-extended per kind, converted to f64 (every element
    /// value is exactly representable — the widest integer kind is 32-bit,
    /// and f32 promotes exactly).
    fn ta_load(
        &mut self,
        view: OslotView,
        ii: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let addr = self.ta_addr(view, ii);
        let mf = MemFlagsData::new();
        match view.baked {
            ElemView::TA_F64 => self.b.ins().load(types::F64, mf, addr, 0),
            ElemView::TA_F32 => {
                let f = self.b.ins().load(types::F32, mf, addr, 0);
                self.b.ins().fpromote(types::F64, f)
            }
            _ => {
                let x = match view.baked {
                    ElemView::TA_I8 => self.b.ins().sload8(types::I64, mf, addr, 0),
                    ElemView::TA_U8 | ElemView::TA_U8C => {
                        self.b.ins().uload8(types::I64, mf, addr, 0)
                    }
                    ElemView::TA_I16 => self.b.ins().sload16(types::I64, mf, addr, 0),
                    ElemView::TA_U16 => self.b.ins().uload16(types::I64, mf, addr, 0),
                    ElemView::TA_I32 => self.b.ins().sload32(mf, addr, 0),
                    _ => self.b.ins().uload32(mf, addr, 0), // TA_U32
                };
                self.b.ins().fcvt_from_sint(types::F64, x)
            }
        }
    }

    /// [`Self::ta_load`] for an INT destination over a baked integer kind:
    /// the raw widened load, no float conversion.
    fn ta_load_int(
        &mut self,
        view: OslotView,
        ii: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let addr = self.ta_addr(view, ii);
        let mf = MemFlagsData::new();
        match view.baked {
            ElemView::TA_I8 => self.b.ins().sload8(types::I64, mf, addr, 0),
            ElemView::TA_U8 | ElemView::TA_U8C => self.b.ins().uload8(types::I64, mf, addr, 0),
            ElemView::TA_I16 => self.b.ins().sload16(types::I64, mf, addr, 0),
            ElemView::TA_U16 => self.b.ins().uload16(types::I64, mf, addr, 0),
            ElemView::TA_I32 => self.b.ins().sload32(mf, addr, 0),
            _ => self.b.ins().uload32(mf, addr, 0), // TA_U32 (typing gate)
        }
    }

    /// [`Self::ta_store`] from an INT value: the codec's `to_int` of an
    /// in-band integer is the integer itself (no saturation region, no
    /// non-finite case), so the store is just the wrapping narrow store;
    /// float kinds convert exactly first.
    fn ta_store_int(
        &mut self,
        view: OslotView,
        ii: cranelift_codegen::ir::Value,
        ival: cranelift_codegen::ir::Value,
    ) {
        let addr = self.ta_addr(view, ii);
        let mf = MemFlagsData::new();
        match view.baked {
            ElemView::TA_F64 => {
                let f = self.b.ins().fcvt_from_sint(types::F64, ival);
                self.b.ins().store(mf, f, addr, 0);
            }
            ElemView::TA_F32 => {
                let f = self.b.ins().fcvt_from_sint(types::F64, ival);
                let f32v = self.b.ins().fdemote(types::F32, f);
                self.b.ins().store(mf, f32v, addr, 0);
            }
            ElemView::TA_I8 | ElemView::TA_U8 => {
                self.b.ins().istore8(mf, ival, addr, 0);
            }
            ElemView::TA_I16 | ElemView::TA_U16 => {
                self.b.ins().istore16(mf, ival, addr, 0);
            }
            _ => {
                // TA_I32 / TA_U32 (TA_U8C has no direct store arm).
                self.b.ins().istore32(mf, ival, addr, 0);
            }
        }
    }

    /// The baked kind's exact `encode`: f64 stores the value, f32 demotes
    /// (IEEE round-to-nearest — exactly `v as f32`), and the integer kinds
    /// inline `to_int` + the wrapping element cast — non-finite to 0
    /// (`fcvt_to_sint_sat` gives NaN 0 already; the finiteness select fixes
    /// ±Inf, which it would saturate), otherwise truncate-and-saturate to
    /// i64 exactly like `t as i64`, then store the low bytes (`istore8/16/
    /// 32` — the `as i8`/`as u8`/… wrap).
    fn ta_store(
        &mut self,
        view: OslotView,
        ii: cranelift_codegen::ir::Value,
        val: cranelift_codegen::ir::Value,
    ) {
        let addr = self.ta_addr(view, ii);
        let mf = MemFlagsData::new();
        match view.baked {
            ElemView::TA_F64 => {
                self.b.ins().store(mf, val, addr, 0);
            }
            ElemView::TA_F32 => {
                let f = self.b.ins().fdemote(types::F32, val);
                self.b.ins().store(mf, f, addr, 0);
            }
            _ => {
                let sat = self.b.ins().fcvt_to_sint_sat(types::I64, val);
                let d = self.b.ins().fsub(val, val);
                let zero_f = self.b.ins().f64const(0.0);
                let finite = self.b.ins().fcmp(FloatCC::Equal, d, zero_f);
                let zero_i = self.b.ins().iconst(types::I64, 0);
                let t = self.b.ins().select(finite, sat, zero_i);
                match view.baked {
                    ElemView::TA_I8 | ElemView::TA_U8 => {
                        self.b.ins().istore8(mf, t, addr, 0);
                    }
                    ElemView::TA_I16 | ElemView::TA_U16 => {
                        self.b.ins().istore16(mf, t, addr, 0);
                    }
                    _ => {
                        // TA_I32 / TA_U32 (TA_U8C never reaches here — no
                        // direct store arm is emitted for it).
                        self.b.ins().istore32(mf, t, addr, 0);
                    }
                }
            }
        }
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

    /// [`Self::elem_index_ok`] from an index REGISTER: an `Int` register
    /// needs no integrality round-trip — one unsigned compare (a negative
    /// index wraps huge and fails the bound, exactly like the float form).
    fn elem_index(
        &mut self,
        idx_reg: u16,
        bound: cranelift_codegen::ir::Value,
    ) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
        if self.is_int(idx_reg) {
            let ii = self.get_int(idx_reg);
            let ok = self.b.ins().icmp(IntCC::UnsignedLessThan, ii, bound);
            (ok, ii)
        } else {
            let idx = self.get(idx_reg);
            self.elem_index_ok(idx, bound)
        }
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

    /// [`Self::emit_len`] with an INT-typed destination: the raw i64 view
    /// length directly, or the helper's (integral) result converted; either
    /// arm bails past the typing's length band (`jit_ty::LEN_HI`) — a
    /// bail is always a legal outcome, and lengths beyond 2^48 cannot
    /// occur on real allocations anyway.
    fn emit_len_int(
        &mut self,
        pc: usize,
        obj: u16,
        bail: u16,
    ) -> Option<cranelift_codegen::ir::Value> {
        let view = self.ta_views[obj as usize];
        let bail_b = self.dest(pc, bail as usize);
        let direct = self.b.create_block();
        let helper = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, types::I64);
        self.b.ins().brif(view.is_direct, direct, &[], helper, &[]);
        self.b.switch_to_block(direct);
        let small = self.b.ins().icmp_imm_s(
            IntCC::SignedLessThanOrEqual,
            view.len_i64,
            crate::jit_ty::LEN_HI as i64,
        );
        let dir_ok = self.b.create_block();
        self.b.ins().brif(small, dir_ok, &[], bail_b, &[]);
        self.b.switch_to_block(dir_ok);
        self.b.ins().jump(join, &[view.len_i64.into()]);
        self.b.switch_to_block(helper);
        let oslot = self.b.ins().iconst(types::I64, i64::from(obj));
        let st = self.callh("cjit_elem_len", &[self.ctx_ptr, oslot])?;
        let ok_b = self.b.create_block();
        self.b.ins().brif(st, ok_b, &[], bail_b, &[]);
        self.b.switch_to_block(ok_b);
        let v = self.scratch();
        let iv = self.b.ins().fcvt_to_sint_sat(types::I64, v);
        let small2 = self.b.ins().icmp_imm_s(
            IntCC::SignedLessThanOrEqual,
            iv,
            crate::jit_ty::LEN_HI as i64,
        );
        let ok2 = self.b.create_block();
        self.b.ins().brif(small2, ok2, &[], bail_b, &[]);
        self.b.switch_to_block(ok2);
        self.b.ins().jump(join, &[iv.into()]);
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

    /// Emit the kernel body once into `cur_blocks` (already switched-away
    /// entry): every op into its block, back-edge trampolines flushed.
    fn emit_body(&mut self) -> Option<()> {
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
        Some(())
    }

    fn translate(
        mut self,
        frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
    ) -> Option<()> {
        // Recursive bodies: single body, blocks pre-wired by the member
        // constructor.
        if self.rec.is_some() {
            self.emit_body()?;
            self.b.seal_all_blocks();
            self.b.finalize(frontend_config);
            return Some(());
        }
        let dispatch = self.dispatch.expect("non-rec translators have one");
        let typing = self.typing.clone();
        let float_exit = self.exit_block;
        let float_intr = self.intr_block;
        let int_body = typing.as_ref().map(|t| {
            let blocks: Vec<Block> = (0..self.k.code.len())
                .map(|_| self.b.create_block())
                .collect();
            let exit = self.b.create_block();
            self.b.append_block_param(exit, types::I64);
            let intr = self.b.create_block();
            (t.clone(), blocks, exit, intr)
        });
        // Dispatch: the typing's runtime entry checks pick the body.
        self.b.switch_to_block(dispatch);
        match &int_body {
            None => {
                let first = self.cur_blocks[0];
                self.b.ins().jump(first, &[]);
            }
            Some((t, int_blocks, _, _)) => {
                self.emit_entry_dispatch(t.clone(), int_blocks[0]);
            }
        }
        // Float body — exactly the untyped emission.
        self.in_int_body = false;
        self.emit_body()?;
        self.emit_epilogue(float_exit, float_intr, false);
        // Int body.
        if let Some((_, int_blocks, int_exit, int_intr)) = int_body {
            self.cur_blocks = int_blocks;
            self.exit_block = int_exit;
            self.intr_block = int_intr;
            self.in_int_body = true;
            self.emit_body()?;
            self.emit_epilogue(int_exit, int_intr, true);
        }
        self.b.seal_all_blocks();
        self.b.finalize(frontend_config);
        Some(())
    }

    /// Fill the entry dispatch: run the typing's per-register checks on the
    /// raw entry-loaded f64s — integral (rejects NaN/±Inf), within the
    /// entry band, nonnegative (≥ 1 for a `Pos` check), and not `-0` — and
    /// on all-pass define every `Int` register's i64 variable (the checked
    /// registers from their saturating conversions, pure temporaries as 0)
    /// and enter the int body; any failure enters the float body, whose
    /// variables the entry already defined.
    fn emit_entry_dispatch(&mut self, t: Rc<crate::jit_ty::RegTyping>, int_first: Block) {
        use crate::jit_ty::{EntryCheck, ENTRY_HI};
        let fail = self.b.create_block();
        let mut sats: Vec<(u16, cranelift_codegen::ir::Value)> = Vec::new();
        for &(r, chk) in &t.checks {
            let v = self.b.use_var(self.vars[r as usize]);
            let sat = self.b.ins().fcvt_to_sint_sat(types::I64, v);
            let rt = self.b.ins().fcvt_from_sint(types::F64, sat);
            let integral = self.b.ins().fcmp(FloatCC::Equal, rt, v);
            // `-0` passes the round-trip (0.0 == -0.0); reject it by bits.
            let bits = self
                .b
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), v);
            let not_nz =
                self.b
                    .ins()
                    .icmp_imm_s(IntCC::NotEqual, bits, (-0.0f64).to_bits() as i64);
            let ok2 = match chk {
                EntryCheck::NonNeg | EntryCheck::Pos => {
                    let lo = if matches!(chk, EntryCheck::Pos) { 1 } else { 0 };
                    let lo_ok = self
                        .b
                        .ins()
                        .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, sat, lo);
                    let hi_ok = self
                        .b
                        .ins()
                        .icmp_imm_s(IntCC::SignedLessThanOrEqual, sat, ENTRY_HI as i64);
                    self.b.ins().band(lo_ok, hi_ok)
                }
                // A baked divisor: exactly the compiling activation's value.
                EntryCheck::Exact(v) => self.b.ins().icmp_imm_s(IntCC::Equal, sat, v),
            };
            let ok1 = self.b.ins().band(integral, not_nz);
            let ok = self.b.ins().band(ok1, ok2);
            let next = self.b.create_block();
            self.b.ins().brif(ok, next, &[], fail, &[]);
            self.b.switch_to_block(next);
            sats.push((r, sat));
        }
        for (r, sat) in sats {
            self.b
                .def_var(self.ivars[r as usize].expect("checked reg is Int"), sat);
        }
        for &r in &t.temps {
            let z = self.b.ins().iconst(types::I64, 0);
            self.b
                .def_var(self.ivars[r as usize].expect("temp reg is Int"), z);
        }
        self.b.ins().jump(int_first, &[]);
        self.b.switch_to_block(fail);
        let first = self.cur_blocks[0];
        self.b.ins().jump(first, &[]);
    }

    /// `dst = a + b` with int-native emission when the typing allows.
    fn emit_add_regs(&mut self, dst: u16, a: u16, b: u16) {
        if self.is_int(dst) && self.is_int(a) && self.is_int(b) {
            let (x, y) = (self.get_int(a), self.get_int(b));
            let v = self.b.ins().iadd(x, y);
            self.set_int(dst, v);
        } else {
            let (x, y) = (self.get(a), self.get(b));
            let v = self.b.ins().fadd(x, y);
            self.set(dst, v);
        }
    }

    /// `dst = a + k` with int-native emission when the typing allows.
    fn emit_addk(&mut self, dst: u16, a: u16, k: f64) {
        if self.is_int(dst) && self.is_int(a) {
            let x = self.get_int(a);
            let v = self.b.ins().iadd_imm_s(x, k as i64);
            self.set_int(dst, v);
        } else {
            let x = self.get(a);
            let kk = self.b.ins().f64const(k);
            let v = self.b.ins().fadd(x, kk);
            self.set(dst, v);
        }
    }

    /// `dst = a <kind> rhs` with int-native emission when the typing typed
    /// `dst` Int (the analysis mirror of what each kind admits); falls back
    /// to the float path (whose `set` converts exactly) otherwise.
    fn emit_arith_to(
        &mut self,
        kind: ArithKind,
        dst: u16,
        a: u16,
        rhs: Result<u16, f64>,
    ) -> Option<()> {
        if self.is_int(dst) {
            match kind {
                ArithKind::Sub | ArithKind::Mul | ArithKind::Mod => {
                    let rhs_int_ok = match rhs {
                        Ok(r) => self.is_int(r),
                        Err(k) => k.fract() == 0.0,
                    };
                    if self.is_int(a) && rhs_int_ok {
                        let x = self.get_int(a);
                        let y = match rhs {
                            Ok(r) => self.get_int(r),
                            Err(k) => self.b.ins().iconst(types::I64, k as i64),
                        };
                        let v = match kind {
                            ArithKind::Sub => self.b.ins().isub(x, y),
                            ArithKind::Mul => self.b.ins().imul(x, y),
                            // Divisor ≥ 1 and dividend ≥ 0 by the typing's
                            // range proof: no trap, no `-0` case.
                            _ => self.b.ins().srem(x, y),
                        };
                        self.set_int(dst, v);
                        return Some(());
                    }
                }
                ArithKind::BitAnd
                | ArithKind::BitOr
                | ArithKind::BitXor
                | ArithKind::Shl
                | ArithKind::Shr
                | ArithKind::UShr => {
                    let ia = self.operand_i32(a)?;
                    let ib = match rhs {
                        Ok(r) => self.operand_i32(r)?,
                        Err(k) => {
                            let ki = crate::vm::to_int32(k);
                            self.b.ins().iconst(types::I32, i64::from(ki))
                        }
                    };
                    let (narrow, unsigned) = match kind {
                        ArithKind::BitAnd => (self.b.ins().band(ia, ib), false),
                        ArithKind::BitOr => (self.b.ins().bor(ia, ib), false),
                        ArithKind::BitXor => (self.b.ins().bxor(ia, ib), false),
                        ArithKind::Shl => {
                            let cnt = self.b.ins().band_imm_u(ib, 31i64);
                            (self.b.ins().ishl(ia, cnt), false)
                        }
                        ArithKind::Shr => {
                            let cnt = self.b.ins().band_imm_u(ib, 31i64);
                            (self.b.ins().sshr(ia, cnt), false)
                        }
                        _ => {
                            let cnt = self.b.ins().band_imm_u(ib, 31i64);
                            (self.b.ins().ushr(ia, cnt), true)
                        }
                    };
                    let wide = if unsigned {
                        self.b.ins().uextend(types::I64, narrow)
                    } else {
                        self.b.ins().sextend(types::I64, narrow)
                    };
                    self.set_int(dst, wide);
                    return Some(());
                }
                ArithKind::Div | ArithKind::Pow => {}
            }
        }
        let x = self.get(a);
        let (y, bk) = match rhs {
            Ok(r) => (self.get(r), None),
            Err(k) => (self.b.ins().f64const(k), Some(k)),
        };
        let v = self.arith(kind, x, y, bk)?;
        self.set(dst, v);
        Some(())
    }

    /// Emit one op into the current block. Returns the fall-through skip
    /// (1, or 2 past a fused op's landing pad), `Some(None)` when the op
    /// placed its own terminator, or `None` to decline the whole
    /// translation. Shared between the top-level kernel and inlined callee
    /// kernels (`emit_call_kernel` swaps `cur_blocks`/`reg_base`/
    /// `inline_ret` around it), and between the FLOAT and INT bodies
    /// (`in_int_body` steers the typed accessors and the int-native arms).
    fn emit_op(&mut self, op: KOp, pc: usize) -> Option<Option<usize>> {
        let mut fallthrough = Some(1usize);
        match op {
            KOp::Mov { dst, src } => {
                if self.is_int(dst) && self.is_int(src) {
                    let v = self.get_int(src);
                    self.set_int(dst, v);
                } else {
                    let v = self.get(src);
                    self.set(dst, v);
                }
            }
            KOp::Const { dst, k } => {
                if self.is_int(dst) {
                    let v = self.b.ins().iconst(types::I64, k as i64);
                    self.set_int(dst, v);
                } else {
                    let v = self.b.ins().f64const(k);
                    self.set(dst, v);
                }
            }
            KOp::Add { dst, a, b } => {
                self.emit_add_regs(dst, a, b);
            }
            KOp::AddK { dst, a, k } => {
                self.emit_addk(dst, a, k);
            }
            KOp::Arith { kind, dst, a, b } => {
                self.emit_arith_to(kind, dst, a, Ok(b))?;
            }
            KOp::ArithK { kind, dst, a, k } => {
                self.emit_arith_to(kind, dst, a, Err(k))?;
            }
            KOp::Neg { dst, src } => {
                let x = self.get(src);
                let v = self.b.ins().fneg(x);
                self.set(dst, v);
            }
            KOp::BitNot { dst, src } => {
                let ix = self.operand_i32(src)?;
                let nx = self.b.ins().bnot(ix);
                if self.is_int(dst) {
                    let wide = self.b.ins().sextend(types::I64, nx);
                    self.set_int(dst, wide);
                } else {
                    let v = self.i32_to_f64(nx);
                    self.set(dst, v);
                }
            }
            KOp::Mov2 { d1, s1, d2, s2 } => {
                if self.is_int(d1) && self.is_int(s1) {
                    let v = self.get_int(s1);
                    self.set_int(d1, v);
                } else {
                    let v1 = self.get(s1);
                    self.set(d1, v1);
                }
                if self.is_int(d2) && self.is_int(s2) {
                    let v = self.get_int(s2);
                    self.set_int(d2, v);
                } else {
                    let v2 = self.get(s2);
                    self.set(d2, v2);
                }
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
                self.emit_arith_to(kind, dst, a, Ok(b))?;
                self.emit_add_regs(d2, a2, b2);
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
                self.emit_arith_to(kind, dst, a, Err(k))?;
                self.emit_add_regs(d2, a2, b2);
                fallthrough = Some(2);
            }
            KOp::AddKBr { dst, a, k, target } => {
                self.emit_addk(dst, a, k);
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
                let c = self.cmp_regs(cmp, a, b);
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
                let c = self.cmp_reg_const(cmp, a, k);
                self.branch_on(c, if_true, pc, target as usize);
                fallthrough = None;
            }
            KOp::BrFalsy { src, target } => {
                let c = self.truthy_reg(src);
                self.branch_on(c, false, pc, target as usize);
                fallthrough = None;
            }
            KOp::BrTruthy { src, target } => {
                let c = self.truthy_reg(src);
                self.branch_on(c, true, pc, target as usize);
                fallthrough = None;
            }
            KOp::CmpSet { cmp, dst, a, b } => {
                let c = self.cmp_regs(cmp, a, b);
                if self.is_int(dst) {
                    let one = self.b.ins().iconst(types::I64, 1);
                    let zero = self.b.ins().iconst(types::I64, 0);
                    let v = self.b.ins().select(c, one, zero);
                    self.set_int(dst, v);
                } else {
                    let one = self.b.ins().f64const(1.0);
                    let zero = self.b.ins().f64const(0.0);
                    let v = self.b.ins().select(c, one, zero);
                    self.set(dst, v);
                }
            }
            KOp::BoolNot { dst, src } => {
                let c = self.truthy_reg(src);
                if self.is_int(dst) {
                    let one = self.b.ins().iconst(types::I64, 1);
                    let zero = self.b.ins().iconst(types::I64, 0);
                    let v = self.b.ins().select(c, zero, one);
                    self.set_int(dst, v);
                } else {
                    let one = self.b.ins().f64const(1.0);
                    let zero = self.b.ins().f64const(0.0);
                    let v = self.b.ins().select(c, zero, one);
                    self.set(dst, v);
                }
            }
            KOp::Math1 { kind, dst, src } => {
                // Int identity kinds (the typing admits exactly these).
                if self.is_int(dst)
                    && self.is_int(src)
                    && matches!(
                        kind,
                        KMath::Abs | KMath::Floor | KMath::Ceil | KMath::Trunc | KMath::Round
                    )
                {
                    let x = self.get_int(src);
                    let v = if matches!(kind, KMath::Abs) {
                        self.b.ins().iabs(x)
                    } else {
                        x
                    };
                    self.set_int(dst, v);
                } else {
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
            }
            KOp::Math2 { kind, dst, a, b } => {
                if self.is_int(dst)
                    && self.is_int(a)
                    && self.is_int(b)
                    && matches!(kind, KMath::Min2 | KMath::Max2)
                {
                    let (x, y) = (self.get_int(a), self.get_int(b));
                    let v = if matches!(kind, KMath::Min2) {
                        self.b.ins().smin(x, y)
                    } else {
                        self.b.ins().smax(x, y)
                    };
                    self.set_int(dst, v);
                } else if self.is_int(dst) && matches!(kind, KMath::Imul2) {
                    let (ix, iy) = (self.operand_i32(a)?, self.operand_i32(b)?);
                    let r = self.b.ins().imul(ix, iy);
                    let wide = self.b.ins().sextend(types::I64, r);
                    self.set_int(dst, wide);
                } else {
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
                if self.is_int(dst) {
                    let v = self.emit_elem_load_int(pc, obj, idx, bail)?;
                    self.set_int(dst, v);
                } else {
                    let v = self.emit_elem_load(pc, obj, idx, bail)?;
                    self.set(dst, v);
                }
            }
            KOp::StoreElem {
                obj,
                idx,
                val,
                bail,
            } => {
                self.emit_elem_store(pc, obj, idx, val, bail)?;
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
                if self.is_int(dst) {
                    let v = self.emit_elem_load_int(pc, obj, idx, bail)?;
                    self.set_int(dst, v);
                } else {
                    let v = self.emit_elem_load(pc, obj, idx, bail)?;
                    self.set(dst, v);
                }
                self.emit_add_regs(d2, a2, b2);
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
                if self.is_int(dst) {
                    let v = self.emit_elem_load_int(pc, obj, idx, bail)?;
                    self.set_int(dst, v);
                } else {
                    let v = self.emit_elem_load(pc, obj, idx, bail)?;
                    self.set(dst, v);
                }
                self.emit_arith_to(kind, d2, a2, Ok(b2))?;
                fallthrough = Some(2);
            }
            KOp::LoadLen { dst, obj, bail } => {
                if self.is_int(dst) {
                    let v = self.emit_len_int(pc, obj, bail)?;
                    self.set_int(dst, v);
                } else {
                    let v = self.emit_len(pc, obj, bail)?;
                    self.set(dst, v);
                }
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
                if self.is_int(dst) {
                    let v = self.emit_len_int(pc, obj, bail)?;
                    self.set_int(dst, v);
                } else {
                    let v = self.emit_len(pc, obj, bail)?;
                    self.set(dst, v);
                }
                let c = self.cmp_regs(cmp, a, b);
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
                if self.is_int(dst) {
                    self.set_int(dst, slen);
                } else {
                    let v = self.b.ins().fcvt_from_sint(types::F64, slen);
                    self.set(dst, v);
                }
            }
            KOp::CharCodeAt {
                dst,
                str,
                idx,
                bail,
            } => {
                let (sptr, slen) = self.sviews[str as usize];
                if self.is_int(dst) && self.is_int(idx) {
                    // INT body: an out-of-range index would need the NaN an
                    // i64 register cannot hold — bail instead (the generic
                    // rerun computes it); in-range is one unsigned compare
                    // and a byte load (ASCII: code == byte).
                    let bail_b = self.dest(pc, bail as usize);
                    let ii = self.get_int(idx);
                    let ok = self.b.ins().icmp(IntCC::UnsignedLessThan, ii, slen);
                    let load_b = self.b.create_block();
                    self.b.ins().brif(ok, load_b, &[], bail_b, &[]);
                    self.b.switch_to_block(load_b);
                    let addr = self.b.ins().iadd(sptr, ii);
                    let byte = self.b.ins().load(types::I8, MemFlagsData::new(), addr, 0);
                    let wide = self.b.ins().uextend(types::I64, byte);
                    self.set_int(dst, wide);
                    return Some(fallthrough);
                }
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
            // A family call (recursive bodies only; see `compile_family`):
            // the interpreter's per-call depth guard and shared poll, then a
            // real native call to the member the callee id resolved to, then
            // the abandon-unwind check.
            KOp::SelfCall {
                dst,
                base,
                argc: _,
                callee,
            } => {
                let rec = self.rec.clone()?;
                self.emit_self_call(&rec, dst, base, callee)?;
            }
            // Excluded by `eligible`.
            KOp::LoadProp { .. } | KOp::StoreProp { .. } => return None,
        }
        Some(fallthrough)
    }

    /// One family call site (recursive bodies; see the `SelfCall` arm):
    /// depth guard → shared poll → native call to the resolved member →
    /// abandon check.
    fn emit_self_call(&mut self, rec: &RecMode, dst: u16, base: u16, callee: u16) -> Option<()> {
        // Depth: the interpreter abandons when the NEXT frame would exceed
        // the budget — i.e. when this frame's remaining budget is < 1.
        let depth_ok = self.b.create_block();
        let out_of_depth = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, rec.depth, 1);
        self.b
            .ins()
            .brif(out_of_depth, rec.abandon_depth, &[], depth_ok, &[]);
        self.b.switch_to_block(depth_ok);
        // Shared per-CALL poll counter (ctx-resident so the every-256-calls
        // cadence spans frames, like the interpreter's activation-wide
        // counter — a depth-derived poll would never fire on a shallow-but-
        // hot recursion). Same cache line as the rest of the ctx; the
        // store-to-load chain is cheap next to the call itself.
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
        // Resolve the callee id through this member's row and pass the
        // TARGET's consumed arguments from the call site's contiguous
        // registers, one less depth. (The resolution cross-check proved
        // every target argument index < the site's argc.)
        let t = *rec.cmap.get(callee as usize)? as usize;
        let target_args = rec.callee_args.get(t)?;
        let mut call_args = Vec::with_capacity(target_args.len() + 3);
        for &a in target_args.iter() {
            call_args.push(self.get(base + a));
        }
        let next_depth = self.b.ins().iadd_imm_s(rec.depth, -1i64);
        call_args.push(next_depth);
        call_args.push(self.int_ptr);
        call_args.push(self.ctx_ptr);
        let target_fn = *rec.member_fns.get(t)?;
        let call = self.b.ins().call(target_fn, &call_args);
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

    /// One body's exit path: the interrupt landing routes to the epilogue
    /// with the sentinel; the epilogue stores every register back (an int
    /// body's `Int` registers convert from their i64 variables — exact,
    /// every value bounded far below 2^53) and returns the exit code.
    fn emit_epilogue(&mut self, exit_block: Block, intr_block: Block, int_body: bool) {
        self.b.switch_to_block(intr_block);
        let sentinel = self.b.ins().iconst(types::I64, INTERRUPTED);
        self.b.ins().jump(exit_block, &[sentinel.into()]);
        self.b.switch_to_block(exit_block);
        let code_v = self.b.block_params(exit_block)[0];
        for r in 0..self.vars.len() {
            let int_var = if int_body {
                self.ivars.get(r).copied().flatten()
            } else {
                None
            };
            let v = match int_var {
                Some(iv) => {
                    let x = self.b.use_var(iv);
                    self.b.ins().fcvt_from_sint(types::F64, x)
                }
                None => self.b.use_var(self.vars[r]),
            };
            self.b
                .ins()
                .store(MemFlagsData::trusted(), v, self.regs_ptr, (8 * r) as i32);
        }
        self.b.ins().return_(&[code_v]);
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
