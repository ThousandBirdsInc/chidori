//! INT-TYPING analysis for the Cranelift kernel tier (`jit` feature).
//!
//! Decides, per kernel register, whether the compiled code may carry the
//! register as a native **i64** instead of an f64 — the induction-variable /
//! integer-accumulator reasoning `docs/cranelift-jit.md` names as the next
//! step for the `%`-heavy and element-indexing rows. The contract is strict
//! bit-exactness: a register is `Int` only when EVERY value it can hold at
//! runtime is a mathematical integer that f64 arithmetic would compute
//! exactly the same way — so the analysis tracks value RANGES and admits an
//! operation only where integer and IEEE-double semantics provably coincide
//! (sums bounded well below 2^53, products of proven-nonnegative operands,
//! `%` with a nonnegative dividend and positive divisor, the ToInt32
//! bitwise family, comparisons — and nothing that could produce `-0`, NaN,
//! or a fraction).
//!
//! Registers whose ENTRY value comes from the guarded activation state
//! (mapped locals / bools / prop slots — or consumed args and upvalue
//! snapshots for function kernels) get a RUNTIME entry check in the
//! compiled code: integral, within the entry bound, nonnegative (`Pos`
//! additionally requires ≥ 1 — demanded for `%` divisors), and not `-0`.
//! An activation whose live values fail any check runs the compiled FLOAT
//! body — exactly today's code — so nothing regresses; pure stack
//! temporaries (entry value never observable — exit shapes only reference
//! written operands) are typed without checks and start at 0.
//!
//! Loop counters (`i++` bounded by an `i < len` header) are the one place
//! flow-insensitive ranges diverge, so a small forward must-dataflow
//! collects compare-established upper-bound FACTS (`r < B` on the edge
//! where the compare held) and the self-increment rule consumes the fact
//! live at its program point. Only true-polarity edges provide facts (a
//! false edge proves nothing under NaN); a fact survives register `B`'s
//! redefinition because it is consumed against `B`'s WHOLE-kernel range.

use crate::bytecode::{CmpOp, KMath, KOp, KSlot, Kernel};
use crate::exec::ArithKind;
use crate::jit::ElemView;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RegTy {
    Int,
    Float,
}

/// Runtime entry-check kind for a mapped Int register.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EntryCheck {
    /// integral, `0 ≤ v ≤ ENTRY_HI`, not `-0`.
    NonNeg,
    /// integral, `1 ≤ v ≤ ENTRY_HI` (a `%` divisor).
    Pos,
    /// integral and EXACTLY this value — a read-only `%` divisor baked from
    /// the compiling activation (a `const MOD = …` binding in practice).
    /// Every read of the register emits the CONSTANT, so Cranelift's
    /// divide-by-constant strength reduction turns the `srem` into the
    /// multiply sequence V8 uses; an activation entering a different value
    /// runs the float body.
    Exact(i64),
}

pub(crate) struct RegTyping {
    /// Per window-0 register.
    pub ty: Vec<RegTy>,
    /// Mapped Int registers with their runtime entry checks.
    pub checks: Vec<(u16, EntryCheck)>,
    /// Scratch Int registers (entry value never observable): initialized to
    /// 0 at entry, no check.
    pub temps: Vec<u16>,
}

/// Entry-check magnitude bound: generous enough for real accumulators,
/// small enough that sums and modest products stay far below 2^53.
pub(crate) const ENTRY_HI: f64 = 1_099_511_627_776.0; // 2^40
/// Ranges must stay within ±this for int and f64 arithmetic to coincide.
const MAX_SAFE: f64 = 4_503_599_627_370_496.0; // 2^52
/// `.length` results are typed with this bound; the emitted direct/helper
/// arms bail on anything larger (see the LoadLen emission).
pub(crate) const LEN_HI: f64 = 281_474_976_710_656.0; // 2^48
/// Guard-fact bounds (`i < B`) must be at most this — covers the length
/// registers (`LEN_HI`) that bound real loop counters, while keeping the
/// guarded increment far below the exactness band.
const GUARD_B_MAX: f64 = LEN_HI;

const I32_LO: f64 = -2_147_483_648.0;
const I32_HI: f64 = 2_147_483_647.0;
const U32_HI: f64 = 4_294_967_295.0;

#[derive(Clone, Copy, PartialEq)]
struct Range {
    lo: f64,
    hi: f64,
}

impl Range {
    const BOTTOM: Range = Range {
        lo: f64::INFINITY,
        hi: f64::NEG_INFINITY,
    };
    fn new(lo: f64, hi: f64) -> Range {
        Range { lo, hi }
    }
    fn join(self, o: Range) -> Range {
        Range {
            lo: self.lo.min(o.lo),
            hi: self.hi.max(o.hi),
        }
    }
    fn is_bottom(self) -> bool {
        self.lo > self.hi
    }
    fn within_safe(self) -> bool {
        !self.is_bottom() && self.lo >= -MAX_SAFE && self.hi <= MAX_SAFE
    }
}

/// A compare-established upper-bound fact: `reg < bound` (strict) or
/// `reg ≤ bound`.
#[derive(Clone, Copy)]
struct Fact {
    reg: u16,
    bound: Bound,
    strict: bool,
}

#[derive(Clone, Copy)]
enum Bound {
    Const(f64),
    Reg(u16),
}

/// Whether `k` is a plain-integer constant the Int domain can hold
/// (rejecting `-0.0`, which is not an integer value the domain represents).
fn int_const(k: f64) -> bool {
    k.fract() == 0.0 && k.abs() <= MAX_SAFE && !(k == 0.0 && k.is_sign_negative())
}

/// The element-value range of a baked typed-array view kind, when that
/// kind always decodes to integers.
fn elem_kind_range(code: u64) -> Option<Range> {
    Some(match code {
        ElemView::TA_I8 => Range::new(-128.0, 127.0),
        ElemView::TA_U8 | ElemView::TA_U8C => Range::new(0.0, 255.0),
        ElemView::TA_I16 => Range::new(-32768.0, 32767.0),
        ElemView::TA_U16 => Range::new(0.0, 65535.0),
        ElemView::TA_I32 => Range::new(I32_LO, I32_HI),
        ElemView::TA_U32 => Range::new(0.0, U32_HI),
        _ => return None,
    })
}

/// CFG successors of the op at `pc`, with compare polarity carried on the
/// edge (`Some(true)` = the compare held on this edge). Mirrors the
/// translator's control flow exactly, bail edges included.
fn successors(op: &KOp, pc: usize) -> Vec<(usize, Option<bool>)> {
    let p1 = pc + 1;
    let p2 = pc + 2;
    match *op {
        KOp::Mov2 { .. } | KOp::ArithAdd { .. } | KOp::ArithKAdd { .. } => vec![(p2, None)],
        KOp::AddKBr { target, .. } | KOp::Br { target } => vec![(target as usize, None)],
        KOp::BrCmp {
            if_true, target, ..
        }
        | KOp::BrCmpK {
            if_true, target, ..
        } => vec![(target as usize, Some(if_true)), (p1, Some(!if_true))],
        KOp::BrFalsy { target, .. } | KOp::BrTruthy { target, .. } => {
            vec![(target as usize, None), (p1, None)]
        }
        KOp::LenBrCmp {
            bail,
            if_true,
            target,
            ..
        } => vec![
            (target as usize, Some(if_true)),
            (p2, Some(!if_true)),
            (bail as usize, None),
        ],
        KOp::LoadElem { bail, .. }
        | KOp::StoreElem { bail, .. }
        | KOp::ArrayPush { bail, .. }
        | KOp::ArrayPop { bail, .. }
        | KOp::LoadLen { bail, .. }
        | KOp::CharCodeAt { bail, .. } => vec![(p1, None), (bail as usize, None)],
        KOp::LoadElemAdd { bail, .. } | KOp::LoadElemArith { bail, .. } => {
            vec![(p2, None), (bail as usize, None)]
        }
        KOp::Exit { .. } | KOp::Ret { .. } => vec![],
        _ => vec![(p1, None)],
    }
}

/// Window-0 registers the op WRITES.
fn defs_of(op: &KOp) -> Vec<u16> {
    match *op {
        KOp::Mov { dst, .. }
        | KOp::Const { dst, .. }
        | KOp::Add { dst, .. }
        | KOp::AddK { dst, .. }
        | KOp::Arith { dst, .. }
        | KOp::ArithK { dst, .. }
        | KOp::Neg { dst, .. }
        | KOp::BitNot { dst, .. }
        | KOp::CmpSet { dst, .. }
        | KOp::BoolNot { dst, .. }
        | KOp::Math1 { dst, .. }
        | KOp::Math2 { dst, .. }
        | KOp::StrLen { dst, .. }
        | KOp::CharCodeAt { dst, .. }
        | KOp::CallKernel { dst, .. }
        | KOp::SelfCall { dst, .. }
        | KOp::AddKBr { dst, .. }
        | KOp::LoadElem { dst, .. }
        | KOp::LoadLen { dst, .. }
        | KOp::LenBrCmp { dst, .. }
        | KOp::ArrayPush { dst, .. }
        | KOp::ArrayPop { dst, .. } => vec![dst],
        KOp::Mov2 { d1, d2, .. } => vec![d1, d2],
        KOp::ArithAdd { dst, d2, .. }
        | KOp::ArithKAdd { dst, d2, .. }
        | KOp::LoadElemAdd { dst, d2, .. }
        | KOp::LoadElemArith { dst, d2, .. } => vec![dst, d2],
        _ => vec![],
    }
}

/// Window-0 registers the op READS.
fn uses_of(op: &KOp) -> Vec<u16> {
    match *op {
        KOp::Mov { src, .. }
        | KOp::Neg { src, .. }
        | KOp::BitNot { src, .. }
        | KOp::BoolNot { src, .. }
        | KOp::Math1 { src, .. }
        | KOp::BrFalsy { src, .. }
        | KOp::BrTruthy { src, .. }
        | KOp::Ret { src, .. } => vec![src],
        KOp::Add { a, b, .. }
        | KOp::Arith { a, b, .. }
        | KOp::BrCmp { a, b, .. }
        | KOp::CmpSet { a, b, .. }
        | KOp::Math2 { a, b, .. }
        | KOp::LenBrCmp { a, b, .. } => vec![a, b],
        KOp::AddK { a, .. }
        | KOp::ArithK { a, .. }
        | KOp::AddKBr { a, .. }
        | KOp::BrCmpK { a, .. } => vec![a],
        KOp::Mov2 { s1, s2, .. } => vec![s1, s2],
        KOp::ArithAdd { a, b, a2, b2, .. } => vec![a, b, a2, b2],
        KOp::ArithKAdd { a, a2, b2, .. } => vec![a, a2, b2],
        KOp::LoadElem { idx, .. } | KOp::CharCodeAt { idx, .. } => vec![idx],
        KOp::StoreElem { idx, val, .. } => vec![idx, val],
        KOp::LoadElemAdd { idx, a2, b2, .. } => vec![idx, a2, b2],
        KOp::LoadElemArith { idx, a2, b2, .. } => vec![idx, a2, b2],
        KOp::ArrayPush { val, .. } => vec![val],
        KOp::CallKernel { base, argc, .. } | KOp::SelfCall { base, argc, .. } => {
            (base..base + argc).collect()
        }
        _ => vec![],
    }
}

/// Whether the entry value of register `r` is MEANINGFUL (populated by the
/// activation guard) — such registers need the runtime entry check when
/// int-typed; the rest are pure scratch whose entry value is never
/// observable.
fn entry_meaningful(k: &Kernel, r: usize, fn_mode: bool) -> bool {
    let nl = k.locals.len();
    let nb = k.bool_locals.len();
    if r < nl {
        if fn_mode {
            matches!(k.locals[r], KSlot::Arg(_) | KSlot::Upvalue(_))
        } else {
            true
        }
    } else if r < nl + nb {
        true
    } else {
        let prop_base = k.n_regs as usize - k.props_used.len();
        !fn_mode && r >= prop_base
    }
}

/// The upper-bound fact (if any) a TRUE-polarity edge of this compare
/// establishes.
fn fact_of(cmp: CmpOp, a: Bound, b: Bound) -> Option<Fact> {
    let (reg, bound, strict) = match (cmp, a, b) {
        (CmpOp::Lt, Bound::Reg(r), bd) => (r, bd, true),
        (CmpOp::Le, Bound::Reg(r), bd) => (r, bd, false),
        (CmpOp::Gt, bd, Bound::Reg(r)) => (r, bd, true),
        (CmpOp::Ge, bd, Bound::Reg(r)) => (r, bd, false),
        _ => return None,
    };
    if let Bound::Const(c) = bound {
        if !(c.fract() == 0.0 && c.abs() <= GUARD_B_MAX) {
            return None;
        }
    }
    Some(Fact { reg, bound, strict })
}

/// The add-family range: both operands int with in-bounds sum. A BOTTOM
/// operand (no value has flowed there yet) yields BOTTOM — "not yet",
/// never a demotion.
fn add_range(ai: bool, ra: Range, bi: bool, rb: Range) -> Option<Range> {
    if !ai || !bi {
        return None;
    }
    if ra.is_bottom() || rb.is_bottom() {
        return Some(Range::BOTTOM);
    }
    Some(Range::new(ra.lo + rb.lo, ra.hi + rb.hi))
}

/// The `Arith` def rule. `b_reg` is Some for a register rhs (`%`-divisor
/// demand reporting).
fn arith_range(
    kind: ArithKind,
    ai: bool,
    ra: Range,
    bi: bool,
    rb: Range,
    b_reg: Option<u16>,
    demand_pos: &mut Option<u16>,
) -> Option<Range> {
    match kind {
        ArithKind::Sub => {
            if !ai || !bi {
                return None;
            }
            if ra.is_bottom() || rb.is_bottom() {
                return Some(Range::BOTTOM);
            }
            Some(Range::new(ra.lo - rb.hi, ra.hi - rb.lo))
        }
        ArithKind::Mul => {
            if !ai || !bi {
                return None;
            }
            if ra.is_bottom() || rb.is_bottom() {
                return Some(Range::BOTTOM);
            }
            if ra.lo < 0.0 || rb.lo < 0.0 {
                return None;
            }
            Some(Range::new(ra.lo * rb.lo, ra.hi * rb.hi))
        }
        ArithKind::Mod => {
            if !ai || !bi {
                return None;
            }
            if ra.is_bottom() || rb.is_bottom() {
                return Some(Range::BOTTOM);
            }
            if ra.lo < 0.0 {
                return None;
            }
            if rb.lo < 1.0 {
                *demand_pos = b_reg;
                return None;
            }
            Some(Range::new(0.0, rb.hi - 1.0))
        }
        ArithKind::Div | ArithKind::Pow => None,
        // The ToInt32 family always produces int32-valued results whatever
        // the operands hold (the emission converts float operands through
        // the shared ToInt32 path).
        ArithKind::BitAnd
        | ArithKind::BitOr
        | ArithKind::BitXor
        | ArithKind::Shl
        | ArithKind::Shr => Some(Range::new(I32_LO, I32_HI)),
        ArithKind::UShr => Some(Range::new(0.0, U32_HI)),
    }
}

/// State the per-op evaluation reads.
struct Ctx<'a> {
    ty: &'a [RegTy],
    range: &'a [Range],
    facts: &'a [Fact],
    fin: &'a [u64],
    elem_kinds: &'a [u64],
}

impl Ctx<'_> {
    fn int(&self, r: u16) -> bool {
        self.ty[r as usize] == RegTy::Int
    }
    fn rng(&self, r: u16) -> Range {
        self.range[r as usize]
    }
    fn arg(&self, r: u16) -> (bool, Range) {
        (self.int(r), self.rng(r))
    }
    fn karg(&self, c: f64) -> (bool, Range) {
        if int_const(c) {
            (true, Range::new(c, c))
        } else {
            (false, Range::BOTTOM)
        }
    }
    /// The guarded self-increment rule (`r = r + k` under a live `r < B`).
    fn guarded_inc(&self, dst: u16, a: u16, kc: f64, pc: usize) -> Option<Range> {
        if dst != a || !int_const(kc) || kc <= 0.0 || kc > 4096.0 {
            return None;
        }
        let live = self.fin[pc];
        for (i, f) in self.facts.iter().enumerate() {
            if live & (1u64 << i) == 0 || f.reg != dst {
                continue;
            }
            let bhi = match f.bound {
                Bound::Const(c) => c,
                Bound::Reg(b) => {
                    if !self.int(b) {
                        continue;
                    }
                    let r = self.rng(b);
                    if r.is_bottom() || r.hi > GUARD_B_MAX {
                        continue;
                    }
                    r.hi
                }
            };
            let cap = if f.strict { bhi - 1.0 } else { bhi };
            let lo = self.rng(dst).lo.min(0.0);
            return Some(Range::new(lo + kc, cap + kc));
        }
        None
    }
}

/// Evaluate one op's defs under `cx`: `(dst, None)` = the def is not
/// int-capable (dst must demote); `(dst, Some(range))` = an int def with
/// that result range.
fn eval_op(op: &KOp, pc: usize, cx: &Ctx, demand_pos: &mut Option<u16>) -> Vec<(u16, Option<Range>)> {
    match *op {
        KOp::Mov { dst, src } => {
            vec![(dst, if cx.int(src) { Some(cx.rng(src)) } else { None })]
        }
        KOp::Mov2 { d1, s1, d2, s2 } => vec![
            (d1, if cx.int(s1) { Some(cx.rng(s1)) } else { None }),
            (d2, if cx.int(s2) { Some(cx.rng(s2)) } else { None }),
        ],
        KOp::Const { dst, k } => {
            vec![(dst, if int_const(k) { Some(Range::new(k, k)) } else { None })]
        }
        KOp::Add { dst, a, b } => {
            let (ai, ra) = cx.arg(a);
            let (bi, rb) = cx.arg(b);
            vec![(dst, add_range(ai, ra, bi, rb))]
        }
        KOp::AddK { dst, a, k } | KOp::AddKBr { dst, a, k, .. } => {
            let r = if !cx.int(a) || !int_const(k) {
                None
            } else if let Some(g) = cx.guarded_inc(dst, a, k, pc) {
                Some(g)
            } else {
                let (ai, ra) = cx.arg(a);
                let (bi, rb) = cx.karg(k);
                add_range(ai, ra, bi, rb)
            };
            vec![(dst, r)]
        }
        KOp::Arith { kind, dst, a, b } => {
            let (ai, ra) = cx.arg(a);
            let (bi, rb) = cx.arg(b);
            vec![(dst, arith_range(kind, ai, ra, bi, rb, Some(b), demand_pos))]
        }
        KOp::ArithK { kind, dst, a, k } => {
            let (ai, ra) = cx.arg(a);
            let (bi, rb) = cx.karg(k);
            vec![(dst, arith_range(kind, ai, ra, bi, rb, None, demand_pos))]
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
            let (ai, ra) = cx.arg(a);
            let (bi, rb) = cx.arg(b);
            let r1 = arith_range(kind, ai, ra, bi, rb, Some(b), demand_pos);
            let (ai2, ra2) = cx.arg(a2);
            let (bi2, rb2) = cx.arg(b2);
            vec![(dst, r1), (d2, add_range(ai2, ra2, bi2, rb2))]
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
            let (ai, ra) = cx.arg(a);
            let (bi, rb) = cx.karg(k);
            let r1 = arith_range(kind, ai, ra, bi, rb, None, demand_pos);
            let (ai2, ra2) = cx.arg(a2);
            let (bi2, rb2) = cx.arg(b2);
            vec![(dst, r1), (d2, add_range(ai2, ra2, bi2, rb2))]
        }
        KOp::Neg { dst, .. } => vec![(dst, None)],
        KOp::BitNot { dst, .. } => vec![(dst, Some(Range::new(I32_LO, I32_HI)))],
        KOp::CmpSet { dst, .. } | KOp::BoolNot { dst, .. } => {
            vec![(dst, Some(Range::new(0.0, 1.0)))]
        }
        KOp::Math1 { kind, dst, src } => {
            let r = if !cx.int(src) {
                None
            } else if cx.rng(src).is_bottom() {
                match kind {
                    KMath::Abs | KMath::Floor | KMath::Ceil | KMath::Trunc | KMath::Round => {
                        Some(Range::BOTTOM)
                    }
                    _ => None,
                }
            } else {
                let rs = cx.rng(src);
                match kind {
                    KMath::Abs => Some(Range::new(0.0, rs.lo.abs().max(rs.hi.abs()))),
                    KMath::Floor | KMath::Ceil | KMath::Trunc | KMath::Round => Some(rs),
                    _ => None,
                }
            };
            vec![(dst, r)]
        }
        KOp::Math2 { kind, dst, a, b } => {
            let r = match kind {
                KMath::Min2 | KMath::Max2 => {
                    if !cx.int(a) || !cx.int(b) {
                        None
                    } else if cx.rng(a).is_bottom() || cx.rng(b).is_bottom() {
                        Some(Range::BOTTOM)
                    } else {
                        let (ra, rb) = (cx.rng(a), cx.rng(b));
                        if matches!(kind, KMath::Min2) {
                            Some(Range::new(ra.lo.min(rb.lo), ra.hi.min(rb.hi)))
                        } else {
                            Some(Range::new(ra.lo.max(rb.lo), ra.hi.max(rb.hi)))
                        }
                    }
                }
                KMath::Imul2 => Some(Range::new(I32_LO, I32_HI)),
                _ => None,
            };
            vec![(dst, r)]
        }
        KOp::LoadElem { dst, obj, .. } => {
            let code = cx.elem_kinds.get(obj as usize).copied().unwrap_or(0);
            vec![(dst, elem_kind_range(code))]
        }
        KOp::LoadElemAdd {
            dst, obj, d2, a2, b2, ..
        } => {
            let code = cx.elem_kinds.get(obj as usize).copied().unwrap_or(0);
            let (ai2, ra2) = cx.arg(a2);
            let (bi2, rb2) = cx.arg(b2);
            vec![(dst, elem_kind_range(code)), (d2, add_range(ai2, ra2, bi2, rb2))]
        }
        KOp::LoadElemArith {
            dst,
            obj,
            kind,
            d2,
            a2,
            b2,
            ..
        } => {
            let code = cx.elem_kinds.get(obj as usize).copied().unwrap_or(0);
            let (ai2, ra2) = cx.arg(a2);
            let (bi2, rb2) = cx.arg(b2);
            vec![
                (dst, elem_kind_range(code)),
                (d2, arith_range(kind, ai2, ra2, bi2, rb2, Some(b2), demand_pos)),
            ]
        }
        KOp::LoadLen { dst, .. } | KOp::LenBrCmp { dst, .. } => {
            vec![(dst, Some(Range::new(0.0, LEN_HI)))]
        }
        // Pinned strings are guard-validated flat ASCII: an in-range code
        // unit is an integer in [0, 127], and the INT body bails on the
        // out-of-range NaN case. Typed only over an Int index (the loop-
        // counter hash shape) — a float index keeps the total float path.
        KOp::CharCodeAt { dst, idx, .. } => {
            let r = if cx.int(idx) {
                Some(Range::new(0.0, 127.0))
            } else {
                None
            };
            vec![(dst, r)]
        }
        // Pinned strings are entry-capped below 2^48 code units (the sslot
        // guard), so the length is an in-band integer.
        KOp::StrLen { dst, .. } => vec![(dst, Some(Range::new(0.0, LEN_HI)))],
        KOp::CallKernel { dst, .. }
        | KOp::SelfCall { dst, .. }
        | KOp::ArrayPush { dst, .. }
        | KOp::ArrayPop { dst, .. } => vec![(dst, None)],
        _ => vec![],
    }
}

/// Analyze `k`'s window-0 registers against the compiling activation's
/// baked element kinds and its live entry register values (`entry_regs`,
/// used only to bake read-only `%` divisors — empty when the caller has no
/// meaningful values at hand). `None` = nothing worth int-typing; the
/// caller emits the single float body exactly as before.
pub(crate) fn analyze(k: &Kernel, elem_kinds: &[u64], entry_regs: &[f64]) -> Option<RegTyping> {
    let n_regs = k.n_regs as usize;
    let code: &[KOp] = &k.code;
    if n_regs == 0 || code.is_empty() || code.len() > 512 {
        return None;
    }
    // Recursive bodies use a different emission dialect; never typed here.
    if code.iter().any(|op| matches!(op, KOp::SelfCall { .. })) {
        return None;
    }
    let fn_mode = code.iter().any(|op| matches!(op, KOp::Ret { .. }));

    // ---- static shape ----------------------------------------------------
    let mut written = vec![false; n_regs];
    let mut used = vec![false; n_regs];
    for (pc, op) in code.iter().enumerate() {
        for d in defs_of(op) {
            *written.get_mut(d as usize)? = true;
        }
        for u in uses_of(op) {
            *used.get_mut(u as usize)? = true;
        }
        for (succ, _) in successors(op, pc) {
            if succ >= code.len() {
                return None;
            }
        }
    }

    // ---- guard facts + forward must-dataflow ----------------------------
    let mut facts: Vec<Fact> = Vec::new();
    // Per pc: (succ, fact index) edges.
    let mut edges: Vec<Vec<(usize, Option<usize>)>> = Vec::with_capacity(code.len());
    for (pc, op) in code.iter().enumerate() {
        let mut ef = Vec::new();
        for (succ, polarity) in successors(op, pc) {
            let fid = if polarity == Some(true) {
                let f = match *op {
                    KOp::BrCmp { cmp, a, b, .. } => fact_of(cmp, Bound::Reg(a), Bound::Reg(b)),
                    KOp::BrCmpK { cmp, a, k, .. } => fact_of(cmp, Bound::Reg(a), Bound::Const(k)),
                    KOp::LenBrCmp { cmp, a, b, .. } => fact_of(cmp, Bound::Reg(a), Bound::Reg(b)),
                    _ => None,
                };
                f.map(|f| {
                    facts.push(f);
                    facts.len() - 1
                })
            } else {
                None
            };
            ef.push((succ, fid));
        }
        edges.push(ef);
    }
    if facts.len() > 64 {
        facts.clear();
        for ef in &mut edges {
            for e in ef.iter_mut() {
                e.1 = None;
            }
        }
    }
    let full: u64 = match facts.len() {
        0 => 0,
        64 => u64::MAX,
        n => (1u64 << n) - 1,
    };
    let kill: Vec<u64> = code
        .iter()
        .map(|op| {
            let ds = defs_of(op);
            let mut m = 0u64;
            for (i, f) in facts.iter().enumerate() {
                if ds.contains(&f.reg) {
                    m |= 1u64 << i;
                }
            }
            m
        })
        .collect();
    let mut fin: Vec<u64> = vec![full; code.len()];
    fin[0] = 0;
    loop {
        let mut changed = false;
        for pc in 0..code.len() {
            let out_base = fin[pc] & !kill[pc];
            for &(succ, fid) in &edges[pc] {
                let mut edge = out_base;
                if let Some(f) = fid {
                    edge |= 1u64 << f;
                }
                let new = fin[succ] & edge;
                if succ != 0 && new != fin[succ] {
                    fin[succ] = new;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // ---- the typing fixed point -----------------------------------------
    // Types are per-REGISTER (emission needs one representation for the
    // whole body); ranges are FLOW-SENSITIVE (per program point, defs KILL
    // the old range) — a pooled scratch register holding the loop counter
    // at one point and an accumulator at another would otherwise leak the
    // counter's bound into the accumulator cycle and diverge. The outer
    // loop re-runs the range dataflow whenever a register demotes or a
    // `%`-divisor demand tightens an entry check.
    let debug = std::env::var_os("CJIT_TY_DEBUG").is_some();
    let mut ty = vec![RegTy::Int; n_regs];
    let mut checks: Vec<EntryCheck> = vec![EntryCheck::NonNeg; n_regs];
    let widen_after = code.len() + 4;
    let pass_cap = widen_after + 16;
    'outer: for _ in 0..(3 * n_regs + 2) {
        // IN[pc]: the register ranges on entry to each op.
        let mut rin: Vec<Vec<Range>> = vec![vec![Range::BOTTOM; n_regs]; code.len()];
        for r in 0..n_regs {
            if ty[r] != RegTy::Int {
                continue;
            }
            rin[0][r] = if entry_meaningful(k, r, fn_mode) {
                match checks[r] {
                    EntryCheck::NonNeg => Range::new(0.0, ENTRY_HI),
                    EntryCheck::Pos => Range::new(1.0, ENTRY_HI),
                    EntryCheck::Exact(v) => Range::new(v as f64, v as f64),
                }
            } else {
                // Scratch: no observable entry value; a (nonexistent)
                // read-before-write would see BOTTOM and demote.
                Range::BOTTOM
            };
        }
        let mut demote_now: Vec<u16> = Vec::new();
        for pass in 0..=pass_cap {
            let mut changed = false;
            for (pc, op) in code.iter().enumerate() {
                let mut work = rin[pc].clone();
                let mut demand: Option<u16> = None;
                // Apply defs sequentially (a fused op's second def sees the
                // first def's result in its operands).
                let results = {
                    let cx = Ctx {
                        ty: &ty,
                        range: &work,
                        facts: &facts,
                        fin: &fin,
                        elem_kinds,
                    };
                    eval_op(op, pc, &cx, &mut demand)
                };
                // A `%` divisor demanded lo ≥ 1: tighten its entry check
                // when it is an entry-only mapped register and restart —
                // without letting the failure that raised the demand demote
                // its destination first.
                if let Some(dr) = demand {
                    let d = dr as usize;
                    if ty[d] == RegTy::Int
                        && !written[d]
                        && entry_meaningful(k, d, fn_mode)
                        && checks[d] == EntryCheck::NonNeg
                    {
                        // Bake the compiling activation's value when it is a
                        // usable divisor — every later read becomes that
                        // constant (strength-reducible `srem`); otherwise a
                        // plain ≥ 1 check.
                        let bake = entry_regs.get(d).copied().and_then(|v| {
                            (v.fract() == 0.0 && (1.0..=ENTRY_HI).contains(&v))
                                .then_some(v as i64)
                        });
                        checks[d] = match bake {
                            Some(v) => EntryCheck::Exact(v),
                            None => EntryCheck::Pos,
                        };
                        continue 'outer;
                    }
                }
                let apply = |d: usize,
                                 res: Option<Range>,
                                 work: &mut Vec<Range>,
                                 demote_now: &mut Vec<u16>| {
                    match res {
                        Some(r0) if r0.is_bottom() => work[d] = Range::BOTTOM,
                        Some(r0) if r0.within_safe() => work[d] = r0,
                        _ => {
                            if ty[d] == RegTy::Int {
                                if debug {
                                    eprintln!(
                                        "    demote r{d} at pc {pc}: {op:?} (res: {:?})",
                                        res.map(|r| (r.lo, r.hi))
                                    );
                                }
                                demote_now.push(d as u16);
                            }
                            work[d] = Range::BOTTOM;
                        }
                    }
                };
                let two = results.len() == 2;
                if let Some(&(dst, res)) = results.first() {
                    apply(dst as usize, res, &mut work, &mut demote_now);
                }
                if two {
                    // Re-evaluate the second def against the first's result
                    // (a fused op's tail may read its own head's dst).
                    let mut d2 = None;
                    let cx = Ctx {
                        ty: &ty,
                        range: &work,
                        facts: &facts,
                        fin: &fin,
                        elem_kinds,
                    };
                    let re = eval_op(op, pc, &cx, &mut d2);
                    if let Some((dst2, res2)) = re.into_iter().nth(1) {
                        apply(dst2 as usize, res2, &mut work, &mut demote_now);
                    }
                }
                if !demote_now.is_empty() {
                    for d in demote_now.drain(..) {
                        ty[d as usize] = RegTy::Float;
                    }
                    continue 'outer;
                }
                // Propagate to successors, WIDENING late-pass growth: a
                // range still climbing after every bound has had time to
                // propagate is a divergent loop-carried value (an unguarded
                // increment) — snap it to the band edge so its next def
                // overflows the band and demotes it, instead of climbing
                // one step per pass forever.
                for &(succ, _) in &edges[pc] {
                    let dst_in = &mut rin[succ];
                    for r in 0..n_regs {
                        if ty[r] != RegTy::Int {
                            continue;
                        }
                        let mut j = dst_in[r].join(work[r]);
                        if pass > widen_after && j != dst_in[r] && !dst_in[r].is_bottom() {
                            if j.hi > dst_in[r].hi {
                                j.hi = MAX_SAFE;
                            }
                            if j.lo < dst_in[r].lo {
                                j.lo = -MAX_SAFE;
                            }
                        }
                        if j != dst_in[r] {
                            dst_in[r] = j;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
            if pass == pass_cap {
                // Should be unreachable with widening; bail out of typing
                // rather than risk looping.
                return None;
            }
        }
        break;
    }

    // ---- final filter ----------------------------------------------------
    // Only registers the code actually touches are worth typing; a mapped
    // register no op reads or writes would add an entry check that can only
    // hurt (a fractional value there would spuriously fail the whole body).
    for r in 0..n_regs {
        if !used[r] && !written[r] {
            ty[r] = RegTy::Float;
        }
    }
    if ty.iter().all(|t| *t == RegTy::Float) {
        return None;
    }
    let mut out_checks = Vec::new();
    let mut temps = Vec::new();
    for r in 0..n_regs {
        if ty[r] != RegTy::Int {
            continue;
        }
        if entry_meaningful(k, r, fn_mode) {
            out_checks.push((r as u16, checks[r]));
        } else {
            temps.push(r as u16);
        }
    }
    Some(RegTyping {
        ty,
        checks: out_checks,
        temps,
    })
}
