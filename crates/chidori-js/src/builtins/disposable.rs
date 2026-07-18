//! `DisposableStack` (and `AsyncDisposableStack`) — the explicit-resource-
//! management built-in classes. A stack's internal state lives in a JS array
//! stored under the engine-private `[[DisposableState]]` symbol: element 0 is the
//! disposed flag, elements 1.. are disposer functions (each a 0-arg native
//! closure run, in reverse, by `dispose()`). The symbol key keeps the state out
//! of `getOwnPropertyNames` so a fresh stack has no own string keys.

use super::arg;
use crate::value::*;
use crate::vm::Vm;
use std::cell::RefCell;
use std::rc::Rc;

pub fn install(vm: &mut Vm) {
    install_one(vm, false);
    install_one(vm, true);
}

fn state_key(vm: &Vm) -> PropertyKey {
    PropertyKey::Sym(vm.realm.symbol_disposable_state.clone())
}

/// The internal state array of a (Async)DisposableStack `this`, or a TypeError.
fn stack_state(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    if let Value::Object(o) = this {
        let key = state_key(vm);
        let found = o.borrow().own_get(&key).and_then(|p| match &p.kind {
            PropertyKind::Data {
                value: Value::Object(arr),
                ..
            } if arr.borrow().is_array() => Some(arr.clone()),
            _ => None,
        });
        if let Some(arr) = found {
            return Ok(arr);
        }
    }
    Err(vm.throw_type("receiver is not a DisposableStack"))
}

fn is_disposed(arr: &JsObject) -> bool {
    matches!(
        arr.borrow().internal,
        Internal::Array(ref a) if matches!(a.first(), Some(Value::Bool(true)))
    )
}

fn push_disposer(arr: &JsObject, f: Value) {
    if let Internal::Array(a) = &mut arr.borrow_mut().internal {
        a.push(f);
    }
}

/// Build a fresh stack object carrying an empty (not-disposed) state array.
fn new_stack(vm: &mut Vm, proto: &JsObject) -> JsObject {
    let arr = vm.new_array(vec![Value::Bool(false)]);
    let o = vm.alloc_ordinary(Some(proto.clone()));
    let key = state_key(vm);
    o.borrow_mut()
        .own_insert(key, Property::builtin(Value::Object(arr)));
    o
}

fn install_one(vm: &mut Vm, is_async: bool) {
    let name = if is_async {
        "AsyncDisposableStack"
    } else {
        "DisposableStack"
    };
    let proto = vm.alloc_ordinary(Some(vm.realm.object_proto.clone()));
    let proto_for_ctor = proto.clone();
    let ctor = vm.new_native_ctor(
        name,
        0,
        move |vm, _t, _a| Err(vm.throw_type(&format!("Constructor {name} requires 'new'"))),
        move |vm, _t, _a| Ok(Value::Object(new_stack(vm, &proto_for_ctor))),
    );
    vm.install_ctor(name, &ctor, &proto);

    // `disposed` getter.
    let getter = vm.new_native("get disposed", 0, |vm, this, _a| {
        let arr = stack_state(vm, &this)?;
        Ok(Value::Bool(is_disposed(&arr)))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("disposed"),
        Some(Value::Object(getter)),
        None,
    );

    // use(value): register `value[@@dispose]` (or @@asyncDispose) for disposal.
    let dispose_sym = if is_async {
        vm.realm.symbol_async_dispose.clone()
    } else {
        vm.realm.symbol_dispose.clone()
    };
    {
        let dsym = dispose_sym.clone();
        vm.define_method(&proto, "use", 1, move |vm, this, args| {
            let arr = stack_state(vm, &this)?;
            if is_disposed(&arr) {
                return Err(vm.throw_reference("DisposableStack is already disposed"));
            }
            let value = arg(args, 0);
            if value.is_nullish() {
                if is_async {
                    // A null/undefined ASYNC resource still records a step:
                    // its disposal awaits undefined (one observable tick).
                    let disposer = vm.new_native("", 0, |_vm, _t, _a| Ok(Value::Undefined));
                    push_disposer(&arr, Value::Object(disposer));
                }
                return Ok(value);
            }
            let mut method = vm.get_prop(&value, &PropertyKey::Sym(dsym.clone()))?;
            // GetDisposeMethod hint async-dispose: fall back to @@dispose,
            // wrapped so the sync result is NOT awaited as a thenable.
            let mut sync_fallback = false;
            if is_async && method.is_nullish() {
                let sd = vm.realm.symbol_dispose.clone();
                method = vm.get_prop(&value, &PropertyKey::Sym(sd))?;
                sync_fallback = true;
            }
            if !vm.is_callable(&method) {
                return Err(vm.throw_type("value is not disposable (no @@dispose method)"));
            }
            let v = value.clone();
            let disposer = vm.new_native("", 0, move |vm, _t, _a| {
                let r = vm.call(method.clone(), v.clone(), &[])?;
                Ok(if sync_fallback { Value::Undefined } else { r })
            });
            push_disposer(&arr, Value::Object(disposer));
            Ok(value)
        });
    }

    // adopt(value, onDispose): dispose by calling `onDispose(value)`.
    vm.define_method(&proto, "adopt", 2, |vm, this, args| {
        let arr = stack_state(vm, &this)?;
        if is_disposed(&arr) {
            return Err(vm.throw_reference("DisposableStack is already disposed"));
        }
        let value = arg(args, 0);
        let on_dispose = arg(args, 1);
        if !vm.is_callable(&on_dispose) {
            return Err(vm.throw_type("onDispose is not a function"));
        }
        let v = value.clone();
        let disposer = vm.new_native("", 0, move |vm, _t, _a| {
            vm.call(
                on_dispose.clone(),
                Value::Undefined,
                std::slice::from_ref(&v),
            )
        });
        push_disposer(&arr, Value::Object(disposer));
        Ok(value)
    });

    // defer(onDispose): dispose by calling `onDispose()`.
    vm.define_method(&proto, "defer", 1, |vm, this, args| {
        let arr = stack_state(vm, &this)?;
        if is_disposed(&arr) {
            return Err(vm.throw_reference("DisposableStack is already disposed"));
        }
        let on_dispose = arg(args, 0);
        if !vm.is_callable(&on_dispose) {
            return Err(vm.throw_type("onDispose is not a function"));
        }
        let disposer = vm.new_native("", 0, move |vm, _t, _a| {
            vm.call(on_dispose.clone(), Value::Undefined, &[])
        });
        push_disposer(&arr, Value::Object(disposer));
        Ok(Value::Undefined)
    });

    // move(): transfer this stack's resources to a new stack; dispose this one.
    {
        let proto_m = proto.clone();
        vm.define_method(&proto, "move", 0, move |vm, this, _args| {
            let arr = stack_state(vm, &this)?;
            if is_disposed(&arr) {
                return Err(vm.throw_reference("DisposableStack is already disposed"));
            }
            let new_obj = new_stack(vm, &proto_m);
            // Take this stack's disposers (elements 1..) and mark it disposed.
            let moved: Vec<Value> = if let Internal::Array(a) = &mut arr.borrow_mut().internal {
                let rest = a.split_off(1); // a == [false]
                a.clear();
                a.push(Value::Bool(true)); // a == [true] (disposed)
                rest
            } else {
                Vec::new()
            };
            let new_arr = stack_state(vm, &Value::Object(new_obj.clone()))?;
            if let Internal::Array(a) = &mut new_arr.borrow_mut().internal {
                a.extend(moved); // new_arr == [false, ...moved]
            }
            Ok(Value::Object(new_obj))
        });
    }

    if is_async {
        install_async_dispose(vm, &proto, &dispose_sym);
    } else {
        install_sync_dispose(vm, &proto, &dispose_sym);
    }

    // @@toStringTag
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto
        .borrow_mut()
        .own_insert(PropertyKey::Sym(tag), Property::builtin(Value::str(name)));
}

/// Run the disposers in reverse, chaining failures into a SuppressedError.
fn run_disposers(vm: &mut Vm, disposers: Vec<Value>) -> Result<(), Value> {
    let mut completion: Option<Value> = None;
    for d in disposers.into_iter().rev() {
        if let Err(e) = vm.call(d, Value::Undefined, &[]) {
            completion = Some(match completion.take() {
                None => e,
                Some(prev) => make_suppressed(vm, e, prev),
            });
        }
    }
    match completion {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `new SuppressedError(error, suppressed, "<msg>")` via the global constructor.
fn make_suppressed(vm: &mut Vm, error: Value, suppressed: Value) -> Value {
    let g = Value::Object(vm.realm.global.clone());
    let ctor = match vm.get_prop(&g, &PropertyKey::str("SuppressedError")) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let msg = Value::str("An error was suppressed during disposal.");
    match vm.construct(&ctor, &[error, suppressed, msg], &ctor) {
        Ok(v) => v,
        Err(e) => e,
    }
}

fn install_sync_dispose(vm: &mut Vm, proto: &JsObject, dispose_sym: &JsSymbol) {
    let dispose = vm.new_native("dispose", 0, |vm, this, _a| {
        let arr = stack_state(vm, &this)?;
        if is_disposed(&arr) {
            return Ok(Value::Undefined);
        }
        // Take the disposers and mark disposed.
        let disposers: Vec<Value> = if let Internal::Array(a) = &mut arr.borrow_mut().internal {
            let rest = a.split_off(1);
            a.clear();
            a.push(Value::Bool(true));
            rest
        } else {
            Vec::new()
        };
        run_disposers(vm, disposers)?;
        Ok(Value::Undefined)
    });
    proto.borrow_mut().own_insert(
        PropertyKey::str("dispose"),
        Property::builtin(Value::Object(dispose.clone())),
    );
    // `[Symbol.dispose]` is the same function object as `dispose`.
    proto.borrow_mut().own_insert(
        PropertyKey::Sym(dispose_sym.clone()),
        Property::builtin(Value::Object(dispose)),
    );
}

/// A promise already rejected with `reason`.
fn rejected(vm: &mut Vm, reason: Value) -> Value {
    let p = vm.new_promise();
    vm.reject_promise(&p, reason);
    Value::Object(p)
}

fn install_async_dispose(vm: &mut Vm, proto: &JsObject, dispose_sym: &JsSymbol) {
    // disposeAsync(): run the disposers in reverse registration order,
    // AWAITING each call's result before the next (so a thenable-returning
    // @@asyncDispose holds up later disposals); errors chain through
    // SuppressedError, last error outermost.
    let dispose = vm.new_native("disposeAsync", 0, |vm, this, _a| {
        let arr = match stack_state(vm, &this) {
            Ok(a) => a,
            Err(e) => return Ok(rejected(vm, e)),
        };
        if is_disposed(&arr) {
            return Ok(Value::Object(vm.promise_resolve(Value::Undefined)));
        }
        let disposers: Vec<Value> = if let Internal::Array(a) = &mut arr.borrow_mut().internal {
            let rest = a.split_off(1);
            a.clear();
            a.push(Value::Bool(true));
            rest
        } else {
            Vec::new()
        };
        let result = vm.new_promise();
        // `queue` pops from the END = reverse registration order.
        let queue: Rc<RefCell<Vec<Value>>> = Rc::new(RefCell::new(disposers));
        let completion: Rc<RefCell<Option<Value>>> = Rc::new(RefCell::new(None));
        // The driver calls one disposer, awaits its result, then re-invokes
        // itself (via the shared cell) until the queue is drained.
        let driver_cell: Rc<RefCell<Value>> = Rc::new(RefCell::new(Value::Undefined));
        let dc = driver_cell.clone();
        let q = queue.clone();
        let comp = completion.clone();
        let res_p = result.clone();
        let driver = vm.new_native("", 0, move |vm, _t, _a| {
            let next = q.borrow_mut().pop();
            match next {
                None => {
                    let c = comp.borrow_mut().take();
                    match c {
                        Some(e) => vm.reject_promise(&res_p, e),
                        None => vm.resolve_promise(&res_p, Value::Undefined),
                    }
                }
                Some(d) => {
                    let step = match vm.call(d, Value::Undefined, &[]) {
                        Ok(v) => v,
                        Err(e) => {
                            // Record (suppress-chaining) and keep going.
                            let mut slot = comp.borrow_mut();
                            let chained = match slot.take() {
                                None => e,
                                Some(prev) => {
                                    drop(slot);
                                    let s = make_suppressed(vm, e, prev);
                                    slot = comp.borrow_mut();
                                    s
                                }
                            };
                            *slot = Some(chained);
                            drop(slot);
                            Value::Undefined
                        }
                    };
                    let target = vm.promise_resolve(step);
                    let drive2 = dc.borrow().clone();
                    let on_f = {
                        let drive2 = drive2.clone();
                        vm.new_native("", 1, move |vm, _t, _a| {
                            vm.call(drive2.clone(), Value::Undefined, &[])
                        })
                    };
                    let comp2 = comp.clone();
                    let on_r = vm.new_native("", 1, move |vm, _t, a| {
                        let e = a.first().cloned().unwrap_or(Value::Undefined);
                        let mut slot = comp2.borrow_mut();
                        let chained = match slot.take() {
                            None => e,
                            Some(prev) => {
                                drop(slot);
                                let s = make_suppressed(vm, e, prev);
                                slot = comp2.borrow_mut();
                                s
                            }
                        };
                        *slot = Some(chained);
                        drop(slot);
                        vm.call(drive2.clone(), Value::Undefined, &[])
                    });
                    vm.promise_then(&target, Value::Object(on_f), Value::Object(on_r));
                }
            }
            Ok(Value::Undefined)
        });
        *driver_cell.borrow_mut() = Value::Object(driver.clone());
        if let Err(e) = vm.call(Value::Object(driver), Value::Undefined, &[]) {
            return Ok(rejected(vm, e));
        }
        Ok(Value::Object(result))
    });
    proto.borrow_mut().own_insert(
        PropertyKey::str("disposeAsync"),
        Property::builtin(Value::Object(dispose.clone())),
    );
    proto.borrow_mut().own_insert(
        PropertyKey::Sym(dispose_sym.clone()),
        Property::builtin(Value::Object(dispose)),
    );
}
