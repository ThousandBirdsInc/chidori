//! Experimental closure-threading "JIT" for `chidori-js`.
//!
//! ## What this is (and what it deliberately is not)
//!
//! `docs/interpreter-optimization.md` argues at length against a *native-code*
//! JIT for this engine: native codegen needs `unsafe` executable memory (or a
//! heavyweight backend such as Cranelift), it fights the deterministic-replay
//! contract with compile/deopt timing nondeterminism, and a measured
//! agent-replay benchmark shows JS execution is well under 1% of an agent's live
//! wall-clock. All three reasons stand. This module is therefore **not** a
//! native JIT — it is a *closure-threading* execution backend that stays inside
//! every one of the engine's load-bearing invariants:
//!
//! * **Zero `unsafe`.** It is ordinary safe Rust: a `Vec` of boxed closures.
//! * **No new dependencies.** Only `std`.
//! * **Byte-identical deterministic replay.** It is a *pure performance side
//!   effect*: each compiled op produces exactly the result, side effects, thrown
//!   errors, and host-call ordering of the switch interpreter's `step`, because
//!   the hot ops reuse the **same** helper functions `step` calls and every op
//!   that isn't specialized delegates to `step` itself. The
//!   [`Vm::jit_enabled`](crate::vm::Vm::jit_enabled) toggle lets the test suite
//!   run both backends and assert their outputs and journals match
//!   (`tests/jit.rs`) — the toggle-equivalence property the determinism contract
//!   calls for.
//!
//! ## How it goes faster
//!
//! The switch interpreter pays, per executed op: a central `match self.step(..)`
//! (one indirect branch the predictor mispredicts constantly), operand decoding
//! out of `&Op`, and the construction + re-match of a `Result<Ctl, Value>`. The
//! closure thread compiles a [`FuncProto`] once into one closure per op, with
//! the operands **pre-decoded into the closure's captures**. Dispatch becomes a
//! direct call through a per-ip function pointer; the hot ops (loads, cell/local
//! access, arithmetic, comparisons, branches) run their tiny body inline and
//! return `Ctl::Next`/`Ctl::Jump` without revisiting the giant match. Some
//! per-op work is also hoisted to compile time — e.g. a cell's
//! `stable_cells` membership is resolved once here instead of a `Vec::contains`
//! on every `InitCell`.
//!
//! See `docs/jit.md` for the design, the determinism argument, and measured
//! results (including a deterministic specialized-vs-fallback dispatch proxy,
//! since this environment's wall-clock noise floor is ~10–15%, per
//! `docs/interpreter-optimization.md` §7.6).

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::{CmpOp, FuncProto, Op};
use crate::exec::{bin_arith, ArithKind, Ctl, UnaryKind};
use crate::value::Value;
use crate::vm::{Frame, Vm};

/// One compiled operation. Takes the VM and the running frame and returns the
/// **same** `Result<Ctl, Value>` the switch interpreter's `step` returns for the
/// op this was lowered from — so the surrounding `run_frame` driver handles it
/// with byte-identical control flow whether the result came from a closure or
/// from `step`.
pub(crate) type OpFn = Box<dyn Fn(&mut Vm, &mut Frame) -> Result<Ctl, Value>>;

/// The closure-threaded form of a [`FuncProto`]: exactly one [`OpFn`] per
/// bytecode op, at the same index. Because the thread is index-parallel to
/// `proto.code`, jump targets (absolute code offsets) carry over unchanged — no
/// remapping, no separate encoding.
pub struct JitThread {
    /// Crate-private because `OpFn` names the crate-private `Ctl`; external code
    /// inspects the thread via [`JitThread::op_count`] instead.
    pub(crate) ops: Vec<OpFn>,
    /// Count of ops lowered to a specialized inline closure.
    pub specialized: u32,
    /// Count of ops that delegate to `Vm::step` (the long tail). A
    /// deterministic, environment-independent proxy: `specialized` is the number
    /// of central-match dispatches the JIT removes per pass over the code.
    pub fallback: u32,
}

impl JitThread {
    /// Number of compiled ops (index-parallel to the proto's bytecode).
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }
}

/// Interior-mutable, lazily-populated cache of a proto's [`JitThread`], stored on
/// the [`FuncProto`] itself. Compiled at most once per proto; its lifetime is
/// the proto's. A pure performance side effect — never serialized, never
/// observed by the journal, rebuilt from scratch on every fresh `Vm`.
pub struct JitCache(RefCell<Option<Rc<JitThread>>>);

impl JitCache {
    pub fn new() -> Self {
        JitCache(RefCell::new(None))
    }

    /// Return the cached thread, compiling it on first use. Clones the `Rc` out
    /// and releases the borrow before returning, so a reentrant activation of
    /// the same proto (direct recursion, e.g. `fib`) never double-borrows the
    /// cell.
    pub fn get_or_compile(&self, proto: &FuncProto) -> Rc<JitThread> {
        if let Some(t) = self.0.borrow().as_ref() {
            return t.clone();
        }
        let thread = Rc::new(compile(proto));
        *self.0.borrow_mut() = Some(thread.clone());
        thread
    }

    /// Whether this proto has been compiled yet (for tests/introspection).
    pub fn is_compiled(&self) -> bool {
        self.0.borrow().is_some()
    }
}

impl Default for JitCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for JitCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = if self.0.borrow().is_some() {
            "compiled"
        } else {
            "uncompiled"
        };
        write!(f, "JitCache({state})")
    }
}

/// `pop!()` in `step` is `frame.stack.pop().unwrap_or(Value::Undefined)`; mirror
/// it exactly (an under-running stack yields `undefined`, never a panic) so the
/// specialized closures can't diverge from the interpreter on a malformed stack.
#[inline(always)]
fn pop(frame: &mut Frame) -> Value {
    frame.stack.pop().unwrap_or(Value::Undefined)
}

/// Box a closure as an [`OpFn`]. Boxing `Fn(&mut Vm, &mut Frame)` closures
/// *inline* (`Box::new(..) as OpFn`) trips the compiler's "implementation of
/// `Fn` is not general enough" error, because an inline closure's `&mut`
/// parameter lifetimes get inferred as a single concrete pair rather than the
/// higher-ranked `for<'a, 'b>` the trait object needs. Passing the closure
/// through this generic constructor — whose bound uses elided (hence
/// higher-ranked) lifetimes — forces the right `for<'a, 'b>` quantification.
#[inline(always)]
fn boxed<F>(f: F) -> OpFn
where
    F: Fn(&mut Vm, &mut Frame) -> Result<Ctl, Value> + 'static,
{
    Box::new(f)
}

/// Lower one bytecode op to an [`OpFn`]. Returns `(closure, specialized)` where
/// `specialized` is `false` for the `step`-delegating fallback.
///
/// Every specialized arm below is a transcription of the corresponding arm of
/// `Vm::step` (`src/exec.rs`) that reuses the identical helper (`op_add`,
/// `bin_arith`, `less_than`, `loose_equals`, `strict_equals`, `unary_arith`,
/// `to_boolean`, `to_number`, `to_numeric`, `const_val`) — so coercion order,
/// thrown error type/site, and ±0/NaN/BigInt behavior are identical by
/// construction. The full test suite runs with the JIT on by default and
/// `tests/jit.rs` diffs JIT-on vs JIT-off output and journals; any drift fails.
fn lower(proto: &FuncProto, op: &Op) -> (OpFn, bool) {
    macro_rules! spec {
        ($body:expr) => {
            (boxed($body), true)
        };
    }
    match *op {
        // ---- constants / literals ----
        Op::LoadConst(i) => spec!(move |vm, frame| {
            let v = vm.const_val(frame, i);
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),
        Op::LoadUndefined => spec!(move |_vm, frame| {
            frame.stack.push(Value::Undefined);
            Ok(Ctl::Next)
        }),
        Op::LoadHole => spec!(move |_vm, frame| {
            frame.stack.push(Value::Hole);
            Ok(Ctl::Next)
        }),
        Op::LoadNull => spec!(move |_vm, frame| {
            frame.stack.push(Value::Null);
            Ok(Ctl::Next)
        }),
        Op::LoadTrue => spec!(move |_vm, frame| {
            frame.stack.push(Value::Bool(true));
            Ok(Ctl::Next)
        }),
        Op::LoadFalse => spec!(move |_vm, frame| {
            frame.stack.push(Value::Bool(false));
            Ok(Ctl::Next)
        }),
        Op::LoadThis => spec!(move |_vm, frame| {
            let v = frame.this.clone();
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),
        Op::LoadArg(i) => spec!(move |_vm, frame| {
            let v = frame
                .args
                .get(i as usize)
                .cloned()
                .unwrap_or(Value::Undefined);
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),

        // ---- locals ----
        Op::LoadLocal(i) => spec!(move |_vm, frame| {
            let v = frame.locals[i as usize].clone();
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),
        Op::StoreLocal(i) => spec!(move |_vm, frame| {
            let v = pop(frame);
            frame.locals[i as usize] = v;
            Ok(Ctl::Next)
        }),

        // ---- cells (with the identical TDZ check) ----
        Op::LoadCell(i) => spec!(move |vm, frame| {
            let v = frame.cells[i as usize].borrow().clone();
            if matches!(v, Value::Uninitialized) {
                return Err(vm.throw_reference("Cannot access binding before initialization"));
            }
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),
        Op::LoadCellConst { cell, konst } => spec!(move |vm, frame| {
            let v = frame.cells[cell as usize].borrow().clone();
            if matches!(v, Value::Uninitialized) {
                return Err(vm.throw_reference("Cannot access binding before initialization"));
            }
            frame.stack.push(v);
            let k = vm.const_val(frame, konst);
            frame.stack.push(k);
            Ok(Ctl::Next)
        }),
        Op::StoreCell(i) => spec!(move |_vm, frame| {
            let v = pop(frame);
            *frame.cells[i as usize].borrow_mut() = v;
            Ok(Ctl::Next)
        }),
        // `stable_cells` membership is immutable per proto, so resolve it once at
        // compile time instead of a `Vec::contains` on every execution.
        Op::InitCell(i) => {
            let stable = proto.stable_cells.contains(&i);
            spec!(move |_vm, frame| {
                let v = pop(frame);
                if stable {
                    *frame.cells[i as usize].borrow_mut() = v;
                } else {
                    frame.cells[i as usize] = Rc::new(RefCell::new(v));
                }
                Ok(Ctl::Next)
            })
        }
        Op::InitCellTdz(i) => {
            let stable = proto.stable_cells.contains(&i);
            spec!(move |_vm, frame| {
                if stable {
                    *frame.cells[i as usize].borrow_mut() = Value::Uninitialized;
                } else {
                    frame.cells[i as usize] = Rc::new(RefCell::new(Value::Uninitialized));
                }
                Ok(Ctl::Next)
            })
        }

        // ---- upvalues (TDZ check identical to step) ----
        Op::LoadUpvalue(i) => spec!(move |vm, frame| {
            let v = frame.func.upvalues[i as usize].borrow().clone();
            if matches!(v, Value::Uninitialized) {
                return Err(vm.throw_reference("Cannot access binding before initialization"));
            }
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),
        Op::StoreUpvalue(i) => spec!(move |_vm, frame| {
            let v = pop(frame);
            *frame.func.upvalues[i as usize].borrow_mut() = v;
            Ok(Ctl::Next)
        }),

        // ---- stack manipulation ----
        Op::Pop => spec!(move |_vm, frame| {
            frame.stack.pop();
            Ok(Ctl::Next)
        }),
        Op::Dup => spec!(move |_vm, frame| {
            let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
            frame.stack.push(v);
            Ok(Ctl::Next)
        }),
        Op::Swap => spec!(move |_vm, frame| {
            let n = frame.stack.len();
            if n >= 2 {
                frame.stack.swap(n - 1, n - 2);
            }
            Ok(Ctl::Next)
        }),
        Op::Rot3 => spec!(move |_vm, frame| {
            let n = frame.stack.len();
            if n >= 3 {
                frame.stack[n - 3..].rotate_left(1);
            }
            Ok(Ctl::Next)
        }),

        // ---- arithmetic / unary (same helpers as step) ----
        Op::Add => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.op_add(a, b)?;
            frame.stack.push(r);
            Ok(Ctl::Next)
        }),
        Op::Sub => spec!(arith(ArithKind::Sub)),
        Op::Mul => spec!(arith(ArithKind::Mul)),
        Op::Div => spec!(arith(ArithKind::Div)),
        Op::Mod => spec!(arith(ArithKind::Mod)),
        Op::Pow => spec!(arith(ArithKind::Pow)),
        Op::BitAnd => spec!(arith(ArithKind::BitAnd)),
        Op::BitOr => spec!(arith(ArithKind::BitOr)),
        Op::BitXor => spec!(arith(ArithKind::BitXor)),
        Op::Shl => spec!(arith(ArithKind::Shl)),
        Op::Shr => spec!(arith(ArithKind::Shr)),
        Op::UShr => spec!(arith(ArithKind::UShr)),
        Op::Neg => spec!(move |vm, frame| {
            let a = pop(frame);
            let r = vm.unary_arith(a, UnaryKind::Neg)?;
            frame.stack.push(r);
            Ok(Ctl::Next)
        }),
        Op::BitNot => spec!(move |vm, frame| {
            let a = pop(frame);
            let r = vm.unary_arith(a, UnaryKind::BitNot)?;
            frame.stack.push(r);
            Ok(Ctl::Next)
        }),
        Op::Pos => spec!(move |vm, frame| {
            let a = pop(frame);
            let n = vm.to_number(&a)?;
            frame.stack.push(Value::Number(n));
            Ok(Ctl::Next)
        }),
        Op::Not => spec!(move |vm, frame| {
            let a = pop(frame);
            let r = vm.to_boolean(&a);
            frame.stack.push(Value::Bool(!r));
            Ok(Ctl::Next)
        }),
        Op::ToNumeric => spec!(move |vm, frame| {
            let a = pop(frame);
            let r = vm.to_numeric(&a)?;
            frame.stack.push(r);
            Ok(Ctl::Next)
        }),
        Op::TypeofExpr => spec!(move |_vm, frame| {
            let a = pop(frame);
            frame.stack.push(Value::str(a.type_of()));
            Ok(Ctl::Next)
        }),

        // ---- comparison (same helpers as step) ----
        Op::Eq => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.loose_equals(&a, &b)?;
            frame.stack.push(Value::Bool(r));
            Ok(Ctl::Next)
        }),
        Op::Ne => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.loose_equals(&a, &b)?;
            frame.stack.push(Value::Bool(!r));
            Ok(Ctl::Next)
        }),
        Op::StrictEq => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            frame.stack.push(Value::Bool(vm.strict_equals(&a, &b)));
            Ok(Ctl::Next)
        }),
        Op::StrictNe => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            frame.stack.push(Value::Bool(!vm.strict_equals(&a, &b)));
            Ok(Ctl::Next)
        }),
        Op::Lt => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.less_than(&a, &b)?;
            frame.stack.push(Value::Bool(r == Some(true)));
            Ok(Ctl::Next)
        }),
        Op::Gt => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.less_than(&b, &a)?;
            frame.stack.push(Value::Bool(r == Some(true)));
            Ok(Ctl::Next)
        }),
        Op::Le => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.less_than(&b, &a)?;
            frame.stack.push(Value::Bool(r == Some(false)));
            Ok(Ctl::Next)
        }),
        Op::Ge => spec!(move |vm, frame| {
            let b = pop(frame);
            let a = pop(frame);
            let r = vm.less_than(&a, &b)?;
            frame.stack.push(Value::Bool(r == Some(false)));
            Ok(Ctl::Next)
        }),

        // ---- control flow ----
        Op::Jump(t) => {
            let t = t as usize;
            spec!(move |_vm, _frame| Ok(Ctl::Jump(t)))
        }
        Op::JumpIfTrue(t) => {
            let t = t as usize;
            spec!(move |vm, frame| {
                let v = pop(frame);
                if vm.to_boolean(&v) {
                    Ok(Ctl::Jump(t))
                } else {
                    Ok(Ctl::Next)
                }
            })
        }
        Op::JumpIfFalse(t) => {
            let t = t as usize;
            spec!(move |vm, frame| {
                let v = pop(frame);
                if !vm.to_boolean(&v) {
                    Ok(Ctl::Jump(t))
                } else {
                    Ok(Ctl::Next)
                }
            })
        }
        Op::CmpBranchFalse { cmp, target } => {
            let target = target as usize;
            spec!(move |vm, frame| {
                let b = pop(frame);
                let a = pop(frame);
                let r = eval_cmp(vm, cmp, &a, &b)?;
                if !r {
                    Ok(Ctl::Jump(target))
                } else {
                    Ok(Ctl::Next)
                }
            })
        }
        Op::CmpBranchTrue { cmp, target } => {
            let target = target as usize;
            spec!(move |vm, frame| {
                let b = pop(frame);
                let a = pop(frame);
                let r = eval_cmp(vm, cmp, &a, &b)?;
                if r {
                    Ok(Ctl::Jump(target))
                } else {
                    Ok(Ctl::Next)
                }
            })
        }
        Op::JumpIfFalsyPeek(t) => {
            let t = t as usize;
            spec!(move |vm, frame| {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if !vm.to_boolean(&v) {
                    return Ok(Ctl::Jump(t));
                }
                frame.stack.pop();
                Ok(Ctl::Next)
            })
        }
        Op::JumpIfTruthyPeek(t) => {
            let t = t as usize;
            spec!(move |vm, frame| {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if vm.to_boolean(&v) {
                    return Ok(Ctl::Jump(t));
                }
                frame.stack.pop();
                Ok(Ctl::Next)
            })
        }
        Op::JumpIfNullishPeek(t) => {
            let t = t as usize;
            spec!(move |_vm, frame| {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if !v.is_nullish() {
                    return Ok(Ctl::Jump(t));
                }
                frame.stack.pop();
                Ok(Ctl::Next)
            })
        }

        // ---- everything else: delegate to the reference interpreter ----
        // Cloning the op keeps the closure `'static`. This covers calls,
        // property access, object/array construction, generators/async, `with`,
        // private elements, `super`, exceptions, iteration, modules — the full
        // long tail, executed with byte-identical semantics because it *is*
        // `step`.
        ref other => {
            let op = other.clone();
            (boxed(move |vm, frame| vm.step(frame, &op)), false)
        }
    }
}

/// Build a specialized closure for a binary arithmetic/bitwise op. `bin_arith`
/// is the exact function `step` invokes (it pops `b` then `a` off the frame
/// stack and pushes the result), so coercion order and thrown errors match.
#[inline]
fn arith(kind: ArithKind) -> impl Fn(&mut Vm, &mut Frame) -> Result<Ctl, Value> {
    move |vm, frame| {
        bin_arith(vm, frame, kind)?;
        Ok(Ctl::Next)
    }
}

/// Evaluate a fused-comparison operand pair exactly as `step`'s `CmpBranch*`
/// arms do — same helpers, same coercion, same thrown error.
#[inline]
fn eval_cmp(vm: &mut Vm, cmp: CmpOp, a: &Value, b: &Value) -> Result<bool, Value> {
    Ok(match cmp {
        CmpOp::Eq => vm.loose_equals(a, b)?,
        CmpOp::Ne => !vm.loose_equals(a, b)?,
        CmpOp::StrictEq => vm.strict_equals(a, b),
        CmpOp::StrictNe => !vm.strict_equals(a, b),
        CmpOp::Lt => vm.less_than(a, b)? == Some(true),
        CmpOp::Gt => vm.less_than(b, a)? == Some(true),
        CmpOp::Le => vm.less_than(b, a)? == Some(false),
        CmpOp::Ge => vm.less_than(a, b)? == Some(false),
    })
}

/// Compile a whole [`FuncProto`] into its closure thread.
fn compile(proto: &FuncProto) -> JitThread {
    let mut ops: Vec<OpFn> = Vec::with_capacity(proto.code.len());
    let mut specialized = 0u32;
    let mut fallback = 0u32;
    for op in &proto.code {
        let (f, was_specialized) = lower(proto, op);
        if was_specialized {
            specialized += 1;
        } else {
            fallback += 1;
        }
        ops.push(f);
    }
    JitThread {
        ops,
        specialized,
        fallback,
    }
}
