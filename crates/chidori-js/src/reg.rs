//! Register bytecode tier (docs/js-performance-roadmap.md §3.5): translate a
//! whole eligible function body from the stack bytecode into a register
//! program executed by `Vm::run_reg_frame` (exec.rs) — no operand stack, no
//! per-value push/pop traffic, and one dispatch where the stack machine pays
//! two or three (`LoadLocal a; LoadLocal b; Add; StoreLocal c` is a single
//! `Add { dst: c, a, b }`).
//!
//! The model mirrors the loop-kernel tier's discipline (kernel.rs), applied
//! to BOXED values over the whole body instead of unboxed `f64`s over loop
//! regions:
//!
//! - **Same helpers, same semantics.** Every register op calls the SAME
//!   `Vm` helper as its stack twin (`op_add`, `Vm::arith`, `cmp_values`,
//!   `get_prop`/`put_value`, the inline-cache fast paths, `call_valuevec`,
//!   …), so coercion order, thrown errors, and observable effects are
//!   identical by construction. The translation changes only WHERE operands
//!   live (indexed registers instead of a stack), never what runs.
//! - **Whole-function eligibility.** Translation is all-or-nothing per
//!   function: any op outside the translated subset (try/finally handlers,
//!   `with`/direct-`eval` scope machinery, class `super`/private elements,
//!   suspension ops, `Op::LoopKernel`, …) declines the function and it keeps
//!   the stack interpreter. Loop-kernelized functions keep the stack tier so
//!   their unboxed kernels — far faster than boxed register ops — stay in
//!   charge.
//! - **Registers are `frame.locals`.** Register `0..num_locals` ARE the
//!   localized bindings (same slots, same TDZ marker); `num_locals..` are the
//!   canonical homes of the stack machine's operand-stack depths. A frame
//!   running in register mode is an ordinary `Frame` — the GC tracer, the
//!   frame pool, `arguments`, cells, and upvalues all work unchanged.
//! - **Determinism.** The register program is compiled from the final stack
//!   bytecode at compile finish — a pure function of the source. Execution
//!   order of every observable operation is identical to the stack path
//!   (gated by the reg-on/off differential corpus in `tests/reg.rs`), so the
//!   replay journal is byte-identical either way. Like kernels, the tier is
//!   OFF whenever an op budget is installed: per-op accounting stays exact
//!   on the generic path.
//!
//! # Translation scheme
//!
//! A virtual stack of [`Entry`] values abstract-interprets the stack code.
//! The entry at depth `d` is either **canonical** (its value lives in
//! register `canon(d) = num_locals + d`) or **lazy** — a reference to a
//! local register, a constant, or a keyword value (`undefined`, `this`, …)
//! that has not been materialized. Lazy entries make the classic stack
//! shuffle free: `LoadLocal` emits NOTHING; the consuming op reads the local
//! register directly.
//!
//! Soundness of lazy references:
//! - A `Local(l)` entry aliases a MUTABLE register, so every op that writes
//!   local `l` first flushes any live `Local(l)` entries to their canonical
//!   slots (the stack machine's push took a copy at push time; the flush
//!   reproduces it). Locals are unaliasable outside the frame (that is what
//!   the localization pass proved), so explicit local-writing ops are the
//!   ONLY writers.
//! - A `Reg(r)` entry (created by `Dup` of a canonical entry) may only alias
//!   a register BELOW its own depth: results always land at `canon(top)`,
//!   which can only revisit `r` after the aliasing entry itself was consumed.
//! - At every branch, jump target, and label fall-in the whole virtual stack
//!   is flushed to canonical form, so control-flow joins agree on where every
//!   live value lives regardless of path.
//!
//! TDZ: reads of locals that MIGHT hold the `Uninitialized` marker emit an
//! explicit [`ROp::TdzCheck`]; a small forward dataflow (intersection at
//! joins, to a fixpoint) proves most reads — every loop variable after its
//! init — never need one, so they alias with zero checks where the stack
//! machine re-checks on every `LoadLocal`.

use std::collections::HashMap;

use crate::bytecode::{CmpOp, IcEntry, Op};
use crate::exec::ArithKind;

/// Compiled register program for one function. Stored on
/// [`crate::bytecode::FuncProto::reg`]; executed by `Vm::run_reg_frame`.
#[derive(Debug)]
pub struct RegProto {
    pub code: Vec<ROp>,
    /// Total register-file size: `num_locals` localized bindings followed by
    /// the canonical operand slots. `Frame.locals` is resized to this on
    /// register-mode entry.
    pub num_regs: u16,
    /// Inline-cache entries for `GetProp`/`SetProp`/`LoadGlobal` sites,
    /// indexed by the op's `ic` payload. Same key-verified discipline as
    /// [`crate::bytecode::FuncProto::ic`].
    pub ic: Box<[IcEntry]>,
}

/// Unary value ops sharing one register shape (`dst = op(src)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RUnary {
    Neg,
    Pos,
    ToNumeric,
    Inc,
    Dec,
    BitNot,
    Not,
    Typeof,
    ToStr,
    ToKey,
}

/// Property-definition kinds sharing one register shape (`DefineProp`),
/// mapping 1:1 onto the six stack `Define*` ops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefKind {
    Field,
    Method,
    Getter,
    Setter,
    MethodGetter,
    MethodSetter,
}

/// The register instruction set. `dst`/`src`/operand fields index the
/// frame's register file (`frame.locals`); `target` fields are absolute
/// indices into [`RegProto::code`]; `name`/`konst`/`idx` fields index the
/// function's const table exactly like their stack twins.
#[derive(Clone, Debug)]
pub enum ROp {
    // ---- moves / constants / frame values ----
    Mov {
        dst: u16,
        src: u16,
    },
    Const {
        dst: u16,
        idx: u32,
    },
    Undef {
        dst: u16,
    },
    Null {
        dst: u16,
    },
    Bool {
        dst: u16,
        v: bool,
    },
    Hole {
        dst: u16,
    },
    This {
        dst: u16,
    },
    NewTarget {
        dst: u16,
    },
    Arg {
        dst: u16,
        idx: u32,
    },
    RestArgs {
        dst: u16,
        from: u32,
    },
    Arguments {
        dst: u16,
    },
    /// OrdinaryCallBindThis (sloppy): rebind `regs[reg]` in place.
    BindThisSloppy {
        reg: u16,
    },

    // ---- locals (registers 0..num_locals) ----
    /// Throw a ReferenceError if `regs[src]` is the TDZ marker. Emitted only
    /// where the init dataflow could not prove the read safe.
    TdzCheck {
        src: u16,
    },
    StoreLocalChecked {
        local: u16,
        src: u16,
    },
    InitLocalTdz {
        local: u16,
    },
    /// Statement-position `i++`/`--i` on a local (mirror of
    /// `Op::IncLocalStmt`, TDZ check included).
    IncLocal {
        local: u16,
        dec: bool,
    },

    // ---- cells / upvalues ----
    LoadCell {
        dst: u16,
        cell: u32,
    },
    StoreCell {
        cell: u32,
        src: u16,
    },
    StoreCellChecked {
        cell: u32,
        src: u16,
    },
    InitCell {
        cell: u32,
        src: u16,
    },
    InitCellTdz {
        cell: u32,
    },
    IncCell {
        cell: u32,
        dec: bool,
    },
    LoadCellInit {
        src: u32,
        dest: u32,
    },
    AddCellK {
        dst: u16,
        cell: u32,
        konst: u32,
    },
    ArithCellK {
        kind: ArithKind,
        dst: u16,
        cell: u32,
        konst: u32,
    },
    CmpBrCellK {
        cmp: CmpOp,
        cell: u32,
        konst: u32,
        if_true: bool,
        target: u32,
    },
    LoadUpvalue {
        dst: u16,
        idx: u32,
    },
    StoreUpvalue {
        idx: u32,
        src: u16,
    },
    StoreUpvalueChecked {
        idx: u32,
        src: u16,
    },

    // ---- globals ----
    LoadGlobal {
        dst: u16,
        name: u32,
        ic: u32,
    },
    StoreGlobal {
        name: u32,
        src: u16,
    },
    LoadGlobalTypeof {
        dst: u16,
        name: u32,
    },
    DeclareGlobal {
        name: u32,
        deletable: bool,
    },
    CanDeclareGlobalFunc {
        name: u32,
    },
    DefineGlobalFunc {
        name: u32,
        deletable: bool,
        src: u16,
    },

    // ---- arithmetic / unary / comparison ----
    Add {
        dst: u16,
        a: u16,
        b: u16,
    },
    AddK {
        dst: u16,
        a: u16,
        konst: u32,
    },
    Arith {
        kind: ArithKind,
        dst: u16,
        a: u16,
        b: u16,
    },
    ArithK {
        kind: ArithKind,
        dst: u16,
        a: u16,
        konst: u32,
    },
    Unary {
        kind: RUnary,
        dst: u16,
        src: u16,
    },
    Cmp {
        cmp: CmpOp,
        dst: u16,
        a: u16,
        b: u16,
    },
    InstanceOf {
        dst: u16,
        a: u16,
        b: u16,
    },

    // ---- control flow ----
    Jmp {
        target: u32,
    },
    BrTrue {
        src: u16,
        target: u32,
    },
    BrFalse {
        src: u16,
        target: u32,
    },
    /// `&&`: branch when falsy keeping the value (fallthrough abandons it).
    BrFalsyKeep {
        src: u16,
        target: u32,
    },
    /// `||`: branch when truthy keeping the value.
    BrTruthyKeep {
        src: u16,
        target: u32,
    },
    /// `??`: branch when NOT nullish keeping the value.
    BrNotNullishKeep {
        src: u16,
        target: u32,
    },
    /// Optional-chain short-circuit: when nullish, overwrite with `undefined`
    /// and branch (the value survives on both edges).
    BrNullishUndef {
        reg: u16,
        target: u32,
    },
    CmpBr {
        cmp: CmpOp,
        a: u16,
        b: u16,
        if_true: bool,
        target: u32,
    },
    CmpBrK {
        cmp: CmpOp,
        a: u16,
        konst: u32,
        if_true: bool,
        target: u32,
    },

    // ---- property access ----
    GetProp {
        dst: u16,
        obj: u16,
        name: u32,
        ic: u32,
    },
    SetProp {
        dst: u16,
        obj: u16,
        name: u32,
        src: u16,
        ic: u32,
    },
    GetElem {
        dst: u16,
        obj: u16,
        key: u16,
    },
    SetElem {
        dst: u16,
        obj: u16,
        key: u16,
        src: u16,
    },
    DelProp {
        dst: u16,
        obj: u16,
        name: u32,
    },
    DelElem {
        dst: u16,
        obj: u16,
        key: u16,
    },
    HasProp {
        dst: u16,
        key: u16,
        obj: u16,
    },

    // ---- calls ----
    /// Arguments live in the CONTIGUOUS canonical registers `at..at+argc`
    /// (moved out by the callee — they are dead operand slots). `func`/`this`
    /// may be any register and are cloned, never moved.
    Call {
        dst: u16,
        func: u16,
        this: u16,
        at: u16,
        argc: u16,
        has_this: bool,
    },
    New {
        dst: u16,
        ctor: u16,
        at: u16,
        argc: u16,
    },
    CallSpread {
        dst: u16,
        func: u16,
        this: u16,
        args: u16,
    },
    NewSpread {
        dst: u16,
        ctor: u16,
        args: u16,
    },
    Ret {
        src: u16,
    },
    /// Mirror of `Op::ReturnUndefined` (returns `frame.completion`).
    RetCompletion,
    Throw {
        src: u16,
    },
    ThrowConstAssign,

    // ---- closures / objects / arrays ----
    Closure {
        dst: u16,
        idx: u32,
    },
    NewObject {
        dst: u16,
    },
    /// Elements move out of canonical registers `at..at+n`.
    NewArray {
        dst: u16,
        at: u16,
        n: u16,
    },
    ArraySpread {
        arr: u16,
        src: u16,
    },
    DefineProp {
        kind: DefKind,
        obj: u16,
        key: u16,
        val: u16,
    },
    SetHomeObject {
        obj: u16,
        val: u16,
    },
    ObjectSpread {
        target: u16,
        src: u16,
    },
    /// Excluded keys move out of canonical registers `at..at+n`.
    CopyDataPropsExcept {
        target: u16,
        src: u16,
        at: u16,
        n: u16,
    },
    GetTemplateObject {
        dst: u16,
        idx: u32,
    },
    NewRegExp {
        dst: u16,
        pattern: u32,
        flags: u32,
    },
    /// Parts move out of canonical registers `at..at+n`.
    ConcatStrings {
        dst: u16,
        at: u16,
        n: u16,
    },
    RequireObjectCoercible {
        src: u16,
    },
    RequireCoercible {
        src: u16,
    },
    RequireIterResult {
        src: u16,
    },
    SetFunctionNameFromKey {
        prefix: u32,
        key: u16,
        val: u16,
    },
    SetProtoFromLiteral {
        obj: u16,
        src: u16,
    },

    // ---- iteration ----
    GetIterator {
        dst: u16,
        src: u16,
    },
    IterNext {
        dst: u16,
        it: u16,
    },
    ForInEnumerate {
        dst: u16,
        src: u16,
    },
    /// Writes the next key to `dst` and the has-next flag to `dst + 1`.
    ForInNext {
        dst: u16,
    },
    ForInPop,
}

// =============================================================================
// Eligibility + stack effects
// =============================================================================

/// The stack effect of one translatable op: values popped and pushed on the
/// straight-line path. `None` = the op is outside the translated subset (the
/// whole function declines). Branch/terminal shapes are handled by the
/// callers (`flow_out`, the emitter) — this covers operand traffic only.
fn effect(op: &Op) -> Option<(u32, u32)> {
    use Op::*;
    Some(match op {
        Nop => (0, 0),
        LoadConst(_) | LoadUndefined | LoadHole | LoadNull | LoadTrue | LoadFalse | LoadThis
        | LoadNewTarget | LoadArg(_) | LoadRestArgs(_) | LoadLocal(_) | LoadCell(_)
        | LoadUpvalue(_) | LoadGlobal(_) | LoadGlobalTypeof(_) | LoadArguments
        | ArrayPushElision => (0, 1),
        RequireObjectCoercible | RequireIterResult => (0, 0),
        BindThisSloppy => (1, 1),
        StoreLocal(_)
        | StoreLocalChecked(_)
        | StoreCell(_)
        | StoreCellChecked(_)
        | InitCell(_)
        | StoreUpvalue(_)
        | StoreUpvalueChecked(_)
        | StoreGlobal(_)
        | RequireCoercible
        | Pop => (1, 0),
        InitLocalTdz(_)
        | InitCellTdz(_)
        | IncLocalStmt { .. }
        | IncCellStmt { .. }
        | CopyLocal { .. }
        | LoadCellInit { .. }
        | ForInPop => (0, 0),
        LoadLocalConst { .. } | LoadCellConst { .. } => (0, 2),
        AddLocalConst { .. }
        | ArithLocalConst { .. }
        | AddCellConst { .. }
        | ArithCellConst { .. } => (0, 1),
        DeclareGlobal { .. } | CanDeclareGlobalFunc(_) => (0, 0),
        DefineGlobalFunc { .. } => (1, 0),
        Dup => (0, 1),
        Swap | Rot3 => (0, 0),
        NewObject | GetTemplateObject(_) | NewRegExp { .. } | Closure(_) => (0, 1),
        NewArray(n) | ConcatStrings(n) => (*n, 1),
        ArraySpread | ObjectSpread => (2, 1),
        CopyDataPropertiesExcept(n) => (*n + 2, 1),
        DefineField | DefineMethod | DefineGetter | DefineSetter | DefineMethodGetter
        | DefineMethodSetter => (3, 1),
        SetHomeObject | SetFunctionNameFromKey(_) => (0, 0),
        SetProtoFromLiteral => (1, 0),
        GetProp(_) | DeleteProp(_) => (1, 1),
        SetProp(_) | GetPropDynamic | DeletePropDynamic | HasProp => (2, 1),
        SetPropDynamic => (3, 1),
        JumpIfNullish(_) => (0, 0),
        Call(argc) => (*argc + 2, 1),
        CallMethodless(argc) => (*argc + 1, 1),
        New(argc) => (*argc + 1, 1),
        CallSpread => (3, 1),
        NewSpread => (2, 1),
        Return | Throw => (1, 0),
        ReturnUndefined | ThrowConstAssign => (0, 0),
        Add | Sub | Mul | Div | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr | UShr => (2, 1),
        Neg | Pos | ToNumeric | Inc | Dec | BitNot | Not | TypeofExpr | ToPropertyKey
        | ToStringOp => (1, 1),
        Eq | Ne | StrictEq | StrictNe | Lt | Le | Gt | Ge | InstanceOf => (2, 1),
        Jump(_) => (0, 0),
        JumpIfTrue(_) | JumpIfFalse(_) => (1, 0),
        CmpBranchFalse { .. } | CmpBranchTrue { .. } => (2, 0),
        CmpCellConstBranchFalse { .. }
        | CmpCellConstBranchTrue { .. }
        | CmpLocalConstBranchFalse { .. }
        | CmpLocalConstBranchTrue { .. } => (0, 0),
        JumpIfFalsyPeek(_) | JumpIfTruthyPeek(_) | JumpIfNullishPeek(_) => (0, 0),
        GetIterator | ForInEnumerate => (1, 1),
        IteratorNext => (0, 1),
        ForInNext => (0, 2),
        // Everything else — try/finally machinery, `with`/eval scope ops,
        // super/private/class wiring, dispose, suspension, delegation,
        // dynamic import, `Op::LoopKernel` (the unboxed kernels stay in
        // charge of their functions), IteratorClose (coupled to the parked-
        // completion machinery) — declines the function.
        _ => return None,
    })
}

/// Control-flow shape of one translatable op.
enum FlowKind {
    /// Falls through only.
    Linear,
    /// Unconditional jump.
    Jump(u32),
    /// Conditional: both fallthrough and target, at the same depth (after
    /// `effect` pops).
    Branch(u32),
    /// Peek-branch: the TARGET edge keeps the top value; the fallthrough
    /// edge pops it.
    PeekBranch(u32),
    /// Return/Throw: no successors.
    Terminal,
}

fn flow_of(op: &Op) -> FlowKind {
    use Op::*;
    match op {
        Jump(t) => FlowKind::Jump(*t),
        JumpIfTrue(t) | JumpIfFalse(t) | JumpIfNullish(t) => FlowKind::Branch(*t),
        CmpBranchFalse { target, .. }
        | CmpBranchTrue { target, .. }
        | CmpCellConstBranchFalse { target, .. }
        | CmpCellConstBranchTrue { target, .. }
        | CmpLocalConstBranchFalse { target, .. }
        | CmpLocalConstBranchTrue { target, .. } => FlowKind::Branch(*target),
        JumpIfFalsyPeek(t) | JumpIfTruthyPeek(t) | JumpIfNullishPeek(t) => FlowKind::PeekBranch(*t),
        Return | ReturnUndefined | Throw | ThrowConstAssign => FlowKind::Terminal,
        _ => FlowKind::Linear,
    }
}

// =============================================================================
// TDZ / depth dataflow
// =============================================================================

/// Bitset over local slots that MIGHT hold the TDZ marker at a program point.
#[derive(Clone, PartialEq, Eq)]
struct TdzSet {
    bits: Box<[u64]>,
}

impl TdzSet {
    fn empty(n: u32) -> TdzSet {
        TdzSet {
            bits: vec![0u64; n.div_ceil(64) as usize].into_boxed_slice(),
        }
    }
    fn set(&mut self, i: u32) {
        self.bits[(i / 64) as usize] |= 1 << (i % 64);
    }
    fn clear(&mut self, i: u32) {
        self.bits[(i / 64) as usize] &= !(1 << (i % 64));
    }
    fn get(&self, i: u32) -> bool {
        (self.bits[(i / 64) as usize] >> (i % 64)) & 1 != 0
    }
    /// Join: a local is maybe-TDZ after the join if it is on ANY inflow edge.
    fn union_with(&mut self, other: &TdzSet) -> bool {
        let mut changed = false;
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            let n = *a | *b;
            if n != *a {
                *a = n;
                changed = true;
            }
        }
        changed
    }
}

/// Apply one op's effect on the maybe-TDZ set. Reads of a local through the
/// TDZ-checking stack ops PROVE the slot initialized on the fallthrough path
/// (they throw otherwise), so they clear the bit.
fn tdz_transfer(op: &Op, tdz: &mut TdzSet) {
    use Op::*;
    match op {
        InitLocalTdz(i) => tdz.set(*i),
        StoreLocal(i) => tdz.clear(*i),
        // Checked reads/writes prove the slot initialized past this point.
        LoadLocal(i) | StoreLocalChecked(i) | IncLocalStmt { local: i, .. } => tdz.clear(*i),
        LoadLocalConst { local, .. }
        | AddLocalConst { local, .. }
        | ArithLocalConst { local, .. }
        | CmpLocalConstBranchFalse { local, .. }
        | CmpLocalConstBranchTrue { local, .. } => tdz.clear(*local),
        CopyLocal { src, dest } => {
            tdz.clear(*src);
            tdz.clear(*dest);
        }
        _ => {}
    }
}

/// Per-label converged state from the pre-emission dataflow.
struct LabelState {
    depth: u32,
    tdz: TdzSet,
}

// =============================================================================
// Translator
// =============================================================================

/// One virtual-stack slot during emission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    /// Value lives in `canon(depth)`.
    Temp,
    /// Value lives in register `r` — an alias created by `Dup`; always
    /// `r < canon(own depth)`, see the module docs for why that is sound.
    Reg(u16),
    /// Lazy local reference (TDZ already checked at push where needed).
    Local(u16),
    /// Lazy constant (const-table index).
    K(u32),
    Undef,
    Null,
    True,
    False,
    Hole,
    This,
    NewTarget,
}

struct Emitter {
    num_locals: u16,
    code: Vec<ROp>,
    entries: Vec<Entry>,
    tdz: TdzSet,
    /// Highest register index used so far (register-file size − 1).
    max_reg: u16,
    /// Number of inline-cache sites assigned so far.
    ics: u32,
    /// rpc of the last emitted single-`dst` op whose result is the current
    /// virtual-stack top (for the `StoreLocal` dst-rewrite peephole).
    last_def: Option<usize>,
}

/// Translate a function's final stack bytecode into a register program.
/// Returns `None` when the function contains any op outside the translated
/// subset, contains loop kernels, or exceeds the register budget.
pub fn regify(code: &[Op], num_locals: u32) -> Option<RegProto> {
    if code.is_empty() || num_locals > u16::MAX as u32 / 2 {
        return None;
    }
    // Phase 1: eligibility + label collection.
    let mut labels: Vec<u32> = Vec::new();
    for op in code {
        effect(op)?;
        match flow_of(op) {
            FlowKind::Jump(t) | FlowKind::Branch(t) | FlowKind::PeekBranch(t) => labels.push(t),
            _ => {}
        }
    }
    labels.sort_unstable();
    labels.dedup();
    let is_label = |ip: u32| labels.binary_search(&ip).is_ok();

    // Phase 2: depth + maybe-TDZ dataflow to a fixpoint over the labels.
    // Depths are deterministic per label (mismatch = malformed input: bail);
    // TDZ sets grow monotonically under union, so this converges.
    // Never iterated (get/insert only), so hasher nondeterminism is unobservable.
    let mut label_states: HashMap<u32, LabelState> = HashMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        let mut cur: Option<(u32, TdzSet)> = Some((0, TdzSet::empty(num_locals)));
        for ip in 0..code.len() as u32 {
            if is_label(ip) {
                // Merge fall-in state into the label, then continue FROM the
                // label's converged state.
                if let Some((depth, tdz)) = cur.take() {
                    match label_states.get_mut(&ip) {
                        Some(st) => {
                            if st.depth != depth {
                                return None;
                            }
                            if st.tdz.union_with(&tdz) {
                                changed = true;
                            }
                        }
                        None => {
                            label_states.insert(ip, LabelState { depth, tdz });
                            changed = true;
                        }
                    }
                }
                cur = label_states.get(&ip).map(|st| (st.depth, st.tdz.clone()));
            }
            let Some((depth, mut tdz)) = cur else {
                cur = None;
                continue;
            };
            let op = &code[ip as usize];
            let (pops, pushes) = effect(op).unwrap();
            if depth < pops {
                return None; // malformed: operand-stack underflow
            }
            let after = depth - pops + pushes;
            // Register indices are u16: `canon(depth) = num_locals + depth`
            // (plus a scratch slot) must stay addressable. A pathological
            // operand-stack depth (a 60k-element array literal) declines.
            if num_locals + after + 8 > u16::MAX as u32 {
                return None;
            }
            tdz_transfer(op, &mut tdz);
            // Propagate to the branch target (if any).
            let mut edge = |target: u32, depth: u32, tdz: &TdzSet| -> Option<()> {
                if target as usize > code.len() {
                    return None;
                }
                match label_states.get_mut(&target) {
                    Some(st) => {
                        if st.depth != depth {
                            return None;
                        }
                        if st.tdz.union_with(tdz) {
                            changed = true;
                        }
                    }
                    None => {
                        label_states.insert(
                            target,
                            LabelState {
                                depth,
                                tdz: tdz.clone(),
                            },
                        );
                        changed = true;
                    }
                }
                Some(())
            };
            cur = match flow_of(op) {
                FlowKind::Linear => Some((after, tdz)),
                FlowKind::Jump(t) => {
                    edge(t, after, &tdz)?;
                    None
                }
                FlowKind::Branch(t) => {
                    edge(t, after, &tdz)?;
                    Some((after, tdz))
                }
                FlowKind::PeekBranch(t) => {
                    // Target keeps the top value; fallthrough pops it.
                    edge(t, depth, &tdz)?;
                    Some((depth - 1, tdz))
                }
                FlowKind::Terminal => None,
            };
        }
    }

    // Phase 3: emission.
    let mut e = Emitter {
        num_locals: num_locals as u16,
        code: Vec::with_capacity(code.len()),
        entries: Vec::new(),
        tdz: TdzSet::empty(num_locals),
        max_reg: num_locals.saturating_sub(1) as u16,
        ics: 0,
        last_def: None,
    };
    // rpc of each stack ip (branch targets remapped through this at the end).
    let mut rpc_at: Vec<u32> = vec![0; code.len() + 1];
    let mut live = true;
    let mut ip: u32 = 0;
    while ip < code.len() as u32 {
        if is_label(ip) {
            if live {
                e.flush_all();
                debug_assert_eq!(
                    e.entries.len() as u32,
                    label_states.get(&ip).map(|s| s.depth).unwrap_or(0)
                );
            }
            match label_states.get(&ip) {
                Some(st) => {
                    e.entries.clear();
                    e.entries.resize(st.depth as usize, Entry::Temp);
                    e.tdz = st.tdz.clone();
                    live = true;
                }
                None => live = false, // label never reached from anywhere
            }
            e.last_def = None;
        }
        rpc_at[ip as usize] = e.code.len() as u32;
        if !live {
            // Unreachable op: emit nothing (jumps around it resolve to the
            // next emitted position).
            ip += 1;
            continue;
        }
        let (fallthrough, consumed) = e.emit_op(code, ip, &is_label)?;
        live = fallthrough;
        for k in 0..consumed {
            tdz_transfer(&code[(ip + k) as usize], &mut e.tdz);
            if k > 0 {
                rpc_at[(ip + k) as usize] = e.code.len() as u32;
            }
        }
        ip += consumed;
    }
    rpc_at[code.len()] = e.code.len() as u32;
    // Falling off the end of the code returns like the stack loop's
    // `ip >= code.len()` exit; also the landing pad for end-of-code jump
    // targets, so pc can never overrun.
    e.code.push(ROp::RetCompletion);
    // Remap branch targets from stack ips to rpcs.
    for op in &mut e.code {
        if let Some(t) = rop_target_mut(op) {
            *t = rpc_at[*t as usize];
        }
    }
    let num_regs = e.max_reg as u32 + 1;
    if num_regs > u16::MAX as u32 {
        return None;
    }
    let ic = (0..e.ics)
        .map(|_| IcEntry {
            own_slot: std::cell::Cell::new(u32::MAX),
            proto_slot: std::cell::Cell::new(u32::MAX),
            holder: std::cell::RefCell::new(None),
        })
        .collect();
    Some(RegProto {
        code: e.code,
        num_regs: num_regs as u16,
        ic,
    })
}

/// Mutable access to an op's branch target for the final remap.
fn rop_target_mut(op: &mut ROp) -> Option<&mut u32> {
    match op {
        ROp::Jmp { target }
        | ROp::BrTrue { target, .. }
        | ROp::BrFalse { target, .. }
        | ROp::BrFalsyKeep { target, .. }
        | ROp::BrTruthyKeep { target, .. }
        | ROp::BrNotNullishKeep { target, .. }
        | ROp::BrNullishUndef { target, .. }
        | ROp::CmpBr { target, .. }
        | ROp::CmpBrK { target, .. }
        | ROp::CmpBrCellK { target, .. } => Some(target),
        _ => None,
    }
}

/// Rewrite the destination register of a single-`dst` op (the `StoreLocal`
/// peephole). Must list every op the emitter marks as `last_def`.
fn rop_set_dst(op: &mut ROp, new_dst: u16) -> bool {
    match op {
        ROp::Mov { dst, .. }
        | ROp::Const { dst, .. }
        | ROp::Undef { dst }
        | ROp::Null { dst }
        | ROp::Bool { dst, .. }
        | ROp::Hole { dst }
        | ROp::This { dst }
        | ROp::NewTarget { dst }
        | ROp::Arg { dst, .. }
        | ROp::RestArgs { dst, .. }
        | ROp::Arguments { dst }
        | ROp::LoadCell { dst, .. }
        | ROp::AddCellK { dst, .. }
        | ROp::ArithCellK { dst, .. }
        | ROp::LoadUpvalue { dst, .. }
        | ROp::LoadGlobal { dst, .. }
        | ROp::LoadGlobalTypeof { dst, .. }
        | ROp::Add { dst, .. }
        | ROp::AddK { dst, .. }
        | ROp::Arith { dst, .. }
        | ROp::ArithK { dst, .. }
        | ROp::Unary { dst, .. }
        | ROp::Cmp { dst, .. }
        | ROp::InstanceOf { dst, .. }
        | ROp::GetProp { dst, .. }
        | ROp::SetProp { dst, .. }
        | ROp::GetElem { dst, .. }
        | ROp::SetElem { dst, .. }
        | ROp::DelProp { dst, .. }
        | ROp::DelElem { dst, .. }
        | ROp::HasProp { dst, .. }
        | ROp::Call { dst, .. }
        | ROp::New { dst, .. }
        | ROp::CallSpread { dst, .. }
        | ROp::NewSpread { dst, .. }
        | ROp::Closure { dst, .. }
        | ROp::NewObject { dst }
        | ROp::NewArray { dst, .. }
        | ROp::GetTemplateObject { dst, .. }
        | ROp::NewRegExp { dst, .. }
        | ROp::ConcatStrings { dst, .. }
        | ROp::GetIterator { dst, .. }
        | ROp::IterNext { dst, .. } => {
            *dst = new_dst;
            true
        }
        _ => false,
    }
}

impl Emitter {
    fn canon(&self, depth: usize) -> u16 {
        self.num_locals + depth as u16
    }

    fn touch(&mut self, r: u16) {
        if r > self.max_reg {
            self.max_reg = r;
        }
    }

    fn push_op(&mut self, op: ROp) {
        self.last_def = None;
        self.code.push(op);
    }

    /// Emit an op whose single `dst` produced the new virtual-stack top
    /// (arming the `StoreLocal` rewrite peephole).
    fn push_def(&mut self, op: ROp) {
        self.code.push(op);
        self.last_def = Some(self.code.len() - 1);
    }

    /// Register that currently holds the value of `entries[pos]`, emitting a
    /// materialization for keyword/const entries (into the slot's canonical
    /// register — free, nothing else can occupy it).
    fn reg_of(&mut self, pos: usize) -> u16 {
        let c = self.canon(pos);
        match self.entries[pos] {
            Entry::Temp => c,
            Entry::Reg(r) => r,
            Entry::Local(l) => l,
            e => {
                self.materialize(pos, e, c);
                c
            }
        }
    }

    /// Force `entries[pos]` into its canonical register (for contiguous
    /// argument ranges and control-flow joins).
    fn canonicalize(&mut self, pos: usize) {
        let ent = self.entries[pos];
        if ent == Entry::Temp {
            return;
        }
        let c = self.canon(pos);
        match ent {
            Entry::Reg(r) => self.push_op(ROp::Mov { dst: c, src: r }),
            Entry::Local(l) => self.push_op(ROp::Mov { dst: c, src: l }),
            e => self.materialize(pos, e, c),
        }
        self.touch(c);
        self.entries[pos] = Entry::Temp;
    }

    fn materialize(&mut self, pos: usize, e: Entry, dst: u16) {
        let op = match e {
            Entry::K(idx) => ROp::Const { dst, idx },
            Entry::Undef => ROp::Undef { dst },
            Entry::Null => ROp::Null { dst },
            Entry::True => ROp::Bool { dst, v: true },
            Entry::False => ROp::Bool { dst, v: false },
            Entry::Hole => ROp::Hole { dst },
            Entry::This => ROp::This { dst },
            Entry::NewTarget => ROp::NewTarget { dst },
            Entry::Temp | Entry::Reg(_) | Entry::Local(_) => unreachable!(),
        };
        self.push_op(op);
        self.touch(dst);
        self.entries[pos] = Entry::Temp;
    }

    /// Flush the whole virtual stack to canonical form (control-flow edges).
    fn flush_all(&mut self) {
        for pos in 0..self.entries.len() {
            self.canonicalize(pos);
        }
        self.last_def = None;
    }

    /// Flush any lazy references to local `l` (an op is about to write it).
    fn flush_local(&mut self, l: u16) -> bool {
        let mut any = false;
        for pos in 0..self.entries.len() {
            if self.entries[pos] == Entry::Local(l) {
                self.canonicalize(pos);
                any = true;
            }
        }
        any
    }

    fn pop(&mut self) -> Entry {
        self.last_def = None;
        self.entries.pop().expect("virtual stack underflow")
    }

    /// Pop the top entry and return a register holding its value.
    fn pop_reg(&mut self) -> u16 {
        let pos = self.entries.len() - 1;
        let r = self.reg_of(pos);
        self.entries.pop();
        self.last_def = None;
        r
    }

    /// Result register for an op producing at the current top.
    fn dst(&mut self) -> u16 {
        let c = self.canon(self.entries.len());
        self.touch(c);
        c
    }

    fn push_temp(&mut self) {
        self.entries.push(Entry::Temp);
    }

    fn next_ic(&mut self) -> u32 {
        let i = self.ics;
        self.ics += 1;
        i
    }

    /// Read of local `l`: TDZ-check when the dataflow could not prove the
    /// slot initialized, then push a lazy reference.
    fn push_local(&mut self, l: u32) {
        if self.tdz.get(l) {
            self.push_op(ROp::TdzCheck { src: l as u16 });
            self.tdz.clear(l);
        }
        self.entries.push(Entry::Local(l as u16));
    }

    /// TDZ guard for the fused local superinstructions (which read `local`
    /// directly as an operand).
    fn tdz_guard(&mut self, l: u32) {
        if self.tdz.get(l) {
            self.push_op(ROp::TdzCheck { src: l as u16 });
            self.tdz.clear(l);
        }
    }

    /// Store the popped top into local `l`, rewriting the defining op's `dst`
    /// when the value was produced by the immediately preceding op.
    fn store_local(&mut self, l: u16) {
        let needs_flush = self.entries.contains(&Entry::Local(l));
        if !needs_flush {
            if let (Some(def), Some(&Entry::Temp)) = (self.last_def, self.entries.last()) {
                if def + 1 == self.code.len() && rop_set_dst(&mut self.code[def], l) {
                    self.entries.pop();
                    self.last_def = None;
                    return;
                }
            }
            // A lazy top stores straight into the local.
            match self.entries.last().copied() {
                Some(Entry::Local(src)) if src == l => {
                    // x = x: nothing moves.
                    self.entries.pop();
                    self.last_def = None;
                    return;
                }
                Some(Entry::Local(src)) => {
                    self.entries.pop();
                    self.push_op(ROp::Mov { dst: l, src });
                    return;
                }
                Some(
                    e @ (Entry::K(_)
                    | Entry::Undef
                    | Entry::Null
                    | Entry::True
                    | Entry::False
                    | Entry::Hole
                    | Entry::This
                    | Entry::NewTarget),
                ) => {
                    let pos = self.entries.len() - 1;
                    self.materialize(pos, e, l);
                    self.entries.pop();
                    return;
                }
                _ => {}
            }
        } else {
            self.flush_local(l);
        }
        let src = self.pop_reg();
        self.push_op(ROp::Mov { dst: l, src });
    }

    /// Emit the translation of the op at `ip` (possibly fusing a following
    /// window). Returns `Some((live, consumed))` where `live` says whether
    /// execution can fall through and `consumed` how many stack ops the
    /// emission covered; `None` = untranslatable (unreachable here — phase 1
    /// vetted every op — but kept as a guard).
    fn emit_op(
        &mut self,
        code: &[Op],
        ip: u32,
        is_label: &dyn Fn(u32) -> bool,
    ) -> Option<(bool, u32)> {
        use Op::*;
        let op = &code[ip as usize];
        match op {
            Nop => {}
            LoadConst(i) => self.entries.push(Entry::K(*i)),
            LoadUndefined => self.entries.push(Entry::Undef),
            LoadHole | ArrayPushElision => self.entries.push(match op {
                LoadHole => Entry::Hole,
                // ArrayPushElision pushes plain `undefined` (see step_cold).
                _ => Entry::Undef,
            }),
            LoadNull => self.entries.push(Entry::Null),
            LoadTrue => self.entries.push(Entry::True),
            LoadFalse => self.entries.push(Entry::False),
            LoadThis => self.entries.push(Entry::This),
            LoadNewTarget => self.entries.push(Entry::NewTarget),
            LoadArg(i) => {
                let dst = self.dst();
                self.push_def(ROp::Arg { dst, idx: *i });
                self.push_temp();
            }
            LoadRestArgs(n) => {
                let dst = self.dst();
                self.push_def(ROp::RestArgs { dst, from: *n });
                self.push_temp();
            }
            LoadArguments => {
                let dst = self.dst();
                self.push_def(ROp::Arguments { dst });
                self.push_temp();
            }
            RequireObjectCoercible => {
                let pos = self.entries.len() - 1;
                let src = self.reg_of(pos);
                self.push_op(ROp::RequireObjectCoercible { src });
            }
            RequireIterResult => {
                let pos = self.entries.len() - 1;
                let src = self.reg_of(pos);
                self.push_op(ROp::RequireIterResult { src });
            }
            RequireCoercible => {
                let src = self.pop_reg();
                self.push_op(ROp::RequireCoercible { src });
            }
            BindThisSloppy => {
                // Rebind in place: materialize to canonical, mutate there.
                let pos = self.entries.len() - 1;
                self.canonicalize(pos);
                let reg = self.canon(pos);
                self.push_op(ROp::BindThisSloppy { reg });
            }

            // ---- locals ----
            LoadLocal(i) => self.push_local(*i),
            StoreLocal(i) => self.store_local(*i as u16),
            StoreLocalChecked(i) => {
                self.flush_local(*i as u16);
                let src = self.pop_reg();
                self.push_op(ROp::StoreLocalChecked {
                    local: *i as u16,
                    src,
                });
            }
            InitLocalTdz(i) => {
                self.flush_local(*i as u16);
                self.push_op(ROp::InitLocalTdz { local: *i as u16 });
            }
            LoadLocalConst { local, konst } => {
                self.push_local(*local);
                self.entries.push(Entry::K(*konst));
            }
            CmpLocalConstBranchFalse {
                local,
                konst,
                cmp,
                target,
            }
            | CmpLocalConstBranchTrue {
                local,
                konst,
                cmp,
                target,
            } => {
                self.tdz_guard(*local);
                self.flush_all();
                let if_true = matches!(op, CmpLocalConstBranchTrue { .. });
                self.push_op(ROp::CmpBrK {
                    cmp: *cmp,
                    a: *local as u16,
                    konst: *konst,
                    if_true,
                    target: *target,
                });
            }
            AddLocalConst { local, konst } => {
                self.tdz_guard(*local);
                let dst = self.dst();
                self.push_def(ROp::AddK {
                    dst,
                    a: *local as u16,
                    konst: *konst,
                });
                self.push_temp();
            }
            ArithLocalConst { local, konst, kind } => {
                self.tdz_guard(*local);
                let dst = self.dst();
                self.push_def(ROp::ArithK {
                    kind: *kind,
                    dst,
                    a: *local as u16,
                    konst: *konst,
                });
                self.push_temp();
            }
            IncLocalStmt { local, dec } => {
                self.flush_local(*local as u16);
                self.push_op(ROp::IncLocal {
                    local: *local as u16,
                    dec: *dec,
                });
            }
            CopyLocal { src, dest } => {
                self.tdz_guard(*src);
                self.flush_local(*dest as u16);
                if src != dest {
                    self.push_op(ROp::Mov {
                        dst: *dest as u16,
                        src: *src as u16,
                    });
                }
            }

            // ---- cells / upvalues ----
            LoadCell(i) => {
                let dst = self.dst();
                self.push_def(ROp::LoadCell { dst, cell: *i });
                self.push_temp();
            }
            LoadCellConst { cell, konst } => {
                let dst = self.dst();
                self.push_def(ROp::LoadCell { dst, cell: *cell });
                self.push_temp();
                self.entries.push(Entry::K(*konst));
            }
            StoreCell(i) => {
                let src = self.pop_reg();
                self.push_op(ROp::StoreCell { cell: *i, src });
            }
            StoreCellChecked(i) => {
                let src = self.pop_reg();
                self.push_op(ROp::StoreCellChecked { cell: *i, src });
            }
            InitCell(i) => {
                let src = self.pop_reg();
                self.push_op(ROp::InitCell { cell: *i, src });
            }
            InitCellTdz(i) => self.push_op(ROp::InitCellTdz { cell: *i }),
            IncCellStmt { cell, dec } => self.push_op(ROp::IncCell {
                cell: *cell,
                dec: *dec,
            }),
            LoadCellInit { src, dest } => self.push_op(ROp::LoadCellInit {
                src: *src,
                dest: *dest,
            }),
            AddCellConst { cell, konst } => {
                let dst = self.dst();
                self.push_def(ROp::AddCellK {
                    dst,
                    cell: *cell,
                    konst: *konst,
                });
                self.push_temp();
            }
            ArithCellConst { cell, konst, kind } => {
                let dst = self.dst();
                self.push_def(ROp::ArithCellK {
                    kind: *kind,
                    dst,
                    cell: *cell,
                    konst: *konst,
                });
                self.push_temp();
            }
            CmpCellConstBranchFalse {
                cell,
                konst,
                cmp,
                target,
            }
            | CmpCellConstBranchTrue {
                cell,
                konst,
                cmp,
                target,
            } => {
                self.flush_all();
                let if_true = matches!(op, CmpCellConstBranchTrue { .. });
                self.push_op(ROp::CmpBrCellK {
                    cmp: *cmp,
                    cell: *cell,
                    konst: *konst,
                    if_true,
                    target: *target,
                });
            }
            LoadUpvalue(i) => {
                let dst = self.dst();
                self.push_def(ROp::LoadUpvalue { dst, idx: *i });
                self.push_temp();
            }
            StoreUpvalue(i) => {
                let src = self.pop_reg();
                self.push_op(ROp::StoreUpvalue { idx: *i, src });
            }
            StoreUpvalueChecked(i) => {
                let src = self.pop_reg();
                self.push_op(ROp::StoreUpvalueChecked { idx: *i, src });
            }

            // ---- globals ----
            LoadGlobal(i) => {
                let dst = self.dst();
                let ic = self.next_ic();
                self.push_def(ROp::LoadGlobal { dst, name: *i, ic });
                self.push_temp();
            }
            StoreGlobal(i) => {
                let src = self.pop_reg();
                self.push_op(ROp::StoreGlobal { name: *i, src });
            }
            LoadGlobalTypeof(i) => {
                let dst = self.dst();
                self.push_def(ROp::LoadGlobalTypeof { dst, name: *i });
                self.push_temp();
            }
            DeclareGlobal { name, deletable } => self.push_op(ROp::DeclareGlobal {
                name: *name,
                deletable: *deletable,
            }),
            CanDeclareGlobalFunc(i) => self.push_op(ROp::CanDeclareGlobalFunc { name: *i }),
            DefineGlobalFunc { name, deletable } => {
                let src = self.pop_reg();
                self.push_op(ROp::DefineGlobalFunc {
                    name: *name,
                    deletable: *deletable,
                    src,
                });
            }

            // ---- stack manipulation ----
            Pop => {
                self.pop();
            }
            Dup => {
                // Method-call idiom `Dup; GetProp(name); Swap` (the compiler's
                // `o.m(...)` prologue): fuse so it pays ONE property op (plus
                // at most one move) instead of three dispatches.
                if let (Some(GetProp(name)), Some(Swap)) =
                    (code.get(ip as usize + 1), code.get(ip as usize + 2))
                {
                    if !is_label(ip + 1) && !is_label(ip + 2) {
                        let pos = self.entries.len() - 1;
                        let obj = match self.entries[pos] {
                            Entry::Local(l) => l,
                            Entry::Reg(r) => r,
                            Entry::Temp => {
                                // Move the receiver up to the post-swap slot.
                                let up = self.canon(pos + 1);
                                self.touch(up);
                                self.push_op(ROp::Mov {
                                    dst: up,
                                    src: self.canon(pos),
                                });
                                self.entries[pos] = Entry::Temp; // method lands here
                                up
                            }
                            e => {
                                // Materialize the lazy receiver directly into
                                // the post-swap slot.
                                let up = self.canon(pos + 1);
                                // materialize() writes entries[pos]; emit by hand.
                                let mat = match e {
                                    Entry::K(idx) => ROp::Const { dst: up, idx },
                                    Entry::Undef => ROp::Undef { dst: up },
                                    Entry::Null => ROp::Null { dst: up },
                                    Entry::True => ROp::Bool { dst: up, v: true },
                                    Entry::False => ROp::Bool { dst: up, v: false },
                                    Entry::Hole => ROp::Hole { dst: up },
                                    Entry::This => ROp::This { dst: up },
                                    Entry::NewTarget => ROp::NewTarget { dst: up },
                                    _ => unreachable!(),
                                };
                                self.push_op(mat);
                                self.touch(up);
                                up
                            }
                        };
                        let dst = self.canon(pos);
                        self.touch(dst);
                        let ic = self.next_ic();
                        self.push_op(ROp::GetProp {
                            dst,
                            obj,
                            name: *name,
                            ic,
                        });
                        // Post-swap shape: [method (canonical), receiver].
                        let recv = match self.entries[pos] {
                            Entry::Local(l) => Entry::Local(l),
                            Entry::Reg(r) => Entry::Reg(r),
                            _ => Entry::Temp,
                        };
                        self.entries[pos] = Entry::Temp;
                        self.entries.push(match recv {
                            // The receiver moved to canon(pos + 1) above.
                            Entry::Temp => Entry::Temp,
                            other => other,
                        });
                        return Some((true, 3));
                    }
                }
                let pos = self.entries.len() - 1;
                let dup = match self.entries[pos] {
                    // Alias the canonical slot below (sound: r < canon(pos+1)).
                    Entry::Temp => Entry::Reg(self.canon(pos)),
                    e => e, // lazy entries (and existing aliases) copy freely
                };
                self.entries.push(dup);
            }
            Swap => self.swap_top2(),
            Rot3 => {
                // a b c -> b c a: flush the three entries, rotate via scratch.
                let n = self.entries.len();
                for pos in n - 3..n {
                    self.canonicalize(pos);
                }
                let (a, b, c) = (self.canon(n - 3), self.canon(n - 2), self.canon(n - 1));
                let scratch = self.canon(n);
                self.touch(scratch);
                self.push_op(ROp::Mov {
                    dst: scratch,
                    src: a,
                });
                self.push_op(ROp::Mov { dst: a, src: b });
                self.push_op(ROp::Mov { dst: b, src: c });
                self.push_op(ROp::Mov {
                    dst: c,
                    src: scratch,
                });
            }

            // ---- objects / arrays ----
            NewObject => {
                let dst = self.dst();
                self.push_def(ROp::NewObject { dst });
                self.push_temp();
            }
            NewArray(n) => {
                let n = *n as usize;
                let base = self.entries.len() - n;
                for pos in base..self.entries.len() {
                    self.canonicalize(pos);
                }
                let at = self.canon(base);
                self.entries.truncate(base);
                let dst = self.dst();
                self.push_def(ROp::NewArray {
                    dst,
                    at,
                    n: n as u16,
                });
                self.push_temp();
            }
            GetTemplateObject(idx) => {
                let dst = self.dst();
                self.push_def(ROp::GetTemplateObject { dst, idx: *idx });
                self.push_temp();
            }
            ArraySpread => {
                let src = self.pop_reg();
                let pos = self.entries.len() - 1;
                let arr = self.reg_of(pos);
                self.push_op(ROp::ArraySpread { arr, src });
                // The array entry stays as the result.
            }
            DefineField | DefineMethod | DefineGetter | DefineSetter | DefineMethodGetter
            | DefineMethodSetter => {
                let val = self.pop_reg();
                let key = self.pop_reg();
                let pos = self.entries.len() - 1;
                let obj = self.reg_of(pos);
                let kind = match op {
                    DefineField => DefKind::Field,
                    DefineMethod => DefKind::Method,
                    DefineGetter => DefKind::Getter,
                    DefineSetter => DefKind::Setter,
                    DefineMethodGetter => DefKind::MethodGetter,
                    _ => DefKind::MethodSetter,
                };
                self.push_op(ROp::DefineProp {
                    kind,
                    obj,
                    key,
                    val,
                });
                // The object entry stays as the result.
            }
            SetHomeObject => {
                let n = self.entries.len();
                let val = self.reg_of(n - 1);
                let obj = self.reg_of(n - 3);
                self.push_op(ROp::SetHomeObject { obj, val });
            }
            ObjectSpread => {
                let src = self.pop_reg();
                let pos = self.entries.len() - 1;
                let target = self.reg_of(pos);
                self.push_op(ROp::ObjectSpread { target, src });
            }
            CopyDataPropertiesExcept(n) => {
                let n = *n as usize;
                let base = self.entries.len() - n;
                for pos in base..self.entries.len() {
                    self.canonicalize(pos);
                }
                let at = self.canon(base);
                self.entries.truncate(base);
                let src = self.pop_reg();
                let pos = self.entries.len() - 1;
                let target = self.reg_of(pos);
                self.push_op(ROp::CopyDataPropsExcept {
                    target,
                    src,
                    at,
                    n: n as u16,
                });
            }
            GetProp(i) => {
                let obj = self.pop_reg();
                let dst = self.dst();
                let ic = self.next_ic();
                self.push_def(ROp::GetProp {
                    dst,
                    obj,
                    name: *i,
                    ic,
                });
                self.push_temp();
            }
            SetProp(i) => {
                let src = self.pop_reg();
                let obj = self.pop_reg();
                let dst = self.dst();
                let ic = self.next_ic();
                self.push_def(ROp::SetProp {
                    dst,
                    obj,
                    name: *i,
                    src,
                    ic,
                });
                self.push_temp();
            }
            GetPropDynamic => {
                let key = self.pop_reg();
                let obj = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::GetElem { dst, obj, key });
                self.push_temp();
            }
            SetPropDynamic => {
                let src = self.pop_reg();
                let key = self.pop_reg();
                let obj = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::SetElem { dst, obj, key, src });
                self.push_temp();
            }
            DeleteProp(i) => {
                let obj = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::DelProp { dst, obj, name: *i });
                self.push_temp();
            }
            DeletePropDynamic => {
                let key = self.pop_reg();
                let obj = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::DelElem { dst, obj, key });
                self.push_temp();
            }
            HasProp => {
                let obj = self.pop_reg();
                let key = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::HasProp { dst, key, obj });
                self.push_temp();
            }
            SetFunctionNameFromKey(prefix) => {
                let n = self.entries.len();
                let val = self.reg_of(n - 1);
                let key = self.reg_of(n - 2);
                self.push_op(ROp::SetFunctionNameFromKey {
                    prefix: *prefix,
                    key,
                    val,
                });
            }
            SetProtoFromLiteral => {
                let src = self.pop_reg();
                let pos = self.entries.len() - 1;
                let obj = self.reg_of(pos);
                self.push_op(ROp::SetProtoFromLiteral { obj, src });
            }

            // ---- calls ----
            Call(argc) | CallMethodless(argc) => {
                let has_this = matches!(op, Call(_));
                let argc = *argc as usize;
                let base = self.entries.len() - argc;
                for pos in base..self.entries.len() {
                    self.canonicalize(pos);
                }
                let at = self.canon(base);
                self.entries.truncate(base);
                let this = if has_this { self.pop_reg() } else { 0 };
                let func = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::Call {
                    dst,
                    func,
                    this,
                    at,
                    argc: argc as u16,
                    has_this,
                });
                self.push_temp();
            }
            New(argc) => {
                let argc = *argc as usize;
                let base = self.entries.len() - argc;
                for pos in base..self.entries.len() {
                    self.canonicalize(pos);
                }
                let at = self.canon(base);
                self.entries.truncate(base);
                let ctor = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::New {
                    dst,
                    ctor,
                    at,
                    argc: argc as u16,
                });
                self.push_temp();
            }
            CallSpread => {
                let args = self.pop_reg();
                let this = self.pop_reg();
                let func = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::CallSpread {
                    dst,
                    func,
                    this,
                    args,
                });
                self.push_temp();
            }
            NewSpread => {
                let args = self.pop_reg();
                let ctor = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::NewSpread { dst, ctor, args });
                self.push_temp();
            }
            Return => {
                let src = self.pop_reg();
                self.push_op(ROp::Ret { src });
                return Some((false, 1));
            }
            ReturnUndefined => {
                self.push_op(ROp::RetCompletion);
                return Some((false, 1));
            }
            Throw => {
                let src = self.pop_reg();
                self.push_op(ROp::Throw { src });
                return Some((false, 1));
            }
            ThrowConstAssign => {
                self.push_op(ROp::ThrowConstAssign);
                return Some((false, 1));
            }

            Closure(i) => {
                let dst = self.dst();
                self.push_def(ROp::Closure { dst, idx: *i });
                self.push_temp();
            }

            // ---- arithmetic / unary ----
            Add => self.add(),
            Sub => self.arith(ArithKind::Sub),
            Mul => self.arith(ArithKind::Mul),
            Div => self.arith(ArithKind::Div),
            Mod => self.arith(ArithKind::Mod),
            Pow => self.arith(ArithKind::Pow),
            BitAnd => self.arith(ArithKind::BitAnd),
            BitOr => self.arith(ArithKind::BitOr),
            BitXor => self.arith(ArithKind::BitXor),
            Shl => self.arith(ArithKind::Shl),
            Shr => self.arith(ArithKind::Shr),
            UShr => self.arith(ArithKind::UShr),
            Neg => self.unary(RUnary::Neg),
            Pos => self.unary(RUnary::Pos),
            ToNumeric => self.unary(RUnary::ToNumeric),
            Inc => self.unary(RUnary::Inc),
            Dec => self.unary(RUnary::Dec),
            BitNot => self.unary(RUnary::BitNot),
            Not => self.unary(RUnary::Not),
            TypeofExpr => self.unary(RUnary::Typeof),
            ToPropertyKey => self.unary(RUnary::ToKey),
            ToStringOp => self.unary(RUnary::ToStr),

            // ---- comparison ----
            Eq => self.cmp(CmpOp::Eq),
            Ne => self.cmp(CmpOp::Ne),
            StrictEq => self.cmp(CmpOp::StrictEq),
            StrictNe => self.cmp(CmpOp::StrictNe),
            Lt => self.cmp(CmpOp::Lt),
            Gt => self.cmp(CmpOp::Gt),
            Le => self.cmp(CmpOp::Le),
            Ge => self.cmp(CmpOp::Ge),
            InstanceOf => {
                let b = self.pop_reg();
                let a = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::InstanceOf { dst, a, b });
                self.push_temp();
            }

            // ---- control flow ----
            Jump(t) => {
                self.flush_all();
                self.push_op(ROp::Jmp { target: *t });
                return Some((false, 1));
            }
            JumpIfTrue(t) | JumpIfFalse(t) => {
                // Pop the condition BEFORE flushing (its slot is dead on both
                // edges), then flush for the join.
                let src = self.pop_reg();
                self.flush_all();
                let opv = if matches!(op, JumpIfTrue(_)) {
                    ROp::BrTrue { src, target: *t }
                } else {
                    ROp::BrFalse { src, target: *t }
                };
                self.push_op(opv);
            }
            CmpBranchFalse { cmp, target } | CmpBranchTrue { cmp, target } => {
                let if_true = matches!(op, CmpBranchTrue { .. });
                // K-fold the right operand when it is a lazy constant.
                let konst = match self.entries.last() {
                    Some(Entry::K(k)) => {
                        let k = *k;
                        self.entries.pop();
                        self.last_def = None;
                        Some(k)
                    }
                    _ => None,
                };
                match konst {
                    Some(k) => {
                        let a = self.pop_reg();
                        self.flush_all();
                        self.push_op(ROp::CmpBrK {
                            cmp: *cmp,
                            a,
                            konst: k,
                            if_true,
                            target: *target,
                        });
                    }
                    None => {
                        let b = self.pop_reg();
                        let a = self.pop_reg();
                        self.flush_all();
                        self.push_op(ROp::CmpBr {
                            cmp: *cmp,
                            a,
                            b,
                            if_true,
                            target: *target,
                        });
                    }
                }
            }
            JumpIfFalsyPeek(t) | JumpIfTruthyPeek(t) | JumpIfNullishPeek(t) => {
                // The target edge keeps the value: it must sit in its
                // canonical slot on BOTH edges.
                let pos = self.entries.len() - 1;
                self.canonicalize(pos);
                self.flush_all();
                let src = self.canon(pos);
                let opv = match op {
                    JumpIfFalsyPeek(_) => ROp::BrFalsyKeep { src, target: *t },
                    JumpIfTruthyPeek(_) => ROp::BrTruthyKeep { src, target: *t },
                    _ => ROp::BrNotNullishKeep { src, target: *t },
                };
                self.push_op(opv);
                // Fallthrough pops the value.
                self.entries.pop();
            }
            JumpIfNullish(t) => {
                // Both edges keep the value (target sees `undefined`).
                let pos = self.entries.len() - 1;
                self.canonicalize(pos);
                self.flush_all();
                let reg = self.canon(pos);
                self.push_op(ROp::BrNullishUndef { reg, target: *t });
            }

            // ---- iteration ----
            GetIterator => {
                let src = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::GetIterator { dst, src });
                self.push_temp();
            }
            IteratorNext => {
                let pos = self.entries.len() - 1;
                let it = self.reg_of(pos);
                let dst = self.dst();
                self.push_def(ROp::IterNext { dst, it });
                self.push_temp();
            }
            ForInEnumerate => {
                let src = self.pop_reg();
                let dst = self.dst();
                self.push_def(ROp::ForInEnumerate { dst, src });
                self.push_temp();
            }
            ForInNext => {
                let dst = self.dst();
                self.touch(dst + 1);
                self.push_op(ROp::ForInNext { dst });
                self.push_temp();
                self.push_temp();
            }
            ForInPop => self.push_op(ROp::ForInPop),

            ConcatStrings(n) => {
                let n = *n as usize;
                let base = self.entries.len() - n;
                for pos in base..self.entries.len() {
                    self.canonicalize(pos);
                }
                let at = self.canon(base);
                self.entries.truncate(base);
                let dst = self.dst();
                self.push_def(ROp::ConcatStrings {
                    dst,
                    at,
                    n: n as u16,
                });
                self.push_temp();
            }
            NewRegExp { pattern, flags } => {
                let dst = self.dst();
                self.push_def(ROp::NewRegExp {
                    dst,
                    pattern: *pattern,
                    flags: *flags,
                });
                self.push_temp();
            }

            _ => return None,
        }
        Some((true, 1))
    }

    fn add(&mut self) {
        // K-fold a lazy-const right operand into the AddK form.
        if let Some(Entry::K(k)) = self.entries.last() {
            let k = *k;
            self.entries.pop();
            self.last_def = None;
            let a = self.pop_reg();
            let dst = self.dst();
            self.push_def(ROp::AddK { dst, a, konst: k });
            self.push_temp();
            return;
        }
        let b = self.pop_reg();
        let a = self.pop_reg();
        let dst = self.dst();
        self.push_def(ROp::Add { dst, a, b });
        self.push_temp();
    }

    fn arith(&mut self, kind: ArithKind) {
        if let Some(Entry::K(k)) = self.entries.last() {
            let k = *k;
            self.entries.pop();
            self.last_def = None;
            let a = self.pop_reg();
            let dst = self.dst();
            self.push_def(ROp::ArithK {
                kind,
                dst,
                a,
                konst: k,
            });
            self.push_temp();
            return;
        }
        let b = self.pop_reg();
        let a = self.pop_reg();
        let dst = self.dst();
        self.push_def(ROp::Arith { kind, dst, a, b });
        self.push_temp();
    }

    fn unary(&mut self, kind: RUnary) {
        let src = self.pop_reg();
        let dst = self.dst();
        self.push_def(ROp::Unary { kind, dst, src });
        self.push_temp();
    }

    fn cmp(&mut self, cmp: CmpOp) {
        let b = self.pop_reg();
        let a = self.pop_reg();
        let dst = self.dst();
        self.push_def(ROp::Cmp { cmp, dst, a, b });
        self.push_temp();
    }

    /// `Swap` of the top two entries, preserving the alias invariants.
    fn swap_top2(&mut self) {
        let n = self.entries.len();
        let (i, j) = (n - 2, n - 1);
        let (a, b) = (self.entries[i], self.entries[j]);
        let lazy = |e: Entry| !matches!(e, Entry::Temp | Entry::Reg(_));
        match (a, b) {
            // Two lazy entries (or a below-alias moving down) swap freely.
            _ if lazy(a) && lazy(b) => self.entries.swap(i, j),
            // b is the dup alias of a (`Reg(canon(i))` above a Temp): the
            // stack holds the same value twice; a swap is a no-op.
            (Entry::Temp, Entry::Reg(r)) if r == self.canon(i) => {}
            (Entry::Temp, Entry::Reg(r)) => {
                // r aliases some slot below i, so it is a valid alias at i too.
                self.push_op(ROp::Mov {
                    dst: self.canon(j),
                    src: self.canon(i),
                });
                self.touch(self.canon(j));
                self.entries[i] = Entry::Reg(r);
                self.entries[j] = Entry::Temp;
            }
            (Entry::Temp, Entry::Temp) => {
                // Full three-move rotation through a scratch register.
                let (ci, cj) = (self.canon(i), self.canon(j));
                let scratch = self.canon(n);
                self.touch(scratch);
                self.push_op(ROp::Mov {
                    dst: scratch,
                    src: ci,
                });
                self.push_op(ROp::Mov { dst: ci, src: cj });
                self.push_op(ROp::Mov {
                    dst: cj,
                    src: scratch,
                });
            }
            (Entry::Temp, _b_lazy) => {
                // Value in canon(i) moves up; the lazy entry moves down.
                self.push_op(ROp::Mov {
                    dst: self.canon(j),
                    src: self.canon(i),
                });
                self.touch(self.canon(j));
                self.entries[i] = b;
                self.entries[j] = Entry::Temp;
            }
            (Entry::Reg(r), Entry::Temp) => {
                // a aliases r (< canon(i)); moving it up keeps it valid, and
                // the value at canon(j) moves down into canon(i).
                self.push_op(ROp::Mov {
                    dst: self.canon(i),
                    src: self.canon(j),
                });
                self.entries[i] = Entry::Temp;
                self.entries[j] = Entry::Reg(r);
            }
            (Entry::Reg(_), _) => {
                // Alias + lazy: both position-independent.
                self.entries.swap(i, j);
            }
            (_a_lazy, Entry::Temp) => {
                self.push_op(ROp::Mov {
                    dst: self.canon(i),
                    src: self.canon(j),
                });
                self.entries[j] = a;
                self.entries[i] = Entry::Temp;
            }
            (_a_lazy, Entry::Reg(r)) if r == self.canon(i) => {
                // b aliases canon(i), but slot i holds a LAZY entry — canon(i)
                // is a stale register only reachable through b; materialize b
                // up to canonical and swap the lazies.
                self.push_op(ROp::Mov {
                    dst: self.canon(j),
                    src: r,
                });
                self.touch(self.canon(j));
                self.entries[j] = Entry::Temp;
                self.entries.swap(i, j);
            }
            // Remaining combinations (lazy/lazy, lazy over a valid below-
            // alias) are position-independent: swap entries, move nothing.
            _ => self.entries.swap(i, j),
        }
        self.last_def = None;
    }
}
