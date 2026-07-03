//! Typed loop kernels: translate eligible bytecode loops into unboxed-`f64`
//! register programs (docs/js-performance-roadmap.md §6.5).
//!
//! ## Why
//!
//! The retired closure-threading experiment (docs/jit.md) proved that removing
//! *dispatch* alone buys almost nothing (1.01–1.11×): the interpreter's real
//! per-op tax on numeric loops is the boxed [`Value`](crate::value::Value)
//! traffic — clone, match, drop, operand-stack push/pop — around every add and
//! compare. A kernel keeps the loop's state in a flat `f64` register file for
//! the whole loop, so an iteration of the canonical counting loop is a handful
//! of unboxed register ops instead of a dozen boxed dispatches.
//!
//! ## Safety model
//!
//! A kernel is a pure performance side effect, gated like the inline caches:
//! it may only run where it provably computes exactly what the generic
//! interpreter would.
//!
//! - **Eligibility is static.** A loop region qualifies only if every op in it
//!   is on the numeric allowlist: loads/stores of `frame.locals` slots,
//!   `Number` constants, arithmetic/comparison/branch ops and their fused
//!   forms. Anything else — calls, property access, cells/upvalues, TDZ
//!   *initialization*, try handlers, iterators, suspension — and the loop is
//!   simply not kernelized. No op is ever half-supported.
//! - **Entry is guarded.** At runtime the kernel runs only after checking that
//!   every mapped local currently holds a `Number`. JS numeric ops are closed
//!   over numbers, so once the guard passes no non-number can appear
//!   mid-kernel — there is no mid-loop type surprise and therefore no deopt
//!   map. A failed guard executes the original header op ([`Kernel::fallback`])
//!   and the generic interpreter takes that iteration; the kernel retries when
//!   the back-edge next reaches the header (late entry: a binding that warms
//!   to a number on iteration 1 enters the kernel from iteration 2).
//! - **Semantics are shared, not re-implemented.** Kernel arithmetic calls the
//!   same `number_arith_raw`/`js_mod`/`to_int32` helpers as the interpreter's
//!   `Number`×`Number` fast paths, so results are bit-identical (NaN, -0,
//!   overflow, shift masking — all of it).
//! - **Observability.** Within an eligible region the generic interpreter
//!   touches nothing but local slots, the operand stack, and control flow: no
//!   journal, no allocation, no user code. The op budget IS observable (an
//!   exact-count uncatchable throw), so kernels are disabled whenever a budget
//!   is installed (see `Vm::run_kernel`); the cooperative interrupt flag is
//!   polled on kernel back-edges, preserving prompt cancellation.
//!
//! Determinism: translation is a pure function of the bytecode (itself a pure
//! function of the source) and the guard depends only on program values, so
//! record and replay execute identically with kernels on. The differential
//! corpus (`tests/kernels.rs`) runs every supported construct with kernels on
//! and off and asserts byte-identical behavior.

use crate::bytecode::{CmpOp, Const, KOp, Kernel, Op};
use crate::exec::ArithKind;

/// Bounds keeping `u16` fields comfortable and per-kernel work finite. Loops
/// beyond them stay generic.
const MAX_REGION_OPS: usize = 512;
const MAX_KOPS: usize = 2048;
/// Provisional numbering during translation: stack slot `d` is register
/// `STACK_BASE + d`, scratch is `SCRATCH0/1`. A final remap compacts the file
/// to `locals + stack + scratch` so the executor zeroes only what is used.
const STACK_BASE: u16 = 64;
const SCRATCH0: u16 = 252;
const SCRATCH1: u16 = 253;

/// Find every eligible loop in `code` and install kernels for them. Returns
/// the (possibly rewritten) code and the kernel table. Loop-header ops are
/// replaced by [`Op::LoopKernel`] IN PLACE — code length, instruction indices
/// and all jump targets are unchanged.
pub fn kernelize(mut code: Vec<Op>, consts: &[Const]) -> (Vec<Op>, Vec<Kernel>) {
    let mut kernels: Vec<Kernel> = Vec::new();

    // Collect back-edges: any branch whose target is at or before it. Group by
    // header, keeping the furthest back-edge, so a loop with several continue
    // paths gets ONE region covering all of them.
    let mut regions: Vec<(usize, usize)> = Vec::new(); // (start, end) inclusive
    for (ip, op) in code.iter().enumerate() {
        for t in op_targets(op) {
            let t = t as usize;
            if t <= ip {
                match regions.iter_mut().find(|(s, _)| *s == t) {
                    Some((_, e)) => *e = (*e).max(ip),
                    None => regions.push((t, ip)),
                }
            }
        }
    }
    // Innermost-first: smaller regions translate before enclosing ones; an
    // enclosing region then sees the inner loop's fresh `LoopKernel` op and is
    // rejected by the allowlist (nested kernels are out of scope for v1).
    regions.sort_by_key(|&(s, e)| (e - s, s));

    for (start, end) in regions {
        if end - start + 1 > MAX_REGION_OPS {
            continue;
        }
        // Single-entry check: nothing outside the region may branch into its
        // interior. (The region start is the loop entry and is fine — jumps
        // and fallthrough arrive at the header, which is the kernel op.)
        let jumps_into_interior = code.iter().enumerate().any(|(ip, op)| {
            (ip < start || ip > end)
                && op_targets(op)
                    .iter()
                    .any(|&t| (t as usize) > start && (t as usize) <= end)
        });
        if jumps_into_interior {
            continue;
        }
        if let Some(mut k) = translate(&code[start..=end], start as u32, consts) {
            k.fallback = Box::new(code[start].clone());
            code[start] = Op::LoopKernel(kernels.len() as u32);
            kernels.push(k);
        }
    }
    (code, kernels)
}

/// Every code index an op can transfer control to (not counting fallthrough).
/// `u32::MAX` sentinels ("no finally", "no delegation return") are filtered.
/// This must cover EVERY variant carrying a code index — a missed one would
/// let the pass treat a jumped-into loop as single-entry.
fn op_targets(op: &Op) -> Vec<u32> {
    let mut out = Vec::new();
    match op {
        Op::Jump(t)
        | Op::JumpIfTrue(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfNullish(t)
        | Op::JumpIfFalsyPeek(t)
        | Op::JumpIfTruthyPeek(t)
        | Op::JumpIfNullishPeek(t) => out.push(*t),
        Op::CmpBranchFalse { target, .. }
        | Op::CmpBranchTrue { target, .. }
        | Op::CmpLocalConstBranchFalse { target, .. }
        | Op::CmpLocalConstBranchTrue { target, .. }
        | Op::CmpCellConstBranchFalse { target, .. }
        | Op::CmpCellConstBranchTrue { target, .. }
        | Op::CompletionJump { target, .. } => out.push(*target),
        Op::PushTryHandler { catch, finally } => {
            if *catch != u32::MAX {
                out.push(*catch);
            }
            if *finally != u32::MAX {
                out.push(*finally);
            }
        }
        Op::MarkDelegationHandler(t) if *t != u32::MAX => out.push(*t),
        _ => {}
    }
    out
}

struct Xlate<'a> {
    region: &'a [Op],
    base_ip: u32,
    consts: &'a [Const],
    kops: Vec<KOp>,
    /// kernel pc for each region-relative ip (`u16::MAX` = not emitted).
    kpc: Vec<u16>,
    /// expected canonical stack depth at each region-relative ip, when known.
    depth_at: Vec<Option<u16>>,
    /// in-region branch targets (any op branching to that ip).
    is_target: Vec<bool>,
    /// (frame-local index, register) pairs, registers dense from 0.
    local_reg: Vec<(u32, u16)>,
    /// pending in-region branch fixups: (kop index, region-relative target).
    fixups: Vec<(usize, usize)>,
    /// pending exits: (kop index, absolute resume ip, stack depth).
    exits: Vec<(usize, u32, u16)>,
    /// a compare op fused with its following conditional jump: skip that ip.
    absorbed: Option<usize>,
    max_stack: u16,
}

/// Attempt to translate a region into a kernel. Returns `None` — the loop
/// stays generic — on ANY construct outside the allowlist. `fallback` is a
/// placeholder for the caller to fill.
fn translate(region: &[Op], base_ip: u32, consts: &[Const]) -> Option<Kernel> {
    let mut is_target = vec![false; region.len()];
    for op in region {
        for t in op_targets(op) {
            let rel = (t as usize).wrapping_sub(base_ip as usize);
            if rel < region.len() {
                is_target[rel] = true;
            }
        }
    }

    let mut x = Xlate {
        region,
        base_ip,
        consts,
        kops: Vec::new(),
        kpc: vec![u16::MAX; region.len()],
        depth_at: vec![None; region.len()],
        is_target,
        local_reg: Vec::new(),
        fixups: Vec::new(),
        exits: Vec::new(),
        absorbed: None,
        max_stack: 0,
    };
    x.depth_at[0] = Some(0);

    let mut depth: u16 = 0;
    let mut reachable = true;
    // The index is load-bearing (kpc/depth_at bookkeeping keyed by ip).
    #[allow(clippy::needless_range_loop)]
    for i in 0..region.len() {
        if x.absorbed.take() == Some(i) {
            // This conditional jump was fused into the preceding compare.
            continue;
        }
        match x.depth_at[i] {
            Some(d) => {
                // A recorded depth: either a forward-branch target (must agree
                // with the fallthrough depth when both reach it) or ip 0.
                if reachable && d != depth {
                    return None; // inconsistent stack depth at a merge point
                }
                depth = d;
                reachable = true;
            }
            None => {
                if !reachable {
                    // Dead code (after an unconditional jump, not a known
                    // target). It can never execute; skip. A later branch INTO
                    // it fails the fixup pass below (kpc stays unmapped).
                    continue;
                }
                // Record the depth every executed op is translated at, so a
                // later BACKWARD branch to it verifies register alignment.
                x.depth_at[i] = Some(depth);
            }
        }
        x.kpc[i] = u16::try_from(x.kops.len()).ok()?;
        depth = x.emit(i, depth)?;
        x.max_stack = x.max_stack.max(depth);
        if x.kops.len() > MAX_KOPS {
            return None;
        }
        // Ops with no fallthrough edge end the current basic block.
        if matches!(region[i], Op::Jump(_) | Op::JumpIfNullishPeek(_)) {
            reachable = false;
        }
    }
    // A region ending on an op WITH a fallthrough edge (a conditional
    // back-edge — the do/while shape) continues into the bytecode right after
    // the region: synthesize that exit.
    if reachable {
        let resume = base_ip + u32::try_from(region.len()).ok()?;
        x.kops.push(KOp::Exit {
            resume_ip: resume,
            stack: depth,
        });
    }

    // Patch in-region branches.
    let fixups = std::mem::take(&mut x.fixups);
    for (kidx, rel) in fixups {
        let pc = x.kpc[rel];
        if pc == u16::MAX {
            return None; // branch into an absorbed/dead instruction
        }
        patch(&mut x.kops, kidx, pc);
    }
    // Synthesize exit stubs.
    let exits = std::mem::take(&mut x.exits);
    for (kidx, resume_ip, stack) in exits {
        let pc = u16::try_from(x.kops.len()).ok()?;
        x.kops.push(KOp::Exit { resume_ip, stack });
        patch(&mut x.kops, kidx, pc);
    }
    if x.kops.len() > MAX_KOPS {
        return None;
    }

    // Compact the register file: locals stay 0..n, provisional stack slots
    // move to n.., scratch to the top. The executor zeroes exactly `n_regs`.
    let n_locals = u16::try_from(x.local_reg.len()).ok()?;
    let remap = |r: u16| -> u16 {
        if r < STACK_BASE {
            r
        } else if r == SCRATCH0 {
            n_locals + x.max_stack
        } else if r == SCRATCH1 {
            n_locals + x.max_stack + 1
        } else {
            n_locals + (r - STACK_BASE)
        }
    };
    for kop in &mut x.kops {
        match kop {
            KOp::Mov { dst, src } | KOp::Neg { dst, src } | KOp::BitNot { dst, src } => {
                *dst = remap(*dst);
                *src = remap(*src);
            }
            KOp::Const { dst, .. } => *dst = remap(*dst),
            KOp::Add { dst, a, b } | KOp::Arith { dst, a, b, .. } => {
                *dst = remap(*dst);
                *a = remap(*a);
                *b = remap(*b);
            }
            KOp::AddK { dst, a, .. } | KOp::ArithK { dst, a, .. } => {
                *dst = remap(*dst);
                *a = remap(*a);
            }
            KOp::BrCmp { a, b, .. } => {
                *a = remap(*a);
                *b = remap(*b);
            }
            KOp::BrCmpK { a, .. } => *a = remap(*a),
            KOp::BrFalsy { src, .. } | KOp::BrTruthy { src, .. } => *src = remap(*src),
            KOp::Br { .. } | KOp::Exit { .. } => {}
        }
    }

    let mut locals: Vec<u32> = vec![0; n_locals as usize];
    for &(l, r) in &x.local_reg {
        locals[r as usize] = l;
    }
    Some(Kernel {
        code: x.kops.into_boxed_slice(),
        locals: locals.into_boxed_slice(),
        n_regs: n_locals + x.max_stack + 2,
        fallback: Box::new(Op::Nop), // caller stores the real header op
    })
}

fn patch(kops: &mut [KOp], kidx: usize, pc: u16) {
    match &mut kops[kidx] {
        KOp::Br { target }
        | KOp::BrCmp { target, .. }
        | KOp::BrCmpK { target, .. }
        | KOp::BrFalsy { target, .. }
        | KOp::BrTruthy { target, .. } => *target = pc,
        _ => unreachable!("patching a non-branch kop"),
    }
}

impl Xlate<'_> {
    /// Register mirroring `frame.locals[l]`.
    fn lreg(&mut self, l: u32) -> Option<u16> {
        if let Some(&(_, r)) = self.local_reg.iter().find(|(ll, _)| *ll == l) {
            return Some(r);
        }
        let r = u16::try_from(self.local_reg.len()).ok()?;
        if r >= STACK_BASE {
            return None; // more distinct locals than the numbering supports
        }
        self.local_reg.push((l, r));
        Some(r)
    }

    /// Provisional register for canonical stack slot `d`.
    fn sreg(&self, d: u16) -> Option<u16> {
        if STACK_BASE + d >= SCRATCH0 {
            return None;
        }
        Some(STACK_BASE + d)
    }

    fn num_const(&self, idx: u32) -> Option<f64> {
        match self.consts.get(idx as usize)? {
            Const::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Record a branch to absolute bytecode ip `t` from the kop at `kidx`:
    /// in-region targets become fixups (recording/checking the canonical
    /// stack depth `depth` at the target); out-of-region targets synthesize an
    /// [`KOp::Exit`] stub that materializes `depth` stack slots.
    fn branch_to(&mut self, kidx: usize, t: u32, depth: u16) -> Option<()> {
        let rel = (t as usize).wrapping_sub(self.base_ip as usize);
        if rel < self.region.len() {
            match self.depth_at[rel] {
                Some(d) if d != depth => return None,
                _ => self.depth_at[rel] = Some(depth),
            }
            self.fixups.push((kidx, rel));
        } else {
            self.exits.push((kidx, t, depth));
        }
        Some(())
    }

    fn bin(&mut self, kind: ArithKind, d: u16) -> Option<u16> {
        let a = self.sreg(d.checked_sub(2)?)?;
        let b = self.sreg(d - 1)?;
        self.kops.push(KOp::Arith { kind, dst: a, a, b });
        Some(d - 1)
    }

    /// Translate the op at region-relative ip `i` at canonical stack depth
    /// `d`; returns the post-op depth, or `None` to reject the whole region.
    fn emit(&mut self, i: usize, d: u16) -> Option<u16> {
        use KOp as K;
        let op = self.region[i].clone();
        Some(match &op {
            Op::Nop => d,

            // ---- locals (TDZ subsumed by the entry guard: a mapped local is
            // a Number, never Uninitialized) ----
            Op::LoadLocal(l) => {
                let src = self.lreg(*l)?;
                let dst = self.sreg(d)?;
                self.kops.push(K::Mov { dst, src });
                d + 1
            }
            // The checked store's TDZ test reads the CURRENT slot value; the
            // guard proves it is a Number, so the store is a plain move.
            Op::StoreLocal(l) | Op::StoreLocalChecked(l) => {
                let dst = self.lreg(*l)?;
                let src = self.sreg(d.checked_sub(1)?)?;
                self.kops.push(K::Mov { dst, src });
                d - 1
            }
            Op::CopyLocal { src, dest } => {
                let s = self.lreg(*src)?;
                let dst = self.lreg(*dest)?;
                if s != dst {
                    self.kops.push(K::Mov { dst, src: s });
                }
                d
            }
            Op::LoadConst(c) => {
                let k = self.num_const(*c)?;
                let dst = self.sreg(d)?;
                self.kops.push(K::Const { dst, k });
                d + 1
            }
            Op::LoadLocalConst { local, konst } => {
                let src = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let dst0 = self.sreg(d)?;
                let dst1 = self.sreg(d + 1)?;
                self.kops.push(K::Mov { dst: dst0, src });
                self.kops.push(K::Const { dst: dst1, k });
                d + 2
            }

            // ---- stack shuffles ----
            Op::Pop => d.checked_sub(1)?,
            Op::Dup => {
                let src = self.sreg(d.checked_sub(1)?)?;
                let dst = self.sreg(d)?;
                self.kops.push(K::Mov { dst, src });
                d + 1
            }
            Op::Swap => {
                let a = self.sreg(d.checked_sub(2)?)?;
                let b = self.sreg(d - 1)?;
                self.kops.push(K::Mov {
                    dst: SCRATCH0,
                    src: a,
                });
                self.kops.push(K::Mov { dst: a, src: b });
                self.kops.push(K::Mov {
                    dst: b,
                    src: SCRATCH0,
                });
                d
            }
            Op::Rot3 => {
                // [a b c] -> [c a b] (match the interpreter's Rot3).
                let a = self.sreg(d.checked_sub(3)?)?;
                let b = self.sreg(d - 2)?;
                let c = self.sreg(d - 1)?;
                self.kops.push(K::Mov {
                    dst: SCRATCH0,
                    src: c,
                });
                self.kops.push(K::Mov { dst: c, src: b });
                self.kops.push(K::Mov { dst: b, src: a });
                self.kops.push(K::Mov {
                    dst: a,
                    src: SCRATCH0,
                });
                d
            }

            // ---- arithmetic (numbers in, numbers out) ----
            Op::Add => {
                let a = self.sreg(d.checked_sub(2)?)?;
                let b = self.sreg(d - 1)?;
                self.kops.push(K::Add { dst: a, a, b });
                d - 1
            }
            Op::Sub => self.bin(ArithKind::Sub, d)?,
            Op::Mul => self.bin(ArithKind::Mul, d)?,
            Op::Div => self.bin(ArithKind::Div, d)?,
            Op::Mod => self.bin(ArithKind::Mod, d)?,
            Op::Pow => self.bin(ArithKind::Pow, d)?,
            Op::BitAnd => self.bin(ArithKind::BitAnd, d)?,
            Op::BitOr => self.bin(ArithKind::BitOr, d)?,
            Op::BitXor => self.bin(ArithKind::BitXor, d)?,
            Op::Shl => self.bin(ArithKind::Shl, d)?,
            Op::Shr => self.bin(ArithKind::Shr, d)?,
            Op::UShr => self.bin(ArithKind::UShr, d)?,
            Op::Neg => {
                let s = self.sreg(d.checked_sub(1)?)?;
                self.kops.push(K::Neg { dst: s, src: s });
                d
            }
            Op::BitNot => {
                let s = self.sreg(d.checked_sub(1)?)?;
                self.kops.push(K::BitNot { dst: s, src: s });
                d
            }
            // ToNumber/ToNumeric on a Number is the identity.
            Op::Pos | Op::ToNumeric => d,
            Op::Inc => {
                let s = self.sreg(d.checked_sub(1)?)?;
                self.kops.push(K::AddK {
                    dst: s,
                    a: s,
                    k: 1.0,
                });
                d
            }
            Op::Dec => {
                let s = self.sreg(d.checked_sub(1)?)?;
                self.kops.push(K::AddK {
                    dst: s,
                    a: s,
                    k: -1.0,
                });
                d
            }

            // ---- fused local-const forms ----
            Op::AddLocalConst { local, konst } => {
                let a = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let dst = self.sreg(d)?;
                self.kops.push(K::AddK { dst, a, k });
                d + 1
            }
            Op::ArithLocalConst { local, konst, kind } => {
                let a = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let dst = self.sreg(d)?;
                self.kops.push(K::ArithK {
                    kind: *kind,
                    dst,
                    a,
                    k,
                });
                d + 1
            }
            Op::IncLocalStmt { local, dec } => {
                let r = self.lreg(*local)?;
                self.kops.push(K::AddK {
                    dst: r,
                    a: r,
                    k: if *dec { -1.0 } else { 1.0 },
                });
                d
            }

            // ---- comparisons: supported only in compare-and-branch form. A
            // materialized boolean (stored, returned, fed to arithmetic)
            // would put a non-number on the stack — reject the region.
            Op::Eq | Op::Ne | Op::StrictEq | Op::StrictNe | Op::Lt | Op::Gt | Op::Le | Op::Ge => {
                let cmp = match &op {
                    Op::Eq => CmpOp::Eq,
                    Op::Ne => CmpOp::Ne,
                    Op::StrictEq => CmpOp::StrictEq,
                    Op::StrictNe => CmpOp::StrictNe,
                    Op::Lt => CmpOp::Lt,
                    Op::Gt => CmpOp::Gt,
                    Op::Le => CmpOp::Le,
                    Op::Ge => CmpOp::Ge,
                    _ => unreachable!(),
                };
                // Must be immediately consumed by a conditional jump that is
                // itself not a branch target (it is fused away entirely).
                let (if_true, target) = match self.region.get(i + 1) {
                    Some(Op::JumpIfFalse(t)) if !self.is_target[i + 1] => (false, *t),
                    Some(Op::JumpIfTrue(t)) if !self.is_target[i + 1] => (true, *t),
                    _ => return None,
                };
                let a = self.sreg(d.checked_sub(2)?)?;
                let b = self.sreg(d - 1)?;
                let kidx = self.kops.len();
                self.kops.push(K::BrCmp {
                    cmp,
                    a,
                    b,
                    if_true,
                    target: u16::MAX,
                });
                self.branch_to(kidx, target, d - 2)?;
                self.absorbed = Some(i + 1);
                d - 2
            }

            // ---- branches ----
            Op::Jump(t) => {
                let kidx = self.kops.len();
                self.kops.push(K::Br { target: u16::MAX });
                self.branch_to(kidx, *t, d)?;
                d
            }
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                let src = self.sreg(d.checked_sub(1)?)?;
                let kidx = self.kops.len();
                if matches!(op, Op::JumpIfFalse(_)) {
                    self.kops.push(K::BrFalsy {
                        src,
                        target: u16::MAX,
                    });
                } else {
                    self.kops.push(K::BrTruthy {
                        src,
                        target: u16::MAX,
                    });
                }
                self.branch_to(kidx, *t, d - 1)?;
                d - 1
            }
            // Peek variants (`&&`/`||` short-circuits): the TAKEN edge keeps
            // the tested value on the stack (depth d); the fallthrough edge
            // pops it (depth d-1).
            Op::JumpIfFalsyPeek(t) | Op::JumpIfTruthyPeek(t) => {
                let src = self.sreg(d.checked_sub(1)?)?;
                let kidx = self.kops.len();
                if matches!(op, Op::JumpIfFalsyPeek(_)) {
                    self.kops.push(K::BrFalsy {
                        src,
                        target: u16::MAX,
                    });
                } else {
                    self.kops.push(K::BrTruthy {
                        src,
                        target: u16::MAX,
                    });
                }
                self.branch_to(kidx, *t, d)?;
                d - 1
            }
            // A Number is never nullish. `JumpIfNullishPeek` jumps on NOT
            // nullish keeping the value — for kernel-typed values that is an
            // unconditional jump (the pop-and-fall-through edge is the nullish
            // case, unreachable here).
            Op::JumpIfNullishPeek(t) => {
                d.checked_sub(1)?;
                let kidx = self.kops.len();
                self.kops.push(K::Br { target: u16::MAX });
                self.branch_to(kidx, *t, d)?;
                d
            }
            // `JumpIfNullish` jumps on nullish (replacing the top with
            // `undefined`) — never taken for kernel-typed values: no-op.
            Op::JumpIfNullish(_) => {
                d.checked_sub(1)?; // require an operand, like the interpreter
                d
            }

            Op::CmpBranchFalse { cmp, target } | Op::CmpBranchTrue { cmp, target } => {
                let if_true = matches!(op, Op::CmpBranchTrue { .. });
                let a = self.sreg(d.checked_sub(2)?)?;
                let b = self.sreg(d - 1)?;
                let kidx = self.kops.len();
                self.kops.push(K::BrCmp {
                    cmp: *cmp,
                    a,
                    b,
                    if_true,
                    target: u16::MAX,
                });
                self.branch_to(kidx, *target, d - 2)?;
                d - 2
            }
            Op::CmpLocalConstBranchFalse {
                local,
                konst,
                cmp,
                target,
            }
            | Op::CmpLocalConstBranchTrue {
                local,
                konst,
                cmp,
                target,
            } => {
                let if_true = matches!(op, Op::CmpLocalConstBranchTrue { .. });
                let a = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let kidx = self.kops.len();
                self.kops.push(K::BrCmpK {
                    cmp: *cmp,
                    a,
                    k,
                    if_true,
                    target: u16::MAX,
                });
                self.branch_to(kidx, *target, d)?;
                d
            }

            // Anything else — calls, property access, cells, upvalues, TDZ
            // init, globals, objects, strings, try/dispose/iterator/suspend
            // machinery, nested kernels — rejects the region.
            _ => return None,
        })
    }
}
