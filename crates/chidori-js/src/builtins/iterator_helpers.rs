//! The `Iterator` global and the iterator-helpers surface (ES2025 27.1):
//! the abstract `Iterator` constructor, `Iterator.from`, the lazy
//! `Iterator.prototype` helpers (`map`/`filter`/`take`/`drop`/`flatMap`)
//! backed by [`Internal::IteratorHelper`] state machines, and the eager
//! consumers (`reduce`/`toArray`/`forEach`/`some`/`every`/`find`).
//!
//! Helpers follow the spec's generator-object semantics without the
//! generator machinery: a brand-checked `next`/`return` on
//! %IteratorHelperPrototype% drives the underlying iterator record captured
//! at creation (GetIteratorDirect), with the spec's close-on-abrupt and
//! re-entrancy rules (`running`) enforced in the state machine itself.

use super::arg;
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    let iter_proto = vm.realm.iterator_proto.clone();

    // ---- the abstract Iterator constructor ----
    let ctor = vm.new_native_ctor(
        "Iterator",
        0,
        |vm, this, _args| {
            // Reached without `new` — either an erroneous plain call, or a
            // subclass `super()` (the engine models super() as a plain call
            // with the pre-created derived `this`). The latter is the one
            // legitimate case; everything else throws per spec.
            if let Value::Object(o) = &this {
                if proto_chain_contains(o, &vm.realm.iterator_proto) {
                    return Ok(Value::Undefined);
                }
            }
            Err(vm.throw_type("Constructor Iterator requires 'new'"))
        },
        |vm, _t, _args| {
            // Abstract-class check: `new Iterator()` (new.target whose
            // prototype IS %Iterator.prototype%) throws; a subclass or
            // Reflect.construct target creates an ordinary object with the
            // new.target's prototype (taking the stash marks the prototype as
            // handled here, so construct_inner leaves it alone).
            let nt = vm.native_new_target.take();
            let mut proto = vm.realm.iterator_proto.clone();
            if let Some(nt) = &nt {
                let p = vm.get_prop(nt, &PropertyKey::str("prototype"))?;
                if let Value::Object(po) = p {
                    if po.ptr_eq(&vm.realm.iterator_proto) {
                        return Err(
                            vm.throw_type("Abstract class Iterator not directly constructable")
                        );
                    }
                    proto = po;
                }
            }
            Ok(Value::Object(
                vm.alloc(ObjectData::new(Some(proto), Internal::Ordinary)),
            ))
        },
    );
    // Iterator.prototype (non-writable, non-enumerable, non-configurable).
    ctor.borrow_mut().own_insert(
        PropertyKey::str("prototype"),
        Property {
            kind: PropertyKind::Data {
                value: Value::Object(iter_proto.clone()),
                writable: false,
            },
            enumerable: false,
            configurable: false,
        },
    );
    vm.define_value(
        &vm.realm.global.clone(),
        "Iterator",
        Value::Object(ctor.clone()),
    );

    // Iterator.from(O): wrap anything iterable (or a bare iterator) into a
    // proper Iterator instance.
    vm.define_method(&ctor.clone(), "from", 1, |vm, _t, args| {
        let o = arg(args, 0);
        let (iterator, next) = get_iterator_flattenable(vm, &o, true)?;
        // Already an Iterator instance (OrdinaryHasInstance) — pass through.
        if let Value::Object(io) = &iterator {
            if proto_chain_contains(io, &vm.realm.iterator_proto) {
                return Ok(iterator);
            }
        }
        Ok(new_helper(
            vm,
            &vm.realm.wrap_valid_iterator_proto.clone(),
            iterator,
            next,
            HelperKind::Wrap,
        ))
    });

    // ---- Iterator.prototype.constructor / @@toStringTag ----
    // Both are accessor pairs with the spec's "setter that ignores prototype
    // properties": assignment through a subclass instance defines an own
    // property; assignment to %Iterator.prototype% itself throws.
    install_ignoring_accessor(
        vm,
        &iter_proto,
        PropertyKey::str("constructor"),
        "constructor",
        Value::Object(ctor),
    );
    let tag_key = PropertyKey::Sym(vm.realm.symbol_to_string_tag.clone());
    install_ignoring_accessor(
        vm,
        &iter_proto,
        tag_key.clone(),
        "[Symbol.toStringTag]",
        Value::str("Iterator"),
    );

    // ---- %IteratorHelperPrototype% ----
    let helper_proto = vm.realm.iterator_helper_proto.clone();
    vm.define_method(&helper_proto, "next", 0, |vm, this, _a| {
        helper_next(vm, &this)
    });
    vm.define_method(&helper_proto, "return", 0, |vm, this, _a| {
        helper_return(vm, &this)
    });
    helper_proto.borrow_mut().own_insert(
        tag_key.clone(),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Iterator Helper"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    // ---- %WrapForValidIteratorPrototype% ----
    let wrap_proto = vm.realm.wrap_valid_iterator_proto.clone();
    vm.define_method(&wrap_proto, "next", 0, |vm, this, _a| {
        let (iter, next) = wrap_record(vm, &this)?;
        // Spec forwards verbatim: no iterator-result validation here.
        vm.call(next, iter, &[])
    });
    vm.define_method(&wrap_proto, "return", 0, |vm, this, _a| {
        let (iter, _next) = wrap_record(vm, &this)?;
        let ret = vm.get_prop(&iter, &crate::names::key_return())?;
        if ret.is_undefined() || ret.is_null() {
            return Ok(vm.make_iter_result(Value::Undefined, true));
        }
        vm.call(ret, iter, &[])
    });

    // ---- the lazy helpers ----
    vm.define_method(&iter_proto, "map", 1, |vm, this, args| {
        let f = require_callable_closing(vm, &this, args)?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        Ok(new_std_helper(vm, iter, next, HelperKind::Map(f)))
    });
    vm.define_method(&iter_proto, "filter", 1, |vm, this, args| {
        let f = require_callable_closing(vm, &this, args)?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        Ok(new_std_helper(vm, iter, next, HelperKind::Filter(f)))
    });
    vm.define_method(&iter_proto, "flatMap", 1, |vm, this, args| {
        let f = require_callable_closing(vm, &this, args)?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        Ok(new_std_helper(
            vm,
            iter,
            next,
            HelperKind::FlatMap {
                mapper: f,
                inner: None,
            },
        ))
    });
    vm.define_method(&iter_proto, "take", 1, |vm, this, args| {
        let limit = checked_limit(vm, &this, arg(args, 0))?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        Ok(new_std_helper(vm, iter, next, HelperKind::Take(limit)))
    });
    vm.define_method(&iter_proto, "drop", 1, |vm, this, args| {
        let limit = checked_limit(vm, &this, arg(args, 0))?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        Ok(new_std_helper(vm, iter, next, HelperKind::Drop(limit)))
    });

    // ---- the eager consumers ----
    vm.define_method(&iter_proto, "reduce", 1, |vm, this, args| {
        let f = require_callable_closing(vm, &this, args)?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        let mut counter: f64;
        let mut acc: Value;
        if args.len() >= 2 {
            acc = args[1].clone();
            counter = 0.0;
        } else {
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Err(vm.throw_type("Reduce of empty iterator with no initial value"));
            }
            acc = v;
            counter = 1.0;
        }
        loop {
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Ok(acc);
            }
            let fc = f.clone();
            match vm.call(
                fc,
                Value::Undefined,
                &[acc.clone(), v, Value::Number(counter)],
            ) {
                Ok(r) => acc = r,
                Err(e) => return Err(close_with(vm, &iter, e)),
            }
            counter += 1.0;
        }
    });
    vm.define_method(&iter_proto, "toArray", 0, |vm, this, _args| {
        let (iter, next) = get_iterator_direct(vm, &this)?;
        let mut out = Vec::new();
        loop {
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Ok(Value::Object(vm.new_array(out)));
            }
            out.push(v);
        }
    });
    vm.define_method(&iter_proto, "forEach", 1, |vm, this, args| {
        let f = require_callable_closing(vm, &this, args)?;
        let (iter, next) = get_iterator_direct(vm, &this)?;
        let mut counter = 0.0;
        loop {
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Ok(Value::Undefined);
            }
            let fc = f.clone();
            if let Err(e) = vm.call(fc, Value::Undefined, &[v, Value::Number(counter)]) {
                return Err(close_with(vm, &iter, e));
            }
            counter += 1.0;
        }
    });
    vm.define_method(&iter_proto, "some", 1, |vm, this, args| {
        iter_predicate(vm, &this, args, PredicateMode::Some)
    });
    vm.define_method(&iter_proto, "every", 1, |vm, this, args| {
        iter_predicate(vm, &this, args, PredicateMode::Every)
    });
    vm.define_method(&iter_proto, "find", 1, |vm, this, args| {
        iter_predicate(vm, &this, args, PredicateMode::Find)
    });
}

// ---------------------------------------------------------------------------
// shared plumbing
// ---------------------------------------------------------------------------

/// CreateDataProperty on an ordinary receiver: define an own
/// {writable, enumerable, configurable} data property, refusing (false) for
/// a non-configurable existing key or a non-extensible object.
fn create_data_property(o: &JsObject, key: PropertyKey, v: Value) -> bool {
    let mut b = o.borrow_mut();
    match b.own_get(&key).map(|p| p.configurable) {
        Some(false) => false,
        Some(true) => {
            b.own_insert(key, Property::data(v));
            true
        }
        None => {
            if !b.extensible {
                return false;
            }
            b.own_insert(key, Property::data(v));
            true
        }
    }
}

fn proto_chain_contains(o: &JsObject, target: &JsObject) -> bool {
    let mut cur = o.borrow().proto.clone();
    let mut hops = 0;
    while let Some(p) = cur {
        if p.ptr_eq(target) {
            return true;
        }
        hops += 1;
        if hops > 10_000 {
            return false; // cyclic proto chains are rejected elsewhere
        }
        cur = p.borrow().proto.clone();
    }
    false
}

/// Validation prologue shared by the callback-taking methods: `this` must be
/// an Object; a non-callable argument throws a TypeError AFTER closing the
/// receiver (spec: IteratorClose(O, throwCompletion) — `next` is never read).
fn require_callable_closing(vm: &mut Vm, this: &Value, args: &[Value]) -> Result<Value, Value> {
    if !matches!(this, Value::Object(_)) {
        return Err(vm.throw_type("Iterator helper called on non-object"));
    }
    let f = arg(args, 0);
    if !vm.is_callable(&f) {
        let e = vm.throw_type("Iterator helper callback is not a function");
        return Err(close_with(vm, this, e));
    }
    Ok(f)
}

/// GetIteratorDirect: capture {iterator, next} from an object assumed to be
/// an iterator.
fn get_iterator_direct(vm: &mut Vm, o: &Value) -> Result<(Value, Value), Value> {
    if !matches!(o, Value::Object(_)) {
        return Err(vm.throw_type("Iterator helper called on non-object"));
    }
    let next = vm.get_prop(o, &crate::names::key_next())?;
    Ok((o.clone(), next))
}

/// take/drop limit validation, run BEFORE GetIteratorDirect: ToNumber
/// (abrupt closes the receiver), NaN/negative → RangeError that also closes
/// the receiver, then ToIntegerOrInfinity.
fn checked_limit(vm: &mut Vm, iter: &Value, limit: Value) -> Result<f64, Value> {
    if !matches!(iter, Value::Object(_)) {
        return Err(vm.throw_type("Iterator helper called on non-object"));
    }
    let n = match vm.to_number(&limit) {
        Ok(n) => n,
        Err(e) => return Err(close_with(vm, iter, e)),
    };
    if n.is_nan() {
        let e = vm.throw_range("Iterator limit must not be NaN");
        return Err(close_with(vm, iter, e));
    }
    let int = if n.is_infinite() { n } else { n.trunc() };
    // ToIntegerOrInfinity maps -0 to +0; reject anything below zero.
    let int = if int == 0.0 { 0.0 } else { int };
    if int < 0.0 {
        let e = vm.throw_range("Iterator limit must be non-negative");
        return Err(close_with(vm, iter, e));
    }
    Ok(int)
}

/// IteratorClose(iter, throw completion): run `return` for effect, then
/// rethrow the original error. Per spec, EVERY failure inside the close —
/// including a throwing `return` getter — is swallowed in favor of the
/// original error.
fn close_with(vm: &mut Vm, iter: &Value, err: Value) -> Value {
    close_quiet(vm, iter);
    err
}

/// The swallow-everything IteratorClose used on abrupt paths.
fn close_quiet(vm: &mut Vm, iter: &Value) {
    if let Ok(ret) = vm.get_prop(iter, &crate::names::key_return()) {
        if !ret.is_undefined() && !ret.is_null() {
            let _ = vm.call(ret, iter.clone(), &[]);
        }
    }
}

/// IteratorClose(iter, normal completion): errors from reading or calling
/// `return` — and a non-Object result — PROPAGATE (the early-exit paths:
/// some/every/find hits, take exhaustion, helper `return()`).
fn close_propagating(vm: &mut Vm, iter: &Value) -> Result<(), Value> {
    let ret = vm.get_prop(iter, &crate::names::key_return())?;
    if ret.is_undefined() || ret.is_null() {
        return Ok(());
    }
    let res = vm.call(ret, iter.clone(), &[])?;
    if !matches!(res, Value::Object(_)) {
        return Err(vm.throw_type("iterator return result is not an object"));
    }
    Ok(())
}

/// GetIteratorFlattenable: obtain an iterator record from an iterable or a
/// bare iterator. `allow_strings` distinguishes `Iterator.from` (strings
/// iterate) from `flatMap` results (all primitives rejected).
fn get_iterator_flattenable(
    vm: &mut Vm,
    obj: &Value,
    allow_strings: bool,
) -> Result<(Value, Value), Value> {
    match obj {
        Value::Object(_) => {}
        Value::String(_) if allow_strings => {}
        _ => return Err(vm.throw_type("Iterator.from requires an object or string")),
    }
    let sym = PropertyKey::Sym(vm.realm.symbol_iterator.clone());
    let method = vm.get_prop(obj, &sym)?;
    let iterator = if method.is_undefined() || method.is_null() {
        obj.clone()
    } else {
        vm.call(method, obj.clone(), &[])?
    };
    if !matches!(iterator, Value::Object(_)) {
        return Err(vm.throw_type("Iterator.from result is not an object"));
    }
    let next = vm.get_prop(&iterator, &crate::names::key_next())?;
    Ok((iterator, next))
}

/// Allocate a helper object with the given prototype and state.
fn new_helper(vm: &Vm, proto: &JsObject, iter: Value, next: Value, kind: HelperKind) -> Value {
    Value::Object(vm.alloc(ObjectData::new(
        Some(proto.clone()),
        Internal::IteratorHelper(Box::new(IteratorHelperData {
            iter,
            next,
            done: false,
            running: false,
            counter: 0.0,
            kind,
        })),
    )))
}

fn new_std_helper(vm: &Vm, iter: Value, next: Value, kind: HelperKind) -> Value {
    new_helper(
        vm,
        &vm.realm.iterator_helper_proto.clone(),
        iter,
        next,
        kind,
    )
}

/// Brand-check a %WrapForValidIteratorPrototype% receiver and read its record.
fn wrap_record(vm: &mut Vm, this: &Value) -> Result<(Value, Value), Value> {
    if let Value::Object(o) = this {
        if let Internal::IteratorHelper(h) = &o.borrow().internal {
            if matches!(h.kind, HelperKind::Wrap) {
                return Ok((h.iter.clone(), h.next.clone()));
            }
        }
    }
    Err(vm.throw_type("Method called on incompatible receiver"))
}

// ---------------------------------------------------------------------------
// the helper state machine
// ---------------------------------------------------------------------------

/// Borrow the receiver's helper state, brand-checking it (Wrap objects live
/// on the other prototype and are excluded).
fn with_helper<R>(this: &Value, f: impl FnOnce(&mut IteratorHelperData) -> R) -> Option<R> {
    if let Value::Object(o) = this {
        if let Internal::IteratorHelper(h) = &mut o.borrow_mut().internal {
            if !matches!(h.kind, HelperKind::Wrap) {
                return Some(f(h));
            }
        }
    }
    None
}

fn helper_next(vm: &mut Vm, this: &Value) -> Result<Value, Value> {
    // Entry checks + snapshot of what this step needs, under one borrow.
    enum Step {
        Done,
        Run { iter: Value, next: Value },
    }
    let step = with_helper(this, |h| {
        if h.done {
            Ok(Step::Done)
        } else if h.running {
            Err(())
        } else {
            h.running = true;
            Ok(Step::Run {
                iter: h.iter.clone(),
                next: h.next.clone(),
            })
        }
    });
    let step = match step {
        None => return Err(vm.throw_type("Method called on incompatible receiver")),
        Some(Err(())) => return Err(vm.throw_type("Iterator Helper is already running")),
        Some(Ok(s)) => s,
    };
    let (iter, next) = match step {
        Step::Done => return Ok(vm.make_iter_result(Value::Undefined, true)),
        Step::Run { iter, next } => (iter, next),
    };
    let r = helper_next_inner(vm, this, iter, next);
    with_helper(this, |h| {
        h.running = false;
        if !matches!(r, Ok((_, false))) {
            h.done = true;
        }
    });
    match r {
        Ok((v, done)) => Ok(vm.make_iter_result(v, done)),
        Err(e) => Err(e),
    }
}

/// One `next()` step. Returns (value, done). The caller manages the
/// running/done flags; this must NOT hold a borrow of the helper across any
/// `vm.call` (callbacks may re-enter the helper).
fn helper_next_inner(
    vm: &mut Vm,
    this: &Value,
    iter: Value,
    next: Value,
) -> Result<(Value, bool), Value> {
    // Decode the kind up front; per-kind mutable state (counters, inner
    // records) is re-read/written around each callback.
    enum K {
        Map(Value),
        Filter(Value),
        Take,
        Drop,
        FlatMap(Value),
    }
    let k = with_helper(this, |h| match &h.kind {
        HelperKind::Map(f) => K::Map(f.clone()),
        HelperKind::Filter(f) => K::Filter(f.clone()),
        HelperKind::Take(_) => K::Take,
        HelperKind::Drop(_) => K::Drop,
        HelperKind::FlatMap { mapper, .. } => K::FlatMap(mapper.clone()),
        HelperKind::Wrap => unreachable!("wrap objects use the wrap prototype"),
    })
    .expect("brand-checked by helper_next");

    match k {
        K::Map(f) => {
            let (v, done) = vm.iterator_step_value(next, iter.clone())?;
            if done {
                return Ok((Value::Undefined, true));
            }
            let counter = with_helper(this, |h| {
                let c = h.counter;
                h.counter += 1.0;
                c
            })
            .unwrap_or(0.0);
            match vm.call(f, Value::Undefined, &[v, Value::Number(counter)]) {
                Ok(mapped) => Ok((mapped, false)),
                Err(e) => Err(close_with(vm, &iter, e)),
            }
        }
        K::Filter(f) => loop {
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Ok((Value::Undefined, true));
            }
            let counter = with_helper(this, |h| {
                let c = h.counter;
                h.counter += 1.0;
                c
            })
            .unwrap_or(0.0);
            let fc = f.clone();
            match vm.call(fc, Value::Undefined, &[v.clone(), Value::Number(counter)]) {
                Ok(sel) => {
                    if vm.to_boolean(&sel) {
                        return Ok((v, false));
                    }
                }
                Err(e) => return Err(close_with(vm, &iter, e)),
            }
        },
        K::Take => {
            let remaining = with_helper(this, |h| match &mut h.kind {
                HelperKind::Take(r) => {
                    let cur = *r;
                    if cur != 0.0 && cur.is_finite() {
                        *r = cur - 1.0;
                    }
                    cur
                }
                _ => unreachable!(),
            })
            .expect("brand-checked");
            if remaining == 0.0 {
                close_propagating(vm, &iter)?;
                return Ok((Value::Undefined, true));
            }
            let (v, done) = vm.iterator_step_value(next, iter)?;
            if done {
                return Ok((Value::Undefined, true));
            }
            Ok((v, false))
        }
        K::Drop => loop {
            let remaining = with_helper(this, |h| match &mut h.kind {
                HelperKind::Drop(r) => {
                    let cur = *r;
                    if cur > 0.0 && cur.is_finite() {
                        *r = cur - 1.0;
                    }
                    cur
                }
                _ => unreachable!(),
            })
            .expect("brand-checked");
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Ok((Value::Undefined, true));
            }
            if remaining == 0.0 {
                return Ok((v, false));
            }
        },
        K::FlatMap(f) => loop {
            // Drain the live inner iterator first, if any.
            let inner = with_helper(this, |h| match &h.kind {
                HelperKind::FlatMap { inner, .. } => inner.clone(),
                _ => unreachable!(),
            })
            .expect("brand-checked");
            if let Some((in_iter, in_next)) = inner {
                match vm.iterator_step_value(in_next, in_iter) {
                    Ok((v, false)) => return Ok((v, false)),
                    Ok((_, true)) => {
                        with_helper(this, |h| {
                            if let HelperKind::FlatMap { inner, .. } = &mut h.kind {
                                *inner = None;
                            }
                        });
                        continue;
                    }
                    Err(e) => return Err(close_with(vm, &iter, e)),
                }
            }
            // Advance the outer iterator and open the next inner one.
            let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
            if done {
                return Ok((Value::Undefined, true));
            }
            let counter = with_helper(this, |h| {
                let c = h.counter;
                h.counter += 1.0;
                c
            })
            .unwrap_or(0.0);
            let fc = f.clone();
            let mapped = match vm.call(fc, Value::Undefined, &[v, Value::Number(counter)]) {
                Ok(m) => m,
                Err(e) => return Err(close_with(vm, &iter, e)),
            };
            match get_iterator_flattenable(vm, &mapped, false) {
                Ok(rec) => {
                    with_helper(this, |h| {
                        if let HelperKind::FlatMap { inner, .. } = &mut h.kind {
                            *inner = Some(rec.clone());
                        }
                    });
                }
                Err(e) => return Err(close_with(vm, &iter, e)),
            }
        },
    }
}

fn helper_return(vm: &mut Vm, this: &Value) -> Result<Value, Value> {
    enum State {
        AlreadyDone,
        Close { iter: Value, inner: Option<Value> },
    }
    let st = with_helper(this, |h| {
        if h.running {
            return Err(());
        }
        if h.done {
            return Ok(State::AlreadyDone);
        }
        h.done = true;
        let inner = match &h.kind {
            HelperKind::FlatMap {
                inner: Some((i, _)),
                ..
            } => Some(i.clone()),
            _ => None,
        };
        Ok(State::Close {
            iter: h.iter.clone(),
            inner,
        })
    });
    match st {
        None => Err(vm.throw_type("Method called on incompatible receiver")),
        Some(Err(())) => Err(vm.throw_type("Iterator Helper is already running")),
        Some(Ok(State::AlreadyDone)) => Ok(vm.make_iter_result(Value::Undefined, true)),
        Some(Ok(State::Close { iter, inner })) => {
            // Generator-return semantics: unwind closes the inner iterator
            // (flatMap) and then the underlying one.
            if let Some(i) = inner {
                close_quiet(vm, &i);
            }
            close_propagating(vm, &iter)?;
            Ok(vm.make_iter_result(Value::Undefined, true))
        }
    }
}

// ---------------------------------------------------------------------------
// eager predicate consumers (some / every / find)
// ---------------------------------------------------------------------------

enum PredicateMode {
    Some,
    Every,
    Find,
}

fn iter_predicate(
    vm: &mut Vm,
    this: &Value,
    args: &[Value],
    mode: PredicateMode,
) -> Result<Value, Value> {
    let f = require_callable_closing(vm, this, args)?;
    let (iter, next) = get_iterator_direct(vm, this)?;
    let mut counter = 0.0;
    loop {
        let (v, done) = vm.iterator_step_value(next.clone(), iter.clone())?;
        if done {
            return Ok(match mode {
                PredicateMode::Some => Value::Bool(false),
                PredicateMode::Every => Value::Bool(true),
                PredicateMode::Find => Value::Undefined,
            });
        }
        let fc = f.clone();
        let sel = match vm.call(fc, Value::Undefined, &[v.clone(), Value::Number(counter)]) {
            Ok(s) => s,
            Err(e) => return Err(close_with(vm, &iter, e)),
        };
        let truthy = vm.to_boolean(&sel);
        match mode {
            PredicateMode::Some if truthy => {
                close_propagating(vm, &iter)?;
                return Ok(Value::Bool(true));
            }
            PredicateMode::Every if !truthy => {
                close_propagating(vm, &iter)?;
                return Ok(Value::Bool(false));
            }
            PredicateMode::Find if truthy => {
                close_propagating(vm, &iter)?;
                return Ok(v);
            }
            _ => {}
        }
        counter += 1.0;
    }
}

/// The spec's "SetterThatIgnoresPrototypeProperties" accessor pair used for
/// `Iterator.prototype.constructor` and `Iterator.prototype[@@toStringTag]`:
/// the getter returns a fixed value; the setter throws for the home object
/// itself and otherwise defines an own data property on the receiver, so
/// `instance.constructor = x` behaves like ordinary assignment over a
/// writable prototype data property.
fn install_ignoring_accessor(
    vm: &mut Vm,
    home: &JsObject,
    key: PropertyKey,
    name: &str,
    value: Value,
) {
    let getter = {
        let value = value.clone();
        vm.new_native(&format!("get {name}"), 0, move |_vm, _t, _a| {
            Ok(value.clone())
        })
    };
    let setter = {
        let home = home.clone();
        let key = key.clone();
        vm.new_native(&format!("set {name}"), 1, move |vm, this, a| {
            let o = match &this {
                Value::Object(o) => o.clone(),
                _ => return Err(vm.throw_type("Cannot assign to property of primitive")),
            };
            if o.ptr_eq(&home) {
                return Err(vm.throw_type("Cannot assign to read only property"));
            }
            // CreateDataPropertyOrThrow(this, key, v).
            if !create_data_property(&o, key.clone(), arg(a, 0)) {
                return Err(vm.throw_type("Cannot define property"));
            }
            Ok(Value::Undefined)
        })
    };
    home.borrow_mut().own_insert(
        key,
        Property {
            kind: PropertyKind::Accessor {
                get: Some(Value::Object(getter)),
                set: Some(Value::Object(setter)),
            },
            enumerable: false,
            configurable: true,
        },
    );
}
