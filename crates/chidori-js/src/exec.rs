//! VM execution: the per-frame interpreter loop, binary/unary operators, and the
//! call/construct/closure machinery.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::{CmpOp, Const, FuncKind, KOp, Op, UpvalueSource, KWIN, KWIN_MASK};
use crate::value::*;
use crate::vm::*;

/// Peek a call op's callee: `Some(bf)` when it is a PLAIN bytecode function —
/// synchronous, not a generator, not a class constructor — i.e. the class the
/// interpreter can run via [`Vm::call_direct`] without any of the generic
/// path's special-casing. Everything else (native, bound, proxy, async,
/// generator, class ctor, non-callable) returns `None` and takes the generic
/// path, which owns the error reporting for the non-callable case.
#[inline]
fn peek_plain_bytecode(v: &Value) -> Option<Rc<BytecodeFunction>> {
    if let Value::Object(o) = v {
        if let Internal::Function(FunctionInner::Bytecode(bf)) = &o.borrow().internal {
            let k = bf.proto.kind;
            if !bf.is_class_ctor && !k.is_generator() && !k.is_async() {
                return Some(bf.clone());
            }
        }
    }
    None
}

/// A callback prepared once per native higher-order invocation — the
/// invocation-invariant slice of the `run_fn_kernel` entry guard plus the
/// register buffer, hoisted out of the per-element call. Built by
/// [`Vm::prepare_kernel_callback`], executed by [`Vm::exec_prepared_kernel`].
pub(crate) struct PreparedKernel {
    bf: Rc<BytecodeFunction>,
    /// Kernel register window (fixed size; masked, bounds-check-free access).
    regs: [f64; crate::bytecode::KWIN],
    /// Back-edge interrupt poll counter (cadence spans calls).
    poll: u32,
    interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Control-flow outcome of a register-mode completion dispatch (the
/// register tier neither suspends nor delegates, so this is `Ctl` minus
/// those arms).
enum RegCtl {
    Jump(usize),
    Return(Value),
}

/// Control-flow signal from a single opcode step.
enum Ctl {
    Next,
    Jump(usize),
    Return(Value),
    Await(Value),
    Yield(Value),
    YieldStar(Value),
    GeneratorStart,
}

/// One slot-verified global identity check for a cached [`RecFamily`]:
/// `globals.props` slot `slot` must still carry `key` as a plain data
/// property holding the very closure `funcs[func]`. Insertion slots are
/// stable under value writes and appends, so a hit costs an indexed read +
/// two pointer compares — no hashing; a restructured table or a rebound
/// name fails the key/identity match and the family re-resolves from
/// scratch (today's full path).
pub(crate) struct RecGlobalCheck {
    key: PropertyKey,
    slot: u32,
    func: u8,
}

/// A resolved recursion FAMILY for the windowed recursive-kernel executor
/// ([`Vm::run_fn_kernel_rec`]), cached on the Vm keyed by the entry
/// closure's identity (`funcs[0]`). Everything here is a pure function of
/// the member closures (immutable protos) plus the recorded identity
/// checks, so a validated hit skips resolution entirely: the per-activation
/// cost drops from half a dozen table allocations + O(code) scans per
/// member to O(members) pointer compares. Holding member closures alive
/// across activations is the same retention class as the prototype-holder
/// inline caches ([`crate::bytecode::IcEntry`]); the cache is bounded and
/// eviction is only ever a perf event, never observable.
pub(crate) struct RecFamily {
    /// The family members; `funcs[0]` is the entry closure (cache key).
    funcs: Vec<Rc<BytecodeFunction>>,
    /// `callee_map[j][c]` = `funcs` index for member `j`'s `SelfCall`
    /// callee `c` (`c = 0` is `j` itself; `c = 1 + i` is `rec.globals[i]`).
    callee_map: Vec<Vec<u8>>,
    /// Per-member register-window size.
    /// Per-member (register, argument index) entry loads.
    arg_slots: Vec<Vec<(usize, usize)>>,
    /// Per-member (register, upvalue index, snapshot) — the VALUE is
    /// refreshed on every activation (cells can change between calls, never
    /// during one).
    uv_snaps: Vec<Vec<(usize, u32, f64)>>,
    /// Every global binding the family's guard depends on.
    global_checks: Vec<RecGlobalCheck>,
    /// (member, upvalue index) self-references: the cell must hold that
    /// member's own closure.
    upval_self_checks: Vec<(u8, u32)>,
    /// Members whose kernels use `Math` intrinsics (canonicals re-verified
    /// per activation).
    math_members: Vec<u8>,
    /// The family's uniform `Ret` register type.
    ret_bool: bool,
}

impl Vm {
    // =====================================================================
    // Calling
    // =====================================================================

    /// Call `func` with `this` and `args`. Never suspends the caller: async
    /// callees return a promise, generator callees return a generator object.
    pub fn call(&mut self, func: Value, this: Value, args: &[Value]) -> Result<Value, Value> {
        if let Value::Object(o) = &func {
            let o = o.clone();
            let (callable, is_proxy) = {
                let b = o.borrow();
                (b.is_callable(), matches!(b.internal, Internal::Proxy(_)))
            };
            // A callable Proxy ([[Call]] forwards to the apply trap / target);
            // checked before the ordinary function path, since a proxy now
            // reports `is_callable` via its captured flag.
            if is_proxy {
                if callable {
                    return self.proxy_call(&o, this, args);
                }
            } else if callable {
                return self.call_object(&o, this, args, Value::Undefined);
            }
        }
        let desc = self.describe(&func);
        Err(self.throw_type(&format!("{desc} is not a function")))
    }

    /// As [`Vm::call`], but takes OWNERSHIP of the argument buffer. The
    /// interpreter's call ops route here so a plain JS->JS call moves its
    /// (pooled) argument vec straight into the callee frame -- the &[Value]
    /// path copies the arguments a second time in make_frame. Native/bound/
    /// proxy callees borrow the vec and recycle it; every early error simply
    /// drops it (a pool miss, never a leak).
    ///
    /// This is the interpreter's hottest call entry, so the callable check and
    /// the dispatch extraction are ONE object borrow (the layered
    /// `call_object_vec` path borrows once for `is_callable` and again for the
    /// dispatch), and the depth guard is applied here directly.
    pub(crate) fn call_valuevec(
        &mut self,
        func: Value,
        this: Value,
        args: Vec<Value>,
    ) -> Result<Value, Value> {
        enum Disp {
            Native(NativeFn),
            Bytecode(Rc<BytecodeFunction>),
            Bound(JsObject, Value, Vec<Value>),
            Proxy,
            NotCallable,
        }
        if let Value::Object(o) = &func {
            let disp = {
                let b = o.borrow();
                match &b.internal {
                    Internal::Function(FunctionInner::Native(nf)) => Disp::Native(nf.func.clone()),
                    Internal::Function(FunctionInner::Bytecode(bf)) => Disp::Bytecode(bf.clone()),
                    Internal::Function(FunctionInner::Bound(bound)) => Disp::Bound(
                        bound.target.clone(),
                        bound.bound_this.clone(),
                        bound.bound_args.clone(),
                    ),
                    Internal::Proxy(p) if p.callable => Disp::Proxy,
                    _ => Disp::NotCallable,
                }
            };
            match disp {
                Disp::NotCallable => {}
                Disp::Proxy => {
                    let o = o.clone();
                    let r = self.proxy_call(&o, this, &args);
                    self.recycle_value_vec(args);
                    return r;
                }
                disp => {
                    self.call_depth += 1;
                    if self.call_depth > self.max_call_depth {
                        self.call_depth -= 1;
                        return Err(self.throw_range("Maximum call stack size exceeded"));
                    }
                    let r = match disp {
                        Disp::Bytecode(bf) => {
                            let o = o.clone();
                            self.call_bytecode_vec(&o, bf, this, args, Value::Undefined)
                        }
                        Disp::Native(f) => {
                            let r = f(self, this, &args);
                            self.recycle_value_vec(args);
                            r
                        }
                        Disp::Bound(target, bthis, bargs) => {
                            let mut all = bargs;
                            let mut args = args;
                            all.append(&mut args);
                            self.recycle_value_vec(args);
                            self.call_object_vec(&target, bthis, all, Value::Undefined)
                        }
                        Disp::Proxy | Disp::NotCallable => unreachable!(),
                    };
                    self.call_depth -= 1;
                    return r;
                }
            }
        }
        let desc = self.describe(&func);
        Err(self.throw_type(&format!("{desc} is not a function")))
    }

    pub(crate) fn call_object_vec(
        &mut self,
        obj: &JsObject,
        this: Value,
        args: Vec<Value>,
        new_target: Value,
    ) -> Result<Value, Value> {
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(self.throw_range("Maximum call stack size exceeded"));
        }
        let result = self.call_object_inner_vec(obj, this, args, new_target);
        self.call_depth -= 1;
        result
    }

    fn call_object_inner_vec(
        &mut self,
        obj: &JsObject,
        this: Value,
        mut args: Vec<Value>,
        new_target: Value,
    ) -> Result<Value, Value> {
        enum Disp {
            Native(NativeFn),
            Bytecode(Rc<BytecodeFunction>),
            Bound(JsObject, Value, Vec<Value>),
        }
        let disp = {
            let b = obj.borrow();
            match b.as_function() {
                Some(FunctionInner::Native(nf)) => Disp::Native(nf.func.clone()),
                Some(FunctionInner::Bytecode(bf)) => Disp::Bytecode(bf.clone()),
                Some(FunctionInner::Bound(bound)) => Disp::Bound(
                    bound.target.clone(),
                    bound.bound_this.clone(),
                    bound.bound_args.clone(),
                ),
                None => return Err(self.throw_type("not a function")),
            }
        };
        match disp {
            Disp::Native(f) => {
                let r = f(self, this, &args);
                self.recycle_value_vec(args);
                r
            }
            Disp::Bound(target, bthis, bargs) => {
                let mut all = bargs;
                all.append(&mut args);
                self.recycle_value_vec(args);
                self.call_object_vec(&target, bthis, all, new_target)
            }
            Disp::Bytecode(bf) => self.call_bytecode_vec(obj, bf, this, args, new_target),
        }
    }

    fn call_bytecode_vec(
        &mut self,
        func_obj: &JsObject,
        bf: Rc<BytecodeFunction>,
        this: Value,
        args: Vec<Value>,
        new_target: Value,
    ) -> Result<Value, Value> {
        let kind = bf.proto.kind;
        if bf.is_class_ctor {
            return Err(self.throw_type(&format!(
                "Class constructor {} cannot be invoked without 'new'",
                bf.proto.name
            )));
        }
        if kind.is_generator() {
            let r = self.make_generator(func_obj, bf, this, &args, new_target);
            self.recycle_value_vec(args);
            return r;
        }
        // Function kernel (kernel.rs): a tiny pure-scalar body executes
        // FRAMELESS when the entry guard passes. Only plain sync functions
        // ever carry one, so this can't intercept async/generator callees.
        if bf.proto.fn_kernel.is_some() {
            if let Some(r) = self.run_fn_kernel(&bf, &args) {
                self.recycle_value_vec(args);
                return r;
            }
        }
        let uses_arguments = bf.proto.uses_arguments;
        let mut frame = self.make_frame_owned(bf, this, args, new_target);
        // `func_obj` has exactly one consumer — the `arguments` object's
        // `callee` — so the per-call `Rc` round-trip is skipped for the
        // overwhelming majority of functions that never materialize it.
        if uses_arguments {
            frame.func_obj = Some(func_obj.clone());
        }
        let token = self.trace_enter(&frame.func.proto);
        frame.trace_token = token;
        if kind.is_async() {
            Ok(self.start_async(frame))
        } else {
            match self.run_frame(frame) {
                Flow::Return(v) => {
                    self.trace_exit(token, false);
                    Ok(v)
                }
                Flow::Throw(e) => {
                    self.trace_exit(token, true);
                    Err(e)
                }
                Flow::Suspend(_) => {
                    let _ = func_obj;
                    Err(self.throw_type("internal: sync function suspended"))
                }
            }
        }
    }

    /// The interpreter call ops' fast path (see `Op::Call`): the callee was
    /// already peeked as a plain (non-generator, non-async, non-class-ctor)
    /// bytecode function sitting on the caller's operand stack under `n`
    /// arguments at `at..`. Moves the arguments straight off the caller stack
    /// into the pooled callee frame — no intermediate pooled Vec round-trip —
    /// and reuses the popped function VALUE as the callee's `func_obj` (no
    /// refcount traffic). Behavior is identical to the generic
    /// `call_valuevec` → `call_bytecode_vec` chain for this callee class.
    fn call_direct(
        &mut self,
        caller: &mut Frame,
        at: usize,
        bf: Rc<BytecodeFunction>,
        has_this: bool,
    ) -> Result<Value, Value> {
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(self.throw_range("Maximum call stack size exceeded"));
        }
        // Function kernel: run frameless straight off the caller's operand
        // stack (the arguments sit at `at..`), then pop the ceremony.
        if bf.proto.fn_kernel.is_some() {
            if let Some(r) = self.run_fn_kernel(&bf, &caller.stack[at..]) {
                caller.stack.truncate(at);
                if has_this {
                    caller.stack.pop();
                }
                caller.stack.pop(); // the function value
                self.call_depth -= 1;
                return r;
            }
        }
        let mut callee = self.take_frame();
        callee.args.extend(caller.stack.drain(at..));
        let this = if has_this {
            caller.stack.pop().unwrap_or(Value::Undefined)
        } else {
            Value::Undefined
        };
        let func_v = caller.stack.pop().unwrap_or(Value::Undefined);
        let uses_arguments = bf.proto.uses_arguments;
        self.init_frame(&mut callee, bf, this, Value::Undefined);
        if uses_arguments {
            if let Value::Object(o) = func_v {
                callee.func_obj = Some(o);
            }
        }
        let token = self.trace_enter(&callee.func.proto);
        callee.trace_token = token;
        let r = match self.run_frame(callee) {
            Flow::Return(v) => {
                self.trace_exit(token, false);
                Ok(v)
            }
            Flow::Throw(e) => {
                self.trace_exit(token, true);
                Err(e)
            }
            Flow::Suspend(_) => Err(self.throw_type("internal: sync function suspended")),
        };
        self.call_depth -= 1;
        r
    }

    /// Verify a typed-array kernel base still resolves `.length` through the
    /// realm's canonical `%TypedArray%.prototype.length` getter: no own
    /// properties (nothing can shadow), and the FIRST `length` owner up the
    /// prototype chain is an accessor whose getter is the pinned canonical.
    /// Checked once per activation — nothing inside a kernel region can add
    /// properties or change prototypes. Any deviation declines the kernel and
    /// the generic path observes whatever the program set up.
    fn kernel_ta_len_ok(&self, o: &crate::value::JsObject) -> bool {
        let Some(canon) = &self.realm.ta_length_getter else {
            return false;
        };
        if !o.borrow().own_is_empty() {
            return false;
        }
        let key = crate::value::StrKeyRef("length");
        let mut cur = o.borrow().proto.clone();
        // The canonical chain is 2 hops (Float64Array.prototype →
        // %TypedArray%.prototype); a longer walk is already exotic. Bound it
        // so a pathological proto chain can't turn the entry guard O(n).
        for _ in 0..4 {
            let Some(p) = cur else { break };
            let b = p.borrow();
            if let Some(prop) = b.own_get(&key) {
                return matches!(
                    &prop.kind,
                    PropertyKind::Accessor { get: Some(Value::Object(g)), .. }
                        if g.ptr_eq(canon)
                );
            }
            cur = b.proto.clone();
        }
        false
    }

    /// Verify the global `Math` binding and each used method are still the
    /// realm's canonical objects, as plain data properties. Any deviation —
    /// deleted/replaced/shadowed `Math`, an accessor, a monkeypatched method
    /// — declines the kernel (the generic path then does whatever the
    /// program set up, observably).
    fn kernel_math_ok(&self, used: &[crate::bytecode::KMath]) -> bool {
        let Some(canon) = &self.realm.math_object else {
            return false;
        };
        {
            let g = self.realm.global.borrow();
            let math_ok = matches!(
                g.own_get(&crate::value::StrKeyRef("Math")),
                Some(Property {
                    kind: PropertyKind::Data { value: Value::Object(o), .. },
                    ..
                }) if o.ptr_eq(canon)
            );
            if !math_ok {
                return false;
            }
        }
        let mb = canon.borrow();
        used.iter().all(|k| {
            matches!(
                mb.own_get(&crate::value::StrKeyRef(k.name())),
                Some(Property {
                    kind: PropertyKind::Data { value: Value::Object(o), .. },
                    ..
                }) if o.ptr_eq(&self.realm.math_kernel[*k as usize])
            )
        })
    }

    /// Entry check for kernels containing [`KOp::ArrayPush`]/[`KOp::ArrayPop`]:
    /// the canonical Array prototype's method property must still be a plain
    /// DATA property holding the pinned canonical function (methods are
    /// writable). The receiver-side conditions (unshadowed, extensible,
    /// direct proto IS the canonical prototype) are entry-checked per
    /// receiver and re-checked per op; the element-creation chain walk is
    /// the `stores_elems` entry guard.
    fn kernel_array_method_ok(&self, name: &str, canon: &Option<JsObject>) -> bool {
        let Some(canon) = canon else {
            return false;
        };
        matches!(
            self.realm.array_proto.borrow().own_get(&crate::value::StrKeyRef(name)),
            Some(Property {
                kind: PropertyKind::Data { value: Value::Object(o), .. },
                ..
            }) if o.ptr_eq(canon)
        )
    }

    /// `KOp::CharCodeAt` entry guard: `String.prototype.charCodeAt` must
    /// still be a plain data property holding the canonical builtin (a
    /// patched/replaced/accessor'd method must run generically, observably).
    /// A string primitive receiver can carry no own shadow, and nothing
    /// inside a kernel can write props, so one entry check covers the
    /// activation.
    fn kernel_char_code_ok(&self) -> bool {
        let Some(canon) = &self.realm.string_char_code_at else {
            return false;
        };
        matches!(
            self.realm
                .string_proto
                .borrow()
                .own_get(&crate::value::StrKeyRef("charCodeAt")),
            Some(Property {
                kind: PropertyKind::Data { value: Value::Object(o), .. },
                ..
            }) if o.ptr_eq(canon)
        )
    }

    /// Execute the typed loop kernel at `Op::LoopKernel(idx)` (see
    /// `kernel.rs` for the model). Runs only when per-op accounting is off
    /// (no op budget), every mapped numeric local holds a `Number`, and every
    /// array-base local holds an object; otherwise the original header op
    /// (`Kernel::fallback`) executes and the generic interpreter takes this
    /// iteration — the kernel simply retries the next time the back-edge
    /// reaches the header. Array-element accesses re-check their fast-path
    /// conditions on every use and BAIL to the generic interpreter at the
    /// access op (operand stack reconstructed from the kernel's shape table)
    /// on any surprise — a bail is a slow iteration, never a wrong answer.
    ///
    /// `inline(never)`: called once per loop ACTIVATION (not per iteration),
    /// and keeping its large register loop out of `step` pins the codegen of
    /// both — growing this function must not perturb the interpreter's hot
    /// dispatch.
    ///
    /// The register loop is MONOMORPHIZED on whether the kernel calls pinned
    /// closures: the plain instantiation compiles the `CallKernel` arm (and
    /// the callee state's liveness) out entirely, so growing the closure
    /// tier cannot tax pure numeric/array/property loops with register
    /// spills in their dispatch.
    #[inline(never)]
    fn run_kernel_op(&mut self, frame: &mut Frame, idx: u32) -> Result<Ctl, Value> {
        if frame.func.proto.kernels[idx as usize]
            .callee_slots
            .is_empty()
        {
            self.run_kernel_op_impl::<false>(frame, idx)
        } else {
            self.run_kernel_op_impl::<true>(frame, idx)
        }
    }

    #[inline(never)]
    fn run_kernel_op_impl<const CALLEES: bool>(
        &mut self,
        frame: &mut Frame,
        idx: u32,
    ) -> Result<Ctl, Value> {
        let proto = frame.func.proto.clone();
        let k = &proto.kernels[idx as usize];
        // An installed op budget makes per-op counts observable (the
        // exhaustion throw lands on an exact op); kernels would skew it.
        if self.op_budget.is_some() {
            return self.step(frame, &k.fallback);
        }
        for slot in k.locals.iter() {
            let ok = match slot {
                crate::bytecode::KSlot::Local(l) => {
                    matches!(frame.locals[*l as usize], Value::Number(_))
                }
                crate::bytecode::KSlot::Upvalue(u) => {
                    matches!(*frame.func.upvalues[*u as usize].borrow(), Value::Number(_))
                }
                // Function-kernel-only slot; loop translation never maps one.
                crate::bytecode::KSlot::Arg(_) => false,
            };
            if !ok {
                return self.step(frame, &k.fallback);
            }
        }
        for &l in k.bool_locals.iter() {
            if !matches!(frame.locals[l as usize], Value::Bool(_)) {
                return self.step(frame, &k.fallback);
            }
        }
        for &l in k.oslots.iter() {
            if !matches!(frame.locals[l as usize], Value::Object(_)) {
                return self.step(frame, &k.fallback);
            }
        }
        // Pinned STRING bases: each sslot local must hold a FLAT ASCII string
        // (unit == byte, so `StrLen`/`CharCodeAt` are O(1) and total); a
        // `charCodeAt` region additionally needs the canonical method
        // resolution. Checked before any pooled state is taken; the strings
        // themselves are cached below alongside the object bases.
        for &l in k.sslots.iter() {
            let ok = matches!(
                &frame.locals[l as usize],
                Value::String(st)
                    if matches!(st.flatten_utf8(), Some(f) if f.len() == st.len_utf16())
            );
            if !ok {
                return self.step(frame, &k.fallback);
            }
        }
        if k.uses_char_code && !self.kernel_char_code_ok() {
            return self.step(frame, &k.fallback);
        }
        // A `StoreElem` may CREATE an element (hole fill / exact append), and
        // the spec's OrdinarySet consults the prototype chain when the own
        // property is absent — so a chain carrying a reified index entry (a
        // defineProperty'd accessor / non-writable index) or an exotic proto
        // must intercept via the generic path. One entry check covers the
        // whole activation: nothing inside a kernel region can run user code
        // or restructure a property map. Read-only loops skip the walk.
        if k.stores_elems {
            for &l in k.oslots.iter() {
                if let Value::Object(o) = &frame.locals[l as usize] {
                    if !crate::value::protos_allow_any_index_create(o) {
                        return self.step(frame, &k.fallback);
                    }
                }
            }
        }
        // Typed-array `.length` resolves through a prototype ACCESSOR (unlike
        // a dense array's own exotic property), so any LoadLen base holding a
        // typed array must still resolve to the canonical getter. Element
        // reads/writes need no such check — valid-index integer-indexed
        // [[Get]]/[[Set]] never consult own props or the chain, and every
        // access re-checks kind/bounds and bails otherwise.
        if k.loads_len {
            for op in k.code.iter() {
                // `LenBrCmp` is a fused `LoadLen` and needs the same check.
                let (crate::bytecode::KOp::LoadLen { obj, .. }
                | crate::bytecode::KOp::LenBrCmp { obj, .. }) = op
                else {
                    continue;
                };
                if let Value::Object(o) = &frame.locals[k.oslots[*obj as usize] as usize] {
                    if matches!(o.borrow().internal, Internal::TypedArray(_))
                        && !self.kernel_ta_len_ok(o)
                    {
                        return self.step(frame, &k.fallback);
                    }
                }
            }
        }
        // Math intrinsics: the global `Math` binding must still be the
        // canonical object as a plain DATA property (an accessor or a
        // replacement would be observable), and each used method must still
        // be the canonical builtin (methods are writable). Nothing inside a
        // kernel region can mutate globals, so entry-time checks suffice.
        if !k.math_used.is_empty() && !self.kernel_math_ok(&k.math_used) {
            return self.step(frame, &k.fallback);
        }
        // Pinned `Array.prototype.push` (see `KOp::ArrayPush`): the realm
        // canonical must still back `Array.prototype.push`, and EVERY push
        // receiver must resolve `push` to it — an unshadowed (`props`
        // empty) extensible dense array whose direct proto IS the canonical
        // prototype. Checked ONCE: nothing inside a kernel region can add a
        // shadowing property or change a prototype, so the resolution holds
        // for the whole activation — which is what lets ANY mid-kernel exit
        // materialize the canonical method object into a resumed operand
        // stack (`KShapeSlot::ArrayPushFn`). A shadowed/patched/re-proto'd
        // receiver declines here and the generic loop owns it.
        if k.uses_array_push || k.uses_array_pop {
            if k.uses_array_push
                && !self.kernel_array_method_ok("push", &self.realm.array_push.clone())
            {
                return self.step(frame, &k.fallback);
            }
            if k.uses_array_pop
                && !self.kernel_array_method_ok("pop", &self.realm.array_pop.clone())
            {
                return self.step(frame, &k.fallback);
            }
            for op in k.code.iter() {
                let obj = match op {
                    KOp::ArrayPush { obj, .. } | KOp::ArrayPop { obj, .. } => *obj,
                    _ => continue,
                };
                let ok = if let Value::Object(o) = &frame.locals[k.oslots[obj as usize] as usize] {
                    let b = o.borrow();
                    matches!(b.internal, Internal::Array(_))
                        && b.own_is_empty()
                        && b.extensible
                        && b.proto
                            .as_ref()
                            .is_some_and(|p| p.ptr_eq(&self.realm.array_proto))
                } else {
                    false
                };
                if !ok {
                    return self.step(frame, &k.fallback);
                }
            }
        }
        // Named-property access classes resolve ONCE per activation to raw
        // slot indices (see `KProp`) — and, since prop LOCALIZATION, to a
        // dedicated REGISTER each: the entry loads every slot's current
        // value below, the build pass rewrote the in-region accesses to
        // register Movs, and store-class registers are written back to the
        // slots on every exit/bail/interrupt unwind. Each base must be an
        // ORDINARY object whose own data property exists, holds a Number
        // (the register must carry the CURRENT value even for a store-only
        // class — a conditionally skipped store writes the original back),
        // and is writable where stored. Slots stay valid for the whole
        // activation because nothing inside a kernel region can
        // create/delete properties or run user code. Two classes resolving
        // to the SAME (object, slot) — aliased bases — would split one
        // property across two registers, so they decline.
        let mut prop_slots = std::mem::take(&mut self.kernel_prop_slots);
        prop_slots.clear();
        let mut prop_ids: Vec<(usize, u32)> = Vec::with_capacity(k.props_used.len());
        for p in k.props_used.iter() {
            let mut ok = false;
            if let Value::Object(o) = &frame.locals[k.oslots[p.oslot as usize] as usize] {
                let b = o.borrow();
                if matches!(b.internal, Internal::Ordinary) {
                    if let Some((
                        idx,
                        _,
                        Property {
                            kind: PropertyKind::Data { value, writable },
                            ..
                        },
                    )) = b.own_get_full(&PropertyKey::str(&p.key))
                    {
                        if matches!(value, Value::Number(_)) && (!p.store || *writable) {
                            let id = (o.ptr_id(), idx as u32);
                            if !prop_ids.contains(&id) {
                                prop_ids.push(id);
                                prop_slots.push(idx as u32);
                                ok = true;
                            }
                        }
                    }
                }
            }
            if !ok {
                self.kernel_prop_slots = prop_slots;
                return self.step(frame, &k.fallback);
            }
        }
        // Unboxed register file + array-base cache (both pooled on the Vm;
        // kernels never nest at runtime). The base objects are pinned for the
        // whole activation — sound because stores to base LOCALS inside the
        // region are rejected at translation.
        // The buffer is GROW-ONLY across activations: stale values from a
        // previous activation are unreadable by construction (every mapped
        // slot is loaded below; stack-temp registers are written before any
        // read by the virtual-stack discipline), so re-zeroing would be pure
        // memset tax.
        // Layout: the caller's window is the first KWIN slots; pinned-callee
        // windows (CALLEES) follow at fixed KWIN strides. Translation capped
        // every kernel at KWIN registers, so the dispatch loop below indexes
        // its `[f64; KWIN]` window with `& KWIN_MASK` — no bounds checks.
        let mut regs = std::mem::take(&mut self.kernel_regs);
        let need = KWIN * (1 + if CALLEES { k.callee_slots.len() } else { 0 });
        if regs.len() < need {
            regs.resize(need, 0.0);
        }
        for (r, slot) in k.locals.iter().enumerate() {
            let v = match slot {
                crate::bytecode::KSlot::Local(l) => frame.locals[*l as usize].clone(),
                crate::bytecode::KSlot::Upvalue(u) => {
                    frame.func.upvalues[*u as usize].borrow().clone()
                }
                crate::bytecode::KSlot::Arg(_) => unreachable!("declined by the guard"),
            };
            if let Value::Number(n) = v {
                regs[r] = n;
            }
        }
        let bool_base = k.locals.len();
        for (j, &l) in k.bool_locals.iter().enumerate() {
            if let Value::Bool(b) = frame.locals[l as usize] {
                regs[bool_base + j] = if b { 1.0 } else { 0.0 };
            }
        }
        let mut objs = std::mem::take(&mut self.kernel_objs);
        objs.clear();
        for &l in k.oslots.iter() {
            if let Value::Object(o) = &frame.locals[l as usize] {
                objs.push(o.clone());
            }
        }
        // Pinned strings (validated above; immutable, so no re-checks exist).
        let mut sstrs = std::mem::take(&mut self.kernel_strs);
        sstrs.clear();
        for &l in k.sslots.iter() {
            if let Value::String(st) = &frame.locals[l as usize] {
                sstrs.push(st.clone());
            }
        }
        // Prop registers (the tail of the register file): load each resolved
        // slot's current value. The guard above proved every one a Number.
        if !k.props_used.is_empty() {
            let prop_base = k.n_regs as usize - k.props_used.len();
            for (i, p) in k.props_used.iter().enumerate() {
                let b = objs[p.oslot as usize].borrow();
                match b.own_get_index(prop_slots[i] as usize) {
                    Some((
                        _,
                        Property {
                            kind:
                                PropertyKind::Data {
                                    value: Value::Number(n),
                                    ..
                                },
                            ..
                        },
                    )) => regs[prop_base + i] = *n,
                    _ => unreachable!("kernel prop slot invariant"),
                }
            }
        }
        // Pinned closure callees (`KOp::CallKernel`): ONE guard per
        // activation covers everything the per-call `run_fn_kernel` guard
        // would check — the callee resolution (an oslot local that in-region
        // stores reject, or a global-object own data property that nothing
        // in-region can rebind), so its identity, kernel, upvalue types and
        // the Math canonicals cannot change while the loop runs. Loop calls
        // happen at one constant depth, so a single depth check suffices; a
        // trace sink declines (it must see an enter/exit per call).
        let mut callee_bfs = std::mem::take(&mut self.kernel_callees);
        callee_bfs.clear();
        if CALLEES {
            let mut ok = self.trace_sink.is_none() && self.call_depth < self.max_call_depth;
            let mut win = KWIN;
            let mut any_uv_writes = false;
            if ok {
                for c in k.callee_slots.iter() {
                    let bf = match &c.source {
                        crate::bytecode::KCalleeSrc::Oslot(oslot) => {
                            let b = objs[*oslot as usize].borrow();
                            match &b.internal {
                                Internal::Function(FunctionInner::Bytecode(bf))
                                    if !bf.is_class_ctor =>
                                {
                                    Some(bf.clone())
                                }
                                _ => None,
                            }
                        }
                        // The `LoadGlobal` fast path: an own DATA property
                        // of the global object. Accessor / proto-inherited /
                        // missing globals decline — the generic loop then
                        // resolves (or throws the ReferenceError) exactly as
                        // the spec says.
                        crate::bytecode::KCalleeSrc::Global(name) => {
                            let g = self.realm.global.borrow();
                            match g.own_get(&PropertyKey::str(name)) {
                                Some(Property {
                                    kind:
                                        PropertyKind::Data {
                                            value: Value::Object(o),
                                            ..
                                        },
                                    ..
                                }) => match &o.borrow().internal {
                                    Internal::Function(FunctionInner::Bytecode(bf))
                                        if !bf.is_class_ctor =>
                                    {
                                        Some(bf.clone())
                                    }
                                    _ => None,
                                },
                                _ => None,
                            }
                        }
                    };
                    let Some(bf) = bf else {
                        ok = false;
                        break;
                    };
                    let Some(ck) = bf.proto.fn_kernel.as_ref() else {
                        ok = false;
                        break;
                    };
                    // Number-returning, non-recursive, fully-supplied args,
                    // canonical Math, Number upvalues — else stay generic.
                    // Cell-writing callees are admitted (their windows flush
                    // written cells back per call) unless a written cell
                    // aliases another of the callee's own captured cells.
                    let ck_ok = ck.rec.is_none()
                        && ck
                            .code
                            .iter()
                            .all(|op| !matches!(op, KOp::Ret { boolean: true, .. }))
                        && ck.locals.iter().all(|sl| match sl {
                            crate::bytecode::KSlot::Arg(a) => *a < u32::from(c.min_argc),
                            _ => true,
                        })
                        && (ck.math_used.is_empty() || self.kernel_math_ok(&ck.math_used))
                        && ck.locals.iter().all(|sl| match sl {
                            crate::bytecode::KSlot::Upvalue(u) => {
                                matches!(*bf.upvalues[*u as usize].borrow(), Value::Number(_))
                            }
                            _ => true,
                        })
                        && (ck.uv_writes.is_empty() || !uv_write_cells_alias(ck, &bf));
                    if !ck_ok {
                        ok = false;
                        break;
                    }
                    any_uv_writes |= !ck.uv_writes.is_empty();
                    callee_bfs.push((bf, win as u32));
                    win += KWIN;
                }
            }
            // Cross-window aliasing: a cell one callee WRITES must not be
            // captured by any other callee window (their once-per-activation
            // snapshots would go stale) or snapshot by the caller kernel's
            // own upvalue registers. Distinct bindings are distinct cells,
            // so this only fires when the same closure/cell is pinned twice.
            if ok && any_uv_writes {
                ok = !callee_cell_writes_alias(&callee_bfs, k, &frame.func.upvalues);
            }
            if !ok {
                self.kernel_regs = regs;
                objs.clear();
                self.kernel_objs = objs;
                sstrs.clear();
                self.kernel_strs = sstrs;
                self.kernel_prop_slots = prop_slots;
                callee_bfs.clear();
                self.kernel_callees = callee_bfs;
                return self.step(frame, &k.fallback);
            }
            // Extend the register file with the callee windows and load each
            // window's upvalue snapshot ONCE (identities are pinned; callee
            // code never writes an upvalue register). Grow-only: the buffer
            // may already be longer than this activation needs.
            if regs.len() < win {
                regs.resize(win, 0.0);
            }
            for (bf, wb) in callee_bfs.iter() {
                let ck = bf.proto.fn_kernel.as_ref().expect("guarded");
                for (r, slot) in ck.locals.iter().enumerate() {
                    if let crate::bytecode::KSlot::Upvalue(u) = slot {
                        if let Value::Number(n) = *bf.upvalues[*u as usize].borrow() {
                            regs[*wb as usize + r] = n;
                        }
                    }
                }
            }
        }
        // Masked fixed window: `w` is this kernel's window (first KWIN slots);
        // `wtail` holds the pinned-callee windows at KWIN strides (CALLEES
        // only). Every register index is `< n_regs ≤ KWIN` by translation, so
        // `& KWIN_MASK` is an identity on valid indices that proves the access
        // in-bounds — the dispatch loop below carries no bounds checks.
        let (whead, wtail) = regs.split_at_mut(KWIN);
        let w: &mut [f64; KWIN] = whead.try_into().expect("sized above");
        let interrupt = self.interrupt.clone();
        let mut poll: u32 = 0;
        let code = &k.code;
        let mut pc = 0usize;
        let (resume_ip, shape) = loop {
            // Taken branches funnel through here so back-edges can poll the
            // cooperative interrupt flag at the interpreter's cadence.
            macro_rules! branch {
                ($t:expr) => {{
                    let t = $t as usize;
                    if t <= pc {
                        poll = poll.wrapping_add(1);
                        if poll & 0xFF == 0 {
                            if let Some(flag) = &interrupt {
                                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                    // Same latch-and-unwind as the interpreter
                                    // loop: zero the budget so a JS catch
                                    // cannot resume execution.
                                    for (r, slot) in k.locals.iter().enumerate() {
                                        if let crate::bytecode::KSlot::Local(l) = slot {
                                            frame.locals[*l as usize] =
                                                Value::Number(w[r & KWIN_MASK]);
                                        }
                                    }
                                    for (j, &l) in k.bool_locals.iter().enumerate() {
                                        frame.locals[l as usize] =
                                            Value::Bool(w[(bool_base + j) & KWIN_MASK] != 0.0);
                                    }
                                    writeback_kernel_props(k, &objs, &prop_slots, &w[..]);
                                    self.kernel_regs = regs;
                                    objs.clear();
                                    self.kernel_objs = objs;
                                    sstrs.clear();
                                    self.kernel_strs = sstrs;
                                    self.kernel_prop_slots = prop_slots;
                                    callee_bfs.clear();
                                    self.kernel_callees = callee_bfs;
                                    self.op_budget = Some(0);
                                    return Err(self.throw_range("execution interrupted"));
                                }
                            }
                        }
                    }
                    pc = t;
                    continue;
                }};
            }
            match code[pc] {
                KOp::Mov { dst, src } => w[dst as usize & KWIN_MASK] = w[src as usize & KWIN_MASK],
                KOp::Const { dst, k } => w[dst as usize & KWIN_MASK] = k,
                KOp::Add { dst, a, b } => {
                    w[dst as usize & KWIN_MASK] =
                        w[a as usize & KWIN_MASK] + w[b as usize & KWIN_MASK]
                }
                KOp::AddK { dst, a, k } => {
                    w[dst as usize & KWIN_MASK] = w[a as usize & KWIN_MASK] + k
                }
                KOp::Arith { kind, dst, a, b } => {
                    w[dst as usize & KWIN_MASK] =
                        number_arith_raw(w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK], kind)
                }
                KOp::ArithK { kind, dst, a, k } => {
                    w[dst as usize & KWIN_MASK] =
                        number_arith_raw(w[a as usize & KWIN_MASK], k, kind)
                }
                KOp::Neg { dst, src } => w[dst as usize & KWIN_MASK] = -w[src as usize & KWIN_MASK],
                KOp::BitNot { dst, src } => {
                    w[dst as usize & KWIN_MASK] =
                        !crate::vm::to_int32(w[src as usize & KWIN_MASK]) as f64
                }
                KOp::Br { target } => branch!(target),
                KOp::BrCmp {
                    cmp,
                    a,
                    b,
                    if_true,
                    target,
                } => {
                    if knum_cmp(cmp, w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK])
                        == if_true
                    {
                        branch!(target)
                    }
                }
                KOp::BrCmpK {
                    cmp,
                    a,
                    k,
                    if_true,
                    target,
                } => {
                    if knum_cmp(cmp, w[a as usize & KWIN_MASK], k) == if_true {
                        branch!(target)
                    }
                }
                KOp::BrFalsy { src, target } => {
                    if !knum_truthy(w[src as usize & KWIN_MASK]) {
                        branch!(target)
                    }
                }
                KOp::BrTruthy { src, target } => {
                    if knum_truthy(w[src as usize & KWIN_MASK]) {
                        branch!(target)
                    }
                }
                // Dense element read: full fast-path re-check, else bail to
                // the generic op (the `bail` target is an Exit stub).
                KOp::LoadElem {
                    dst,
                    obj,
                    idx,
                    bail,
                } => {
                    let i = w[idx as usize & KWIN_MASK];
                    let mut ok = false;
                    if let Some(iu) = dense_index(i) {
                        let b = objs[obj as usize].borrow();
                        match &b.internal {
                            Internal::Array(arr) if b.own_is_empty() => {
                                if let Some(Value::Number(n)) = arr.get(iu) {
                                    w[dst as usize & KWIN_MASK] = *n;
                                    ok = true;
                                }
                            }
                            // Numeric typed arrays: a valid-index [[Get]]
                            // reads element storage directly — own props and
                            // the prototype chain are never consulted, so no
                            // props/proto check is needed. OOB (incl.
                            // detached / shrunk-view) bails to the generic
                            // path, which owns the `undefined` absorption.
                            Internal::TypedArray(t)
                                if !t.kind.is_bigint()
                                    && iu < crate::typed_array::ta_eff_length(t) =>
                            {
                                let off = t.byte_offset + iu * t.kind.bytes();
                                let buf = t.buffer.borrow();
                                if let Internal::ArrayBuffer(Some(bytes)) = &buf.internal {
                                    w[dst as usize & KWIN_MASK] =
                                        crate::typed_array::decode(bytes, off, t.kind);
                                    ok = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                }
                // Pinned-native push: append + new length, in-kernel. The
                // entry guard proved the canonical method still resolves
                // and (stores_elems) that no proto can intercept element
                // creation; the receiver conditions re-check per push —
                // anything else re-runs the whole generic Call via the
                // bail (the method object reconstructed from the realm).
                KOp::ArrayPush {
                    obj,
                    val,
                    dst,
                    bail,
                } => {
                    let mut ok = false;
                    {
                        let mut b = objs[obj as usize].borrow_mut();
                        if b.own_is_empty()
                            && b.extensible
                            && b.proto
                                .as_ref()
                                .is_some_and(|p| p.ptr_eq(&self.realm.array_proto))
                        {
                            if let Internal::Array(arr) = &mut b.internal {
                                if arr.len() < crate::value::MAX_DENSE_ARRAY {
                                    arr.push(Value::Number(w[val as usize & KWIN_MASK]));
                                    w[dst as usize & KWIN_MASK] = arr.len() as f64;
                                    ok = true;
                                }
                            }
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                }
                // Pinned-native pop: remove + yield the last element when it
                // is a plain Number. An empty array (undefined result), a
                // trailing hole (prototype consult), or a non-Number element
                // re-runs the generic Call via the bail.
                KOp::ArrayPop { obj, dst, bail } => {
                    let mut ok = false;
                    {
                        let mut b = objs[obj as usize].borrow_mut();
                        if b.own_is_empty() && b.extensible {
                            if let Internal::Array(arr) = &mut b.internal {
                                if let Some(Value::Number(n)) = arr.last() {
                                    w[dst as usize & KWIN_MASK] = *n;
                                    arr.pop();
                                    ok = true;
                                }
                            }
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                }
                // Dense element write: in-place overwrite (exactly the
                // `Op::SetPropDynamic` fast-path conditions), an in-bounds
                // HOLE fill, or an exact one-past-the-end APPEND. Filling and
                // appending CREATE a property, so they additionally require
                // the array to be extensible (a sealed/prevented receiver
                // must reject through the generic path: silent in sloppy,
                // TypeError in strict) and to stay under the dense-storage
                // bound (the generic path owns that RangeError). Everything
                // else bails.
                KOp::StoreElem {
                    obj,
                    idx,
                    val,
                    bail,
                } => {
                    let i = w[idx as usize & KWIN_MASK];
                    let mut ok = false;
                    if let Some(iu) = dense_index(i) {
                        let mut b = objs[obj as usize].borrow_mut();
                        let extensible = b.extensible;
                        let props_empty = b.own_is_empty();
                        match &mut b.internal {
                            Internal::Array(arr) if props_empty => {
                                match arr.get_mut(iu) {
                                    Some(slot) if !matches!(slot, Value::Hole) => {
                                        *slot = Value::Number(w[val as usize & KWIN_MASK]);
                                        ok = true;
                                    }
                                    Some(slot) => {
                                        // In-bounds hole: creation.
                                        if extensible {
                                            *slot = Value::Number(w[val as usize & KWIN_MASK]);
                                            ok = true;
                                        }
                                    }
                                    None => {
                                        // Exact append (no hole gap).
                                        if extensible
                                            && iu == arr.len()
                                            && iu < crate::value::MAX_DENSE_ARRAY
                                        {
                                            arr.push(Value::Number(w[val as usize & KWIN_MASK]));
                                            ok = true;
                                        }
                                    }
                                }
                            }
                            // Numeric typed arrays: a valid-index [[Set]]
                            // writes element storage directly, no props/proto
                            // consult; the register already holds the
                            // ToNumber'd value, and `encode` applies the same
                            // per-kind conversion (f32 rounding, ToInt32-class
                            // wrapping) as the builtin write path. OOB — a
                            // silent no-op per spec — bails to the generic
                            // path, which owns that behavior.
                            Internal::TypedArray(t)
                                if !t.kind.is_bigint()
                                    && iu < crate::typed_array::ta_eff_length(t) =>
                            {
                                let off = t.byte_offset + iu * t.kind.bytes();
                                let kind = t.kind;
                                let mut buf = t.buffer.borrow_mut();
                                if let Internal::ArrayBuffer(Some(bytes)) = &mut buf.internal {
                                    crate::typed_array::encode(
                                        bytes,
                                        off,
                                        kind,
                                        w[val as usize & KWIN_MASK],
                                    );
                                    ok = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                }
                KOp::CmpSet { cmp, dst, a, b } => {
                    w[dst as usize & KWIN_MASK] =
                        if knum_cmp(cmp, w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK]) {
                            1.0
                        } else {
                            0.0
                        }
                }
                KOp::BoolNot { dst, src } => {
                    w[dst as usize & KWIN_MASK] = if knum_truthy(w[src as usize & KWIN_MASK]) {
                        0.0
                    } else {
                        1.0
                    }
                }
                KOp::Math1 { kind, dst, src } => {
                    w[dst as usize & KWIN_MASK] = kmath1(kind, w[src as usize & KWIN_MASK])
                }
                KOp::Math2 { kind, dst, a, b } => {
                    w[dst as usize & KWIN_MASK] =
                        kmath2(kind, w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK])
                }
                // Dense array `length` (unshadowed only) or typed-array
                // effective length (the entry guard verified the canonical
                // accessor resolution for typed-array bases; the length
                // itself cannot change mid-activation — resize/detach require
                // calls, which kernel regions exclude), else bail.
                KOp::LoadLen { dst, obj, bail } => {
                    let mut ok = false;
                    {
                        let b = objs[obj as usize].borrow();
                        match &b.internal {
                            Internal::Array(arr) if b.own_is_empty() => {
                                w[dst as usize & KWIN_MASK] = arr.len() as f64;
                                ok = true;
                            }
                            // Any kind — a BigInt-element array's `.length`
                            // is the same plain count (its ELEMENT accesses
                            // are what bail).
                            Internal::TypedArray(t) => {
                                w[dst as usize & KWIN_MASK] =
                                    crate::typed_array::ta_eff_length(t) as f64;
                                ok = true;
                            }
                            _ => {}
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                }
                // Prop LOCALIZATION rewrote every LoadProp/StoreProp into a
                // register Mov at kernel build; the slots live only in the
                // entry load and the exit/unwind write-back now.
                KOp::LoadProp { .. } | KOp::StoreProp { .. } => {
                    unreachable!("prop op survived kernel build")
                }
                KOp::Mov2 { d1, s1, d2, s2 } => {
                    w[d1 as usize & KWIN_MASK] = w[s1 as usize & KWIN_MASK];
                    w[d2 as usize & KWIN_MASK] = w[s2 as usize & KWIN_MASK];
                    // The unfused second op remains in the next slot as a
                    // branch-target landing pad; skip it.
                    pc += 1;
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
                    w[dst as usize & KWIN_MASK] = number_arith_raw(
                        w[a as usize & KWIN_MASK],
                        w[b as usize & KWIN_MASK],
                        kind,
                    );
                    w[d2 as usize & KWIN_MASK] =
                        w[a2 as usize & KWIN_MASK] + w[b2 as usize & KWIN_MASK];
                    pc += 1;
                }
                KOp::AddKBr { dst, a, k, target } => {
                    w[dst as usize & KWIN_MASK] = w[a as usize & KWIN_MASK] + k;
                    branch!(target)
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
                    w[dst as usize & KWIN_MASK] =
                        number_arith_raw(w[a as usize & KWIN_MASK], k, kind);
                    w[d2 as usize & KWIN_MASK] =
                        w[a2 as usize & KWIN_MASK] + w[b2 as usize & KWIN_MASK];
                    pc += 1;
                }
                // Fused `i < a.length` header test: LoadLen's semantics (and
                // bail edge) then BrCmp's compare-and-branch, one dispatch.
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
                    let mut ok = false;
                    {
                        let b = objs[obj as usize].borrow();
                        match &b.internal {
                            Internal::Array(arr) if b.own_is_empty() => {
                                w[dst as usize & KWIN_MASK] = arr.len() as f64;
                                ok = true;
                            }
                            Internal::TypedArray(t) => {
                                w[dst as usize & KWIN_MASK] =
                                    crate::typed_array::ta_eff_length(t) as f64;
                                ok = true;
                            }
                            _ => {}
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                    if knum_cmp(cmp, w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK])
                        == if_true
                    {
                        branch!(target)
                    }
                    pc += 1;
                }
                // Fused `s += a[i]`: LoadElem's semantics (and bail edge)
                // then the accumulating Add, one dispatch.
                KOp::LoadElemAdd {
                    dst,
                    obj,
                    idx,
                    bail,
                    d2,
                    a2,
                    b2,
                } => {
                    let i = w[idx as usize & KWIN_MASK];
                    let mut ok = false;
                    if let Some(iu) = dense_index(i) {
                        let b = objs[obj as usize].borrow();
                        match &b.internal {
                            Internal::Array(arr) if b.own_is_empty() => {
                                if let Some(Value::Number(n)) = arr.get(iu) {
                                    w[dst as usize & KWIN_MASK] = *n;
                                    ok = true;
                                }
                            }
                            Internal::TypedArray(t)
                                if !t.kind.is_bigint()
                                    && iu < crate::typed_array::ta_eff_length(t) =>
                            {
                                let off = t.byte_offset + iu * t.kind.bytes();
                                let buf = t.buffer.borrow();
                                if let Internal::ArrayBuffer(Some(bytes)) = &buf.internal {
                                    w[dst as usize & KWIN_MASK] =
                                        crate::typed_array::decode(bytes, off, t.kind);
                                    ok = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                    w[d2 as usize & KWIN_MASK] =
                        w[a2 as usize & KWIN_MASK] + w[b2 as usize & KWIN_MASK];
                    pc += 1;
                }
                // Fused `a[i] <op> …` (the dot-product second load feeding
                // its multiply): LoadElem then Arith, one dispatch.
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
                    let i = w[idx as usize & KWIN_MASK];
                    let mut ok = false;
                    if let Some(iu) = dense_index(i) {
                        let b = objs[obj as usize].borrow();
                        match &b.internal {
                            Internal::Array(arr) if b.own_is_empty() => {
                                if let Some(Value::Number(n)) = arr.get(iu) {
                                    w[dst as usize & KWIN_MASK] = *n;
                                    ok = true;
                                }
                            }
                            Internal::TypedArray(t)
                                if !t.kind.is_bigint()
                                    && iu < crate::typed_array::ta_eff_length(t) =>
                            {
                                let off = t.byte_offset + iu * t.kind.bytes();
                                let buf = t.buffer.borrow();
                                if let Internal::ArrayBuffer(Some(bytes)) = &buf.internal {
                                    w[dst as usize & KWIN_MASK] =
                                        crate::typed_array::decode(bytes, off, t.kind);
                                    ok = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !ok {
                        branch!(bail)
                    }
                    w[d2 as usize & KWIN_MASK] = number_arith_raw(
                        w[a2 as usize & KWIN_MASK],
                        w[b2 as usize & KWIN_MASK],
                        kind,
                    );
                    pc += 1;
                }
                // Pinned-string reads: TOTAL on the entry-guarded flat-ASCII
                // string (unit == byte). `StrLen` is an activation constant;
                // `CharCodeAt` computes ToIntegerOrInfinity (NaN→0, truncate
                // toward zero, saturating casts) and the out-of-range NaN
                // exactly — no bail exists.
                KOp::StrLen { dst, str } => {
                    w[dst as usize & KWIN_MASK] = sstrs[str as usize].len_utf16() as f64;
                }
                KOp::CharCodeAt { dst, str, idx } => {
                    let i = w[idx as usize & KWIN_MASK];
                    let bytes = sstrs[str as usize]
                        .flatten_utf8()
                        .expect("entry-guarded flat ASCII")
                        .as_bytes();
                    let p = if i.is_nan() { 0i64 } else { i as i64 };
                    w[dst as usize & KWIN_MASK] =
                        match usize::try_from(p).ok().and_then(|p| bytes.get(p)) {
                            Some(&b) => b as f64,
                            None => f64::NAN,
                        };
                }
                // A pinned-closure call: copy the arguments into the
                // callee's window and run its (guarded) kernel inline.
                KOp::CallKernel {
                    dst,
                    fslot,
                    base,
                    argc: _,
                } if CALLEES => {
                    let (bf, wb) = &callee_bfs[fslot as usize];
                    let ck = bf.proto.fn_kernel.as_ref().expect("guarded");
                    // Callee windows live in `wtail` at KWIN strides (window
                    // base `wb` counts from the buffer start, so subtract the
                    // caller's window).
                    let cw: &mut [f64; KWIN] = (&mut wtail[*wb as usize - KWIN..][..KWIN])
                        .try_into()
                        .expect("sized above");
                    for (r, slot) in ck.locals.iter().enumerate() {
                        if let crate::bytecode::KSlot::Arg(a) = slot {
                            cw[r & KWIN_MASK] = w[(base as usize + *a as usize) & KWIN_MASK];
                        }
                    }
                    let ret = run_callee_window(cw, ck, &interrupt, &mut poll);
                    // A cell-writing callee flushes after EVERY call (return
                    // or interrupt): the cell must be current before any
                    // subsequent op that can bail to the generic loop, whose
                    // remaining iterations call the callee generically. The
                    // register persists in the pinned window, so no reload
                    // is needed — nothing else can write the cell mid-loop
                    // (cross-window aliasing declined at entry).
                    if !ck.uv_writes.is_empty() {
                        flush_uv_writes(ck, bf, cw);
                    }
                    if let Some(ret) = ret {
                        w[dst as usize & KWIN_MASK] = ret;
                    } else {
                        // Interrupted on a callee back-edge: the same
                        // latch-and-unwind as an interrupted caller edge.
                        for (r, slot) in k.locals.iter().enumerate() {
                            if let crate::bytecode::KSlot::Local(l) = slot {
                                frame.locals[*l as usize] = Value::Number(w[r & KWIN_MASK]);
                            }
                        }
                        for (j, &l) in k.bool_locals.iter().enumerate() {
                            frame.locals[l as usize] =
                                Value::Bool(w[(bool_base + j) & KWIN_MASK] != 0.0);
                        }
                        writeback_kernel_props(k, &objs, &prop_slots, &w[..]);
                        self.kernel_regs = regs;
                        objs.clear();
                        self.kernel_objs = objs;
                        sstrs.clear();
                        self.kernel_strs = sstrs;
                        self.kernel_prop_slots = prop_slots;
                        callee_bfs.clear();
                        self.kernel_callees = callee_bfs;
                        self.op_budget = Some(0);
                        return Err(self.throw_range("execution interrupted"));
                    }
                }
                KOp::Exit { resume_ip, shape } => break (resume_ip, shape),
                // Plain instantiation: a kernel without callee_slots never
                // contains CallKernel (translator invariant).
                KOp::CallKernel { .. } => unreachable!("CallKernel in a plain kernel loop"),
                // Function-kernel-only ops; loop translation never emits them.
                KOp::Ret { .. } | KOp::SelfCall { .. } => unreachable!("fn op in a loop kernel"),
            }
            pc += 1;
        };
        // Materialize: every mapped numeric local back to the frame (as
        // Numbers), then the operand stack from the exit's shape (bottom-up:
        // registers as Numbers, object slots as objects), then resume the
        // bytecode interpreter at the exit's target.
        for (r, slot) in k.locals.iter().enumerate() {
            if let crate::bytecode::KSlot::Local(l) = slot {
                frame.locals[*l as usize] = Value::Number(w[r & KWIN_MASK]);
            }
        }
        for (j, &l) in k.bool_locals.iter().enumerate() {
            frame.locals[l as usize] = Value::Bool(w[(bool_base + j) & KWIN_MASK] != 0.0);
        }
        writeback_kernel_props(k, &objs, &prop_slots, &w[..]);
        for slot in k.shapes[shape as usize].iter() {
            match slot {
                crate::bytecode::KShapeSlot::Num(r) => {
                    frame.stack.push(Value::Number(w[*r as usize & KWIN_MASK]))
                }
                crate::bytecode::KShapeSlot::Bool(r) => frame
                    .stack
                    .push(Value::Bool(w[*r as usize & KWIN_MASK] != 0.0)),
                crate::bytecode::KShapeSlot::Obj(o) => {
                    frame.stack.push(Value::Object(objs[*o as usize].clone()))
                }
                // The guard proved the live values ARE the canonicals.
                crate::bytecode::KShapeSlot::MathObj => frame.stack.push(Value::Object(
                    self.realm.math_object.clone().expect("guarded"),
                )),
                crate::bytecode::KShapeSlot::MathFn(kind) => frame.stack.push(Value::Object(
                    self.realm.math_kernel[*kind as usize].clone(),
                )),
                crate::bytecode::KShapeSlot::ArrayPushFn => frame.stack.push(Value::Object(
                    self.realm.array_push.clone().expect("guarded"),
                )),
                crate::bytecode::KShapeSlot::ArrayPopFn => frame.stack.push(Value::Object(
                    self.realm.array_pop.clone().expect("guarded"),
                )),
                crate::bytecode::KShapeSlot::Str(sl) => {
                    frame.stack.push(Value::String(sstrs[*sl as usize].clone()))
                }
                crate::bytecode::KShapeSlot::CharCodeFn => frame.stack.push(Value::Object(
                    self.realm.string_char_code_at.clone().expect("guarded"),
                )),
            }
        }
        self.kernel_regs = regs;
        objs.clear();
        self.kernel_objs = objs;
        sstrs.clear();
        self.kernel_strs = sstrs;
        self.kernel_prop_slots = prop_slots;
        callee_bfs.clear();
        self.kernel_callees = callee_bfs;
        Ok(Ctl::Jump(resume_ip as usize))
    }

    /// Execute a FUNCTION kernel (`FuncProto::fn_kernel`): the entire call
    /// runs in unboxed registers with no frame at all. `None` = the entry
    /// guard declined (an op budget installed — per-op counts are observable;
    /// a trace sink active — it must see an enter/exit per call; a consumed
    /// argument or captured upvalue not a `Number`; a monkeypatched `Math`;
    /// a written cell aliasing another captured cell) — the caller proceeds
    /// down the ordinary frame path. Cells the body writes (`uv_writes`)
    /// buffer in registers and flush back on completion. `Some` is the call's
    /// exact result, bit-identical to the generic path by the loop-kernel
    /// argument (shared numeric cores, an op set closed over numbers, typed
    /// returns). Callers have already applied the depth guard, so the
    /// max-call-depth RangeError fires identically on both paths.
    fn run_fn_kernel(
        &mut self,
        bf: &Rc<BytecodeFunction>,
        args: &[Value],
    ) -> Option<Result<Value, Value>> {
        let k = bf.proto.fn_kernel.as_ref()?;
        if self.op_budget.is_some() || self.trace_sink.is_some() {
            return None;
        }
        // Recursive kernels (self or mutual) run the windowed executor.
        if k.rec.is_some() {
            return self.run_fn_kernel_rec(bf, args);
        }
        // One fixed window on the native stack (function kernels are tiny and
        // never nest at runtime); masked indexing inside the executor makes
        // every register access bounds-check-free.
        let mut regs = [0.0f64; KWIN];
        for (r, slot) in k.locals.iter().enumerate() {
            match slot {
                crate::bytecode::KSlot::Arg(a) => match args.get(*a as usize) {
                    Some(Value::Number(n)) => regs[r & KWIN_MASK] = *n,
                    _ => return None,
                },
                // Internal locals are pure register scratch: translation
                // proved a store dominates every read, so no frame slot is
                // needed (and none exists).
                crate::bytecode::KSlot::Local(_) => {}
                crate::bytecode::KSlot::Upvalue(u) => match &*bf.upvalues[*u as usize].borrow() {
                    Value::Number(n) => regs[r & KWIN_MASK] = *n,
                    _ => return None,
                },
            }
        }
        if !k.math_used.is_empty() && !self.kernel_math_ok(&k.math_used) {
            return None;
        }
        // Written cells (`uv_writes`) buffer in their registers and flush on
        // completion (out-of-line: most kernels have no writes, and keeping
        // this function small protects the LTO inlining of the hot paths).
        if !k.uv_writes.is_empty() && uv_write_cells_alias(k, bf) {
            return None;
        }
        let interrupt = self.interrupt.clone();
        let mut poll: u32 = 0;
        let ret = exec_fn_kernel_code(&k.code, &mut regs, &interrupt, &mut poll);
        if !k.uv_writes.is_empty() {
            flush_uv_writes(k, bf, &regs);
        }
        match ret {
            Some(ret) => Some(Ok(ret)),
            None => {
                // Interrupted on a back-edge: latch the zero budget so a JS
                // catch cannot resume execution.
                self.op_budget = Some(0);
                Some(Err(self.throw_range("execution interrupted")))
            }
        }
    }

    /// The WINDOWED executor for RECURSIVE function kernels
    /// ([`Kernel::rec`]): each [`KOp::SelfCall`] pushes a fresh register
    /// window and an explicit (caller, return-pc, dst, caller-window) record
    /// — the whole recursion (self OR mutual) runs without a single frame or
    /// `Value`. Entry resolves the recursion FAMILY once:
    ///
    /// - Every [`SelfRefKind`] must hold for its member — a `Global` name
    ///   must be a plain data property holding that very closure, an
    ///   `Upvalue` cell must contain it (pointer identity) — so a
    ///   shadowed/rebound/accessor'd reference declines to the generic path.
    /// - Each [`KernelRec::globals`] name must resolve to a plain sync
    ///   bytecode closure carrying a function kernel, transitively closed
    ///   over the whole family (bounded), with every member's `Ret` type
    ///   matching the entry kernel's and every call site supplying at least
    ///   the resolved callee's consumed arguments.
    ///
    /// Nothing inside a RECURSIVE kernel can write globals or cells
    /// (upvalue-writing kernels decline this tier at translation and at
    /// family resolution), so the entry resolution holds for the whole
    /// activation. Depth mirrors the
    /// interpreter's limit; on overflow the activation is ABANDONED and
    /// `None` returned — sound because kernels are pure (registers only),
    /// and the caller's generic rerun then raises the spec RangeError from
    /// the exact frame it belongs to.
    ///
    /// The resolved family is CACHED ([`RecFamily`], keyed by the entry
    /// closure's identity): resolution allocates half a dozen tables and
    /// walks every member's code, which dominated shallow recursions
    /// (isEven-class) when paid per outer call. A hit re-verifies only the
    /// dynamic facts — recorded global slots (key-verified, no hashing),
    /// upvalue self-cells, canonical `Math` — and re-snapshots upvalue
    /// values; any failure drops the entry and re-resolves from scratch,
    /// which is exactly the uncached behavior. Hit or miss is therefore
    /// unobservable: same declines, same results.
    fn run_fn_kernel_rec(
        &mut self,
        bf: &Rc<BytecodeFunction>,
        args: &[Value],
    ) -> Option<Result<Value, Value>> {
        let k0 = bf.proto.fn_kernel.as_ref()?;
        k0.rec.as_deref()?;

        let fam = match self.take_rec_family(bf) {
            Some(f) => f,
            None => self.build_rec_family(bf)?,
        };
        let ret_bool = fam.ret_bool;

        // --- Window 0: the invoked member's arguments and upvalues. Windows
        // are FIXED KWIN-slot strides (window d = regs[d*KWIN..(d+1)*KWIN]),
        // so the dispatch loop below runs over a `&mut [f64; KWIN]` with
        // masked, bounds-check-free indexing. The buffer is GROW-ONLY across
        // activations: a deep recursion leaves it at its high-water length,
        // so the next outer call's window pushes (`SelfCall`'s conditional
        // resize) stop paying a zero-fill per window — which was ~11% of the
        // mutual-recursion workload. Stale values are unreadable:
        // args/upvalues are written here, locals are store-before-read by
        // translation proof.
        let mut regs = std::mem::take(&mut self.kernel_regs);
        if regs.len() < KWIN {
            regs.resize(KWIN, 0.0);
        }
        for &(r, a) in &fam.arg_slots[0] {
            match args.get(a) {
                Some(Value::Number(v)) => regs[r] = *v,
                _ => {
                    self.kernel_regs = regs;
                    self.park_rec_family(fam);
                    return None;
                }
            }
        }
        for &(r, _, v) in &fam.uv_snaps[0] {
            regs[r] = v;
        }

        /// How the windowed loop below exited; the single tail then parks
        /// the pooled buffers + family exactly once on every path.
        enum RecOut {
            Done(Value),
            Interrupted,
            Abandon,
        }
        /// A window-crossing step the masked dispatch loop cannot perform
        /// while it holds the current window borrow: perform it over the
        /// full buffer, then re-enter with the new window.
        enum Act {
            Call { t: usize, abase: u16, dst: u16 },
            Ret { v: f64 },
        }

        let interrupt = self.interrupt.clone();
        let mut poll: u32 = 0;
        // (caller member, return pc, dst register, caller window base) —
        // pooled on the Vm so a deep recursion allocates its stack once per
        // Vm, not once per outer call.
        let mut calls = std::mem::take(&mut self.rec_calls);
        debug_assert!(calls.is_empty());
        let mut cur = 0usize;
        let mut base = 0usize;
        let mut pc = 0usize;
        let out = 'outer: loop {
            // Member level: table hoists happen only when the executing
            // MEMBER changes; same-kernel calls/returns (fib-class, the
            // overwhelming case) stay inside the window loop below.
            let code: &[KOp] = &fam.funcs[cur]
                .proto
                .fn_kernel
                .as_ref()
                .expect("resolved family")
                .code;
            let cur_map = &fam.callee_map[cur][..];
            'win: loop {
                // The current activation's fixed window (strided, sized above).
                let w: &mut [f64; KWIN] = (&mut regs[base..base + KWIN])
                    .try_into()
                    .expect("strided window");
                let act = loop {
                    macro_rules! poll_interrupt {
                        () => {{
                            poll = poll.wrapping_add(1);
                            if poll & 0xFF == 0 {
                                if let Some(flag) = &interrupt {
                                    if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                        break 'outer RecOut::Interrupted;
                                    }
                                }
                            }
                        }};
                    }
                    macro_rules! branch {
                        ($t:expr) => {{
                            let t = $t as usize;
                            if t <= pc {
                                poll_interrupt!();
                            }
                            pc = t;
                            continue;
                        }};
                    }
                    match code[pc] {
                        KOp::Mov { dst, src } => {
                            w[dst as usize & KWIN_MASK] = w[src as usize & KWIN_MASK]
                        }
                        KOp::Const { dst, k } => w[dst as usize & KWIN_MASK] = k,
                        KOp::Add { dst, a, b } => {
                            w[dst as usize & KWIN_MASK] =
                                w[a as usize & KWIN_MASK] + w[b as usize & KWIN_MASK]
                        }
                        KOp::AddK { dst, a, k } => {
                            w[dst as usize & KWIN_MASK] = w[a as usize & KWIN_MASK] + k
                        }
                        KOp::Arith { kind, dst, a, b } => {
                            w[dst as usize & KWIN_MASK] = number_arith_raw(
                                w[a as usize & KWIN_MASK],
                                w[b as usize & KWIN_MASK],
                                kind,
                            )
                        }
                        KOp::ArithK { kind, dst, a, k } => {
                            w[dst as usize & KWIN_MASK] =
                                number_arith_raw(w[a as usize & KWIN_MASK], k, kind)
                        }
                        KOp::Neg { dst, src } => {
                            w[dst as usize & KWIN_MASK] = -w[src as usize & KWIN_MASK]
                        }
                        KOp::BitNot { dst, src } => {
                            w[dst as usize & KWIN_MASK] =
                                !crate::vm::to_int32(w[src as usize & KWIN_MASK]) as f64
                        }
                        KOp::Br { target } => branch!(target),
                        KOp::BrCmp {
                            cmp,
                            a,
                            b,
                            if_true,
                            target,
                        } => {
                            if knum_cmp(cmp, w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK])
                                == if_true
                            {
                                branch!(target)
                            }
                        }
                        KOp::BrCmpK {
                            cmp,
                            a,
                            k,
                            if_true,
                            target,
                        } => {
                            if knum_cmp(cmp, w[a as usize & KWIN_MASK], k) == if_true {
                                branch!(target)
                            }
                        }
                        KOp::BrFalsy { src, target } => {
                            if !knum_truthy(w[src as usize & KWIN_MASK]) {
                                branch!(target)
                            }
                        }
                        KOp::BrTruthy { src, target } => {
                            if knum_truthy(w[src as usize & KWIN_MASK]) {
                                branch!(target)
                            }
                        }
                        KOp::CmpSet { cmp, dst, a, b } => {
                            w[dst as usize & KWIN_MASK] = if knum_cmp(
                                cmp,
                                w[a as usize & KWIN_MASK],
                                w[b as usize & KWIN_MASK],
                            ) {
                                1.0
                            } else {
                                0.0
                            }
                        }
                        KOp::BoolNot { dst, src } => {
                            w[dst as usize & KWIN_MASK] =
                                if knum_truthy(w[src as usize & KWIN_MASK]) {
                                    0.0
                                } else {
                                    1.0
                                }
                        }
                        KOp::Math1 { kind, dst, src } => {
                            w[dst as usize & KWIN_MASK] = kmath1(kind, w[src as usize & KWIN_MASK])
                        }
                        KOp::Math2 { kind, dst, a, b } => {
                            w[dst as usize & KWIN_MASK] =
                                kmath2(kind, w[a as usize & KWIN_MASK], w[b as usize & KWIN_MASK])
                        }
                        KOp::Mov2 { d1, s1, d2, s2 } => {
                            w[d1 as usize & KWIN_MASK] = w[s1 as usize & KWIN_MASK];
                            w[d2 as usize & KWIN_MASK] = w[s2 as usize & KWIN_MASK];
                            pc += 1;
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
                            w[dst as usize & KWIN_MASK] = number_arith_raw(
                                w[a as usize & KWIN_MASK],
                                w[b as usize & KWIN_MASK],
                                kind,
                            );
                            w[d2 as usize & KWIN_MASK] =
                                w[a2 as usize & KWIN_MASK] + w[b2 as usize & KWIN_MASK];
                            pc += 1;
                        }
                        KOp::AddKBr { dst, a, k, target } => {
                            w[dst as usize & KWIN_MASK] = w[a as usize & KWIN_MASK] + k;
                            branch!(target)
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
                            w[dst as usize & KWIN_MASK] =
                                number_arith_raw(w[a as usize & KWIN_MASK], k, kind);
                            w[d2 as usize & KWIN_MASK] =
                                w[a2 as usize & KWIN_MASK] + w[b2 as usize & KWIN_MASK];
                            pc += 1;
                        }
                        KOp::SelfCall {
                            dst,
                            base: abase,
                            argc: _,
                            callee,
                        } => {
                            // Mirror the generic per-call depth guard: an
                            // overflow abandons the pure activation; the generic
                            // rerun then recurses to the same depth and raises
                            // the RangeError.
                            if self.call_depth + calls.len() + 1 > self.max_call_depth {
                                break 'outer RecOut::Abandon;
                            }
                            poll_interrupt!();
                            let t = cur_map[callee as usize] as usize;
                            break Act::Call { t, abase, dst };
                        }
                        KOp::Ret { src, boolean } => {
                            // Entry verified every member's Ret type against the
                            // family's (`ret_bool`); registers carry booleans as
                            // exactly 0.0/1.0, so the raw value moves across.
                            debug_assert_eq!(boolean, ret_bool);
                            break Act::Ret {
                                v: w[src as usize & KWIN_MASK],
                            };
                        }
                        KOp::ArrayPush { .. }
                        | KOp::ArrayPop { .. }
                        | KOp::LoadElem { .. }
                        | KOp::StoreElem { .. }
                        | KOp::LoadLen { .. }
                        | KOp::StrLen { .. }
                        | KOp::CharCodeAt { .. }
                        | KOp::LoadElemAdd { .. }
                        | KOp::LoadElemArith { .. }
                        | KOp::LenBrCmp { .. }
                        | KOp::LoadProp { .. }
                        | KOp::StoreProp { .. }
                        | KOp::CallKernel { .. }
                        | KOp::Exit { .. } => unreachable!("bail op in a function kernel"),
                    }
                    pc += 1;
                };
                // Window-crossing steps run over the full buffer (the fixed
                // window borrow has ended): populate the callee window / write
                // the return value into the caller's dst. Same-member switches
                // re-enter the window loop directly; a member change re-hoists.
                match act {
                    Act::Call { t, abase, dst } => {
                        let new_base = base + KWIN;
                        if regs.len() < new_base + KWIN {
                            regs.resize(new_base + KWIN, 0.0);
                        }
                        // Arguments MOVE from the call site's contiguous
                        // registers (translation guarantees argc covers every
                        // consumed index — the entry cross-check re-verified it
                        // against the RESOLVED callee), upvalues from the
                        // member's entry snapshot, locals are scratch
                        // (store-before-read proven at translation).
                        let (lo, hi) = regs.split_at_mut(new_base);
                        let caller = &lo[base..];
                        let cw = &mut hi[..KWIN];
                        for &(r, a) in &fam.arg_slots[t] {
                            cw[r] = caller[abase as usize + a];
                        }
                        for &(r, _, v) in &fam.uv_snaps[t] {
                            cw[r] = v;
                        }
                        calls.push((cur as u8, pc as u16 + 1, dst, base as u32));
                        base = new_base;
                        pc = 0;
                        if t == cur {
                            continue 'win;
                        }
                        cur = t;
                        continue 'outer;
                    }
                    Act::Ret { v } => match calls.pop() {
                        Some((cf, ret_pc, dst, prev)) => {
                            base = prev as usize;
                            regs[base + (dst as usize)] = v;
                            pc = ret_pc as usize;
                            if cf as usize == cur {
                                continue 'win;
                            }
                            cur = cf as usize;
                            continue 'outer;
                        }
                        None => {
                            break 'outer RecOut::Done(if ret_bool {
                                Value::Bool(v != 0.0)
                            } else {
                                Value::Number(v)
                            })
                        }
                    },
                }
            }
        };
        self.kernel_regs = regs;
        calls.clear();
        self.rec_calls = calls;
        self.park_rec_family(fam);
        match out {
            RecOut::Done(v) => Some(Ok(v)),
            RecOut::Abandon => None,
            RecOut::Interrupted => {
                self.op_budget = Some(0);
                Some(Err(self.throw_range("execution interrupted")))
            }
        }
    }

    /// Take a cached, VALIDATED family for entry closure `bf`, or `None`
    /// (miss, or a dynamic check failed — the entry is dropped and the
    /// caller re-resolves, which is the uncached path and may itself
    /// decline).
    fn take_rec_family(&mut self, bf: &Rc<BytecodeFunction>) -> Option<RecFamily> {
        let i = self
            .rec_families
            .iter()
            .position(|f| Rc::ptr_eq(&f.funcs[0], bf))?;
        let mut fam = self.rec_families.swap_remove(i);
        if self.validate_rec_family(&mut fam) {
            Some(fam)
        } else {
            None
        }
    }

    /// Re-verify a cached family's DYNAMIC facts and refresh its upvalue
    /// snapshots. Static facts (kernel presence, Ret types, argc coverage,
    /// window sizes) live on immutable protos pinned by the identity checks,
    /// so they never need re-checking.
    fn validate_rec_family(&self, fam: &mut RecFamily) -> bool {
        {
            let g = self.realm.global.borrow();
            for c in fam.global_checks.iter() {
                let ok = matches!(
                    g.own_get_index(c.slot as usize),
                    Some((
                        k,
                        Property {
                            kind: PropertyKind::Data { value: Value::Object(o), .. },
                            ..
                        },
                    )) if *k == c.key && matches!(
                        &o.borrow().internal,
                        Internal::Function(FunctionInner::Bytecode(bf2))
                            if Rc::ptr_eq(bf2, &fam.funcs[c.func as usize])
                    )
                );
                if !ok {
                    return false;
                }
            }
        }
        for &(m, u) in fam.upval_self_checks.iter() {
            let f = &fam.funcs[m as usize];
            let ok = matches!(
                &*f.upvalues[u as usize].borrow(),
                Value::Object(o) if matches!(
                    &o.borrow().internal,
                    Internal::Function(FunctionInner::Bytecode(bf2))
                        if Rc::ptr_eq(bf2, f)
                )
            );
            if !ok {
                return false;
            }
        }
        for &m in fam.math_members.iter() {
            let k = fam.funcs[m as usize]
                .proto
                .fn_kernel
                .as_ref()
                .expect("family member");
            if !self.kernel_math_ok(&k.math_used) {
                return false;
            }
        }
        // Refresh upvalue snapshots: cells can change BETWEEN activations
        // (never during one — kernels contain no calls).
        let RecFamily {
            funcs, uv_snaps, ..
        } = fam;
        for (f, uvs) in funcs.iter().zip(uv_snaps.iter_mut()) {
            for (_, u, v) in uvs.iter_mut() {
                match &*f.upvalues[*u as usize].borrow() {
                    Value::Number(n) => *v = *n,
                    _ => return false,
                }
            }
        }
        true
    }

    /// Park a family back in the cache (MRU at the tail, bounded). Eviction
    /// only ever costs the evicted entry a re-resolution on its next call.
    fn park_rec_family(&mut self, fam: RecFamily) {
        const REC_FAMILY_CACHE_CAP: usize = 8;
        if self.rec_families.len() >= REC_FAMILY_CACHE_CAP {
            self.rec_families.remove(0);
        }
        self.rec_families.push(fam);
    }

    /// Resolve the recursion family for entry closure `bf` from scratch
    /// (`funcs[0]` = `bf`), recording the identity checks a cache hit will
    /// re-verify. `None` = the family declines kernelization (this
    /// activation runs generically); nothing is cached on decline.
    fn build_rec_family(&self, bf: &Rc<BytecodeFunction>) -> Option<RecFamily> {
        let ret_bool = bf.proto.fn_kernel.as_ref()?.ret_bool;
        let mut funcs: Vec<Rc<BytecodeFunction>> = vec![bf.clone()];
        // callee_map[j][c] = funcs index for member j's SelfCall callee c
        // (c = 0 is j itself; c = 1 + i is j's rec.globals[i]).
        let mut callee_map: Vec<Vec<u8>> = Vec::new();
        let mut global_checks: Vec<RecGlobalCheck> = Vec::new();
        let mut upval_self_checks: Vec<(u8, u32)> = Vec::new();
        let mut math_members: Vec<u8> = Vec::new();
        let mut wl = 0usize;
        while wl < funcs.len() {
            if funcs.len() > 8 {
                return None;
            }
            let f = funcs[wl].clone();
            let k = f.proto.fn_kernel.as_ref()?;
            // No member may write cells: the windowed executor snapshots
            // upvalues once per member and the family cache treats them as
            // activation constants. (Recursive members can't have writes —
            // translation declines — but a NON-recursive helper pulled in as
            // a call target can.)
            if !k.uv_writes.is_empty() {
                return None;
            }
            // Every member's Ret type must match the entry kernel's: a
            // mismatched value would land in a caller register of the wrong
            // static type. (Also rejects mixed-type non-recursive members.)
            if k.code
                .iter()
                .any(|op| matches!(op, KOp::Ret { boolean, .. } if *boolean != ret_bool))
            {
                return None;
            }
            // Self references must hold for THIS member's closure.
            if let Some(rec) = k.rec.as_deref() {
                for r in rec.self_refs.iter() {
                    match r {
                        crate::bytecode::SelfRefKind::Global(name) => {
                            let g = self.realm.global.borrow();
                            match g.own_get_full(&PropertyKey::str(name)) {
                                Some((
                                    slot,
                                    key,
                                    Property {
                                        kind:
                                            PropertyKind::Data {
                                                value: Value::Object(o),
                                                ..
                                            },
                                        ..
                                    },
                                )) if matches!(
                                    &o.borrow().internal,
                                    Internal::Function(FunctionInner::Bytecode(bf2))
                                        if Rc::ptr_eq(bf2, &f)
                                ) =>
                                {
                                    global_checks.push(RecGlobalCheck {
                                        // The MAP's key, so validation's
                                        // equality hits the Rc fast path.
                                        key: key.clone(),
                                        slot: u32::try_from(slot).ok()?,
                                        func: wl as u8,
                                    });
                                }
                                _ => return None,
                            }
                        }
                        crate::bytecode::SelfRefKind::Upvalue(u) => {
                            let cell = f.upvalues.get(*u as usize)?;
                            let ok = matches!(
                                &*cell.borrow(),
                                Value::Object(o) if matches!(
                                    &o.borrow().internal,
                                    Internal::Function(FunctionInner::Bytecode(bf2))
                                        if Rc::ptr_eq(bf2, &f)
                                )
                            );
                            if !ok {
                                return None;
                            }
                            upval_self_checks.push((wl as u8, *u));
                        }
                    }
                }
            }
            if !k.math_used.is_empty() {
                if !self.kernel_math_ok(&k.math_used) {
                    return None;
                }
                math_members.push(wl as u8);
            }
            // Resolve this member's mutual-recursion partners.
            let mut cmap: Vec<u8> = vec![wl as u8];
            if let Some(rec) = k.rec.as_deref() {
                for name in rec.globals.iter() {
                    let (bf2, slot, key) = {
                        let g = self.realm.global.borrow();
                        match g.own_get_full(&PropertyKey::str(name)) {
                            Some((
                                slot,
                                key,
                                Property {
                                    kind:
                                        PropertyKind::Data {
                                            value: Value::Object(o),
                                            ..
                                        },
                                    ..
                                },
                            )) => match &o.borrow().internal {
                                Internal::Function(FunctionInner::Bytecode(bf2))
                                    if !bf2.is_class_ctor
                                        && !bf2.proto.kind.is_generator()
                                        && !bf2.proto.kind.is_async() =>
                                {
                                    (bf2.clone(), slot, key.clone())
                                }
                                _ => return None,
                            },
                            _ => return None,
                        }
                    };
                    bf2.proto.fn_kernel.as_ref()?;
                    let idx = match funcs.iter().position(|x| Rc::ptr_eq(x, &bf2)) {
                        Some(i) => i,
                        None => {
                            funcs.push(bf2);
                            funcs.len() - 1
                        }
                    };
                    global_checks.push(RecGlobalCheck {
                        key,
                        slot: u32::try_from(slot).ok()?,
                        func: idx as u8,
                    });
                    cmap.push(u8::try_from(idx).ok()?);
                }
            }
            callee_map.push(cmap);
            wl += 1;
        }

        // --- Per-member tables: window size, arg slots, upvalue snapshots
        // (a member's upvalue cells are activation constants: nothing inside
        // a kernel can write a cell). Then cross-check every call site's
        // argc against its RESOLVED callee's consumption.
        let n = funcs.len();
        let mut arg_slots: Vec<Vec<(usize, usize)>> = Vec::with_capacity(n);
        let mut uv_snaps: Vec<Vec<(usize, u32, f64)>> = Vec::with_capacity(n);
        for f in funcs.iter() {
            let k = f.proto.fn_kernel.as_ref()?;
            let mut aslots = Vec::new();
            let mut uvs = Vec::new();
            for (r, slot) in k.locals.iter().enumerate() {
                match slot {
                    crate::bytecode::KSlot::Arg(a) => aslots.push((r, *a as usize)),
                    crate::bytecode::KSlot::Upvalue(u) => {
                        match &*f.upvalues.get(*u as usize)?.borrow() {
                            Value::Number(v) => uvs.push((r, *u, *v)),
                            _ => return None,
                        }
                    }
                    crate::bytecode::KSlot::Local(_) => {}
                }
            }
            arg_slots.push(aslots);
            uv_snaps.push(uvs);
        }
        for (j, f) in funcs.iter().enumerate() {
            let k = f.proto.fn_kernel.as_ref()?;
            for op in k.code.iter() {
                if let KOp::SelfCall { argc, callee, .. } = op {
                    let t = *callee_map[j].get(*callee as usize)? as usize;
                    let tk = funcs[t].proto.fn_kernel.as_ref()?;
                    if u32::from(*argc) < tk.args_used {
                        return None;
                    }
                }
            }
        }
        Some(RecFamily {
            funcs,
            callee_map,
            arg_slots,
            uv_snaps,
            global_checks,
            upval_self_checks,
            math_members,
            ret_bool,
        })
    }

    /// Prepare a callback for REPEATED calls from a native higher-order
    /// builtin (sort's comparator, map/filter/reduce/... callbacks): the
    /// invocation-invariant parts of the `run_fn_kernel` entry — callee
    /// resolution to a plain bytecode function with a non-recursive function
    /// kernel, the trace/accounting/depth checks, the register allocation —
    /// run ONCE here instead of on every element. `None` means the callback
    /// has no usable kernel; the caller just uses `Vm::call` per element,
    /// exactly as before.
    ///
    /// The prepared calls all happen at the builtin's (constant) call depth,
    /// so a single depth check mirrors the per-call guard — the same
    /// argument as the loop-kernel pinned-callee path. Tracing and per-op
    /// accounting are host-controlled (no JS between two prepared calls can
    /// enable them), so they too are entry checks; everything user code CAN
    /// change between elements is re-checked per call in
    /// [`Vm::exec_prepared_kernel`].
    pub(crate) fn prepare_kernel_callback(&mut self, func: &Value) -> Option<PreparedKernel> {
        let Value::Object(o) = func else { return None };
        let bf = {
            let b = o.borrow();
            match &b.internal {
                Internal::Function(FunctionInner::Bytecode(bf)) if !bf.is_class_ctor => bf.clone(),
                _ => return None,
            }
        };
        let kind = bf.proto.kind;
        if kind.is_generator() || kind.is_async() {
            return None;
        }
        {
            let k = bf.proto.fn_kernel.as_ref()?;
            // No upvalue writes: the prepared paths snapshot upvalues per
            // call (or once per sort) and never flush, so a cell-writing
            // callback falls back to the generic `Vm::call` per element.
            if k.rec.is_some() || !k.uv_writes.is_empty() {
                return None;
            }
        }
        if self.op_budget.is_some() || self.trace_sink.is_some() {
            return None;
        }
        if self.call_depth + 1 > self.max_call_depth {
            return None;
        }
        Some(PreparedKernel {
            regs: [0.0; KWIN],
            poll: 0,
            interrupt: self.interrupt.clone(),
            bf,
        })
    }

    /// One prepared call (see [`Vm::prepare_kernel_callback`]). `None` = a
    /// per-call guard failed — a non-`Number` argument or upvalue, per-op
    /// accounting installed, a non-canonical `Math` — and the caller falls
    /// back to the generic `Vm::call` for THIS element (keeping the handle
    /// for the next). `Some` is the call's exact result, bit-identical to
    /// the generic path by the `run_fn_kernel` argument.
    ///
    /// Math and upvalues are re-checked/re-read on EVERY call, unlike inside
    /// a single kernel activation: user code can run between two prepared
    /// calls (an element's getter or `valueOf` on the generic fallback path)
    /// and patch `Math` or write a captured cell.
    pub(crate) fn exec_prepared_kernel(
        &mut self,
        p: &mut PreparedKernel,
        args: &[Value],
    ) -> Option<Result<Value, Value>> {
        if self.op_budget.is_some() {
            return None;
        }
        let k = p.bf.proto.fn_kernel.as_ref().expect("prepared");
        if !k.math_used.is_empty() && !self.kernel_math_ok(&k.math_used) {
            return None;
        }
        for (r, slot) in k.locals.iter().enumerate() {
            match slot {
                crate::bytecode::KSlot::Arg(a) => match args.get(*a as usize) {
                    Some(Value::Number(n)) => p.regs[r & KWIN_MASK] = *n,
                    _ => return None,
                },
                // Register scratch: translation proved a store dominates
                // every read, so a stale value from the previous call is
                // never observable.
                crate::bytecode::KSlot::Local(_) => {}
                crate::bytecode::KSlot::Upvalue(u) => match &*p.bf.upvalues[*u as usize].borrow() {
                    Value::Number(n) => p.regs[r & KWIN_MASK] = *n,
                    _ => return None,
                },
            }
        }
        // Poll the cooperative interrupt flag per CALL as well as on kernel
        // back-edges: a tiny comparator body has no back-edge, and the native
        // merge loop between calls never ticks — without this, a hostile
        // n·log n sort would be uninterruptible.
        p.poll = p.poll.wrapping_add(1);
        if p.poll & 0xFF == 0 {
            if let Some(flag) = &p.interrupt {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    self.op_budget = Some(0);
                    return Some(Err(self.throw_range("execution interrupted")));
                }
            }
        }
        match exec_fn_kernel_code(&k.code, &mut p.regs, &p.interrupt, &mut p.poll) {
            Some(v) => Some(Ok(v)),
            None => {
                // Same latch-and-unwind as an interrupted kernel back-edge:
                // zero the budget so a JS catch cannot resume execution.
                self.op_budget = Some(0);
                Some(Err(self.throw_range("execution interrupted")))
            }
        }
    }

    /// Verify the guards `exec_prepared_kernel` re-checks per call — no op
    /// accounting, canonical `Math`, all-`Number` upvalues (pre-loaded into
    /// the registers here) — ONCE, for a run during which no user code can
    /// execute between calls: the all-Number sort specialization, where
    /// every value fed to the comparator is a raw `f64` and everything
    /// between two comparator calls is engine Rust + the pure kernel, so
    /// nothing can patch `Math`, write a captured cell, or install a budget
    /// mid-run. Returns the register indices of the two comparator
    /// parameters (`None` for a parameter the body never consumes); `None`
    /// overall declines the specialization.
    pub(crate) fn prime_prepared_cmp(
        &self,
        p: &mut PreparedKernel,
    ) -> Option<(Option<usize>, Option<usize>)> {
        if self.op_budget.is_some() {
            return None;
        }
        let k = p.bf.proto.fn_kernel.as_ref().expect("prepared");
        if !k.math_used.is_empty() && !self.kernel_math_ok(&k.math_used) {
            return None;
        }
        let mut ra = None;
        let mut rb = None;
        for (r, slot) in k.locals.iter().enumerate() {
            match slot {
                crate::bytecode::KSlot::Arg(0) => ra = Some(r),
                crate::bytecode::KSlot::Arg(1) => rb = Some(r),
                // Consumes a third argument the comparator is never given.
                crate::bytecode::KSlot::Arg(_) => return None,
                crate::bytecode::KSlot::Local(_) => {}
                crate::bytecode::KSlot::Upvalue(u) => match &*p.bf.upvalues[*u as usize].borrow() {
                    Value::Number(n) => p.regs[r & KWIN_MASK] = *n,
                    _ => return None,
                },
            }
        }
        Some((ra, rb))
    }

    /// One primed comparator call over raw `f64`s (see
    /// [`Vm::prime_prepared_cmp`]): write the two parameter registers, run
    /// the kernel, fold the `Number`/`Bool` result to the sort's -1/0/1
    /// (NaN → 0, as SortCompare's ToNumber+comparison does). The per-call
    /// interrupt poll stands in for the native merge loop, which never
    /// ticks.
    pub(crate) fn exec_prepared_cmp_f64(
        &mut self,
        p: &mut PreparedKernel,
        regs_ab: (Option<usize>, Option<usize>),
        x: f64,
        y: f64,
    ) -> Result<i32, Value> {
        if let Some(r) = regs_ab.0 {
            p.regs[r & KWIN_MASK] = x;
        }
        if let Some(r) = regs_ab.1 {
            p.regs[r & KWIN_MASK] = y;
        }
        p.poll = p.poll.wrapping_add(1);
        let interrupted = if p.poll & 0xFF == 0 {
            if let Some(flag) = &p.interrupt {
                flag.load(std::sync::atomic::Ordering::Relaxed)
            } else {
                false
            }
        } else {
            false
        };
        let k = p.bf.proto.fn_kernel.as_ref().expect("prepared");
        let ret = if interrupted {
            None
        } else {
            exec_fn_kernel_code(&k.code, &mut p.regs, &p.interrupt, &mut p.poll)
        };
        match ret {
            Some(Value::Number(n)) => Ok(if n < 0.0 {
                -1
            } else if n > 0.0 {
                1
            } else {
                0
            }),
            Some(Value::Bool(b)) => Ok(if b { 1 } else { 0 }),
            Some(_) => unreachable!("fn kernels return Number or Bool"),
            None => {
                self.op_budget = Some(0);
                Err(self.throw_range("execution interrupted"))
            }
        }
    }

    fn describe(&self, v: &Value) -> String {
        match v {
            Value::Undefined | Value::Uninitialized | Value::Hole => "undefined".into(),
            Value::Null => "null".into(),
            Value::String(s) => format!("\"{}\"", s.as_str()),
            Value::Number(n) => number_to_string(*n),
            Value::Bool(b) => b.to_string(),
            Value::Symbol(_) => "Symbol".into(),
            Value::BigInt(_) => "BigInt".into(),
            Value::Object(_) => "object".into(),
        }
    }

    pub fn call_object(
        &mut self,
        obj: &JsObject,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Value> {
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(self.throw_range("Maximum call stack size exceeded"));
        }
        let result = self.call_object_inner(obj, this, args, new_target);
        self.call_depth -= 1;
        result
    }

    fn call_object_inner(
        &mut self,
        obj: &JsObject,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Value> {
        // Extract the function kind / data without holding the borrow across the
        // recursive call.
        enum Disp {
            Native(NativeFn),
            Bytecode(Rc<BytecodeFunction>),
            Bound(JsObject, Value, Vec<Value>),
        }
        let disp = {
            let b = obj.borrow();
            match b.as_function() {
                Some(FunctionInner::Native(nf)) => Disp::Native(nf.func.clone()),
                Some(FunctionInner::Bytecode(bf)) => Disp::Bytecode(bf.clone()),
                Some(FunctionInner::Bound(bound)) => Disp::Bound(
                    bound.target.clone(),
                    bound.bound_this.clone(),
                    bound.bound_args.clone(),
                ),
                None => return Err(self.throw_type("not a function")),
            }
        };
        match disp {
            Disp::Native(f) => f(self, this, args),
            Disp::Bound(target, bthis, bargs) => {
                let mut all = bargs;
                all.extend_from_slice(args);
                self.call_object(&target, bthis, &all, new_target)
            }
            Disp::Bytecode(bf) => self.call_bytecode(obj, bf, this, args, new_target),
        }
    }

    fn call_bytecode(
        &mut self,
        func_obj: &JsObject,
        bf: Rc<BytecodeFunction>,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Value> {
        let kind = bf.proto.kind;
        // A class constructor is only reachable through [[Construct]] (spec
        // 10.2.1 step 2) — `C()`, `C.call(..)`, etc. all throw.
        if bf.is_class_ctor {
            return Err(self.throw_type(&format!(
                "Class constructor {} cannot be invoked without 'new'",
                bf.proto.name
            )));
        }
        if kind.is_generator() {
            return self.make_generator(func_obj, bf, this, args, new_target);
        }
        // Function kernel: frameless fast path (see `call_bytecode_vec`).
        if bf.proto.fn_kernel.is_some() {
            if let Some(r) = self.run_fn_kernel(&bf, args) {
                return r;
            }
        }
        let uses_arguments = bf.proto.uses_arguments;
        let mut frame = self.make_frame(bf, this, args, new_target);
        // See `call_bytecode_vec`: `func_obj` only feeds `arguments.callee`.
        if uses_arguments {
            frame.func_obj = Some(func_obj.clone());
        }
        let token = self.trace_enter(&frame.func.proto);
        frame.trace_token = token;
        if kind.is_async() {
            // start_async owns the exit/suspend bookkeeping for this token.
            Ok(self.start_async(frame))
        } else {
            match self.run_frame(frame) {
                Flow::Return(v) => {
                    self.trace_exit(token, false);
                    Ok(v)
                }
                Flow::Throw(e) => {
                    self.trace_exit(token, true);
                    Err(e)
                }
                Flow::Suspend(_) => {
                    let _ = func_obj;
                    Err(self.throw_type("internal: sync function suspended"))
                }
            }
        }
    }

    /// Build the `arguments` exotic object for a frame (spec 10.4.4):
    /// indexed own properties, `length`, `@@iterator` (%Array.prototype.values%),
    /// `[object Arguments]` tag, and `callee` — the function itself for a
    /// mapped (sloppy, simple-parameter-list) frame, the %ThrowTypeError%
    /// accessor otherwise. A mapped frame's indices ALIAS the parameter
    /// cells (reads/writes flow both ways) via the `Internal::Arguments` map.
    fn make_arguments_object(&mut self, frame: &Frame) -> Value {
        let map: Vec<Option<Rc<RefCell<Value>>>> = {
            let p = &frame.func.proto;
            if p.mapped_param_cells.is_empty() {
                Vec::new()
            } else {
                (0..frame.args.len().min(p.mapped_param_cells.len()))
                    .map(|i| p.mapped_param_cells[i].map(|c| frame.cells[c as usize].clone()))
                    .collect()
            }
        };
        let o = self.alloc(ObjectData::new(
            Some(self.realm.object_proto.clone()),
            Internal::Arguments(map),
        ));
        {
            let mut b = o.borrow_mut();
            for (i, v) in frame.args.iter().enumerate() {
                b.own_insert(
                    PropertyKey::from_index(i as u32),
                    Property {
                        kind: PropertyKind::Data {
                            value: v.clone(),
                            writable: true,
                        },
                        enumerable: true,
                        configurable: true,
                    },
                );
            }
            b.own_insert(
                PropertyKey::str("length"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(frame.args.len() as f64),
                        writable: true,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
        }
        // @@iterator: %Array.prototype.values% (writable, non-enum, configurable).
        let values = self
            .realm
            .array_proto
            .borrow()
            .own_get(&PropertyKey::str("values"))
            .and_then(|p| p.value().cloned())
            .unwrap_or(Value::Undefined);
        let iter_key = PropertyKey::Sym(self.realm.symbol_iterator.clone());
        o.borrow_mut().own_insert(
            iter_key,
            Property {
                kind: PropertyKind::Data {
                    value: values,
                    writable: true,
                },
                enumerable: false,
                configurable: true,
            },
        );
        // callee: mapped (sloppy + simple parameter list) exposes the function;
        // unmapped poisons it with the %ThrowTypeError% accessor.
        let p = &frame.func.proto;
        let simple_params = !p.has_rest
            && (p.num_params as usize) == p.param_names.len()
            && p.param_names.iter().all(|n| !n.is_empty());
        let callee = if !p.is_strict && simple_params {
            Property {
                kind: PropertyKind::Data {
                    value: frame
                        .func_obj
                        .as_ref()
                        .map(|f| Value::Object(f.clone()))
                        .unwrap_or(Value::Undefined),
                    writable: true,
                },
                enumerable: false,
                configurable: true,
            }
        } else {
            let tte = Value::Object(self.realm.throw_type_error.clone());
            Property {
                kind: PropertyKind::Accessor {
                    get: Some(tte.clone()),
                    set: Some(tte),
                },
                enumerable: false,
                configurable: false,
            }
        };
        o.borrow_mut()
            .own_insert(PropertyKey::str("callee"), callee);
        Value::Object(o)
    }

    /// Build a SuppressedError(error, suppressed) for DisposeResources'
    /// error-chaining; falls back to `error` if the intrinsic is unusable.
    fn make_suppressed_error(&mut self, error: Value, suppressed: Value) -> Value {
        let g = Value::Object(self.realm.global.clone());
        if let Ok(ctor) = self.get_prop(&g, &PropertyKey::str("SuppressedError")) {
            if self.is_constructor(&ctor) {
                let args = [
                    error.clone(),
                    suppressed,
                    Value::str("An error was suppressed during disposal"),
                ];
                if let Ok(se) = self.construct(&ctor, &args, &ctor.clone()) {
                    return se;
                }
            }
        }
        error
    }

    /// Cap on the pooled buffer free-list. Comfortably exceeds any realistic
    /// synchronous recursion depth (`max_call_depth` is 2000) so deep recursion
    /// keeps recycling, while a pathological churn can't retain unbounded memory.
    const VALUE_VEC_POOL_CAP: usize = 4096;

    /// Pull a cleared `Vec<Value>` from the pool, or allocate a fresh one.
    #[inline]
    pub(crate) fn take_value_vec(&mut self) -> Vec<Value> {
        self.value_vec_pool.pop().unwrap_or_default()
    }

    /// Return a buffer to the pool for reuse (clearing it drops any residual
    /// values). A never-grown (capacity 0) buffer isn't worth pooling; the list
    /// is size-capped so it can't retain memory without bound.
    #[inline]
    fn recycle_value_vec(&mut self, mut v: Vec<Value>) {
        if v.capacity() == 0 || self.value_vec_pool.len() >= Self::VALUE_VEC_POOL_CAP {
            return;
        }
        v.clear();
        self.value_vec_pool.push(v);
    }

    /// Reclaim a synchronously-finished frame WHOLE into the frame pool: every
    /// value-bearing field is cleared (the pool must never extend a value's
    /// lifetime — same discipline as the cell pool), while the stack / locals /
    /// cells / args buffers keep their capacity for the next call. Only called
    /// on `Return`/`Throw` exits — a suspended frame keeps its box (it rides
    /// inside the `Suspension`/generator state).
    #[inline]
    fn recycle_frame(&mut self, mut frame: Box<Frame>) {
        if self.frame_pool.len() >= Self::VALUE_VEC_POOL_CAP {
            // Over cap: fall back to recycling the inner buffers individually.
            let stack = std::mem::take(&mut frame.stack);
            let locals = std::mem::take(&mut frame.locals);
            let args = std::mem::take(&mut frame.args);
            self.recycle_value_vec(stack);
            self.recycle_value_vec(locals);
            self.recycle_value_vec(args);
            for cell in frame.cells.drain(..) {
                self.recycle_cell(cell);
            }
            return;
        }
        frame.func = self.dummy_bf.clone();
        frame.stack.clear();
        frame.locals.clear();
        for cell in frame.cells.drain(..) {
            self.recycle_cell(cell);
        }
        frame.this = Value::Undefined;
        frame.new_target = Value::Undefined;
        frame.handlers.clear();
        frame.pending_completion = None;
        frame.pending_throw = None;
        frame.pending_return = None;
        frame.args.clear();
        frame.func_obj = None;
        frame.dispose_scopes.clear();
        frame.completion = Value::Undefined;
        frame.enumerators.clear();
        frame.with_scope.clear();
        frame.trace_token = None;
        frame.skip_delegation_throw = false;
        frame.eval_vars = None;
        frame.priv_env = None;
        self.frame_pool.push(frame);
    }

    /// Pull a binding cell holding `v` from the pool, or allocate a fresh one.
    pub(crate) fn take_cell(&mut self, v: Value) -> Rc<RefCell<Value>> {
        match self.cell_pool.pop() {
            Some(c) => {
                *c.borrow_mut() = v;
                c
            }
            None => Rc::new(RefCell::new(v)),
        }
    }

    /// Return a cell to the pool — ONLY when nothing else can ever see it
    /// (`strong_count == 1`; a cell captured by a closure, upvalue chain,
    /// mapped-arguments alias, or module link stays out). Cleared on the way
    /// in so the pool never extends a value's lifetime.
    pub(crate) fn recycle_cell(&mut self, c: Rc<RefCell<Value>>) {
        if Rc::strong_count(&c) == 1 && self.cell_pool.len() < Self::VALUE_VEC_POOL_CAP {
            *c.borrow_mut() = Value::Undefined;
            self.cell_pool.push(c);
        }
    }

    pub fn make_frame(
        &mut self,
        bf: Rc<BytecodeFunction>,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> Box<Frame> {
        let mut args_buf = self.take_value_vec();
        args_buf.extend_from_slice(args);
        self.make_frame_owned(bf, this, args_buf, new_target)
    }

    /// As [`Vm::make_frame`], but adopts an already-owned argument buffer
    /// (the interpreter's pooled call-op buffer) as `frame.args` directly,
    /// skipping the copy.
    ///
    /// The frame itself comes from the frame pool when one is available: a
    /// recycled frame arrives scrubbed (see [`Vm::recycle_frame`]) with its
    /// four buffers' capacities intact, so this only re-initializes fields in
    /// place — no buffer pool round-trips, no ~400-byte struct move.
    pub fn make_frame_owned(
        &mut self,
        bf: Rc<BytecodeFunction>,
        this: Value,
        args: Vec<Value>,
        new_target: Value,
    ) -> Box<Frame> {
        let mut f = self.take_frame();
        self.init_frame(&mut f, bf, this, new_target);
        let old_args = std::mem::replace(&mut f.args, args);
        self.recycle_value_vec(old_args);
        f
    }

    /// Pop a scrubbed frame from the pool, or allocate a fresh (equally blank)
    /// one. Every field of a pooled frame was reset by [`Vm::recycle_frame`].
    #[inline]
    fn take_frame(&mut self) -> Box<Frame> {
        match self.frame_pool.pop() {
            Some(f) => f,
            None => Box::new(Frame {
                func: self.dummy_bf.clone(),
                ip: 0,
                stack: Vec::with_capacity(8),
                locals: Vec::new(),
                cells: Vec::new(),
                this: Value::Undefined,
                new_target: Value::Undefined,
                handlers: Vec::new(),
                pending_completion: None,
                pending_throw: None,
                pending_return: None,
                args: Vec::new(),
                func_obj: None,
                dispose_scopes: Vec::new(),
                completion: Value::Undefined,
                enumerators: Vec::new(),
                with_scope: Vec::new(),
                trace_token: None,
                skip_delegation_throw: false,
                eval_vars: None,
                priv_env: None,
            }),
        }
    }

    /// Initialize a blank (fresh or pool-scrubbed) frame's per-call fields in
    /// place. The caller provides `args` separately (either an owned buffer
    /// swapped in, or values moved straight off its operand stack).
    #[inline]
    fn init_frame(
        &mut self,
        f: &mut Frame,
        bf: Rc<BytecodeFunction>,
        this: Value,
        new_target: Value,
    ) {
        f.ip = 0;
        let proto = &bf.proto;
        for i in 0..proto.num_cells as usize {
            // A localized index lives in `frame.locals`; its cell slot is a
            // shared never-read placeholder (`Rc` bump, no pool round-trip).
            let c = if proto.localized.get(i).copied().unwrap_or(false) {
                self.dummy_cell.clone()
            } else {
                self.take_cell(Value::Undefined)
            };
            f.cells.push(c);
        }
        f.locals.resize(proto.num_locals as usize, Value::Undefined);
        // A closure created inside `with (o) { … }` carries the with-object
        // chain; seed the frame's with-scope stack with it so the body's
        // dynamic name ops resolve against it (under any with the body enters).
        f.with_scope.extend_from_slice(&bf.captured_with);
        f.priv_env = bf.captured_priv_env.clone();
        f.func = bf;
        f.this = this;
        f.new_target = new_target;
    }

    // =====================================================================
    // Construct
    // =====================================================================

    pub fn construct(
        &mut self,
        ctor: &Value,
        args: &[Value],
        new_target: &Value,
    ) -> Result<Value, Value> {
        let cobj = match ctor {
            // A constructable Proxy ([[Construct]] forwards to the construct
            // trap) — checked first, since a callable proxy now reports
            // `is_callable` and must not fall into the ordinary path.
            Value::Object(o) if matches!(o.borrow().internal, Internal::Proxy(_)) => {
                if !self.is_constructor(ctor) {
                    return Err(self.throw_type("not a constructor"));
                }
                return self.proxy_construct(&o.clone(), args, new_target.clone());
            }
            Value::Object(o) if o.borrow().is_callable() => o.clone(),
            _ => return Err(self.throw_type("not a constructor")),
        };
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(self.throw_range("Maximum call stack size exceeded"));
        }
        let r = self.construct_inner(&cobj, args, new_target);
        self.call_depth -= 1;
        r
    }

    fn construct_inner(
        &mut self,
        cobj: &JsObject,
        args: &[Value],
        new_target: &Value,
    ) -> Result<Value, Value> {
        enum Disp {
            Native(NativeFn),
            Bytecode(Rc<BytecodeFunction>),
            Bound(JsObject, Vec<Value>),
            NotCtor,
        }
        let disp = {
            let b = cobj.borrow();
            match b.as_function() {
                Some(FunctionInner::Native(nf)) => match &nf.construct {
                    Some(c) => Disp::Native(c.clone()),
                    None => Disp::NotCtor,
                },
                Some(FunctionInner::Bytecode(bf)) => {
                    if bf.proto.kind.is_async()
                        || bf.proto.kind.is_generator()
                        || bf.proto.kind.is_arrow()
                        || bf.proto.kind.is_method()
                    {
                        // Methods and accessors are not constructors
                        // (no [[Construct]]).
                        Disp::NotCtor
                    } else {
                        Disp::Bytecode(bf.clone())
                    }
                }
                Some(FunctionInner::Bound(bound)) => {
                    Disp::Bound(bound.target.clone(), bound.bound_args.clone())
                }
                None => Disp::NotCtor,
            }
        };
        match disp {
            Disp::NotCtor => {
                let name =
                    self.get_prop(&Value::Object(cobj.clone()), &PropertyKey::str("name"))?;
                let n = self.to_string_lossy(&name);
                Err(self.throw_type(&format!("{n} is not a constructor")))
            }
            Disp::Native(c) => {
                let r = c(self, Value::Undefined, args)?;
                // GetPrototypeFromConstructor: when constructed via a different
                // new.target (a subclass `super()` or Reflect.construct), the
                // fresh instance's [[Prototype]] comes from new_target.prototype
                // (falling back to the intrinsic default the builtin installed).
                // Results that merely echo an argument (`new Object(existing)`)
                // and proxies (no own [[Prototype]]) are left untouched.
                if !new_target.same_obj(cobj) {
                    if let Value::Object(res) = &r {
                        let echoes_arg = args.iter().any(|a| a.same_obj(res));
                        let is_proxy = matches!(res.borrow().internal, Internal::Proxy(_));
                        if !echoes_arg && !is_proxy && matches!(new_target, Value::Object(_)) {
                            let p = self.get_prop(new_target, &PropertyKey::str("prototype"))?;
                            if let Value::Object(po) = p {
                                res.borrow_mut().proto = Some(po);
                            }
                        }
                    }
                }
                Ok(r)
            }
            Disp::Bound(target, bargs) => {
                let mut all = bargs;
                all.extend_from_slice(args);
                let nt = if new_target.same_obj(cobj) {
                    Value::Object(target.clone())
                } else {
                    new_target.clone()
                };
                self.construct(&Value::Object(target), &all, &nt)
            }
            Disp::Bytecode(bf) => {
                // A derived-class constructor gets NO pre-created `this`: its
                // `%this` cell stays in TDZ until `super()` constructs the
                // instance (which is what gives `class A extends Array` a real
                // exotic array). The derived-constructor completion rules apply
                // HERE, at frame exit, so `finally` blocks (which may call
                // super()) have already run: an object return passes through;
                // undefined yields the bound `this` (ReferenceError when
                // super() never ran); any other primitive is a TypeError. The
                // `%this` cell is STABLE (same `Rc` for the whole call), so
                // watching it across run_frame is sound.
                if bf.proto.kind == FuncKind::DerivedCtor {
                    let this_cell = bf.proto.this_cell;
                    let frame = self.make_frame(bf, Value::Uninitialized, args, new_target.clone());
                    let watched = this_cell.map(|i| frame.cells[i as usize].clone());
                    return match self.run_frame(frame) {
                        Flow::Return(v) => match v {
                            Value::Object(_) => Ok(v),
                            Value::Undefined => {
                                let t = watched
                                    .map(|c| c.borrow().clone())
                                    .unwrap_or(Value::Uninitialized);
                                if matches!(t, Value::Uninitialized) {
                                    Err(self.throw_reference(
                                        "Must call super constructor in derived class before returning from derived constructor",
                                    ))
                                } else {
                                    Ok(t)
                                }
                            }
                            _ => Err(self.throw_type(
                                "Derived constructors may only return object or undefined",
                            )),
                        },
                        Flow::Throw(e) => Err(e),
                        Flow::Suspend(_) => Err(self.throw_type("internal: constructor suspended")),
                    };
                }
                // Create `this` with prototype from new_target.prototype.
                let nt_obj = match new_target {
                    Value::Object(o) => o.clone(),
                    _ => cobj.clone(),
                };
                let proto_val = self.get_prop(
                    &Value::Object(nt_obj.clone()),
                    &PropertyKey::str("prototype"),
                )?;
                let proto = match proto_val {
                    Value::Object(o) => Some(o),
                    _ => Some(self.realm.object_proto.clone()),
                };
                let this_obj = self.alloc_ordinary(proto);
                let this = Value::Object(this_obj.clone());
                let frame = self.make_frame(bf, this.clone(), args, new_target.clone());
                match self.run_frame(frame) {
                    Flow::Return(v) => {
                        if matches!(v, Value::Object(_)) {
                            Ok(v)
                        } else {
                            Ok(this)
                        }
                    }
                    Flow::Throw(e) => Err(e),
                    Flow::Suspend(_) => Err(self.throw_type("internal: constructor suspended")),
                }
            }
        }
    }

    // =====================================================================
    // The interpreter loop
    // =====================================================================

    /// Run a frame to completion (or suspension): the register tier when the
    /// proto carries a register program and per-op accounting is off, the
    /// stack interpreter otherwise. Every execution entry point (calls,
    /// [[Construct]], accessors, module/script evaluation, generator and
    /// async resumption) funnels through here, so the two tiers can never be
    /// entered inconsistently. Resumed generator/async frames always take the
    /// stack path by construction: their protos contain suspension ops, which
    /// decline register translation.
    pub fn run_frame(&mut self, frame: Box<Frame>) -> Flow {
        if let Some(reg) = &frame.func.proto.reg {
            // Budgeted runs (the production op budget, the conformance
            // runner, untrusted eval) stay on the register tier too:
            // `RegProto::costs` charges each register op its EXACT
            // stack-op units, so the budget drains — and exhausts —
            // identically on both tiers (gated by the budget-sweep
            // differential in tests/reg.rs). The kernel tiers still decline
            // under a budget. The debug-assert documents the resumed-frame
            // invariant.
            debug_assert!(
                frame.ip == 0 && frame.pending_throw.is_none() && frame.pending_return.is_none()
            );
            let reg = reg.clone();
            return self.run_reg_frame(frame, &reg);
        }
        self.run_stack_frame(frame)
    }

    fn run_stack_frame(&mut self, mut frame: Box<Frame>) -> Flow {
        // Recycle this frame whole into the frame pool, then return the
        // (already-owned) outcome. Used only on synchronous Return/Throw exits;
        // the Suspend paths move the whole frame (buffers included) into the
        // Suspension/generator state, so they must NOT recycle here.
        macro_rules! done {
            ($flow:expr) => {{
                let outcome = $flow;
                self.recycle_frame(frame);
                return outcome;
            }};
        }
        // The frame's compiled function is fixed for its whole lifetime; clone
        // the `Rc` once so the per-op fetch borrows from this local rather than
        // re-deriving it (and so it doesn't alias the `&mut frame` step needs).
        let proto = frame.func.proto.clone();
        let mut interrupt_poll: u32 = 0;
        // Injected resume completions are handled ONCE here, before the loop,
        // rather than re-checked on every iteration. `pending_throw` /
        // `pending_return` are set only by `resume_frame_throw` /
        // `resume_frame_return` immediately before `run_frame` (see `promise.rs`)
        // — a generator `.return(v)` or an awaited rejection delivered at resume.
        // Nothing inside the loop ever sets them, so once taken here they stay
        // `None` for the rest of the frame; this lifts two per-op `Option::take`s
        // off the hot path. A resolved `Jump` just positions `frame.ip` and falls
        // into the loop. (Phase 1, docs/interpreter-optimization.md.)
        if let Some(e) = frame.pending_throw.take() {
            match self.do_completion(&mut frame, Completion::Throw(e)) {
                Ok(Ctl::Jump(t)) => frame.ip = t,
                Ok(Ctl::Return(v)) => done!(Flow::Return(v)),
                Ok(_) => unreachable!("throw completion yields jump or return"),
                Err(e) => done!(Flow::Throw(e)),
            }
        } else if let Some(v) = frame.pending_return.take() {
            // Injected `.return(v)` on a suspended generator: dispatch a Return
            // completion so enclosing `finally` blocks run before the frame ends
            // (a `yield` in a finally re-suspends as a normal yield in the loop).
            match self.do_completion(&mut frame, Completion::Return(v)) {
                Ok(Ctl::Jump(t)) => frame.ip = t,
                Ok(Ctl::Return(rv)) => done!(Flow::Return(rv)),
                Ok(_) => unreachable!("return completion yields jump or return"),
                Err(e) => match self.do_completion(&mut frame, Completion::Throw(e)) {
                    Ok(Ctl::Jump(t)) => frame.ip = t,
                    Ok(Ctl::Return(rv)) => done!(Flow::Return(rv)),
                    Ok(_) => unreachable!(),
                    Err(e) => done!(Flow::Throw(e)),
                },
            }
        }
        // Budget/interrupt accounting hoisted to ONE register-friendly bool: in
        // the common case (no op budget, no interrupt flag — every production
        // run) the per-op cost is a single predicted-not-taken branch instead
        // of two `Option` loads through `self`. Sampled once per frame entry;
        // both are only ever installed BEFORE execution starts (the conformance
        // runner, untrusted eval), never from inside a running frame, so no
        // frame can miss a budget that applies to it. The interrupt latch below
        // zeroes the budget of an already-`counting` frame, and every frame
        // entered afterwards re-samples.
        let counting = self.op_budget.is_some() || self.interrupt.is_some();
        loop {
            if counting {
                if let Some(budget) = self.op_budget.as_mut() {
                    if *budget == 0 {
                        // Uncatchable so execution is guaranteed to terminate.
                        done!(Flow::Throw(self.throw_range("execution budget exceeded")));
                    }
                    *budget -= 1;
                }
                // Cooperative cancellation: poll the interrupt flag every 256
                // ops to keep the atomic load off the hot per-op path while
                // still reacting promptly even when individual ops are expensive
                // (e.g. O(n) string concatenation in a loop). Once observed,
                // latch it by zeroing the op budget so a JS `try/catch` around
                // the slow loop can't resume execution — guaranteeing a prompt,
                // terminating unwind.
                if self.interrupt.is_some() {
                    interrupt_poll = interrupt_poll.wrapping_add(1);
                    if interrupt_poll & 0xFF == 0 {
                        if let Some(flag) = &self.interrupt {
                            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                self.op_budget = Some(0);
                                done!(Flow::Throw(self.throw_range("execution interrupted")));
                            }
                        }
                    }
                }
            }
            let ip = frame.ip;
            if ip >= proto.code.len() {
                done!(Flow::Return(Value::Undefined));
            }
            frame.ip = ip + 1;
            // Phase-0 dynamic opcode-frequency instrumentation. Compiled out
            // entirely in the default build (the `op-histogram` feature is OFF
            // by default), so the shipping interpreter loop is byte-identical;
            // see `opstats.rs` and `docs/interpreter-optimization.md` §Phase 0.
            #[cfg(feature = "op-histogram")]
            crate::opstats::record(&proto.code[ip]);
            // Dispatch on a borrow of the instruction. `proto` is a clone of the
            // frame's (immutable) `FuncProto` Rc taken once per frame, so the
            // borrow is independent of `frame` and costs no per-op `Op::clone`
            // (previously ~5% of a call-heavy run's instructions).
            match self.step(&mut frame, &proto.code[ip]) {
                Ok(Ctl::Next) => continue,
                Ok(Ctl::Jump(target)) => {
                    frame.ip = target;
                    continue;
                }
                Ok(Ctl::Return(v)) => {
                    // Module linker hook: snapshot this frame's final cells when it
                    // is the module body being evaluated (matched by proto pointer).
                    if let Some(p) = &self.module_capture_proto {
                        if Rc::ptr_eq(&proto, p) {
                            self.module_capture = Some(frame.cells.clone());
                        }
                    }
                    done!(Flow::Return(v));
                }
                Ok(Ctl::Await(v)) => {
                    return Flow::Suspend(Suspension {
                        frame,
                        kind: SuspendKind::Await(v),
                    })
                }
                Ok(Ctl::Yield(v)) => {
                    return Flow::Suspend(Suspension {
                        frame,
                        kind: SuspendKind::Yield(v),
                    })
                }
                Ok(Ctl::YieldStar(v)) => {
                    return Flow::Suspend(Suspension {
                        frame,
                        kind: SuspendKind::YieldStar(v),
                    })
                }
                Ok(Ctl::GeneratorStart) => {
                    return Flow::Suspend(Suspension {
                        frame,
                        kind: SuspendKind::GeneratorStart,
                    })
                }
                Err(e) => match self.do_completion(&mut frame, Completion::Throw(e)) {
                    Ok(Ctl::Jump(t)) => {
                        frame.ip = t;
                        continue;
                    }
                    Ok(Ctl::Return(v)) => done!(Flow::Return(v)),
                    Ok(_) => unreachable!("throw completion yields jump or return"),
                    Err(e) => done!(Flow::Throw(e)),
                },
            }
        }
    }

    /// Drive a non-local completion (`return`/`break`/`continue`/throw) through
    /// the handler stack, running each enclosing `finally` it crosses. Pops
    /// handlers down to the completion's boundary (0 for return/throw; the target
    /// loop's handler depth for break/continue):
    /// - a `throw` that meets a handler with a `catch` jumps into the catch;
    /// - any handler with a `finally` parks the completion and jumps into the
    ///   finalizer (whose `EndFinally` resumes this dispatch);
    /// - once no more crossing handlers remain, the action is performed
    ///   (`Ctl::Return` / re-`throw` as `Err` / `Ctl::Jump` to the loop target).
    ///
    /// PerformEval (spec 19.2.1.1) for a direct call to %eval%: compile the
    /// source against the call site's scope snapshot, run the spec's
    /// EvalDeclarationInstantiation checks (the sloppy `var arguments`
    /// SyntaxError in function scopes; var-shadows-lexical), instantiate
    /// escaping sloppy vars on the global / the caller frame's eval-vars
    /// object, and execute the body with the caller's `this`, `new.target`,
    /// [[HomeObject]], and with-scope chain. Visible caller bindings become
    /// the body's upvalues, wired to the caller frame's LIVE cells.
    fn perform_direct_eval(
        &mut self,
        frame: &mut Frame,
        scope: u32,
        args: Vec<Value>,
    ) -> Result<Value, Value> {
        let arg0 = args.first().cloned().unwrap_or(Value::Undefined);
        let src = match &arg0 {
            Value::String(s) => s.as_str().to_string(),
            _ => return Ok(arg0), // non-string eval returns its argument
        };
        let desc = frame
            .func
            .proto
            .eval_scopes
            .get(scope as usize)
            .cloned()
            .ok_or_else(|| self.throw_type("internal: missing eval scope"))?;
        let compiled = match crate::compiler::compile_direct_eval(&src, &desc) {
            Ok(c) => c,
            Err(msg) => return Err(self.throw_syntax(msg.trim_start_matches("SyntaxError: "))),
        };
        // EvalDeclarationInstantiation for the sloppy escaping vars.
        if !compiled.strict {
            for name in &compiled.var_names {
                if name == "arguments" && desc.arguments_param_scope {
                    return Err(self.throw_syntax(
                        "Unexpected eval or arguments in eval code within a function",
                    ));
                }
                if let Some(b) = desc.bindings.iter().find(|b| &b.name == name) {
                    if b.is_lexical {
                        return Err(self.throw_syntax(&format!(
                            "Identifier '{name}' has already been declared"
                        )));
                    }
                    continue; // visible var/param: writes hit the live cell
                }
                if desc.is_global_var_scope {
                    // CanDeclareGlobalVar: a fresh global binding requires an
                    // extensible global object.
                    let g = self.realm.global.clone();
                    let key = PropertyKey::str(name);
                    let (present, extensible) = {
                        let b = g.borrow();
                        (b.own_contains_key(&key), b.extensible)
                    };
                    if !present {
                        if !extensible {
                            return Err(self
                                .throw_type(&format!("Cannot declare global variable '{name}'")));
                        }
                        // CreateGlobalVarBinding(name, D=true): an EVAL-created
                        // global var is deletable (configurable), unlike a
                        // script-level one.
                        g.borrow_mut()
                            .own_insert(key, Property::data(Value::Undefined));
                    }
                } else {
                    // Function-scope eval var: lives on the caller frame's
                    // eval-vars object (created by InitEvalVars at entry).
                    let ev = match &frame.eval_vars {
                        Some(o) => o.clone(),
                        None => {
                            let o =
                                self.alloc(crate::value::ObjectData::new(None, Internal::Ordinary));
                            frame.with_scope.insert(0, o.clone());
                            frame.eval_vars = Some(o.clone());
                            o
                        }
                    };
                    let key = PropertyKey::str(name);
                    if !ev.borrow().own_contains_key(&key) {
                        let mut p = Property::data(Value::Undefined);
                        p.enumerable = true;
                        ev.borrow_mut().own_insert(key, p);
                    }
                }
            }
        }
        // Wire the body's upvalues to the caller frame's live cells.
        let mut upvalues: Vec<Rc<RefCell<Value>>> = Vec::new();
        for uv in &compiled.proto.upvalues {
            let idx = match *uv {
                UpvalueSource::ParentCell(i) => i as usize,
                UpvalueSource::ParentUpvalue(_) => {
                    return Err(self.throw_type("internal: eval upvalue shape"))
                }
            };
            let b = desc
                .bindings
                .get(idx)
                .ok_or_else(|| self.throw_type("internal: eval binding index"))?;
            let cell = match b.slot {
                crate::bytecode::EvalSlot::Cell(i) => frame.cells[i as usize].clone(),
                crate::bytecode::EvalSlot::Upvalue(i) => frame.func.upvalues[i as usize].clone(),
            };
            upvalues.push(cell);
        }
        let bf = Rc::new(BytecodeFunction {
            proto: Rc::new(compiled.proto),
            upvalues,
            home_object: frame.func.home_object.clone(),
            is_class_ctor: false,
            captured_with: frame.with_scope.clone(),
            // The eval body resolves `#x` against the caller's private scope.
            captured_priv_env: frame.priv_env.clone(),
        });
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(self.throw_range("Maximum call stack size exceeded"));
        }
        let eframe = self.make_frame(bf, frame.this.clone(), &[], frame.new_target.clone());
        let flow = self.run_frame(eframe);
        self.call_depth -= 1;
        match flow {
            Flow::Return(v) => Ok(v),
            Flow::Throw(e) => Err(e),
            Flow::Suspend(_) => Err(self.throw_type("internal: eval body suspended")),
        }
    }

    fn do_completion(&mut self, frame: &mut Frame, comp: Completion) -> Result<Ctl, Value> {
        let boundary = match &comp {
            Completion::Jump { boundary, .. } => *boundary as usize,
            _ => 0,
        };
        // One-shot: an internal (await-rejection) throw resumption passes
        // `yield*` delegation handlers by — only external `.throw()` delegates.
        let skip_delegation = std::mem::take(&mut frame.skip_delegation_throw);
        while frame.handlers.len() > boundary {
            let h = frame.handlers.pop().unwrap();
            frame.stack.truncate(h.stack_depth);
            // Discard any `with` environments entered after this handler.
            frame.with_scope.truncate(h.with_depth);
            // Restore the private-environment chain (a class definition that
            // threw mid-evaluation must not leak its private scope).
            frame.priv_env = h.priv_env.clone();
            if let Completion::Throw(err) = &comp {
                if let Some(catch_ip) = h.catch_ip {
                    if !(skip_delegation && h.delegation) {
                        frame.stack.push(err.clone());
                        return Ok(Ctl::Jump(catch_ip as usize));
                    }
                }
            }
            // `yield*` return delegation: a `.return(v)` resumption crossing
            // the delegation handler is forwarded to the inner iterator's
            // `return` method instead of completing the outer generator.
            if let Completion::Return(v) = &comp {
                if let Some(return_ip) = h.delegation_return_ip {
                    frame.stack.push(v.clone());
                    return Ok(Ctl::Jump(return_ip as usize));
                }
            }
            if let Some(finally_ip) = h.finally_ip {
                frame.pending_completion = Some(comp);
                return Ok(Ctl::Jump(finally_ip as usize));
            }
        }
        match comp {
            Completion::Return(v) => Ok(Ctl::Return(v)),
            Completion::Throw(e) => Err(e),
            Completion::Jump { target, .. } => Ok(Ctl::Jump(target as usize)),
        }
    }

    fn const_val(&self, frame: &Frame, idx: u32) -> Value {
        match &frame.func.proto.consts[idx as usize] {
            Const::Undefined => Value::Undefined,
            Const::Null => Value::Null,
            Const::Bool(b) => Value::Bool(*b),
            Const::Number(n) => Value::Number(*n),
            Const::String(s) => Value::String(s.clone()),
            Const::Func(_) => Value::Undefined, // handled by Closure
            Const::BigInt(s) => {
                Value::bigint(parse_string_bigint(s).unwrap_or_else(|| num_bigint::BigInt::from(0)))
            }
        }
    }

    /// Resolve a private storage-key constant (`#x@<class id>`) through the
    /// frame's PrivateEnvironment chain to its runtime [`PrivateName`].
    fn resolve_private_name(&mut self, frame: &Frame, idx: u32) -> Result<PrivateName, Value> {
        let key = self.const_name(frame, idx);
        PrivateEnv::resolve(&frame.priv_env, key.as_str()).ok_or_else(|| {
            let desc = key
                .as_str()
                .rfind('@')
                .map_or(key.as_str(), |at| &key.as_str()[..at]);
            self.throw_syntax(&format!(
                "Private field '{desc}' must be declared in an enclosing class"
            ))
        })
    }

    // =====================================================================
    // Shared op bodies (stack interpreter + register tier)
    //
    // These are the op implementations with nontrivial inline logic — the
    // key-verified inline caches and the dense-element fast paths. Both
    // `step`/`step_cold` and `run_reg_frame` call them, so each op keeps
    // exactly ONE implementation and the two tiers cannot drift.
    // =====================================================================

    /// `Op::GetProp` / `ROp::GetProp` body: IC-accelerated named property
    /// read. See `FuncProto::ic` for the cache discipline (a stale hint is a
    /// miss, never a wrong answer).
    #[inline]
    pub(crate) fn ic_get_prop(
        &mut self,
        obj: Value,
        name: JsString,
        ic: Option<&crate::bytecode::IcEntry>,
    ) -> Result<Value, Value> {
        // Inline cache (key-verified hints; see `FuncProto::ic`).
        // Two levels:
        //  - `holder == None`: the receiver's OWN data property at
        //    `slot` (ordinary objects).
        //  - `holder == Some(p)`: a data property at `slot` on `p`,
        //    verified to still be the receiver's DIRECT prototype and
        //    not shadowed by an own property — the method-lookup
        //    pattern (`arr.push`, class instances calling prototype
        //    methods). Array receivers exclude keys with exotic own
        //    behavior (`length`, indices). Deeper chains, accessors,
        //    proxies, and every other exotic fall to the unchanged
        //    slow path.
        if let Value::Object(o) = &obj {
            if let Some(ic) = ic {
                let b = o.borrow();
                let (is_ord, is_arr) = (
                    matches!(b.internal, Internal::Ordinary),
                    matches!(b.internal, Internal::Array(_)),
                );
                // Array exotics: `length` and index keys never take
                // the IC (they don't live in the props map). But `length`
                // on an array with no own props — the loop bound of every
                // non-kernelized `for (i = 0; i < arr.length; i++)` — is
                // answered directly from the dense Vec, mirroring
                // `get_from_object`'s array arm (a reified `length` can
                // only live in `props`, which is empty here).
                if is_arr && b.own_is_empty() && name.as_str() == "length" {
                    if let Internal::Array(a) = &b.internal {
                        return Ok(Value::Number(a.len() as f64));
                    }
                }
                let plain_key = !is_arr
                    || (name.as_str() != "length"
                        && crate::value::canonical_index(name.as_str()).is_none());
                if (is_ord || is_arr) && plain_key {
                    // Own-property hit: never touches the holder cell.
                    if is_ord {
                        // Shape-verified hit (docs §3.3): one `Rc::ptr_eq`
                        // replaces the key compare + probe — shape identity
                        // pins the key at every slot; only the property KIND
                        // can differ (attributes live per-object), checked
                        // as before. Dictionary-mode receivers keep the
                        // key-verified slot hint unchanged.
                        let shape_ok = match (b.own_shape(), &*ic.own_shape.borrow()) {
                            (Some(s), Some(c)) => Rc::ptr_eq(c, s),
                            _ => false,
                        };
                        if shape_ok {
                            if let Some(prop) = b.own_prop_at(ic.own_slot.get() as usize) {
                                if let PropertyKind::Data { value, .. } = &prop.kind {
                                    let v = value.clone();
                                    return Ok(v);
                                }
                            }
                        } else if b.own_shape().is_none() {
                            if let Some((PropertyKey::Str(k), prop)) =
                                b.own_get_index(ic.own_slot.get() as usize)
                            {
                                if let PropertyKind::Data { value, .. } = &prop.kind {
                                    if k == &name {
                                        let v = value.clone();
                                        return Ok(v);
                                    }
                                }
                            }
                        }
                    }
                    // Proto hit: valid only when the receiver has no
                    // own props (nothing can shadow) and its CURRENT
                    // direct proto is the cached holder.
                    if b.own_is_empty() {
                        let holder = ic.holder.borrow();
                        if let Some(h) = &*holder {
                            if b.proto.as_ref().is_some_and(|p| p.ptr_eq(h)) {
                                let hb = h.borrow();
                                // Holder identity is already verified; a
                                // shape-verified slot replaces the key
                                // compare when the holder is shaped.
                                let shape_ok = match (hb.own_shape(), &*ic.proto_shape.borrow()) {
                                    (Some(s), Some(c)) => Rc::ptr_eq(c, s),
                                    _ => false,
                                };
                                if shape_ok {
                                    if let Some(prop) = hb.own_prop_at(ic.proto_slot.get() as usize)
                                    {
                                        if let PropertyKind::Data { value, .. } = &prop.kind {
                                            let v = value.clone();
                                            return Ok(v);
                                        }
                                    }
                                } else if hb.own_shape().is_none() {
                                    if let Some((PropertyKey::Str(k), prop)) =
                                        hb.own_get_index(ic.proto_slot.get() as usize)
                                    {
                                        if let PropertyKind::Data { value, .. } = &prop.kind {
                                            if k == &name {
                                                let v = value.clone();
                                                return Ok(v);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Refill: own data property first (ordinary only),
                    // then a one-level proto data property when no own
                    // props can shadow.
                    let key = PropertyKey::Str(name.clone());
                    if is_ord {
                        if let Some((idx, _, prop)) = b.own_get_full(&key) {
                            if let PropertyKind::Data { value, .. } = &prop.kind {
                                ic.own_slot.set(idx as u32);
                                let v = value.clone();
                                *ic.own_shape.borrow_mut() = b.own_shape().cloned();
                                return Ok(v);
                            }
                        }
                    }
                    if b.own_is_empty() {
                        if let Some(p) = &b.proto {
                            let pb = p.borrow();
                            if matches!(pb.internal, Internal::Ordinary)
                                || matches!(pb.internal, Internal::Array(_))
                            {
                                if let Some((idx, _, prop)) = pb.own_get_full(&key) {
                                    if let PropertyKind::Data { value, .. } = &prop.kind {
                                        ic.proto_slot.set(idx as u32);
                                        let v = value.clone();
                                        let holder_obj = p.clone();
                                        *ic.proto_shape.borrow_mut() = pb.own_shape().cloned();
                                        drop(pb);
                                        *ic.holder.borrow_mut() = Some(holder_obj);
                                        return Ok(v);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        self.get_prop(&obj, &PropertyKey::Str(name))
    }

    /// `Op::SetProp` / `ROp::SetProp` body: IC-accelerated named property
    /// write. Returns the value (the assignment expression's result).
    #[inline]
    pub(crate) fn ic_set_prop(
        &mut self,
        obj: Value,
        name: JsString,
        value: Value,
        strict: bool,
        ic: Option<&crate::bytecode::IcEntry>,
    ) -> Result<Value, Value> {
        // Inline cache (key-verified slot hint; see `FuncProto::ic`).
        // Fast path only when the receiver is an ordinary object whose
        // OWN property at the hinted slot is a WRITABLE data property —
        // per OrdinarySetWithOwnDescriptor that assignment updates the
        // value in place (attributes and slot order preserved, no
        // prototype setter can intervene). Everything else (missing own
        // key → proto-chain setters/read-only checks, accessors,
        // non-writable → strict TypeError, exotics) takes the
        // unchanged put_value path, which also refills the hint.
        if let Value::Object(o) = &obj {
            if let Some(ic) = ic {
                let mut b = o.borrow_mut();
                if matches!(b.internal, Internal::Ordinary) {
                    // Shape-verified write (docs §3.3): pointer compare, then
                    // the same writable-data-kind check as before.
                    let shape_ok = match (b.own_shape(), &*ic.own_shape.borrow()) {
                        (Some(s), Some(c)) => Rc::ptr_eq(c, s),
                        _ => false,
                    };
                    if shape_ok {
                        if let Some(prop) = b.own_prop_at_mut(ic.own_slot.get() as usize) {
                            if let PropertyKind::Data {
                                value: slot,
                                writable: true,
                            } = &mut prop.kind
                            {
                                *slot = value.clone();
                                return Ok(value);
                            }
                        }
                    } else if b.own_shape().is_none() {
                        if let Some((PropertyKey::Str(k), prop)) =
                            b.own_get_index_mut(ic.own_slot.get() as usize)
                        {
                            if k == &name {
                                if let PropertyKind::Data {
                                    value: slot,
                                    writable: true,
                                } = &mut prop.kind
                                {
                                    *slot = value.clone();
                                    return Ok(value);
                                }
                            }
                        }
                    }
                    let key = PropertyKey::Str(name.clone());
                    let new_shape = b.own_shape().cloned();
                    if let Some((idx, _, prop)) = b.own_get_full_mut(&key) {
                        if let PropertyKind::Data {
                            value: slot,
                            writable: true,
                        } = &mut prop.kind
                        {
                            ic.own_slot.set(idx as u32);
                            *ic.own_shape.borrow_mut() = new_shape;
                            *slot = value.clone();
                            return Ok(value);
                        }
                    }
                }
            }
        }
        self.put_value(&obj, &PropertyKey::Str(name), value.clone(), strict)?;
        Ok(value)
    }

    /// `Op::GetPropDynamic` / `ROp::GetElem` body: computed-key read with the
    /// dense-array integer fast path.
    #[inline]
    pub(crate) fn elem_get(&mut self, obj: Value, key_v: Value) -> Result<Value, Value> {
        // Integer fast path: `a[i]` on a dense array with an integral
        // Number key reads the element directly — skipping
        // ToPropertyKey's Number→String conversion (a float-format +
        // heap allocation per access!), the reparse back to an index,
        // and the property-map machinery. Only when no reified props
        // entry can shadow the dense element (`props.is_empty()`) and
        // the slot is a real element (in bounds, not a hole);
        // everything else takes the unchanged spec path.
        if let (Value::Object(o), Value::Number(n)) = (&obj, &key_v) {
            if let Some(iu) = dense_index(*n) {
                let b = o.borrow();
                if let Internal::Array(arr) = &b.internal {
                    if b.own_is_empty() {
                        if let Some(v) = arr.get(iu) {
                            if !matches!(v, Value::Hole) {
                                let v = v.clone();
                                return Ok(v);
                            }
                        }
                    }
                }
            }
        }
        // String fast path: `s[i]` with an integral in-bounds Number key
        // yields the code unit directly (O(1) on ASCII), skipping the
        // Number→String key conversion and the digit re-parse inside
        // `string_own_prop`. Out-of-bounds falls through — the spec path
        // climbs to String.prototype, which is observable.
        if let (Value::String(s), Value::Number(n)) = (&obj, &key_v) {
            if let Some(iu) = dense_index(*n) {
                if let Some(u) = s.code_unit_at(iu) {
                    return Ok(Value::String(JsString::from_code_units(&[u])));
                }
            }
        }
        // GetValue: RequireObjectCoercible(base) (via ToObject) throws
        // BEFORE ToPropertyKey coerces the key expression's value.
        self.require_object_coercible(&obj, "read properties of")?;
        let key = self.to_property_key(&key_v)?;
        self.get_prop(&obj, &key)
    }

    /// `Op::SetPropDynamic` / `ROp::SetElem` body: computed-key write with
    /// the dense-array fast paths (in-place overwrite, hole fill, append).
    /// Returns the value (the assignment expression's result).
    #[inline]
    pub(crate) fn elem_set(
        &mut self,
        obj: Value,
        key_v: Value,
        value: Value,
        strict: bool,
    ) -> Result<Value, Value> {
        // Integer fast path mirroring `elem_get`: overwrite an
        // EXISTING dense element in place (an in-bounds non-hole dense
        // slot is a plain writable data property per the array exotic
        // [[Set]] — no setter, no length change, no extensibility
        // interaction), fill an in-bounds HOLE, or append at exactly
        // `length`. Filling and appending CREATE a property (the own
        // property is absent), so they additionally require the array
        // to be extensible (a sealed/prevented receiver must reject
        // through the generic path), the dense-storage bound (the
        // generic path owns that RangeError), and a prototype chain
        // with no reified entry at the index — OrdinarySet consults
        // the chain for an absent own property, so a proto accessor /
        // non-writable index must intercept via the generic path
        // (`protos_allow_index_create`). Gaps past the end, shadowed
        // elements, and non-arrays take the spec path.
        if let (Value::Object(o), Value::Number(n)) = (&obj, &key_v) {
            if let Some(i) = dense_index(*n) {
                let mut creates = None;
                {
                    let mut b = o.borrow_mut();
                    if b.own_is_empty() {
                        let extensible = b.extensible;
                        if let Internal::Array(arr) = &mut b.internal {
                            match arr.get_mut(i) {
                                Some(slot) if !matches!(slot, Value::Hole) => {
                                    *slot = value.clone();
                                    return Ok(value);
                                }
                                Some(_) => {
                                    if extensible {
                                        creates = Some(b.proto.clone());
                                    }
                                }
                                None => {
                                    if extensible
                                        && i == arr.len()
                                        && i < crate::value::MAX_DENSE_ARRAY
                                    {
                                        creates = Some(b.proto.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(proto) = creates {
                    if crate::value::protos_allow_index_create(proto, i as u32, 1) {
                        // No user code ran since the conditions were
                        // checked (same native frame), so they still
                        // hold.
                        let mut b = o.borrow_mut();
                        if let Internal::Array(arr) = &mut b.internal {
                            match arr.get_mut(i) {
                                Some(slot) => *slot = value.clone(),
                                None => arr.push(value.clone()),
                            }
                            return Ok(value);
                        }
                    }
                }
            }
        }
        self.require_object_coercible(&obj, "set properties of")?;
        let key = self.to_property_key(&key_v)?;
        self.put_value(&obj, &key, value.clone(), strict)?;
        Ok(value)
    }

    /// `Op::LoadGlobal` / `ROp::LoadGlobal` body: IC-accelerated global read.
    #[inline]
    pub(crate) fn ic_load_global(
        &mut self,
        name: JsString,
        ic: Option<&crate::bytecode::IcEntry>,
    ) -> Result<Value, Value> {
        let g = self.realm.global.clone();
        // Inline cache (key-verified slot hint; see `FuncProto::ic`):
        // after the first resolution, a global read — above all a
        // function resolving its own recursive self-reference — is one
        // indexed load plus a pointer-equality key check, no hashing.
        if let Some(ic) = ic {
            let b = g.borrow();
            if let Some((PropertyKey::Str(k), prop)) = b.own_get_index(ic.own_slot.get() as usize) {
                if let PropertyKind::Data { value, .. } = &prop.kind {
                    if k == &name {
                        let v = value.clone();
                        return Ok(v);
                    }
                }
            }
        }
        let key = PropertyKey::Str(name.clone());
        // Fast path: an own data property directly on the global object —
        // the case for every top-level `function`/`var`/`let` binding.
        // Resolves in a single hash with no prototype walk, and refills
        // the inline cache with the slot it finds.
        let fast = {
            let b = g.borrow();
            match b.own_get_full(&key) {
                Some((
                    idx,
                    _,
                    Property {
                        kind: PropertyKind::Data { value, .. },
                        ..
                    },
                )) => {
                    if let Some(ic) = ic {
                        ic.own_slot.set(idx as u32);
                    }
                    Some(value.clone())
                }
                _ => None,
            }
        };
        match fast {
            Some(v) => Ok(v),
            None => {
                // Accessor global, or a binding inherited via the global's
                // prototype chain: fall back to the full [[Get]] (after the
                // unresolvable-reference check that yields a ReferenceError).
                if !self.has_own_or_proto(&g, &key) {
                    return Err(self.throw_reference(&format!("{} is not defined", name.as_str())));
                }
                self.get_prop(&Value::Object(g), &key)
            }
        }
    }

    /// `Op::StoreGlobal` / `ROp::StoreGlobal` body.
    #[inline]
    pub(crate) fn store_global(
        &mut self,
        name: JsString,
        v: Value,
        strict: bool,
    ) -> Result<(), Value> {
        let g = self.realm.global.clone();
        let key = PropertyKey::Str(name.clone());
        // A bare assignment to a name bound nowhere is an unresolvable
        // reference; PutValue on one throws ReferenceError in strict mode
        // (a global-object property anywhere on the proto chain counts as
        // resolvable). Sloppy mode creates the global property instead.
        if strict && !self.has_prop(&Value::Object(g.clone()), &key)? {
            return Err(self.throw_reference(&format!("{} is not defined", name.as_str())));
        }
        self.put_value(&Value::Object(g), &key, v, strict)
    }

    /// `Op::Closure` / `ROp::Closure` body: instantiate the nested function
    /// template at const index `idx`, capturing cells/upvalues from `frame`
    /// (plus the active with-scope, private-env, and [[HomeObject]]).
    pub(crate) fn closure_from_const(&mut self, frame: &Frame, idx: u32) -> Result<Value, Value> {
        let proto = match &frame.func.proto.consts[idx as usize] {
            Const::Func(p) => p.clone(),
            _ => return Err(self.throw_type("internal: bad closure const")),
        };
        let upvalues = proto
            .upvalues
            .iter()
            .map(|src| match src {
                UpvalueSource::ParentCell(idx) => frame.cells[*idx as usize].clone(),
                UpvalueSource::ParentUpvalue(idx) => frame.func.upvalues[*idx as usize].clone(),
            })
            .collect();
        let inherit_home = proto.kind.is_arrow() || proto.inherit_home;
        let f = self.make_closure(proto, upvalues);
        // Capture the active with-scope chain (closures defined inside
        // `with` resolve free identifiers against it after the block)
        // and the active private-environment chain (methods and
        // initializers defined inside class bodies resolve `#x`
        // against it). Arrows (and synthetic in-class closures) also
        // inherit the creating frame's [[HomeObject]], so `super.x`
        // inside them resolves like the enclosing method.
        if !frame.with_scope.is_empty()
            || frame.priv_env.is_some()
            || (inherit_home && frame.func.home_object.is_some())
        {
            if let Internal::Function(FunctionInner::Bytecode(bf)) = &mut f.borrow_mut().internal {
                let bf = Rc::make_mut(bf);
                bf.captured_with = frame.with_scope.clone();
                bf.captured_priv_env = frame.priv_env.clone();
                if inherit_home {
                    bf.home_object = frame.func.home_object.clone();
                }
            }
        }
        Ok(Value::Object(f))
    }

    /// `Op::BindThisSloppy` / `ROp::BindThisSloppy` body.
    pub(crate) fn bind_this_sloppy(&mut self, t: Value) -> Result<Value, Value> {
        Ok(match t {
            Value::Undefined | Value::Null => Value::Object(self.realm.global.clone()),
            Value::Object(_) => t,
            // A primitive `this` is boxed (ToObject) in sloppy mode.
            other => Value::Object(self.to_object(&other)?),
        })
    }

    /// `Op::LoadRestArgs` / `ROp::RestArgs` body.
    pub(crate) fn rest_args(&mut self, frame: &Frame, n: u32) -> Value {
        let rest: Vec<Value> = if (n as usize) < frame.args.len() {
            frame.args[n as usize..].to_vec()
        } else {
            Vec::new()
        };
        Value::Object(self.new_array(rest))
    }

    /// `Op::DeclareGlobal` / `ROp::DeclareGlobal` body.
    pub(crate) fn declare_global(&mut self, name: JsString, deletable: bool) -> Result<(), Value> {
        let g = self.realm.global.clone();
        let key = PropertyKey::Str(name.clone());
        let (present, extensible) = {
            let b = g.borrow();
            (b.own_contains_key(&key), b.extensible)
        };
        if !present {
            // CanDeclareGlobalVar/Function: needs an extensible global.
            if !extensible {
                return Err(self.throw_type(&format!("Cannot declare global '{}'", name.as_str())));
            }
            // CreateGlobalVarBinding(N, D): writable, enumerable;
            // configurable only for eval-created bindings.
            g.borrow_mut().own_insert(
                key,
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Undefined,
                        writable: true,
                    },
                    enumerable: true,
                    configurable: deletable,
                },
            );
        }
        Ok(())
    }

    /// `Op::CanDeclareGlobalFunc` / `ROp::CanDeclareGlobalFunc` body.
    pub(crate) fn can_declare_global_func(&mut self, name: JsString) -> Result<(), Value> {
        let g = self.realm.global.clone();
        let key = PropertyKey::Str(name.clone());
        // CanDeclareGlobalFunction (9.1.1.4.16): an existing
        // non-configurable property is acceptable only if it is a
        // writable, enumerable data property; otherwise (or, when
        // absent, on a non-extensible global) the declaration fails.
        let definable = {
            let b = g.borrow();
            match b.own_get(&key) {
                None => b.extensible,
                Some(p) => {
                    p.configurable
                        || matches!(
                            &p.kind,
                            PropertyKind::Data { writable, .. }
                                if *writable && p.enumerable
                        )
                }
            }
        };
        if !definable {
            return Err(self.throw_type(&format!(
                "Cannot declare global function '{}'",
                name.as_str()
            )));
        }
        Ok(())
    }

    /// `Op::DefineGlobalFunc` / `ROp::DefineGlobalFunc` body.
    pub(crate) fn define_global_func(&mut self, name: JsString, value: Value, deletable: bool) {
        let g = self.realm.global.clone();
        let key = PropertyKey::Str(name.clone());
        // CreateGlobalFunctionBinding (9.1.1.4.18): an absent or
        // configurable existing property is (re)defined with function
        // attributes; a non-configurable one keeps its attributes and
        // is just assigned the new value.
        let redefine = {
            let b = g.borrow();
            match b.own_get(&key) {
                None => true,
                Some(p) => p.configurable,
            }
        };
        let mut b = g.borrow_mut();
        if redefine {
            b.own_insert(
                key,
                Property {
                    kind: PropertyKind::Data {
                        value,
                        writable: true,
                    },
                    enumerable: true,
                    configurable: deletable,
                },
            );
        } else if let Some(p) = b.own_get_mut(&key) {
            if let PropertyKind::Data { value: slot, .. } = &mut p.kind {
                *slot = value;
            }
        }
    }

    /// `Op::GetTemplateObject` / `ROp::GetTemplateObject` body: the cached,
    /// frozen template object, keyed by `(proto pointer, index)`.
    pub(crate) fn template_object(&mut self, frame: &Frame, idx: u32) -> Result<Value, Value> {
        let key = (Rc::as_ptr(&frame.func.proto) as *const () as usize, idx);
        if let Some(o) = self.template_cache.get(&key) {
            return Ok(Value::Object(o.clone()));
        }
        let parts = frame.func.proto.templates[idx as usize].clone();
        // Cooked strings (an illegal escape cooks to `undefined`).
        let cooked: Vec<Value> = parts
            .cooked
            .iter()
            .map(|c| match c {
                Some(s) => Value::String(JsString::from_rc_str(s.clone())),
                None => Value::Undefined,
            })
            .collect();
        let arr = self.new_array(cooked);
        let raw: Vec<Value> = parts
            .raw
            .iter()
            .map(|s| Value::String(JsString::from_rc_str(s.clone())))
            .collect();
        let raw_arr = self.new_array(raw);
        // The `raw` array is frozen; `raw` is a non-enumerable,
        // non-writable, non-configurable own property of the
        // template object, which is itself frozen (spec
        // GetTemplateObject / TemplateString integrity).
        crate::builtins::fundamental::set_integrity_level(self, &raw_arr, true)?;
        arr.borrow_mut().own_insert(
            PropertyKey::str("raw"),
            Property {
                kind: PropertyKind::Data {
                    value: Value::Object(raw_arr),
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            },
        );
        crate::builtins::fundamental::set_integrity_level(self, &arr, true)?;
        self.template_cache.insert(key, arr.clone());
        Ok(Value::Object(arr))
    }

    /// `Op::ArraySpread` / `ROp::ArraySpread` body.
    pub(crate) fn array_spread(&mut self, arr_v: &Value, src: &Value) -> Result<(), Value> {
        let items = self.iterate_to_vec(src)?;
        if let Value::Object(a) = arr_v {
            let mut b = a.borrow_mut();
            if let Internal::Array(elems) = &mut b.internal {
                elems.extend(items);
            }
        }
        Ok(())
    }

    /// The six `Op::Define*` / `ROp::DefineProp` bodies.
    pub(crate) fn define_prop_kind(
        &mut self,
        kind: crate::reg::DefKind,
        obj: &Value,
        key_v: &Value,
        value: Value,
    ) -> Result<(), Value> {
        use crate::reg::DefKind;
        let key = self.to_property_key(key_v)?;
        match kind {
            DefKind::Field => {
                if let Value::Object(o) = obj {
                    crate::builtins::fundamental::create_data_property_or_throw(
                        self, o, &key, value,
                    )?;
                }
            }
            DefKind::Method => {
                // Class methods are non-enumerable (writable, configurable),
                // defined with DefinePropertyOrThrow (a non-configurable own
                // key — e.g. a computed static "prototype" — is a TypeError).
                self.check_redefinable(obj, &key)?;
                if let Value::Object(o) = obj {
                    o.borrow_mut().own_insert(key, Property::builtin(value));
                }
            }
            DefKind::Getter => {
                self.check_redefinable(obj, &key)?;
                self.define_accessor_with(obj, key, Some(value), None, true);
            }
            DefKind::Setter => {
                self.check_redefinable(obj, &key)?;
                self.define_accessor_with(obj, key, None, Some(value), true);
            }
            DefKind::MethodGetter => {
                self.check_redefinable(obj, &key)?;
                self.define_accessor_with(obj, key, Some(value), None, false);
            }
            DefKind::MethodSetter => {
                self.check_redefinable(obj, &key)?;
                self.define_accessor_with(obj, key, None, Some(value), false);
            }
        }
        Ok(())
    }

    /// `Op::SetHomeObject` / `ROp::SetHomeObject` body (MakeMethod).
    pub(crate) fn set_home_object_op(home: &Value, method: &Value) {
        if let (Value::Object(home), Value::Object(m)) = (home, method) {
            if let Internal::Function(FunctionInner::Bytecode(bf)) = &mut m.borrow_mut().internal {
                Rc::make_mut(bf).home_object = Some(home.clone());
            }
        }
    }

    /// `Op::ObjectSpread` / `ROp::ObjectSpread` body.
    pub(crate) fn object_spread(&mut self, target: &Value, src: &Value) -> Result<(), Value> {
        if let Value::Object(t) = target {
            if let Value::Object(s) = src {
                for k in self.enumerable_own_keys_dyn(s)? {
                    let val = self.get_prop(src, &k)?;
                    t.borrow_mut().own_insert(k, Property::data(val));
                }
            } else if let Value::String(st) = src {
                for (i, c) in st.as_str().chars().enumerate() {
                    t.borrow_mut().own_insert(
                        PropertyKey::from_index(i as u32),
                        Property::data(Value::str(c.to_string())),
                    );
                }
            }
        }
        Ok(())
    }

    /// `Op::CopyDataPropertiesExcept` / `ROp::CopyDataPropsExcept` body
    /// (keys already converted to property keys, in stack order).
    pub(crate) fn copy_data_props_except(
        &mut self,
        target: &Value,
        src: &Value,
        excluded: &[PropertyKey],
    ) -> Result<(), Value> {
        if let Value::Object(t) = target {
            if let Value::Object(s) = src {
                // Excluded keys are skipped before ANY source access
                // (CopyDataProperties step 4.b.i): a proxy source must
                // not observe a [[GetOwnProperty]]/[[Get]] for them.
                for k in self.enumerable_own_keys_excluding(s, excluded)? {
                    let val = self.get_prop(src, &k)?;
                    t.borrow_mut().own_insert(k, Property::data(val));
                }
            } else if let Value::String(st) = src {
                // A primitive-string source contributes its index keys.
                for (i, c) in st.as_str().chars().enumerate() {
                    let k = PropertyKey::from_index(i as u32);
                    if excluded.contains(&k) {
                        continue;
                    }
                    t.borrow_mut()
                        .own_insert(k, Property::data(Value::str(c.to_string())));
                }
            }
        }
        Ok(())
    }

    /// `Op::SetFunctionNameFromKey` / `ROp::SetFunctionNameFromKey` body.
    pub(crate) fn set_function_name_from_key(
        &mut self,
        key: &Value,
        value: &Value,
        prefix: JsString,
    ) {
        if let Value::Object(f) = value {
            if f.borrow().is_callable() {
                let base = match key {
                    Value::Symbol(sym) => match sym.description() {
                        Some(d) => format!("[{d}]"),
                        None => String::new(),
                    },
                    other => self.to_string_lossy(other),
                };
                let name = if prefix.as_str().is_empty() {
                    base
                } else {
                    format!("{} {}", prefix.as_str(), base)
                };
                f.borrow_mut().own_insert(
                    PropertyKey::str("name"),
                    Property {
                        kind: PropertyKind::Data {
                            value: Value::str(name),
                            writable: false,
                        },
                        enumerable: false,
                        configurable: true,
                    },
                );
            }
        }
    }

    /// `Op::SetProtoFromLiteral` / `ROp::SetProtoFromLiteral` body.
    pub(crate) fn set_proto_from_literal(obj: &Value, v: Value) {
        if let Value::Object(o) = obj {
            match v {
                Value::Object(p) => o.borrow_mut().proto = Some(p),
                Value::Null => o.borrow_mut().proto = None,
                // Non-object, non-null values are silently ignored.
                _ => {}
            }
        }
    }

    /// `Op::DeleteProp` / `ROp::DelProp` body.
    pub(crate) fn del_prop_named(
        &mut self,
        obj: Value,
        name: JsString,
        strict: bool,
    ) -> Result<Value, Value> {
        // `delete base.x` does ToObject(base) (spec step 5.b): a
        // nullish base is a TypeError before any delete is attempted.
        self.require_object_coercible(&obj, "delete properties of")?;
        let r = self.delete_prop(&obj, &PropertyKey::Str(name.clone()))?;
        // Strict-mode `delete` that fails throws (spec 13.5.1.2 step 5.c).
        if !r && strict {
            return Err(self.throw_type(&format!(
                "Cannot delete property '{}' in strict mode",
                name.as_str()
            )));
        }
        Ok(Value::Bool(r))
    }

    /// `Op::DeletePropDynamic` / `ROp::DelElem` body.
    pub(crate) fn del_prop_dynamic(
        &mut self,
        obj: Value,
        key_v: Value,
        strict: bool,
    ) -> Result<Value, Value> {
        self.require_object_coercible(&obj, "delete properties of")?;
        let key = self.to_property_key(&key_v)?;
        let r = self.delete_prop(&obj, &key)?;
        if !r && strict {
            return Err(self.throw_type("Cannot delete property in strict mode"));
        }
        Ok(Value::Bool(r))
    }

    /// `Op::ConcatStrings` / `ROp::ConcatStrings` body (template literals).
    pub(crate) fn concat_strings(&mut self, parts: &[Value]) -> Result<Value, Value> {
        let mut strs = Vec::with_capacity(parts.len());
        let mut total = 0usize;
        for p in parts {
            let s = self.to_js_string(p)?;
            // Same bound as `op_add`: a template-literal join in a doubling
            // loop (`` s = `${s}${s}` ``) must not grow without limit.
            total += s.byte_len();
            if total > crate::value::MAX_STRING_LEN {
                return Err(self.throw_range("invalid string length"));
            }
            strs.push(s);
        }
        // Fast path when every part is well-formed (the common case):
        // a plain UTF-8 join. Otherwise go through code units so a
        // surrogate straddling a boundary re-pairs (and lone surrogates
        // survive instead of becoming U+FFFD).
        let out = if strs.iter().all(|s| s.is_well_formed()) {
            let mut out = String::with_capacity(total);
            for s in &strs {
                out.push_str(s.as_str());
            }
            JsString::new(out)
        } else {
            let mut units = Vec::new();
            for s in &strs {
                units.extend(s.code_units());
            }
            JsString::from_code_units(&units)
        };
        Ok(Value::String(out))
    }

    /// `Op::ForInEnumerate` / `ROp::ForInEnumerate` body: push a fresh
    /// enumerator, returning its index value.
    pub(crate) fn for_in_enumerate(
        &mut self,
        frame: &mut Frame,
        v: &Value,
    ) -> Result<Value, Value> {
        let keys = self.for_in_keys(v)?;
        frame.enumerators.push((keys, 0));
        Ok(Value::Number((frame.enumerators.len() - 1) as f64))
    }

    /// `Op::ForInNext` / `ROp::ForInNext` body: `(key, has_next)`.
    pub(crate) fn for_in_next(frame: &mut Frame) -> (Value, Value) {
        let idx = frame.enumerators.len() - 1;
        let (keys, cursor) = &mut frame.enumerators[idx];
        if *cursor < keys.len() {
            let k = keys[*cursor].clone();
            *cursor += 1;
            (Value::String(k), Value::Bool(true))
        } else {
            (Value::Undefined, Value::Bool(false))
        }
    }

    fn const_name(&self, frame: &Frame, idx: u32) -> JsString {
        match &frame.func.proto.consts[idx as usize] {
            Const::String(s) => s.clone(),
            _ => JsString::new(""),
        }
    }

    /// PutValue: strict-mode assignment (`Throw=true`) routes failed writes
    /// through `set_prop_strict` (which throws a TypeError); sloppy no-ops.
    fn put_value(
        &mut self,
        base: &Value,
        key: &PropertyKey,
        value: Value,
        strict: bool,
    ) -> Result<(), Value> {
        if strict {
            self.set_prop_strict(base, key, value)
        } else {
            self.set_prop(base, key, value)
        }
    }

    // =====================================================================
    // Register-tier execution (reg.rs; docs/js-performance-roadmap.md §3.5)
    // =====================================================================

    /// Execute a frame through its register program. Same observable
    /// behavior as `run_stack_frame` over the same proto by construction:
    /// every arm calls the SAME helper as its stack twin, and the reg-on/off
    /// differential corpus (`tests/reg.rs`) pins it. Register frames carry
    /// no try-handlers (functions with handlers decline translation), so a
    /// thrown error always tears the frame down; and no suspension ops
    /// translate, so this never returns `Flow::Suspend`.
    fn run_reg_frame(&mut self, mut frame: Box<Frame>, reg: &crate::reg::RegProto) -> Flow {
        use crate::reg::{ROp, RUnary};
        macro_rules! done {
            ($flow:expr) => {{
                let outcome = $flow;
                self.recycle_frame(frame);
                return outcome;
            }};
        }
        let proto = frame.func.proto.clone();
        // Registers 0..num_locals are the localized bindings (already sized
        // by `init_frame`); the rest are the canonical operand slots.
        frame.locals.resize(reg.num_regs as usize, Value::Undefined);
        // Budget + cooperative cancellation mirror the stack loop's
        // `counting` protocol (one predicted branch when neither is
        // installed). Each register op charges its EXACT stack-op units
        // (`RegProto::costs`): the charge is hard-checked BEFORE the op —
        // the budget-exceeded throw fires exactly where the stack loop's
        // per-op check would have, and no op the stack tier would not have
        // reached can run. Pure stack ops charge at the NEXT anchor on
        // their path (see `RegProto::costs` for why that is exact).
        let counting = self.op_budget.is_some() || self.interrupt.is_some();
        let costs = &reg.costs[..];
        let mut poll: u32 = 0;
        let code = &reg.code[..];
        let num_locals = proto.num_locals as usize;
        let mut pc: usize = 0;
        loop {
            if counting {
                if let Some(budget) = self.op_budget.as_mut() {
                    let cost = u64::from(costs[pc]);
                    if *budget < cost {
                        *budget = 0;
                        // Uncatchable so execution is guaranteed to terminate.
                        done!(Flow::Throw(self.throw_range("execution budget exceeded")));
                    }
                    *budget -= cost;
                }
                if self.interrupt.is_some() {
                    poll = poll.wrapping_add(1);
                    if poll & 0xFF == 0 {
                        if let Some(flag) = &self.interrupt {
                            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                self.op_budget = Some(0);
                                done!(Flow::Throw(self.throw_range("execution interrupted")));
                            }
                        }
                    }
                }
            }
            let op = &code[pc];
            pc += 1;
            macro_rules! rd {
                ($i:expr) => {
                    frame.locals[$i as usize].clone()
                };
            }
            macro_rules! wr {
                ($i:expr, $v:expr) => {
                    frame.locals[$i as usize] = $v
                };
            }
            // Frame exit with the module-linker snapshot (mirror of the
            // stack loop's `Ctl::Return` arm).
            macro_rules! ret {
                ($v:expr) => {{
                    let v = $v;
                    if let Some(p) = &self.module_capture_proto {
                        if Rc::ptr_eq(&proto, p) {
                            self.module_capture = Some(frame.cells.clone());
                        }
                    }
                    done!(Flow::Return(v));
                }};
            }
            // Dispatch an abrupt (or return) completion through the frame's
            // try-handlers — the register-mode `do_completion`.
            macro_rules! complete {
                ($comp:expr) => {{
                    match self.reg_do_completion(&mut frame, num_locals, $comp) {
                        Ok(RegCtl::Jump(t)) => {
                            pc = t;
                            continue;
                        }
                        Ok(RegCtl::Return(v)) => ret!(v),
                        Err(e) => done!(Flow::Throw(e)),
                    }
                }};
            }
            macro_rules! tryv {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(e) => complete!(Completion::Throw(e)),
                    }
                };
            }
            // A TDZ read outside any handler is overwhelmingly common; the
            // checked form routes the ReferenceError like any other throw.
            macro_rules! tdz_throw {
                () => {
                    complete!(Completion::Throw(
                        self.throw_reference("Cannot access binding before initialization")
                    ))
                };
            }
            match op {
                // Pure budget landing: its units are charged by the loop
                // head via the cost table; the op itself runs nothing.
                ROp::Charge => {}
                ROp::Mov { dst, src } => wr!(*dst, rd!(*src)),
                ROp::Const { dst, idx } => wr!(*dst, self.const_val(&frame, *idx)),
                ROp::Undef { dst } => wr!(*dst, Value::Undefined),
                ROp::Null { dst } => wr!(*dst, Value::Null),
                ROp::Bool { dst, v } => wr!(*dst, Value::Bool(*v)),
                ROp::Hole { dst } => wr!(*dst, Value::Hole),
                ROp::This { dst } => wr!(*dst, frame.this.clone()),
                ROp::NewTarget { dst } => wr!(*dst, frame.new_target.clone()),
                ROp::Arg { dst, idx } => wr!(
                    *dst,
                    frame
                        .args
                        .get(*idx as usize)
                        .cloned()
                        .unwrap_or(Value::Undefined)
                ),
                ROp::TdzCheck { src } => {
                    if matches!(frame.locals[*src as usize], Value::Uninitialized) {
                        tdz_throw!();
                    }
                }
                ROp::StoreLocalChecked { local, src } => {
                    let v = rd!(*src);
                    let slot = &mut frame.locals[*local as usize];
                    if matches!(slot, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    *slot = v;
                }
                ROp::InitLocalTdz { local } => wr!(*local, Value::Uninitialized),
                ROp::IncLocal { local, dec } => {
                    let v = rd!(*local);
                    if matches!(v, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    // ToNumeric may run user code, but a localized binding is
                    // reachable only from this frame, so read-coerce-write is
                    // exactly the unfused sequence.
                    let n = tryv!(self.to_numeric(&v));
                    let r = tryv!(
                        self.unary_arith(n, if *dec { UnaryKind::Dec } else { UnaryKind::Inc })
                    );
                    wr!(*local, r);
                }
                ROp::LoadCell { dst, cell } => {
                    let v = frame.cells[*cell as usize].borrow().clone();
                    if matches!(v, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    wr!(*dst, v);
                }
                ROp::StoreCell { cell, src } => {
                    let v = rd!(*src);
                    *frame.cells[*cell as usize].borrow_mut() = v;
                }
                ROp::LoadUpvalue { dst, idx } => {
                    let v = frame.func.upvalues[*idx as usize].borrow().clone();
                    if matches!(v, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    wr!(*dst, v);
                }
                ROp::LoadGlobal { dst, name, ic } => {
                    let name = Self::const_name_proto(&proto, *name);
                    let v = tryv!(self.ic_load_global(name, reg.ic.get(*ic as usize)));
                    wr!(*dst, v);
                }
                ROp::Add { dst, a, b } => {
                    let r = tryv!(self.op_add(rd!(*a), rd!(*b)));
                    wr!(*dst, r);
                }
                ROp::AddK { dst, a, konst } => {
                    let b = self.const_val(&frame, *konst);
                    let r = tryv!(self.op_add(rd!(*a), b));
                    wr!(*dst, r);
                }
                ROp::Arith { kind, dst, a, b } => {
                    let r = tryv!(self.arith(rd!(*a), rd!(*b), *kind));
                    wr!(*dst, r);
                }
                ROp::ArithK {
                    kind,
                    dst,
                    a,
                    konst,
                } => {
                    let b = self.const_val(&frame, *konst);
                    let r = tryv!(self.arith(rd!(*a), b, *kind));
                    wr!(*dst, r);
                }
                ROp::AddCellK { dst, cell, konst } => {
                    let a = frame.cells[*cell as usize].borrow().clone();
                    if matches!(a, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    let b = self.const_val(&frame, *konst);
                    let r = tryv!(self.op_add(a, b));
                    wr!(*dst, r);
                }
                ROp::ArithCellK {
                    kind,
                    dst,
                    cell,
                    konst,
                } => {
                    let a = frame.cells[*cell as usize].borrow().clone();
                    if matches!(a, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    let b = self.const_val(&frame, *konst);
                    let r = tryv!(self.arith(a, b, *kind));
                    wr!(*dst, r);
                }
                ROp::Unary { kind, dst, src } => {
                    let a = rd!(*src);
                    let r = match kind {
                        RUnary::Neg => tryv!(self.unary_arith(a, UnaryKind::Neg)),
                        // ToNumber throws TypeError for BigInt (unary + is
                        // invalid on it).
                        RUnary::Pos => Value::Number(tryv!(self.to_number(&a))),
                        RUnary::ToNumeric => tryv!(self.to_numeric(&a)),
                        RUnary::Inc => tryv!(self.unary_arith(a, UnaryKind::Inc)),
                        RUnary::Dec => tryv!(self.unary_arith(a, UnaryKind::Dec)),
                        RUnary::BitNot => tryv!(self.unary_arith(a, UnaryKind::BitNot)),
                        RUnary::Not => Value::Bool(!self.to_boolean(&a)),
                        RUnary::Typeof => Value::String(crate::names::typeof_result(a.type_of())),
                        RUnary::ToStr => Value::String(tryv!(self.to_js_string(&a))),
                        RUnary::ToKey => match tryv!(self.to_property_key(&a)) {
                            PropertyKey::Str(s) => Value::String(s),
                            PropertyKey::Sym(s) => Value::Symbol(s),
                        },
                    };
                    wr!(*dst, r);
                }
                // Compare/branch operands are borrowed from the register
                // file, not cloned out of it (`rd!` is an `Rc` round-trip
                // per operand per iteration on string/object values).
                // `frame` is a local distinct from `self`, and nested user
                // code (a `valueOf` inside `cmp_values`) runs in its own
                // frames — it can reach this frame's CELLS, never its
                // locals — so the borrows are sound.
                ROp::Cmp { cmp, dst, a, b } => {
                    let r = tryv!(self.cmp_values(
                        *cmp,
                        &frame.locals[*a as usize],
                        &frame.locals[*b as usize]
                    ));
                    wr!(*dst, Value::Bool(r));
                }
                // ---- control flow ----
                ROp::Jmp { target } => pc = *target as usize,
                ROp::BrTrue { src, target } => {
                    if self.to_boolean(&frame.locals[*src as usize]) {
                        pc = *target as usize;
                    }
                }
                ROp::BrFalse { src, target } => {
                    if !self.to_boolean(&frame.locals[*src as usize]) {
                        pc = *target as usize;
                    }
                }
                ROp::BrFalsyKeep { src, target } => {
                    if !self.to_boolean(&frame.locals[*src as usize]) {
                        pc = *target as usize;
                    }
                }
                ROp::BrTruthyKeep { src, target } => {
                    if self.to_boolean(&frame.locals[*src as usize]) {
                        pc = *target as usize;
                    }
                }
                ROp::BrNotNullishKeep { src, target } => {
                    if !frame.locals[*src as usize].is_nullish() {
                        pc = *target as usize;
                    }
                }
                ROp::BrNullishUndef { reg, target } => {
                    if frame.locals[*reg as usize].is_nullish() {
                        // An optional chain short-circuits to `undefined` even
                        // when the base was `null`.
                        frame.locals[*reg as usize] = Value::Undefined;
                        pc = *target as usize;
                    }
                }
                ROp::CmpBr {
                    cmp,
                    a,
                    b,
                    if_true,
                    target,
                } => {
                    let r = tryv!(self.cmp_values(
                        *cmp,
                        &frame.locals[*a as usize],
                        &frame.locals[*b as usize]
                    ));
                    if r == *if_true {
                        pc = *target as usize;
                    }
                }
                ROp::CmpBrK {
                    cmp,
                    a,
                    konst,
                    if_true,
                    target,
                } => {
                    let b = self.const_val(&frame, *konst);
                    let r = tryv!(self.cmp_values(*cmp, &frame.locals[*a as usize], &b));
                    if r == *if_true {
                        pc = *target as usize;
                    }
                }
                ROp::TypeofBr {
                    src,
                    tag,
                    br_on_eq,
                    target,
                } => {
                    // Total: `typeof` never throws, and the tag is one of
                    // the eight statics `Value::type_of` returns, so this
                    // is content equality of the unmaterialized compare.
                    if (frame.locals[*src as usize].type_of() == *tag) == *br_on_eq {
                        pc = *target as usize;
                    }
                }
                ROp::CmpBrCellK {
                    cmp,
                    cell,
                    konst,
                    if_true,
                    target,
                } => {
                    let a = frame.cells[*cell as usize].borrow().clone();
                    if matches!(a, Value::Uninitialized) {
                        tdz_throw!();
                    }
                    let b = self.const_val(&frame, *konst);
                    if tryv!(self.cmp_values(*cmp, &a, &b)) == *if_true {
                        pc = *target as usize;
                    }
                }
                // ---- property access ----
                ROp::GetProp { dst, obj, name, ic } => {
                    let obj = rd!(*obj);
                    let name = Self::const_name_proto(&proto, *name);
                    let v = tryv!(self.ic_get_prop(obj, name, reg.ic.get(*ic as usize)));
                    wr!(*dst, v);
                }
                ROp::SetProp {
                    dst,
                    obj,
                    name,
                    src,
                    ic,
                } => {
                    let obj = rd!(*obj);
                    let value = rd!(*src);
                    let name = Self::const_name_proto(&proto, *name);
                    let v = tryv!(self.ic_set_prop(
                        obj,
                        name,
                        value,
                        proto.is_strict,
                        reg.ic.get(*ic as usize)
                    ));
                    wr!(*dst, v);
                }
                ROp::GetElem { dst, obj, key } => {
                    let (obj, key) = (rd!(*obj), rd!(*key));
                    let v = tryv!(self.elem_get(obj, key));
                    wr!(*dst, v);
                }
                ROp::SetElem { dst, obj, key, src } => {
                    let (obj, key, value) = (rd!(*obj), rd!(*key), rd!(*src));
                    let v = tryv!(self.elem_set(obj, key, value, proto.is_strict));
                    wr!(*dst, v);
                }
                // ---- calls ----
                ROp::Call {
                    dst,
                    func,
                    this,
                    at,
                    argc,
                    has_this,
                } => {
                    let func_v = rd!(*func);
                    let this_v = if *has_this {
                        rd!(*this)
                    } else {
                        Value::Undefined
                    };
                    let r = if let Some(bf) = peek_plain_bytecode(&func_v) {
                        self.call_direct_reg(&mut frame, *at, *argc, bf, func_v, this_v)
                    } else {
                        // Arguments MOVE out of their (dead) canonical slots
                        // into the pooled buffer — same ownership shape as the
                        // stack path's drain.
                        let mut args = self.take_value_vec();
                        for i in *at..*at + *argc {
                            args.push(std::mem::replace(
                                &mut frame.locals[i as usize],
                                Value::Undefined,
                            ));
                        }
                        self.call_valuevec(func_v, this_v, args)
                    };
                    wr!(*dst, tryv!(r));
                }
                ROp::New {
                    dst,
                    ctor,
                    at,
                    argc,
                } => {
                    let ctor_v = rd!(*ctor);
                    // A ZERO-arg window's `at` may sit one past the register
                    // file (nothing was ever materialized there, so the
                    // emitter never grew it): slicing would panic on the
                    // out-of-range START even though the range is empty.
                    let args = if *argc == 0 {
                        &[]
                    } else {
                        &frame.locals[*at as usize..(*at + *argc) as usize]
                    };
                    let v = tryv!(self.construct(&ctor_v, args, &ctor_v));
                    wr!(*dst, v);
                }
                ROp::Ret { src } => {
                    let v = std::mem::replace(&mut frame.locals[*src as usize], Value::Undefined);
                    // Route through any enclosing `finally` blocks (mirror of
                    // `Op::Return`); the no-handler fast path returns directly.
                    if frame.handlers.is_empty() {
                        ret!(v);
                    }
                    complete!(Completion::Return(v));
                }
                ROp::RetCompletion => {
                    let v = frame.completion.clone();
                    if frame.handlers.is_empty() {
                        ret!(v);
                    }
                    complete!(Completion::Return(v));
                }
                ROp::Throw { src } => {
                    let v = std::mem::replace(&mut frame.locals[*src as usize], Value::Undefined);
                    complete!(Completion::Throw(v));
                }
                // ---- exceptions / completions ----
                ROp::PushTryHandler {
                    catch,
                    finally,
                    depth,
                } => {
                    frame.handlers.push(TryHandler {
                        catch_ip: if *catch == u32::MAX {
                            None
                        } else {
                            Some(*catch)
                        },
                        finally_ip: if *finally == u32::MAX {
                            None
                        } else {
                            Some(*finally)
                        },
                        // Register mode: the CANONICAL operand depth (the
                        // register file holds `num_locals + depth` live slots).
                        stack_depth: *depth as usize,
                        with_depth: frame.with_scope.len(),
                        priv_env: frame.priv_env.clone(),
                        delegation: false,
                        delegation_return_ip: None,
                    });
                }
                ROp::PopTryHandler => {
                    frame.handlers.pop();
                }
                ROp::CompletionJump { target, boundary } => {
                    complete!(Completion::Jump {
                        target: *target,
                        boundary: *boundary,
                    });
                }
                ROp::EndFinally => {
                    // Resume a parked non-local completion; the normal path
                    // (finalizer ran with nothing parked) falls through.
                    if let Some(c) = frame.pending_completion.take() {
                        complete!(c);
                    }
                }
                ROp::Closure { dst, idx } => {
                    let f = tryv!(self.closure_from_const(&frame, *idx));
                    wr!(*dst, f);
                }
                // Everything rarer runs through the cold arm, keeping this
                // loop's native stack frame small (same split — and same
                // reason — as `step`/`step_cold`).
                cold => {
                    tryv!(self.rstep_cold(&mut frame, reg, &proto, cold));
                }
            }
        }
    }

    /// Register-mode mirror of [`Vm::do_completion`]: walk the frame's
    /// try-handlers with a pending completion. "Truncate the operand stack"
    /// becomes "clear the canonical registers above the handler's recorded
    /// depth" (value-lifetime parity with the stack machine's truncate), and
    /// the exception lands in `canon(depth)` — exactly where the catch
    /// label's register state expects it. The delegation arms cannot occur:
    /// generators never carry register programs.
    #[inline(never)]
    fn reg_do_completion(
        &mut self,
        frame: &mut Frame,
        num_locals: usize,
        comp: Completion,
    ) -> Result<RegCtl, Value> {
        let boundary = match &comp {
            Completion::Jump { boundary, .. } => *boundary as usize,
            _ => 0,
        };
        debug_assert!(!frame.skip_delegation_throw);
        while frame.handlers.len() > boundary {
            let h = frame.handlers.pop().unwrap();
            debug_assert!(!h.delegation && h.delegation_return_ip.is_none());
            let base = num_locals + h.stack_depth;
            for slot in frame.locals[base..].iter_mut() {
                *slot = Value::Undefined;
            }
            // Discard any `with` environments entered after this handler and
            // restore the private-environment chain (both constant in
            // register frames today — the ops that change them decline
            // translation — but kept for exactness).
            frame.with_scope.truncate(h.with_depth);
            frame.priv_env = h.priv_env.clone();
            if let Completion::Throw(err) = &comp {
                if let Some(catch_ip) = h.catch_ip {
                    frame.locals[base] = err.clone();
                    return Ok(RegCtl::Jump(catch_ip as usize));
                }
            }
            if let Some(finally_ip) = h.finally_ip {
                frame.pending_completion = Some(comp);
                return Ok(RegCtl::Jump(finally_ip as usize));
            }
        }
        match comp {
            Completion::Return(v) => Ok(RegCtl::Return(v)),
            Completion::Throw(e) => Err(e),
            Completion::Jump { target, .. } => Ok(RegCtl::Jump(target as usize)),
        }
    }

    /// The register tier's rare ops. `#[inline(never)]` keeps their unioned
    /// locals out of `run_reg_frame`'s native frame (deep JS recursion must
    /// exhaust `max_call_depth` before the native stack; see `step_cold`).
    /// Cold ops never branch, return, or call JS→JS directly, so the
    /// signature carries no control flow.
    #[inline(never)]
    fn rstep_cold(
        &mut self,
        frame: &mut Frame,
        reg: &crate::reg::RegProto,
        proto: &Rc<crate::bytecode::FuncProto>,
        op: &crate::reg::ROp,
    ) -> Result<(), Value> {
        use crate::reg::ROp;
        macro_rules! rd {
            ($i:expr) => {
                frame.locals[$i as usize].clone()
            };
        }
        macro_rules! wr {
            ($i:expr, $v:expr) => {
                frame.locals[$i as usize] = $v
            };
        }
        match op {
            ROp::RestArgs { dst, from } => {
                let v = self.rest_args(frame, *from);
                wr!(*dst, v);
            }
            ROp::Arguments { dst } => {
                let v = self.make_arguments_object(frame);
                wr!(*dst, v);
            }
            ROp::BindThisSloppy { reg: r } => {
                let t = std::mem::replace(&mut frame.locals[*r as usize], Value::Undefined);
                let bound = self.bind_this_sloppy(t)?;
                wr!(*r, bound);
            }
            ROp::StoreCellChecked { cell, src } => {
                let v = rd!(*src);
                let mut slot = frame.cells[*cell as usize].borrow_mut();
                if matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                *slot = v;
            }
            ROp::InitCell { cell, src } => {
                let v = rd!(*src);
                // A module's top-level cells are STABLE: mutate in place so a
                // pre-wired import binding keeps pointing at the live cell.
                if proto.stable_flags[*cell as usize] {
                    *frame.cells[*cell as usize].borrow_mut() = v;
                } else {
                    let fresh = self.take_cell(v);
                    let old = std::mem::replace(&mut frame.cells[*cell as usize], fresh);
                    self.recycle_cell(old);
                }
            }
            ROp::InitCellTdz { cell } => {
                if proto.stable_flags[*cell as usize] {
                    *frame.cells[*cell as usize].borrow_mut() = Value::Uninitialized;
                } else {
                    let fresh = self.take_cell(Value::Uninitialized);
                    let old = std::mem::replace(&mut frame.cells[*cell as usize], fresh);
                    self.recycle_cell(old);
                }
            }
            ROp::IncCell { cell, dec } => {
                let v = frame.cells[*cell as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let n = self.to_numeric(&v)?;
                let r = self.unary_arith(n, if *dec { UnaryKind::Dec } else { UnaryKind::Inc })?;
                *frame.cells[*cell as usize].borrow_mut() = r;
            }
            ROp::LoadCellInit { src, dest } => {
                let v = frame.cells[*src as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                if proto.stable_flags[*dest as usize] {
                    *frame.cells[*dest as usize].borrow_mut() = v;
                } else {
                    let fresh = self.take_cell(v);
                    let old = std::mem::replace(&mut frame.cells[*dest as usize], fresh);
                    self.recycle_cell(old);
                }
            }
            ROp::StoreUpvalue { idx, src } => {
                let v = rd!(*src);
                *frame.func.upvalues[*idx as usize].borrow_mut() = v;
            }
            ROp::StoreUpvalueChecked { idx, src } => {
                let v = rd!(*src);
                let mut slot = frame.func.upvalues[*idx as usize].borrow_mut();
                if matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                *slot = v;
            }
            ROp::StoreGlobal { name, src } => {
                let v = rd!(*src);
                let name = Self::const_name_proto(proto, *name);
                self.store_global(name, v, proto.is_strict)?;
            }
            ROp::LoadGlobalTypeof { dst, name } => {
                let name = Self::const_name_proto(proto, *name);
                let g = self.realm.global.clone();
                let v = self.get_prop(&Value::Object(g), &PropertyKey::Str(name))?;
                wr!(*dst, v);
            }
            ROp::DeclareGlobal { name, deletable } => {
                let name = Self::const_name_proto(proto, *name);
                self.declare_global(name, *deletable)?;
            }
            ROp::CanDeclareGlobalFunc { name } => {
                let name = Self::const_name_proto(proto, *name);
                self.can_declare_global_func(name)?;
            }
            ROp::DefineGlobalFunc {
                name,
                deletable,
                src,
            } => {
                // Clone, not move: `src` may be a live register (the
                // translator does not canonicalize this operand).
                let v = rd!(*src);
                let name = Self::const_name_proto(proto, *name);
                self.define_global_func(name, v, *deletable);
            }
            ROp::InstanceOf { dst, a, b } => {
                let (a, b) = (rd!(*a), rd!(*b));
                let r = self.instance_of(&a, &b)?;
                wr!(*dst, Value::Bool(r));
            }
            ROp::DelProp { dst, obj, name } => {
                let obj = rd!(*obj);
                let name = Self::const_name_proto(proto, *name);
                let v = self.del_prop_named(obj, name, proto.is_strict)?;
                wr!(*dst, v);
            }
            ROp::DelElem { dst, obj, key } => {
                let (obj, key) = (rd!(*obj), rd!(*key));
                let v = self.del_prop_dynamic(obj, key, proto.is_strict)?;
                wr!(*dst, v);
            }
            ROp::HasProp { dst, key, obj } => {
                let (key_v, obj) = (rd!(*key), rd!(*obj));
                let key = self.to_property_key(&key_v)?;
                let r = self.has_prop(&obj, &key)?;
                wr!(*dst, Value::Bool(r));
            }
            ROp::CallSpread {
                dst,
                func,
                this,
                args,
            } => {
                let (func, this, args_arr) = (rd!(*func), rd!(*this), rd!(*args));
                let args = self.iterate_to_vec(&args_arr)?;
                let r = self.call(func, this, &args)?;
                wr!(*dst, r);
            }
            ROp::NewSpread { dst, ctor, args } => {
                let (ctor, args_arr) = (rd!(*ctor), rd!(*args));
                let args = self.iterate_to_vec(&args_arr)?;
                let r = self.construct(&ctor, &args, &ctor)?;
                wr!(*dst, r);
            }
            ROp::ThrowConstAssign => {
                return Err(self.throw_type("Assignment to constant variable."));
            }
            ROp::NewObject { dst } => wr!(*dst, Value::Object(self.new_object())),
            ROp::NewArray { dst, at, n } => {
                let mut elems = Vec::with_capacity(*n as usize);
                for i in *at..*at + *n {
                    elems.push(std::mem::replace(
                        &mut frame.locals[i as usize],
                        Value::Undefined,
                    ));
                }
                wr!(*dst, Value::Object(self.new_array(elems)));
            }
            ROp::NewObjectTpl { dst, at, n, tpl } => {
                let tpl = frame.func.proto.obj_tpls[*tpl as usize].clone();
                let obj = self.new_object_from_tpl(
                    &tpl,
                    frame.locals[*at as usize..(*at + *n) as usize]
                        .iter_mut()
                        .map(|slot| std::mem::replace(slot, Value::Undefined)),
                );
                wr!(*dst, Value::Object(obj));
            }
            ROp::ArraySpread { arr, src } => {
                let (arr_v, src) = (rd!(*arr), rd!(*src));
                self.array_spread(&arr_v, &src)?;
            }
            ROp::DefineProp {
                kind,
                obj,
                key,
                val,
            } => {
                let (obj, key_v, value) = (rd!(*obj), rd!(*key), rd!(*val));
                self.define_prop_kind(*kind, &obj, &key_v, value)?;
            }
            ROp::SetHomeObject { obj, val } => {
                let (home, m) = (rd!(*obj), rd!(*val));
                Self::set_home_object_op(&home, &m);
            }
            ROp::ObjectSpread { target, src } => {
                let (target, src) = (rd!(*target), rd!(*src));
                self.object_spread(&target, &src)?;
            }
            ROp::CopyDataPropsExcept { target, src, at, n } => {
                let mut excluded: Vec<PropertyKey> = Vec::with_capacity(*n as usize);
                for i in *at..*at + *n {
                    let k = std::mem::replace(&mut frame.locals[i as usize], Value::Undefined);
                    excluded.push(self.to_property_key(&k)?);
                }
                let (target, src) = (rd!(*target), rd!(*src));
                self.copy_data_props_except(&target, &src, &excluded)?;
            }
            ROp::GetTemplateObject { dst, idx } => {
                let v = self.template_object(frame, *idx)?;
                wr!(*dst, v);
            }
            ROp::NewRegExp {
                dst,
                pattern,
                flags,
            } => {
                let p = Self::const_name_proto(proto, *pattern);
                let f = Self::const_name_proto(proto, *flags);
                let re = self.make_regexp(p.as_str(), f.as_str())?;
                wr!(*dst, re);
            }
            ROp::ConcatStrings { dst, at, n } => {
                let parts = &frame.locals[*at as usize..(*at + *n) as usize];
                let v = self.concat_strings(parts)?;
                wr!(*dst, v);
            }
            ROp::RequireObjectCoercible { src } => {
                if frame.locals[*src as usize].is_nullish() {
                    return Err(self.throw_type("Cannot destructure a null or undefined value"));
                }
            }
            ROp::RequireCoercible { src } => {
                let v = rd!(*src);
                self.require_object_coercible(&v, "read properties of")?;
            }
            ROp::RequireIterResult { src } => {
                if !matches!(frame.locals[*src as usize], Value::Object(_)) {
                    return Err(self.throw_type("Iterator result is not an object"));
                }
            }
            ROp::SetFunctionNameFromKey { prefix, key, val } => {
                let (key, value) = (rd!(*key), rd!(*val));
                let prefix = Self::const_name_proto(proto, *prefix);
                self.set_function_name_from_key(&key, &value, prefix);
            }
            ROp::SetProtoFromLiteral { obj, src } => {
                let (obj, v) = (rd!(*obj), rd!(*src));
                Self::set_proto_from_literal(&obj, v);
            }
            ROp::GetIterator { dst, src } => {
                let v = rd!(*src);
                let it = self.get_iterator(&v)?;
                wr!(*dst, it);
            }
            ROp::IterNext { dst, it } => {
                let it = rd!(*it);
                let next = self.get_prop(&it, &crate::names::key_next())?;
                let res = self.call(next, it, &[])?;
                wr!(*dst, res);
            }
            ROp::IterStepValue { dst, next, it } => {
                let (next, it) = (rd!(*next), rd!(*it));
                let (value, done) = self.iterator_step_value(next, it)?;
                wr!(*dst, value);
                wr!(*dst + 1, Value::Bool(done));
            }
            ROp::ForInEnumerate { dst, src } => {
                let v = rd!(*src);
                let idx = self.for_in_enumerate(frame, &v)?;
                wr!(*dst, idx);
            }
            ROp::ForInNext { dst } => {
                let (k, more) = Self::for_in_next(frame);
                wr!(*dst, k);
                wr!(*dst + 1, more);
            }
            ROp::IteratorClose { it } => {
                // Reached only on an abrupt completion (for-of / destructuring
                // landing pads), with that completion parked. Per spec
                // (IteratorClose), if the completion is a throw, any error
                // from `return()` is suppressed (the original throw wins);
                // otherwise a `return()` error propagates and a non-object
                // result is a TypeError.
                let it = rd!(*it);
                let completion_is_throw =
                    matches!(frame.pending_completion, Some(Completion::Throw(_)));
                let ret = match self.get_prop(&it, &crate::names::key_return()) {
                    Ok(r) => r,
                    Err(e) => {
                        if completion_is_throw {
                            Value::Undefined
                        } else {
                            return Err(e);
                        }
                    }
                };
                if self.is_callable(&ret) {
                    match self.call(ret, it.clone(), &[]) {
                        Ok(v) => {
                            if !completion_is_throw && !matches!(v, Value::Object(_)) {
                                return Err(
                                    self.throw_type("iterator return() result is not an object")
                                );
                            }
                        }
                        Err(e) => {
                            if !completion_is_throw {
                                return Err(e);
                            }
                        }
                    }
                } else if !ret.is_nullish() && !completion_is_throw {
                    // GetMethod: a present but non-callable `return` is a
                    // TypeError (masked only by an in-flight throw).
                    return Err(self.throw_type("iterator return is not a function"));
                }
            }
            ROp::ForInPop => {
                if let Some((keys, _)) = frame.enumerators.pop() {
                    self.park_forin_vec(keys);
                }
            }
            _ => unreachable!("op handled inline in run_reg_frame: {op:?}"),
        }
        let _ = reg;
        Ok(())
    }

    /// The register tier's JS→JS fast call (`ROp::Call` with a plain sync
    /// bytecode callee): mirror of [`Vm::call_direct`] with arguments moving
    /// out of the caller's registers instead of its operand stack.
    fn call_direct_reg(
        &mut self,
        caller: &mut Frame,
        at: u16,
        argc: u16,
        bf: Rc<BytecodeFunction>,
        func_v: Value,
        this: Value,
    ) -> Result<Value, Value> {
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(self.throw_range("Maximum call stack size exceeded"));
        }
        // Function kernel: run frameless straight off the caller's registers
        // (the arguments sit contiguously at `at..at+argc`). A ZERO-arg
        // window's `at` may sit one past the register file — nothing was
        // ever materialized there — so slicing it would panic on the
        // out-of-range start even though the range is empty.
        if bf.proto.fn_kernel.is_some() {
            let args: &[Value] = if argc == 0 {
                &[]
            } else {
                &caller.locals[at as usize..(at + argc) as usize]
            };
            if let Some(r) = self.run_fn_kernel(&bf, args) {
                self.call_depth -= 1;
                return r;
            }
        }
        let mut callee = self.take_frame();
        for i in at..at + argc {
            callee.args.push(std::mem::replace(
                &mut caller.locals[i as usize],
                Value::Undefined,
            ));
        }
        let uses_arguments = bf.proto.uses_arguments;
        self.init_frame(&mut callee, bf, this, Value::Undefined);
        if uses_arguments {
            if let Value::Object(o) = func_v {
                callee.func_obj = Some(o);
            }
        }
        let token = self.trace_enter(&callee.func.proto);
        callee.trace_token = token;
        let r = match self.run_frame(callee) {
            Flow::Return(v) => {
                self.trace_exit(token, false);
                Ok(v)
            }
            Flow::Throw(e) => {
                self.trace_exit(token, true);
                Err(e)
            }
            Flow::Suspend(_) => Err(self.throw_type("internal: sync function suspended")),
        };
        self.call_depth -= 1;
        r
    }

    /// `const_name` against a proto (the register tier has no `&Frame`
    /// borrow to spare at its call sites).
    #[inline]
    fn const_name_proto(proto: &crate::bytecode::FuncProto, idx: u32) -> JsString {
        match &proto.consts[idx as usize] {
            Const::String(s) => s.clone(),
            _ => JsString::new(""),
        }
    }

    /// One opcode. Split in two for the sake of the NATIVE stack: the hot
    /// ops (loads/stores, arithmetic, comparisons, branches, calls, property
    /// access, the fused superinstructions) are handled inline here, and every
    /// other op delegates to the `#[inline(never)]` [`Vm::step_cold`]. With
    /// one giant match, LLVM's imperfect stack coloring gave `step` a ~4 KB
    /// stack frame -- the UNION of ~190 arms' locals -- which is what a JS
    /// call's native-stack footprint is mostly made of (deep JS recursion
    /// must exhaust `max_call_depth` before the native stack). Each op has
    /// exactly ONE implementation: here, or in `step_cold`, never both.
    fn step(&mut self, frame: &mut Frame, op: &Op) -> Result<Ctl, Value> {
        macro_rules! pop {
            () => {
                frame.stack.pop().unwrap_or(Value::Undefined)
            };
        }
        macro_rules! push {
            ($v:expr) => {
                frame.stack.push($v)
            };
        }
        match op {
            Op::Nop => {}
            Op::LoadConst(i) => push!(self.const_val(frame, *i)),
            Op::LoadUndefined => push!(Value::Undefined),
            Op::LoadHole => push!(Value::Hole),
            Op::LoadNull => push!(Value::Null),
            Op::LoadTrue => push!(Value::Bool(true)),
            Op::LoadFalse => push!(Value::Bool(false)),
            Op::LoadThis => push!(frame.this.clone()),
            Op::LoadNewTarget => push!(frame.new_target.clone()),
            Op::LoadArg(i) => push!(frame
                .args
                .get(*i as usize)
                .cloned()
                .unwrap_or(Value::Undefined)),
            Op::LoadLocal(i) => {
                let v = frame.locals[*i as usize].clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
            }
            Op::StoreLocal(i) => {
                let v = pop!();
                frame.locals[*i as usize] = v;
            }
            Op::StoreLocalChecked(i) => {
                let v = pop!();
                let slot = &mut frame.locals[*i as usize];
                if matches!(slot, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                *slot = v;
            }
            Op::InitLocalTdz(i) => {
                frame.locals[*i as usize] = Value::Uninitialized;
            }
            // Local mirrors of the cell superinstructions — same helpers, same
            // TDZ checks, minus the Rc/RefCell indirection.
            Op::LoadLocalConst { local, konst } => {
                let v = frame.locals[*local as usize].clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
                push!(self.const_val(frame, *konst));
            }
            Op::CmpLocalConstBranchFalse {
                local,
                konst,
                cmp,
                target,
            } => {
                let a = frame.locals[*local as usize].clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                if !self.cmp_values(*cmp, &a, &b)? {
                    return Ok(Ctl::Jump(*target as usize));
                }
            }
            Op::CmpLocalConstBranchTrue {
                local,
                konst,
                cmp,
                target,
            } => {
                let a = frame.locals[*local as usize].clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                if self.cmp_values(*cmp, &a, &b)? {
                    return Ok(Ctl::Jump(*target as usize));
                }
            }
            Op::AddLocalConst { local, konst } => {
                let a = frame.locals[*local as usize].clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                let r = self.op_add(a, b)?;
                push!(r);
            }
            Op::ArithLocalConst { local, konst, kind } => {
                let a = frame.locals[*local as usize].clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                let r = self.arith(a, b, *kind)?;
                push!(r);
            }
            Op::IncLocalStmt { local, dec } => {
                let v = frame.locals[*local as usize].clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                // ToNumeric may run user code, but a localized binding is
                // reachable only from this frame, so read-coerce-write is
                // exactly the unfused sequence.
                let n = self.to_numeric(&v)?;
                let r = self.unary_arith(n, if *dec { UnaryKind::Dec } else { UnaryKind::Inc })?;
                frame.locals[*local as usize] = r;
            }
            Op::CopyLocal { src, dest } => {
                let v = frame.locals[*src as usize].clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                frame.locals[*dest as usize] = v;
            }
            Op::LoadCell(i) => {
                let v = frame.cells[*i as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
            }
            // Fused `LoadCell(cell) ; LoadConst(konst)` (see `fuse.rs`). Identical
            // to running the two ops: the cell read keeps the same TDZ check, then
            // the constant is pushed.
            Op::LoadCellConst { cell, konst } => {
                let v = frame.cells[*cell as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
                push!(self.const_val(frame, *konst));
            }
            Op::StoreCell(i) => {
                let v = pop!();
                *frame.cells[*i as usize].borrow_mut() = v;
            }
            Op::StoreCellChecked(i) => {
                let v = pop!();
                let mut slot = frame.cells[*i as usize].borrow_mut();
                if matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                *slot = v;
            }
            Op::InitCell(i) => {
                let v = pop!();
                // A module's top-level cells are STABLE: mutate in place so a
                // pre-wired import binding (or self-reference) keeps pointing at
                // the live cell. All other bindings get a fresh `Rc` (needed for
                // per-iteration `let` semantics).
                if frame.func.proto.stable_flags[*i as usize] {
                    *frame.cells[*i as usize].borrow_mut() = v;
                } else {
                    let fresh = self.take_cell(v);
                    let old = std::mem::replace(&mut frame.cells[*i as usize], fresh);
                    self.recycle_cell(old);
                }
            }
            Op::InitCellTdz(i) => {
                // Fresh cell holding the Temporal Dead Zone marker (a hoisted
                // `let`/`const`/`class` binding before its initializer runs).
                if frame.func.proto.stable_flags[*i as usize] {
                    *frame.cells[*i as usize].borrow_mut() = Value::Uninitialized;
                } else {
                    let fresh = self.take_cell(Value::Uninitialized);
                    let old = std::mem::replace(&mut frame.cells[*i as usize], fresh);
                    self.recycle_cell(old);
                }
            }
            Op::LoadUpvalue(i) => {
                let v = frame.func.upvalues[*i as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
            }
            Op::StoreUpvalue(i) => {
                let v = pop!();
                *frame.func.upvalues[*i as usize].borrow_mut() = v;
            }
            Op::StoreUpvalueChecked(i) => {
                let v = pop!();
                let mut slot = frame.func.upvalues[*i as usize].borrow_mut();
                if matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                *slot = v;
            }
            Op::LoadGlobal(i) => {
                let name = self.const_name(frame, *i);
                let ic = frame.func.proto.ic.get(frame.ip.wrapping_sub(1));
                let v = self.ic_load_global(name, ic)?;
                push!(v);
            }
            Op::StoreGlobal(i) => {
                let name = self.const_name(frame, *i);
                let v = pop!();
                let strict = frame.func.proto.is_strict;
                self.store_global(name, v, strict)?;
            }
            Op::Pop => {
                frame.stack.pop();
            }
            Op::Dup => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                push!(v);
            }
            Op::Swap => {
                let n = frame.stack.len();
                if n >= 2 {
                    frame.stack.swap(n - 1, n - 2);
                }
            }
            Op::Rot3 => {
                let n = frame.stack.len();
                if n >= 3 {
                    // a b c -> b c a
                    frame.stack[n - 3..].rotate_left(1);
                }
            }

            Op::NewObject => push!(Value::Object(self.new_object())),
            Op::NewObjectTpl { idx, n } => {
                let tpl = frame.func.proto.obj_tpls[*idx as usize].clone();
                let at = frame.stack.len() - *n as usize;
                let obj = self.new_object_from_tpl(&tpl, frame.stack.drain(at..));
                push!(Value::Object(obj));
            }
            Op::NewArray(n) => {
                let n = *n as usize;
                let at = frame.stack.len() - n;
                let elems = frame.stack.split_off(at);
                push!(Value::Object(self.new_array(elems)));
            }
            Op::GetProp(i) => {
                let name = self.const_name(frame, *i);
                let obj = pop!();
                let ic = frame.func.proto.ic.get(frame.ip.wrapping_sub(1));
                let v = self.ic_get_prop(obj, name, ic)?;
                push!(v);
            }
            Op::SetProp(i) => {
                let name = self.const_name(frame, *i);
                let value = pop!();
                let obj = pop!();
                let strict = frame.func.proto.is_strict;
                let ic = frame.func.proto.ic.get(frame.ip.wrapping_sub(1));
                let value = self.ic_set_prop(obj, name, value, strict, ic)?;
                push!(value);
            }
            Op::GetPropDynamic => {
                let key_v = pop!();
                let obj = pop!();
                let v = self.elem_get(obj, key_v)?;
                push!(v);
            }
            Op::SetPropDynamic => {
                let value = pop!();
                let key_v = pop!();
                let obj = pop!();
                let strict = frame.func.proto.is_strict;
                let value = self.elem_set(obj, key_v, value, strict)?;
                push!(value);
            }
            Op::HasProp => {
                let obj = pop!();
                let key_v = pop!();
                let key = self.to_property_key(&key_v)?;
                let r = self.has_prop(&obj, &key)?;
                push!(Value::Bool(r));
            }
            Op::JumpIfNullish(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if v.is_nullish() {
                    // An optional chain short-circuits to `undefined` even when
                    // the base was `null` (the other emit sites pop the value).
                    if let Some(top) = frame.stack.last_mut() {
                        *top = Value::Undefined;
                    }
                    return Ok(Ctl::Jump(*t as usize));
                }
            }

            Op::Call(argc) => {
                let n = *argc as usize;
                let at = frame.stack.len() - n;
                // Stack layout: [func, this, a0..]. A plain sync bytecode
                // callee takes the direct path: its args MOVE straight from
                // this operand stack into the pooled callee frame.
                if let Some(bf) = peek_plain_bytecode(&frame.stack[at - 2]) {
                    let r = self.call_direct(frame, at, bf, true)?;
                    push!(r);
                } else {
                    // Move the argument values into a pooled buffer rather than
                    // `split_off` (which allocates a fresh Vec on every call).
                    let mut args = self.take_value_vec();
                    args.extend(frame.stack.drain(at..));
                    let this = pop!();
                    let func = pop!();
                    // Owned-args path: the pooled buffer moves into the callee
                    // frame (recycled by its recycle_frame), skipping the second
                    // copy make_frame's slice path would do.
                    let r = self.call_valuevec(func, this, args);
                    push!(r?);
                }
            }
            Op::CallMethodless(argc) => {
                let n = *argc as usize;
                let at = frame.stack.len() - n;
                // Stack layout: [func, a0..] — no explicit `this`.
                if let Some(bf) = peek_plain_bytecode(&frame.stack[at - 1]) {
                    let r = self.call_direct(frame, at, bf, false)?;
                    push!(r);
                } else {
                    let mut args = self.take_value_vec();
                    args.extend(frame.stack.drain(at..));
                    let func = pop!();
                    let r = self.call_valuevec(func, Value::Undefined, args);
                    push!(r?);
                }
            }
            Op::CallSpread => {
                let args_arr = pop!();
                let this = pop!();
                let func = pop!();
                let args = self.iterate_to_vec(&args_arr)?;
                let r = self.call(func, this, &args)?;
                push!(r);
            }
            Op::New(argc) => {
                let n = *argc as usize;
                let at = frame.stack.len() - n;
                let args = frame.stack.split_off(at);
                let ctor = pop!();
                let r = self.construct(&ctor, &args, &ctor)?;
                push!(r);
            }
            Op::NewSpread => {
                let args_arr = pop!();
                let ctor = pop!();
                let args = self.iterate_to_vec(&args_arr)?;
                let r = self.construct(&ctor, &args, &ctor)?;
                push!(r);
            }
            Op::Return => {
                let v = pop!();
                // Route through any enclosing `finally` blocks before returning.
                return self.do_completion(frame, Completion::Return(v));
            }
            Op::ReturnUndefined => {
                let v = frame.completion.clone();
                return self.do_completion(frame, Completion::Return(v));
            }

            Op::Closure(i) => {
                let f = self.closure_from_const(frame, *i)?;
                push!(f);
            }

            // ---- arithmetic ----
            Op::Add => {
                let b = pop!();
                let a = pop!();
                let r = self.op_add(a, b)?;
                push!(r);
            }
            Op::Sub => bin_arith(self, frame, ArithKind::Sub)?,
            Op::Mul => bin_arith(self, frame, ArithKind::Mul)?,
            Op::Div => bin_arith(self, frame, ArithKind::Div)?,
            Op::Mod => bin_arith(self, frame, ArithKind::Mod)?,
            Op::Pow => bin_arith(self, frame, ArithKind::Pow)?,
            Op::Neg => {
                let a = pop!();
                push!(self.unary_arith(a, UnaryKind::Neg)?);
            }
            Op::Pos => {
                // ToNumber throws TypeError for BigInt (unary + is invalid on it).
                let a = pop!();
                let n = self.to_number(&a)?;
                push!(Value::Number(n));
            }
            Op::ToNumeric => {
                // ToNumeric: like ToNumber but keeps BigInt (used by ++/-- to
                // produce the coerced old value).
                let a = pop!();
                push!(self.to_numeric(&a)?);
            }
            Op::Inc => {
                let a = pop!();
                push!(self.unary_arith(a, UnaryKind::Inc)?);
            }
            Op::Dec => {
                let a = pop!();
                push!(self.unary_arith(a, UnaryKind::Dec)?);
            }
            Op::BitNot => {
                let a = pop!();
                push!(self.unary_arith(a, UnaryKind::BitNot)?);
            }
            Op::Not => {
                let a = pop!();
                push!(Value::Bool(!self.to_boolean(&a)));
            }
            Op::BitAnd => bin_arith(self, frame, ArithKind::BitAnd)?,
            Op::BitOr => bin_arith(self, frame, ArithKind::BitOr)?,
            Op::BitXor => bin_arith(self, frame, ArithKind::BitXor)?,
            Op::Shl => bin_arith(self, frame, ArithKind::Shl)?,
            Op::Shr => bin_arith(self, frame, ArithKind::Shr)?,
            Op::UShr => bin_arith(self, frame, ArithKind::UShr)?,
            Op::TypeofExpr => {
                let a = pop!();
                push!(Value::String(crate::names::typeof_result(a.type_of())));
            }

            // ---- comparison ----
            Op::Eq => {
                let b = pop!();
                let a = pop!();
                let r = self.loose_equals(&a, &b)?;
                push!(Value::Bool(r));
            }
            Op::Ne => {
                let b = pop!();
                let a = pop!();
                let r = self.loose_equals(&a, &b)?;
                push!(Value::Bool(!r));
            }
            Op::StrictEq => {
                let b = pop!();
                let a = pop!();
                push!(Value::Bool(self.strict_equals(&a, &b)));
            }
            Op::StrictNe => {
                let b = pop!();
                let a = pop!();
                push!(Value::Bool(!self.strict_equals(&a, &b)));
            }
            Op::Lt => {
                let b = pop!();
                let a = pop!();
                let r = self.less_than(&a, &b)?;
                push!(Value::Bool(r == Some(true)));
            }
            Op::Gt => {
                let b = pop!();
                let a = pop!();
                let r = self.less_than(&b, &a)?;
                push!(Value::Bool(r == Some(true)));
            }
            Op::Le => {
                let b = pop!();
                let a = pop!();
                let r = self.less_than(&b, &a)?;
                push!(Value::Bool(r == Some(false)));
            }
            Op::Ge => {
                let b = pop!();
                let a = pop!();
                let r = self.less_than(&a, &b)?;
                push!(Value::Bool(r == Some(false)));
            }
            Op::InstanceOf => {
                let ctor = pop!();
                let obj = pop!();
                let r = self.instance_of(&obj, &ctor)?;
                push!(Value::Bool(r));
            }

            // ---- control flow ----
            Op::Jump(t) => return Ok(Ctl::Jump(*t as usize)),
            Op::JumpIfTrue(t) => {
                let v = pop!();
                if self.to_boolean(&v) {
                    return Ok(Ctl::Jump(*t as usize));
                }
            }
            Op::JumpIfFalse(t) => {
                let v = pop!();
                if !self.to_boolean(&v) {
                    return Ok(Ctl::Jump(*t as usize));
                }
            }
            // Fused `cmp ; JumpIfFalse` (see `fuse.rs`). Each arm reuses the
            // SAME helper as the standalone comparison op, so coercion and any
            // thrown error are identical; the boolean the pair would materialize
            // is consumed directly by the branch instead of round-tripping the
            // operand stack. `to_boolean(Bool(r)) == r`, so the branch condition
            // matches `JumpIfFalse` exactly.
            Op::CmpBranchFalse { cmp, target } => {
                let b = pop!();
                let a = pop!();
                if !self.cmp_values(*cmp, &a, &b)? {
                    return Ok(Ctl::Jump(*target as usize));
                }
            }
            // Fused `cmp ; JumpIfTrue` — mirror of `CmpBranchFalse`; branches when
            // the comparison is true. Same helpers, so behavior is identical.
            Op::CmpBranchTrue { cmp, target } => {
                let b = pop!();
                let a = pop!();
                let r = self.cmp_values(*cmp, &a, &b)?;
                if r {
                    return Ok(Ctl::Jump(*target as usize));
                }
            }
            // Fused `LoadCellConst ; CmpBranchFalse` — a whole `i < N` loop test.
            // Same TDZ check, comparison helpers, and branch as the sequence.
            Op::CmpCellConstBranchFalse {
                cell,
                konst,
                cmp,
                target,
            } => {
                let a = frame.cells[*cell as usize].borrow().clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                if !self.cmp_values(*cmp, &a, &b)? {
                    return Ok(Ctl::Jump(*target as usize));
                }
            }
            Op::CmpCellConstBranchTrue {
                cell,
                konst,
                cmp,
                target,
            } => {
                let a = frame.cells[*cell as usize].borrow().clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                if self.cmp_values(*cmp, &a, &b)? {
                    return Ok(Ctl::Jump(*target as usize));
                }
            }
            // Fused `LoadCellConst ; Add` / `LoadCellConst ; <binop>` — the same
            // op_add / arith helpers compute the result; only the intermediate
            // stack traffic is elided.
            Op::AddCellConst { cell, konst } => {
                let a = frame.cells[*cell as usize].borrow().clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                let r = self.op_add(a, b)?;
                push!(r);
            }
            Op::ArithCellConst { cell, konst, kind } => {
                let a = frame.cells[*cell as usize].borrow().clone();
                if matches!(a, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let b = self.const_val(frame, *konst);
                let r = self.arith(a, b, *kind)?;
                push!(r);
            }
            // Fused statement-position `i++`/`--i`-style update on a cell. The
            // exact sequence semantics: TDZ-checked read, ToNumeric (which may
            // run user code that reassigns the binding — the borrow is released
            // before it runs), the same unary_arith step, plain in-place store.
            Op::IncCellStmt { cell, dec } => {
                let v = frame.cells[*cell as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                let n = self.to_numeric(&v)?;
                let r = self.unary_arith(n, if *dec { UnaryKind::Dec } else { UnaryKind::Inc })?;
                *frame.cells[*cell as usize].borrow_mut() = r;
            }
            // Fused `LoadCell(src) ; InitCell(dest)` — per-iteration `let` copy.
            Op::LoadCellInit { src, dest } => {
                let v = frame.cells[*src as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                if frame.func.proto.stable_flags[*dest as usize] {
                    *frame.cells[*dest as usize].borrow_mut() = v;
                } else {
                    let fresh = self.take_cell(v);
                    let old = std::mem::replace(&mut frame.cells[*dest as usize], fresh);
                    self.recycle_cell(old);
                }
            }
            Op::JumpIfFalsyPeek(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if !self.to_boolean(&v) {
                    return Ok(Ctl::Jump(*t as usize));
                }
                frame.stack.pop();
            }
            Op::JumpIfTruthyPeek(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if self.to_boolean(&v) {
                    return Ok(Ctl::Jump(*t as usize));
                }
                frame.stack.pop();
            }
            Op::JumpIfNullishPeek(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if !v.is_nullish() {
                    return Ok(Ctl::Jump(*t as usize));
                }
                frame.stack.pop();
            }

            // ---- exceptions ----
            Op::Throw => {
                let v = pop!();
                return Err(v);
            }
            Op::PushTryHandler { catch, finally } => {
                frame.handlers.push(TryHandler {
                    catch_ip: if *catch == u32::MAX {
                        None
                    } else {
                        Some(*catch)
                    },
                    finally_ip: if *finally == u32::MAX {
                        None
                    } else {
                        Some(*finally)
                    },
                    stack_depth: frame.stack.len(),
                    with_depth: frame.with_scope.len(),
                    priv_env: frame.priv_env.clone(),
                    delegation: false,
                    delegation_return_ip: None,
                });
            }
            Op::PopTryHandler => {
                frame.handlers.pop();
            }
            Op::GetIterator => {
                let v = pop!();
                let it = self.get_iterator(&v)?;
                push!(it);
            }
            Op::IteratorNext => {
                let it = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                let next = self.get_prop(&it, &crate::names::key_next())?;
                let res = self.call(next, it, &[])?;
                push!(res);
            }
            Op::IteratorStepValue => {
                let it = pop!();
                let next = pop!();
                let (value, done) = self.iterator_step_value(next, it)?;
                push!(value);
                push!(Value::Bool(done));
            }
            Op::ForInPop => {
                if let Some((keys, _)) = frame.enumerators.pop() {
                    self.park_forin_vec(keys);
                }
            }
            Op::ForInNext => {
                let (k, more) = Self::for_in_next(frame);
                push!(k);
                push!(more);
            }

            // ---- generators / async ----
            Op::Await => {
                let v = pop!();
                return Ok(Ctl::Await(v));
            }
            Op::Yield => {
                let v = pop!();
                return Ok(Ctl::Yield(v));
            }
            Op::GeneratorStart => return Ok(Ctl::GeneratorStart),
            Op::YieldStar => {
                let v = pop!();
                return Ok(Ctl::YieldStar(v));
            }
            Op::ToPropertyKey => {
                let v = pop!();
                let k = self.to_property_key(&v)?;
                push!(match k {
                    PropertyKey::Str(s) => Value::String(s),
                    PropertyKey::Sym(s) => Value::Symbol(s),
                });
            }
            Op::ToStringOp => {
                let v = pop!();
                let s = self.to_js_string(&v)?;
                push!(Value::String(s));
            }
            Op::ConcatStrings(n) => {
                let n = *n as usize;
                let at = frame.stack.len() - n;
                let parts = frame.stack.split_off(at);
                let mut strs = Vec::with_capacity(parts.len());
                let mut total = 0usize;
                for p in &parts {
                    let s = self.to_js_string(p)?;
                    // Same bound as `op_add`: a template-literal join in a doubling
                    // loop (`` s = `${s}${s}` ``) must not grow without limit.
                    total += s.byte_len();
                    if total > crate::value::MAX_STRING_LEN {
                        return Err(self.throw_range("invalid string length"));
                    }
                    strs.push(s);
                }
                // Fast path when every part is well-formed (the common case):
                // a plain UTF-8 join. Otherwise go through code units so a
                // surrogate straddling a boundary re-pairs (and lone surrogates
                // survive instead of becoming U+FFFD).
                let out = if strs.iter().all(|s| s.is_well_formed()) {
                    let mut out = String::with_capacity(total);
                    for s in &strs {
                        out.push_str(s.as_str());
                    }
                    JsString::new(out)
                } else {
                    let mut units = Vec::new();
                    for s in &strs {
                        units.extend(s.code_units());
                    }
                    JsString::from_code_units(&units)
                };
                push!(Value::String(out));
            }
            Op::LoopKernel(i) => return self.run_kernel_op(frame, *i),

            _ => return self.step_cold(frame, op),
        }
        Ok(Ctl::Next)
    }

    /// The non-hot ops (declarations, names/with/eval, object/class
    /// definition, super/private, iterators' slow paths, dispose, spread,
    /// regexp, dynamic import, ...). `#[inline(never)]` keeps their (large,
    /// unioned) locals out of `step`'s frame; see `step` for why. An op
    /// handled in `step` never reaches this match.
    #[inline(never)]
    fn step_cold(&mut self, frame: &mut Frame, op: &Op) -> Result<Ctl, Value> {
        macro_rules! pop {
            () => {
                frame.stack.pop().unwrap_or(Value::Undefined)
            };
        }
        macro_rules! push {
            ($v:expr) => {
                frame.stack.push($v)
            };
        }
        match op {
            Op::RequireObjectCoercible => {
                if frame.stack.last().map(|v| v.is_nullish()).unwrap_or(true) {
                    return Err(self.throw_type("Cannot destructure a null or undefined value"));
                }
            }
            Op::BindThisSloppy => {
                let t = pop!();
                let bound = self.bind_this_sloppy(t)?;
                push!(bound);
            }
            Op::LoadRestArgs(n) => {
                let v = self.rest_args(frame, *n);
                push!(v);
            }
            Op::LoadArguments => {
                let o = self.make_arguments_object(frame);
                push!(o);
            }

            Op::LoadGlobalTypeof(i) => {
                let name = self.const_name(frame, *i);
                let key = PropertyKey::Str(name);
                let g = self.realm.global.clone();
                let v = self.get_prop(&Value::Object(g), &key)?;
                push!(v);
            }
            Op::DeclareGlobal { name: i, deletable } => {
                let name = self.const_name(frame, *i);
                self.declare_global(name, *deletable)?;
            }

            Op::CanDeclareGlobalFunc(i) => {
                let name = self.const_name(frame, *i);
                self.can_declare_global_func(name)?;
            }
            Op::DefineGlobalFunc { name: i, deletable } => {
                let name = self.const_name(frame, *i);
                let value = pop!();
                self.define_global_func(name, value, *deletable);
            }

            Op::PushWithScope => {
                let v = pop!();
                let obj = self.to_object(&v)?;
                frame.with_scope.push(obj);
            }
            Op::PopWithScope => {
                frame.with_scope.pop();
            }
            Op::LoadName { name, fallback } => {
                let nm = self.const_name(frame, *name);
                let key = PropertyKey::Str(nm);
                if let Some(obj) = self.with_lookup(frame, &key)? {
                    // Object Environment Record GetBindingValue: re-check
                    // HasProperty (the binding could have been removed by the
                    // @@unscopables getter), then read.
                    let base = Value::Object(obj);
                    if self.has_prop(&base, &key)? {
                        let v = self.get_prop(&base, &key)?;
                        push!(v);
                    } else {
                        push!(Value::Undefined);
                    }
                } else {
                    return self.step(frame, &(*fallback).clone());
                }
            }
            Op::StoreName { name, fallback } => {
                let nm = self.const_name(frame, *name);
                let key = PropertyKey::Str(nm);
                if let Some(obj) = self.with_lookup(frame, &key)? {
                    let v = pop!();
                    let strict = frame.func.proto.is_strict;
                    self.put_value(&Value::Object(obj), &key, v, strict)?;
                } else {
                    return self.step(frame, &(*fallback).clone());
                }
            }
            Op::DeleteName(name) => {
                let nm = self.const_name(frame, *name);
                let key = PropertyKey::Str(nm);
                if let Some(obj) = self.with_lookup(frame, &key)? {
                    let r = self.delete_prop(&Value::Object(obj), &key)?;
                    push!(Value::Bool(r));
                } else {
                    // Global Environment Record DeleteBinding: a global-object
                    // property deletes per its configurability (NaN/undefined/
                    // var-created globals are non-configurable -> false); an
                    // unresolvable bare name reports success.
                    let g = self.realm.global.clone();
                    if self.has_own_or_proto(&g, &key) {
                        let r = self.delete_prop(&Value::Object(g), &key)?;
                        push!(Value::Bool(r));
                    } else {
                        push!(Value::Bool(true));
                    }
                }
            }
            Op::ResolveNameBase(name) => {
                let nm = self.const_name(frame, *name);
                let key = PropertyKey::Str(nm);
                match self.with_lookup(frame, &key)? {
                    Some(obj) => push!(Value::Object(obj)),
                    None => push!(Value::Undefined),
                }
            }
            Op::LoadFromBase { name, fallback } => {
                let base = pop!();
                if let Value::Object(_) = &base {
                    let nm = self.const_name(frame, *name);
                    let key = PropertyKey::Str(nm);
                    // Object Environment Record GetBindingValue: re-check
                    // HasProperty (the binding may have been deleted since the
                    // base was captured), then read.
                    if self.has_prop(&base, &key)? {
                        let v = self.get_prop(&base, &key)?;
                        push!(v);
                    } else {
                        push!(Value::Undefined);
                    }
                } else {
                    return self.step(frame, &(*fallback).clone());
                }
            }
            Op::StoreToBase { name, fallback } => {
                let v = pop!();
                let base = pop!();
                if let Value::Object(_) = &base {
                    let nm = self.const_name(frame, *name);
                    let key = PropertyKey::Str(nm.clone());
                    let strict = frame.func.proto.is_strict;
                    // Object Environment Record SetMutableBinding: if the
                    // binding was deleted since the base was captured, a strict
                    // write throws ReferenceError (not a silent re-create).
                    if strict && !self.has_prop(&base, &key)? {
                        return Err(
                            self.throw_reference(&format!("{} is not defined", nm.as_str()))
                        );
                    }
                    self.put_value(&base, &key, v, strict)?;
                } else {
                    frame.stack.push(v);
                    return self.step(frame, &(*fallback).clone());
                }
            }
            Op::RequireCoercible => {
                let base = pop!();
                self.require_object_coercible(&base, "read properties of")?;
            }
            Op::RequireIterResult => {
                let ok = matches!(frame.stack.last(), Some(Value::Object(_)));
                if !ok {
                    return Err(self.throw_type("Iterator result is not an object"));
                }
            }

            Op::GetTemplateObject(idx) => {
                let v = self.template_object(frame, *idx)?;
                push!(v);
            }

            Op::ArrayPushElision => {
                // For array literals we build via NewArray; elisions handled by
                // pushing undefined holes at compile time.
                push!(Value::Undefined);
            }
            Op::ArraySpread => {
                let src = pop!();
                let arr_v = pop!();
                self.array_spread(&arr_v, &src)?;
                push!(arr_v);
            }
            Op::DefineField
            | Op::DefineMethod
            | Op::DefineGetter
            | Op::DefineSetter
            | Op::DefineMethodGetter
            | Op::DefineMethodSetter => {
                let value = pop!();
                let key_v = pop!();
                let obj = pop!();
                let kind = match op {
                    Op::DefineField => crate::reg::DefKind::Field,
                    Op::DefineMethod => crate::reg::DefKind::Method,
                    Op::DefineGetter => crate::reg::DefKind::Getter,
                    Op::DefineSetter => crate::reg::DefKind::Setter,
                    Op::DefineMethodGetter => crate::reg::DefKind::MethodGetter,
                    _ => crate::reg::DefKind::MethodSetter,
                };
                self.define_prop_kind(kind, &obj, &key_v, value)?;
                push!(obj);
            }
            Op::SetHomeObject => {
                // Stack [obj, key, value] unchanged; stamp the value closure's
                // [[HomeObject]] = obj (MakeMethod) so its `super.prop` resolves.
                let n = frame.stack.len();
                if n >= 3 {
                    let home = frame.stack[n - 3].clone();
                    let m = frame.stack[n - 1].clone();
                    Self::set_home_object_op(&home, &m);
                }
            }
            Op::SetHomeObjectAt(n) => {
                let len = frame.stack.len();
                if len > *n as usize {
                    let home = frame.stack[len - 1 - *n as usize].clone();
                    if let (Value::Object(home), Value::Object(m)) =
                        (home, frame.stack[len - 1].clone())
                    {
                        if let Internal::Function(FunctionInner::Bytecode(bf)) =
                            &mut m.borrow_mut().internal
                        {
                            Rc::make_mut(bf).home_object = Some(home);
                        }
                    }
                }
            }
            Op::GetSuperBase => {
                let base = frame
                    .func
                    .home_object
                    .as_ref()
                    .and_then(|h| h.borrow().proto.clone())
                    .map(Value::Object)
                    .unwrap_or(Value::Undefined);
                push!(base);
            }
            Op::SuperGet(k) => {
                let name = self.const_name(frame, *k);
                let base = pop!();
                let this = pop!();
                let key = PropertyKey::Str(name);
                let v = self.super_get(&base, &key, this)?;
                push!(v);
            }
            Op::SuperGetDynamic => {
                let key_v = pop!();
                let base = pop!();
                let this = pop!();
                let key = self.to_property_key(&key_v)?;
                let v = self.super_get(&base, &key, this)?;
                push!(v);
            }
            Op::SuperSet(k) => {
                let name = self.const_name(frame, *k);
                let value = pop!();
                let base = pop!();
                let this = pop!();
                let key = PropertyKey::Str(name);
                self.super_set(&base, &key, value.clone(), this, frame.func.proto.is_strict)?;
                push!(value);
            }
            Op::SuperSetDynamic => {
                let value = pop!();
                let key_v = pop!();
                let base = pop!();
                let this = pop!();
                let key = self.to_property_key(&key_v)?;
                self.super_set(&base, &key, value.clone(), this, frame.func.proto.is_strict)?;
                push!(value);
            }
            Op::ObjectSpread => {
                let src = pop!();
                let target = pop!();
                self.object_spread(&target, &src)?;
                push!(target);
            }
            Op::CopyDataPropertiesExcept(n) => {
                let at = frame.stack.len() - *n as usize;
                let raw_keys = frame.stack.split_off(at);
                let mut excluded: Vec<PropertyKey> = Vec::with_capacity(raw_keys.len());
                for k in raw_keys {
                    excluded.push(self.to_property_key(&k)?);
                }
                let src = pop!();
                let target = pop!();
                self.copy_data_props_except(&target, &src, &excluded)?;
                push!(target);
            }
            Op::PrivateGet(i) => {
                let name = self.resolve_private_name(frame, *i)?;
                let obj = pop!();
                // PrivateGet: the receiver's own [[PrivateElements]] must have
                // the name (the brand check — never the prototype chain, never
                // Proxy traps; `Object.create(instance)` is not an instance).
                let el = obj
                    .as_object()
                    .and_then(|o| o.borrow().private_get(name.id).cloned());
                let v = match el {
                    Some(PrivateElement::Field(v)) | Some(PrivateElement::Method(v)) => v,
                    Some(PrivateElement::Accessor { get: Some(g), .. }) => {
                        self.call(g, obj.clone(), &[])?
                    }
                    // An accessor with only a setter has no [[Get]].
                    Some(PrivateElement::Accessor { get: None, .. }) => {
                        return Err(self.throw_type(&format!(
                            "'{}' was defined without a getter",
                            name.description.as_str()
                        )));
                    }
                    None => {
                        return Err(self.throw_type(&format!(
                            "Cannot read private member {} from an object whose class did not declare it",
                            name.description.as_str()
                        )));
                    }
                };
                push!(v);
            }
            Op::PrivateSet(i) => {
                let name = self.resolve_private_name(frame, *i)?;
                let value = pop!();
                let obj = pop!();
                let el = obj
                    .as_object()
                    .and_then(|o| o.borrow().private_get(name.id).cloned());
                match el {
                    Some(PrivateElement::Field(_)) => {
                        if let Some(o) = obj.as_object() {
                            if let Some(t) = o.borrow_mut().privates.as_mut() {
                                t.insert(name.id, PrivateElement::Field(value.clone()));
                            }
                        }
                    }
                    // A private METHOD is never writable (spec PrivateSet step 6).
                    Some(PrivateElement::Method(_)) => {
                        return Err(self.throw_type(&format!(
                            "Cannot assign to private method {}",
                            name.description.as_str()
                        )));
                    }
                    Some(PrivateElement::Accessor { set: Some(s), .. }) => {
                        self.call(s, obj.clone(), std::slice::from_ref(&value))?;
                    }
                    Some(PrivateElement::Accessor { set: None, .. }) => {
                        return Err(self.throw_type(&format!(
                            "'{}' was defined without a setter",
                            name.description.as_str()
                        )));
                    }
                    None => {
                        return Err(self.throw_type(&format!(
                            "Cannot write private member {} to an object whose class did not declare it",
                            name.description.as_str()
                        )));
                    }
                }
                push!(value);
            }
            Op::PushPrivateEnv(keys) => {
                // NewPrivateEnvironment + AddPrivateName: mint a fresh runtime
                // name per declared key for THIS evaluation of the class.
                let mut names = Vec::with_capacity(keys.len());
                for &k in keys.iter() {
                    let key = self.const_name(frame, k);
                    // Source-visible description: strip the "@<class id>" suffix.
                    let desc = match key.as_str().rfind('@') {
                        Some(at) => JsString::new(&key.as_str()[..at]),
                        None => key.clone(),
                    };
                    let id = self.private_name_counter;
                    self.private_name_counter += 1;
                    names.push((
                        key,
                        PrivateName {
                            id,
                            description: desc,
                        },
                    ));
                }
                frame.priv_env = Some(Rc::new(PrivateEnv {
                    parent: frame.priv_env.take(),
                    names,
                }));
            }
            Op::PopPrivateEnv => {
                frame.priv_env = frame.priv_env.take().and_then(|e| e.parent.clone());
            }
            Op::PrivateFieldAdd(i) => {
                let name = self.resolve_private_name(frame, *i)?;
                let value = pop!();
                let obj = pop!();
                let Some(o) = obj.as_object() else {
                    return Err(self.throw_type("Cannot define a private field on a non-object"));
                };
                // PrivateFieldAdd / PrivateMethodOrAccessorAdd step 1: a
                // non-extensible receiver rejects new private elements.
                if !o.borrow().extensible {
                    return Err(self.throw_type(&format!(
                        "Cannot define private member {} on a non-extensible object",
                        name.description.as_str()
                    )));
                }
                if !o
                    .borrow_mut()
                    .private_add(name.id, PrivateElement::Field(value))
                {
                    return Err(self.throw_type(&format!(
                        "Cannot initialize {} twice on the same object",
                        name.description.as_str()
                    )));
                }
                push!(obj);
            }
            Op::PrivateMethodAdd(i) => {
                let name = self.resolve_private_name(frame, *i)?;
                let value = pop!();
                let obj = pop!();
                let Some(o) = obj.as_object() else {
                    return Err(self.throw_type("Cannot define a private method on a non-object"));
                };
                // PrivateFieldAdd / PrivateMethodOrAccessorAdd step 1: a
                // non-extensible receiver rejects new private elements.
                if !o.borrow().extensible {
                    return Err(self.throw_type(&format!(
                        "Cannot define private member {} on a non-extensible object",
                        name.description.as_str()
                    )));
                }
                if !o
                    .borrow_mut()
                    .private_add(name.id, PrivateElement::Method(value))
                {
                    return Err(self.throw_type(&format!(
                        "Cannot initialize {} twice on the same object",
                        name.description.as_str()
                    )));
                }
                push!(obj);
            }
            Op::PrivateAccessorAdd(i) => {
                let name = self.resolve_private_name(frame, *i)?;
                let set = pop!();
                let get = pop!();
                let obj = pop!();
                let Some(o) = obj.as_object() else {
                    return Err(self.throw_type("Cannot define a private accessor on a non-object"));
                };
                // PrivateFieldAdd / PrivateMethodOrAccessorAdd step 1: a
                // non-extensible receiver rejects new private elements.
                if !o.borrow().extensible {
                    return Err(self.throw_type(&format!(
                        "Cannot define private member {} on a non-extensible object",
                        name.description.as_str()
                    )));
                }
                let el = PrivateElement::Accessor {
                    get: (!get.is_undefined()).then_some(get),
                    set: (!set.is_undefined()).then_some(set),
                };
                if !o.borrow_mut().private_add(name.id, el) {
                    return Err(self.throw_type(&format!(
                        "Cannot initialize {} twice on the same object",
                        name.description.as_str()
                    )));
                }
                push!(obj);
            }
            Op::ConstructSuper(argc) => {
                let nt = pop!();
                let at = frame.stack.len() - *argc as usize;
                let args = frame.stack.split_off(at);
                let sup = pop!();
                let r = self.construct(&sup, &args, &nt)?;
                push!(r);
            }
            Op::ConstructSuperSpread => {
                let nt = pop!();
                let args_arr = pop!();
                let sup = pop!();
                let args = self.iterate_to_vec(&args_arr)?;
                let r = self.construct(&sup, &args, &nt)?;
                push!(r);
            }
            Op::BindThisCell(i) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                let mut slot = frame.cells[*i as usize].borrow_mut();
                if !matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Super constructor may only be called once"));
                }
                *slot = v;
            }
            Op::BindThisUpvalue(i) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                let mut slot = frame.func.upvalues[*i as usize].borrow_mut();
                if !matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Super constructor may only be called once"));
                }
                *slot = v;
            }
            Op::SetFunctionNameFromKey(prefix) => {
                // [.., key, fn] (peek both): SetFunctionName with the runtime
                // property key — symbols name as "[description]" (or "").
                let n = frame.stack.len();
                if n >= 2 {
                    let value = frame.stack[n - 1].clone();
                    let key = frame.stack[n - 2].clone();
                    let prefix = self.const_name(frame, *prefix);
                    self.set_function_name_from_key(&key, &value, prefix);
                }
            }
            Op::SetProtoFromLiteral => {
                let v = pop!();
                let obj = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                Self::set_proto_from_literal(&obj, v);
            }
            Op::PushDisposeScope => {
                frame.dispose_scopes.push(Vec::new());
            }
            Op::TrackDisposable { is_await } => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if matches!(v, Value::Null | Value::Undefined) {
                    // `await using x = null` still records an entry (method
                    // undefined) so disposal performs its Await tick; sync
                    // `using` records nothing.
                    if *is_await {
                        if let Some(scope) = frame.dispose_scopes.last_mut() {
                            scope.push((Value::Undefined, Value::Undefined));
                        }
                    }
                } else {
                    if !matches!(v, Value::Object(_)) {
                        return Err(self.throw_type(
                            "'using' declarations may only be used with object, null, or undefined values",
                        ));
                    }
                    // GetDisposeMethod: @@asyncDispose (await using, falling
                    // back to @@dispose) or @@dispose. A nullish property is
                    // "no method"; a non-callable one is a TypeError.
                    let get_method =
                        |vm: &mut Self, sym: &JsSymbol| -> Result<Option<Value>, Value> {
                            let m = vm.get_prop(&v, &PropertyKey::Sym(sym.clone()))?;
                            if m.is_nullish() {
                                return Ok(None);
                            }
                            if !vm.is_callable(&m) {
                                return Err(vm.throw_type("dispose method is not a function"));
                            }
                            Ok(Some(m))
                        };
                    let method = if *is_await {
                        let asym = self.realm.symbol_async_dispose.clone();
                        match get_method(self, &asym)? {
                            Some(m) => Some(m),
                            None => {
                                let dsym = self.realm.symbol_dispose.clone();
                                get_method(self, &dsym)?
                            }
                        }
                    } else {
                        let dsym = self.realm.symbol_dispose.clone();
                        get_method(self, &dsym)?
                    };
                    let method = method.ok_or_else(|| {
                        self.throw_type(
                            "The value being disposed does not have a [Symbol.dispose] method",
                        )
                    })?;
                    match frame.dispose_scopes.last_mut() {
                        Some(scope) => scope.push((v, method)),
                        None => {
                            return Err(self.throw_type("internal: 'using' outside a dispose scope"))
                        }
                    }
                }
            }
            Op::DisposeScope => {
                // DisposeResources: reverse order; each error converts the
                // parked completion to a throw — chaining an already-thrown
                // completion via SuppressedError(error, suppressed).
                let resources = frame.dispose_scopes.pop().unwrap_or_default();
                for (value, method) in resources.into_iter().rev() {
                    if let Err(e) = self.call(method, value, &[]) {
                        let merged = match frame.pending_completion.take() {
                            Some(Completion::Throw(prev)) => self.make_suppressed_error(e, prev),
                            _ => e,
                        };
                        frame.pending_completion = Some(Completion::Throw(merged));
                    }
                }
            }
            Op::DisposeAsyncNext => {
                let entry = frame.dispose_scopes.last_mut().and_then(|s| s.pop());
                match entry {
                    Some((value, method)) => {
                        let result = if matches!(method, Value::Undefined) {
                            // Nullish `await using`: nothing to call, but the
                            // landing pad still Awaits undefined.
                            Ok(Value::Undefined)
                        } else {
                            self.call(method, value, &[])
                        };
                        match result {
                            Ok(r) => {
                                push!(r);
                                push!(Value::Bool(true));
                            }
                            Err(e) => {
                                let merged = match frame.pending_completion.take() {
                                    Some(Completion::Throw(prev)) => {
                                        self.make_suppressed_error(e, prev)
                                    }
                                    _ => e,
                                };
                                frame.pending_completion = Some(Completion::Throw(merged));
                                push!(Value::Undefined);
                                push!(Value::Bool(true));
                            }
                        }
                    }
                    None => {
                        frame.dispose_scopes.pop();
                        push!(Value::Undefined);
                        push!(Value::Bool(false));
                    }
                }
            }
            Op::MergeDisposeError => {
                let e = pop!();
                let merged = match frame.pending_completion.take() {
                    Some(Completion::Throw(prev)) => self.make_suppressed_error(e, prev),
                    _ => e,
                };
                frame.pending_completion = Some(Completion::Throw(merged));
            }
            Op::ClassLinkSuper => {
                let sup = pop!();
                let ctor = pop!();
                let ctor_obj = match &ctor {
                    Value::Object(o) => o.clone(),
                    _ => return Err(self.throw_type("internal: class ctor not an object")),
                };
                match &sup {
                    Value::Null => {
                        // `extends null`: instances have no prototype chain; the
                        // constructor itself still inherits %Function.prototype%.
                        let p = self.get_prop(&ctor, &PropertyKey::str("prototype"))?;
                        if let Value::Object(po) = p {
                            po.borrow_mut().proto = None;
                        }
                    }
                    _ => {
                        if !self.is_constructor(&sup) {
                            return Err(
                                self.throw_type("Class extends value is not a constructor or null")
                            );
                        }
                        let so = match &sup {
                            Value::Object(o) => o.clone(),
                            _ => unreachable!("constructors are objects"),
                        };
                        let sp = self.get_prop(&sup, &PropertyKey::str("prototype"))?;
                        let proto_parent =
                            match sp {
                                Value::Object(o) => Some(o),
                                Value::Null => None,
                                _ => return Err(self.throw_type(
                                    "Class extends value does not have valid prototype property",
                                )),
                            };
                        let p = self.get_prop(&ctor, &PropertyKey::str("prototype"))?;
                        if let Value::Object(po) = p {
                            po.borrow_mut().proto = proto_parent;
                        }
                        ctor_obj.borrow_mut().proto = Some(so);
                    }
                }
            }
            Op::PrivateHasOwn(i) => {
                let name = self.resolve_private_name(frame, *i)?;
                let obj = pop!();
                // `#x in v`: the RHS must be an object (spec 13.10.1 step 5).
                let Some(o) = obj.as_object() else {
                    return Err(
                        self.throw_type("Cannot use 'in' operator to search in a non-object")
                    );
                };
                let has = o.borrow().private_get(name.id).is_some();
                push!(Value::Bool(has));
            }
            Op::DeleteProp(i) => {
                let name = self.const_name(frame, *i);
                let obj = pop!();
                let strict = frame.func.proto.is_strict;
                let v = self.del_prop_named(obj, name, strict)?;
                push!(v);
            }
            Op::DeletePropDynamic => {
                let key_v = pop!();
                let obj = pop!();
                let strict = frame.func.proto.is_strict;
                let v = self.del_prop_dynamic(obj, key_v, strict)?;
                push!(v);
            }
            Op::ThrowConstAssign => {
                return Err(self.throw_type("Assignment to constant variable."));
            }
            Op::DynamicImport => {
                // Specifier already evaluated (and on the stack); coerce it to a
                // string per spec (a Symbol specifier must reject, never throw).
                // With a host hook installed, the load/link/evaluate runs as a
                // queued job (spec: HostImportModuleDynamically completes in a
                // separate job) and settles the returned promise; without one,
                // module loading is unsupported and the promise rejects.
                let spec = pop!();
                let p = self.new_promise();
                match self.to_js_string(&spec) {
                    Ok(s) => {
                        if let Some(hook) = self.dynamic_import.clone() {
                            let spec_str = s.as_str().to_string();
                            let pj = p.clone();
                            self.microtasks
                                .push_back(crate::vm::Microtask::Job(Box::new(
                                    move |vm: &mut Vm| {
                                        match hook(vm, &spec_str) {
                                            Ok(ns) => vm.resolve_promise(&pj, ns),
                                            Err(e) => vm.reject_promise(&pj, e),
                                        }
                                        Ok(())
                                    },
                                )));
                        } else {
                            let reason = self.make_error(
                                crate::vm::ErrorKind::Type,
                                "dynamic import is not supported",
                            );
                            self.reject_promise(&p, reason);
                        }
                    }
                    Err(e) => self.reject_promise(&p, e),
                }
                push!(Value::Object(p));
            }
            Op::MarkDelegationHandler(return_ip) => {
                if let Some(h) = frame.handlers.last_mut() {
                    h.delegation = true;
                    h.delegation_return_ip = if *return_ip == u32::MAX {
                        None
                    } else {
                        Some(*return_ip)
                    };
                }
            }
            Op::InitEvalVars => {
                // Null prototype: with-scope lookups HasProperty through the
                // chain, and Object.prototype names must not shadow globals.
                let o = self.alloc(crate::value::ObjectData::new(None, Internal::Ordinary));
                // Outermost with-scope: real `with` objects entered later (and
                // the static fallback for names it doesn't hold) take priority.
                frame.with_scope.insert(0, o.clone());
                frame.eval_vars = Some(o);
            }
            Op::DirectEval { argc, scope } => {
                let mut args: Vec<Value> = Vec::with_capacity(*argc as usize);
                for _ in 0..*argc {
                    args.push(pop!());
                }
                args.reverse();
                let callee = pop!();
                // Direct-eval semantics apply only when the callee IS %eval%.
                let is_intrinsic = match (&callee, &self.realm.eval_fn) {
                    (Value::Object(o), Some(e)) => o.same(e),
                    _ => false,
                };
                if !is_intrinsic {
                    let r = self.call(callee, Value::Undefined, &args)?;
                    push!(r);
                } else {
                    let v = self.perform_direct_eval(frame, *scope, args)?;
                    push!(v);
                }
            }
            Op::CompletionJump { target, boundary } => {
                return self.do_completion(
                    frame,
                    Completion::Jump {
                        target: *target,
                        boundary: *boundary,
                    },
                );
            }
            Op::EndFinally => {
                // If a non-local completion is parked, resume it (run the next
                // outer finally, or perform the action). Otherwise the finalizer
                // ran on the normal path: fall through.
                if let Some(c) = frame.pending_completion.take() {
                    return self.do_completion(frame, c);
                }
            }

            // ---- iteration ----
            Op::IteratorClose => {
                // Reached only on an abrupt completion of a `for-of` loop, with
                // that completion parked in `pending_completion`. Per spec
                // (IteratorClose), if the completion is a throw, any error from
                // `return()` is suppressed (the original throw wins); otherwise a
                // `return()` error propagates and a non-object result is a
                // TypeError.
                let it = pop!();
                let completion_is_throw =
                    matches!(frame.pending_completion, Some(Completion::Throw(_)));
                let ret = match self.get_prop(&it, &crate::names::key_return()) {
                    Ok(r) => r,
                    Err(e) => {
                        if completion_is_throw {
                            Value::Undefined
                        } else {
                            return Err(e);
                        }
                    }
                };
                if self.is_callable(&ret) {
                    match self.call(ret, it.clone(), &[]) {
                        Ok(v) => {
                            if !completion_is_throw && !matches!(v, Value::Object(_)) {
                                return Err(
                                    self.throw_type("iterator return() result is not an object")
                                );
                            }
                        }
                        Err(e) => {
                            if !completion_is_throw {
                                return Err(e);
                            }
                        }
                    }
                } else if !ret.is_nullish() && !completion_is_throw {
                    // GetMethod: a present but non-callable `return` is a
                    // TypeError (masked only by an in-flight throw).
                    return Err(self.throw_type("iterator return is not a function"));
                }
            }
            Op::ForInEnumerate => {
                let v = pop!();
                let idx = self.for_in_enumerate(frame, &v)?;
                push!(idx);
            }
            Op::AsyncReturn => {
                let v = pop!();
                return Ok(Ctl::Return(v));
            }

            // ---- misc ----
            Op::NewRegExp { pattern, flags } => {
                let p = self.const_name(frame, *pattern);
                let f = self.const_name(frame, *flags);
                let re = self.make_regexp(p.as_str(), f.as_str())?;
                push!(re);
            }
            Op::GetAsyncIterator => {
                let v = pop!();
                let it = self.get_async_iterator(&v)?;
                push!(it);
            }
            _ => unreachable!("op handled inline in step: {op:?}"),
        }
        Ok(Ctl::Next)
    }

    /// Resolve `key` against the frame's active `with` scopes (innermost first).
    /// Returns the with-object that should service the binding, or `None` if no
    /// with-object provides it (so the caller falls back to lexical/global).
    ///
    /// Implements the object Environment Record `HasBinding`: a name binds to a
    /// with-object iff `HasProperty(obj, key)` is true AND it is not excluded by
    /// the object's `@@unscopables` (a name whose @@unscopables entry is truthy
    /// is treated as absent).
    fn with_lookup(&mut self, frame: &Frame, key: &PropertyKey) -> Result<Option<JsObject>, Value> {
        // Snapshot the scope objects so we don't borrow `frame` across the
        // `&mut self` calls below.
        let scopes: Vec<JsObject> = frame.with_scope.iter().rev().cloned().collect();
        for obj in scopes {
            let base = Value::Object(obj.clone());
            if !self.has_prop(&base, key)? {
                continue;
            }
            // @@unscopables filter.
            let unscm = self.realm.symbol_unscopables.clone();
            let unsc = self.get_prop(&base, &PropertyKey::Sym(unscm))?;
            if let Value::Object(_) = &unsc {
                let blocked = self.get_prop(&unsc, key)?;
                if self.to_boolean(&blocked) {
                    continue;
                }
            }
            return Ok(Some(obj));
        }
        Ok(None)
    }

    fn has_own_or_proto(&self, obj: &JsObject, key: &PropertyKey) -> bool {
        let mut cur = obj.clone();
        loop {
            let proto = {
                let b = cur.borrow();
                if b.own_contains_key(key) {
                    return true;
                }
                b.proto.clone()
            };
            match proto {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    /// `super.x` read: Get(base, key) with an explicit receiver. A nullish
    /// base (null home prototype) is a TypeError, like reading a property of
    /// undefined.
    fn super_get(&mut self, base: &Value, key: &PropertyKey, recv: Value) -> Result<Value, Value> {
        match base {
            Value::Object(o) => self.get_from_object(&o.clone(), key, recv),
            _ => Err(self.throw_type(&format!(
                "Cannot read properties of undefined (reading {key:?})"
            ))),
        }
    }

    /// `super.x = v`: Set(base, key, v, receiver=this); a failed write throws
    /// in strict code, like any strict PutValue.
    fn super_set(
        &mut self,
        base: &Value,
        key: &PropertyKey,
        value: Value,
        recv: Value,
        strict: bool,
    ) -> Result<(), Value> {
        match base {
            Value::Object(o) => {
                let ok = crate::builtins::reflect::reflect_set(self, &o.clone(), key, value, recv)?;
                if !ok && strict {
                    return Err(
                        self.throw_type(&format!("Cannot assign to read only property {key:?}"))
                    );
                }
                Ok(())
            }
            _ => Err(self.throw_type(&format!(
                "Cannot set properties of undefined (setting {key:?})"
            ))),
        }
    }

    /// DefinePropertyOrThrow's non-configurable guard for method/accessor
    /// definitions (`static ['prototype']() {}` must throw, not overwrite).
    fn check_redefinable(&mut self, obj: &Value, key: &PropertyKey) -> Result<(), Value> {
        if let Value::Object(o) = obj {
            let non_config = o.borrow().own_get(key).is_some_and(|p| !p.configurable);
            if non_config {
                return Err(self.throw_type(&format!("Cannot redefine property: {key:?}")));
            }
        }
        Ok(())
    }

    pub fn define_accessor(
        &self,
        obj: &Value,
        key: PropertyKey,
        get: Option<Value>,
        set: Option<Value>,
    ) {
        // Builtin prototype/constructor accessors are always non-enumerable
        // (every spec table 'get X' entry); object-literal accessors go
        // through the DefineGetter/DefineSetter ops instead.
        self.define_accessor_with(obj, key, get, set, false);
    }

    /// Install a non-enumerable `get [Symbol.species]` accessor on `ctor` that
    /// returns the receiver (`this`), the default for the species-aware builtins
    /// (`Array`, `Map`, `Set`, `Promise`, `RegExp`, `ArrayBuffer`, `%TypedArray%`).
    pub fn install_species(&mut self, ctor: &JsObject) {
        let sym = self.realm.symbol_species.clone();
        let getter = self.new_native("get [Symbol.species]", 0, |_vm, this, _a| Ok(this));
        self.define_accessor_with(
            &Value::Object(ctor.clone()),
            PropertyKey::Sym(sym),
            Some(Value::Object(getter)),
            None,
            false,
        );
    }

    /// Define (or merge into) an accessor property, choosing its `enumerable`
    /// attribute. Object-literal accessors are enumerable; class and builtin
    /// accessors are not. Merging a get/set pair preserves the existing flag.
    pub fn define_accessor_with(
        &self,
        obj: &Value,
        key: PropertyKey,
        get: Option<Value>,
        set: Option<Value>,
        enumerable: bool,
    ) {
        if let Value::Object(o) = obj {
            let mut b = o.borrow_mut();
            match b.own_get_mut(&key) {
                Some(Property {
                    kind: PropertyKind::Accessor { get: g, set: s },
                    ..
                }) => {
                    if get.is_some() {
                        *g = get;
                    }
                    if set.is_some() {
                        *s = set;
                    }
                }
                _ => {
                    b.own_insert(
                        key,
                        Property {
                            kind: PropertyKind::Accessor { get, set },
                            enumerable,
                            configurable: true,
                        },
                    );
                }
            }
        }
    }

    pub fn make_closure(
        &self,
        proto: Rc<crate::bytecode::FuncProto>,
        upvalues: Vec<Rc<RefCell<Value>>>,
    ) -> JsObject {
        // `num_params` already holds ExpectedArgumentCount (leading params before
        // the first default/rest), so it is the function's `length` directly.
        let length = proto.num_params;
        let name = proto.name.clone();
        let kind = proto.kind;
        let bf = Rc::new(BytecodeFunction {
            proto,
            upvalues,
            home_object: None,
            is_class_ctor: kind.is_class_ctor(),
            captured_with: Vec::new(),
            captured_priv_env: None,
        });
        // [[Prototype]] is the function-kind intrinsic: %GeneratorFunction.prototype%,
        // %AsyncFunction.prototype%, %AsyncGeneratorFunction.prototype%, or
        // %Function.prototype% for ordinary/arrow/method functions.
        let func_proto = if matches!(
            kind,
            FuncKind::AsyncGenerator | FuncKind::AsyncGeneratorMethod
        ) {
            self.realm.async_generator_function_proto.clone()
        } else if matches!(kind, FuncKind::Generator | FuncKind::GeneratorMethod) {
            self.realm.generator_function_proto.clone()
        } else if kind.is_async() {
            self.realm.async_function_proto.clone()
        } else {
            self.realm.function_proto.clone()
        };
        let obj = self.alloc(ObjectData::new(
            Some(func_proto),
            Internal::Function(FunctionInner::Bytecode(bf)),
        ));
        {
            let mut b = obj.borrow_mut();
            b.own_insert(
                PropertyKey::str("length"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(length as f64),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
            b.own_insert(
                PropertyKey::str("name"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::str(&name),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
        }
        // Ordinary (non-arrow, non-method) functions get a fresh `.prototype`.
        if matches!(
            kind,
            FuncKind::Normal | FuncKind::ClassCtor | FuncKind::DerivedCtor
        ) {
            let proto_obj = self.new_object();
            proto_obj.borrow_mut().own_insert(
                PropertyKey::str("constructor"),
                Property::builtin(Value::Object(obj.clone())),
            );
            obj.borrow_mut().own_insert(
                PropertyKey::str("prototype"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Object(proto_obj),
                        // A CLASS constructor's `prototype` is read-only
                        // (spec ClassDefinitionEvaluation); an ordinary
                        // function's is writable.
                        writable: !kind.is_class_ctor(),
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        } else if matches!(
            kind,
            FuncKind::Generator
                | FuncKind::GeneratorMethod
                | FuncKind::AsyncGenerator
                | FuncKind::AsyncGeneratorMethod
        ) {
            // A generator/async-generator function's `.prototype` is an object
            // whose [[Prototype]] is %Generator%/%AsyncGenerator% — and it has no
            // `constructor` property (spec 27.3.3 / 27.4.3).
            let instance_proto = if kind.is_async() {
                self.realm.async_generator_proto.clone()
            } else {
                self.realm.generator_proto.clone()
            };
            let proto_obj = self.alloc_ordinary(Some(instance_proto));
            obj.borrow_mut().own_insert(
                PropertyKey::str("prototype"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Object(proto_obj),
                        writable: true,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        }
        obj
    }

    // =====================================================================
    // Operators
    // =====================================================================

    pub fn op_add(&mut self, a: Value, b: Value) -> Result<Value, Value> {
        // Fast path: Number + Number. Both operands are already primitive
        // numbers, so ToPrimitive/ToNumeric are identities and the spec result
        // is simply their f64 sum — skip the coercion ceremony entirely. This
        // is by far the most common `+` in numeric code (loop counters, sums).
        if let (Value::Number(x), Value::Number(y)) = (&a, &b) {
            return Ok(Value::Number(x + y));
        }
        // Fast path: String + String. Both operands are already primitive —
        // ToPrimitive and ToString are identities — so the result is their
        // concatenation, with the same length cap and the same `concat` as
        // the generic route below; skipped are two identity-clone
        // ToPrimitive calls and two identity ToString round-trips (the glue
        // idiom `label + ":" + v` pays this twice per join).
        if let (Value::String(sa), Value::String(sb)) = (&a, &b) {
            if sa.byte_len() + sb.byte_len() > crate::value::MAX_STRING_LEN {
                return Err(self.throw_range("invalid string length"));
            }
            return Ok(Value::String(sa.concat(sb)));
        }
        // Fast path: a small plain string ⊕ a Number. Both operands are
        // already primitive (ToPrimitive is the identity) and the result is
        // the concatenation with the number's decimal form —
        // `push_number_string` produces the EXACT digits `to_js_string`
        // would. Formatting straight into the once-allocated result buffer
        // skips two intermediate allocations and copies per `+`. Restricted
        // to sub-rope-threshold flat strings so a big accumulator keeps
        // `concat`'s O(1) rope path (a number string is ≤ 24 bytes, so the
        // bound keeps the total in eager-copy territory either way).
        {
            let sn = match (&a, &b) {
                (Value::String(s), Value::Number(n)) => Some((s, *n, true)),
                (Value::Number(n), Value::String(s)) => Some((s, *n, false)),
                _ => None,
            };
            if let Some((s, n, str_first)) = sn {
                if let Some(flat) = s.as_flat_utf8() {
                    if flat.len() + 24 < crate::value::ROPE_MIN_BYTES {
                        let mut out = String::with_capacity(flat.len() + 24);
                        if str_first {
                            out.push_str(flat);
                            crate::vm::push_number_string(n, &mut out);
                        } else {
                            crate::vm::push_number_string(n, &mut out);
                            out.push_str(flat);
                        }
                        return Ok(Value::String(JsString::from(out)));
                    }
                }
            }
        }
        let pa = self.to_primitive(&a, Hint::Default)?;
        let pb = self.to_primitive(&b, Hint::Default)?;
        if matches!(pa, Value::String(_)) || matches!(pb, Value::String(_)) {
            let sa = self.to_js_string(&pa)?;
            let sb = self.to_js_string(&pb)?;
            let total = sa.byte_len() + sb.byte_len();
            // Bound a single concatenation so a doubling loop (`s += s`) cannot
            // grow a string without limit and OOM the host. The cap is well above
            // any legitimate string; exceeding it throws RangeError, matching how
            // `repeat`/`padStart` already guard eager string growth.
            if total > crate::value::MAX_STRING_LEN {
                return Err(self.throw_range("invalid string length"));
            }
            // Code-unit-preserving concatenation (re-pairs surrogates that
            // straddle the boundary); the common all-UTF-8 case is a plain
            // `push_str` under the hood.
            Ok(Value::String(sa.concat(&sb)))
        } else {
            let xa = self.to_numeric(&pa)?;
            let xb = self.to_numeric(&pb)?;
            match (xa, xb) {
                (Value::BigInt(x), Value::BigInt(y)) => {
                    Ok(Value::bigint((*x).clone() + (*y).clone()))
                }
                (Value::Number(x), Value::Number(y)) => Ok(Value::Number(x + y)),
                _ => {
                    Err(self
                        .throw_type("Cannot mix BigInt and other types, use explicit conversions"))
                }
            }
        }
    }

    /// ToNumeric: ToPrimitive(number) then keep BigInt, else ToNumber.
    pub fn to_numeric(&mut self, v: &Value) -> Result<Value, Value> {
        let p = self.to_primitive(v, Hint::Number)?;
        if let Value::BigInt(_) = p {
            Ok(p)
        } else {
            Ok(Value::Number(self.to_number(&p)?))
        }
    }

    /// ToBigInt: ToPrimitive(number) then coerce. A Number throws a TypeError
    /// (unlike the `BigInt()` constructor, which converts integral Numbers).
    pub fn to_bigint(&mut self, v: &Value) -> Result<num_bigint::BigInt, Value> {
        let p = self.to_primitive(v, Hint::Number)?;
        match &p {
            Value::BigInt(n) => Ok((**n).clone()),
            Value::Bool(b) => Ok(num_bigint::BigInt::from(if *b { 1 } else { 0 })),
            Value::String(s) => parse_string_bigint(s.as_str())
                .ok_or_else(|| self.throw_syntax("Cannot convert string to a BigInt")),
            Value::Number(_) => Err(self.throw_type("Cannot convert a Number to a BigInt")),
            Value::Symbol(_) => Err(self.throw_type("Cannot convert a Symbol to a BigInt")),
            _ => Err(self.throw_type("Cannot convert undefined or null to a BigInt")),
        }
    }

    /// Binary numeric/bigint operation (already-popped operands).
    pub fn arith(&mut self, a: Value, b: Value, kind: ArithKind) -> Result<Value, Value> {
        // Fast path: Number op Number — already numeric primitives, so
        // ToNumeric is the identity. Covers the common arithmetic/bitwise mix
        // in hot loops without the per-operand ToPrimitive/ToNumber detour.
        if let (Value::Number(x), Value::Number(y)) = (&a, &b) {
            return Ok(number_arith(*x, *y, kind));
        }
        let xa = self.to_numeric(&a)?;
        let xb = self.to_numeric(&b)?;
        match (&xa, &xb) {
            (Value::BigInt(x), Value::BigInt(y)) => self.bigint_arith(x, y, kind),
            (Value::Number(x), Value::Number(y)) => Ok(number_arith(*x, *y, kind)),
            _ => {
                Err(self.throw_type("Cannot mix BigInt and other types, use explicit conversions"))
            }
        }
    }

    fn bigint_arith(
        &mut self,
        x: &num_bigint::BigInt,
        y: &num_bigint::BigInt,
        kind: ArithKind,
    ) -> Result<Value, Value> {
        use num_traits::{Signed, Zero};
        let x = x.clone();
        let y = y.clone();
        let v = match kind {
            ArithKind::Sub => x - y,
            ArithKind::Mul => x * y,
            ArithKind::Div => {
                if y.is_zero() {
                    return Err(self.throw_range("Division by zero"));
                }
                x / y
            }
            ArithKind::Mod => {
                if y.is_zero() {
                    return Err(self.throw_range("Division by zero"));
                }
                x % y
            }
            ArithKind::Pow => {
                if y.is_negative() {
                    return Err(self.throw_range("Exponent must be non-negative"));
                }
                match u32::try_from(y) {
                    Ok(e) if e <= 1_000_000 => num_traits::Pow::pow(x, e),
                    _ => return Err(self.throw_range("BigInt exponent too large")),
                }
            }
            ArithKind::BitAnd => x & y,
            ArithKind::BitOr => x | y,
            ArithKind::BitXor => x ^ y,
            ArithKind::Shl | ArithKind::Shr => {
                // JS BigInt: `<<` by negative shifts right and vice-versa.
                let right = matches!(kind, ArithKind::Shr);
                let neg = y.is_negative();
                let mag = y.abs();
                let amt = match u32::try_from(&mag) {
                    Ok(a) if a <= 4096 => a,
                    _ => {
                        // Huge shift: left -> 0 collapses only for right; bound it.
                        if (right ^ neg) && !x.is_zero() {
                            return Err(self.throw_range("BigInt shift too large"));
                        }
                        0
                    }
                };
                if right ^ neg {
                    x >> amt
                } else {
                    x << amt
                }
            }
            ArithKind::UShr => {
                return Err(self.throw_type("BigInts have no unsigned right shift, use >> instead"))
            }
        };
        Ok(Value::bigint(v))
    }

    /// Unary numeric/bigint operation.
    pub fn unary_arith(&mut self, a: Value, kind: UnaryKind) -> Result<Value, Value> {
        // Fast path: a Number is already numeric, so ToNumeric is the identity.
        // `++`/`--` on loop counters route through here every iteration.
        if let Value::Number(n) = a {
            return Ok(Value::Number(match kind {
                UnaryKind::Neg => -n,
                UnaryKind::Inc => n + 1.0,
                UnaryKind::Dec => n - 1.0,
                UnaryKind::BitNot => !crate::vm::to_int32(n) as f64,
            }));
        }
        let x = self.to_numeric(&a)?;
        match x {
            Value::BigInt(n) => {
                let n = (*n).clone();
                let r = match kind {
                    UnaryKind::Neg => -n,
                    UnaryKind::Inc => n + 1,
                    UnaryKind::Dec => n - 1,
                    UnaryKind::BitNot => !n,
                };
                Ok(Value::bigint(r))
            }
            Value::Number(n) => Ok(Value::Number(match kind {
                UnaryKind::Neg => -n,
                UnaryKind::Inc => n + 1.0,
                UnaryKind::Dec => n - 1.0,
                UnaryKind::BitNot => !crate::vm::to_int32(n) as f64,
            })),
            _ => Ok(Value::Number(f64::NAN)),
        }
    }

    pub fn strict_equals(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => x == y,
            _ => strict_equals_nonnumeric(a, b),
        }
    }

    pub fn loose_equals(&mut self, a: &Value, b: &Value) -> Result<bool, Value> {
        use Value::*;
        Ok(match (a, b) {
            (Undefined | Null, Undefined | Null) => true,
            (Number(x), Number(y)) => x == y,
            (String(x), String(y)) => x == y,
            (Bool(x), Bool(y)) => x == y,
            (Symbol(x), Symbol(y)) => x == y,
            (BigInt(x), BigInt(y)) => x == y,
            (Object(x), Object(y)) => x.same(y),
            (BigInt(x), Number(y)) => bigint_eq_f64(x, *y),
            (Number(x), BigInt(y)) => bigint_eq_f64(y, *x),
            (BigInt(x), String(s)) => bigint_eq_str(x, s.as_str()),
            (String(s), BigInt(y)) => bigint_eq_str(y, s.as_str()),
            (Number(_), String(_)) => {
                let nb = self.to_number(b)?;
                a.as_number().unwrap() == nb
            }
            (String(_), Number(_)) => {
                let na = self.to_number(a)?;
                na == b.as_number().unwrap()
            }
            (Bool(_), _) => {
                let na = self.to_number(a)?;
                self.loose_equals(&Number(na), b)?
            }
            (_, Bool(_)) => {
                let nb = self.to_number(b)?;
                self.loose_equals(a, &Number(nb))?
            }
            (Object(_), Number(_) | String(_) | Symbol(_)) => {
                let pa = self.to_primitive(a, Hint::Default)?;
                self.loose_equals(&pa, b)?
            }
            (Number(_) | String(_) | Symbol(_), Object(_)) => {
                let pb = self.to_primitive(b, Hint::Default)?;
                self.loose_equals(a, &pb)?
            }
            _ => false,
        })
    }

    /// Evaluate one of the eight comparison operators — the single source of
    /// truth shared by the standalone comparison ops and every fused
    /// compare-and-branch superinstruction, so coercion order and thrown
    /// errors cannot diverge between fused and unfused code.
    pub fn cmp_values(&mut self, cmp: CmpOp, a: &Value, b: &Value) -> Result<bool, Value> {
        Ok(match cmp {
            CmpOp::Eq => self.loose_equals(a, b)?,
            CmpOp::Ne => !self.loose_equals(a, b)?,
            CmpOp::StrictEq => self.strict_equals(a, b),
            CmpOp::StrictNe => !self.strict_equals(a, b),
            CmpOp::Lt => self.less_than(a, b)? == Some(true),
            CmpOp::Gt => self.less_than(b, a)? == Some(true),
            CmpOp::Le => self.less_than(b, a)? == Some(false),
            CmpOp::Ge => self.less_than(a, b)? == Some(false),
        })
    }

    /// Abstract Relational Comparison `a < b`. Returns None for unordered (NaN).
    pub fn less_than(&mut self, a: &Value, b: &Value) -> Result<Option<bool>, Value> {
        // Fast path: Number < Number (loop bounds, sorts). NaN is unordered, so
        // either operand being NaN yields `None` (all of `<`/`>`/`<=`/`>=`
        // become false at the call site), matching the general path below.
        if let (Value::Number(x), Value::Number(y)) = (a, b) {
            if x.is_nan() || y.is_nan() {
                return Ok(None);
            }
            return Ok(Some(x < y));
        }
        let pa = self.to_primitive(a, Hint::Number)?;
        let pb = self.to_primitive(b, Hint::Number)?;
        if let (Value::String(x), Value::String(y)) = (&pa, &pb) {
            return Ok(Some(x.as_str() < y.as_str()));
        }
        // BigInt comparisons (incl. BigInt vs Number / numeric String).
        match (&pa, &pb) {
            (Value::BigInt(x), Value::BigInt(y)) => return Ok(Some(x < y)),
            (Value::BigInt(x), _) => {
                if let Value::String(s) = &pb {
                    return Ok(bigint_cmp_str(x, s.as_str(), true));
                }
                let nb = self.to_number(&pb)?;
                return Ok(bigint_cmp_f64(x, nb, true));
            }
            (_, Value::BigInt(y)) => {
                if let Value::String(s) = &pa {
                    return Ok(bigint_cmp_str(y, s.as_str(), false));
                }
                let na = self.to_number(&pa)?;
                return Ok(bigint_cmp_f64(y, na, false));
            }
            _ => {}
        }
        let na = self.to_number(&pa)?;
        let nb = self.to_number(&pb)?;
        if na.is_nan() || nb.is_nan() {
            Ok(None)
        } else {
            Ok(Some(na < nb))
        }
    }

    pub fn instance_of(&mut self, obj: &Value, ctor: &Value) -> Result<bool, Value> {
        let cobj = match ctor {
            Value::Object(o) => o.clone(),
            _ => return Err(self.throw_type("Right-hand side of 'instanceof' is not callable")),
        };
        // Symbol.hasInstance
        let has_inst = self.realm.symbol_has_instance.clone();
        let method = self.get_prop(ctor, &PropertyKey::Sym(has_inst))?;
        if self.is_callable(&method) {
            let r = self.call(method, ctor.clone(), std::slice::from_ref(obj))?;
            return Ok(self.to_boolean(&r));
        }
        if !cobj.borrow().is_callable() {
            return Err(self.throw_type("Right-hand side of 'instanceof' is not callable"));
        }
        let target_proto = self.get_prop(ctor, &PropertyKey::str("prototype"))?;
        let target_proto = match target_proto {
            Value::Object(o) => o,
            _ => return Err(self.throw_type("prototype is not an object")),
        };
        // OrdinaryHasInstance walks the chain via [[GetPrototypeOf]], which a
        // proxy in the chain routes through its trap (its own `proto` is None).
        let mut cur = match obj {
            Value::Object(o) => o.clone(),
            _ => return Ok(false),
        };
        loop {
            let proto = self.proxy_or_ordinary_get_prototype_of(&cur)?;
            match proto {
                Value::Object(p) => {
                    if p.same(&target_proto) {
                        return Ok(true);
                    }
                    cur = p;
                }
                _ => return Ok(false),
            }
        }
    }
}

/// Integral array-index probe without libm: `Some(i as usize)` exactly when
/// `i.fract() == 0.0 && (0.0..4294967295.0).contains(&i)` — i.e. `i` is a
/// non-negative integral f64 strictly below 2^32-1 (the array-index bound).
/// The saturating u32 cast round-trip IS the integrality+range test (`-0.0`
/// round-trips to `0` and compares equal, negatives/NaN/too-large all fail
/// the equality), so no libm `trunc` call is emitted.
#[inline]
fn dense_index(i: f64) -> Option<usize> {
    let iu = i as u32;
    if iu as f64 == i && iu != u32::MAX {
        Some(iu as usize)
    } else {
        None
    }
}

fn js_mod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_nan() || b.is_nan() || a.is_infinite() {
        f64::NAN
    } else if b.is_infinite() || a == 0.0 {
        a
    } else {
        // Fast path: both operands integral and exactly representable (|x| <=
        // 2^53) — the loop-counter case. Integer remainder matches libm fmod on
        // integers (truncated division, sign of the dividend), except that a
        // zero result must carry the dividend's sign (-6 % 3 is -0 in JS, as
        // fmod also returns); restore that explicitly.
        const MAX_EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
                                                        // Integrality via cast round-trip (no libm trunc): within ±2^53 the
                                                        // i64 cast is exact, so equality holds iff the value is integral.
        let (ai, bi) = (a as i64, b as i64);
        if ai as f64 == a && bi as f64 == b && a.abs() <= MAX_EXACT && b.abs() <= MAX_EXACT {
            let r = ai % bi;
            if r == 0 {
                return if a.is_sign_negative() { -0.0 } else { 0.0 };
            }
            return r as f64;
        }
        a % b
    }
}

/// Binary arithmetic/bitwise operations dispatched by kind (Number or BigInt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithKind {
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
}

#[derive(Clone, Copy)]
pub enum UnaryKind {
    Neg,
    Inc,
    Dec,
    BitNot,
}

fn bin_arith(vm: &mut Vm, frame: &mut Frame, kind: ArithKind) -> Result<(), Value> {
    let b = frame.stack.pop().unwrap_or(Value::Undefined);
    let a = frame.stack.pop().unwrap_or(Value::Undefined);
    let r = vm.arith(a, b, kind)?;
    frame.stack.push(r);
    Ok(())
}

/// Execute the pinned-callee kernel `ck` (guarded by the caller loop
/// kernel's entry: non-recursive, Number-returning, args/upvalues in the
/// window at `win`) and write its return value to `regs[dst]`. Returns
/// `false` when the cooperative interrupt fired on a callee back-edge — the
/// caller then unwinds exactly like one of its own interrupted edges.
#[inline(never)]
/// Write the STORE-class prop registers back to their entry-resolved
/// property slots — the activation-exit half of kernel prop localization
/// (the entry half loads every slot into the register file's tail). Runs on
/// every path that leaves the register world: normal exits, bails (which
/// route through an Exit), and interrupt unwinds. Load-only classes never
/// change, so their slots are left untouched.
fn writeback_kernel_props(
    k: &crate::bytecode::Kernel,
    objs: &[JsObject],
    prop_slots: &[u32],
    regs: &[f64],
) {
    if k.props_used.is_empty() {
        return;
    }
    let prop_base = k.n_regs as usize - k.props_used.len();
    for (i, p) in k.props_used.iter().enumerate() {
        if !p.store {
            continue;
        }
        let mut b = objs[p.oslot as usize].borrow_mut();
        match b.own_get_index_mut(prop_slots[i] as usize) {
            Some((
                _,
                Property {
                    kind: PropertyKind::Data { value, .. },
                    ..
                },
            )) => *value = Value::Number(regs[prop_base + i]),
            _ => unreachable!("kernel prop slot invariant"),
        }
    }
}

/// Belt-and-braces alias check for a cell-writing function kernel
/// ([`crate::bytecode::Kernel::uv_writes`]): distinct upvalue indices are
/// distinct bindings by construction, but the buffered writes must never be
/// able to diverge from the generic write-through order, so a written cell
/// aliasing any other captured cell declines the call. Out of line to keep
/// `run_fn_kernel` small (write-free kernels skip it entirely).
#[inline(never)]
fn uv_write_cells_alias(k: &crate::bytecode::Kernel, bf: &BytecodeFunction) -> bool {
    k.uv_writes.iter().any(|&(_, u)| {
        let cell = &bf.upvalues[u as usize];
        k.locals.iter().any(|slot| {
            matches!(slot, crate::bytecode::KSlot::Upvalue(u2)
                if *u2 != u && Rc::ptr_eq(cell, &bf.upvalues[*u2 as usize]))
        })
    })
}

/// Flush a cell-writing kernel's written registers back to their cells, on
/// BOTH completions. On return this is the generic path's final cell state.
/// On an interrupt the registers hold every completed store (an upvalue
/// register is written only by translated stores), so the flush leaves
/// exactly the generic prefix state at that poll point — the same contract
/// as the loop kernels' local write-back on the interrupt unwind.
#[inline(never)]
fn flush_uv_writes(k: &crate::bytecode::Kernel, bf: &BytecodeFunction, regs: &[f64; KWIN]) {
    for &(r, u) in k.uv_writes.iter() {
        *bf.upvalues[u as usize].borrow_mut() = Value::Number(regs[usize::from(r) & KWIN_MASK]);
    }
}

/// Cross-window alias check for the pinned-callee path when any callee
/// writes cells: a written cell captured by ANOTHER callee window (whose
/// upvalue snapshot loads once per activation) or by the CALLER kernel's
/// own upvalue registers would go stale after the first flush, so any such
/// overlap declines the activation. Distinct bindings are distinct cells —
/// this only fires when the same closure (or a shared captured binding) is
/// pinned more than once. Out of line: write-free activations never call it.
#[inline(never)]
fn callee_cell_writes_alias(
    callee_bfs: &[(Rc<BytecodeFunction>, u32)],
    caller: &crate::bytecode::Kernel,
    caller_upvalues: &[Rc<RefCell<Value>>],
) -> bool {
    for (i, (bf, _)) in callee_bfs.iter().enumerate() {
        let ck = bf.proto.fn_kernel.as_ref().expect("guarded");
        for &(_, u) in ck.uv_writes.iter() {
            let cell = &bf.upvalues[u as usize];
            // The caller kernel's own upvalue snapshots.
            for slot in caller.locals.iter() {
                if let crate::bytecode::KSlot::Upvalue(cu) = slot {
                    if Rc::ptr_eq(cell, &caller_upvalues[*cu as usize]) {
                        return true;
                    }
                }
            }
            // Every OTHER callee window's captured cells (the same callee's
            // own aliases were declined per-callee at entry).
            for (j, (bf2, _)) in callee_bfs.iter().enumerate() {
                if i == j {
                    continue;
                }
                let ck2 = bf2.proto.fn_kernel.as_ref().expect("guarded");
                for slot in ck2.locals.iter() {
                    if let crate::bytecode::KSlot::Upvalue(u2) = slot {
                        if Rc::ptr_eq(cell, &bf2.upvalues[*u2 as usize]) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Execute a plain (non-recursive, frameless) FUNCTION kernel body over
/// `regs`, returning the call's result — `Value::Number`, or `Value::Bool`
/// for a boolean-typed `Ret`. Shared by the per-call [`Vm::run_fn_kernel`]
/// entry and the prepared-callback path ([`Vm::exec_prepared_kernel`]).
/// Returns `None` when the cooperative interrupt flag latched on a back-edge
/// poll — the caller owns the budget-zeroing unwind.
fn exec_fn_kernel_code(
    code: &[KOp],
    regs: &mut [f64; KWIN],
    interrupt: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    poll: &mut u32,
) -> Option<Value> {
    let mut pc = 0usize;
    loop {
        macro_rules! branch {
            ($t:expr) => {{
                let t = $t as usize;
                if t <= pc {
                    *poll = poll.wrapping_add(1);
                    if *poll & 0xFF == 0 {
                        if let Some(flag) = interrupt {
                            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                return None;
                            }
                        }
                    }
                }
                pc = t;
                continue;
            }};
        }
        match code[pc] {
            KOp::Mov { dst, src } => {
                regs[dst as usize & KWIN_MASK] = regs[src as usize & KWIN_MASK]
            }
            KOp::Const { dst, k } => regs[dst as usize & KWIN_MASK] = k,
            KOp::Add { dst, a, b } => {
                regs[dst as usize & KWIN_MASK] =
                    regs[a as usize & KWIN_MASK] + regs[b as usize & KWIN_MASK]
            }
            KOp::AddK { dst, a, k } => {
                regs[dst as usize & KWIN_MASK] = regs[a as usize & KWIN_MASK] + k
            }
            KOp::Arith { kind, dst, a, b } => {
                regs[dst as usize & KWIN_MASK] = number_arith_raw(
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                    kind,
                )
            }
            KOp::ArithK { kind, dst, a, k } => {
                regs[dst as usize & KWIN_MASK] =
                    number_arith_raw(regs[a as usize & KWIN_MASK], k, kind)
            }
            KOp::Neg { dst, src } => {
                regs[dst as usize & KWIN_MASK] = -regs[src as usize & KWIN_MASK]
            }
            KOp::BitNot { dst, src } => {
                regs[dst as usize & KWIN_MASK] =
                    !crate::vm::to_int32(regs[src as usize & KWIN_MASK]) as f64
            }
            KOp::Br { target } => branch!(target),
            KOp::BrCmp {
                cmp,
                a,
                b,
                if_true,
                target,
            } => {
                if knum_cmp(
                    cmp,
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                ) == if_true
                {
                    branch!(target)
                }
            }
            KOp::BrCmpK {
                cmp,
                a,
                k,
                if_true,
                target,
            } => {
                if knum_cmp(cmp, regs[a as usize & KWIN_MASK], k) == if_true {
                    branch!(target)
                }
            }
            KOp::BrFalsy { src, target } => {
                if !knum_truthy(regs[src as usize & KWIN_MASK]) {
                    branch!(target)
                }
            }
            KOp::BrTruthy { src, target } => {
                if knum_truthy(regs[src as usize & KWIN_MASK]) {
                    branch!(target)
                }
            }
            KOp::CmpSet { cmp, dst, a, b } => {
                regs[dst as usize & KWIN_MASK] = if knum_cmp(
                    cmp,
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                ) {
                    1.0
                } else {
                    0.0
                }
            }
            KOp::BoolNot { dst, src } => {
                regs[dst as usize & KWIN_MASK] = if knum_truthy(regs[src as usize & KWIN_MASK]) {
                    0.0
                } else {
                    1.0
                }
            }
            KOp::Math1 { kind, dst, src } => {
                regs[dst as usize & KWIN_MASK] = kmath1(kind, regs[src as usize & KWIN_MASK])
            }
            KOp::Math2 { kind, dst, a, b } => {
                regs[dst as usize & KWIN_MASK] = kmath2(
                    kind,
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                )
            }
            KOp::Mov2 { d1, s1, d2, s2 } => {
                regs[d1 as usize & KWIN_MASK] = regs[s1 as usize & KWIN_MASK];
                regs[d2 as usize & KWIN_MASK] = regs[s2 as usize & KWIN_MASK];
                // The unfused second op remains in the next slot as a
                // branch-target landing pad; skip it.
                pc += 1;
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
                regs[dst as usize & KWIN_MASK] = number_arith_raw(
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                    kind,
                );
                regs[d2 as usize & KWIN_MASK] =
                    regs[a2 as usize & KWIN_MASK] + regs[b2 as usize & KWIN_MASK];
                pc += 1;
            }
            KOp::AddKBr { dst, a, k, target } => {
                regs[dst as usize & KWIN_MASK] = regs[a as usize & KWIN_MASK] + k;
                branch!(target)
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
                regs[dst as usize & KWIN_MASK] =
                    number_arith_raw(regs[a as usize & KWIN_MASK], k, kind);
                regs[d2 as usize & KWIN_MASK] =
                    regs[a2 as usize & KWIN_MASK] + regs[b2 as usize & KWIN_MASK];
                pc += 1;
            }
            KOp::Ret { src, boolean } => {
                return Some(if boolean {
                    Value::Bool(regs[src as usize & KWIN_MASK] != 0.0)
                } else {
                    Value::Number(regs[src as usize & KWIN_MASK])
                })
            }
            // A frameless kernel has no bytecode frame to bail into;
            // fn-mode translation rejects anything needing one. A
            // SelfCall implies `self_global`, dispatched by the caller.
            KOp::ArrayPush { .. }
            | KOp::ArrayPop { .. }
            | KOp::LoadElem { .. }
            | KOp::StoreElem { .. }
            | KOp::LoadLen { .. }
            | KOp::StrLen { .. }
            | KOp::CharCodeAt { .. }
            | KOp::LoadElemAdd { .. }
            | KOp::LoadElemArith { .. }
            | KOp::LenBrCmp { .. }
            | KOp::LoadProp { .. }
            | KOp::StoreProp { .. }
            | KOp::Exit { .. }
            | KOp::CallKernel { .. }
            | KOp::SelfCall { .. } => unreachable!("bail op in a function kernel"),
        }
        pc += 1;
    }
}

/// Run a pinned-callee kernel over its own fixed window. Returns the callee's
/// `Ret` value (Number-only — the caller guard rejected boolean returns), or
/// `None` when the cooperative interrupt flag latched on a back-edge poll.
fn run_callee_window(
    regs: &mut [f64; KWIN],
    ck: &crate::bytecode::Kernel,
    interrupt: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    poll: &mut u32,
) -> Option<f64> {
    let code = &ck.code;
    let mut pc = 0usize;
    loop {
        macro_rules! branch {
            ($t:expr) => {{
                let t = $t as usize;
                if t <= pc {
                    *poll = poll.wrapping_add(1);
                    if *poll & 0xFF == 0 {
                        if let Some(flag) = interrupt {
                            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                return None;
                            }
                        }
                    }
                }
                pc = t;
                continue;
            }};
        }
        match code[pc] {
            KOp::Mov { dst, src } => {
                regs[dst as usize & KWIN_MASK] = regs[src as usize & KWIN_MASK]
            }
            KOp::Const { dst, k } => regs[dst as usize & KWIN_MASK] = k,
            KOp::Add { dst, a, b } => {
                regs[dst as usize & KWIN_MASK] =
                    regs[a as usize & KWIN_MASK] + regs[b as usize & KWIN_MASK]
            }
            KOp::AddK { dst, a, k } => {
                regs[dst as usize & KWIN_MASK] = regs[a as usize & KWIN_MASK] + k
            }
            KOp::Arith { kind, dst, a, b } => {
                regs[dst as usize & KWIN_MASK] = number_arith_raw(
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                    kind,
                )
            }
            KOp::ArithK { kind, dst, a, k } => {
                regs[dst as usize & KWIN_MASK] =
                    number_arith_raw(regs[a as usize & KWIN_MASK], k, kind)
            }
            KOp::Neg { dst, src } => {
                regs[dst as usize & KWIN_MASK] = -regs[src as usize & KWIN_MASK]
            }
            KOp::BitNot { dst, src } => {
                regs[dst as usize & KWIN_MASK] =
                    !crate::vm::to_int32(regs[src as usize & KWIN_MASK]) as f64
            }
            KOp::Br { target } => branch!(target),
            KOp::BrCmp {
                cmp,
                a,
                b,
                if_true,
                target,
            } => {
                if knum_cmp(
                    cmp,
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                ) == if_true
                {
                    branch!(target)
                }
            }
            KOp::BrCmpK {
                cmp,
                a,
                k,
                if_true,
                target,
            } => {
                if knum_cmp(cmp, regs[a as usize & KWIN_MASK], k) == if_true {
                    branch!(target)
                }
            }
            KOp::BrFalsy { src, target } => {
                if !knum_truthy(regs[src as usize & KWIN_MASK]) {
                    branch!(target)
                }
            }
            KOp::BrTruthy { src, target } => {
                if knum_truthy(regs[src as usize & KWIN_MASK]) {
                    branch!(target)
                }
            }
            KOp::CmpSet { cmp, dst, a, b } => {
                regs[dst as usize & KWIN_MASK] = if knum_cmp(
                    cmp,
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                ) {
                    1.0
                } else {
                    0.0
                }
            }
            KOp::BoolNot { dst, src } => {
                regs[dst as usize & KWIN_MASK] = if knum_truthy(regs[src as usize & KWIN_MASK]) {
                    0.0
                } else {
                    1.0
                }
            }
            KOp::Math1 { kind, dst, src } => {
                regs[dst as usize & KWIN_MASK] = kmath1(kind, regs[src as usize & KWIN_MASK])
            }
            KOp::Math2 { kind, dst, a, b } => {
                regs[dst as usize & KWIN_MASK] = kmath2(
                    kind,
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                )
            }
            KOp::Mov2 { d1, s1, d2, s2 } => {
                regs[d1 as usize & KWIN_MASK] = regs[s1 as usize & KWIN_MASK];
                regs[d2 as usize & KWIN_MASK] = regs[s2 as usize & KWIN_MASK];
                pc += 1;
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
                regs[dst as usize & KWIN_MASK] = number_arith_raw(
                    regs[a as usize & KWIN_MASK],
                    regs[b as usize & KWIN_MASK],
                    kind,
                );
                regs[d2 as usize & KWIN_MASK] =
                    regs[a2 as usize & KWIN_MASK] + regs[b2 as usize & KWIN_MASK];
                pc += 1;
            }
            KOp::AddKBr { dst, a, k, target } => {
                regs[dst as usize & KWIN_MASK] = regs[a as usize & KWIN_MASK] + k;
                branch!(target)
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
                regs[dst as usize & KWIN_MASK] =
                    number_arith_raw(regs[a as usize & KWIN_MASK], k, kind);
                regs[d2 as usize & KWIN_MASK] =
                    regs[a2 as usize & KWIN_MASK] + regs[b2 as usize & KWIN_MASK];
                pc += 1;
            }
            KOp::Ret { src, boolean: _ } => {
                return Some(regs[src as usize & KWIN_MASK]);
            }
            // Impossible in a guarded callee: no bails, no exits, no
            // recursion, no nested closure calls.
            KOp::ArrayPush { .. }
            | KOp::ArrayPop { .. }
            | KOp::LoadElem { .. }
            | KOp::StoreElem { .. }
            | KOp::LoadLen { .. }
            | KOp::StrLen { .. }
            | KOp::CharCodeAt { .. }
            | KOp::LoadElemAdd { .. }
            | KOp::LoadElemArith { .. }
            | KOp::LenBrCmp { .. }
            | KOp::LoadProp { .. }
            | KOp::StoreProp { .. }
            | KOp::Exit { .. }
            | KOp::SelfCall { .. }
            | KOp::CallKernel { .. } => unreachable!("unsupported op in a callee kernel"),
        }
        pc += 1;
    }
}

/// Number (f64) arithmetic preserving the original JS semantics.
/// Numeric comparison for kernel registers — exactly the interpreter's
/// `cmp_values`/`less_than` restricted to `Number` operands: `f64` ordered
/// comparisons are false on NaN, and `==` gives NaN != NaN, +0 == -0.
#[inline]
fn knum_cmp(cmp: CmpOp, a: f64, b: f64) -> bool {
    match cmp {
        CmpOp::Eq | CmpOp::StrictEq => a == b,
        CmpOp::Ne | CmpOp::StrictNe => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

/// ToBoolean restricted to `Number`: false for `0`, `-0`, and NaN.
#[inline]
fn knum_truthy(x: f64) -> bool {
    x != 0.0 && !x.is_nan()
}

/// Kernel Math dispatch — every arm is the SAME core its builtin uses, so a
/// kernelized `Math.round` (etc.) is bit-identical to the generic call.
fn kmath1(kind: crate::bytecode::KMath, x: f64) -> f64 {
    use crate::builtins::numbers as m;
    use crate::bytecode::KMath;
    match kind {
        KMath::Abs => x.abs(),
        KMath::Floor => x.floor(),
        KMath::Ceil => x.ceil(),
        KMath::Round => m::math_round(x),
        KMath::Trunc => x.trunc(),
        KMath::Sign => m::math_sign(x),
        KMath::Sqrt => x.sqrt(),
        KMath::Fround => m::math_fround(x),
        _ => unreachable!("binary Math kind in Math1"),
    }
}

fn kmath2(kind: crate::bytecode::KMath, a: f64, b: f64) -> f64 {
    use crate::builtins::numbers as m;
    use crate::bytecode::KMath;
    match kind {
        KMath::Min2 => m::math_min2(a, b),
        KMath::Max2 => m::math_max2(a, b),
        KMath::Pow2 => m::math_pow(a, b),
        KMath::Imul2 => m::math_imul2(a, b),
        _ => unreachable!("unary Math kind in Math2"),
    }
}

fn number_arith(x: f64, y: f64, kind: ArithKind) -> Value {
    Value::Number(number_arith_raw(x, y, kind))
}

/// The raw `f64` form of [`number_arith`] — shared with the typed loop
/// kernels (`kernel.rs` / [`Vm::run_kernel_op`]) so kernel arithmetic is
/// bit-identical to the interpreter's Number×Number paths by construction.
pub(crate) fn number_arith_raw(x: f64, y: f64, kind: ArithKind) -> f64 {
    use crate::vm::{to_int32, to_uint32};
    match kind {
        ArithKind::Sub => x - y,
        ArithKind::Mul => x * y,
        ArithKind::Div => x / y,
        ArithKind::Mod => js_mod(x, y),
        ArithKind::Pow => x.powf(y),
        ArithKind::BitAnd => (to_int32(x) & to_int32(y)) as f64,
        ArithKind::BitOr => (to_int32(x) | to_int32(y)) as f64,
        ArithKind::BitXor => (to_int32(x) ^ to_int32(y)) as f64,
        ArithKind::Shl => to_int32(x).wrapping_shl(to_uint32(y) & 31) as f64,
        ArithKind::Shr => to_int32(x).wrapping_shr(to_uint32(y) & 31) as f64,
        ArithKind::UShr => (to_uint32(x) >> (to_uint32(y) & 31)) as f64,
    }
}

// ---- BigInt comparison helpers (relational + loose-equality) ----

fn bigint_eq_f64(x: &num_bigint::BigInt, y: f64) -> bool {
    if !y.is_finite() || y.fract() != 0.0 {
        return false;
    }
    // y is an integer-valued finite f64; convert it exactly and compare.
    match <num_bigint::BigInt as num_traits::FromPrimitive>::from_f64(y) {
        Some(yb) => *x == yb,
        None => false,
    }
}

fn big_to_f64(x: &num_bigint::BigInt) -> f64 {
    num_traits::ToPrimitive::to_f64(x).unwrap_or(f64::NAN)
}

fn bigint_eq_str(x: &num_bigint::BigInt, s: &str) -> bool {
    match parse_string_bigint(s) {
        Some(b) => b == *x,
        None => false,
    }
}

/// `bigint < other` (or `other < bigint` when `bigint_left` is false) against an
/// f64. Returns None when the f64 is NaN.
fn bigint_cmp_f64(x: &num_bigint::BigInt, y: f64, bigint_left: bool) -> Option<bool> {
    if y.is_nan() {
        return None;
    }
    let xf = big_to_f64(x);
    Some(if bigint_left { xf < y } else { y < xf })
}

fn bigint_cmp_str(x: &num_bigint::BigInt, s: &str, bigint_left: bool) -> Option<bool> {
    let y = parse_string_bigint(s)?;
    Some(if bigint_left { *x < y } else { y < *x })
}

/// StringToBigInt: trims, parses an integer literal (decimal or 0x/0o/0b), or the
/// empty string as 0. Returns None on failure.
pub fn parse_string_bigint(s: &str) -> Option<num_bigint::BigInt> {
    use num_bigint::BigInt;
    use num_traits::Num;
    let t = s.trim();
    if t.is_empty() {
        return Some(BigInt::from(0));
    }
    let (signed, neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, true, r),
        None => match t.strip_prefix('+') {
            Some(r) => (true, false, r),
            None => (false, false, t),
        },
    };
    // Non-decimal literals (0x/0o/0b) must not carry a sign per StringToBigInt.
    let radix_prefixed = |b: &str| {
        let lower = b.as_bytes().get(1).map(|c| c.to_ascii_lowercase());
        b.starts_with('0') && matches!(lower, Some(b'x') | Some(b'o') | Some(b'b'))
    };
    let parsed = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        if signed {
            return None;
        }
        BigInt::from_str_radix(h, 16).ok()
    } else if let Some(o) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        if signed {
            return None;
        }
        BigInt::from_str_radix(o, 8).ok()
    } else if let Some(bny) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        if signed {
            return None;
        }
        BigInt::from_str_radix(bny, 2).ok()
    } else if radix_prefixed(body) {
        // e.g. a bare "0x" with no digits already handled above; this guards a
        // signed form whose body still looks radix-prefixed.
        return None;
    } else {
        BigInt::from_str_radix(body, 10).ok()
    }?;
    Some(if neg { -parsed } else { parsed })
}

impl Value {
    fn same_obj(&self, other: &JsObject) -> bool {
        matches!(self, Value::Object(o) if o.same(other))
    }
}
