//! Promises, the microtask queue, async-function driving, and the host-effect
//! boundary (`run_jobs_until_blocked`, host-op registration/resolution).
//!
//! Async functions are driven on the same VM as everything else: calling one runs
//! its frame synchronously until the first `await`, then suspends by attaching the
//! (in-memory) frame to the awaited promise as an `AsyncResume` reaction. When the
//! awaited promise settles, a microtask resumes the frame. No VM state is ever
//! serialized — durability is the journal's job (see `replay.rs`).

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::*;
use crate::vm::*;

impl Vm {
    pub fn new_promise(&self) -> JsObject {
        JsObject::new(ObjectData::new(
            Some(self.realm.promise_proto.clone()),
            Internal::Promise(PromiseData {
                state: PromiseState::Pending,
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                handled: false,
                host_id: None,
            }),
        ))
    }

    pub fn is_native_promise(&self, v: &Value) -> bool {
        matches!(v, Value::Object(o) if matches!(o.borrow().internal, Internal::Promise(_)))
    }

    pub(crate) fn promise_state(&self, p: &JsObject) -> PromiseState {
        match &p.borrow().internal {
            Internal::Promise(pd) => pd.state.clone(),
            _ => PromiseState::Pending,
        }
    }

    /// `PromiseResolve`: return `v` if it is a native promise, else a new promise
    /// resolved with `v`.
    pub fn promise_resolve(&mut self, v: Value) -> JsObject {
        if let Value::Object(o) = &v {
            if matches!(o.borrow().internal, Internal::Promise(_)) {
                return o.clone();
            }
        }
        let p = self.new_promise();
        self.resolve_promise(&p, v);
        p
    }

    /// The resolve function of a promise: handles thenables/self-resolution.
    pub fn resolve_promise(&mut self, promise: &JsObject, value: Value) {
        // Already settled? ignore.
        if !matches!(self.promise_state(promise), PromiseState::Pending) {
            return;
        }
        // Self resolution.
        if let Value::Object(o) = &value {
            if o.same(promise) {
                let err = self.throw_type("Chaining cycle detected for promise");
                self.reject_promise(promise, err);
                return;
            }
            // Native promise: chain reactions.
            if matches!(o.borrow().internal, Internal::Promise(_)) {
                let target = promise.clone();
                let t2 = promise.clone();
                self.promise_then_internal(
                    o,
                    Reaction::Then {
                        handler: None,
                        result_capability: target,
                        is_reject: false,
                    },
                    Reaction::Then {
                        handler: None,
                        result_capability: t2,
                        is_reject: true,
                    },
                );
                return;
            }
            // Thenable? schedule a job.
            let then = self
                .get_prop(&value, &PropertyKey::str("then"))
                .unwrap_or(Value::Undefined);
            if self.is_callable(&then) {
                let promise2 = promise.clone();
                let value2 = value.clone();
                let then2 = then.clone();
                self.microtasks.push_back(Microtask::Job(Box::new(move |vm: &mut Vm| {
                    vm.run_thenable_job(&promise2, value2, then2);
                    Ok(())
                })));
                return;
            }
        }
        self.fulfill_promise(promise, value);
    }

    fn run_thenable_job(&mut self, promise: &JsObject, thenable: Value, then: Value) {
        let p1 = promise.clone();
        let p2 = promise.clone();
        let resolve = self.new_native("", 1, move |vm, _this, args| {
            let v = args.get(0).cloned().unwrap_or(Value::Undefined);
            vm.resolve_promise(&p1, v);
            Ok(Value::Undefined)
        });
        let reject = self.new_native("", 1, move |vm, _this, args| {
            let v = args.get(0).cloned().unwrap_or(Value::Undefined);
            vm.reject_promise(&p2, v);
            Ok(Value::Undefined)
        });
        let r = self.call(
            then,
            thenable,
            &[Value::Object(resolve), Value::Object(reject)],
        );
        if let Err(e) = r {
            self.reject_promise(promise, e);
        }
    }

    pub fn fulfill_promise(&mut self, promise: &JsObject, value: Value) {
        let reactions = {
            let mut b = promise.borrow_mut();
            if let Internal::Promise(pd) = &mut b.internal {
                if !matches!(pd.state, PromiseState::Pending) {
                    return;
                }
                pd.state = PromiseState::Fulfilled(value.clone());
                pd.reject_reactions.clear();
                std::mem::take(&mut pd.fulfill_reactions)
            } else {
                return;
            }
        };
        for r in reactions {
            self.enqueue_reaction(r, value.clone());
        }
    }

    pub fn reject_promise(&mut self, promise: &JsObject, reason: Value) {
        let (reactions, had_handler) = {
            let mut b = promise.borrow_mut();
            if let Internal::Promise(pd) = &mut b.internal {
                if !matches!(pd.state, PromiseState::Pending) {
                    return;
                }
                pd.state = PromiseState::Rejected(reason.clone());
                pd.fulfill_reactions.clear();
                let had = !pd.reject_reactions.is_empty() || pd.handled;
                (std::mem::take(&mut pd.reject_reactions), had)
            } else {
                return;
            }
        };
        if !had_handler {
            self.unhandled_rejections.push(reason.clone());
        }
        for r in reactions {
            self.enqueue_reaction(r, reason.clone());
        }
    }

    fn enqueue_reaction(&mut self, reaction: Reaction, argument: Value) {
        self.microtasks
            .push_back(Microtask::Reaction { reaction, argument });
    }

    /// Register `on_fulfill`/`on_reject` reactions on `promise`, firing
    /// immediately (as microtasks) if already settled.
    pub fn promise_then_internal(
        &mut self,
        promise: &JsObject,
        on_fulfill: Reaction,
        on_reject: Reaction,
    ) {
        let state = self.promise_state(promise);
        // Mark handled to suppress unhandled-rejection for this promise.
        if let Internal::Promise(pd) = &mut promise.borrow_mut().internal {
            pd.handled = true;
        }
        match state {
            PromiseState::Pending => {
                if let Internal::Promise(pd) = &mut promise.borrow_mut().internal {
                    pd.fulfill_reactions.push(on_fulfill);
                    pd.reject_reactions.push(on_reject);
                }
            }
            PromiseState::Fulfilled(v) => self.enqueue_reaction(on_fulfill, v),
            PromiseState::Rejected(v) => self.enqueue_reaction(on_reject, v),
        }
    }

    /// `Promise.prototype.then`: register handlers and return the dependent
    /// promise.
    pub fn promise_then(&mut self, promise: &JsObject, on_f: Value, on_r: Value) -> JsObject {
        let result = self.new_promise();
        let on_fulfill = Reaction::Then {
            handler: if self.is_callable(&on_f) { Some(on_f) } else { None },
            result_capability: result.clone(),
            is_reject: false,
        };
        let on_reject = Reaction::Then {
            handler: if self.is_callable(&on_r) { Some(on_r) } else { None },
            result_capability: result.clone(),
            is_reject: true,
        };
        self.promise_then_internal(promise, on_fulfill, on_reject);
        result
    }

    /// A new already-rejected promise.
    pub fn new_rejected(&mut self, reason: Value) -> JsObject {
        let p = self.new_promise();
        self.reject_promise(&p, reason);
        p
    }

    // =====================================================================
    // Async function driving
    // =====================================================================

    pub fn start_async(&mut self, frame: Frame) -> Value {
        let promise = self.new_promise();
        // The trace token (set by the caller before start_async) rides the frame
        // through every suspend/resume; capture it here for the synchronous
        // first segment's terminal flow.
        let token = frame.trace_token;
        let flow = self.run_frame(frame);
        self.dispatch_async_flow(flow, promise.clone(), token);
        Value::Object(promise)
    }

    fn dispatch_async_flow(&mut self, flow: Flow, own_promise: JsObject, token: Option<u64>) {
        match flow {
            Flow::Return(v) => {
                self.trace_exit(token, false);
                self.resolve_promise(&own_promise, v)
            }
            Flow::Throw(e) => {
                self.trace_exit(token, true);
                self.reject_promise(&own_promise, e)
            }
            Flow::Suspend(s) => match s.kind {
                SuspendKind::Await(awaited) => {
                    self.trace_suspend(token);
                    let target = self.promise_resolve(awaited);
                    let cell = Rc::new(RefCell::new(Some(s.frame)));
                    let on_f = Reaction::AsyncResume {
                        frame: cell.clone(),
                        own_promise: own_promise.clone(),
                        is_reject: false,
                    };
                    let on_r = Reaction::AsyncResume {
                        frame: cell,
                        own_promise,
                        is_reject: true,
                    };
                    self.promise_then_internal(&target, on_f, on_r);
                }
                SuspendKind::Yield(_) | SuspendKind::YieldStar(_) | SuspendKind::GeneratorStart => {
                    // async generators are deferred; treat as immediate resolve.
                    self.resolve_promise(&own_promise, Value::Undefined);
                }
            },
        }
    }

    /// Resume a suspended frame with an injected value (await fulfilled).
    pub fn resume_frame(&mut self, mut frame: Box<Frame>, value: Value) -> Flow {
        frame.stack.push(value);
        self.run_frame(*frame)
    }

    /// Resume a suspended frame by raising an injected exception (await rejected).
    pub fn resume_frame_throw(&mut self, mut frame: Box<Frame>, err: Value) -> Flow {
        frame.pending_throw = Some(err);
        self.run_frame(*frame)
    }

    /// Resume a suspended generator frame with an injected `return` completion
    /// (`generator.return(v)`), so any `finally` blocks enclosing the suspended
    /// `yield` run before the frame completes.
    pub fn resume_frame_return(&mut self, mut frame: Box<Frame>, value: Value) -> Flow {
        frame.pending_return = Some(value);
        self.run_frame(*frame)
    }

    // =====================================================================
    // Microtask draining + host boundary
    // =====================================================================

    /// Drain microtasks to quiescence, then report whether we completed or are
    /// blocked on the earliest-registered pending host op.
    pub fn run_jobs_until_blocked(&mut self) -> RunOutcome {
        while let Some(task) = self.microtasks.pop_front() {
            self.run_microtask(task);
        }
        if let Some((id, _)) = self.pending_host.first() {
            return RunOutcome::BlockedOnHost(*id);
        }
        RunOutcome::Completed
    }

    fn run_microtask(&mut self, task: Microtask) {
        match task {
            Microtask::Job(f) => {
                let _ = f(self);
            }
            Microtask::Reaction { reaction, argument } => match reaction {
                Reaction::Then {
                    handler,
                    result_capability,
                    is_reject,
                } => {
                    match handler {
                        Some(h) if self.is_callable(&h) => {
                            match self.call(h, Value::Undefined, &[argument]) {
                                Ok(v) => self.resolve_promise(&result_capability, v),
                                Err(e) => self.reject_promise(&result_capability, e),
                            }
                        }
                        _ => {
                            // Passthrough.
                            if is_reject {
                                self.reject_promise(&result_capability, argument);
                            } else {
                                self.resolve_promise(&result_capability, argument);
                            }
                        }
                    }
                }
                Reaction::AsyncResume {
                    frame,
                    own_promise,
                    is_reject,
                } => {
                    let taken = frame.borrow_mut().take();
                    if let Some(fr) = taken {
                        let token = fr.trace_token;
                        self.trace_resume(token);
                        let flow = if is_reject {
                            self.resume_frame_throw(fr, argument)
                        } else {
                            self.resume_frame(fr, argument)
                        };
                        self.dispatch_async_flow(flow, own_promise, token);
                    }
                }
            },
        }
    }

    // ---- host-effect boundary ----

    /// Register a new pending host operation; returns its id and the promise JS
    /// awaits. The runtime resolves it later (live) or replay supplies the result.
    pub fn register_host_op(&mut self) -> (u64, JsObject) {
        let id = self.next_host_id;
        self.next_host_id += 1;
        let p = self.new_promise();
        if let Internal::Promise(pd) = &mut p.borrow_mut().internal {
            pd.host_id = Some(id);
            pd.handled = true;
        }
        self.pending_host.insert(id, p.clone());
        (id, p)
    }

    pub fn resolve_host_op(&mut self, id: u64, value: Value) {
        if let Some(p) = self.pending_host.shift_remove(&id) {
            self.resolve_promise(&p, value);
        }
    }

    pub fn reject_host_op(&mut self, id: u64, error: Value) {
        if let Some(p) = self.pending_host.shift_remove(&id) {
            self.reject_promise(&p, error);
        }
    }

    pub fn has_pending_host(&self) -> bool {
        !self.pending_host.is_empty()
    }
}
