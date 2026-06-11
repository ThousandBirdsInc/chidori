//! Synchronous generators. A generator object wraps a suspended frame; `.next()`
//! resumes it until the next `yield` (frozen in memory) or completion. `yield*`
//! is desugared by the compiler into an explicit delegation loop, so the VM only
//! ever sees plain `yield`.

use crate::value::*;
use crate::vm::*;

#[derive(Clone, Copy)]
pub enum ResumeKind {
    Next,
    Return,
    Throw,
}

impl Vm {
    pub fn make_generator(
        &mut self,
        func_obj: &JsObject,
        bf: BytecodeFunction,
        this: Value,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Value> {
        let is_async = bf.proto.kind.is_async();
        // The generator object's [[Prototype]] is the function's own `.prototype`
        // (an object whose own proto is %Generator%), falling back to %Generator%
        // itself (spec EvaluateGeneratorBody / OrdinaryCreateFromConstructor).
        let proto = {
            let own = func_obj.borrow();
            match own.props.get(&PropertyKey::str("prototype")) {
                Some(Property {
                    kind:
                        PropertyKind::Data {
                            value: Value::Object(p),
                            ..
                        },
                    ..
                }) => p.clone(),
                _ => {
                    drop(own);
                    if is_async {
                        self.realm.async_generator_proto.clone()
                    } else {
                        self.realm.generator_proto.clone()
                    }
                }
            }
        };
        let mut frame = self.make_frame(bf, this, args, new_target);
        let token = self.trace_enter(&frame.func.proto);
        frame.trace_token = token;
        // Run the parameter prologue now (call-time parameter evaluation): the
        // frame suspends at `GeneratorStart` before the body. A parameter error
        // (bad destructuring, throwing default) throws here, at call time.
        let state = match self.run_frame(frame) {
            Flow::Suspend(s) => match s.kind {
                SuspendKind::GeneratorStart => {
                    self.trace_suspend(token);
                    GeneratorState::SuspendedStart(s.frame)
                }
                // An `await` in an async-generator parameter default ran during
                // the prologue; resume into it lazily (best-effort).
                _ => {
                    self.trace_suspend(token);
                    GeneratorState::SuspendedStart(s.frame)
                }
            },
            Flow::Throw(e) => {
                self.trace_exit(token, true);
                return Err(e);
            }
            Flow::Return(_) => {
                self.trace_exit(token, false);
                GeneratorState::Completed
            }
        };
        let gen = self.alloc(ObjectData::new(
            Some(proto),
            Internal::Generator(GeneratorData {
                state,
                is_async,
                queue: std::collections::VecDeque::new(),
            }),
        ));
        Ok(Value::Object(gen))
    }

    /// Resume an async generator: returns a Promise of `{ value, done }`. The
    /// frame may both `yield` (settles this promise) and `await` (suspends the
    /// frame on another promise, then continues toward the next yield/return).
    pub fn async_generator_resume(
        &mut self,
        gen: &Value,
        kind: ResumeKind,
        value: Value,
    ) -> Result<Value, Value> {
        let gobj = match gen {
            // Must be an *async* generator; a sync generator (or anything else)
            // rejects the returned promise (spec 27.6.1.2/.3/.4 brand check).
            Value::Object(o) if matches!(&o.borrow().internal, Internal::Generator(g) if g.is_async) => {
                o.clone()
            }
            _ => {
                let p = self.new_promise();
                let e = self.throw_type("not an async generator");
                self.reject_promise(&p, e);
                return Ok(Value::Object(p));
            }
        };
        // Enqueue the request and drive a step only if the generator is idle.
        // Concurrent `next`/`return`/`throw` calls are serialized through the
        // queue (AsyncGeneratorEnqueue) — never rejected as "already running".
        let result = self.new_promise();
        {
            let mut b = gobj.borrow_mut();
            if let Internal::Generator(g) = &mut b.internal {
                g.queue.push_back(AsyncGenRequest {
                    kind,
                    value,
                    result: result.clone(),
                });
            }
        }
        self.agen_drain(&gobj);
        Ok(Value::Object(result))
    }

    /// If the async generator is idle (not mid-step) and has a queued request,
    /// start driving the front request. A completing step pops that request and
    /// re-drains, so a fully-synchronous batch of `next()` calls is processed in
    /// FIFO order without re-entrancy errors. The front request stays on the
    /// queue until its step settles (see [`agen_complete_step`]).
    fn agen_drain(&mut self, gobj: &JsObject) {
        enum Action {
            Resume(Box<Frame>, bool, (ResumeKind, Value, JsObject)),
            Completed((ResumeKind, Value, JsObject)),
        }
        loop {
            let action = {
                let mut b = gobj.borrow_mut();
                let g = match &mut b.internal {
                    Internal::Generator(g) => g,
                    _ => return,
                };
                if matches!(g.state, GeneratorState::Executing) {
                    return; // a step is in flight; it re-drains on completion
                }
                let front = match g.queue.front() {
                    Some(r) => (r.kind, r.value.clone(), r.result.clone()),
                    None => return,
                };
                match std::mem::replace(&mut g.state, GeneratorState::Executing) {
                    GeneratorState::SuspendedStart(f) => Action::Resume(f, true, front),
                    GeneratorState::SuspendedYield(f) => Action::Resume(f, false, front),
                    GeneratorState::Completed => {
                        g.state = GeneratorState::Completed;
                        Action::Completed(front)
                    }
                    GeneratorState::Executing => unreachable!(),
                }
            };
            match action {
                Action::Completed((kind, value, result)) => {
                    // Generator already done: settle this request against the
                    // closed state and loop to drain any further queued ones.
                    match kind {
                        ResumeKind::Throw => self.reject_promise(&result, value),
                        ResumeKind::Return => {
                            let r = self.make_iter_result(value, true);
                            self.resolve_promise(&result, r);
                        }
                        ResumeKind::Next => {
                            let r = self.make_iter_result(Value::Undefined, true);
                            self.resolve_promise(&result, r);
                        }
                    }
                    self.pop_agen_request(gobj);
                    continue;
                }
                Action::Resume(frame, is_start, (kind, value, result)) => {
                    if matches!(kind, ResumeKind::Return) {
                        // AsyncGeneratorReturn: Await the return value (unwrapping
                        // a thenable) before settling `{value, done:true}`; a
                        // rejected value rejects the result. (Running an enclosing
                        // `finally` on a suspended-yield return is the separate
                        // try/finally gap.)
                        self.set_gen_state(gobj, GeneratorState::Completed);
                        let target = self.promise_resolve(value);
                        let gen_f = gobj.clone();
                        let res_f = result.clone();
                        let on_f = self.new_native("", 1, move |vm, _t, args| {
                            let v = args.first().cloned().unwrap_or(Value::Undefined);
                            let r = vm.make_iter_result(v, true);
                            vm.resolve_promise(&res_f, r);
                            vm.agen_complete_step(&gen_f);
                            Ok(Value::Undefined)
                        });
                        let gen_r = gobj.clone();
                        let res_r = result.clone();
                        let on_r = self.new_native("", 1, move |vm, _t, args| {
                            let e = args.first().cloned().unwrap_or(Value::Undefined);
                            vm.reject_promise(&res_r, e);
                            vm.agen_complete_step(&gen_r);
                            Ok(Value::Undefined)
                        });
                        self.promise_then(&target, Value::Object(on_f), Value::Object(on_r));
                        return; // completion is async; on_f/on_r re-drain
                    }
                    let token = frame.trace_token;
                    self.trace_resume(token);
                    let flow = match (is_start, kind) {
                        (true, ResumeKind::Throw) => self.resume_frame_throw(frame, value),
                        (true, _) => self.run_frame(*frame),
                        (false, ResumeKind::Throw) => self.resume_frame_throw(frame, value),
                        (false, _) => self.resume_frame(frame, value),
                    };
                    self.agen_drive(gobj, flow, &result, token);
                    return; // agen_drive completed (and re-drained) or is awaiting
                }
            }
        }
    }

    /// Remove the just-settled front request, then drive the next queued one.
    /// Called when a step finishes (a yield surfaces, or the generator
    /// returns/throws) — the point where the current request's promise settles.
    fn agen_complete_step(&mut self, gobj: &JsObject) {
        self.pop_agen_request(gobj);
        self.agen_drain(gobj);
    }

    fn pop_agen_request(&mut self, gobj: &JsObject) {
        if let Internal::Generator(g) = &mut gobj.borrow_mut().internal {
            g.queue.pop_front();
        }
    }

    /// Continue driving an async generator after a frame step, settling
    /// `result` when the frame yields/returns/throws, or chaining on an internal
    /// `await`.
    fn agen_drive(&mut self, gobj: &JsObject, flow: Flow, result: &JsObject, token: Option<u64>) {
        match flow {
            Flow::Return(v) => {
                self.trace_exit(token, false);
                self.set_gen_state(gobj, GeneratorState::Completed);
                let r = self.make_iter_result(v, true);
                self.resolve_promise(result, r);
                self.agen_complete_step(gobj);
            }
            Flow::Throw(e) => {
                self.trace_exit(token, true);
                self.set_gen_state(gobj, GeneratorState::Completed);
                self.reject_promise(result, e);
                self.agen_complete_step(gobj);
            }
            Flow::Suspend(s) => match s.kind {
                SuspendKind::Yield(y) => {
                    self.trace_suspend(token);
                    // AsyncGeneratorYield: the operand is `Await`ed before the
                    // generator actually suspends. A yielded thenable that
                    // fulfills produces the iterator result with the *awaited*
                    // value; one that rejects is thrown back into the generator
                    // at the yield point (so `yield Promise.reject(e)` with no
                    // local handler rejects this `next()` and closes the gen).
                    let target = self.promise_resolve(y);
                    let cell = std::rc::Rc::new(std::cell::RefCell::new(Some(s.frame)));
                    let gen_f = gobj.clone();
                    let res_f = result.clone();
                    let cell_f = cell.clone();
                    let on_f = self.new_native("", 1, move |vm, _t, args| {
                        if let Some(fr) = cell_f.borrow_mut().take() {
                            let awaited = args.get(0).cloned().unwrap_or(Value::Undefined);
                            vm.set_gen_state(&gen_f, GeneratorState::SuspendedYield(fr));
                            let r = vm.make_iter_result(awaited, false);
                            vm.resolve_promise(&res_f, r);
                            // The yield surfaced: this request is satisfied, so
                            // pop it and drive the next queued one.
                            vm.agen_complete_step(&gen_f);
                        }
                        Ok(Value::Undefined)
                    });
                    let gen_r = gobj.clone();
                    let res_r = result.clone();
                    let on_r = self.new_native("", 1, move |vm, _t, args| {
                        if let Some(fr) = cell.borrow_mut().take() {
                            let token = fr.trace_token;
                            vm.trace_resume(token);
                            let e = args.get(0).cloned().unwrap_or(Value::Undefined);
                            let flow = vm.resume_frame_throw(fr, e);
                            vm.agen_drive(&gen_r, flow, &res_r, token);
                        }
                        Ok(Value::Undefined)
                    });
                    self.promise_then(&target, Value::Object(on_f), Value::Object(on_r));
                }
                SuspendKind::Await(awaited) => {
                    // Internal await: resume the frame when `awaited` settles,
                    // then keep driving the SAME result promise. The trace token
                    // rides `s.frame`, so each resume re-reads it.
                    let target = self.promise_resolve(awaited);
                    let cell = std::rc::Rc::new(std::cell::RefCell::new(Some(s.frame)));
                    let gen_f = gobj.clone();
                    let res_f = result.clone();
                    let cell_f = cell.clone();
                    let on_f = self.new_native("", 1, move |vm, _t, args| {
                        if let Some(fr) = cell_f.borrow_mut().take() {
                            let token = fr.trace_token;
                            vm.trace_resume(token);
                            let v = args.get(0).cloned().unwrap_or(Value::Undefined);
                            let flow = vm.resume_frame(fr, v);
                            vm.agen_drive(&gen_f, flow, &res_f, token);
                        }
                        Ok(Value::Undefined)
                    });
                    let gen_r = gobj.clone();
                    let res_r = result.clone();
                    let on_r = self.new_native("", 1, move |vm, _t, args| {
                        if let Some(fr) = cell.borrow_mut().take() {
                            let token = fr.trace_token;
                            vm.trace_resume(token);
                            let e = args.get(0).cloned().unwrap_or(Value::Undefined);
                            let flow = vm.resume_frame_throw(fr, e);
                            vm.agen_drive(&gen_r, flow, &res_r, token);
                        }
                        Ok(Value::Undefined)
                    });
                    self.promise_then(&target, Value::Object(on_f), Value::Object(on_r));
                }
                SuspendKind::YieldStar(_) | SuspendKind::GeneratorStart => {
                    self.trace_exit(token, true);
                    self.set_gen_state(gobj, GeneratorState::Completed);
                    let e = self.throw_type("internal: yield* not desugared");
                    self.reject_promise(result, e);
                    self.agen_complete_step(gobj);
                }
            },
        }
    }

    pub fn generator_resume(
        &mut self,
        gen: &Value,
        kind: ResumeKind,
        value: Value,
    ) -> Result<Value, Value> {
        let gobj = match gen {
            // Must be a *sync* generator; an async generator (or anything else)
            // throws a TypeError (spec 27.5.1.2/.3/.4 brand check).
            Value::Object(o) if matches!(&o.borrow().internal, Internal::Generator(g) if !g.is_async) => {
                o.clone()
            }
            _ => return Err(self.throw_type("not a generator")),
        };
        // Take the frame out, marking executing.
        let taken = {
            let mut b = gobj.borrow_mut();
            if let Internal::Generator(g) = &mut b.internal {
                match std::mem::replace(&mut g.state, GeneratorState::Executing) {
                    GeneratorState::SuspendedStart(f) => Some((f, true)),
                    GeneratorState::SuspendedYield(f) => Some((f, false)),
                    GeneratorState::Completed => None,
                    GeneratorState::Executing => {
                        return Err(self.throw_type("generator is already running"))
                    }
                }
            } else {
                None
            }
        };
        let (frame, is_start) = match taken {
            Some(t) => t,
            None => {
                // Already completed. The `mem::replace` above optimistically set
                // the state to `Executing`; restore `Completed` so subsequent calls
                // don't see a spurious "already running".
                self.set_gen_state(&gobj, GeneratorState::Completed);
                // Completed: next->{undefined,true}, return->{value,true}, throw->throw.
                return match kind {
                    ResumeKind::Throw => Err(value),
                    ResumeKind::Return => Ok(self.make_iter_result(value, true)),
                    ResumeKind::Next => Ok(self.make_iter_result(Value::Undefined, true)),
                };
            }
        };

        // `.return()` on a generator suspended *before its body started* has no
        // `try` in scope, so there is no finally to run: complete immediately.
        if matches!(kind, ResumeKind::Return) && is_start {
            self.set_gen_state(&gobj, GeneratorState::Completed);
            return Ok(self.make_iter_result(value, true));
        }

        let token = frame.trace_token;
        self.trace_resume(token);
        let flow = if is_start {
            // First resume: ignore the sent value (spec).
            match kind {
                ResumeKind::Throw => self.resume_frame_throw(frame, value),
                _ => self.run_frame(*frame),
            }
        } else {
            match kind {
                ResumeKind::Throw => self.resume_frame_throw(frame, value),
                // `.return(v)` resumes with an injected return completion so an
                // enclosing `finally` runs (and a `yield` in that finally can
                // re-suspend, trapping the return) — matching the spec.
                ResumeKind::Return => self.resume_frame_return(frame, value),
                _ => self.resume_frame(frame, value),
            }
        };

        match flow {
            Flow::Return(v) => {
                self.trace_exit(token, false);
                self.set_gen_state(&gobj, GeneratorState::Completed);
                Ok(self.make_iter_result(v, true))
            }
            Flow::Throw(e) => {
                self.trace_exit(token, true);
                self.set_gen_state(&gobj, GeneratorState::Completed);
                Err(e)
            }
            Flow::Suspend(s) => match s.kind {
                SuspendKind::Yield(y) => {
                    self.trace_suspend(token);
                    self.set_gen_state(&gobj, GeneratorState::SuspendedYield(s.frame));
                    Ok(self.make_iter_result(y, false))
                }
                SuspendKind::YieldStar(_) | SuspendKind::GeneratorStart => {
                    // Desugared / consumed earlier; should not occur here.
                    self.trace_exit(token, true);
                    self.set_gen_state(&gobj, GeneratorState::Completed);
                    Err(self.throw_type("internal: yield* not desugared"))
                }
                SuspendKind::Await(_) => {
                    // A sync generator cannot await.
                    self.trace_exit(token, true);
                    self.set_gen_state(&gobj, GeneratorState::Completed);
                    Err(self.throw_type("await in non-async generator"))
                }
            },
        }
    }

    fn set_gen_state(&self, gobj: &JsObject, state: GeneratorState) {
        if let Internal::Generator(g) = &mut gobj.borrow_mut().internal {
            g.state = state;
        }
    }
}
