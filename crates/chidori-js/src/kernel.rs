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
//! - **Eligibility is static.** A loop region qualifies only if every op is on
//!   the allowlist: loads/stores of localized `frame.locals` slots, `Number`
//!   constants, arithmetic/comparison/branch ops and their fused forms, plus
//!   dense-array element access (`a[i]`, `a[i] = v`, `a.length`) on
//!   local-held bases. Anything else — calls, other property access,
//!   cells/upvalues, TDZ *initialization*, try handlers, iterators,
//!   suspension — and the loop is simply not kernelized. An inner loop's own
//!   `Op::LoopKernel` translates as its preserved fallback op, so NESTED
//!   numeric loops kernelize as one outer kernel.
//! - **Entry is guarded, accesses are checked, exits are precise.** The kernel
//!   runs only after checking every mapped numeric local holds a `Number` and
//!   every array-base local holds an object. Numeric JS ops are closed over
//!   numbers, so no non-number can appear mid-kernel from arithmetic; the ONLY
//!   speculative reads are array elements, and each `LoadElem`/`StoreElem`/
//!   `LoadLen` re-checks its full fast-path condition (unshadowed dense array,
//!   integral in-bounds index, non-hole `Number` element) and otherwise BAILS:
//!   registers are written back and the generic interpreter resumes AT the
//!   access op with the operand stack reconstructed from a shape table — the
//!   slow path then performs the exact spec semantics. A failed entry guard
//!   executes the original header op ([`Kernel::fallback`]); the kernel
//!   retries when the back-edge next reaches the header (late entry).
//! - **Semantics are shared, not re-implemented.** Kernel arithmetic calls the
//!   same `number_arith_raw`/`js_mod`/`to_int32` helpers as the interpreter's
//!   `Number`×`Number` fast paths, and element access mirrors the
//!   `Op::GetPropDynamic`/`Op::SetPropDynamic` fast-path conditions verbatim,
//!   so results are bit-identical (NaN, -0, shift masking, holes — all of it).
//! - **Observability.** Within an eligible region the generic interpreter
//!   touches nothing but local slots, dense elements, the operand stack, and
//!   control flow: no journal, no allocation, no user code. The op budget IS
//!   observable (an exact-count uncatchable throw), so kernels are disabled
//!   whenever a budget is installed (see `Vm::run_kernel_op`); the cooperative
//!   interrupt flag is polled on kernel back-edges, preserving prompt
//!   cancellation.
//!
//! Determinism: translation is a pure function of the bytecode (itself a pure
//! function of the source) and every guard depends only on program values, so
//! record and replay execute identically with kernels on. The differential
//! corpus + fuzz (`tests/kernels.rs`) runs every supported construct with
//! kernels on and off and asserts byte-identical behavior.

use crate::bytecode::{CmpOp, Const, KOp, KShapeSlot, Kernel, Op};
use crate::exec::ArithKind;

/// Bounds keeping `u16` fields comfortable and per-kernel work finite. Loops
/// beyond them stay generic.
const MAX_REGION_OPS: usize = 512;
const MAX_KOPS: usize = 2048;
/// Provisional numbering during translation: the virtual-stack entry at
/// position `p` owns register `STACK_BASE + p` (object entries reserve theirs
/// unused — uniform numbering keeps shuffles and merges trivial); `SCRATCH0`
/// is the shuffle scratch. A final remap compacts the register file.
const STACK_BASE: u16 = 64;
const SCRATCH0: u16 = 252;

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
    // Innermost-first: an inner loop kernelizes on its own; when the ENCLOSING
    // region is then translated, the inner `Op::LoopKernel` header translates
    // as its preserved fallback op and the rest of the inner loop's bytecode
    // (still present in the region) translates normally — so a fully-numeric
    // loop NEST becomes one outer kernel, and the inner kernel simply never
    // dispatches at runtime. If the outer region is ineligible, the inner
    // kernel still runs on its own.
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
        // Phase 1 discovers which locals are used as array BASES (`a[i]`);
        // phase 2 re-translates with those locals as object slots. Both
        // phases reject identically on anything off the allowlist.
        let oslots = match translate(&code[start..=end], start as u32, consts, &kernels, &[]) {
            Some((_, discovered)) => discovered,
            None => continue,
        };
        if let Some((mut k, _)) =
            translate(&code[start..=end], start as u32, consts, &kernels, &oslots)
        {
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

/// A virtual operand-stack entry during translation. The entry at stack
/// position `p` owns provisional register `STACK_BASE + p`; `Obj` entries
/// reserve theirs unused (values live in object slots, resolved at runtime).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VE {
    Num,
    /// An array base: object slot index.
    Obj(u16),
    /// The value `undefined`, from `LoadUndefined`. Exists only transiently
    /// within a basic block: its ONLY legal consumer is a store to a local
    /// that is provably re-stored before any read (the compiler's
    /// per-iteration `let` reset emits `LoadUndefined; StoreLocal(j)` right
    /// before the loop re-initializes `j`). Everything else rejects.
    Undef,
}

struct Xlate<'a> {
    region: &'a [Op],
    base_ip: u32,
    consts: &'a [Const],
    /// Already-built kernels (inner loops) — `Op::LoopKernel` translates as
    /// its fallback op.
    inner: &'a [Kernel],
    /// Locals designated as array bases (phase 2) — empty in phase 1.
    oslot_locals: &'a [u32],
    kops: Vec<KOp>,
    /// kernel pc for each region-relative ip (`u16::MAX` = not emitted).
    kpc: Vec<u16>,
    /// expected virtual-stack shape at each region-relative ip, when known.
    shape_at: Vec<Option<Vec<VE>>>,
    /// in-region branch targets (any op branching to that ip).
    is_target: Vec<bool>,
    /// numeric locals: (frame-local index, register), registers dense from 0.
    local_reg: Vec<(u32, u16)>,
    /// phase 1: locals observed as array bases (with clean origins).
    discovered: Vec<u32>,
    /// phase 1 origin tracking: (local, version) of a pushed LoadLocal.
    origins: Vec<Option<(u32, u32)>>,
    versions: std::collections::HashMap<u32, u32>,
    /// the current virtual stack.
    vstack: Vec<VE>,
    /// pending in-region branch fixups: (kop index, region-relative target).
    fixups: Vec<(usize, usize)>,
    /// pending exits: (kop index or LoadElem-style bail, resume ip, shape).
    exits: Vec<(usize, u32, Vec<VE>)>,
    /// a compare op fused with its following conditional jump: skip that ip.
    absorbed: Option<usize>,
    max_stack: u16,
}

/// Attempt to translate a region. Returns the kernel (fallback is a
/// placeholder) and the discovered array-base locals, or `None` — the loop
/// stays generic — on ANY construct outside the allowlist.
fn translate(
    region: &[Op],
    base_ip: u32,
    consts: &[Const],
    inner: &[Kernel],
    oslot_locals: &[u32],
) -> Option<(Kernel, Vec<u32>)> {
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
        inner,
        oslot_locals,
        kops: Vec::new(),
        kpc: vec![u16::MAX; region.len()],
        shape_at: vec![None; region.len()],
        is_target,
        local_reg: Vec::new(),
        discovered: Vec::new(),
        origins: Vec::new(),
        versions: std::collections::HashMap::new(),
        vstack: Vec::new(),
        fixups: Vec::new(),
        exits: Vec::new(),
        absorbed: None,
        max_stack: 0,
    };
    x.shape_at[0] = Some(Vec::new());

    let mut reachable = true;
    // The index is load-bearing (kpc/shape_at bookkeeping keyed by ip).
    #[allow(clippy::needless_range_loop)]
    for i in 0..region.len() {
        if x.absorbed.take() == Some(i) {
            // This conditional jump was fused into the preceding compare.
            continue;
        }
        match x.shape_at[i].take() {
            Some(shape) => {
                if reachable && shape != x.vstack {
                    return None; // inconsistent stack shape at a merge point
                }
                if !reachable {
                    x.vstack = shape.clone();
                    x.origins = vec![None; shape.len()];
                }
                x.shape_at[i] = Some(shape);
                reachable = true;
            }
            None => {
                if !reachable {
                    // Dead code (after an unconditional jump, not a known
                    // target). It can never execute; skip. A later branch INTO
                    // it fails the fixup pass below (kpc stays unmapped).
                    continue;
                }
                // Record the shape every executed op is translated at, so a
                // later BACKWARD branch to it verifies register alignment.
                x.shape_at[i] = Some(x.vstack.clone());
            }
        }
        x.kpc[i] = u16::try_from(x.kops.len()).ok()?;
        let op = &region[i];
        x.emit(op, i)?;
        x.max_stack = x.max_stack.max(u16::try_from(x.vstack.len()).ok()?);
        if x.kops.len() > MAX_KOPS || x.vstack.len() + (STACK_BASE as usize) >= SCRATCH0 as usize {
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
        let shape = x.vstack.clone();
        let kidx = x.kops.len();
        x.kops.push(KOp::Br { target: u16::MAX });
        x.exits.push((kidx, resume, shape));
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
    // Synthesize exit stubs (deduplicated by resume ip + shape) and collect
    // the shape table.
    let exits = std::mem::take(&mut x.exits);
    let mut shapes: Vec<Vec<VE>> = Vec::new();
    let mut stubs: Vec<(u32, u16, u16)> = Vec::new(); // (resume, shape idx, pc)
    for (kidx, resume_ip, shape) in exits {
        if shape.contains(&VE::Undef) {
            return None; // `undefined` never survives to an exit
        }
        let sidx = match shapes.iter().position(|s| *s == shape) {
            Some(p) => p as u16,
            None => {
                shapes.push(shape);
                (shapes.len() - 1) as u16
            }
        };
        let pc = match stubs.iter().find(|(r, s, _)| *r == resume_ip && *s == sidx) {
            Some(&(_, _, pc)) => pc,
            None => {
                let pc = u16::try_from(x.kops.len()).ok()?;
                x.kops.push(KOp::Exit {
                    resume_ip,
                    shape: sidx,
                });
                stubs.push((resume_ip, sidx, pc));
                pc
            }
        };
        patch(&mut x.kops, kidx, pc);
    }
    if x.kops.len() > MAX_KOPS {
        return None;
    }

    // Compact the register file: numeric locals stay 0..n, provisional stack
    // position regs move to n.., scratch to the top.
    let n_locals = u16::try_from(x.local_reg.len()).ok()?;
    let remap = |r: u16| -> u16 {
        if r < STACK_BASE {
            r
        } else if r == SCRATCH0 {
            n_locals + x.max_stack
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
            KOp::LoadElem { dst, idx, .. } => {
                *dst = remap(*dst);
                *idx = remap(*idx);
            }
            KOp::StoreElem { idx, val, .. } => {
                *idx = remap(*idx);
                *val = remap(*val);
            }
            KOp::LoadLen { dst, .. } => *dst = remap(*dst),
            KOp::Br { .. } | KOp::Exit { .. } => {}
        }
    }
    let shapes: Vec<Box<[KShapeSlot]>> = shapes
        .into_iter()
        .map(|s| {
            s.iter()
                .enumerate()
                .map(|(p, e)| match e {
                    VE::Num => KShapeSlot::Num(remap(STACK_BASE + p as u16)),
                    VE::Obj(o) => KShapeSlot::Obj(*o),
                    VE::Undef => unreachable!("undef never crosses block boundaries"),
                })
                .collect()
        })
        .collect();

    let mut locals: Vec<u32> = vec![0; n_locals as usize];
    for &(l, r) in &x.local_reg {
        locals[r as usize] = l;
    }
    Some((
        Kernel {
            code: x.kops.into_boxed_slice(),
            locals: locals.into_boxed_slice(),
            oslots: x.oslot_locals.to_vec().into_boxed_slice(),
            shapes: shapes.into_boxed_slice(),
            n_regs: n_locals + x.max_stack + 1,
            fallback: Box::new(Op::Nop), // caller stores the real header op
        },
        x.discovered,
    ))
}

fn patch(kops: &mut [KOp], kidx: usize, pc: u16) {
    match &mut kops[kidx] {
        KOp::Br { target }
        | KOp::BrCmp { target, .. }
        | KOp::BrCmpK { target, .. }
        | KOp::BrFalsy { target, .. }
        | KOp::BrTruthy { target, .. } => *target = pc,
        KOp::LoadElem { bail, .. } | KOp::StoreElem { bail, .. } | KOp::LoadLen { bail, .. } => {
            *bail = pc
        }
        _ => unreachable!("patching a non-branch kop"),
    }
}

impl Xlate<'_> {
    /// Register mirroring numeric `frame.locals[l]`. An array-base local can
    /// never be a numeric local (that would need it to hold a Number and an
    /// object at once — some iteration would bail every time; reject).
    fn lreg(&mut self, l: u32) -> Option<u16> {
        if self.oslot_locals.contains(&l) {
            return None;
        }
        if let Some(&(_, r)) = self.local_reg.iter().find(|(ll, _)| *ll == l) {
            return Some(r);
        }
        let r = u16::try_from(self.local_reg.len()).ok()?;
        if r >= STACK_BASE {
            return None;
        }
        self.local_reg.push((l, r));
        Some(r)
    }

    /// Provisional register owned by virtual-stack position `p`.
    fn preg(&self, p: usize) -> Option<u16> {
        let r = STACK_BASE as usize + p;
        if r >= SCRATCH0 as usize {
            return None;
        }
        Some(r as u16)
    }

    /// Register of the CURRENT top-of-stack numeric entry `depth_from_top`
    /// below the top (0 = top).
    fn top_reg(&self, depth_from_top: usize) -> Option<u16> {
        let p = self.vstack.len().checked_sub(1 + depth_from_top)?;
        match self.vstack[p] {
            VE::Num => self.preg(p),
            VE::Obj(_) | VE::Undef => None,
        }
    }

    fn num_const(&self, idx: u32) -> Option<f64> {
        match self.consts.get(idx as usize)? {
            Const::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Push a Num entry, returning its register.
    fn push_num(&mut self) -> Option<u16> {
        let r = self.preg(self.vstack.len())?;
        self.vstack.push(VE::Num);
        self.origins.push(None);
        Some(r)
    }

    fn pop(&mut self) -> Option<VE> {
        self.origins.pop();
        self.vstack.pop()
    }

    /// Pop, requiring a Num entry; returns its (position) register.
    fn pop_num(&mut self) -> Option<u16> {
        let r = self.top_reg(0)?;
        self.pop();
        Some(r)
    }

    /// Bump a local's version (any store invalidates pushed origins).
    fn store_of(&mut self, l: u32) {
        *self.versions.entry(l).or_insert(0) += 1;
    }

    /// Resolve the entry at `depth_from_top` as an ARRAY BASE. Phase 1: if it
    /// has a clean local origin, record the local as discovered and treat the
    /// entry as an object. Phase 2: it must already be `VE::Obj`.
    fn base_slot(&mut self, depth_from_top: usize) -> Option<u16> {
        let p = self.vstack.len().checked_sub(1 + depth_from_top)?;
        match self.vstack[p] {
            VE::Obj(s) => Some(s),
            VE::Undef => None,
            VE::Num => {
                // Phase 1 discovery only. (A local used BOTH as a base and
                // numerically is caught in phase 2: its loads become `Obj`
                // entries there, and any numeric consumer rejects the region.)
                let (l, ver) = self.origins[p]?;
                if *self.versions.get(&l).unwrap_or(&0) != ver {
                    return None;
                }
                let s = match self.discovered.iter().position(|&d| d == l) {
                    Some(s) => s,
                    None => {
                        self.discovered.push(l);
                        self.discovered.len() - 1
                    }
                };
                Some(u16::try_from(s).ok()?)
            }
        }
    }

    /// Within the same basic block starting at region-relative ip `from`, is
    /// local `l` stored again before anything can observe it? Observation =
    /// any op reading `l`, any branch/return-class op, or any branch target
    /// (another block might read it). Conservative and linear.
    fn dead_store_ahead(&self, from: usize, l: u32) -> bool {
        for j in from..self.region.len() {
            if self.is_target[j] {
                return false;
            }
            let op = &self.region[j];
            match op {
                Op::StoreLocal(x) | Op::StoreLocalChecked(x) if *x == l => return true,
                // Ops that read `l` (or copy into it — CopyLocal reads src).
                Op::LoadLocal(x)
                | Op::IncLocalStmt { local: x, .. }
                | Op::AddLocalConst { local: x, .. }
                | Op::ArithLocalConst { local: x, .. }
                | Op::LoadLocalConst { local: x, .. }
                | Op::CmpLocalConstBranchFalse { local: x, .. }
                | Op::CmpLocalConstBranchTrue { local: x, .. }
                    if *x == l =>
                {
                    return false;
                }
                Op::CopyLocal { src, dest } => {
                    if *src == l {
                        return false;
                    }
                    if *dest == l {
                        return true;
                    }
                }
                // Block enders / control transfers end the scan.
                _ if !op_targets(op).is_empty() => return false,
                Op::Return | Op::ReturnUndefined | Op::Throw | Op::LoopKernel(_) => return false,
                _ => {}
            }
        }
        false
    }

    /// Record a branch to absolute bytecode ip `t` from the kop at `kidx`.
    fn branch_to(&mut self, kidx: usize, t: u32, shape: Vec<VE>) -> Option<()> {
        if shape.contains(&VE::Undef) {
            return None; // `undefined` never crosses block boundaries
        }
        let rel = (t as usize).wrapping_sub(self.base_ip as usize);
        if rel < self.region.len() {
            match &self.shape_at[rel] {
                Some(s) if *s != shape => return None,
                Some(_) => {}
                None => self.shape_at[rel] = Some(shape),
            }
            self.fixups.push((kidx, rel));
        } else {
            self.exits.push((kidx, t, shape));
        }
        Some(())
    }

    fn bin(&mut self, kind: ArithKind) -> Option<()> {
        let b = self.pop_num()?;
        let a = self.top_reg(0)?;
        self.kops.push(KOp::Arith { kind, dst: a, a, b });
        self.origins[self.vstack.len() - 1] = None;
        Some(())
    }

    /// Translate one op (region-relative ip `i`); `Op::LoopKernel` recurses
    /// into its fallback. Mutates the virtual stack; returns `None` to reject
    /// the whole region.
    fn emit(&mut self, op: &Op, i: usize) -> Option<()> {
        use KOp as K;
        match op {
            Op::Nop => {}

            // `undefined` may be pushed only as a dead store's operand (see
            // `VE::Undef`); the store arm below validates and elides it.
            Op::LoadUndefined => {
                self.vstack.push(VE::Undef);
                self.origins.push(None);
            }

            // An inner loop's kernel header: translate the op it replaced.
            Op::LoopKernel(idx) => {
                let fb = (*self.inner.get(*idx as usize)?.fallback).clone();
                return self.emit(&fb, i);
            }

            // ---- locals (TDZ subsumed by the entry guard: a mapped local is
            // a Number, never Uninitialized) ----
            Op::LoadLocal(l) => {
                if let Some(pos) = self.oslot_locals.iter().position(|&o| o == *l) {
                    // Phase 2: this local is an array base — push the object.
                    self.vstack.push(VE::Obj(pos as u16));
                    self.origins.push(None);
                } else {
                    let src = self.lreg(*l)?;
                    let dst = self.push_num()?;
                    self.kops.push(K::Mov { dst, src });
                    let ver = *self.versions.get(l).unwrap_or(&0);
                    *self.origins.last_mut()? = Some((*l, ver));
                }
            }
            // The checked store's TDZ test reads the CURRENT slot value; the
            // guard proves it is a Number, so the store is a plain move.
            Op::StoreLocal(l) | Op::StoreLocalChecked(l) => {
                if matches!(self.vstack.last(), Some(VE::Undef)) {
                    // Storing `undefined` to a numeric local can only be
                    // elided if the local is provably RE-STORED before any
                    // read, branch, or branch target — i.e. the store is dead
                    // within this basic block (the per-iteration `let` reset
                    // pattern). Otherwise the region is rejected.
                    self.lreg(*l)?;
                    if !self.dead_store_ahead(i + 1, *l) {
                        return None;
                    }
                    self.pop()?;
                    self.store_of(*l);
                } else {
                    let dst = self.lreg(*l)?;
                    let src = self.pop_num()?;
                    self.kops.push(K::Mov { dst, src });
                    self.store_of(*l);
                }
            }
            Op::CopyLocal { src, dest } => {
                let s = self.lreg(*src)?;
                let d = self.lreg(*dest)?;
                if s != d {
                    self.kops.push(K::Mov { dst: d, src: s });
                }
                self.store_of(*dest);
            }
            Op::LoadConst(c) => {
                let k = self.num_const(*c)?;
                let dst = self.push_num()?;
                self.kops.push(K::Const { dst, k });
            }
            Op::LoadLocalConst { local, konst } => {
                let src = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let dst0 = self.push_num()?;
                self.kops.push(K::Mov { dst: dst0, src });
                let ver = *self.versions.get(local).unwrap_or(&0);
                *self.origins.last_mut()? = Some((*local, ver));
                let dst1 = self.push_num()?;
                self.kops.push(K::Const { dst: dst1, k });
            }

            // ---- stack shuffles (object entries move virtually; numeric
            // VALUES move between position-owned registers) ----
            Op::Pop => {
                self.pop()?;
            }
            Op::Dup => {
                let top = *self.vstack.last()?;
                match top {
                    VE::Num => {
                        let src = self.top_reg(0)?;
                        let dst = self.push_num()?;
                        self.kops.push(K::Mov { dst, src });
                    }
                    VE::Obj(s) => {
                        self.vstack.push(VE::Obj(s));
                        self.origins.push(None);
                    }
                    VE::Undef => return None,
                }
            }
            Op::Swap => {
                let n = self.vstack.len();
                let (pa, pb) = (n.checked_sub(2)?, n - 1);
                let (ea, eb) = (self.vstack[pa], self.vstack[pb]);
                match (ea, eb) {
                    (VE::Undef, _) | (_, VE::Undef) => return None,
                    (VE::Num, VE::Num) => {
                        let (ra, rb) = (self.preg(pa)?, self.preg(pb)?);
                        self.kops.push(K::Mov {
                            dst: SCRATCH0,
                            src: ra,
                        });
                        self.kops.push(K::Mov { dst: ra, src: rb });
                        self.kops.push(K::Mov {
                            dst: rb,
                            src: SCRATCH0,
                        });
                    }
                    (VE::Num, VE::Obj(_)) => {
                        // The number moves from position pa's reg to pb's.
                        let (ra, rb) = (self.preg(pa)?, self.preg(pb)?);
                        self.kops.push(K::Mov { dst: rb, src: ra });
                    }
                    (VE::Obj(_), VE::Num) => {
                        let (ra, rb) = (self.preg(pa)?, self.preg(pb)?);
                        self.kops.push(K::Mov { dst: ra, src: rb });
                    }
                    (VE::Obj(_), VE::Obj(_)) => {}
                }
                self.vstack.swap(pa, pb);
                self.origins.swap(pa, pb);
            }
            Op::Rot3 => {
                // [a b c] -> [c a b] (match the interpreter's Rot3): values
                // rotate downward one position; each Num value moves to its
                // entry's new position register.
                let n = self.vstack.len();
                let (pa, pb, pc_) = (n.checked_sub(3)?, n - 2, n - 1);
                let (ra, rb, rc) = (self.preg(pa)?, self.preg(pb)?, self.preg(pc_)?);
                let (ea, eb, ec) = (self.vstack[pa], self.vstack[pb], self.vstack[pc_]);
                if ea == VE::Undef || eb == VE::Undef || ec == VE::Undef {
                    return None;
                }
                // c -> position a, a -> position b, b -> position c.
                if ec == VE::Num {
                    self.kops.push(K::Mov {
                        dst: SCRATCH0,
                        src: rc,
                    });
                }
                if eb == VE::Num {
                    self.kops.push(K::Mov { dst: rc, src: rb });
                }
                if ea == VE::Num {
                    self.kops.push(K::Mov { dst: rb, src: ra });
                }
                if ec == VE::Num {
                    self.kops.push(K::Mov {
                        dst: ra,
                        src: SCRATCH0,
                    });
                }
                // Rotate the last three entries: [a b c] -> [c a b].
                self.vstack[pa..].rotate_right(1);
                self.origins[pa..].rotate_right(1);
            }

            // ---- arithmetic (numbers in, numbers out) ----
            Op::Add => {
                let b = self.pop_num()?;
                let a = self.top_reg(0)?;
                self.kops.push(K::Add { dst: a, a, b });
                let p = self.vstack.len() - 1;
                self.origins[p] = None;
            }
            Op::Sub => self.bin(ArithKind::Sub)?,
            Op::Mul => self.bin(ArithKind::Mul)?,
            Op::Div => self.bin(ArithKind::Div)?,
            Op::Mod => self.bin(ArithKind::Mod)?,
            Op::Pow => self.bin(ArithKind::Pow)?,
            Op::BitAnd => self.bin(ArithKind::BitAnd)?,
            Op::BitOr => self.bin(ArithKind::BitOr)?,
            Op::BitXor => self.bin(ArithKind::BitXor)?,
            Op::Shl => self.bin(ArithKind::Shl)?,
            Op::Shr => self.bin(ArithKind::Shr)?,
            Op::UShr => self.bin(ArithKind::UShr)?,
            Op::Neg => {
                let s = self.top_reg(0)?;
                self.kops.push(K::Neg { dst: s, src: s });
                let p = self.vstack.len() - 1;
                self.origins[p] = None;
            }
            Op::BitNot => {
                let s = self.top_reg(0)?;
                self.kops.push(K::BitNot { dst: s, src: s });
                let p = self.vstack.len() - 1;
                self.origins[p] = None;
            }
            // ToNumber/ToNumeric on a Number is the identity — but only on a
            // NUMBER entry (an object would coerce: reject).
            Op::Pos | Op::ToNumeric => {
                self.top_reg(0)?;
            }
            Op::Inc | Op::Dec => {
                let s = self.top_reg(0)?;
                self.kops.push(K::AddK {
                    dst: s,
                    a: s,
                    k: if matches!(op, Op::Inc) { 1.0 } else { -1.0 },
                });
                let p = self.vstack.len() - 1;
                self.origins[p] = None;
            }

            // ---- fused local-const forms ----
            Op::AddLocalConst { local, konst } => {
                let a = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let dst = self.push_num()?;
                self.kops.push(K::AddK { dst, a, k });
            }
            Op::ArithLocalConst { local, konst, kind } => {
                let a = self.lreg(*local)?;
                let k = self.num_const(*konst)?;
                let dst = self.push_num()?;
                self.kops.push(K::ArithK {
                    kind: *kind,
                    dst,
                    a,
                    k,
                });
            }
            Op::IncLocalStmt { local, dec } => {
                let r = self.lreg(*local)?;
                self.kops.push(K::AddK {
                    dst: r,
                    a: r,
                    k: if *dec { -1.0 } else { 1.0 },
                });
                self.store_of(*local);
            }

            // ---- dense-array access ----
            // `a[i]` read: base must be an array-base entry, index numeric.
            Op::GetPropDynamic => {
                // Shape AT the op (before popping) is what the generic op
                // expects on a bail.
                let shape = self.vstack.clone();
                let obj = self.base_slot(1)?;
                let idx = self.top_reg(0)?;
                self.pop()?; // idx
                self.pop()?; // base
                let dst = self.push_num()?;
                let kidx = self.kops.len();
                self.kops.push(K::LoadElem {
                    dst,
                    obj,
                    idx,
                    bail: u16::MAX,
                });
                self.exits.push((kidx, self.base_ip + i as u32, shape));
            }
            // `a[i] = v` write: pushes the value back (expression result).
            Op::SetPropDynamic => {
                let shape = self.vstack.clone();
                let obj = self.base_slot(2)?;
                let val = self.top_reg(0)?;
                let idx = self.top_reg(1)?;
                self.pop()?; // val
                self.pop()?; // idx
                self.pop()?; // base
                let dst = self.push_num()?;
                let kidx = self.kops.len();
                self.kops.push(K::StoreElem {
                    obj,
                    idx,
                    val,
                    bail: u16::MAX,
                });
                self.exits.push((kidx, self.base_ip + i as u32, shape));
                // The op's result is the assigned value.
                if dst != val {
                    self.kops.push(K::Mov { dst, src: val });
                }
            }
            // `a.length` (the only named property access supported).
            Op::GetProp(c) => {
                match self.consts.get(*c as usize)? {
                    Const::String(s) if s.as_str() == "length" => {}
                    _ => return None,
                }
                let shape = self.vstack.clone();
                let obj = self.base_slot(0)?;
                self.pop()?; // base
                let dst = self.push_num()?;
                let kidx = self.kops.len();
                self.kops.push(K::LoadLen {
                    dst,
                    obj,
                    bail: u16::MAX,
                });
                self.exits.push((kidx, self.base_ip + i as u32, shape));
            }

            // ---- comparisons: supported only in compare-and-branch form. A
            // materialized boolean (stored, returned, fed to arithmetic)
            // would put a non-number on the stack — reject the region.
            Op::Eq | Op::Ne | Op::StrictEq | Op::StrictNe | Op::Lt | Op::Gt | Op::Le | Op::Ge => {
                let cmp = match op {
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
                let b = self.pop_num()?;
                let a = self.pop_num()?;
                let kidx = self.kops.len();
                self.kops.push(K::BrCmp {
                    cmp,
                    a,
                    b,
                    if_true,
                    target: u16::MAX,
                });
                self.branch_to(kidx, target, self.vstack.clone())?;
                self.absorbed = Some(i + 1);
            }

            // ---- branches ----
            Op::Jump(t) => {
                let kidx = self.kops.len();
                self.kops.push(K::Br { target: u16::MAX });
                self.branch_to(kidx, *t, self.vstack.clone())?;
            }
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                let src = self.pop_num()?;
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
                self.branch_to(kidx, *t, self.vstack.clone())?;
            }
            // Peek variants (`&&`/`||` short-circuits): the TAKEN edge keeps
            // the tested value on the stack; the fallthrough edge pops it.
            Op::JumpIfFalsyPeek(t) | Op::JumpIfTruthyPeek(t) => {
                let src = self.top_reg(0)?;
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
                self.branch_to(kidx, *t, self.vstack.clone())?;
                self.pop()?;
            }
            // A Number is never nullish. `JumpIfNullishPeek` jumps on NOT
            // nullish keeping the value — for kernel-typed values that is an
            // unconditional jump (the pop-and-fall-through edge is the nullish
            // case, unreachable here).
            Op::JumpIfNullishPeek(t) => {
                self.top_reg(0)?;
                let kidx = self.kops.len();
                self.kops.push(K::Br { target: u16::MAX });
                self.branch_to(kidx, *t, self.vstack.clone())?;
            }
            // `JumpIfNullish` jumps on nullish (replacing the top with
            // `undefined`) — never taken for kernel-typed values: no-op.
            Op::JumpIfNullish(_) => {
                self.top_reg(0)?;
            }

            Op::CmpBranchFalse { cmp, target } | Op::CmpBranchTrue { cmp, target } => {
                let if_true = matches!(op, Op::CmpBranchTrue { .. });
                let b = self.pop_num()?;
                let a = self.pop_num()?;
                let kidx = self.kops.len();
                self.kops.push(K::BrCmp {
                    cmp: *cmp,
                    a,
                    b,
                    if_true,
                    target: u16::MAX,
                });
                self.branch_to(kidx, *target, self.vstack.clone())?;
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
                self.branch_to(kidx, *target, self.vstack.clone())?;
            }

            // Anything else — calls, other property access, cells, upvalues,
            // TDZ init, globals, objects, strings, try/dispose/iterator/
            // suspend machinery — rejects the region.
            _ => return None,
        }
        Some(())
    }
}
