//! Typed loop kernels: translate eligible bytecode loops into unboxed-`f64`
//! register programs (docs/js-performance-roadmap.md §6.5).
//!
//! The same translator also compiles FUNCTION kernels
//! ([`kernelize_function`]): a tiny pure-scalar body — a sort comparator, a
//! `map`/`filter`/`reduce` callback — becomes a register program the call
//! paths execute FRAMELESS when its per-call entry guard passes (arguments
//! present and `Number`s, upvalues `Number`s, no op budget, no trace sink);
//! any guard failure takes the ordinary frame path. See `Vm::run_fn_kernel`.
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

use crate::bytecode::{CmpOp, Const, KCallee, KMath, KOp, KProp, KShapeSlot, KSlot, Kernel, Op};
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
/// Provisional register space for BOOLEAN-typed locals (numeric locals are
/// `0..BOOL_BASE`, boolean locals `BOOL_BASE..STACK_BASE`); compacted by the
/// final remap to sit right after the numeric locals.
const BOOL_BASE: u16 = 32;
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
        // Iterate translation to a FIXPOINT over the discovered local types:
        // array bases (`a` in `a[i]`) become object slots, and locals that
        // receive boolean stores become Bool-typed registers. Each run feeds
        // its discoveries into the next; a stable run whose translation
        // succeeded installs the kernel. (Discovery is monotone over a
        // bounded set, so the cap is belt-and-braces.)
        let mut oslots: Vec<u32> = Vec::new();
        let mut bools: Vec<u32> = Vec::new();
        let mut installed: Option<Kernel> = None;
        for _ in 0..6 {
            let (k, d_obj, d_bool) = translate(
                &code[start..=end],
                start as u32,
                consts,
                &kernels,
                &oslots,
                &bools,
                false,
                None,
            );
            let mut grew = false;
            for o in d_obj {
                if !oslots.contains(&o) {
                    oslots.push(o);
                    grew = true;
                }
            }
            for b in d_bool {
                if !bools.contains(&b) {
                    bools.push(b);
                    grew = true;
                }
            }
            if !grew {
                installed = k;
                break;
            }
        }
        if let Some(mut k) = installed {
            k.fallback = Box::new(code[start].clone());
            code[start] = Op::LoopKernel(kernels.len() as u32);
            kernels.push(k);
        }
    }
    (code, kernels)
}

/// Bound keeping FUNCTION-kernel translation cheap: bodies beyond this stay
/// generic (the win is tiny leaf callbacks — comparators, HOF callbacks;
/// larger bodies amortize their frame anyway).
const MAX_FN_OPS: usize = 128;

/// Translate an ENTIRE function body into a kernel executed FRAMELESS at call
/// time (`FuncProto::fn_kernel`): no frame, no operand stack, no local slots —
/// arguments load straight into registers under the entry guard (every
/// consumed argument present and a `Number`, captured upvalues `Number`s) and
/// `Return` yields the call's result directly. Eligibility is the loop-kernel
/// allowlist plus fn-mode rules: no element access or `a.length` (a bail
/// needs a frame to resume into), every local read dominated by a real store
/// on every path (there is no guard to type frameless locals), and every
/// completing path ending at `Return` with a scalar. `None` = stay generic.
pub fn kernelize_function(
    code: &[Op],
    consts: &[Const],
    inner: &[Kernel],
    self_name: Option<&str>,
) -> Option<Kernel> {
    if code.len() > MAX_FN_OPS {
        return None;
    }
    // Cheap pre-screen: a kernelizable body always ends paths at `Op::Return`.
    if !code.iter().any(|op| matches!(op, Op::Return)) {
        return None;
    }
    // Same boolean-local fixpoint as `kernelize`; array-base discoveries
    // reject outright (element access can't run frameless).
    let mut bools: Vec<u32> = Vec::new();
    for _ in 0..6 {
        let (k, d_obj, d_bool) = translate(code, 0, consts, inner, &[], &bools, true, self_name);
        if !d_obj.is_empty() {
            return None;
        }
        let mut grew = false;
        for b in d_bool {
            if !bools.contains(&b) {
                bools.push(b);
                grew = true;
            }
        }
        if !grew {
            let mut k = k?;
            // Frameless kernels are self-contained by construction.
            debug_assert!(k.oslots.is_empty() && k.shapes.is_empty());
            if k.code.iter().any(|op| matches!(op, KOp::SelfCall { .. })) {
                // RECURSIVE kernel. Every self-call must supply every
                // argument the body consumes (a short call would need the
                // generic `undefined` parameter), and every return must be
                // a NUMBER (a boolean result would land in a caller
                // register statically typed Num — typeof/strict-eq would
                // diverge). Either miss keeps the whole function generic.
                let args_used = k
                    .locals
                    .iter()
                    .filter_map(|sl| match sl {
                        KSlot::Arg(a) => Some(*a + 1),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                for op in k.code.iter() {
                    match *op {
                        KOp::SelfCall { argc, .. } if u32::from(argc) < args_used => return None,
                        KOp::Ret { boolean: true, .. } => return None,
                        _ => {}
                    }
                }
                k.self_global = Some(self_name?.into());
            }
            return Some(k);
        }
    }
    None
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
    /// A value statically known BOOLEAN, in the entry's position register as
    /// exactly 0.0/1.0. Coercing consumers (arithmetic, branches, Math args)
    /// read the register raw — identical to ToNumber/ToBoolean on a boolean —
    /// but exits materialize `Value::Bool`, array indices/elements refuse it
    /// (`a[true]` is the property "true"!), and strict equality against a
    /// statically-Number operand folds to a constant.
    Bool,
    /// The canonical `Math` object, from `LoadGlobal("Math")` — speculative:
    /// the entry guard verifies the global binding still IS the canonical
    /// object (a data property, not shadowed/replaced/accessor'd) before the
    /// kernel runs; otherwise the whole kernel declines.
    MathObj,
    /// A kernel-supported `Math` method, from `GetProp` on [`VE::MathObj`].
    MathFn(KMath),
    /// The value `undefined`, from `LoadUndefined`. Exists only transiently
    /// within a basic block: its ONLY legal consumer is a store to a local
    /// that is provably re-stored before any read (the compiler's
    /// per-iteration `let` reset emits `LoadUndefined; StoreLocal(j)` right
    /// before the loop re-initializes `j`). Everything else rejects.
    Undef,
    /// fn mode only: a value the kernel cannot represent — `this` /
    /// `new.target`, which a declared function's calling-convention prologue
    /// materializes into locals the body may never touch. Its ONLY legal
    /// consumer is a store to a local (the `init` tracking then rejects any
    /// read of that local); everything else rejects.
    Opaque,
    /// fn mode only: the function ITSELF, from `LoadGlobal` of its own name
    /// — speculative: the entry guard verifies the global binding still
    /// holds the very closure being invoked. Its ONLY legal consumer is a
    /// `Call` (which fuses to [`KOp::SelfCall`]); everything else rejects.
    SelfFn,
}

struct Xlate<'a> {
    region: &'a [Op],
    base_ip: u32,
    consts: &'a [Const],
    /// Already-built kernels (inner loops) — `Op::LoopKernel` translates as
    /// its fallback op.
    inner: &'a [Kernel],
    /// FUNCTION-kernel translation (`kernelize_function`): the region is an
    /// entire function body executed FRAMELESS. `Op::LoadArg`/`Op::Return`
    /// join the allowlist; exits are impossible (no frame to resume), and
    /// every local read must be dominated by a real store (`init` tracking) —
    /// there is no entry guard over locals to type them.
    fn_mode: bool,
    /// fn mode: the function's own name — `LoadGlobal` of it becomes
    /// [`VE::SelfFn`] and a direct recursive call fuses to `KOp::SelfCall`.
    self_name: Option<&'a str>,
    /// Locals designated as array bases (phase 2) — empty in phase 1.
    oslot_locals: &'a [u32],
    /// Locals statically typed BOOLEAN (fixpoint-discovered from stores).
    bool_locals: &'a [u32],
    kops: Vec<KOp>,
    /// kernel pc for each region-relative ip (`u16::MAX` = not emitted).
    kpc: Vec<u16>,
    /// expected virtual-stack shape at each region-relative ip, when known.
    shape_at: Vec<Option<Vec<VE>>>,
    /// fn mode: the set of locals provably stored on the current path,
    /// SORTED. Merge points require identical sets — the same rule as the
    /// stack shape — so a read is dominated by a store on EVERY path.
    init: Vec<u32>,
    /// fn mode: expected `init` set at each region-relative ip, when known
    /// (kept in lockstep with `shape_at`).
    init_at: Vec<Option<Vec<u32>>>,
    /// in-region branch targets (any op branching to that ip).
    is_target: Vec<bool>,
    /// numeric slots (locals and read-only upvalues): (source, register),
    /// registers dense from 0.
    local_reg: Vec<(KSlot, u16)>,
    /// boolean locals: (frame-local index, register from BOOL_BASE).
    bool_reg: Vec<(u32, u16)>,
    /// phase 1: locals observed as array bases (with clean origins).
    discovered: Vec<u32>,
    /// locals observed receiving BOOLEAN stores (fixpoint feedback).
    discovered_bools: Vec<u32>,
    /// phase 1 origin tracking: (local, version) of a pushed LoadLocal.
    origins: Vec<Option<(u32, u32)>>,
    versions: std::collections::HashMap<u32, u32>,
    /// the current virtual stack.
    vstack: Vec<VE>,
    /// pending in-region branch fixups: (kop index, region-relative target).
    fixups: Vec<(usize, usize)>,
    /// pending exits: (kop index or LoadElem-style bail, resume ip, shape).
    exits: Vec<(usize, u32, Vec<VE>)>,
    /// Math intrinsics used (entry guard checks each against the realm).
    math_used: Vec<KMath>,
    /// Named-property access classes over oslot bases (entry-resolved; see
    /// [`KProp`]). Deduplicated by (oslot, key); flags OR together.
    props_used: Vec<KProp>,
    /// Pinned closure callees (loop mode; see [`KCallee`]): per oslot, the
    /// smallest argc any call site supplies.
    callees: Vec<KCallee>,
    /// a compare op fused with its following conditional jump: skip that ip.
    absorbed: Option<usize>,
    max_stack: u16,
}

/// Attempt to translate a region. Returns the kernel (fallback is a
/// placeholder; `None` on ANY construct outside the allowlist) plus whatever
/// array-base and boolean-local discoveries were made up to the point of
/// success or failure — the caller iterates those to a fixpoint.
// The parameters ARE the pass's inputs (region + the two fixpoint sets +
// the two mode toggles); a bundling struct would just rename the call sites.
#[allow(clippy::too_many_arguments)]
fn translate(
    region: &[Op],
    base_ip: u32,
    consts: &[Const],
    inner: &[Kernel],
    oslot_locals: &[u32],
    bool_locals: &[u32],
    fn_mode: bool,
    self_name: Option<&str>,
) -> (Option<Kernel>, Vec<u32>, Vec<u32>) {
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
        fn_mode,
        self_name,
        oslot_locals,
        bool_locals,
        kops: Vec::new(),
        kpc: vec![u16::MAX; region.len()],
        shape_at: vec![None; region.len()],
        init: Vec::new(),
        init_at: vec![None; region.len()],
        is_target,
        local_reg: Vec::new(),
        bool_reg: Vec::new(),
        discovered: Vec::new(),
        discovered_bools: Vec::new(),
        origins: Vec::new(),
        versions: std::collections::HashMap::new(),
        vstack: Vec::new(),
        fixups: Vec::new(),
        exits: Vec::new(),
        math_used: Vec::new(),
        props_used: Vec::new(),
        callees: Vec::new(),
        absorbed: None,
        max_stack: 0,
    };
    x.shape_at[0] = Some(Vec::new());
    x.init_at[0] = Some(Vec::new());
    let kernel = translate_inner(&mut x);
    let d_obj = std::mem::take(&mut x.discovered);
    let d_bool = std::mem::take(&mut x.discovered_bools);
    (kernel, d_obj, d_bool)
}

fn translate_inner(x: &mut Xlate) -> Option<Kernel> {
    let region = x.region;
    let base_ip = x.base_ip;

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
                if x.fn_mode {
                    // The initialized-locals set merges under the same rule.
                    let init = x.init_at[i].clone().unwrap_or_default();
                    if reachable && init != x.init {
                        return None;
                    }
                    if !reachable {
                        x.init = init;
                    }
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
                if x.fn_mode {
                    x.init_at[i] = Some(x.init.clone());
                }
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
        if matches!(region[i], Op::Jump(_) | Op::JumpIfNullishPeek(_))
            || (x.fn_mode && matches!(region[i], Op::Return))
        {
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
    // A FUNCTION kernel runs frameless — there is no bytecode frame to
    // resume, so any path needing a generic-interpreter exit rejects the
    // whole function (element access, or a body not ending in `Return`).
    if x.fn_mode && !x.exits.is_empty() {
        return None;
    }
    // Synthesize exit stubs (deduplicated by resume ip + shape) and collect
    // the shape table.
    let exits = std::mem::take(&mut x.exits);
    let mut shapes: Vec<Vec<VE>> = Vec::new();
    let mut stubs: Vec<(u32, u16, u16)> = Vec::new(); // (resume, shape idx, pc)
    for (kidx, resume_ip, shape) in exits {
        if shape
            .iter()
            .any(|e| matches!(e, VE::Undef | VE::Opaque | VE::SelfFn))
        {
            return None; // undefined/opaque/self never survives to an exit
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

    // Compact the register file: numeric locals stay 0..n, boolean locals
    // follow at n.., provisional stack position regs after those, scratch at
    // the top.
    let n_locals = u16::try_from(x.local_reg.len()).ok()?;
    let n_bools = u16::try_from(x.bool_reg.len()).ok()?;
    let remap = |r: u16| -> u16 {
        if r < BOOL_BASE {
            r
        } else if r < STACK_BASE {
            n_locals + (r - BOOL_BASE)
        } else if r == SCRATCH0 {
            n_locals + n_bools + x.max_stack
        } else {
            n_locals + n_bools + (r - STACK_BASE)
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
            KOp::CmpSet { dst, a, b, .. } => {
                *dst = remap(*dst);
                *a = remap(*a);
                *b = remap(*b);
            }
            KOp::BoolNot { dst, src } => {
                *dst = remap(*dst);
                *src = remap(*src);
            }
            KOp::LoadElem { dst, idx, .. } => {
                *dst = remap(*dst);
                *idx = remap(*idx);
            }
            KOp::StoreElem { idx, val, .. } => {
                *idx = remap(*idx);
                *val = remap(*val);
            }
            KOp::LoadLen { dst, .. } => *dst = remap(*dst),
            KOp::LoadProp { dst, .. } => *dst = remap(*dst),
            KOp::StoreProp { src, .. } => *src = remap(*src),
            KOp::CallKernel { dst, base, .. } => {
                *dst = remap(*dst);
                *base = remap(*base);
            }
            KOp::Ret { src, .. } => *src = remap(*src),
            KOp::SelfCall { dst, base, .. } => {
                *dst = remap(*dst);
                *base = remap(*base);
            }
            KOp::Math1 { dst, src, .. } => {
                *dst = remap(*dst);
                *src = remap(*src);
            }
            KOp::Math2 { dst, a, b, .. } => {
                *dst = remap(*dst);
                *a = remap(*a);
                *b = remap(*b);
            }
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
                    VE::Bool => KShapeSlot::Bool(remap(STACK_BASE + p as u16)),
                    VE::Obj(o) => KShapeSlot::Obj(*o),
                    VE::MathObj => KShapeSlot::MathObj,
                    VE::MathFn(k) => KShapeSlot::MathFn(*k),
                    VE::Undef | VE::Opaque | VE::SelfFn => {
                        unreachable!("undef/opaque/self never crosses block boundaries")
                    }
                })
                .collect()
        })
        .collect();

    let mut locals: Vec<KSlot> = vec![KSlot::Local(0); n_locals as usize];
    for &(sl, r) in &x.local_reg {
        locals[r as usize] = sl;
    }
    let mut bool_locals: Vec<u32> = vec![0; n_bools as usize];
    for &(l, r) in &x.bool_reg {
        bool_locals[(r - BOOL_BASE) as usize] = l;
    }
    // Post-translation cleanup (copy-prop + dead-Mov DCE, see
    // `cleanup_kops`). The always-live set is everything observed outside
    // straight-line execution: loop kernels write mapped Local and bool
    // registers back to the frame on every exit and interrupt unwind, and
    // exit shapes reference stack registers. Function kernels are frameless
    // and pure — only `Ret` (a normal use) and the upvalue-window copies a
    // `SelfCall` performs observe registers.
    {
        let mut always_live: u128 = 0;
        let mut upvalue_uses: u128 = 0;
        let n_regs = (n_locals + n_bools + x.max_stack + 1) as usize;
        if n_regs <= 128 {
            if x.fn_mode {
                for (r, slot) in locals.iter().enumerate() {
                    if matches!(slot, KSlot::Upvalue(_)) {
                        upvalue_uses |= 1 << r;
                    }
                }
            } else {
                for (r, slot) in locals.iter().enumerate() {
                    if matches!(slot, KSlot::Local(_)) {
                        always_live |= 1 << r;
                    }
                }
                for j in 0..n_bools {
                    always_live |= 1 << (n_locals + j);
                }
                for s in &shapes {
                    for e in s.iter() {
                        if let KShapeSlot::Num(r) | KShapeSlot::Bool(r) = e {
                            always_live |= 1 << *r;
                        }
                    }
                }
            }
            cleanup_kops(&mut x.kops, always_live, upvalue_uses, n_regs);
        }
    }
    let stores_elems = x.kops.iter().any(|op| matches!(op, KOp::StoreElem { .. }));
    Some(Kernel {
        stores_elems,
        code: std::mem::take(&mut x.kops).into_boxed_slice(),
        locals: locals.into_boxed_slice(),
        bool_locals: bool_locals.into_boxed_slice(),
        oslots: x.oslot_locals.to_vec().into_boxed_slice(),
        shapes: shapes.into_boxed_slice(),
        math_used: std::mem::take(&mut x.math_used).into_boxed_slice(),
        props_used: std::mem::take(&mut x.props_used).into_boxed_slice(),
        callee_slots: std::mem::take(&mut x.callees).into_boxed_slice(),
        n_regs: n_locals + n_bools + x.max_stack + 1,
        self_global: None, // `kernelize_function` fills it for recursive kernels
        fallback: Box::new(Op::Nop), // caller stores the real header op
    })
}

/// Statically-decidable comparison outcome: strict (in)equality between a
/// boolean-typed and a number-typed operand is `false`/`true` regardless of
/// values (the generic `strict_equals` checks the type tag first). All other
/// combinations are dynamic — raw register comparison matches the generic
/// numeric coercions exactly.
fn static_cmp(cmp: CmpOp, a_bool: bool, b_bool: bool) -> Option<bool> {
    if a_bool == b_bool {
        return None;
    }
    match cmp {
        CmpOp::StrictEq => Some(false),
        CmpOp::StrictNe => Some(true),
        _ => None,
    }
}

/// Rewrite every branch/bail target in `op` through `f`.
fn map_targets(op: &mut KOp, mut f: impl FnMut(u16) -> u16) {
    match op {
        KOp::Br { target }
        | KOp::BrCmp { target, .. }
        | KOp::BrCmpK { target, .. }
        | KOp::BrFalsy { target, .. }
        | KOp::BrTruthy { target, .. } => *target = f(*target),
        KOp::LoadElem { bail, .. } | KOp::StoreElem { bail, .. } | KOp::LoadLen { bail, .. } => {
            *bail = f(*bail)
        }
        _ => {}
    }
}

/// Post-translation cleanup over the finished KOp array: forward
/// copy-propagation plus dead-`Mov` elimination with real liveness over the
/// kernel's tiny CFG. The stack-machine lowering routes nearly every value
/// through a canonical stack register, so `Mov`s dominate hot kernels — the
/// arith-loop body is 13 ops of which 5 are `Mov`s, and a `(x, y) => x - y`
/// comparator is 6 `Mov`s around one `Arith`. Purely register-level: every
/// surviving op reads the same VALUES and writes the same results as
/// before, so kernel output stays bit-identical to the generic path.
///
/// `always_live` marks registers observed outside straight-line execution —
/// loop kernels write mapped `Local`/bool registers back to the frame on
/// every exit AND on interrupt unwinds at back-edge polls, and exit shapes
/// reference stack registers — so writes to them are never dead.
/// `upvalue_uses` marks the upvalue-slot registers a `SelfCall` implicitly
/// copies into the callee window (function kernels).
fn cleanup_kops(kops: &mut Vec<KOp>, always_live: u128, upvalue_uses: u128, n_regs: usize) {
    if n_regs > 128 {
        return;
    }
    // Alternate the two transforms to a fixpoint: propagation exposes dead
    // copies; deletion shortens chains for the next round. Each round
    // strictly reduces rewrites+ops, so this terminates quickly.
    loop {
        let mut changed = copy_prop_kops(kops);
        changed |= dce_movs(kops, always_live, upvalue_uses, n_regs);
        if !changed {
            break;
        }
    }
}

/// Forward copy propagation within basic blocks: after `Mov d, s`, reads of
/// `d` are rewritten to `s` until either register is written. The map is
/// cleared at every branch/bail target (join points may disagree) — the
/// per-block window is enough for the lowering's `local → stack-slot → use`
/// shuttles. Range reads (`SelfCall`/`CallKernel` argument windows) are
/// never rewritten: the callee reads those exact registers.
fn copy_prop_kops(kops: &mut [KOp]) -> bool {
    let n = kops.len();
    let mut label = vec![false; n];
    let mut preds = vec![0u32; n];
    for (j, op) in kops.iter_mut().enumerate() {
        let falls = !matches!(op, KOp::Br { .. } | KOp::Ret { .. } | KOp::Exit { .. });
        if falls && j + 1 < n {
            preds[j + 1] += 1;
        }
        map_targets(op, |t| {
            label[t as usize] = true;
            preds[t as usize] += 1;
            t
        });
    }
    // copy[d] = Some(s): regs[d] currently equals regs[s], `s` itself a root.
    let mut copy: Vec<Option<u16>> = vec![None; 1 + kops.iter().fold(0, max_reg) as usize];
    // A FORWARD branch to a single-predecessor target carries its copy map
    // to that target (branch ops write nothing, so the state at the branch
    // IS the state at the target) — the if/else join shape. Everything else
    // clears at the label (a join's predecessors may disagree).
    let mut snapshots: Vec<Option<Vec<Option<u16>>>> = vec![None; n];
    let mut changed = false;
    macro_rules! resolve {
        ($r:expr) => {
            if let Some(root) = copy[*$r as usize] {
                if root != *$r {
                    *$r = root;
                    changed = true;
                }
            }
        };
    }
    macro_rules! kill {
        ($d:expr) => {{
            let d = $d;
            copy[d as usize] = None;
            for e in copy.iter_mut() {
                if *e == Some(d) {
                    *e = None;
                }
            }
        }};
    }
    for (i, op) in kops.iter_mut().enumerate() {
        if label[i] {
            match snapshots[i].take() {
                Some(s) => copy = s,
                None => copy.iter_mut().for_each(|e| *e = None),
            }
        }
        match op {
            KOp::Mov { dst, src } => {
                resolve!(src);
                let (d, s) = (*dst, *src);
                kill!(d);
                if d != s {
                    copy[d as usize] = Some(s);
                }
            }
            KOp::Const { dst, .. } => kill!(*dst),
            KOp::Add { dst, a, b } | KOp::Arith { dst, a, b, .. } => {
                resolve!(a);
                resolve!(b);
                kill!(*dst);
            }
            KOp::AddK { dst, a, .. } | KOp::ArithK { dst, a, .. } => {
                resolve!(a);
                kill!(*dst);
            }
            KOp::Neg { dst, src } | KOp::BitNot { dst, src } | KOp::BoolNot { dst, src } => {
                resolve!(src);
                kill!(*dst);
            }
            KOp::CmpSet { dst, a, b, .. } => {
                resolve!(a);
                resolve!(b);
                kill!(*dst);
            }
            KOp::Math1 { dst, src, .. } => {
                resolve!(src);
                kill!(*dst);
            }
            KOp::Math2 { dst, a, b, .. } => {
                resolve!(a);
                resolve!(b);
                kill!(*dst);
            }
            KOp::BrCmp { a, b, .. } => {
                resolve!(a);
                resolve!(b);
            }
            KOp::BrCmpK { a, .. } => resolve!(a),
            KOp::BrFalsy { src, .. } | KOp::BrTruthy { src, .. } => resolve!(src),
            KOp::Ret { src, .. } => resolve!(src),
            KOp::LoadElem { dst, idx, .. } => {
                resolve!(idx);
                kill!(*dst);
            }
            KOp::StoreElem { idx, val, .. } => {
                resolve!(idx);
                resolve!(val);
            }
            KOp::LoadLen { dst, .. } | KOp::LoadProp { dst, .. } => kill!(*dst),
            KOp::StoreProp { src, .. } => resolve!(src),
            // Argument-window RANGE reads: leave the window registers alone,
            // only the result register is a plain def.
            KOp::CallKernel { dst, .. } | KOp::SelfCall { dst, .. } => kill!(*dst),
            KOp::Br { .. } | KOp::Exit { .. } => {}
        }
        let target = match op {
            KOp::Br { target }
            | KOp::BrCmp { target, .. }
            | KOp::BrCmpK { target, .. }
            | KOp::BrFalsy { target, .. }
            | KOp::BrTruthy { target, .. } => Some(*target),
            _ => None,
        };
        if let Some(t) = target {
            if t as usize > i && preds[t as usize] == 1 {
                snapshots[t as usize] = Some(copy.clone());
            }
        }
    }
    changed
}

/// Highest register index referenced by `op` (fold seed for sizing).
fn max_reg(acc: u16, op: &KOp) -> u16 {
    let mut m = acc;
    let mut see = |r: u16| m = m.max(r);
    match op {
        KOp::Mov { dst, src }
        | KOp::Neg { dst, src }
        | KOp::BitNot { dst, src }
        | KOp::BoolNot { dst, src }
        | KOp::Math1 { dst, src, .. } => {
            see(*dst);
            see(*src);
        }
        KOp::Const { dst, .. } | KOp::LoadLen { dst, .. } | KOp::LoadProp { dst, .. } => see(*dst),
        KOp::Add { dst, a, b }
        | KOp::Arith { dst, a, b, .. }
        | KOp::CmpSet { dst, a, b, .. }
        | KOp::Math2 { dst, a, b, .. } => {
            see(*dst);
            see(*a);
            see(*b);
        }
        KOp::AddK { dst, a, .. } | KOp::ArithK { dst, a, .. } => {
            see(*dst);
            see(*a);
        }
        KOp::BrCmp { a, b, .. } => {
            see(*a);
            see(*b);
        }
        KOp::BrCmpK { a, .. } => see(*a),
        KOp::BrFalsy { src, .. } | KOp::BrTruthy { src, .. } | KOp::Ret { src, .. } => see(*src),
        KOp::LoadElem { dst, idx, .. } => {
            see(*dst);
            see(*idx);
        }
        KOp::StoreElem { idx, val, .. } => {
            see(*idx);
            see(*val);
        }
        KOp::StoreProp { src, .. } => see(*src),
        KOp::CallKernel {
            dst, base, argc, ..
        }
        | KOp::SelfCall {
            dst, base, argc, ..
        } => {
            see(*dst);
            see(base + argc);
        }
        KOp::Br { .. } | KOp::Exit { .. } => {}
    }
    m
}

/// Delete `Mov`s whose destination is dead: backward liveness to a fixpoint
/// over the kernel CFG (fall-throughs, branch targets, bail edges), then
/// index compaction with branch retargeting. A branch INTO a deleted `Mov`
/// lands on the next surviving op — sound precisely because the deleted op
/// wrote a register nothing observes.
fn dce_movs(kops: &mut Vec<KOp>, always_live: u128, upvalue_uses: u128, n_regs: usize) -> bool {
    let n = kops.len();
    if n == 0 || n_regs > 128 {
        return false;
    }
    let bit = |r: u16| 1u128 << r;
    // Per-op use/def masks and successor edges.
    let mut uses = vec![0u128; n];
    let mut defs = vec![0u128; n];
    // (fall-through?, branch target)
    let mut succ: Vec<(bool, Option<u16>)> = vec![(true, None); n];
    for (i, op) in kops.iter().enumerate() {
        match op {
            KOp::Mov { dst, src }
            | KOp::Neg { dst, src }
            | KOp::BitNot { dst, src }
            | KOp::BoolNot { dst, src }
            | KOp::Math1 { dst, src, .. } => {
                uses[i] = bit(*src);
                defs[i] = bit(*dst);
            }
            KOp::Const { dst, .. } => defs[i] = bit(*dst),
            KOp::Add { dst, a, b }
            | KOp::Arith { dst, a, b, .. }
            | KOp::CmpSet { dst, a, b, .. }
            | KOp::Math2 { dst, a, b, .. } => {
                uses[i] = bit(*a) | bit(*b);
                defs[i] = bit(*dst);
            }
            KOp::AddK { dst, a, .. } | KOp::ArithK { dst, a, .. } => {
                uses[i] = bit(*a);
                defs[i] = bit(*dst);
            }
            KOp::Br { target } => succ[i] = (false, Some(*target)),
            KOp::BrCmp { a, b, target, .. } => {
                uses[i] = bit(*a) | bit(*b);
                succ[i] = (true, Some(*target));
            }
            KOp::BrCmpK { a, target, .. } => {
                uses[i] = bit(*a);
                succ[i] = (true, Some(*target));
            }
            KOp::BrFalsy { src, target } | KOp::BrTruthy { src, target } => {
                uses[i] = bit(*src);
                succ[i] = (true, Some(*target));
            }
            KOp::Ret { src, .. } => {
                uses[i] = bit(*src);
                succ[i] = (false, None);
            }
            KOp::Exit { .. } => succ[i] = (false, None),
            KOp::LoadElem { dst, idx, bail, .. } => {
                uses[i] = bit(*idx);
                defs[i] = bit(*dst);
                succ[i] = (true, Some(*bail));
            }
            KOp::StoreElem { idx, val, bail, .. } => {
                uses[i] = bit(*idx) | bit(*val);
                succ[i] = (true, Some(*bail));
            }
            KOp::LoadLen { dst, bail, .. } => {
                defs[i] = bit(*dst);
                succ[i] = (true, Some(*bail));
            }
            KOp::LoadProp { dst, .. } => defs[i] = bit(*dst),
            KOp::StoreProp { src, .. } => uses[i] = bit(*src),
            // The executor copies the argument window (and, for SelfCall,
            // every upvalue-slot register) into the callee window.
            KOp::CallKernel {
                dst, base, argc, ..
            } => {
                for r in *base..base + argc {
                    uses[i] |= bit(r);
                }
                defs[i] = bit(*dst);
            }
            KOp::SelfCall {
                dst, base, argc, ..
            } => {
                for r in *base..base + argc {
                    uses[i] |= bit(r);
                }
                uses[i] |= upvalue_uses;
                defs[i] = bit(*dst);
            }
        }
    }
    // Backward liveness to a fixpoint (tiny op counts; converges in a few
    // sweeps).
    let mut live_in = vec![0u128; n];
    let mut live_out = vec![0u128; n];
    loop {
        let mut changed = false;
        for i in (0..n).rev() {
            let mut out = 0u128;
            let (fall, target) = succ[i];
            if fall && i + 1 < n {
                out |= live_in[i + 1];
            }
            if let Some(t) = target {
                out |= live_in[t as usize];
            }
            let inn = uses[i] | (out & !defs[i]);
            if out != live_out[i] || inn != live_in[i] {
                live_out[i] = out;
                live_in[i] = inn;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // A Mov is dead when nothing can observe its destination: not live on
    // any path out, not written back / shape-referenced (always_live), or a
    // self-move.
    let dead: Vec<bool> = kops
        .iter()
        .enumerate()
        .map(|(i, op)| match op {
            KOp::Mov { dst, src } => *dst == *src || (live_out[i] | always_live) & bit(*dst) == 0,
            _ => false,
        })
        .collect();
    if !dead.iter().any(|&d| d) {
        return false;
    }
    // Compact: old index -> index of the first surviving op at-or-after it.
    let mut newidx = vec![0u16; n];
    let mut next = 0u16;
    for i in 0..n {
        newidx[i] = next;
        if !dead[i] {
            next += 1;
        }
    }
    let mut i = 0;
    kops.retain(|_| {
        let keep = !dead[i];
        i += 1;
        keep
    });
    for op in kops.iter_mut() {
        map_targets(op, |t| newidx[t as usize]);
    }
    true
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
    /// Register mirroring numeric `frame.locals[l]`. A local can have exactly
    /// ONE static type: an array base, a boolean, or a number — mixed use
    /// rejects the region (some iteration would bail every time anyway).
    fn lreg(&mut self, l: u32) -> Option<u16> {
        if self.oslot_locals.contains(&l)
            || self.bool_locals.contains(&l)
            || self.bool_reg.iter().any(|&(ll, _)| ll == l)
        {
            return None;
        }
        let slot = KSlot::Local(l);
        if let Some(&(_, r)) = self.local_reg.iter().find(|(sl, _)| *sl == slot) {
            return Some(r);
        }
        let r = u16::try_from(self.local_reg.len()).ok()?;
        if r >= BOOL_BASE {
            return None;
        }
        self.local_reg.push((slot, r));
        Some(r)
    }

    /// Register snapshotting captured upvalue cell `u` (read-only: nothing
    /// can write the cell during a kernel — regions contain no calls — and
    /// in-region upvalue writes are not on the allowlist).
    fn uvreg(&mut self, u: u32) -> Option<u16> {
        let slot = KSlot::Upvalue(u);
        if let Some(&(_, r)) = self.local_reg.iter().find(|(sl, _)| *sl == slot) {
            return Some(r);
        }
        let r = u16::try_from(self.local_reg.len()).ok()?;
        if r >= BOOL_BASE {
            return None;
        }
        self.local_reg.push((slot, r));
        Some(r)
    }

    /// FUNCTION kernels: register holding call argument `a` (read-only; the
    /// entry guard requires it present and a `Number`). Shares the numeric
    /// slot space with locals/upvalues.
    fn argreg(&mut self, a: u32) -> Option<u16> {
        let slot = KSlot::Arg(a);
        if let Some(&(_, r)) = self.local_reg.iter().find(|(sl, _)| *sl == slot) {
            return Some(r);
        }
        let r = u16::try_from(self.local_reg.len()).ok()?;
        if r >= BOOL_BASE {
            return None;
        }
        self.local_reg.push((slot, r));
        Some(r)
    }

    /// fn mode: a local may be read only when a real store to it dominates
    /// (tracked per-path in `init`; merge points require identical sets — no
    /// entry guard exists to type frameless locals). Loop mode: always true.
    fn local_readable(&self, l: u32) -> bool {
        !self.fn_mode || self.init.binary_search(&l).is_ok()
    }

    /// Record a real (value-carrying) store to local `l` on this path.
    fn mark_init(&mut self, l: u32) {
        if self.fn_mode {
            if let Err(p) = self.init.binary_search(&l) {
                self.init.insert(p, l);
            }
        }
    }

    /// fn mode: local `l` no longer holds a readable value on this path (an
    /// elided `undefined`/TDZ store — the generic slot would be
    /// `undefined`/`Uninitialized`, which no kernel register can represent).
    fn clear_init(&mut self, l: u32) {
        if self.fn_mode {
            if let Ok(p) = self.init.binary_search(&l) {
                self.init.remove(p);
            }
        }
    }

    /// Register mirroring BOOLEAN-typed `frame.locals[l]` (holds 0.0/1.0; the
    /// guard requires `Value::Bool`, write-back restores it). Records the
    /// local for the caller's fixpoint when it is not yet in this run's set.
    fn blreg(&mut self, l: u32) -> Option<u16> {
        // Record the discovery FIRST: "this local receives boolean stores" is
        // true even when this run cannot use it yet (e.g. an earlier load in
        // this run already typed it numeric — the next fixpoint run reloads
        // it as a boolean).
        if !self.bool_locals.contains(&l) && !self.discovered_bools.contains(&l) {
            self.discovered_bools.push(l);
        }
        if self.oslot_locals.contains(&l)
            || self.local_reg.iter().any(|&(sl, _)| sl == KSlot::Local(l))
        {
            return None;
        }
        if let Some(&(_, r)) = self.bool_reg.iter().find(|(ll, _)| *ll == l) {
            return Some(r);
        }
        let r = BOOL_BASE + u16::try_from(self.bool_reg.len()).ok()?;
        if r >= STACK_BASE {
            return None;
        }
        self.bool_reg.push((l, r));
        Some(r)
    }

    /// Read-only scalar register for local `l` — numeric or boolean space
    /// (coercing consumers treat a boolean's 0.0/1.0 exactly as ToNumber
    /// would). Never valid for array bases. Every caller is a READ, so fn
    /// mode demands a dominating store.
    fn scalar_lreg(&mut self, l: u32) -> Option<u16> {
        if !self.local_readable(l) {
            return None;
        }
        if self.bool_locals.contains(&l) {
            self.blreg(l)
        } else {
            self.lreg(l)
        }
    }

    /// Whether local `l` is boolean-typed IN THIS RUN.
    fn is_bool_local(&self, l: u32) -> bool {
        self.bool_locals.contains(&l)
    }

    /// Provisional register owned by virtual-stack position `p`.
    fn preg(&self, p: usize) -> Option<u16> {
        let r = STACK_BASE as usize + p;
        if r >= SCRATCH0 as usize {
            return None;
        }
        Some(r as u16)
    }

    /// Register of the entry `depth_from_top` below the top (0 = top) when it
    /// is a SCALAR (number or statically-typed boolean — coercing consumers
    /// read the raw 0.0/1.0). `None` for objects/Math/undefined.
    fn top_reg(&self, depth_from_top: usize) -> Option<u16> {
        let p = self.vstack.len().checked_sub(1 + depth_from_top)?;
        match self.vstack[p] {
            VE::Num | VE::Bool => self.preg(p),
            _ => None,
        }
    }

    /// As [`Xlate::top_reg`], but NUMBER-only — array indices and stored
    /// elements must refuse booleans (`a[true]` is the property "true", and
    /// a stored element must not change type).
    fn top_num_reg(&self, depth_from_top: usize) -> Option<u16> {
        let p = self.vstack.len().checked_sub(1 + depth_from_top)?;
        match self.vstack[p] {
            VE::Num => self.preg(p),
            _ => None,
        }
    }

    /// Static type of the scalar entry `depth_from_top` below the top:
    /// `Some(true)` boolean, `Some(false)` number, `None` non-scalar.
    fn top_is_bool(&self, depth_from_top: usize) -> Option<bool> {
        let p = self.vstack.len().checked_sub(1 + depth_from_top)?;
        match self.vstack[p] {
            VE::Bool => Some(true),
            VE::Num => Some(false),
            _ => None,
        }
    }

    /// Push a Bool entry, returning its register.
    fn push_bool(&mut self) -> Option<u16> {
        let r = self.preg(self.vstack.len())?;
        self.vstack.push(VE::Bool);
        self.origins.push(None);
        Some(r)
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

    /// Pop, requiring a SCALAR entry (number/boolean); returns its register.
    fn pop_scalar(&mut self) -> Option<u16> {
        let r = self.top_reg(0)?;
        self.pop();
        Some(r)
    }

    /// Pop, requiring a NUMBER entry; returns its register.
    fn pop_num(&mut self) -> Option<u16> {
        let r = self.top_num_reg(0)?;
        self.pop();
        Some(r)
    }

    /// A constant usable as a scalar operand: `(value, is_bool)`.
    fn scalar_const(&self, idx: u32) -> Option<(f64, bool)> {
        match self.consts.get(idx as usize)? {
            Const::Number(n) => Some((*n, false)),
            Const::Bool(b) => Some((if *b { 1.0 } else { 0.0 }, true)),
            _ => None,
        }
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
            VE::Bool | VE::Undef | VE::Opaque | VE::SelfFn | VE::MathObj | VE::MathFn(_) => None,
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

    /// Named-property access class for (`oslot`, `key`), deduplicated with
    /// flags OR'd (a site that both loads and stores demands both entry
    /// conditions). Bounded so per-activation entry resolution stays cheap.
    fn kprop(&mut self, oslot: u16, key: &str, load: bool, store: bool) -> Option<u16> {
        if let Some(i) = self
            .props_used
            .iter()
            .position(|p| p.oslot == oslot && &*p.key == key)
        {
            self.props_used[i].load |= load;
            self.props_used[i].store |= store;
            return u16::try_from(i).ok();
        }
        if self.props_used.len() >= 64 {
            return None;
        }
        self.props_used.push(KProp {
            oslot,
            key: key.into(),
            load,
            store,
        });
        u16::try_from(self.props_used.len() - 1).ok()
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
        if shape
            .iter()
            .any(|e| matches!(e, VE::Undef | VE::Opaque | VE::SelfFn))
        {
            return None; // undefined/opaque/self never crosses block boundaries
        }
        let rel = (t as usize).wrapping_sub(self.base_ip as usize);
        if rel < self.region.len() {
            match &self.shape_at[rel] {
                Some(s) if *s != shape => return None,
                Some(_) => {}
                None => self.shape_at[rel] = Some(shape),
            }
            if self.fn_mode {
                // The initialized-locals set must agree at the target too.
                match &self.init_at[rel] {
                    Some(s) if *s != self.init => return None,
                    Some(_) => {}
                    None => self.init_at[rel] = Some(self.init.clone()),
                }
            }
            self.fixups.push((kidx, rel));
        } else {
            self.exits.push((kidx, t, shape));
        }
        Some(())
    }

    fn bin(&mut self, kind: ArithKind) -> Option<()> {
        let b = self.pop_scalar()?;
        let a = self.top_reg(0)?;
        self.kops.push(KOp::Arith { kind, dst: a, a, b });
        let p = self.vstack.len() - 1;
        self.vstack[p] = VE::Num; // ToNumeric(boolean) -> number
        self.origins[p] = None;
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

            // fn mode: the declared-function prologue materializes `this`
            // and `new.target` into locals. The values are unrepresentable
            // (`VE::Opaque`) — legal only when stored into locals the body
            // never reads (`init` tracking rejects any read).
            Op::LoadThis | Op::LoadNewTarget if self.fn_mode => {
                self.vstack.push(VE::Opaque);
                self.origins.push(None);
            }
            // OrdinaryCallBindThis (sloppy): coerces the top-of-stack `this`
            // — opaque in, opaque out.
            Op::BindThisSloppy if self.fn_mode => {
                if !matches!(self.vstack.last(), Some(VE::Opaque)) {
                    return None;
                }
            }

            // The only supported globals, both speculative (entry-guarded):
            // `Math` (canonical object as a data property) and — fn mode —
            // the function's OWN name (the binding must hold the closure
            // being invoked; see `Kernel::self_global`).
            Op::LoadGlobal(c) => {
                let name = match self.consts.get(*c as usize)? {
                    Const::String(n) => n.as_str(),
                    _ => return None,
                };
                if name == "Math" {
                    self.vstack.push(VE::MathObj);
                } else if self.fn_mode && Some(name) == self.self_name {
                    self.vstack.push(VE::SelfFn);
                } else {
                    return None;
                }
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
                if !self.local_readable(*l) {
                    return None;
                }
                if let Some(pos) = self.oslot_locals.iter().position(|&o| o == *l) {
                    // This local is an array base — push the object.
                    self.vstack.push(VE::Obj(pos as u16));
                    self.origins.push(None);
                } else if self.is_bool_local(*l) {
                    let src = self.blreg(*l)?;
                    let dst = self.push_bool()?;
                    self.kops.push(K::Mov { dst, src });
                } else {
                    let src = self.lreg(*l)?;
                    let dst = self.push_num()?;
                    self.kops.push(K::Mov { dst, src });
                    let ver = *self.versions.get(l).unwrap_or(&0);
                    *self.origins.last_mut()? = Some((*l, ver));
                }
            }
            // A captured (read-only in-region) numeric upvalue: snapshot.
            Op::LoadUpvalue(u) => {
                let src = self.uvreg(*u)?;
                let dst = self.push_num()?;
                self.kops.push(K::Mov { dst, src });
            }
            // FUNCTION kernels: a call argument (read-only; the entry guard
            // requires it present and a `Number`). Loop regions never see
            // `LoadArg` — parameters are copied to locals in the prologue.
            Op::LoadArg(a) if self.fn_mode => {
                let src = self.argreg(*a)?;
                let dst = self.push_num()?;
                self.kops.push(K::Mov { dst, src });
            }
            // FUNCTION kernels: yield the (scalar) result and finish the
            // frameless call. Loop regions reject `Return` (the catch-all).
            Op::Return if self.fn_mode => {
                let boolean = self.top_is_bool(0)?;
                let src = self.pop_scalar()?;
                self.kops.push(K::Ret { src, boolean });
            }

            // The checked store's TDZ test reads the CURRENT slot value; the
            // loop-kernel guard proves it is a Number, so the store is a
            // plain move. fn mode has no such guard: the checked store is
            // only provably non-throwing when a real store already dominates
            // (the slot then holds a value, never `Uninitialized`).
            Op::StoreLocal(l) | Op::StoreLocalChecked(l) => {
                if self.fn_mode
                    && matches!(op, Op::StoreLocalChecked(_))
                    && !self.local_readable(*l)
                {
                    return None;
                }
                match self.vstack.last() {
                    Some(VE::Undef) | Some(VE::Opaque) => {
                        // Storing an unrepresentable value is elided. fn
                        // mode: sound outright — the `init` tracking rejects
                        // any read of the local not dominated by a real
                        // store. Loop mode (`undefined` only): the local must
                        // provably be RE-STORED before any read, branch, or
                        // branch target — i.e. the store is dead within this
                        // basic block (the per-iteration `let` reset
                        // pattern). Otherwise the region is rejected.
                        if !self.fn_mode && !self.dead_store_ahead(i + 1, *l) {
                            return None;
                        }
                        self.pop()?;
                        self.store_of(*l);
                        self.clear_init(*l);
                    }
                    Some(VE::Bool) => {
                        // A boolean store TYPES the local (fixpoint feedback);
                        // in the stable run the local is in the bool set and
                        // its loads/guard/write-back use `Value::Bool`.
                        let dst = self.blreg(*l)?;
                        if !self.is_bool_local(*l) {
                            return None; // rerun with the discovery applied
                        }
                        let src = self.pop_scalar()?;
                        self.kops.push(K::Mov { dst, src });
                        self.store_of(*l);
                        self.mark_init(*l);
                    }
                    _ => {
                        let dst = self.lreg(*l)?;
                        let src = self.pop_num()?;
                        self.kops.push(K::Mov { dst, src });
                        self.store_of(*l);
                        self.mark_init(*l);
                    }
                }
            }
            // A block-scoped declaration's TDZ marker: writes `Uninitialized`
            // to the slot. Elidable exactly like the dead `undefined` store —
            // the local must be provably re-stored before anything can read
            // it (a genuine TDZ read would need the generic path's
            // ReferenceError, so such regions stay generic).
            Op::InitLocalTdz(l) => {
                // No type demand here — the local's static type comes from
                // the actual store that must follow (numeric or boolean).
                // fn mode: `init` tracking subsumes the dead-store proof (a
                // genuine TDZ read rejects the region, and the generic path
                // then raises the ReferenceError).
                if !self.fn_mode && !self.dead_store_ahead(i + 1, *l) {
                    return None;
                }
                self.store_of(*l);
                self.clear_init(*l);
            }
            Op::CopyLocal { src, dest } => {
                if !self.local_readable(*src) {
                    return None;
                }
                let (s, d) = if self.is_bool_local(*src) {
                    let s = self.blreg(*src)?;
                    let d = self.blreg(*dest)?;
                    if !self.is_bool_local(*dest) {
                        return None; // rerun with the discovery applied
                    }
                    (s, d)
                } else {
                    (self.lreg(*src)?, self.lreg(*dest)?)
                };
                if s != d {
                    self.kops.push(K::Mov { dst: d, src: s });
                }
                self.store_of(*dest);
                self.mark_init(*dest);
            }
            Op::LoadConst(c) => {
                let (k, is_bool) = self.scalar_const(*c)?;
                let dst = if is_bool {
                    self.push_bool()?
                } else {
                    self.push_num()?
                };
                self.kops.push(K::Const { dst, k });
            }
            Op::LoadTrue | Op::LoadFalse => {
                let dst = self.push_bool()?;
                self.kops.push(K::Const {
                    dst,
                    k: if matches!(op, Op::LoadTrue) { 1.0 } else { 0.0 },
                });
            }
            // `!x` on a scalar: ToBoolean then negate — a boolean result.
            Op::Not => {
                let src = self.pop_scalar()?;
                let dst = self.push_bool()?;
                self.kops.push(K::BoolNot { dst, src });
            }
            Op::LoadLocalConst { local, konst } => {
                if !self.local_readable(*local) {
                    return None;
                }
                let (k, k_bool) = self.scalar_const(*konst)?;
                if self.is_bool_local(*local) {
                    let src = self.blreg(*local)?;
                    let dst0 = self.push_bool()?;
                    self.kops.push(K::Mov { dst: dst0, src });
                } else {
                    let src = self.lreg(*local)?;
                    let dst0 = self.push_num()?;
                    self.kops.push(K::Mov { dst: dst0, src });
                    let ver = *self.versions.get(local).unwrap_or(&0);
                    *self.origins.last_mut()? = Some((*local, ver));
                }
                let dst1 = if k_bool {
                    self.push_bool()?
                } else {
                    self.push_num()?
                };
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
                    VE::Num | VE::Bool => {
                        let src = self.top_reg(0)?;
                        let dst = if top == VE::Bool {
                            self.push_bool()?
                        } else {
                            self.push_num()?
                        };
                        self.kops.push(K::Mov { dst, src });
                    }
                    VE::Obj(s) => {
                        self.vstack.push(VE::Obj(s));
                        self.origins.push(None);
                    }
                    VE::MathObj => {
                        self.vstack.push(VE::MathObj);
                        self.origins.push(None);
                    }
                    VE::MathFn(_) | VE::Undef | VE::Opaque | VE::SelfFn => return None,
                }
            }
            Op::Swap => {
                let n = self.vstack.len();
                let (pa, pb) = (n.checked_sub(2)?, n - 1);
                let (ea, eb) = (self.vstack[pa], self.vstack[pb]);
                if matches!(ea, VE::Undef | VE::Opaque | VE::SelfFn)
                    || matches!(eb, VE::Undef | VE::Opaque | VE::SelfFn)
                {
                    return None;
                }
                let is_reg = |e: VE| matches!(e, VE::Num | VE::Bool);
                match (is_reg(ea), is_reg(eb)) {
                    (true, true) => {
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
                    (true, false) => {
                        // The number moves from position pa's reg to pb's.
                        let (ra, rb) = (self.preg(pa)?, self.preg(pb)?);
                        self.kops.push(K::Mov { dst: rb, src: ra });
                    }
                    (false, true) => {
                        let (ra, rb) = (self.preg(pa)?, self.preg(pb)?);
                        self.kops.push(K::Mov { dst: ra, src: rb });
                    }
                    (false, false) => {}
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
                if [ea, eb, ec]
                    .iter()
                    .any(|e| matches!(e, VE::Undef | VE::Opaque | VE::SelfFn))
                {
                    return None;
                }
                let is_reg = |e: VE| matches!(e, VE::Num | VE::Bool);
                // c -> position a, a -> position b, b -> position c.
                if is_reg(ec) {
                    self.kops.push(K::Mov {
                        dst: SCRATCH0,
                        src: rc,
                    });
                }
                if is_reg(eb) {
                    self.kops.push(K::Mov { dst: rc, src: rb });
                }
                if is_reg(ea) {
                    self.kops.push(K::Mov { dst: rb, src: ra });
                }
                if is_reg(ec) {
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
                let b = self.pop_scalar()?;
                let a = self.top_reg(0)?;
                self.kops.push(K::Add { dst: a, a, b });
                let p = self.vstack.len() - 1;
                self.vstack[p] = VE::Num;
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
                self.vstack[p] = VE::Num;
                self.origins[p] = None;
            }
            Op::BitNot => {
                let s = self.top_reg(0)?;
                self.kops.push(K::BitNot { dst: s, src: s });
                let p = self.vstack.len() - 1;
                self.vstack[p] = VE::Num;
                self.origins[p] = None;
            }
            // ToNumber/ToNumeric on a scalar: identity on the 0/1 register,
            // but the RESULT is a number (retype a boolean entry).
            Op::Pos | Op::ToNumeric => {
                self.top_reg(0)?;
                let p = self.vstack.len() - 1;
                self.vstack[p] = VE::Num;
                self.origins[p] = None;
            }
            Op::Inc | Op::Dec => {
                let s = self.top_reg(0)?;
                self.kops.push(K::AddK {
                    dst: s,
                    a: s,
                    k: if matches!(op, Op::Inc) { 1.0 } else { -1.0 },
                });
                let p = self.vstack.len() - 1;
                self.vstack[p] = VE::Num;
                self.origins[p] = None;
            }

            // ---- fused local-const forms ----
            Op::AddLocalConst { local, konst } => {
                let a = self.scalar_lreg(*local)?;
                let (k, _) = self.scalar_const(*konst)?;
                let dst = self.push_num()?;
                self.kops.push(K::AddK { dst, a, k });
            }
            Op::ArithLocalConst { local, konst, kind } => {
                let a = self.scalar_lreg(*local)?;
                let (k, _) = self.scalar_const(*konst)?;
                let dst = self.push_num()?;
                self.kops.push(K::ArithK {
                    kind: *kind,
                    dst,
                    a,
                    k,
                });
            }
            Op::IncLocalStmt { local, dec } => {
                // Reads the local before writing it.
                if !self.local_readable(*local) {
                    return None;
                }
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
                let idx = self.top_num_reg(0)?;
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
                let val = self.top_num_reg(0)?;
                let idx = self.top_num_reg(1)?;
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
            // Named property access: `a.length` on an array base, or a
            // method/constant on the (guarded canonical) `Math` object.
            Op::GetProp(c) => {
                if matches!(self.vstack.last(), Some(VE::MathObj)) {
                    let name = match self.consts.get(*c as usize)? {
                        Const::String(n) => n.as_str().to_string(),
                        _ => return None,
                    };
                    if let Some(kind) = KMath::from_name(&name) {
                        self.pop()?;
                        self.vstack.push(VE::MathFn(kind));
                        self.origins.push(None);
                        if !self.math_used.contains(&kind) {
                            self.math_used.push(kind);
                        }
                        return Some(());
                    }
                    // The canonical Math object's value constants are
                    // non-writable AND non-configurable — immutable forever —
                    // so with the object identity guarded they fold to
                    // constants outright.
                    let k = match name.as_str() {
                        "PI" => std::f64::consts::PI,
                        "E" => std::f64::consts::E,
                        "LN2" => std::f64::consts::LN_2,
                        "LN10" => std::f64::consts::LN_10,
                        "LOG2E" => std::f64::consts::LOG2_E,
                        "LOG10E" => std::f64::consts::LOG10_E,
                        "SQRT2" => std::f64::consts::SQRT_2,
                        "SQRT1_2" => std::f64::consts::FRAC_1_SQRT_2,
                        _ => return None,
                    };
                    self.pop()?;
                    let dst = self.push_num()?;
                    self.kops.push(K::Const { dst, k });
                    return Some(());
                }
                let key = match self.consts.get(*c as usize)? {
                    Const::String(s) => s.as_str().to_string(),
                    _ => return None,
                };
                if key == "length" {
                    // Arrays: derived length, per-access checked, bailable.
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
                } else {
                    // Ordinary-object own data property: entry-resolved slot,
                    // no per-access check, no bail (see `KOp::LoadProp`).
                    let obj = self.base_slot(0)?;
                    let prop = self.kprop(obj, &key, true, false)?;
                    self.pop()?; // base
                    let dst = self.push_num()?;
                    self.kops.push(K::LoadProp { dst, prop });
                }
            }

            // `o.k = v` named write: the entry-resolved in-place overwrite
            // (`KOp::StoreProp`); the op's result is the assigned value.
            Op::SetProp(c) => {
                let key = match self.consts.get(*c as usize)? {
                    Const::String(s) => s.as_str().to_string(),
                    _ => return None,
                };
                let val = self.top_num_reg(0)?;
                let obj = self.base_slot(1)?;
                let prop = self.kprop(obj, &key, false, true)?;
                self.pop()?; // val
                self.pop()?; // base
                let dst = self.push_num()?;
                self.kops.push(K::StoreProp { prop, src: val });
                if dst != val {
                    self.kops.push(K::Mov { dst, src: val });
                }
            }

            // A call is supported ONLY as the compiler's Math method-call
            // pattern — [.., MathFn(kind), MathObj(this), args...] with the
            // kind's exact arity — or, fn mode, a DIRECT SELF-CALL:
            // [.., SelfFn, undefined(this), args...]. Everything else
            // rejects the region.
            Op::Call(argc) => {
                let n = *argc as usize;
                let fn_pos = self.vstack.len().checked_sub(n + 2)?;
                if matches!(self.vstack.get(fn_pos)?, VE::SelfFn) {
                    // Plain sloppy call: `this` is the pushed `undefined`.
                    if !matches!(self.vstack.get(fn_pos + 1)?, VE::Undef) {
                        return None;
                    }
                    // Every argument must be a statically-NUMBER register
                    // (the callee's Arg slots assume Numbers — a raw-0/1
                    // boolean would diverge under typeof/strict-eq); they
                    // sit contiguously in the positions' own registers.
                    for d in 0..n {
                        self.top_num_reg(d)?;
                    }
                    let base = if n > 0 { self.preg(fn_pos + 2)? } else { 0 };
                    for _ in 0..n + 2 {
                        self.pop()?;
                    }
                    let dst = self.push_num()?;
                    self.kops.push(K::SelfCall {
                        dst,
                        base,
                        argc: u16::try_from(n).ok()?,
                    });
                    return Some(());
                }
                // LOOP mode: a call of a PINNED closure — the callee is an
                // object-typed local (oslot machinery, discovered exactly
                // like an array base) under a plain `undefined` this.
                // Arguments must be statically NUMBER registers: they copy
                // raw into the callee kernel's guarded-Number arg registers.
                if !self.fn_mode
                    && matches!(self.vstack.get(fn_pos + 1)?, VE::Undef)
                    && !matches!(self.vstack.get(fn_pos)?, VE::MathFn(_) | VE::MathObj)
                {
                    for d in 0..n {
                        self.top_num_reg(d)?;
                    }
                    let oslot = self.base_slot(n + 1)?;
                    let argc = u16::try_from(n).ok()?;
                    // `fslot` indexes the CALLEE table (parallel to the
                    // executor's per-activation window list), NOT the oslots.
                    let fslot = match self.callees.iter().position(|c| c.oslot == oslot) {
                        Some(i) => {
                            self.callees[i].min_argc = self.callees[i].min_argc.min(argc);
                            i
                        }
                        None => {
                            self.callees.push(KCallee {
                                oslot,
                                min_argc: argc,
                            });
                            self.callees.len() - 1
                        }
                    };
                    let base = if n > 0 { self.preg(fn_pos + 2)? } else { 0 };
                    for _ in 0..n + 2 {
                        self.pop()?;
                    }
                    let dst = self.push_num()?;
                    self.kops.push(K::CallKernel {
                        dst,
                        fslot: u16::try_from(fslot).ok()?,
                        base,
                        argc,
                    });
                    return Some(());
                }
                let kind = match self.vstack.get(fn_pos)? {
                    VE::MathFn(k) => *k,
                    _ => return None,
                };
                if !matches!(self.vstack.get(fn_pos + 1)?, VE::MathObj) || kind.arity() != n {
                    return None;
                }
                match n {
                    1 => {
                        let src = self.top_reg(0)?;
                        self.pop()?; // arg
                        self.pop()?; // this (MathObj)
                        self.pop()?; // fn
                        let dst = self.push_num()?;
                        self.kops.push(K::Math1 { kind, dst, src });
                    }
                    2 => {
                        let b = self.top_reg(0)?;
                        let a = self.top_reg(1)?;
                        self.pop()?;
                        self.pop()?;
                        self.pop()?; // this
                        self.pop()?; // fn
                        let dst = self.push_num()?;
                        self.kops.push(K::Math2 { kind, dst, a, b });
                    }
                    _ => return None,
                }
            }

            // ---- comparisons: fused into the following conditional jump
            // when one immediately consumes them, otherwise MATERIALIZED as a
            // boolean register (CmpSet). Strict (in)equality between
            // statically mixed boolean/number operands folds to a constant —
            // the generic `strict_equals` never compares values across types.
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
                let b_bool = self.top_is_bool(0)?;
                let a_bool = self.top_is_bool(1)?;
                let fixed = static_cmp(cmp, a_bool, b_bool);
                // Fused form: immediately consumed by a conditional jump that
                // is itself not a branch target.
                let fuse = match self.region.get(i + 1) {
                    Some(Op::JumpIfFalse(t)) if !self.is_target[i + 1] => Some((false, *t)),
                    Some(Op::JumpIfTrue(t)) if !self.is_target[i + 1] => Some((true, *t)),
                    _ => None,
                };
                let b = self.pop_scalar()?;
                let a = self.pop_scalar()?;
                match (fuse, fixed) {
                    (Some((if_true, target)), None) => {
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
                    (Some((if_true, target)), Some(outcome)) => {
                        // Statically decided branch: taken -> unconditional
                        // jump; not taken -> fall through (operands consumed).
                        if outcome == if_true {
                            let kidx = self.kops.len();
                            self.kops.push(K::Br { target: u16::MAX });
                            self.branch_to(kidx, target, self.vstack.clone())?;
                        }
                        self.absorbed = Some(i + 1);
                    }
                    (None, None) => {
                        let dst = self.push_bool()?;
                        self.kops.push(K::CmpSet { cmp, dst, a, b });
                    }
                    (None, Some(outcome)) => {
                        let dst = self.push_bool()?;
                        self.kops.push(K::Const {
                            dst,
                            k: if outcome { 1.0 } else { 0.0 },
                        });
                    }
                }
            }

            // ---- branches ----
            Op::Jump(t) => {
                let kidx = self.kops.len();
                self.kops.push(K::Br { target: u16::MAX });
                self.branch_to(kidx, *t, self.vstack.clone())?;
            }
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                let src = self.pop_scalar()?;
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
                let b_bool = self.top_is_bool(0)?;
                let a_bool = self.top_is_bool(1)?;
                let b = self.pop_scalar()?;
                let a = self.pop_scalar()?;
                if let Some(outcome) = static_cmp(*cmp, a_bool, b_bool) {
                    if outcome == if_true {
                        let kidx = self.kops.len();
                        self.kops.push(K::Br { target: u16::MAX });
                        self.branch_to(kidx, *target, self.vstack.clone())?;
                    }
                } else {
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
                let a_bool = self.is_bool_local(*local);
                let a = self.scalar_lreg(*local)?;
                let (k, k_bool) = self.scalar_const(*konst)?;
                if let Some(outcome) = static_cmp(*cmp, a_bool, k_bool) {
                    if outcome == if_true {
                        let kidx = self.kops.len();
                        self.kops.push(K::Br { target: u16::MAX });
                        self.branch_to(kidx, *target, self.vstack.clone())?;
                    }
                } else {
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
            }

            // Anything else — calls, other property access, cells, upvalues,
            // TDZ init, globals, objects, strings, try/dispose/iterator/
            // suspend machinery — rejects the region.
            _ => return None,
        }
        Some(())
    }
}
