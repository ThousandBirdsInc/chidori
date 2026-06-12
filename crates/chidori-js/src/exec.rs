//! VM execution: the per-frame interpreter loop, binary/unary operators, and the
//! call/construct/closure machinery.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::{Const, FuncKind, Op, UpvalueSource};
use crate::value::*;
use crate::vm::*;

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
            if callable {
                return self.call_object(&o, this, args, Value::Undefined);
            }
            // A callable Proxy ([[Call]] forwards to the apply trap / target).
            if is_proxy && self.is_callable(&func) {
                return self.proxy_call(&o, this, args);
            }
        }
        let desc = self.describe(&func);
        Err(self.throw_type(&format!("{desc} is not a function")))
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
            Bytecode(BytecodeFunction),
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
        bf: BytecodeFunction,
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
        let mut frame = self.make_frame(bf, this, args, new_target);
        frame.func_obj = Some(func_obj.clone());
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
    /// accessor otherwise. Index/parameter aliasing is not modeled.
    fn make_arguments_object(&mut self, frame: &Frame) -> Value {
        let o = self.alloc(ObjectData::new(
            Some(self.realm.object_proto.clone()),
            Internal::Arguments,
        ));
        {
            let mut b = o.borrow_mut();
            for (i, v) in frame.args.iter().enumerate() {
                b.props.insert(
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
            b.props.insert(
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
            .props
            .get(&PropertyKey::str("values"))
            .and_then(|p| p.value().cloned())
            .unwrap_or(Value::Undefined);
        let iter_key = PropertyKey::Sym(self.realm.symbol_iterator.clone());
        o.borrow_mut().props.insert(
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
            .props
            .insert(PropertyKey::str("callee"), callee);
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

    pub fn make_frame(
        &self,
        bf: BytecodeFunction,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> Frame {
        let proto = bf.proto.clone();
        let cells = (0..proto.num_cells)
            .map(|_| Rc::new(RefCell::new(Value::Undefined)))
            .collect();
        // A closure created inside `with (o) { … }` carries the with-object
        // chain; seed the frame's with-scope stack with it so the body's
        // dynamic name ops resolve against it (under any with the body enters).
        let with_scope = bf.captured_with.clone();
        Frame {
            func: bf,
            ip: 0,
            stack: Vec::with_capacity(8),
            locals: vec![Value::Undefined; proto.num_locals as usize],
            cells,
            this,
            new_target,
            handlers: Vec::new(),
            pending_completion: None,
            pending_throw: None,
            pending_return: None,
            args: args.to_vec(),
            func_obj: None,
            dispose_scopes: Vec::new(),
            completion: Value::Undefined,
            enumerators: Vec::new(),
            with_scope,
            trace_token: None,
            skip_delegation_throw: false,
            eval_vars: None,
        }
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
            Value::Object(o) if o.borrow().is_callable() => o.clone(),
            // A constructable Proxy ([[Construct]] forwards to the construct trap).
            Value::Object(o) if matches!(o.borrow().internal, Internal::Proxy(_)) => {
                if !self.is_constructor(ctor) {
                    return Err(self.throw_type("not a constructor"));
                }
                return self.proxy_construct(&o.clone(), args, new_target.clone());
            }
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
            Bytecode(BytecodeFunction),
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
                    {
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

    pub fn run_frame(&mut self, mut frame: Frame) -> Flow {
        let mut interrupt_poll: u32 = 0;
        loop {
            if let Some(budget) = self.op_budget.as_mut() {
                if *budget == 0 {
                    // Uncatchable so execution is guaranteed to terminate.
                    return Flow::Throw(self.throw_range("execution budget exceeded"));
                }
                *budget -= 1;
            }
            // Cooperative cancellation: poll the interrupt flag every 256 ops to
            // keep the atomic load off the hot per-op path while still reacting
            // promptly even when individual ops are expensive (e.g. O(n) string
            // concatenation in a loop). Once observed, latch it by zeroing the op
            // budget so a JS `try/catch` around the slow loop can't resume
            // execution — guaranteeing a prompt, terminating unwind.
            if self.interrupt.is_some() {
                interrupt_poll = interrupt_poll.wrapping_add(1);
                if interrupt_poll & 0xFF == 0 {
                    if let Some(flag) = &self.interrupt {
                        if flag.load(std::sync::atomic::Ordering::Relaxed) {
                            self.op_budget = Some(0);
                            return Flow::Throw(self.throw_range("execution interrupted"));
                        }
                    }
                }
            }
            if let Some(e) = frame.pending_throw.take() {
                match self.do_completion(&mut frame, Completion::Throw(e)) {
                    Ok(Ctl::Jump(t)) => {
                        frame.ip = t;
                        continue;
                    }
                    Ok(Ctl::Return(v)) => return Flow::Return(v),
                    Ok(_) => unreachable!("throw completion yields jump or return"),
                    Err(e) => return Flow::Throw(e),
                }
            }
            // Injected `.return(v)` on a suspended generator: dispatch a Return
            // completion so enclosing `finally` blocks run before the frame ends
            // (a `yield` in a finally re-suspends here as a normal yield).
            if let Some(v) = frame.pending_return.take() {
                match self.do_completion(&mut frame, Completion::Return(v)) {
                    Ok(Ctl::Jump(t)) => {
                        frame.ip = t;
                        continue;
                    }
                    Ok(Ctl::Return(rv)) => return Flow::Return(rv),
                    Ok(_) => unreachable!("return completion yields jump or return"),
                    Err(e) => match self.do_completion(&mut frame, Completion::Throw(e)) {
                        Ok(Ctl::Jump(t)) => {
                            frame.ip = t;
                            continue;
                        }
                        Ok(Ctl::Return(rv)) => return Flow::Return(rv),
                        Ok(_) => unreachable!(),
                        Err(e) => return Flow::Throw(e),
                    },
                }
            }
            let ip = frame.ip;
            if ip >= frame.func.proto.code.len() {
                return Flow::Return(Value::Undefined);
            }
            let op = frame.func.proto.code[ip].clone();
            frame.ip = ip + 1;
            match self.step(&mut frame, op) {
                Ok(Ctl::Next) => continue,
                Ok(Ctl::Jump(target)) => {
                    frame.ip = target;
                    continue;
                }
                Ok(Ctl::Return(v)) => {
                    // Module linker hook: snapshot this frame's final cells when it
                    // is the module body being evaluated (matched by proto pointer).
                    if let Some(p) = &self.module_capture_proto {
                        if Rc::ptr_eq(&frame.func.proto, p) {
                            self.module_capture = Some(frame.cells.clone());
                        }
                    }
                    return Flow::Return(v);
                }
                Ok(Ctl::Await(v)) => {
                    return Flow::Suspend(Suspension {
                        frame: Box::new(frame),
                        kind: SuspendKind::Await(v),
                    })
                }
                Ok(Ctl::Yield(v)) => {
                    return Flow::Suspend(Suspension {
                        frame: Box::new(frame),
                        kind: SuspendKind::Yield(v),
                    })
                }
                Ok(Ctl::YieldStar(v)) => {
                    return Flow::Suspend(Suspension {
                        frame: Box::new(frame),
                        kind: SuspendKind::YieldStar(v),
                    })
                }
                Ok(Ctl::GeneratorStart) => {
                    return Flow::Suspend(Suspension {
                        frame: Box::new(frame),
                        kind: SuspendKind::GeneratorStart,
                    })
                }
                Err(e) => match self.do_completion(&mut frame, Completion::Throw(e)) {
                    Ok(Ctl::Jump(t)) => {
                        frame.ip = t;
                        continue;
                    }
                    Ok(Ctl::Return(v)) => return Flow::Return(v),
                    Ok(_) => unreachable!("throw completion yields jump or return"),
                    Err(e) => return Flow::Throw(e),
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
                        (b.props.contains_key(&key), b.extensible)
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
                            .props
                            .insert(key, Property::data(Value::Undefined));
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
                    if !ev.borrow().props.contains_key(&key) {
                        let mut p = Property::data(Value::Undefined);
                        p.enumerable = true;
                        ev.borrow_mut().props.insert(key, p);
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
        let bf = BytecodeFunction {
            proto: Rc::new(compiled.proto),
            upvalues,
            home_object: frame.func.home_object.clone(),
            is_class_ctor: false,
            captured_with: frame.with_scope.clone(),
        };
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
            Const::String(s) => Value::String(JsString(s.clone())),
            Const::Func(_) => Value::Undefined, // handled by Closure
            Const::BigInt(s) => {
                Value::bigint(parse_string_bigint(s).unwrap_or_else(|| num_bigint::BigInt::from(0)))
            }
        }
    }

    fn const_name(&self, frame: &Frame, idx: u32) -> JsString {
        match &frame.func.proto.consts[idx as usize] {
            Const::String(s) => JsString(s.clone()),
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

    fn step(&mut self, frame: &mut Frame, op: Op) -> Result<Ctl, Value> {
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
            Op::LoadConst(i) => push!(self.const_val(frame, i)),
            Op::LoadUndefined => push!(Value::Undefined),
            Op::LoadHole => push!(Value::Hole),
            Op::LoadNull => push!(Value::Null),
            Op::LoadTrue => push!(Value::Bool(true)),
            Op::LoadFalse => push!(Value::Bool(false)),
            Op::LoadThis => push!(frame.this.clone()),
            Op::RequireObjectCoercible => {
                if frame.stack.last().map(|v| v.is_nullish()).unwrap_or(true) {
                    return Err(self.throw_type("Cannot destructure a null or undefined value"));
                }
            }
            Op::BindThisSloppy => {
                let t = pop!();
                let bound = match t {
                    Value::Undefined | Value::Null => Value::Object(self.realm.global.clone()),
                    Value::Object(_) => t,
                    // A primitive `this` is boxed (ToObject) in sloppy mode.
                    other => Value::Object(self.to_object(&other)?),
                };
                push!(bound);
            }
            Op::LoadNewTarget => push!(frame.new_target.clone()),
            Op::LoadArg(i) => push!(frame
                .args
                .get(i as usize)
                .cloned()
                .unwrap_or(Value::Undefined)),
            Op::LoadRestArgs(n) => {
                let rest: Vec<Value> = if (n as usize) < frame.args.len() {
                    frame.args[n as usize..].to_vec()
                } else {
                    Vec::new()
                };
                push!(Value::Object(self.new_array(rest)));
            }
            Op::LoadArguments => {
                let o = self.make_arguments_object(frame);
                push!(o);
            }

            Op::LoadLocal(i) => push!(frame.locals[i as usize].clone()),
            Op::StoreLocal(i) => {
                let v = pop!();
                frame.locals[i as usize] = v;
            }
            Op::LoadCell(i) => {
                let v = frame.cells[i as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
            }
            Op::StoreCell(i) => {
                let v = pop!();
                *frame.cells[i as usize].borrow_mut() = v;
            }
            Op::StoreCellChecked(i) => {
                let v = pop!();
                let mut slot = frame.cells[i as usize].borrow_mut();
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
                if frame.func.proto.stable_cells.contains(&i) {
                    *frame.cells[i as usize].borrow_mut() = v;
                } else {
                    frame.cells[i as usize] = Rc::new(RefCell::new(v));
                }
            }
            Op::InitCellTdz(i) => {
                // Fresh cell holding the Temporal Dead Zone marker (a hoisted
                // `let`/`const`/`class` binding before its initializer runs).
                if frame.func.proto.stable_cells.contains(&i) {
                    *frame.cells[i as usize].borrow_mut() = Value::Uninitialized;
                } else {
                    frame.cells[i as usize] = Rc::new(RefCell::new(Value::Uninitialized));
                }
            }
            Op::LoadUpvalue(i) => {
                let v = frame.func.upvalues[i as usize].borrow().clone();
                if matches!(v, Value::Uninitialized) {
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                push!(v);
            }
            Op::StoreUpvalue(i) => {
                let v = pop!();
                *frame.func.upvalues[i as usize].borrow_mut() = v;
            }
            Op::StoreUpvalueChecked(i) => {
                let v = pop!();
                let mut slot = frame.func.upvalues[i as usize].borrow_mut();
                if matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Cannot access binding before initialization"));
                }
                *slot = v;
            }
            Op::LoadGlobal(i) => {
                let name = self.const_name(frame, i);
                let key = PropertyKey::Str(name.clone());
                let g = self.realm.global.clone();
                if !self.has_own_or_proto(&g, &key) {
                    return Err(self.throw_reference(&format!("{} is not defined", name.as_str())));
                }
                let v = self.get_prop(&Value::Object(g), &key)?;
                push!(v);
            }
            Op::LoadGlobalTypeof(i) => {
                let name = self.const_name(frame, i);
                let key = PropertyKey::Str(name);
                let g = self.realm.global.clone();
                let v = self.get_prop(&Value::Object(g), &key)?;
                push!(v);
            }
            Op::StoreGlobal(i) => {
                let name = self.const_name(frame, i);
                let v = pop!();
                let g = self.realm.global.clone();
                let strict = frame.func.proto.is_strict;
                let key = PropertyKey::Str(name.clone());
                // A bare assignment to a name bound nowhere is an unresolvable
                // reference; PutValue on one throws ReferenceError in strict mode
                // (a global-object property anywhere on the proto chain counts as
                // resolvable). Sloppy mode creates the global property instead.
                if strict && !self.has_prop(&Value::Object(g.clone()), &key)? {
                    return Err(self.throw_reference(&format!("{} is not defined", name.as_str())));
                }
                self.put_value(&Value::Object(g), &key, v, strict)?;
            }
            Op::DeclareGlobal { name: i, deletable } => {
                let name = self.const_name(frame, i);
                let g = self.realm.global.clone();
                let key = PropertyKey::Str(name.clone());
                let (present, extensible) = {
                    let b = g.borrow();
                    (b.props.contains_key(&key), b.extensible)
                };
                if !present {
                    // CanDeclareGlobalVar/Function: needs an extensible global.
                    if !extensible {
                        return Err(
                            self.throw_type(&format!("Cannot declare global '{}'", name.as_str()))
                        );
                    }
                    // CreateGlobalVarBinding(N, D): writable, enumerable;
                    // configurable only for eval-created bindings.
                    g.borrow_mut().props.insert(
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
                let nm = self.const_name(frame, name);
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
                    return self.step(frame, (*fallback).clone());
                }
            }
            Op::StoreName { name, fallback } => {
                let nm = self.const_name(frame, name);
                let key = PropertyKey::Str(nm);
                if let Some(obj) = self.with_lookup(frame, &key)? {
                    let v = pop!();
                    let strict = frame.func.proto.is_strict;
                    self.put_value(&Value::Object(obj), &key, v, strict)?;
                } else {
                    return self.step(frame, (*fallback).clone());
                }
            }
            Op::DeleteName(name) => {
                let nm = self.const_name(frame, name);
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
                let nm = self.const_name(frame, name);
                let key = PropertyKey::Str(nm);
                match self.with_lookup(frame, &key)? {
                    Some(obj) => push!(Value::Object(obj)),
                    None => push!(Value::Undefined),
                }
            }
            Op::LoadFromBase { name, fallback } => {
                let base = pop!();
                if let Value::Object(_) = &base {
                    let nm = self.const_name(frame, name);
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
                    return self.step(frame, (*fallback).clone());
                }
            }
            Op::StoreToBase { name, fallback } => {
                let v = pop!();
                let base = pop!();
                if let Value::Object(_) = &base {
                    let nm = self.const_name(frame, name);
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
                    return self.step(frame, (*fallback).clone());
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
            Op::NewArray(n) => {
                let n = n as usize;
                let at = frame.stack.len() - n;
                let elems = frame.stack.split_off(at);
                push!(Value::Object(self.new_array(elems)));
            }
            Op::ArrayPushElision => {
                // For array literals we build via NewArray; elisions handled by
                // pushing undefined holes at compile time.
                push!(Value::Undefined);
            }
            Op::ArraySpread => {
                let src = pop!();
                let arr_v = pop!();
                let items = self.iterate_to_vec(&src)?;
                if let Value::Object(a) = &arr_v {
                    let mut b = a.borrow_mut();
                    if let Internal::Array(elems) = &mut b.internal {
                        elems.extend(items);
                    }
                }
                push!(arr_v);
            }
            Op::DefineField => {
                let value = pop!();
                let key_v = pop!();
                let obj = pop!();
                let key = self.to_property_key(&key_v)?;
                if let Value::Object(o) = &obj {
                    // Private storage keys ("#x@id") are spec Private Names:
                    // they attach directly to the receiver — even a Proxy —
                    // without [[DefineOwnProperty]] (no trap, no extensibility
                    // check). Everything else is CreateDataPropertyOrThrow.
                    let is_private = matches!(&key, PropertyKey::Str(s) if s.as_str().starts_with('#'));
                    if is_private {
                        o.borrow_mut().props.insert(key, Property::data(value));
                    } else {
                        crate::builtins::fundamental::create_data_property_or_throw(
                            self, o, &key, value,
                        )?;
                    }
                }
                push!(obj);
            }
            Op::DefineMethod => {
                // Class methods are non-enumerable (writable, configurable).
                let value = pop!();
                let key_v = pop!();
                let obj = pop!();
                let key = self.to_property_key(&key_v)?;
                if let Value::Object(o) = &obj {
                    o.borrow_mut().props.insert(key, Property::builtin(value));
                }
                push!(obj);
            }
            Op::DefineGetter => {
                let getter = pop!();
                let key_v = pop!();
                let obj = pop!();
                let key = self.to_property_key(&key_v)?;
                self.define_accessor_with(&obj, key, Some(getter), None, true);
                push!(obj);
            }
            Op::DefineSetter => {
                let setter = pop!();
                let key_v = pop!();
                let obj = pop!();
                let key = self.to_property_key(&key_v)?;
                self.define_accessor_with(&obj, key, None, Some(setter), true);
                push!(obj);
            }
            Op::DefineMethodGetter => {
                let getter = pop!();
                let key_v = pop!();
                let obj = pop!();
                let key = self.to_property_key(&key_v)?;
                self.define_accessor_with(&obj, key, Some(getter), None, false);
                push!(obj);
            }
            Op::DefineMethodSetter => {
                let setter = pop!();
                let key_v = pop!();
                let obj = pop!();
                let key = self.to_property_key(&key_v)?;
                self.define_accessor_with(&obj, key, None, Some(setter), false);
                push!(obj);
            }
            Op::SetHomeObject => {
                // Stack [obj, key, value] unchanged; stamp the value closure's
                // [[HomeObject]] = obj (MakeMethod) so its `super.prop` resolves.
                let n = frame.stack.len();
                if n >= 3 {
                    let home = frame.stack[n - 3].clone();
                    if let (Value::Object(home), Value::Object(m)) =
                        (home, frame.stack[n - 1].clone())
                    {
                        if let Internal::Function(FunctionInner::Bytecode(bf)) =
                            &mut m.borrow_mut().internal
                        {
                            bf.home_object = Some(home);
                        }
                    }
                }
            }
            Op::GetSuperProp(k) => {
                let name = self.const_name(frame, k);
                let key = PropertyKey::Str(name);
                let recv = frame.this.clone();
                let base = frame
                    .func
                    .home_object
                    .as_ref()
                    .and_then(|h| h.borrow().proto.clone());
                let v = match base {
                    Some(proto) => self.get_from_object(&proto, &key, recv)?,
                    None => Value::Undefined,
                };
                push!(v);
            }
            Op::GetSuperPropDynamic => {
                let key_v = pop!();
                let key = self.to_property_key(&key_v)?;
                let recv = frame.this.clone();
                let base = frame
                    .func
                    .home_object
                    .as_ref()
                    .and_then(|h| h.borrow().proto.clone());
                let v = match base {
                    Some(proto) => self.get_from_object(&proto, &key, recv)?,
                    None => Value::Undefined,
                };
                push!(v);
            }
            Op::ObjectSpread => {
                let src = pop!();
                let target = pop!();
                if let Value::Object(t) = &target {
                    if let Value::Object(s) = &src {
                        for k in self.enumerable_own_keys_dyn(s)? {
                            let val = self.get_prop(&src, &k)?;
                            t.borrow_mut().props.insert(k, Property::data(val));
                        }
                    } else if let Value::String(st) = &src {
                        for (i, c) in st.as_str().chars().enumerate() {
                            t.borrow_mut().props.insert(
                                PropertyKey::from_index(i as u32),
                                Property::data(Value::str(c.to_string())),
                            );
                        }
                    }
                }
                push!(target);
            }
            Op::GetProp(i) => {
                let name = self.const_name(frame, i);
                let obj = pop!();
                let v = self.get_prop(&obj, &PropertyKey::Str(name))?;
                push!(v);
            }
            Op::PrivateGet(i) => {
                let name = self.const_name(frame, i);
                let obj = pop!();
                let key = PropertyKey::Str(name.clone());
                // PrivateBrandCheck: the receiver must OWN the private storage
                // key (fields/static elements are own properties; a prototype-
                // chain hit must NOT pass — `Object.create(instance)` is not an
                // instance). Foreign receivers throw TypeError.
                if !private_owns(&obj, &key) {
                    return Err(self.throw_type(&format!(
                        "Cannot read private member {} from an object whose class did not declare it",
                        private_display(name.as_str())
                    )));
                }
                // A private accessor with only a setter has no [[Get]] (spec
                // PrivateFieldGet step 6.b) — reading it is a TypeError.
                if let Some((has_get, _)) = private_accessor_kind(&obj, &key) {
                    if !has_get {
                        return Err(self.throw_type(&format!(
                            "'{}' was defined without a getter",
                            private_display(name.as_str())
                        )));
                    }
                }
                let v = self.get_prop(&obj, &key)?;
                push!(v);
            }
            Op::PrivateGetB { brand, key } => {
                let brand_name = self.const_name(frame, brand);
                let key_name = self.const_name(frame, key);
                let obj = pop!();
                // PrivateBrandCheck for methods/accessors: the instance carries
                // an own brand key stamped at construction; the element itself
                // lives on the class prototype.
                if !private_owns(&obj, &PropertyKey::Str(brand_name)) {
                    return Err(self.throw_type(&format!(
                        "Cannot read private member {} from an object whose class did not declare it",
                        private_display(key_name.as_str())
                    )));
                }
                let pkey = PropertyKey::Str(key_name.clone());
                if let Some((has_get, _)) = private_accessor_kind(&obj, &pkey) {
                    if !has_get {
                        return Err(self.throw_type(&format!(
                            "'{}' was defined without a getter",
                            private_display(key_name.as_str())
                        )));
                    }
                }
                let v = self.get_prop(&obj, &pkey)?;
                push!(v);
            }
            Op::PrivateSet(i) => {
                let name = self.const_name(frame, i);
                let value = pop!();
                let obj = pop!();
                let key = PropertyKey::Str(name.clone());
                if !private_owns(&obj, &key) {
                    return Err(self.throw_type(&format!(
                        "Cannot write private member {} to an object whose class did not declare it",
                        private_display(name.as_str())
                    )));
                }
                // A private accessor with no [[Set]] is a TypeError to assign.
                if let Some((_, has_set)) = private_accessor_kind(&obj, &key) {
                    if !has_set {
                        return Err(self.throw_type(&format!(
                            "'{}' was defined without a setter",
                            private_display(name.as_str())
                        )));
                    }
                }
                self.set_prop(&obj, &key, value.clone())?;
                push!(value);
            }
            Op::PrivateSetB {
                brand,
                key,
                is_method,
            } => {
                let brand_name = self.const_name(frame, brand);
                let key_name = self.const_name(frame, key);
                let value = pop!();
                let obj = pop!();
                if !private_owns(&obj, &PropertyKey::Str(brand_name)) {
                    return Err(self.throw_type(&format!(
                        "Cannot write private member {} to an object whose class did not declare it",
                        private_display(key_name.as_str())
                    )));
                }
                // A private METHOD is never writable (spec PrivateSet step 6).
                if is_method {
                    return Err(self.throw_type(&format!(
                        "Cannot assign to private method {}",
                        private_display(key_name.as_str())
                    )));
                }
                let pkey = PropertyKey::Str(key_name.clone());
                if let Some((_, has_set)) = private_accessor_kind(&obj, &pkey) {
                    if !has_set {
                        return Err(self.throw_type(&format!(
                            "'{}' was defined without a setter",
                            private_display(key_name.as_str())
                        )));
                    }
                }
                self.set_prop(&obj, &pkey, value.clone())?;
                push!(value);
            }
            Op::ConstructSuper(argc) => {
                let nt = pop!();
                let at = frame.stack.len() - argc as usize;
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
                let mut slot = frame.cells[i as usize].borrow_mut();
                if !matches!(*slot, Value::Uninitialized) {
                    drop(slot);
                    return Err(self.throw_reference("Super constructor may only be called once"));
                }
                *slot = v;
            }
            Op::BindThisUpvalue(i) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                let mut slot = frame.func.upvalues[i as usize].borrow_mut();
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
                    if let Value::Object(f) = &value {
                        if f.borrow().is_callable() {
                            let base = match &key {
                                Value::Symbol(sym) => match sym.description() {
                                    Some(d) => format!("[{d}]"),
                                    None => String::new(),
                                },
                                other => self.to_string_lossy(other),
                            };
                            let prefix = self.const_name(frame, prefix);
                            let name = if prefix.as_str().is_empty() {
                                base
                            } else {
                                format!("{} {}", prefix.as_str(), base)
                            };
                            f.borrow_mut().props.insert(
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
            }
            Op::SetProtoFromLiteral => {
                let v = pop!();
                let obj = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if let Value::Object(o) = &obj {
                    match v {
                        Value::Object(p) => o.borrow_mut().proto = Some(p),
                        Value::Null => o.borrow_mut().proto = None,
                        // Non-object, non-null values are silently ignored.
                        _ => {}
                    }
                }
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
                    if is_await {
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
                    let method = if is_await {
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
                let name = self.const_name(frame, i);
                let obj = pop!();
                // `#x in v`: the RHS must be an object (spec 13.10.1 step 5).
                if !matches!(obj, Value::Object(_)) {
                    return Err(
                        self.throw_type("Cannot use 'in' operator to search in a non-object")
                    );
                }
                push!(Value::Bool(private_owns(&obj, &PropertyKey::Str(name))));
            }
            Op::SetProp(i) => {
                let name = self.const_name(frame, i);
                let value = pop!();
                let obj = pop!();
                let strict = frame.func.proto.is_strict;
                self.put_value(&obj, &PropertyKey::Str(name), value.clone(), strict)?;
                push!(value);
            }
            Op::GetPropDynamic => {
                let key_v = pop!();
                let obj = pop!();
                // GetValue: RequireObjectCoercible(base) (via ToObject) throws
                // BEFORE ToPropertyKey coerces the key expression's value.
                self.require_object_coercible(&obj, "read properties of")?;
                let key = self.to_property_key(&key_v)?;
                let v = self.get_prop(&obj, &key)?;
                push!(v);
            }
            Op::SetPropDynamic => {
                let value = pop!();
                let key_v = pop!();
                let obj = pop!();
                self.require_object_coercible(&obj, "set properties of")?;
                let key = self.to_property_key(&key_v)?;
                let strict = frame.func.proto.is_strict;
                self.put_value(&obj, &key, value.clone(), strict)?;
                push!(value);
            }
            Op::DeleteProp(i) => {
                let name = self.const_name(frame, i);
                let obj = pop!();
                let r = self.delete_prop(&obj, &PropertyKey::Str(name.clone()))?;
                // Strict-mode `delete` that fails throws (spec 13.5.1.2 step 5.c).
                if !r && frame.func.proto.is_strict {
                    return Err(self.throw_type(&format!(
                        "Cannot delete property '{}' in strict mode",
                        name.as_str()
                    )));
                }
                push!(Value::Bool(r));
            }
            Op::DeletePropDynamic => {
                let key_v = pop!();
                let obj = pop!();
                self.require_object_coercible(&obj, "delete properties of")?;
                let key = self.to_property_key(&key_v)?;
                let r = self.delete_prop(&obj, &key)?;
                if !r && frame.func.proto.is_strict {
                    return Err(self.throw_type("Cannot delete property in strict mode"));
                }
                push!(Value::Bool(r));
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
                    return Ok(Ctl::Jump(t as usize));
                }
            }

            Op::Call(argc) => {
                let n = argc as usize;
                let at = frame.stack.len() - n;
                let args = frame.stack.split_off(at);
                let this = pop!();
                let func = pop!();
                let r = self.call(func, this, &args)?;
                push!(r);
            }
            Op::CallMethodless(argc) => {
                let n = argc as usize;
                let at = frame.stack.len() - n;
                let args = frame.stack.split_off(at);
                let func = pop!();
                let r = self.call(func, Value::Undefined, &args)?;
                push!(r);
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
                let n = argc as usize;
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
                let proto = match &frame.func.proto.consts[i as usize] {
                    Const::Func(p) => p.clone(),
                    _ => return Err(self.throw_type("internal: bad closure const")),
                };
                let upvalues = proto
                    .upvalues
                    .iter()
                    .map(|src| match src {
                        UpvalueSource::ParentCell(idx) => frame.cells[*idx as usize].clone(),
                        UpvalueSource::ParentUpvalue(idx) => {
                            frame.func.upvalues[*idx as usize].clone()
                        }
                    })
                    .collect();
                let f = self.make_closure(proto, upvalues);
                // Capture the active with-scope chain (closures defined inside
                // `with` resolve free identifiers against it after the block).
                if !frame.with_scope.is_empty() {
                    if let Internal::Function(FunctionInner::Bytecode(bf)) =
                        &mut f.borrow_mut().internal
                    {
                        bf.captured_with = frame.with_scope.clone();
                    }
                }
                push!(Value::Object(f));
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
                push!(Value::str(a.type_of()));
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
            Op::Jump(t) => return Ok(Ctl::Jump(t as usize)),
            Op::JumpIfTrue(t) => {
                let v = pop!();
                if self.to_boolean(&v) {
                    return Ok(Ctl::Jump(t as usize));
                }
            }
            Op::JumpIfFalse(t) => {
                let v = pop!();
                if !self.to_boolean(&v) {
                    return Ok(Ctl::Jump(t as usize));
                }
            }
            Op::JumpIfFalsyPeek(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if !self.to_boolean(&v) {
                    return Ok(Ctl::Jump(t as usize));
                }
                frame.stack.pop();
            }
            Op::JumpIfTruthyPeek(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if self.to_boolean(&v) {
                    return Ok(Ctl::Jump(t as usize));
                }
                frame.stack.pop();
            }
            Op::JumpIfNullishPeek(t) => {
                let v = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                if !v.is_nullish() {
                    return Ok(Ctl::Jump(t as usize));
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
                    catch_ip: if catch == u32::MAX { None } else { Some(catch) },
                    finally_ip: if finally == u32::MAX {
                        None
                    } else {
                        Some(finally)
                    },
                    stack_depth: frame.stack.len(),
                    with_depth: frame.with_scope.len(),
                    delegation: false,
                    delegation_return_ip: None,
                });
            }
            Op::MarkDelegationHandler(return_ip) => {
                if let Some(h) = frame.handlers.last_mut() {
                    h.delegation = true;
                    h.delegation_return_ip = if return_ip == u32::MAX {
                        None
                    } else {
                        Some(return_ip)
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
                let mut args: Vec<Value> = Vec::with_capacity(argc as usize);
                for _ in 0..argc {
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
                    let v = self.perform_direct_eval(frame, scope, args)?;
                    push!(v);
                }
            }
            Op::PopTryHandler => {
                frame.handlers.pop();
            }
            Op::CompletionJump { target, boundary } => {
                return self.do_completion(frame, Completion::Jump { target, boundary });
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
            Op::GetIterator => {
                let v = pop!();
                let it = self.get_iterator(&v)?;
                push!(it);
            }
            Op::IteratorNext => {
                let it = frame.stack.last().cloned().unwrap_or(Value::Undefined);
                let next = self.get_prop(&it, &PropertyKey::str("next"))?;
                let res = self.call(next, it, &[])?;
                push!(res);
            }
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
                let ret = match self.get_prop(&it, &PropertyKey::str("return")) {
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
                let keys = self.for_in_keys(&v)?;
                frame.enumerators.push((keys, 0));
                push!(Value::Number((frame.enumerators.len() - 1) as f64));
            }
            Op::ForInPop => {
                frame.enumerators.pop();
            }
            Op::ForInNext => {
                let idx = frame.enumerators.len() - 1;
                let (keys, cursor) = &mut frame.enumerators[idx];
                if *cursor < keys.len() {
                    let k = keys[*cursor].clone();
                    *cursor += 1;
                    push!(Value::String(k));
                    push!(Value::Bool(true));
                } else {
                    push!(Value::Undefined);
                    push!(Value::Bool(false));
                }
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
            Op::AsyncReturn => {
                let v = pop!();
                return Ok(Ctl::Return(v));
            }

            // ---- misc ----
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
                let n = n as usize;
                let at = frame.stack.len() - n;
                let parts = frame.stack.split_off(at);
                let mut out = String::new();
                for p in &parts {
                    let s = self.to_js_string(p)?;
                    // Same bound as `op_add`: a template-literal join in a doubling
                    // loop (`` s = `${s}${s}` ``) must not grow without limit.
                    if out.len() + s.as_str().len() > crate::value::MAX_STRING_LEN {
                        return Err(self.throw_range("invalid string length"));
                    }
                    out.push_str(s.as_str());
                }
                push!(Value::str(out));
            }
            Op::NewRegExp { pattern, flags } => {
                let p = self.const_name(frame, pattern);
                let f = self.const_name(frame, flags);
                let re = self.make_regexp(p.as_str(), f.as_str())?;
                push!(re);
            }
            Op::GetAsyncIterator => {
                let v = pop!();
                let it = self.get_async_iterator(&v)?;
                push!(it);
            }
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
                if b.props.contains_key(key) {
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

    pub fn define_accessor(
        &self,
        obj: &Value,
        key: PropertyKey,
        get: Option<Value>,
        set: Option<Value>,
    ) {
        self.define_accessor_with(obj, key, get, set, true);
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
            match b.props.get_mut(&key) {
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
                    b.props.insert(
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
        let bf = BytecodeFunction {
            proto,
            upvalues,
            home_object: None,
            is_class_ctor: kind.is_class_ctor(),
            captured_with: Vec::new(),
        };
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
            b.props.insert(
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
            b.props.insert(
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
            proto_obj.borrow_mut().props.insert(
                PropertyKey::str("constructor"),
                Property::builtin(Value::Object(obj.clone())),
            );
            obj.borrow_mut().props.insert(
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
            obj.borrow_mut().props.insert(
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
        let pa = self.to_primitive(&a, Hint::Default)?;
        let pb = self.to_primitive(&b, Hint::Default)?;
        if matches!(pa, Value::String(_)) || matches!(pb, Value::String(_)) {
            let sa = self.to_js_string(&pa)?;
            let sb = self.to_js_string(&pb)?;
            let total = sa.as_str().len() + sb.as_str().len();
            // Bound a single concatenation so a doubling loop (`s += s`) cannot
            // grow a string without limit and OOM the host. The cap is well above
            // any legitimate string; exceeding it throws RangeError, matching how
            // `repeat`/`padStart` already guard eager string growth.
            if total > crate::value::MAX_STRING_LEN {
                return Err(self.throw_range("invalid string length"));
            }
            let mut s = String::with_capacity(total);
            s.push_str(sa.as_str());
            s.push_str(sb.as_str());
            Ok(Value::str(s))
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

    /// Abstract Relational Comparison `a < b`. Returns None for unordered (NaN).
    pub fn less_than(&mut self, a: &Value, b: &Value) -> Result<Option<bool>, Value> {
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
            let r = self.call(method, ctor.clone(), &[obj.clone()])?;
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
        let mut cur = match obj {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => return Ok(false),
        };
        while let Some(p) = cur {
            if p.same(&target_proto) {
                return Ok(true);
            }
            cur = p.borrow().proto.clone();
        }
        Ok(false)
    }
}

fn js_mod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_nan() || b.is_nan() || a.is_infinite() {
        f64::NAN
    } else if b.is_infinite() {
        a
    } else if a == 0.0 {
        a
    } else {
        a % b
    }
}

/// Binary arithmetic/bitwise operations dispatched by kind (Number or BigInt).
#[derive(Clone, Copy)]
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

/// Number (f64) arithmetic preserving the original JS semantics.
fn number_arith(x: f64, y: f64, kind: ArithKind) -> Value {
    use crate::vm::{to_int32, to_uint32};
    Value::Number(match kind {
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
    })
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

/// If `key` (a private `#name`) resolves on `obj` or its prototype chain to an
/// accessor property, return `(has_getter, has_setter)`; otherwise `None` (a data
/// field/method). Used by `PrivateGet`/`PrivateSet` to enforce the spec's
/// accessor-without-getter / accessor-without-setter TypeErrors.
/// Whether `obj` OWNS the private storage/brand key — the spec's
/// PrivateElementFind over the receiver's own [[PrivateElements]] (never the
/// prototype chain, never Proxy traps).
fn private_owns(obj: &Value, key: &PropertyKey) -> bool {
    match obj {
        Value::Object(o) => o.borrow().props.contains_key(key),
        _ => false,
    }
}

/// The source-visible form of an internal private key: `#name@<classid>`
/// renders as `#name` in error messages.
fn private_display(key: &str) -> &str {
    key.split('@').next().unwrap_or(key)
}

fn private_accessor_kind(obj: &Value, key: &PropertyKey) -> Option<(bool, bool)> {
    let mut cur = match obj {
        Value::Object(o) => o.clone(),
        _ => return None,
    };
    loop {
        let next = {
            let b = cur.borrow();
            if let Some(p) = b.props.get(key) {
                return match &p.kind {
                    PropertyKind::Accessor { get, set } => Some((get.is_some(), set.is_some())),
                    PropertyKind::Data { .. } => None,
                };
            }
            b.proto.clone()
        };
        match next {
            Some(p) => cur = p,
            None => return None,
        }
    }
}
