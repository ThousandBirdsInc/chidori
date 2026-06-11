//! Promise constructor + prototype + combinators, and the generator prototype.

use std::cell::RefCell;
use std::rc::Rc;

use super::{arg, super_target};
use crate::generator::ResumeKind;
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    install_promise(vm);
    install_generator(vm);
}

fn install_promise(vm: &mut Vm) {
    let proto = vm.realm.promise_proto.clone();
    let ctor = vm.new_native_ctor(
        "Promise",
        1,
        |vm, t, args| {
            // Only reachable via `super(...)` from a Promise subclass (the
            // construct handler serves `new Promise`): initialize the subclass
            // instance's promise internals in place, like Set/Map/TypedArray.
            let proto = vm.realm.promise_proto.clone();
            // The target must be an UNinitialized instance (Internal::Ordinary):
            // `Promise.call(existingPromise, ...)` throws rather than re-init.
            let target = super_target(&t, &proto)
                .filter(|o| matches!(o.borrow().internal, Internal::Ordinary))
                .ok_or_else(|| {
                    vm.throw_type("Promise constructor cannot be invoked without 'new'")
                })?;
            let executor = arg(args, 0);
            if !vm.is_callable(&executor) {
                return Err(vm.throw_type("Promise resolver is not a function"));
            }
            target.borrow_mut().internal = Internal::Promise(crate::vm::PromiseData {
                state: crate::vm::PromiseState::Pending,
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                handled: false,
                host_id: None,
            });
            let (resolve, reject) = make_resolving_functions(vm, &target);
            if let Err(e) = vm.call(executor, Value::Undefined, &[resolve, reject]) {
                vm.reject_promise(&target, e);
            }
            Ok(Value::Undefined)
        },
        |vm, _t, args| {
            // 1. If executor is not callable, throw a TypeError.
            let executor = arg(args, 0);
            if !vm.is_callable(&executor) {
                return Err(vm.throw_type("Promise resolver is not a function"));
            }
            // 2. Create the promise and its (idempotent) resolving functions and
            //    run the executor with (resolve, reject). A throw rejects.
            let promise = vm.new_promise();
            let (resolve, reject) = make_resolving_functions(vm, &promise);
            if let Err(e) = vm.call(executor, Value::Undefined, &[resolve, reject]) {
                vm.reject_promise(&promise, e);
            }
            Ok(Value::Object(promise))
        },
    );
    vm.install_ctor("Promise", &ctor, &proto);
    vm.install_species(&ctor);

    // Promise.withResolvers() — { promise, resolve, reject } (ES2024).
    vm.define_method(&ctor, "withResolvers", 0, |vm, _t, _a| {
        let promise = vm.new_promise();
        let (resolve, reject) = make_resolving_functions(vm, &promise);
        let obj = Value::Object(vm.new_object());
        vm.set_prop(&obj, &PropertyKey::str("promise"), Value::Object(promise))?;
        vm.set_prop(&obj, &PropertyKey::str("resolve"), resolve)?;
        vm.set_prop(&obj, &PropertyKey::str("reject"), reject)?;
        Ok(obj)
    });

    // Promise.prototype[Symbol.toStringTag] = "Promise" (non-writable,
    // non-enumerable, configurable) when the engine has Symbol.toStringTag.
    let to_string_tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(to_string_tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Promise"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    vm.define_method(&proto, "then", 2, |vm, this, args| {
        let p = promise_this(vm, &this)?;
        let on_f = arg(args, 0);
        let on_r = arg(args, 1);
        // The result promise is created via SpeciesConstructor(this, %Promise%).
        let default_ctor = vm.get_prop(
            &Value::Object(vm.realm.promise_proto.clone()),
            &PropertyKey::str("constructor"),
        )?;
        let c = promise_species(vm, &this, &default_ctor)?;
        // Fast path: the intrinsic Promise constructor uses the native dependent
        // promise directly (no observable difference, and lower cost).
        if same_value(&c, &default_ctor) {
            return Ok(Value::Object(vm.promise_then(&p, on_f, on_r)));
        }
        // Species path: build the result via NewPromiseCapability(C) and forward
        // the settlement of the native dependent promise into that capability.
        let (promise, resolve, reject) = new_promise_capability(vm, &c)?;
        let native_dep = vm.promise_then(&p, on_f, on_r);
        let res = resolve;
        let fwd_f = vm.new_native("", 1, move |vm, _t, a| {
            vm.call(res.clone(), Value::Undefined, &[arg(a, 0)])?;
            Ok(Value::Undefined)
        });
        let rej = reject;
        let fwd_r = vm.new_native("", 1, move |vm, _t, a| {
            vm.call(rej.clone(), Value::Undefined, &[arg(a, 0)])?;
            Ok(Value::Undefined)
        });
        vm.promise_then(&native_dep, Value::Object(fwd_f), Value::Object(fwd_r));
        Ok(promise)
    });
    vm.define_method(&proto, "catch", 1, |vm, this, args| {
        // catch(onRejected) === Invoke(this, "then", [undefined, onRejected]) —
        // generic over any thenable, and routes through `then`'s species logic.
        let then = vm.get_prop(&this, &PropertyKey::str("then"))?;
        vm.call(then, this.clone(), &[Value::Undefined, arg(args, 0)])
    });
    vm.define_method(&proto, "finally", 1, |vm, this, args| {
        // Spec 27.2.5.3: generic over any object receiver — the result comes
        // from Invoke(this, "then", [thenFinally, catchFinally]), so subclass
        // `then` overrides are observed; the intermediate promise is built via
        // PromiseResolve(SpeciesConstructor(this, %Promise%), …).
        if !matches!(this, Value::Object(_)) {
            return Err(vm.throw_type("Promise.prototype.finally called on a non-object"));
        }
        let on_finally = arg(args, 0);
        // Non-callable onFinally: behaves like then(onFinally, onFinally), which
        // (since neither is callable) passes the value/reason straight through.
        if !vm.is_callable(&on_finally) {
            return invoke_then(vm, &this, on_finally.clone(), on_finally);
        }
        let default_ctor = vm.get_prop(
            &Value::Object(vm.realm.promise_proto.clone()),
            &PropertyKey::str("constructor"),
        )?;
        let c = promise_species(vm, &this, &default_ctor)?;
        // thenFinally(value): call onFinally(), wait for PromiseResolve(C, its
        // result) to settle, then fulfil with the original value. If onFinally
        // throws, that rejection propagates and the value is dropped.
        let f1 = on_finally.clone();
        let c1 = c.clone();
        let on_f = vm.new_native("", 1, move |vm, _t, a| {
            let value = arg(a, 0);
            let result = vm.call(f1.clone(), Value::Undefined, &[])?;
            let p = promise_resolve_with(vm, &c1, result)?;
            let thunk = vm.new_native("", 0, move |_vm, _t, _a| Ok(value.clone()));
            invoke_then_one(vm, &p, Value::Object(thunk))
        });
        // catchFinally(reason): same, then re-throw the original reason.
        let f2 = on_finally;
        let on_r = vm.new_native("", 1, move |vm, _t, a| {
            let reason = arg(a, 0);
            let result = vm.call(f2.clone(), Value::Undefined, &[])?;
            let p = promise_resolve_with(vm, &c, result)?;
            let thrower = vm.new_native("", 0, move |_vm, _t, _a| Err(reason.clone()));
            invoke_then_one(vm, &p, Value::Object(thrower))
        });
        invoke_then(vm, &this, Value::Object(on_f), Value::Object(on_r))
    });

    // Promise.resolve: returns the argument unchanged if it is already a native
    // promise; otherwise wraps it in a new fulfilled (or thenable-tracking)
    // promise.
    vm.define_method(&ctor, "resolve", 1, |vm, this, args| {
        // PromiseResolve(C, x): C = this (must be an Object).
        if !matches!(this, Value::Object(_)) {
            return Err(vm.throw_type("Promise.resolve called on a non-object"));
        }
        promise_resolve_with(vm, &this, arg(args, 0))
    });
    // Promise.reject: C = this; build a capability and call its reject.
    vm.define_method(&ctor, "reject", 1, |vm, this, args| {
        if !matches!(this, Value::Object(_)) {
            return Err(vm.throw_type("Promise.reject called on a non-object"));
        }
        let (promise, _resolve, reject) = new_promise_capability(vm, &this)?;
        vm.call(reject, Value::Undefined, &[arg(args, 0)])?;
        Ok(promise)
    });

    vm.define_method(&ctor, "all", 1, |vm, this, args| {
        // NewPromiseCapability(C) throws synchronously if C is not a constructor.
        let (promise, resolve, reject) = new_promise_capability(vm, &this)?;
        // IfAbruptRejectPromise: a setup/iteration error rejects the capability.
        if let Err(e) =
            perform_promise_all(vm, &this, &arg(args, 0), resolve.clone(), reject.clone())
        {
            let _ = vm.call(reject, Value::Undefined, &[e]);
        }
        Ok(promise)
    });

    vm.define_method(&ctor, "allSettled", 1, |vm, this, args| {
        let (promise, resolve, reject) = new_promise_capability(vm, &this)?;
        if let Err(e) = perform_promise_all_settled(vm, &this, &arg(args, 0), resolve) {
            let _ = vm.call(reject, Value::Undefined, &[e]);
        }
        Ok(promise)
    });

    vm.define_method(&ctor, "race", 1, |vm, this, args| {
        let (promise, resolve, reject) = new_promise_capability(vm, &this)?;
        if let Err(e) = perform_promise_race(vm, &this, &arg(args, 0), resolve, reject.clone()) {
            let _ = vm.call(reject, Value::Undefined, &[e]);
        }
        Ok(promise)
    });

    vm.define_method(&ctor, "any", 1, |vm, this, args| {
        let (promise, resolve, reject) = new_promise_capability(vm, &this)?;
        if let Err(e) = perform_promise_any(vm, &this, &arg(args, 0), resolve, reject.clone()) {
            let _ = vm.call(reject, Value::Undefined, &[e]);
        }
        Ok(promise)
    });
}

/// Test-and-set an element's `alreadyCalled` flag. Returns `true` if the element
/// has already settled (so the caller must bail out), `false` on the first call
/// (and marks it settled). Mirrors the spec's per-element resolve/reject guard so
/// a misbehaving thenable cannot double-count the combinator's pending counter.
fn take_guard(flag: &Rc<RefCell<bool>>) -> bool {
    let mut done = flag.borrow_mut();
    if *done {
        true
    } else {
        *done = true;
        false
    }
}

/// `NewPromiseCapability(C)`: construct a promise via constructor `c` whose
/// executor captures the `resolve`/`reject` functions. Returns
/// `(promise, resolve, reject)`. Throws if `c` is not a constructor or does not
/// hand back callable resolving functions.
fn new_promise_capability(vm: &mut Vm, c: &Value) -> Result<(Value, Value, Value), Value> {
    if !vm.is_constructor(c) {
        return Err(vm.throw_type("Promise capability requires a constructor"));
    }
    let resolve_cell = Rc::new(RefCell::new(Value::Undefined));
    let reject_cell = Rc::new(RefCell::new(Value::Undefined));
    let rc = resolve_cell.clone();
    let jc = reject_cell.clone();
    let executor = vm.new_native("", 2, move |vm, _t, a| {
        if !rc.borrow().is_undefined() || !jc.borrow().is_undefined() {
            return Err(vm.throw_type("Promise executor functions already set"));
        }
        *rc.borrow_mut() = arg(a, 0);
        *jc.borrow_mut() = arg(a, 1);
        Ok(Value::Undefined)
    });
    let promise = vm.construct(c, &[Value::Object(executor)], c)?;
    let resolve = resolve_cell.borrow().clone();
    let reject = reject_cell.borrow().clone();
    if !vm.is_callable(&resolve) || !vm.is_callable(&reject) {
        return Err(vm.throw_type("Promise resolve/reject is not callable"));
    }
    Ok((promise, resolve, reject))
}

/// `SpeciesConstructor(obj, defaultConstructor)` for promises.
fn promise_species(vm: &mut Vm, obj: &Value, default: &Value) -> Result<Value, Value> {
    let c = vm.get_prop(obj, &PropertyKey::str("constructor"))?;
    if c.is_undefined() {
        return Ok(default.clone());
    }
    if !matches!(c, Value::Object(_)) {
        return Err(vm.throw_type("constructor is not an object"));
    }
    let sym = vm.realm.symbol_species.clone();
    let s = vm.get_prop(&c, &PropertyKey::Sym(sym))?;
    if s.is_nullish() {
        return Ok(default.clone());
    }
    if vm.is_constructor(&s) {
        Ok(s)
    } else {
        Err(vm.throw_type("Symbol.species is not a constructor"))
    }
}

/// `PromiseResolve(C, x)`: a promise whose `constructor` is `C` passes through
/// unchanged; anything else is wrapped via NewPromiseCapability(C) + resolve.
fn promise_resolve_with(vm: &mut Vm, c: &Value, x: Value) -> Result<Value, Value> {
    if vm.is_native_promise(&x) {
        let xc = vm.get_prop(&x, &PropertyKey::str("constructor"))?;
        if same_value(&xc, c) {
            return Ok(x);
        }
    }
    let (promise, resolve, _reject) = new_promise_capability(vm, c)?;
    vm.call(resolve, Value::Undefined, &[x])?;
    Ok(promise)
}

/// `GetPromiseResolve(C)`: `Get(C, "resolve")`, which must be callable.
fn get_promise_resolve(vm: &mut Vm, c: &Value) -> Result<Value, Value> {
    let r = vm.get_prop(c, &PropertyKey::str("resolve"))?;
    if !vm.is_callable(&r) {
        return Err(vm.throw_type("Promise.resolve is not callable"));
    }
    Ok(r)
}

/// `Invoke(p, "then", [handler])` — exactly one argument (the spec's
/// finally thunks call `then` unary, observable via `arguments.length`).
fn invoke_then_one(vm: &mut Vm, p: &Value, handler: Value) -> Result<Value, Value> {
    let then = vm.get_prop(p, &PropertyKey::str("then"))?;
    vm.call(then, p.clone(), &[handler])
}

/// `Invoke(p, "then", [onF, onR])`.
fn invoke_then(vm: &mut Vm, p: &Value, on_f: Value, on_r: Value) -> Result<Value, Value> {
    let then = vm.get_prop(p, &PropertyKey::str("then"))?;
    vm.call(then, p.clone(), &[on_f, on_r])
}

fn dec_to_zero(remaining: &Rc<RefCell<usize>>) -> bool {
    let mut r = remaining.borrow_mut();
    *r -= 1;
    *r == 0
}

/// `PerformPromiseAll` (spec 27.2.4.1.2): drive the iterator, mapping each value
/// through `C.resolve` and registering a resolve-element that fills the result
/// array; the shared `reject` settles on any element rejection.
fn perform_promise_all(
    vm: &mut Vm,
    c: &Value,
    iterable: &Value,
    resolve: Value,
    reject: Value,
) -> Result<(), Value> {
    let promise_resolve = get_promise_resolve(vm, c)?;
    let iter = vm.get_iterator(iterable)?;
    let values = Rc::new(RefCell::new(Vec::<Value>::new()));
    let remaining = Rc::new(RefCell::new(1usize));
    let mut index = 0usize;
    loop {
        match vm.iterator_step(&iter)? {
            None => {
                if dec_to_zero(&remaining) {
                    let arr = vm.new_array(values.borrow().clone());
                    vm.call(resolve.clone(), Value::Undefined, &[Value::Object(arr)])?;
                }
                return Ok(());
            }
            Some(value) => {
                values.borrow_mut().push(Value::Undefined);
                let next = vm.call(promise_resolve.clone(), c.clone(), &[value])?;
                let already = Rc::new(RefCell::new(false));
                let (vc, rc, res, idx) =
                    (values.clone(), remaining.clone(), resolve.clone(), index);
                let on_f = vm.new_native("", 1, move |vm, _t, a| {
                    if take_guard(&already) {
                        return Ok(Value::Undefined);
                    }
                    vc.borrow_mut()[idx] = arg(a, 0);
                    if dec_to_zero(&rc) {
                        let arr = vm.new_array(vc.borrow().clone());
                        vm.call(res.clone(), Value::Undefined, &[Value::Object(arr)])?;
                    }
                    Ok(Value::Undefined)
                });
                *remaining.borrow_mut() += 1;
                invoke_then(vm, &next, Value::Object(on_f), reject.clone())?;
                index += 1;
            }
        }
    }
}

/// `PerformPromiseAllSettled`: like `all`, but each element resolves to a
/// `{ status, value | reason }` record and rejections never settle the result.
fn perform_promise_all_settled(
    vm: &mut Vm,
    c: &Value,
    iterable: &Value,
    resolve: Value,
) -> Result<(), Value> {
    let promise_resolve = get_promise_resolve(vm, c)?;
    let iter = vm.get_iterator(iterable)?;
    let values = Rc::new(RefCell::new(Vec::<Value>::new()));
    let remaining = Rc::new(RefCell::new(1usize));
    let mut index = 0usize;
    loop {
        match vm.iterator_step(&iter)? {
            None => {
                if dec_to_zero(&remaining) {
                    let arr = vm.new_array(values.borrow().clone());
                    vm.call(resolve.clone(), Value::Undefined, &[Value::Object(arr)])?;
                }
                return Ok(());
            }
            Some(value) => {
                values.borrow_mut().push(Value::Undefined);
                let next = vm.call(promise_resolve.clone(), c.clone(), &[value])?;
                let already = Rc::new(RefCell::new(false));
                let make_record = |vm: &mut Vm, status: &str, key: &str, v: Value| {
                    let o = vm.new_object();
                    {
                        let mut b = o.borrow_mut();
                        b.props.insert(
                            PropertyKey::str("status"),
                            Property::data(Value::str(status)),
                        );
                        b.props.insert(PropertyKey::str(key), Property::data(v));
                    }
                    Value::Object(o)
                };
                let (vc, rc, res, idx, g) = (
                    values.clone(),
                    remaining.clone(),
                    resolve.clone(),
                    index,
                    already.clone(),
                );
                let on_f = vm.new_native("", 1, move |vm, _t, a| {
                    if take_guard(&g) {
                        return Ok(Value::Undefined);
                    }
                    vc.borrow_mut()[idx] = make_record(vm, "fulfilled", "value", arg(a, 0));
                    if dec_to_zero(&rc) {
                        let arr = vm.new_array(vc.borrow().clone());
                        vm.call(res.clone(), Value::Undefined, &[Value::Object(arr)])?;
                    }
                    Ok(Value::Undefined)
                });
                let (vc, rc, res, idx, g) = (
                    values.clone(),
                    remaining.clone(),
                    resolve.clone(),
                    index,
                    already,
                );
                let on_r = vm.new_native("", 1, move |vm, _t, a| {
                    if take_guard(&g) {
                        return Ok(Value::Undefined);
                    }
                    vc.borrow_mut()[idx] = make_record(vm, "rejected", "reason", arg(a, 0));
                    if dec_to_zero(&rc) {
                        let arr = vm.new_array(vc.borrow().clone());
                        vm.call(res.clone(), Value::Undefined, &[Value::Object(arr)])?;
                    }
                    Ok(Value::Undefined)
                });
                *remaining.borrow_mut() += 1;
                invoke_then(vm, &next, Value::Object(on_f), Value::Object(on_r))?;
                index += 1;
            }
        }
    }
}

/// `PerformPromiseRace`: settle the result with the first element to settle.
fn perform_promise_race(
    vm: &mut Vm,
    c: &Value,
    iterable: &Value,
    resolve: Value,
    reject: Value,
) -> Result<(), Value> {
    let promise_resolve = get_promise_resolve(vm, c)?;
    let iter = vm.get_iterator(iterable)?;
    loop {
        match vm.iterator_step(&iter)? {
            None => return Ok(()),
            Some(value) => {
                let next = vm.call(promise_resolve.clone(), c.clone(), &[value])?;
                invoke_then(vm, &next, resolve.clone(), reject.clone())?;
            }
        }
    }
}

/// `PerformPromiseAny`: resolve with the first fulfilment; if every element
/// rejects, reject with an `AggregateError` of the reasons in order.
fn perform_promise_any(
    vm: &mut Vm,
    c: &Value,
    iterable: &Value,
    resolve: Value,
    reject: Value,
) -> Result<(), Value> {
    let promise_resolve = get_promise_resolve(vm, c)?;
    let iter = vm.get_iterator(iterable)?;
    let errors = Rc::new(RefCell::new(Vec::<Value>::new()));
    let remaining = Rc::new(RefCell::new(1usize));
    let mut index = 0usize;
    loop {
        match vm.iterator_step(&iter)? {
            None => {
                if dec_to_zero(&remaining) {
                    let agg = make_aggregate_error(vm, errors.borrow().clone());
                    vm.call(reject.clone(), Value::Undefined, &[agg])?;
                }
                return Ok(());
            }
            Some(value) => {
                errors.borrow_mut().push(Value::Undefined);
                let next = vm.call(promise_resolve.clone(), c.clone(), &[value])?;
                let already = Rc::new(RefCell::new(false));
                let res = resolve.clone();
                let g_f = already.clone();
                let on_f = vm.new_native("", 1, move |vm, _t, a| {
                    if take_guard(&g_f) {
                        return Ok(Value::Undefined);
                    }
                    vm.call(res.clone(), Value::Undefined, &[arg(a, 0)])?;
                    Ok(Value::Undefined)
                });
                let (ec, rc, rej, idx, g) = (
                    errors.clone(),
                    remaining.clone(),
                    reject.clone(),
                    index,
                    already,
                );
                let on_r = vm.new_native("", 1, move |vm, _t, a| {
                    if take_guard(&g) {
                        return Ok(Value::Undefined);
                    }
                    ec.borrow_mut()[idx] = arg(a, 0);
                    if dec_to_zero(&rc) {
                        let agg = make_aggregate_error(vm, ec.borrow().clone());
                        vm.call(rej.clone(), Value::Undefined, &[agg])?;
                    }
                    Ok(Value::Undefined)
                });
                *remaining.borrow_mut() += 1;
                invoke_then(vm, &next, Value::Object(on_f), Value::Object(on_r))?;
                index += 1;
            }
        }
    }
}

/// Build an `AggregateError` carrying `errors`. Prefers the realm's
/// `AggregateError` constructor (so the result has the correct prototype and
/// `instanceof AggregateError` holds); falls back to a generic Error tagged as
/// an AggregateError if the constructor is unavailable.
fn make_aggregate_error(vm: &mut Vm, errors: Vec<Value>) -> Value {
    let ctor = vm
        .get_prop(
            &Value::Object(vm.realm.global.clone()),
            &PropertyKey::str("AggregateError"),
        )
        .ok();
    if let Some(Value::Object(c)) = ctor {
        if c.borrow().is_callable() {
            let arr = vm.new_array(errors.clone());
            let args = [Value::Object(arr), Value::str("All promises were rejected")];
            let cval = Value::Object(c);
            if let Ok(v) = vm.construct(&cval, &args, &cval) {
                return v;
            }
        }
    }
    // Fallback: a generic error patched to look like an AggregateError (name
    // "AggregateError", own `errors` array, message). Used only if the
    // AggregateError intrinsic is somehow unavailable.
    let agg = vm.make_error(crate::vm::ErrorKind::Error, "All promises were rejected");
    if let Value::Object(o) = &agg {
        let arr = vm.new_array(errors);
        let mut b = o.borrow_mut();
        b.props.insert(
            PropertyKey::str("errors"),
            Property::data(Value::Object(arr)),
        );
        b.props.insert(
            PropertyKey::str("name"),
            Property::builtin(Value::str("AggregateError")),
        );
    }
    agg
}

fn settle_one(
    vm: &mut Vm,
    remaining: &Rc<RefCell<usize>>,
    values: &Rc<RefCell<Vec<Value>>>,
    result: &JsObject,
) {
    let done = {
        let mut r = remaining.borrow_mut();
        *r -= 1;
        *r == 0
    };
    if done {
        let snapshot = values.borrow().clone();
        let arr = vm.new_array(snapshot);
        vm.resolve_promise(result, Value::Object(arr));
    }
}

fn make_resolving_functions(vm: &mut Vm, promise: &JsObject) -> (Value, Value) {
    // A single shared "already resolved" flag so the first call to either
    // resolve or reject wins and subsequent calls are no-ops (spec 27.2.1.3).
    let already = Rc::new(RefCell::new(false));
    let p1 = promise.clone();
    let a1 = already.clone();
    let resolve = vm.new_native("", 1, move |vm, _t, args| {
        let fire = {
            let mut done = a1.borrow_mut();
            if *done {
                false
            } else {
                *done = true;
                true
            }
        };
        if fire {
            vm.resolve_promise(&p1, arg(args, 0));
        }
        Ok(Value::Undefined)
    });
    let p2 = promise.clone();
    let a2 = already;
    let reject = vm.new_native("", 1, move |vm, _t, args| {
        let fire = {
            let mut done = a2.borrow_mut();
            if *done {
                false
            } else {
                *done = true;
                true
            }
        };
        if fire {
            vm.reject_promise(&p2, arg(args, 0));
        }
        Ok(Value::Undefined)
    });
    (Value::Object(resolve), Value::Object(reject))
}

fn promise_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::Promise(_)) => Ok(o.clone()),
        _ => Err(vm.throw_type("Method Promise.prototype called on incompatible receiver")),
    }
}

fn install_generator(vm: &mut Vm) {
    let proto = vm.realm.generator_proto.clone();
    vm.define_method(&proto, "next", 1, |vm, this, args| {
        vm.generator_resume(&this, ResumeKind::Next, arg(args, 0))
    });
    vm.define_method(&proto, "return", 1, |vm, this, args| {
        vm.generator_resume(&this, ResumeKind::Return, arg(args, 0))
    });
    vm.define_method(&proto, "throw", 1, |vm, this, args| {
        vm.generator_resume(&this, ResumeKind::Throw, arg(args, 0))
    });

    // Async generators: next/return/throw return promises of { value, done }.
    let aproto = vm.realm.async_generator_proto.clone();
    vm.define_method(&aproto, "next", 1, |vm, this, args| {
        vm.async_generator_resume(&this, ResumeKind::Next, arg(args, 0))
    });
    vm.define_method(&aproto, "return", 1, |vm, this, args| {
        vm.async_generator_resume(&this, ResumeKind::Return, arg(args, 0))
    });
    vm.define_method(&aproto, "throw", 1, |vm, this, args| {
        vm.async_generator_resume(&this, ResumeKind::Throw, arg(args, 0))
    });
    // [Symbol.asyncIterator]() { return this }
    let async_iter = vm.realm.symbol_async_iterator.clone();
    let self_iter = vm.new_native("[Symbol.asyncIterator]", 0, |_vm, this, _a| Ok(this));
    vm.define_value_sym(&aproto, async_iter, Value::Object(self_iter));
}
