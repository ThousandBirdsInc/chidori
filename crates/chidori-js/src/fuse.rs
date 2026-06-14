//! Peephole **superinstruction / op-fusion** pass (Phase 2 of
//! `docs/interpreter-optimization.md`).
//!
//! Runs once per compiled [`FuncProto`](crate::bytecode::FuncProto) in
//! `Compiler::finish`, rewriting high-frequency adjacent opcode sequences into
//! single fused ops. Fusion reduces both the number of dispatches through the
//! interpreter's central `match` (fewer branch mispredictions) and the operand-
//! stack traffic of the intermediate values the sequence would push and pop.
//!
//! ## Correctness invariants
//!
//! 1. **Observably identical.** Each fused op must produce exactly the result,
//!    side effects, thrown errors, and ordering of the sequence it replaces. The
//!    fused-op handlers in `exec.rs` reuse the *same* helpers as the standalone
//!    ops, so the only difference is the elided intermediate stack value — which
//!    is never visible to JS.
//! 2. **Control flow preserved.** Fusion shortens `code`, which shifts the
//!    absolute jump targets that index into it. Every code-offset operand is
//!    remapped through [`for_each_ip`] — the single source of truth for "which
//!    operands are instruction pointers" (it deliberately skips handler-stack
//!    *depths* and the `u32::MAX` "none" sentinels). A window is fused only when
//!    nothing jumps *into the middle* of it (`is_target[mid] == false`); jumps to
//!    the *first* op of a window stay valid because entering the fused op is
//!    equivalent to entering the original sequence at its head.
//! 3. **Fallback always exists.** Fusion is a pure optimization; compiling with
//!    it disabled (`compile_script_opts(src, fuse = false)`) yields bytecode that
//!    must execute identically. The differential test in `tests/fusion.rs`
//!    enforces this over a corpus, and `for_each_ip` keeps record→replay
//!    deterministic because it perturbs neither values nor host-call ordering.

use crate::bytecode::{CmpOp, Op};

/// Apply `f` to every operand of `op` that is a **code offset** (an absolute ip
/// into the function's `code`). This is the *only* place that enumerates which
/// operands are instruction pointers; both target discovery and retargeting go
/// through it, so they can never disagree.
///
/// Deliberately excluded:
/// - `CompletionJump.boundary` — a handler-stack *depth*, not an ip.
/// - the `u32::MAX` "none" sentinels of `PushTryHandler.finally` and
///   `MarkDelegationHandler` — remapping them would corrupt the sentinel.
///
/// Every other `u32`/index operand in the instruction set is a const index,
/// local/cell index, argument count, or similar — never an ip.
///
/// NOTE: if a future opcode gains a code-offset operand, it MUST be added here,
/// or the fusion pass will silently miscompile control flow. The differential
/// test (`tests/fusion.rs`) is the backstop that would catch such an omission.
fn for_each_ip(op: &mut Op, mut f: impl FnMut(&mut u32)) {
    match op {
        Op::Jump(t)
        | Op::JumpIfTrue(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfFalsyPeek(t)
        | Op::JumpIfTruthyPeek(t)
        | Op::JumpIfNullishPeek(t)
        | Op::JumpIfNullish(t) => f(t),
        Op::CmpBranchFalse { target, .. } => f(target),
        Op::PushTryHandler { catch, finally } => {
            f(catch);
            if *finally != u32::MAX {
                f(finally);
            }
        }
        // `target` is an ip; `boundary` is a handler-stack depth — do NOT remap.
        Op::CompletionJump { target, .. } => f(target),
        Op::MarkDelegationHandler(t) => {
            if *t != u32::MAX {
                f(t);
            }
        }
        _ => {}
    }
}

/// The comparison kind a `cmp ; JumpIfFalse` pair fuses to, if `op` is one of the
/// fuseable comparison opcodes.
fn cmp_of(op: &Op) -> Option<CmpOp> {
    Some(match op {
        Op::Eq => CmpOp::Eq,
        Op::Ne => CmpOp::Ne,
        Op::StrictEq => CmpOp::StrictEq,
        Op::StrictNe => CmpOp::StrictNe,
        Op::Lt => CmpOp::Lt,
        Op::Gt => CmpOp::Gt,
        Op::Le => CmpOp::Le,
        Op::Ge => CmpOp::Ge,
        _ => return None,
    })
}

/// Rewrite `code` in place, fusing recognized adjacent sequences. Safe to call
/// on already-final bytecode (all jump targets absolute). Idempotent in effect:
/// re-running over fused code is a no-op for the patterns it recognizes.
pub fn fuse_code(code: Vec<Op>) -> Vec<Op> {
    let n = code.len();
    if n < 2 {
        return code;
    }

    // 1. Mark every instruction that is the target of some jump/handler. A window
    //    may not be fused if anything jumps into its interior.
    let mut is_target = vec![false; n];
    {
        // for_each_ip needs &mut; clone targets out without mutating `code`.
        let mut tmp = code.clone();
        for op in &mut tmp {
            for_each_ip(op, |t| {
                let i = *t as usize;
                if i < n {
                    is_target[i] = true;
                }
            });
        }
    }

    // 2. Single forward pass: emit fused or copied ops, recording where each old
    //    index lands in the new code (`old_to_new`). The map has `n + 1` entries:
    //    a forward jump may legitimately target `code.len()` (one past the end —
    //    the compiler's `here()` at the tail, which exits the frame), and that
    //    sentinel must remap to the NEW length, not be left dangling.
    let mut out: Vec<Op> = Vec::with_capacity(n);
    let mut old_to_new = vec![0u32; n + 1];
    let mut i = 0usize;
    while i < n {
        // Candidate: `cmp ; JumpIfFalse(target)` with nothing jumping between them.
        if i + 1 < n {
            if let (Some(cmp), Op::JumpIfFalse(target)) = (cmp_of(&code[i]), &code[i + 1]) {
                if !is_target[i + 1] {
                    old_to_new[i] = out.len() as u32;
                    // The fused op replaces both slots; old index i+1 is interior
                    // and never a jump target, so its mapping is unobservable.
                    old_to_new[i + 1] = out.len() as u32;
                    out.push(Op::CmpBranchFalse {
                        cmp,
                        target: *target,
                    });
                    i += 2;
                    continue;
                }
            }
        }
        old_to_new[i] = out.len() as u32;
        out.push(code[i].clone());
        i += 1;
    }
    // The one-past-the-end target maps to the new end-of-code.
    old_to_new[n] = out.len() as u32;

    // 3. Remap every code-offset operand through the recorded mapping.
    for op in &mut out {
        for_each_ip(op, |t| {
            let old = *t as usize;
            if old <= n {
                *t = old_to_new[old];
            }
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cmp ; JumpIfFalse` fuses, the code shortens by one, and every jump
    /// target past the fusion point is remapped to its new index.
    #[test]
    fn fuses_and_remaps_targets() {
        // 0:LoadConst 1:LoadConst 2:Lt 3:JumpIfFalse(6) 4:Nop 5:Jump(0) 6:Return
        let code = vec![
            Op::LoadConst(0),
            Op::LoadConst(1),
            Op::Lt,
            Op::JumpIfFalse(6),
            Op::Nop,
            Op::Jump(0),
            Op::Return,
        ];
        let out = fuse_code(code);
        // One slot shorter: the pair became a single op.
        assert_eq!(out.len(), 6);
        assert!(
            matches!(
                out[2],
                Op::CmpBranchFalse {
                    cmp: CmpOp::Lt,
                    target: 5
                }
            ),
            "Lt;JumpIfFalse(6) should fuse and remap 6 -> 5 (Return shifted left): {:?}",
            out[2]
        );
        // The back-edge target 0 is unaffected; Return moved from 6 to 5.
        assert!(matches!(out[4], Op::Jump(0)));
        assert!(matches!(out[5], Op::Return));
    }

    /// A jump landing ON the `JumpIfFalse` (the interior of a candidate window)
    /// blocks fusion: that instruction must remain independently addressable.
    #[test]
    fn does_not_fuse_into_a_jump_target() {
        // 0:Lt 1:JumpIfFalse(3) 2:Jump(1) 3:Return — index 1 is a jump target.
        let code = vec![Op::Lt, Op::JumpIfFalse(3), Op::Jump(1), Op::Return];
        let out = fuse_code(code);
        assert_eq!(out.len(), 4, "nothing should fuse");
        assert!(!out.iter().any(|op| matches!(op, Op::CmpBranchFalse { .. })));
        assert!(matches!(out[1], Op::JumpIfFalse(3)));
        assert!(matches!(out[2], Op::Jump(1)));
    }

    /// `boundary` of `CompletionJump` is a handler DEPTH, not an ip, and must
    /// survive the remap unchanged even as `target` is remapped.
    #[test]
    fn completion_jump_boundary_is_not_remapped() {
        // Fuse the leading pair so indices shift, then check a CompletionJump.
        let code = vec![
            Op::Lt,
            Op::JumpIfFalse(4),
            Op::Nop,
            Op::Nop,
            Op::CompletionJump {
                target: 0,
                boundary: 2,
            },
        ];
        let out = fuse_code(code);
        // Pair fused: len 4; CompletionJump now at index 3.
        match out[3] {
            Op::CompletionJump { target, boundary } => {
                assert_eq!(target, 0, "target remaps to the (unchanged) head index");
                assert_eq!(boundary, 2, "boundary is a depth and must be untouched");
            }
            ref other => panic!("expected CompletionJump, got {other:?}"),
        }
    }
}
