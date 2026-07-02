//! Cells→locals localization pass (the §3.2 item of
//! `docs/js-performance-roadmap.md`).
//!
//! The binding model compiles **every** source binding as a heap cell
//! (`Rc<RefCell<Value>>`) so that closure capture is always correct. But most
//! bindings are never captured: loop counters, temporaries, parameters of
//! leaf functions. For those, the cell buys nothing and costs a pool
//! round-trip per call plus `Rc`-deref + `RefCell`-borrow on every access.
//!
//! This pass runs once per function at compile finish (before op fusion) and
//! rewrites accesses to provably-uncaptured cells into `frame.locals` slots
//! (a flat, pooled `Vec<Value>`):
//!
//! - `LoadCell(i)`          → `LoadLocal(l)`      (same TDZ check)
//! - `StoreCell(i)`         → `StoreLocal(l)`
//! - `StoreCellChecked(i)`  → `StoreLocalChecked(l)`
//! - `InitCell(i)`          → `StoreLocal(l)`     (fresh-`Rc` identity is
//!   unobservable when nothing can capture the binding)
//! - `InitCellTdz(i)`       → `InitLocalTdz(l)`
//!
//! ## Why this never changes behavior
//!
//! A cell index is **protected** (stays a cell, at its ORIGINAL index) when
//! anything could observe cell identity or reach the binding from outside
//! this frame:
//!
//! - it is captured by a nested function (some direct child proto lists it as
//!   `UpvalueSource::ParentCell`) — grandchildren capture transitively
//!   through the child's own upvalue list, so direct children are exhaustive;
//! - it is in `stable_cells` (module top-level bindings pre-wired by the
//!   linker; a derived constructor's `%this` watched by [[Construct]]);
//! - it is `this_cell` or a mapped-`arguments` parameter alias;
//! - it is the operand of `BindThisCell` (`super()` writes `%this` in place).
//!
//! Because protected cells keep their original indices, child protos'
//! `ParentCell` references stay valid with **no child patching**; the
//! localized indices simply become dummy slots in the frame's cell vec
//! (filled with one shared placeholder `Rc`, never accessed).
//!
//! The whole function **bails** (nothing is localized) when any binding
//! could be resolved dynamically at runtime — direct `eval` (its scope
//! descriptors address cells by index), `with` (the `*Name`/`*Base` ops
//! carry `Load/StoreCell` fallbacks), or an `arguments` object (parameter
//! aliasing). Bailing preserves the status quo exactly.
//!
//! Determinism: this is a compile-time-only rewrite; for a given source it
//! always produces the same bytecode, and the runtime semantics of every
//! rewritten op are identical to the original (same values, same TDZ
//! ReferenceErrors, same coercion order). The `tests/localize.rs`
//! differential corpus asserts localize-on ≡ localize-off observable
//! behavior; Test262 and the replay byte-identity suite gate it end to end.

use crate::bytecode::{Const, Op, UpvalueSource};

pub struct Localized {
    pub code: Vec<Op>,
    pub num_locals: u32,
    /// Per original cell index: `true` when the index was rewritten to a
    /// local (its cell-vec slot is a never-read placeholder).
    pub localized: Box<[bool]>,
}

fn bail(code: Vec<Op>, num_cells: u32) -> Localized {
    Localized {
        code,
        num_locals: 0,
        localized: vec![false; num_cells as usize].into_boxed_slice(),
    }
}

/// `true` for ops that can resolve a binding dynamically (or carry a nested
/// static fallback op) — presence of any means the function must keep every
/// binding as a cell.
fn is_dynamic_resolution(op: &Op) -> bool {
    matches!(
        op,
        Op::LoadName { .. }
            | Op::StoreName { .. }
            | Op::DeleteName(_)
            | Op::ResolveNameBase(_)
            | Op::LoadFromBase { .. }
            | Op::StoreToBase { .. }
            | Op::PushWithScope
            | Op::PopWithScope
            | Op::DirectEval { .. }
            | Op::InitEvalVars
    )
}

#[allow(clippy::too_many_arguments)]
pub fn localize(
    code: Vec<Op>,
    num_cells: u32,
    consts: &[Const],
    stable_cells: &[u32],
    this_cell: Option<u32>,
    mapped_param_cells: &[Option<u32>],
    uses_arguments: bool,
    has_eval_scopes: bool,
) -> Localized {
    let n = num_cells as usize;
    if n == 0 {
        return bail(code, num_cells);
    }
    if uses_arguments || has_eval_scopes || code.iter().any(is_dynamic_resolution) {
        return bail(code, num_cells);
    }

    let mut protected = vec![false; n];
    for &c in stable_cells {
        protected[c as usize] = true;
    }
    if let Some(t) = this_cell {
        protected[t as usize] = true;
    }
    for c in mapped_param_cells.iter().flatten() {
        protected[*c as usize] = true;
    }
    for op in &code {
        if let Op::BindThisCell(i) = op {
            protected[*i as usize] = true;
        }
    }
    // Everything a DIRECT child captures. Children are already finished when
    // the parent reaches this pass, so their upvalue lists are final.
    for c in consts {
        if let Const::Func(f) = c {
            for uv in &f.upvalues {
                if let UpvalueSource::ParentCell(i) = uv {
                    protected[*i as usize] = true;
                }
            }
        }
    }

    // Dense local numbering for the unprotected indices.
    let mut local_of = vec![u32::MAX; n];
    let mut num_locals = 0u32;
    for (i, prot) in protected.iter().enumerate() {
        if !prot {
            local_of[i] = num_locals;
            num_locals += 1;
        }
    }
    if num_locals == 0 {
        return bail(code, num_cells);
    }

    let mut out = code;
    for op in &mut out {
        let rewrite = match *op {
            Op::LoadCell(i) if !protected[i as usize] => Op::LoadLocal(local_of[i as usize]),
            Op::StoreCell(i) if !protected[i as usize] => Op::StoreLocal(local_of[i as usize]),
            Op::StoreCellChecked(i) if !protected[i as usize] => {
                Op::StoreLocalChecked(local_of[i as usize])
            }
            Op::InitCell(i) if !protected[i as usize] => Op::StoreLocal(local_of[i as usize]),
            Op::InitCellTdz(i) if !protected[i as usize] => Op::InitLocalTdz(local_of[i as usize]),
            _ => continue,
        };
        *op = rewrite;
    }

    let localized: Box<[bool]> = protected.iter().map(|p| !p).collect();
    Localized {
        code: out,
        num_locals,
        localized,
    }
}
