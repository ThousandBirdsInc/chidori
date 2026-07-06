//! Array constructor, `Array.prototype`, and the array/shared iterator
//! prototypes.
//!
//! The prototype methods are implemented as the spec's *generic* algorithms:
//! `ToObject(this)`, one `LengthOfArrayLike` read, then per-index
//! `HasProperty`/`Get`/`Set`/`Delete` in spec order. Index arithmetic is `f64`
//! because an array-like `length` ranges up to 2^53-1 (well past `u32`);
//! `elem_key` maps an index to a real array-index key or a string key as
//! appropriate. Every potentially long loop calls `Vm::native_tick` so the
//! opcode budget / interrupt flag bounds hostile `{length: 2**53}` receivers.

use super::arg;
use super::fundamental::{create_data_property_or_throw, is_array_exotic};
use crate::value::*;
use crate::vm::Vm;

/// 2^53 - 1, the spec's maximum array-like length.
const MAX_SAFE_LEN: f64 = 9007199254740991.0;
/// 2^32 - 1, `ArrayCreate`'s maximum array length.
const MAX_ARRAY_LEN: f64 = 4294967295.0;

pub fn install(vm: &mut Vm) {
    install_iterator_protos(vm);
    let proto = vm.realm.array_proto.clone();
    proto.borrow_mut().internal = Internal::Array(Vec::new());

    let ctor = vm.new_native_ctor("Array", 1, array_call, array_call);
    vm.install_ctor("Array", &ctor, &proto);
    vm.install_species(&ctor);

    vm.define_method(&ctor, "isArray", 1, |vm, _t, args| {
        Ok(Value::Bool(spec_is_array(vm, &arg(args, 0))?))
    });
    vm.define_method(&ctor, "of", 0, |vm, t, args| {
        // C = this; A = IsConstructor(C) ? Construct(C, «len») : ArrayCreate(len).
        let len = args.len();
        let a = if vm.is_constructor(&t) {
            vm.construct(&t, &[Value::Number(len as f64)], &t)?
        } else {
            Value::Object(vm.new_array(vec![Value::Hole; len]))
        };
        let ao = match &a {
            Value::Object(o) => o.clone(),
            _ => return Err(vm.throw_type("Array.of: constructor did not return an object")),
        };
        for (k, v) in args.iter().enumerate() {
            create_data_property_or_throw(vm, &ao, &PropertyKey::from_index(k as u32), v.clone())?;
        }
        vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(len as f64))?;
        Ok(a)
    });
    vm.define_method(&ctor, "from", 1, |vm, t, args| {
        let src = arg(args, 0);
        let map_fn = arg(args, 1);
        let has_map = !map_fn.is_undefined();
        if has_map && !vm.is_callable(&map_fn) {
            return Err(vm.throw_type("Array.from: mapFn is not a function"));
        }
        let this_arg = arg(args, 2);
        // C = this; the result is built via the constructor when `this` is one
        // (so `Array.from.call(MySubclass, …)` returns a MySubclass instance).
        let is_ctor = vm.is_constructor(&t);
        // Iterator path constructs with NO arguments; array-like path with «len».
        let new_result = |vm: &mut Vm, len: Option<usize>| -> Result<(Value, JsObject), Value> {
            let a = if is_ctor {
                match len {
                    Some(n) => vm.construct(&t, &[Value::Number(n as f64)], &t)?,
                    None => vm.construct(&t, &[], &t)?,
                }
            } else {
                Value::Object(vm.new_array(vec![Value::Hole; len.unwrap_or(0)]))
            };
            match &a {
                Value::Object(o) => Ok((a.clone(), o.clone())),
                _ => Err(vm.throw_type("Array.from: constructor did not return an object")),
            }
        };
        if is_iterable(vm, &src)? {
            let (a, ao) = new_result(vm, None)?;
            let it = vm.get_iterator(&src)?;
            let mut k: usize = 0;
            loop {
                let val = match vm.iterator_step(&it)? {
                    Some(v) => v,
                    None => break,
                };
                let mapped = if has_map {
                    match vm.call(
                        map_fn.clone(),
                        this_arg.clone(),
                        &[val, Value::Number(k as f64)],
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = vm.iterator_close(&it);
                            return Err(e);
                        }
                    }
                } else {
                    val
                };
                if let Err(e) = create_data_property_or_throw(
                    vm,
                    &ao,
                    &PropertyKey::from_index(k as u32),
                    mapped,
                ) {
                    let _ = vm.iterator_close(&it);
                    return Err(e);
                }
                k += 1;
                if k > crate::value::MAX_DENSE_ARRAY {
                    let _ = vm.iterator_close(&it);
                    return Err(vm.throw_range("array length exceeds engine limit"));
                }
            }
            vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(k as f64))?;
            Ok(a)
        } else {
            // Array-like: read `length`, then Get each index live. Cap the length
            // to the engine's documented dense-storage bound.
            let o = vm.to_object(&src)?;
            let ov = Value::Object(o);
            let len = get_length(vm, &ov)?;
            if len > crate::value::MAX_DENSE_ARRAY {
                return Err(vm.throw_range("array-like length exceeds engine limit"));
            }
            let (a, ao) = new_result(vm, Some(len))?;
            for k in 0..len {
                let kv = vm.get_prop(&ov, &PropertyKey::from_index(k as u32))?;
                let mapped = if has_map {
                    vm.call(
                        map_fn.clone(),
                        this_arg.clone(),
                        &[kv, Value::Number(k as f64)],
                    )?
                } else {
                    kv
                };
                create_data_property_or_throw(vm, &ao, &PropertyKey::from_index(k as u32), mapped)?;
            }
            vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(len as f64))?;
            Ok(a)
        }
    });

    install_proto_methods(vm, &proto);
    install_unscopables(vm, &proto);
    // Pin the canonical `push` for the kernel `ArrayPush` fast path (entry
    // identity check + bail-shape reconstruction; see `KOp::ArrayPush`).
    let push_fn = proto
        .borrow()
        .props
        .get(&PropertyKey::str("push"))
        .and_then(|p| p.value().cloned());
    if let Some(Value::Object(o)) = push_fn {
        vm.realm.array_push = Some(o);
    }
}

fn array_call(vm: &mut Vm, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if args.len() == 1 {
        if let Value::Number(n) = args[0] {
            let len = n as u32;
            if (len as f64) != n {
                return Err(vm.throw_range("Invalid array length"));
            }
            // Dense storage: refuse to eagerly allocate pathologically large
            // arrays (a conformance gap vs. sparse arrays, but bounds memory).
            if len as usize > crate::value::MAX_DENSE_ARRAY {
                return Err(vm.throw_range("Array allocation exceeds engine limit"));
            }
            // `new Array(n)` / `Array(n)` yields n holes, not n undefineds.
            return Ok(Value::Object(vm.new_array(vec![Value::Hole; len as usize])));
        }
    }
    Ok(Value::Object(vm.new_array(args.to_vec())))
}

/// Spec `IsArray` (7.2.2): an Array exotic, or a Proxy whose (transitive)
/// target is one. A revoked proxy throws a TypeError.
fn spec_is_array(vm: &Vm, v: &Value) -> Result<bool, Value> {
    match v {
        Value::Object(o) => is_array_exotic(vm, o),
        _ => Ok(false),
    }
}

/// `IsConcatSpreadable(O)`: object whose `@@isConcatSpreadable` (if defined)
/// is truthy, else any Array (proxy-aware). Non-objects are never spreadable.
fn is_concat_spreadable(vm: &mut Vm, v: &Value) -> Result<bool, Value> {
    if !matches!(v, Value::Object(_)) {
        return Ok(false);
    }
    let sym = vm.realm.symbol_is_concat_spreadable.clone();
    let spread = vm.get_prop(v, &PropertyKey::Sym(sym))?;
    if !spread.is_undefined() {
        return Ok(vm.to_boolean(&spread));
    }
    spec_is_array(vm, v)
}

fn is_iterable(vm: &mut Vm, v: &Value) -> Result<bool, Value> {
    if v.is_nullish() {
        return Ok(false);
    }
    let sym = vm.realm.symbol_iterator.clone();
    let m = vm.get_prop(v, &PropertyKey::Sym(sym))?;
    Ok(vm.is_callable(&m))
}

fn get_length(vm: &mut Vm, o: &Value) -> Result<usize, Value> {
    let l = vm.get_prop(o, &PropertyKey::str("length"))?;
    vm.to_length(&l)
}

/// `LengthOfArrayLike(O)` as `f64` (array-like lengths range up to 2^53-1).
fn length_of(vm: &mut Vm, ov: &Value) -> Result<f64, Value> {
    let l = vm.get_prop(ov, &PropertyKey::str("length"))?;
    Ok(vm.to_length(&l)? as f64)
}

/// Get the dense element vector of an array `this`, or error.
fn dense(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if o.borrow().is_array() => Ok(o.clone()),
        Value::Object(o) => Ok(o.clone()),
        _ => {
            let o = vm.to_object(this)?;
            Ok(o)
        }
    }
}

/// `CreateDataPropertyOrThrow(A, key, v)` on a result value known to be an
/// object (species-created result arrays). Define-semantics, NOT Set: a
/// poisoned `Array.prototype` setter or inherited read-only index must not be
/// consulted when a builtin fills its freshly created result.
fn create_data_on(vm: &mut Vm, target: &Value, key: &PropertyKey, v: Value) -> Result<(), Value> {
    let o = match target {
        Value::Object(o) => o.clone(),
        _ => return Err(vm.throw_type("result is not an object")),
    };
    create_data_property_or_throw(vm, &o, key, v)
}

/// The `HasProperty(O, k) ? Some(Get(O, k)) : None` step every array
/// iteration builtin performs per element, with a dense fast path: an
/// in-bounds non-hole element of an unshadowed dense array is present and
/// IS the value — no idx→String key, no hashing, no prototype walk. Holes,
/// out-of-bounds indices, shadowed elements, and array-likes take the exact
/// spec sequence.
fn has_get_elem(vm: &mut Vm, base: &Value, idx: f64) -> Result<Option<Value>, Value> {
    if idx >= 0.0 && idx <= u32::MAX as f64 {
        if let Value::Object(o) = base {
            let b = o.borrow();
            if let Internal::Array(arr) = &b.internal {
                if b.props.is_empty() {
                    if let Some(v) = arr.get(idx as usize) {
                        if !matches!(v, Value::Hole) {
                            return Ok(Some(v.clone()));
                        }
                    }
                }
            }
        }
    }
    let key = elem_key(idx);
    if vm.has_prop(base, &key)? {
        Ok(Some(vm.get_prop(base, &key)?))
    } else {
        Ok(None)
    }
}

/// `CreateDataPropertyOrThrow(O, ToString(k), V)` for the result arrays the
/// iteration builtins fill: dense in-place write / exact append fast path,
/// spec path otherwise (see `create_data_index`).
fn create_data_elem(vm: &mut Vm, target: &Value, idx: f64, v: Value) -> Result<(), Value> {
    let o = match target {
        Value::Object(o) => o.clone(),
        _ => return Err(vm.throw_type("result is not an object")),
    };
    if idx >= 0.0 && idx <= u32::MAX as f64 {
        return super::fundamental::create_data_index(vm, &o, idx as u32, v);
    }
    create_data_property_or_throw(vm, &o, &elem_key(idx), v)
}

/// `Set(O, ToString(k), V, Throw=true)` for the mutating builtins' write-back
/// loops, with the dense fast path of `Op::SetPropDynamic`: overwriting an
/// EXISTING in-bounds non-hole element of an unshadowed dense array in place.
/// Such a slot is a plain writable data property (per the array exotic
/// [[Set]]), so no setter, no length change, and no extensibility interaction
/// is observable. Appends, holes, out-of-bounds indices, shadowed elements,
/// and array-likes take the exact spec path.
fn set_elem(vm: &mut Vm, base: &Value, idx: f64, v: Value) -> Result<(), Value> {
    if idx >= 0.0 && idx <= u32::MAX as f64 {
        if let Value::Object(o) = base {
            let mut b = o.borrow_mut();
            if b.props.is_empty() {
                if let Internal::Array(arr) = &mut b.internal {
                    if let Some(slot) = arr.get_mut(idx as usize) {
                        if !matches!(slot, Value::Hole) {
                            *slot = v;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    vm.set_prop_strict(base, &elem_key(idx), v)
}

/// `DeletePropertyOrThrow(O, key)`: a failed delete (non-configurable property)
/// raises a TypeError instead of silently succeeding.
fn delete_or_throw(vm: &mut Vm, o: &Value, key: &PropertyKey) -> Result<(), Value> {
    if !vm.delete_prop(o, key)? {
        return Err(vm.throw_type("Cannot delete property"));
    }
    Ok(())
}

/// A property key for an array index, falling back to a string key for indices
/// beyond the 32-bit range (array-likes can carry a `length` up to 2^53-1).
fn elem_key(i: f64) -> PropertyKey {
    if i >= 0.0 && i <= u32::MAX as f64 {
        PropertyKey::from_index(i as u32)
    } else {
        PropertyKey::str((i as u64).to_string())
    }
}

/// `ArrayCreate(len)`-equivalent for a plain dense result array: the spec
/// RangeError beyond 2^32-1, the engine's dense-storage RangeError beyond its
/// cap.
fn array_create(vm: &mut Vm, len: f64) -> Result<JsObject, Value> {
    if len > MAX_ARRAY_LEN {
        return Err(vm.throw_range("Invalid array length"));
    }
    if len > crate::value::MAX_DENSE_ARRAY as f64 {
        return Err(vm.throw_range("array length exceeds engine limit"));
    }
    Ok(vm.new_array(vec![Value::Hole; len as usize]))
}

fn install_proto_methods(vm: &mut Vm, proto: &JsObject) {
    vm.define_method(proto, "push", 1, |vm, this, args| {
        // Dense fast path: an UNSHADOWED (`props` empty — no reified length
        // marker, no index accessors, not frozen/sealed), extensible dense
        // array appends straight onto the backing vec — no per-element
        // index→String key or generic Set machinery. Appending CREATES
        // properties, so the spec consults the prototype chain: the walk is
        // one cheap reified-index probe per proto (`protos_allow_index_create`
        // — a polluted chain declines to the generic path, which fires the
        // proto accessor). The dense-storage bound also falls back to the
        // generic path, which owns that RangeError.
        if let Value::Object(o) = &this {
            let start = {
                let b = o.borrow();
                if b.props.is_empty() && b.extensible {
                    match &b.internal {
                        Internal::Array(arr)
                            if arr.len() + args.len() <= crate::value::MAX_DENSE_ARRAY =>
                        {
                            Some((arr.len() as u32, b.proto.clone()))
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            };
            if let Some((start, proto)) = start {
                if crate::value::protos_allow_index_create(proto, start, args.len() as u32) {
                    // No user code ran since the guard borrow; the conditions
                    // still hold.
                    if let Internal::Array(arr) = &mut o.borrow_mut().internal {
                        arr.extend_from_slice(args);
                        return Ok(Value::Number(arr.len() as f64));
                    }
                }
            }
        }
        // Spec-generic: Set(O, len+i, arg, throw); Set(O, "length", …, throw). The
        // throwing Set surfaces a frozen array / non-writable length as a
        // TypeError, and the 2^53-1 guard runs before any element is written.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let mut len = length_of(vm, &ov)?;
        let argc = args.len() as f64;
        if len + argc > MAX_SAFE_LEN {
            return Err(vm.throw_type("push would exceed the maximum safe integer length"));
        }
        for v in args {
            vm.set_prop_strict(&ov, &elem_key(len), v.clone())?;
            len += 1.0;
        }
        vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(len))?;
        Ok(Value::Number(len))
    });
    vm.define_method(proto, "pop", 0, |vm, this, _args| {
        // Dense fast path mirroring push's: on an unshadowed extensible dense
        // array the Get/Delete/Set-length sequence collapses to `vec.pop`. A
        // trailing HOLE takes the generic path (its Get consults the
        // prototype chain), as does an empty array's length write-back.
        if let Value::Object(o) = &this {
            let mut b = o.borrow_mut();
            if b.props.is_empty() && b.extensible {
                if let Internal::Array(arr) = &mut b.internal {
                    match arr.last() {
                        Some(v) if !matches!(v, Value::Hole) => {
                            return Ok(arr.pop().expect("checked non-empty"));
                        }
                        None => return Ok(Value::Undefined),
                        _ => {}
                    }
                }
            }
        }
        // Spec-generic: Get the last element, DeletePropertyOrThrow it, then set
        // the (throwing) length — so a frozen/non-writable receiver throws.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        if len == 0.0 {
            vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(0.0))?;
            return Ok(Value::Undefined);
        }
        let idx = len - 1.0;
        let key = elem_key(idx);
        let elem = vm.get_prop(&ov, &key)?;
        delete_or_throw(vm, &ov, &key)?;
        vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(idx))?;
        Ok(elem)
    });
    vm.define_method(proto, "shift", 0, |vm, this, _args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        if len == 0.0 {
            vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(0.0))?;
            return Ok(Value::Undefined);
        }
        let first = vm.get_prop(&ov, &PropertyKey::from_index(0))?;
        let mut k = 1.0;
        while k < len {
            vm.native_tick()?;
            let from = elem_key(k);
            let to = elem_key(k - 1.0);
            if vm.has_prop(&ov, &from)? {
                let v = vm.get_prop(&ov, &from)?;
                vm.set_prop_strict(&ov, &to, v)?;
            } else {
                delete_or_throw(vm, &ov, &to)?;
            }
            k += 1.0;
        }
        delete_or_throw(vm, &ov, &elem_key(len - 1.0))?;
        vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(len - 1.0))?;
        Ok(first)
    });
    vm.define_method(proto, "unshift", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let argc = args.len() as f64;
        if argc > 0.0 {
            if len + argc > MAX_SAFE_LEN {
                return Err(vm.throw_type("unshift would exceed the maximum safe integer length"));
            }
            // Shift existing elements up by argc (high-to-low to avoid clobbering).
            let mut k = len;
            while k > 0.0 {
                vm.native_tick()?;
                let from = elem_key(k - 1.0);
                let to = elem_key(k - 1.0 + argc);
                if vm.has_prop(&ov, &from)? {
                    let v = vm.get_prop(&ov, &from)?;
                    vm.set_prop_strict(&ov, &to, v)?;
                } else {
                    delete_or_throw(vm, &ov, &to)?;
                }
                k -= 1.0;
            }
            for (i, v) in args.iter().enumerate() {
                vm.set_prop_strict(&ov, &PropertyKey::from_index(i as u32), v.clone())?;
            }
        }
        let new_len = len + argc;
        vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(new_len))?;
        Ok(Value::Number(new_len))
    });
    vm.define_method(proto, "at", 1, |vm, this, args| {
        // Generic: length + indexed access (works on array-likes).
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(Value::Undefined);
        }
        vm.get_prop(&ov, &elem_key(k))
    });
    vm.define_method(proto, "slice", 2, |vm, this, args| {
        // Generic: works on array-likes (reads length + indexed elements). For a
        // dense array `get_prop` still hits the backing vec, so this stays fast.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let mut k = rel_index(vm, &arg(args, 0), len, 0.0)?;
        let fin = rel_index(vm, &arg(args, 1), len, len)?;
        let count = (fin - k).max(0.0);
        let a = array_species_create(vm, &ov, count)?;
        let mut n = 0.0;
        while k < fin {
            vm.native_tick()?;
            let key = elem_key(k);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                create_data_on(vm, &a, &elem_key(n), v)?;
            }
            n += 1.0;
            k += 1.0;
        }
        vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(n))?;
        Ok(a)
    });
    vm.define_method(proto, "splice", 2, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let actual_start = rel_index(vm, &arg(args, 0), len, 0.0)?;
        let actual_delete = if args.is_empty() {
            0.0
        } else if args.len() == 1 {
            len - actual_start
        } else {
            let dc = to_integer_or_infinity(vm, &arg(args, 1))?;
            dc.clamp(0.0, len - actual_start)
        };
        let inserts: Vec<Value> = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            Vec::new()
        };
        let ins = inserts.len() as f64;
        // Spec step 8: the post-splice length may not exceed 2^53-1.
        if len + ins - actual_delete > MAX_SAFE_LEN {
            return Err(vm.throw_type("splice would exceed the maximum safe integer length"));
        }
        // A = ArraySpeciesCreate(O, actualDeleteCount), filled with the removed
        // values via Get (so getters fire and holes stay holes).
        let a = array_species_create(vm, &ov, actual_delete)?;
        let mut k = 0.0;
        while k < actual_delete {
            vm.native_tick()?;
            let from = elem_key(actual_start + k);
            if vm.has_prop(&ov, &from)? {
                let v = vm.get_prop(&ov, &from)?;
                create_data_on(vm, &a, &elem_key(k), v)?;
            }
            k += 1.0;
        }
        vm.set_prop_strict(
            &a,
            &PropertyKey::str("length"),
            Value::Number(actual_delete),
        )?;
        // Shift the tail of O to make room for / close the gap left by inserts.
        if ins < actual_delete {
            let mut k = actual_start;
            while k < len - actual_delete {
                vm.native_tick()?;
                let from = elem_key(k + actual_delete);
                let to = elem_key(k + ins);
                if vm.has_prop(&ov, &from)? {
                    let v = vm.get_prop(&ov, &from)?;
                    vm.set_prop_strict(&ov, &to, v)?;
                } else {
                    delete_or_throw(vm, &ov, &to)?;
                }
                k += 1.0;
            }
            let mut k = len;
            while k > len - actual_delete + ins {
                vm.native_tick()?;
                delete_or_throw(vm, &ov, &elem_key(k - 1.0))?;
                k -= 1.0;
            }
        } else if ins > actual_delete {
            let mut k = len - actual_delete;
            while k > actual_start {
                vm.native_tick()?;
                let from = elem_key(k + actual_delete - 1.0);
                let to = elem_key(k + ins - 1.0);
                if vm.has_prop(&ov, &from)? {
                    let v = vm.get_prop(&ov, &from)?;
                    vm.set_prop_strict(&ov, &to, v)?;
                } else {
                    delete_or_throw(vm, &ov, &to)?;
                }
                k -= 1.0;
            }
        }
        for (i, v) in inserts.into_iter().enumerate() {
            vm.set_prop_strict(&ov, &elem_key(actual_start + i as f64), v)?;
        }
        vm.set_prop_strict(
            &ov,
            &PropertyKey::str("length"),
            Value::Number(len - actual_delete + ins),
        )?;
        Ok(a)
    });
    vm.define_method(proto, "concat", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        // A = ArraySpeciesCreate(O, 0). items = [O, ...args]; each is spread if
        // IsConcatSpreadable, else appended as a single element. Absent indices of
        // a spreadable leave a hole in A (so its length is preserved).
        let a = array_species_create(vm, &ov, 0.0)?;
        let mut n = 0.0;
        let mut items: Vec<Value> = Vec::with_capacity(args.len() + 1);
        items.push(ov);
        items.extend(args.iter().cloned());
        for e in items {
            if is_concat_spreadable(vm, &e)? {
                let len = length_of(vm, &e)?;
                if n + len > MAX_SAFE_LEN {
                    return Err(
                        vm.throw_type("concat would exceed the maximum safe integer length")
                    );
                }
                let mut k = 0.0;
                while k < len {
                    vm.native_tick()?;
                    let key = elem_key(k);
                    if vm.has_prop(&e, &key)? {
                        let v = vm.get_prop(&e, &key)?;
                        create_data_on(vm, &a, &elem_key(n), v)?;
                    }
                    n += 1.0;
                    k += 1.0;
                }
            } else {
                if n >= MAX_SAFE_LEN {
                    return Err(
                        vm.throw_type("concat would exceed the maximum safe integer length")
                    );
                }
                create_data_on(vm, &a, &elem_key(n), e)?;
                n += 1.0;
            }
        }
        vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(n))?;
        Ok(a)
    });
    vm.define_method(proto, "join", 1, |vm, this, args| {
        // Spec order: ToObject, LengthOfArrayLike, THEN coerce the separator
        // (whose side effects may mutate the array), then Get + ToString each
        // element. ToString errors propagate (no lossy fallback).
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let sep_v = arg(args, 0);
        let sep = if sep_v.is_undefined() {
            ",".to_string()
        } else {
            vm.to_js_string(&sep_v)?.as_str().to_string()
        };
        let mut r = String::new();
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if k > 0.0 {
                r.push_str(&sep);
            }
            let element = vm.get_prop(&ov, &elem_key(k))?;
            // null/undefined (and holes, which Get reads as undefined unless the
            // prototype supplies a value) stringify to the empty string.
            if !element.is_nullish() {
                r.push_str(vm.to_js_string(&element)?.as_str());
            }
            k += 1.0;
        }
        Ok(Value::str(r))
    });
    vm.define_method(proto, "toString", 0, |vm, this, _a| {
        let array = vm.to_object(&this)?;
        let av = Value::Object(array);
        let join = vm.get_prop(&av, &PropertyKey::str("join"))?;
        if vm.is_callable(&join) {
            vm.call(join, av, &[])
        } else {
            // Non-callable `join`: the spec falls back to the INTRINSIC
            // %Object.prototype.toString% (even if that property was deleted).
            super::fundamental::object_to_string(vm, &av)
        }
    });
    vm.define_method(proto, "toLocaleString", 0, |vm, this, _a| {
        // Join each element's `ToString(Invoke(element, "toLocaleString"))` with
        // ",". null/undefined elements (and holes) contribute the empty string.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let mut out = String::new();
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if k > 0.0 {
                out.push(',');
            }
            let v = vm.get_prop(&ov, &elem_key(k))?;
            k += 1.0;
            if v.is_nullish() {
                continue;
            }
            let f = vm.get_prop(&v, &PropertyKey::str("toLocaleString"))?;
            let r = vm.call(f, v, &[])?;
            out.push_str(vm.to_js_string(&r)?.as_str());
        }
        Ok(Value::str(out))
    });
    vm.define_method(proto, "indexOf", 1, |vm, this, args| {
        // Generic + spec-ordered: read length, then HasProperty/Get per index so
        // holes are skipped and a live getter is observed (the `-8-*` tests).
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let target = arg(args, 0);
        if len == 0.0 {
            return Ok(Value::Number(-1.0));
        }
        let n = to_integer_or_infinity(vm, &arg(args, 1))?;
        if n == f64::INFINITY {
            return Ok(Value::Number(-1.0));
        }
        // `n + 0.0` normalizes a fromIndex of -0 (the found index is returned).
        let mut k = if n == f64::NEG_INFINITY {
            0.0
        } else if n >= 0.0 {
            n + 0.0
        } else {
            (len + n).max(0.0)
        };
        while k < len {
            vm.native_tick()?;
            let key = elem_key(k);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                if vm.strict_equals(&v, &target) {
                    return Ok(Value::Number(k));
                }
            }
            k += 1.0;
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "lastIndexOf", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let target = arg(args, 0);
        if len == 0.0 {
            return Ok(Value::Number(-1.0));
        }
        // Default fromIndex is len-1; otherwise ToIntegerOrInfinity.
        let mut k = if args.len() >= 2 {
            let n = to_integer_or_infinity(vm, &arg(args, 1))?;
            if n == f64::NEG_INFINITY {
                return Ok(Value::Number(-1.0));
            }
            if n >= 0.0 {
                // `+ 0.0` normalizes a fromIndex of -0.
                n.min(len - 1.0) + 0.0
            } else {
                len + n
            }
        } else {
            len - 1.0
        };
        while k >= 0.0 {
            vm.native_tick()?;
            let key = elem_key(k);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                if vm.strict_equals(&v, &target) {
                    return Ok(Value::Number(k));
                }
            }
            k -= 1.0;
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "includes", 1, |vm, this, args| {
        // One length read, then Get at EVERY index (holes read as undefined;
        // values are never cached — a getter installed mid-scan is observed).
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let target = arg(args, 0);
        if len == 0.0 {
            return Ok(Value::Bool(false));
        }
        let n = to_integer_or_infinity(vm, &arg(args, 1))?;
        if n == f64::INFINITY {
            return Ok(Value::Bool(false));
        }
        let mut k = if n == f64::NEG_INFINITY {
            0.0
        } else if n >= 0.0 {
            n
        } else {
            (len + n).max(0.0)
        };
        while k < len {
            vm.native_tick()?;
            let v = vm.get_prop(&ov, &elem_key(k))?;
            if same_value_zero(&v, &target) {
                return Ok(Value::Bool(true));
            }
            k += 1.0;
        }
        Ok(Value::Bool(false))
    });
    vm.define_method(proto, "forEach", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &this_arg,
                    &[v, Value::Number(k), ov.clone()],
                )?;
            }
            k += 1.0;
        }
        Ok(Value::Undefined)
    });
    vm.define_method(proto, "map", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        // Result via ArraySpeciesCreate; holes map to holes (the callback is not
        // invoked for an absent index and that output slot stays absent).
        let a = array_species_create(vm, &ov, len)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                let mapped = call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &this_arg,
                    &[v, Value::Number(k), ov.clone()],
                )?;
                create_data_elem(vm, &a, k, mapped)?;
            }
            k += 1.0;
        }
        Ok(a)
    });
    vm.define_method(proto, "filter", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let a = array_species_create(vm, &ov, 0.0)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut to = 0.0;
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                let keep = call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &this_arg,
                    &[v.clone(), Value::Number(k), ov.clone()],
                )?;
                if vm.to_boolean(&keep) {
                    create_data_elem(vm, &a, to, v)?;
                    to += 1.0;
                }
            }
            k += 1.0;
        }
        Ok(a)
    });
    vm.define_method(proto, "find", 1, |vm, this, args| {
        // `find` visits every index via Get (holes read as undefined).
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            let v = vm.get_prop(&ov, &elem_key(k))?;
            let r = call_cb(
                vm,
                &mut prep,
                &cb,
                &this_arg,
                &[v.clone(), Value::Number(k), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(v);
            }
            k += 1.0;
        }
        Ok(Value::Undefined)
    });
    vm.define_method(proto, "findIndex", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            let v = vm.get_prop(&ov, &elem_key(k))?;
            let r = call_cb(
                vm,
                &mut prep,
                &cb,
                &this_arg,
                &[v, Value::Number(k), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Number(k));
            }
            k += 1.0;
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "findLast", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = len - 1.0;
        while k >= 0.0 {
            vm.native_tick()?;
            let v = vm.get_prop(&ov, &elem_key(k))?;
            let r = call_cb(
                vm,
                &mut prep,
                &cb,
                &this_arg,
                &[v.clone(), Value::Number(k), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(v);
            }
            k -= 1.0;
        }
        Ok(Value::Undefined)
    });
    vm.define_method(proto, "findLastIndex", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = len - 1.0;
        while k >= 0.0 {
            vm.native_tick()?;
            let v = vm.get_prop(&ov, &elem_key(k))?;
            let r = call_cb(
                vm,
                &mut prep,
                &cb,
                &this_arg,
                &[v, Value::Number(k), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Number(k));
            }
            k -= 1.0;
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "some", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                let r = call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &this_arg,
                    &[v, Value::Number(k), ov.clone()],
                )?;
                if vm.to_boolean(&r) {
                    return Ok(Value::Bool(true));
                }
            }
            k += 1.0;
        }
        Ok(Value::Bool(false))
    });
    vm.define_method(proto, "every", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                let r = call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &this_arg,
                    &[v, Value::Number(k), ov.clone()],
                )?;
                if !vm.to_boolean(&r) {
                    return Ok(Value::Bool(false));
                }
            }
            k += 1.0;
        }
        Ok(Value::Bool(true))
    });
    vm.define_method(proto, "reduce", 1, |vm, this, args| {
        let (ov, len, cb, _) = iter_setup(vm, &this, args)?;
        let mut k = 0.0;
        let acc;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            // Seed the accumulator with the first present element.
            loop {
                if k >= len {
                    return Err(vm.throw_type("Reduce of empty array with no initial value"));
                }
                vm.native_tick()?;
                let key = elem_key(k);
                k += 1.0;
                if vm.has_prop(&ov, &key)? {
                    acc = vm.get_prop(&ov, &key)?;
                    break;
                }
            }
        }
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut acc = acc;
        while k < len {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                acc = call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &Value::Undefined,
                    &[acc, v, Value::Number(k), ov.clone()],
                )?;
            }
            k += 1.0;
        }
        Ok(acc)
    });
    vm.define_method(proto, "reduceRight", 1, |vm, this, args| {
        let (ov, len, cb, _) = iter_setup(vm, &this, args)?;
        let mut k = len - 1.0;
        let acc;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            loop {
                if k < 0.0 {
                    return Err(vm.throw_type("Reduce of empty array with no initial value"));
                }
                vm.native_tick()?;
                let key = elem_key(k);
                k -= 1.0;
                if vm.has_prop(&ov, &key)? {
                    acc = vm.get_prop(&ov, &key)?;
                    break;
                }
            }
        }
        let mut prep = vm.prepare_kernel_callback(&cb);
        let mut acc = acc;
        while k >= 0.0 {
            vm.native_tick()?;
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                acc = call_cb(
                    vm,
                    &mut prep,
                    &cb,
                    &Value::Undefined,
                    &[acc, v, Value::Number(k), ov.clone()],
                )?;
            }
            k -= 1.0;
        }
        Ok(acc)
    });
    vm.define_method(proto, "fill", 1, |vm, this, args| {
        // Generic: Set(O, k, value, throw) for k in [start, end). Works on
        // array-likes; `set_prop` still hits the backing vec for a dense array.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let value = arg(args, 0);
        let mut k = rel_index(vm, &arg(args, 1), len, 0.0)?;
        let fin = rel_index(vm, &arg(args, 2), len, len)?;
        while k < fin {
            vm.native_tick()?;
            vm.set_prop_strict(&ov, &elem_key(k), value.clone())?;
            k += 1.0;
        }
        Ok(ov)
    });
    vm.define_method(proto, "copyWithin", 2, |vm, this, args| {
        // Generic spec walk: argument coercion can mutate the receiver, and
        // absent source slots must DeletePropertyOrThrow the destination — so
        // every step goes through Has/Get/Set/Delete (no dense fast path).
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let to = rel_index(vm, &arg(args, 0), len, 0.0)?;
        let from = rel_index(vm, &arg(args, 1), len, 0.0)?;
        let fin = rel_index(vm, &arg(args, 2), len, len)?;
        let count = (fin - from).min(len - to).max(0.0);
        // Copy with overlap-aware direction.
        let (mut from, mut to, dir) = if from < to && to < from + count {
            (from + count - 1.0, to + count - 1.0, -1.0)
        } else {
            (from, to, 1.0)
        };
        let mut cnt = count;
        while cnt > 0.0 {
            vm.native_tick()?;
            let fk = elem_key(from);
            let tk = elem_key(to);
            if vm.has_prop(&ov, &fk)? {
                let v = vm.get_prop(&ov, &fk)?;
                vm.set_prop_strict(&ov, &tk, v)?;
            } else {
                delete_or_throw(vm, &ov, &tk)?;
            }
            from += dir;
            to += dir;
            cnt -= 1.0;
        }
        Ok(ov)
    });
    vm.define_method(proto, "reverse", 0, |vm, this, _args| {
        let o = vm.to_object(&this)?;
        // Fast path: a dense array with no holes (a hole would need HasProperty,
        // which can observe an inherited element at that index — see the generic
        // path below) and no reified index accessors.
        let dense_ok = {
            let b = o.borrow();
            matches!(b.internal, Internal::Array(_))
                && !b.props.keys().any(|k| k.array_index().is_some())
                && matches!(&b.internal, Internal::Array(a) if !a.iter().any(|v| matches!(v, Value::Hole)))
        };
        if dense_ok {
            if let Internal::Array(a) = &mut o.borrow_mut().internal {
                a.reverse();
            }
            return Ok(this);
        }
        // Generic array-like: swap O[lower]/O[upper] honoring presence (holes) so
        // an absent slot is created/deleted rather than read as undefined.
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let middle = (len / 2.0).floor();
        let mut lower = 0.0;
        while lower < middle {
            vm.native_tick()?;
            let upper = len - lower - 1.0;
            let lk = elem_key(lower);
            let uk = elem_key(upper);
            let lower_exists = vm.has_prop(&ov, &lk)?;
            let lower_val = if lower_exists {
                vm.get_prop(&ov, &lk)?
            } else {
                Value::Undefined
            };
            let upper_exists = vm.has_prop(&ov, &uk)?;
            let upper_val = if upper_exists {
                vm.get_prop(&ov, &uk)?
            } else {
                Value::Undefined
            };
            match (lower_exists, upper_exists) {
                (true, true) => {
                    vm.set_prop_strict(&ov, &lk, upper_val)?;
                    vm.set_prop_strict(&ov, &uk, lower_val)?;
                }
                (false, true) => {
                    vm.set_prop_strict(&ov, &lk, upper_val)?;
                    delete_or_throw(vm, &ov, &uk)?;
                }
                (true, false) => {
                    delete_or_throw(vm, &ov, &lk)?;
                    vm.set_prop_strict(&ov, &uk, lower_val)?;
                }
                (false, false) => {}
            }
            lower += 1.0;
        }
        Ok(ov)
    });
    vm.define_method(proto, "flat", 0, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let source_len = length_of(vm, &ov)?;
        let depth = {
            let d = arg(args, 0);
            if d.is_undefined() {
                1.0
            } else {
                to_integer_or_infinity(vm, &d)?
            }
        };
        let a = array_species_create(vm, &ov, 0.0)?;
        flatten_into(vm, &a, &ov, source_len, 0.0, depth, None)?;
        Ok(a)
    });
    vm.define_method(proto, "flatMap", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let source_len = length_of(vm, &ov)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let a = array_species_create(vm, &ov, 0.0)?;
        flatten_into(vm, &a, &ov, source_len, 0.0, 1.0, Some((&cb, &this_arg)))?;
        Ok(a)
    });
    vm.define_method(proto, "sort", 1, |vm, this, args| {
        let cmp = arg(args, 0);
        // Comparator must be undefined or callable.
        if !cmp.is_undefined() && !vm.is_callable(&cmp) {
            return Err(vm.throw_type("comparator is not a function"));
        }
        let has_cmp = vm.is_callable(&cmp);
        // Spec-generic (23.1.3.30 + SortIndexedProperties): read present elements
        // through Get (so index getters/inherited accessors fire), sort the
        // snapshot, then write back through Set and delete the trailing slots.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let mut items: Vec<Value> = Vec::new();
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            // Dense fast path (`has_get_elem`): no idx→String key, no hashing,
            // no prototype walk for an unshadowed dense element; everything
            // else takes the exact HasProperty/Get spec sequence.
            if let Some(v) = has_get_elem(vm, &ov, k)? {
                items.push(v);
            }
            k += 1.0;
        }
        let item_count = items.len();
        // Undefineds sort to the end without the comparator ever seeing them.
        // (Values MOVE out of the snapshot — no clone pass.)
        let mut defined = items;
        defined.retain(|v| !v.is_undefined());
        let undef_count = item_count - defined.len();
        merge_sort(vm, &mut defined, &cmp, has_cmp)?;
        let mut j = 0.0;
        for v in defined {
            set_elem(vm, &ov, j, v)?;
            j += 1.0;
        }
        for _ in 0..undef_count {
            set_elem(vm, &ov, j, Value::Undefined)?;
            j += 1.0;
        }
        // Indices [itemCount, len) were holes (absent) — delete them.
        while j < len {
            vm.native_tick()?;
            delete_or_throw(vm, &ov, &elem_key(j))?;
            j += 1.0;
        }
        Ok(ov)
    });
    vm.define_method(proto, "toSorted", 1, |vm, this, args| {
        let cmp = arg(args, 0);
        if !cmp.is_undefined() && !vm.is_callable(&cmp) {
            return Err(vm.throw_type("comparator is not a function"));
        }
        let has_cmp = vm.is_callable(&cmp);
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        // ArrayCreate(len) bounds (the result is a plain dense array).
        array_create(vm, len)?;
        // SortIndexedProperties in read-through-holes mode: Get at EVERY index
        // (no HasProperty skip), so holes read as undefined / via the prototype.
        let mut items: Vec<Value> = Vec::with_capacity(len as usize);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            items.push(vm.get_prop(&ov, &elem_key(k))?);
            k += 1.0;
        }
        let item_count = items.len();
        let mut defined = items;
        defined.retain(|v| !v.is_undefined());
        let undef_count = item_count - defined.len();
        merge_sort(vm, &mut defined, &cmp, has_cmp)?;
        defined.extend(std::iter::repeat(Value::Undefined).take(undef_count));
        Ok(Value::Object(vm.new_array(defined)))
    });
    vm.define_method(proto, "toReversed", 0, |vm, this, _args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        array_create(vm, len)?;
        // Get(O, len-1-k) in ascending k order: reads are observably descending,
        // holes read as undefined / through the prototype, and a getter that
        // mutates the array mid-iteration is honored.
        let mut out: Vec<Value> = Vec::with_capacity(len as usize);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            out.push(vm.get_prop(&ov, &elem_key(len - k - 1.0))?);
            k += 1.0;
        }
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(proto, "toSpliced", 2, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let actual_start = rel_index(vm, &arg(args, 0), len, 0.0)?;
        let insert_count = args.len().saturating_sub(2) as f64;
        let skip = if args.is_empty() {
            0.0
        } else if args.len() == 1 {
            len - actual_start
        } else {
            let dc = to_integer_or_infinity(vm, &arg(args, 1))?;
            dc.clamp(0.0, len - actual_start)
        };
        let new_len = len + insert_count - skip;
        if new_len > MAX_SAFE_LEN {
            return Err(vm.throw_type("toSpliced result exceeds the maximum safe integer length"));
        }
        array_create(vm, new_len)?;
        // Three phases, all through Get in ascending index order: the head
        // [0, actualStart), the inserted items, then the tail starting at
        // actualStart + skipCount (the skipped range is never read).
        let mut out: Vec<Value> = Vec::with_capacity(new_len as usize);
        let mut i = 0.0;
        while i < actual_start {
            vm.native_tick()?;
            out.push(vm.get_prop(&ov, &elem_key(i))?);
            i += 1.0;
        }
        if args.len() > 2 {
            for v in &args[2..] {
                out.push(v.clone());
                i += 1.0;
            }
        }
        let mut r = actual_start + skip;
        while i < new_len {
            vm.native_tick()?;
            out.push(vm.get_prop(&ov, &elem_key(r))?);
            i += 1.0;
            r += 1.0;
        }
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(proto, "with", 2, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = length_of(vm, &ov)?;
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        let actual = if rel >= 0.0 { rel } else { len + rel };
        if actual < 0.0 || actual >= len {
            return Err(vm.throw_range("Invalid index"));
        }
        array_create(vm, len)?;
        let value = arg(args, 1);
        let mut out: Vec<Value> = Vec::with_capacity(len as usize);
        let mut k = 0.0;
        while k < len {
            vm.native_tick()?;
            if k == actual {
                out.push(value.clone());
            } else {
                out.push(vm.get_prop(&ov, &elem_key(k))?);
            }
            k += 1.0;
        }
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(proto, "keys", 0, |vm, this, _a| {
        let o = dense(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.array_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::ArrayKeys,
        ))
    });
    vm.define_method(proto, "values", 0, |vm, this, _a| {
        let o = dense(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.array_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::ArrayValues,
        ))
    });
    vm.define_method(proto, "entries", 0, |vm, this, _a| {
        let o = dense(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.array_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::ArrayEntries,
        ))
    });
    // [Symbol.iterator] = values
    let values = vm
        .get_prop(&Value::Object(proto.clone()), &PropertyKey::str("values"))
        .unwrap();
    let sym = vm.realm.symbol_iterator.clone();
    vm.define_value_sym(proto, sym, values);
}

/// `Array.prototype[@@unscopables]` (23.1.3.38): a null-prototype object whose
/// listed method names are excluded from `with`-scope resolution. The property
/// itself is {writable: false, enumerable: false, configurable: true}; its
/// entries are ordinary `true` data properties.
fn install_unscopables(vm: &mut Vm, proto: &JsObject) {
    let unsc = vm.new_object_proto(None);
    {
        let mut b = unsc.borrow_mut();
        for name in [
            "at",
            "copyWithin",
            "entries",
            "fill",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "flat",
            "flatMap",
            "includes",
            "keys",
            "toReversed",
            "toSorted",
            "toSpliced",
            "values",
        ] {
            b.props
                .insert(PropertyKey::str(name), Property::data(Value::Bool(true)));
        }
    }
    let sym = vm.realm.symbol_unscopables.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(sym),
        Property {
            kind: PropertyKind::Data {
                value: Value::Object(unsc),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
}

/// `FlattenIntoArray(target, source, sourceLen, start, depth [, mapper])`
/// (23.1.3.13.1): walk the source via HasProperty/Get; spread elements that
/// are arrays (proxy-aware `IsArray`) up to `depth` levels; everything else is
/// CreateDataPropertyOrThrow'd onto the target at the running index.
fn flatten_into(
    vm: &mut Vm,
    target: &Value,
    source: &Value,
    source_len: f64,
    start: f64,
    depth: f64,
    mapper: Option<(&Value, &Value)>,
) -> Result<f64, Value> {
    let mut target_index = start;
    let mut source_index = 0.0;
    while source_index < source_len {
        vm.native_tick()?;
        let p = elem_key(source_index);
        if vm.has_prop(source, &p)? {
            let mut element = vm.get_prop(source, &p)?;
            if let Some((cb, this_arg)) = mapper {
                element = vm.call(
                    (*cb).clone(),
                    (*this_arg).clone(),
                    &[element, Value::Number(source_index), source.clone()],
                )?;
            }
            let should_flatten = if depth > 0.0 {
                spec_is_array(vm, &element)?
            } else {
                false
            };
            if should_flatten {
                let element_len = length_of(vm, &element)?;
                target_index = flatten_into(
                    vm,
                    target,
                    &element,
                    element_len,
                    target_index,
                    depth - 1.0,
                    None,
                )?;
            } else {
                if target_index >= MAX_SAFE_LEN {
                    return Err(
                        vm.throw_type("flattened array exceeds the maximum safe integer length")
                    );
                }
                create_data_on(vm, target, &elem_key(target_index), element)?;
                target_index += 1.0;
            }
        }
        source_index += 1.0;
    }
    Ok(target_index)
}

fn merge_sort(
    vm: &mut Vm,
    items: &mut Vec<Value>,
    cmp: &Value,
    has_cmp: bool,
) -> Result<(), Value> {
    if items.len() <= 1 {
        return Ok(());
    }
    // The comparator's function kernel, prepared ONCE for the whole sort:
    // the ~n·log n comparator calls then skip the per-call entry ceremony
    // (callee resolution, register allocation, frame bookkeeping).
    let mut prep = if has_cmp {
        vm.prepare_kernel_callback(cmp)
    } else {
        None
    };
    // All-Number specialization: with a kernel comparator and a snapshot of
    // nothing but Numbers, the whole sort runs over raw `f64`s — no `Value`
    // moves in the merge, and (`prime_prepared_cmp`) no per-call guard: no
    // user code can run between two comparator calls, so the entry checks
    // hold for the entire sort. The recursion/merge structure is IDENTICAL
    // to the generic `merge_sort_range`, so an inconsistent comparator
    // produces the exact same (implementation-defined) order either way.
    if let Some(p) = prep.as_mut() {
        if items.iter().all(|v| matches!(v, Value::Number(_))) {
            if let Some(regs_ab) = vm.prime_prepared_cmp(p) {
                let mut nums: Vec<f64> = items
                    .iter()
                    .map(|v| match v {
                        Value::Number(n) => *n,
                        _ => unreachable!("checked all-Number"),
                    })
                    .collect();
                let mut aux: Vec<f64> = Vec::with_capacity(nums.len() / 2 + 1);
                let n = nums.len();
                merge_sort_range_f64(vm, &mut nums, &mut aux, 0, n, p, regs_ab)?;
                for (slot, n) in items.iter_mut().zip(nums) {
                    *slot = Value::Number(n);
                }
                return Ok(());
            }
        }
    }
    // One scratch buffer for the whole sort (max use: the larger half) instead
    // of two fresh Vec clones per recursion node — the naive version's
    // malloc/free + refcount churn was a measurable slice of sort-heavy runs.
    let mut aux: Vec<Value> = Vec::with_capacity(items.len() / 2 + 1);
    let n = items.len();
    merge_sort_range(vm, items, &mut aux, 0, n, cmp, has_cmp, &mut prep)
}

/// [`merge_sort_range`] over raw `f64`s for the all-Number/kernel-comparator
/// specialization — same recursion, same stable take-left-on-ties merge, so
/// the result (even under an inconsistent comparator) is bit-identical to
/// the generic path; only the value representation changed. Shared with the
/// TypedArray sort (same split/merge structure there too).
pub(crate) fn merge_sort_range_f64(
    vm: &mut Vm,
    items: &mut [f64],
    aux: &mut Vec<f64>,
    lo: usize,
    hi: usize,
    p: &mut crate::exec::PreparedKernel,
    regs_ab: (Option<usize>, Option<usize>),
) -> Result<(), Value> {
    let n = hi - lo;
    if n <= 1 {
        return Ok(());
    }
    let mid = lo + n / 2;
    merge_sort_range_f64(vm, items, aux, lo, mid, p, regs_ab)?;
    merge_sort_range_f64(vm, items, aux, mid, hi, p, regs_ab)?;
    aux.clear();
    aux.extend_from_slice(&items[lo..mid]);
    let mut i = 0; // over aux (left run)
    let mut j = mid; // over items (right run)
    let mut k = lo; // write cursor; k <= j always
    while i < aux.len() && j < hi {
        let order = vm.exec_prepared_cmp_f64(p, regs_ab, aux[i], items[j])?;
        if order <= 0 {
            items[k] = aux[i];
            i += 1;
        } else {
            items[k] = items[j];
            j += 1;
        }
        k += 1;
    }
    while i < aux.len() {
        items[k] = aux[i];
        i += 1;
        k += 1;
    }
    Ok(())
}

/// Sort `items[lo..hi]` in place. Identical recursion structure and stable
/// merge order as the previous out-of-place version, so the comparator sees
/// the exact same call sequence (observable via side effects); only the
/// buffer management changed. On a comparator throw the scratch's contents
/// are abandoned mid-merge — harmless, because every caller sorts a detached
/// scratch list and only writes back to the array on success.
#[allow(clippy::too_many_arguments)]
fn merge_sort_range(
    vm: &mut Vm,
    items: &mut [Value],
    aux: &mut Vec<Value>,
    lo: usize,
    hi: usize,
    cmp: &Value,
    has_cmp: bool,
    prep: &mut Option<crate::exec::PreparedKernel>,
) -> Result<(), Value> {
    let n = hi - lo;
    if n <= 1 {
        return Ok(());
    }
    let mid = lo + n / 2;
    merge_sort_range(vm, items, aux, lo, mid, cmp, has_cmp, prep)?;
    merge_sort_range(vm, items, aux, mid, hi, cmp, has_cmp, prep)?;
    // Move the left run into the scratch (no clones; the vacated slots are
    // overwritten before they are ever read), then merge back into
    // `items[lo..]`. Take from the left on ties (order <= 0) so equal
    // elements keep their original relative order (stable sort).
    aux.clear();
    for slot in &mut items[lo..mid] {
        aux.push(std::mem::replace(slot, Value::Undefined));
    }
    let mut i = 0; // over aux (left run)
    let mut j = mid; // over items (right run)
    let mut k = lo; // write cursor; k <= j always, so unread right
                    // elements are never overwritten
    while i < aux.len() && j < hi {
        let order = compare_values(vm, &aux[i], &items[j], cmp, has_cmp, prep)?;
        if order <= 0 {
            items[k] = std::mem::replace(&mut aux[i], Value::Undefined);
            i += 1;
        } else {
            items.swap(k, j);
            j += 1;
        }
        k += 1;
    }
    while i < aux.len() {
        items[k] = std::mem::replace(&mut aux[i], Value::Undefined);
        i += 1;
        k += 1;
    }
    // Any right-run tail is already in place.
    Ok(())
}

fn compare_values(
    vm: &mut Vm,
    a: &Value,
    b: &Value,
    cmp: &Value,
    has_cmp: bool,
    prep: &mut Option<crate::exec::PreparedKernel>,
) -> Result<i32, Value> {
    if has_cmp {
        // Prepared-kernel comparator: the call runs entirely in unboxed
        // registers, and its result is a `Number` or `Bool` by construction,
        // so ToNumber is a direct match (never user code). A per-call guard
        // miss (non-Number element, patched Math) falls through to the
        // generic call below for this comparison only.
        if let Some(p) = prep {
            if let Some(r) = vm.exec_prepared_kernel(p, &[a.clone(), b.clone()]) {
                let n = match r? {
                    Value::Number(n) => n,
                    Value::Bool(bv) => {
                        if bv {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    _ => unreachable!("fn kernels return Number or Bool"),
                };
                return Ok(if n < 0.0 {
                    -1
                } else if n > 0.0 {
                    1
                } else {
                    0
                });
            }
        }
        // SortCompare with a user comparator: call it, coerce the result via
        // ToNumber, and treat NaN as 0 (spec). Errors propagate. Owned-args
        // path: the pooled buffer moves straight into the callee frame instead
        // of being copied a second time by the slice path's make_frame.
        let mut argv = vm.take_value_vec();
        argv.push(a.clone());
        argv.push(b.clone());
        let r = vm.call_valuevec(cmp.clone(), Value::Undefined, argv)?;
        let n = vm.to_number(&r)?;
        Ok(if n < 0.0 {
            -1
        } else if n > 0.0 {
            1
        } else {
            0
        })
    } else {
        // Default SortCompare: compare ToString(a) and ToString(b) by UTF-16
        // code-unit order. `to_js_string` propagates a thrown error (e.g. a
        // Symbol element must throw TypeError, not be silently coerced).
        let sa = vm.to_js_string(a)?;
        let sb = vm.to_js_string(b)?;
        Ok(utf16_cmp(sa.as_str(), sb.as_str()))
    }
}

/// Compare two strings by UTF-16 code-unit order, matching the spec's
/// string-relational semantics used by the default sort comparator. This differs
/// from raw UTF-8/byte ordering for astral-plane code points (which encode as a
/// surrogate pair `0xD800..=0xDFFF`, ordering before code points in
/// `0xE000..=0xFFFF`).
fn utf16_cmp(a: &str, b: &str) -> i32 {
    let mut ai = a.encode_utf16();
    let mut bi = b.encode_utf16();
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => {
                if x < y {
                    return -1;
                } else if x > y {
                    return 1;
                }
            }
            (Some(_), None) => return 1,
            (None, Some(_)) => return -1,
            (None, None) => return 0,
        }
    }
}

/// Call an iteration callback through its prepared function kernel when one
/// exists (see [`Vm::prepare_kernel_callback`]) — the per-element win behind
/// the native higher-order builtins — falling back to the generic `Vm::call`
/// when the callback has no kernel or a per-call guard declines (a
/// non-`Number` element, a patched `Math`, …). A kernelized callback never
/// consults `this` (translation rejects `this` uses), so skipping `this_arg`
/// on the fast path is unobservable.
fn call_cb(
    vm: &mut Vm,
    prep: &mut Option<crate::exec::PreparedKernel>,
    cb: &Value,
    this_arg: &Value,
    args: &[Value],
) -> Result<Value, Value> {
    if let Some(p) = prep {
        if let Some(r) = vm.exec_prepared_kernel(p, args) {
            return r;
        }
    }
    vm.call(cb.clone(), this_arg.clone(), args)
}

/// Shared prologue for the callback-taking iteration methods (forEach/map/
/// filter/some/every/reduce/find…): `ToObject(this)`, then `LengthOfArrayLike`
/// (so a `length` getter's side effects fire), then the `IsCallable(callbackfn)`
/// check — in that spec order. Returns `(O, len, callbackfn, thisArg)`.
fn iter_setup(
    vm: &mut Vm,
    this: &Value,
    args: &[Value],
) -> Result<(Value, f64, Value, Value), Value> {
    let o = vm.to_object(this)?;
    let ov = Value::Object(o);
    let len = length_of(vm, &ov)?;
    let cb = arg(args, 0);
    if !vm.is_callable(&cb) {
        return Err(vm.throw_type("callback is not a function"));
    }
    Ok((ov, len, cb, arg(args, 1)))
}

/// `ArraySpeciesCreate(originalArray, length)` (spec 10.4.2.2): build the result
/// array for map/filter/slice/splice/concat/flat via the original's constructor
/// `@@species`, falling back to a plain Array when there is no custom species.
/// `IsArray` is proxy-aware, so a Proxy of an array consults its (trapped)
/// `constructor` too.
fn array_species_create(vm: &mut Vm, original: &Value, len: f64) -> Result<Value, Value> {
    let is_arr = spec_is_array(vm, original)?;
    let mut c = Value::Undefined;
    if is_arr {
        c = vm.get_prop(original, &PropertyKey::str("constructor"))?;
        if matches!(c, Value::Object(_)) {
            let sym = vm.realm.symbol_species.clone();
            c = vm.get_prop(&c, &PropertyKey::Sym(sym))?;
            // Only a null @@species falls back to undefined; a null/primitive
            // `constructor` itself must reach the IsConstructor TypeError.
            if matches!(c, Value::Null) {
                c = Value::Undefined;
            }
        }
    }
    if c.is_undefined() {
        return Ok(Value::Object(array_create(vm, len)?));
    }
    if !vm.is_constructor(&c) {
        return Err(vm.throw_type("constructor's Symbol.species is not a constructor"));
    }
    // `len + 0.0` normalizes a negative-zero count (observable by the ctor).
    vm.construct(&c, &[Value::Number(len + 0.0)], &c)
}

/// ECMAScript `ToIntegerOrInfinity`: ToNumber, then NaN -> 0, truncate toward
/// zero, preserving infinities. Used for relative/length-derived indices so we
/// match the spec instead of `to_int32`'s 2^32 wraparound.
fn to_integer_or_infinity(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    let n = vm.to_number(v)?;
    if n.is_nan() {
        return Ok(0.0);
    }
    if n.is_infinite() {
        return Ok(n);
    }
    Ok(n.trunc())
}

/// Resolve a relative start/end index argument against `len`, clamped to
/// `[0, len]`. `default` is used when the argument is `undefined`.
fn rel_index(vm: &mut Vm, v: &Value, len: f64, default: f64) -> Result<f64, Value> {
    if v.is_undefined() {
        return Ok(default);
    }
    let rel = to_integer_or_infinity(vm, v)?;
    Ok(if rel == f64::NEG_INFINITY {
        0.0
    } else if rel < 0.0 {
        (len + rel).max(0.0)
    } else {
        rel.min(len)
    })
}

/// Install `next`, `[Symbol.iterator]` on the array/string/map/set iterator
/// prototypes and the base iterator prototype.
fn install_iterator_protos(vm: &mut Vm) {
    // Base %IteratorPrototype% has [Symbol.iterator]() { return this }.
    let base = vm.realm.iterator_proto.clone();
    let sym = vm.realm.symbol_iterator.clone();
    let self_iter = vm.new_native("[Symbol.iterator]", 0, |_vm, this, _a| Ok(this));
    vm.define_value_sym(&base, sym, Value::Object(self_iter));

    for (proto, tag) in [
        (vm.realm.array_iterator_proto.clone(), "Array Iterator"),
        (vm.realm.string_iterator_proto.clone(), "String Iterator"),
        (vm.realm.map_iterator_proto.clone(), "Map Iterator"),
        (vm.realm.set_iterator_proto.clone(), "Set Iterator"),
    ] {
        vm.define_method(&proto, "next", 0, |vm, this, _args| {
            let o = match &this {
                Value::Object(o) => o.clone(),
                _ => return Err(vm.throw_type("Iterator.next called on non-object")),
            };
            vm.builtin_iterator_next(&o)
        });
        // %XIteratorPrototype%[@@toStringTag] — non-writable, non-enumerable,
        // configurable.
        let sym = vm.realm.symbol_to_string_tag.clone();
        proto.borrow_mut().props.insert(
            PropertyKey::Sym(sym),
            Property {
                kind: PropertyKind::Data {
                    value: Value::str(tag),
                    writable: false,
                },
                enumerable: false,
                configurable: true,
            },
        );
    }
}
