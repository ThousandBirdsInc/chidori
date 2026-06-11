//! Array constructor, `Array.prototype`, and the array/shared iterator
//! prototypes.

use super::arg;
use super::fundamental::create_data_property_or_throw;
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    install_iterator_protos(vm);
    let proto = vm.realm.array_proto.clone();
    proto.borrow_mut().internal = Internal::Array(Vec::new());

    let ctor = vm.new_native_ctor("Array", 1, array_call, array_call);
    vm.install_ctor("Array", &ctor, &proto);
    vm.install_species(&ctor);

    vm.define_method(&ctor, "isArray", 1, |_vm, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Object(o) if o.borrow().is_array()),
        ))
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
        let new_result = |vm: &mut Vm, len: usize| -> Result<(Value, JsObject), Value> {
            let a = if is_ctor {
                vm.construct(&t, &[Value::Number(len as f64)], &t)?
            } else {
                Value::Object(vm.new_array(vec![Value::Hole; len]))
            };
            match &a {
                Value::Object(o) => Ok((a.clone(), o.clone())),
                _ => Err(vm.throw_type("Array.from: constructor did not return an object")),
            }
        };
        if is_iterable(vm, &src)? {
            let (a, ao) = new_result(vm, 0)?;
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
            let (a, ao) = new_result(vm, len)?;
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

/// `IsConcatSpreadable(O)`: object whose `@@isConcatSpreadable` (if defined)
/// is truthy, else any Array. Non-objects are never spreadable.
fn is_concat_spreadable(vm: &mut Vm, v: &Value) -> Result<bool, Value> {
    let o = match v {
        Value::Object(o) => o.clone(),
        _ => return Ok(false),
    };
    let sym = vm.realm.symbol_is_concat_spreadable.clone();
    let spread = vm.get_prop(v, &PropertyKey::Sym(sym))?;
    if !spread.is_undefined() {
        return Ok(vm.to_boolean(&spread));
    }
    let is_arr = o.borrow().is_array();
    Ok(is_arr)
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

fn install_proto_methods(vm: &mut Vm, proto: &JsObject) {
    vm.define_method(proto, "push", 1, |vm, this, args| {
        // Spec-generic: Set(O, len+i, arg, throw); Set(O, "length", …, throw). The
        // throwing Set surfaces a frozen array / non-writable length as a
        // TypeError, and the 2^53-1 guard runs before any element is written.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let mut len = elements_len(vm, &ov)? as f64;
        let argc = args.len() as f64;
        if len + argc > 9007199254740991.0 {
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
        // Spec-generic: Get the last element, DeletePropertyOrThrow it, then set
        // the (throwing) length — so a frozen/non-writable receiver throws.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = elements_len(vm, &ov)? as f64;
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
        let len = elements_len(vm, &ov)?;
        if len > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        if len == 0 {
            vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(0.0))?;
            return Ok(Value::Undefined);
        }
        let first = vm.get_prop(&ov, &PropertyKey::from_index(0))?;
        for k in 1..len {
            let from = elem_key(k as f64);
            let to = elem_key((k - 1) as f64);
            if vm.has_prop(&ov, &from)? {
                let v = vm.get_prop(&ov, &from)?;
                vm.set_prop_strict(&ov, &to, v)?;
            } else {
                delete_or_throw(vm, &ov, &to)?;
            }
        }
        delete_or_throw(vm, &ov, &elem_key((len - 1) as f64))?;
        vm.set_prop_strict(
            &ov,
            &PropertyKey::str("length"),
            Value::Number((len - 1) as f64),
        )?;
        Ok(first)
    });
    vm.define_method(proto, "unshift", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = elements_len(vm, &ov)?;
        let argc = args.len();
        if argc > 0 {
            if (len + argc) as f64 > 9007199254740991.0 {
                return Err(vm.throw_type("unshift would exceed the maximum safe integer length"));
            }
            if len > crate::value::MAX_DENSE_ARRAY {
                return Err(vm.throw_range("array-like length exceeds engine limit"));
            }
            // Shift existing elements up by argc (high-to-low to avoid clobbering).
            let mut k = len;
            while k > 0 {
                let from = elem_key((k - 1) as f64);
                let to = elem_key((k - 1 + argc) as f64);
                if vm.has_prop(&ov, &from)? {
                    let v = vm.get_prop(&ov, &from)?;
                    vm.set_prop_strict(&ov, &to, v)?;
                } else {
                    delete_or_throw(vm, &ov, &to)?;
                }
                k -= 1;
            }
            for (i, v) in args.iter().enumerate() {
                vm.set_prop_strict(&ov, &PropertyKey::from_index(i as u32), v.clone())?;
            }
        }
        let new_len = (len + argc) as f64;
        vm.set_prop_strict(&ov, &PropertyKey::str("length"), Value::Number(new_len))?;
        Ok(Value::Number(new_len))
    });
    vm.define_method(proto, "at", 1, |vm, this, args| {
        // Generic: length + indexed access (works on array-likes).
        let len = elements_len(vm, &this)? as f64;
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(Value::Undefined);
        }
        let recv = vm.to_object(&this)?;
        vm.get_prop(&Value::Object(recv), &PropertyKey::from_index(k as u32))
    });
    vm.define_method(proto, "slice", 2, |vm, this, args| {
        // Generic: works on array-likes (reads length + indexed elements). For a
        // dense array `get_prop` still hits the backing vec, so this stays fast.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = elements_len(vm, &ov)? as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let end = rel_index(vm, &arg(args, 1), len, len)?;
        if (end - start).max(0) as usize > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        let count = (end - start).max(0) as usize;
        let a = array_species_create(vm, &ov, count)?;
        let mut n = 0u32;
        let mut k = start;
        while k < end {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                create_data_on(vm, &a, &PropertyKey::from_index(n), v)?;
            }
            n += 1;
            k += 1;
        }
        vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(count as f64))?;
        Ok(a)
    });
    vm.define_method(proto, "splice", 2, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let len = elements_len(vm, &Value::Object(o.clone()))? as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let delete_count = if args.len() < 2 {
            if args.is_empty() {
                0
            } else {
                len - start
            }
        } else {
            let dc = to_integer_or_infinity(vm, &arg(args, 1))?;
            dc.max(0.0).min((len - start) as f64) as isize
        }
        .min(len - start)
        .max(0);
        let inserts: Vec<Value> = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            Vec::new()
        };
        // Spec-generic: ArraySpeciesCreate for the removed-elements result, then
        // Get/Set/Delete through O so getters/setters and holes are honored.
        // Bound the work to the engine's dense-array cap so a hostile huge
        // `length` (e.g. 2**53) can't OOM/hang instead of allocating.
        if len as usize > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        let ov = Value::Object(o);
        let s = start as usize;
        let dc = delete_count as usize;
        let ulen = len as usize;
        let ins = inserts.len();
        // A = ArraySpeciesCreate(O, deleteCount), filled with the removed values.
        let a = array_species_create(vm, &ov, dc)?;
        for k in 0..dc {
            let from = PropertyKey::from_index((s + k) as u32);
            if vm.has_prop(&ov, &from)? {
                let v = vm.get_prop(&ov, &from)?;
                create_data_on(vm, &a, &PropertyKey::from_index(k as u32), v)?;
            }
        }
        vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(dc as f64))?;
        // Shift the tail of O to make room for / close the gap left by inserts.
        if ins < dc {
            for k in s..(ulen - dc) {
                let from = PropertyKey::from_index((k + dc) as u32);
                let to = PropertyKey::from_index((k + ins) as u32);
                if vm.has_prop(&ov, &from)? {
                    let v = vm.get_prop(&ov, &from)?;
                    vm.set_prop_strict(&ov, &to, v)?;
                } else {
                    vm.delete_prop(&ov, &to)?;
                }
            }
            for k in (ulen - dc + ins..ulen).rev() {
                vm.delete_prop(&ov, &PropertyKey::from_index(k as u32))?;
            }
        } else if ins > dc {
            for k in (s..(ulen - dc)).rev() {
                let from = PropertyKey::from_index((k + dc) as u32);
                let to = PropertyKey::from_index((k + ins) as u32);
                if vm.has_prop(&ov, &from)? {
                    let v = vm.get_prop(&ov, &from)?;
                    vm.set_prop_strict(&ov, &to, v)?;
                } else {
                    vm.delete_prop(&ov, &to)?;
                }
            }
        }
        for (i, v) in inserts.into_iter().enumerate() {
            vm.set_prop_strict(&ov, &PropertyKey::from_index((s + i) as u32), v)?;
        }
        vm.set_prop_strict(
            &ov,
            &PropertyKey::str("length"),
            Value::Number((ulen - dc + ins) as f64),
        )?;
        Ok(a)
    });
    vm.define_method(proto, "concat", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        // A = ArraySpeciesCreate(O, 0). items = [O, ...args]; each is spread if
        // IsConcatSpreadable, else appended as a single element. Absent indices of
        // a spreadable leave a hole in A (so its length is preserved).
        let a = array_species_create(vm, &ov, 0)?;
        let mut n: u32 = 0;
        let mut items: Vec<Value> = Vec::with_capacity(args.len() + 1);
        items.push(ov);
        items.extend(args.iter().cloned());
        for e in items {
            if is_concat_spreadable(vm, &e)? {
                let len = elements_len(vm, &e)?;
                if n as usize + len > crate::value::MAX_DENSE_ARRAY {
                    return Err(vm.throw_range("array length exceeds engine limit"));
                }
                for k in 0..len {
                    let key = PropertyKey::from_index(k as u32);
                    if vm.has_prop(&e, &key)? {
                        let v = vm.get_prop(&e, &key)?;
                        create_data_on(vm, &a, &PropertyKey::from_index(n), v)?;
                    }
                    n += 1;
                }
            } else {
                create_data_on(vm, &a, &PropertyKey::from_index(n), e)?;
                n += 1;
            }
        }
        vm.set_prop_strict(&a, &PropertyKey::str("length"), Value::Number(n as f64))?;
        Ok(a)
    });
    vm.define_method(proto, "join", 1, |vm, this, args| {
        let items = elements(vm, &this)?;
        let sep = {
            let s = arg(args, 0);
            if s.is_undefined() {
                ",".to_string()
            } else {
                vm.to_string_lossy(&s)
            }
        };
        let mut parts = Vec::with_capacity(items.len());
        for v in &items {
            // null/undefined and holes all stringify to the empty string.
            if v.is_nullish() || matches!(v, Value::Hole) {
                parts.push(String::new());
            } else {
                parts.push(vm.to_string_lossy(v));
            }
        }
        Ok(Value::str(parts.join(&sep)))
    });
    vm.define_method(proto, "toString", 0, |vm, this, _a| {
        let join = vm.get_prop(&this, &PropertyKey::str("join"))?;
        if vm.is_callable(&join) {
            vm.call(join, this, &[])
        } else {
            Ok(Value::str("[object Array]"))
        }
    });
    vm.define_method(proto, "toLocaleString", 0, |vm, this, _a| {
        // Join each element's `ToString(Invoke(element, "toLocaleString"))` with
        // ",". null/undefined elements (and holes) contribute the empty string.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o.clone());
        let len = elements_len(vm, &ov)?;
        let mut out = String::new();
        for k in 0..len {
            if k > 0 {
                out.push(',');
            }
            let v = vm.get_prop(&ov, &PropertyKey::from_index(k as u32))?;
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
        let len = elements_len(vm, &ov)?;
        let target = arg(args, 0);
        if len == 0 {
            return Ok(Value::Number(-1.0));
        }
        let from = to_integer_or_infinity(vm, &arg(args, 1))?;
        if from == f64::INFINITY {
            return Ok(Value::Number(-1.0));
        }
        let start = if from == f64::NEG_INFINITY {
            0
        } else if from >= 0.0 {
            from as usize
        } else {
            ((len as f64 + from).max(0.0)) as usize
        };
        for k in start..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                if vm.strict_equals(&v, &target) {
                    return Ok(Value::Number(k as f64));
                }
            }
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "lastIndexOf", 1, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = elements_len(vm, &ov)?;
        let target = arg(args, 0);
        if len == 0 {
            return Ok(Value::Number(-1.0));
        }
        // Default fromIndex is len-1; otherwise ToIntegerOrInfinity.
        let mut start: isize = (len - 1) as isize;
        if args.len() >= 2 {
            let from = to_integer_or_infinity(vm, &arg(args, 1))?;
            if from == f64::NEG_INFINITY {
                return Ok(Value::Number(-1.0));
            } else if from >= 0.0 {
                start = (from as isize).min((len - 1) as isize);
            } else {
                start = len as isize + from as isize;
            }
        }
        let mut k = start;
        while k >= 0 {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                if vm.strict_equals(&v, &target) {
                    return Ok(Value::Number(k as f64));
                }
            }
            k -= 1;
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "includes", 1, |vm, this, args| {
        let len = elements_len(vm, &this)? as f64;
        let target = arg(args, 0);
        if len == 0.0 {
            return Ok(Value::Bool(false));
        }
        let from = to_integer_or_infinity(vm, &arg(args, 1))?;
        let start = if from == f64::INFINITY {
            return Ok(Value::Bool(false));
        } else if from == f64::NEG_INFINITY {
            0
        } else if from >= 0.0 {
            from as usize
        } else {
            ((len + from).max(0.0)) as usize
        };
        let items = elements(vm, &this)?;
        for v in items.iter().skip(start) {
            // `includes` reads holes as undefined (it does not skip them).
            let probe = if matches!(v, Value::Hole) {
                &Value::Undefined
            } else {
                v
            };
            if same_value_zero(probe, &target) {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });
    vm.define_method(proto, "forEach", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in 0..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                vm.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Number(k as f64), ov.clone()],
                )?;
            }
        }
        Ok(Value::Undefined)
    });
    vm.define_method(proto, "map", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        // Result via ArraySpeciesCreate; holes map to holes (the callback is not
        // invoked for an absent index and that output slot stays absent).
        let a = array_species_create(vm, &ov, len)?;
        for k in 0..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                let mapped = vm.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Number(k as f64), ov.clone()],
                )?;
                create_data_on(vm, &a, &key, mapped)?;
            }
        }
        Ok(a)
    });
    vm.define_method(proto, "filter", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        let a = array_species_create(vm, &ov, 0)?;
        let mut to = 0u32;
        for k in 0..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                let keep = vm.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v.clone(), Value::Number(k as f64), ov.clone()],
                )?;
                if vm.to_boolean(&keep) {
                    create_data_on(vm, &a, &PropertyKey::from_index(to), v)?;
                    to += 1;
                }
            }
        }
        Ok(a)
    });
    vm.define_method(proto, "find", 1, |vm, this, args| {
        // `find` visits every index via Get (holes read as undefined).
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in 0..len {
            let v = vm.get_prop(&ov, &PropertyKey::from_index(k as u32))?;
            let r = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v.clone(), Value::Number(k as f64), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(v);
            }
        }
        Ok(Value::Undefined)
    });
    vm.define_method(proto, "findIndex", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in 0..len {
            let v = vm.get_prop(&ov, &PropertyKey::from_index(k as u32))?;
            let r = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v, Value::Number(k as f64), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Number(k as f64));
            }
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "findLast", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in (0..len).rev() {
            let v = vm.get_prop(&ov, &PropertyKey::from_index(k as u32))?;
            let r = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v.clone(), Value::Number(k as f64), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(v);
            }
        }
        Ok(Value::Undefined)
    });
    vm.define_method(proto, "findLastIndex", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in (0..len).rev() {
            let v = vm.get_prop(&ov, &PropertyKey::from_index(k as u32))?;
            let r = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v, Value::Number(k as f64), ov.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Number(k as f64));
            }
        }
        Ok(Value::Number(-1.0))
    });
    vm.define_method(proto, "some", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in 0..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                let r = vm.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Number(k as f64), ov.clone()],
                )?;
                if vm.to_boolean(&r) {
                    return Ok(Value::Bool(true));
                }
            }
        }
        Ok(Value::Bool(false))
    });
    vm.define_method(proto, "every", 1, |vm, this, args| {
        let (ov, len, cb, this_arg) = iter_setup(vm, &this, args)?;
        for k in 0..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                let r = vm.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Number(k as f64), ov.clone()],
                )?;
                if !vm.to_boolean(&r) {
                    return Ok(Value::Bool(false));
                }
            }
        }
        Ok(Value::Bool(true))
    });
    vm.define_method(proto, "reduce", 1, |vm, this, args| {
        let (ov, len, cb, _) = iter_setup(vm, &this, args)?;
        let mut k = 0usize;
        let acc;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            // Seed the accumulator with the first present element.
            loop {
                if k >= len {
                    return Err(vm.throw_type("Reduce of empty array with no initial value"));
                }
                let key = PropertyKey::from_index(k as u32);
                k += 1;
                if vm.has_prop(&ov, &key)? {
                    acc = vm.get_prop(&ov, &key)?;
                    break;
                }
            }
        }
        let mut acc = acc;
        while k < len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                acc = vm.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Number(k as f64), ov.clone()],
                )?;
            }
            k += 1;
        }
        Ok(acc)
    });
    vm.define_method(proto, "reduceRight", 1, |vm, this, args| {
        let (ov, len, cb, _) = iter_setup(vm, &this, args)?;
        let mut k: isize = len as isize - 1;
        let acc;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            loop {
                if k < 0 {
                    return Err(vm.throw_type("Reduce of empty array with no initial value"));
                }
                let key = PropertyKey::from_index(k as u32);
                k -= 1;
                if vm.has_prop(&ov, &key)? {
                    acc = vm.get_prop(&ov, &key)?;
                    break;
                }
            }
        }
        let mut acc = acc;
        while k >= 0 {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                let v = vm.get_prop(&ov, &key)?;
                acc = vm.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Number(k as f64), ov.clone()],
                )?;
            }
            k -= 1;
        }
        Ok(acc)
    });
    vm.define_method(proto, "fill", 1, |vm, this, args| {
        // Generic: Set(O, k, value) for k in [start, end). Works on array-likes;
        // `set_prop` still hits the backing vec for a dense array.
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o);
        let len = elements_len(vm, &ov)? as isize;
        let value = arg(args, 0);
        let start = rel_index(vm, &arg(args, 1), len, 0)?;
        let end = rel_index(vm, &arg(args, 2), len, len)?;
        if (end - start).max(0) as usize > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        let mut i = start.max(0);
        while i < end {
            vm.set_prop(&ov, &PropertyKey::from_index(i as u32), value.clone())?;
            i += 1;
        }
        Ok(this)
    });
    vm.define_method(proto, "copyWithin", 2, |vm, this, args| {
        let o = vm.to_object(&this)?;
        let ov = Value::Object(o.clone());
        let len = elements_len(vm, &ov)? as isize;
        let target = rel_index(vm, &arg(args, 0), len, 0)?;
        let start = rel_index(vm, &arg(args, 1), len, 0)?;
        let end = rel_index(vm, &arg(args, 2), len, len)?;
        let count = (end - start).min(len - target).max(0);
        if count <= 0 {
            return Ok(this);
        }
        if count as usize > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        // Dense fast path.
        if o.borrow().is_array() {
            let mut b = o.borrow_mut();
            if let Internal::Array(a) = &mut b.internal {
                // Re-clamp to the CURRENT length (argument coercion may have run
                // side effects that mutated the array).
                let alen = a.len() as isize;
                let start = start.clamp(0, alen);
                let target = target.clamp(0, alen);
                let count = count.min(alen - start).min(alen - target).max(0);
                let src: Vec<Value> = (0..count)
                    .map(|i| a[(start + i) as usize].clone())
                    .collect();
                for (i, v) in src.into_iter().enumerate() {
                    a[(target + i as isize) as usize] = v;
                }
                return Ok(this);
            }
        }
        // Generic array-like: copy with overlap-aware direction, mirroring holes
        // (HasProperty false → DeleteProperty on the destination).
        let (mut from, mut to, dir) = if start < target && target < start + count {
            (start + count - 1, target + count - 1, -1isize)
        } else {
            (start, target, 1)
        };
        let mut cnt = count;
        while cnt > 0 {
            let fk = PropertyKey::from_index(from as u32);
            let tk = PropertyKey::from_index(to as u32);
            if vm.has_prop(&ov, &fk)? {
                let v = vm.get_prop(&ov, &fk)?;
                vm.set_prop(&ov, &tk, v)?;
            } else {
                vm.delete_prop(&ov, &tk)?;
            }
            from += dir;
            to += dir;
            cnt -= 1;
        }
        Ok(this)
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
        let len = elements_len(vm, &ov)?;
        if len > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        for lower in 0..len / 2 {
            let upper = len - 1 - lower;
            let lk = PropertyKey::from_index(lower as u32);
            let uk = PropertyKey::from_index(upper as u32);
            let lower_exists = vm.has_prop(&ov, &lk)?;
            let lower_val = if lower_exists { vm.get_prop(&ov, &lk)? } else { Value::Undefined };
            let upper_exists = vm.has_prop(&ov, &uk)?;
            let upper_val = if upper_exists { vm.get_prop(&ov, &uk)? } else { Value::Undefined };
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
        }
        Ok(ov)
    });
    vm.define_method(proto, "flat", 0, |vm, this, args| {
        let items = elements_with_holes(vm, &this)?;
        let depth = {
            let d = arg(args, 0);
            if d.is_undefined() {
                1.0
            } else {
                to_integer_or_infinity(vm, &d)?.max(0.0)
            }
        };
        let mut out = Vec::new();
        flatten(vm, items, depth, &mut out);
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(proto, "flatMap", 1, |vm, this, args| {
        let items = elements_with_holes(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let mut out = Vec::new();
        for (i, v) in items.into_iter().enumerate() {
            // Holes are skipped (HasProperty is false).
            if matches!(v, Value::Hole) {
                continue;
            }
            let r = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
            // Spread a one-level array result; everything else is pushed as-is.
            if let Value::Object(ro) = &r {
                if ro.borrow().is_array() {
                    out.extend(arr_clone(ro));
                    continue;
                }
            }
            out.push(r);
        }
        Ok(Value::Object(vm.new_array(out)))
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
        let len = elements_len(vm, &ov)?;
        if len > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array-like length exceeds engine limit"));
        }
        let mut items: Vec<Value> = Vec::new();
        for k in 0..len {
            let key = PropertyKey::from_index(k as u32);
            if vm.has_prop(&ov, &key)? {
                items.push(vm.get_prop(&ov, &key)?);
            }
        }
        let item_count = items.len();
        // Undefineds sort to the end without the comparator ever seeing them.
        let mut defined: Vec<Value> = items
            .iter()
            .filter(|v| !v.is_undefined())
            .cloned()
            .collect();
        let undef_count = item_count - defined.len();
        merge_sort(vm, &mut defined, &cmp, has_cmp)?;
        let mut j = 0usize;
        for v in defined {
            vm.set_prop_strict(&ov, &PropertyKey::from_index(j as u32), v)?;
            j += 1;
        }
        for _ in 0..undef_count {
            vm.set_prop_strict(&ov, &PropertyKey::from_index(j as u32), Value::Undefined)?;
            j += 1;
        }
        // Indices [itemCount, len) were holes (absent) — delete them.
        while j < len {
            vm.delete_prop(&ov, &PropertyKey::from_index(j as u32))?;
            j += 1;
        }
        Ok(ov)
    });
    vm.define_method(proto, "toSorted", 1, |vm, this, args| {
        let cmp = arg(args, 0);
        if !cmp.is_undefined() && !vm.is_callable(&cmp) {
            return Err(vm.throw_type("comparator is not a function"));
        }
        let has_cmp = vm.is_callable(&cmp);
        let items = elements(vm, &this)?;
        let mut defined: Vec<Value> = items
            .iter()
            .filter(|v| !v.is_undefined())
            .cloned()
            .collect();
        let undef_count = items.len() - defined.len();
        merge_sort(vm, &mut defined, &cmp, has_cmp)?;
        defined.extend(std::iter::repeat(Value::Undefined).take(undef_count));
        Ok(Value::Object(vm.new_array(defined)))
    });
    vm.define_method(proto, "toReversed", 0, |vm, this, _args| {
        let mut items = elements(vm, &this)?;
        items.reverse();
        Ok(Value::Object(vm.new_array(items)))
    });
    vm.define_method(proto, "toSpliced", 2, |vm, this, args| {
        let items = elements(vm, &this)?;
        let len = items.len() as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let skip = if args.is_empty() {
            0
        } else if args.len() == 1 {
            len - start
        } else {
            let dc = to_integer_or_infinity(vm, &arg(args, 1))?;
            dc.max(0.0).min((len - start) as f64) as isize
        }
        .min(len - start)
        .max(0);
        let inserts: Vec<Value> = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            Vec::new()
        };
        let mut out: Vec<Value> = Vec::with_capacity(items.len());
        out.extend_from_slice(&items[..start as usize]);
        out.extend(inserts);
        out.extend_from_slice(&items[(start + skip) as usize..]);
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(proto, "with", 2, |vm, this, args| {
        let mut items = elements(vm, &this)?;
        let len = items.len() as f64;
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        let actual = if rel >= 0.0 { rel } else { len + rel };
        if actual < 0.0 || actual >= len {
            return Err(vm.throw_range("Invalid index"));
        }
        items[actual as usize] = arg(args, 1);
        Ok(Value::Object(vm.new_array(items)))
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

/// Flatten `items` recursively to `depth` levels, spreading nested arrays.
fn flatten(vm: &mut Vm, items: Vec<Value>, depth: f64, out: &mut Vec<Value>) {
    for v in items {
        // Holes are skipped (FlattenIntoArray uses HasProperty).
        if matches!(v, Value::Hole) {
            continue;
        }
        if depth > 0.0 {
            if let Value::Object(vo) = &v {
                if vo.borrow().is_array() {
                    let nested = arr_clone(vo);
                    flatten(vm, nested, depth - 1.0, out);
                    continue;
                }
            }
        }
        out.push(v);
    }
}

fn merge_sort(
    vm: &mut Vm,
    items: &mut Vec<Value>,
    cmp: &Value,
    has_cmp: bool,
) -> Result<(), Value> {
    let n = items.len();
    if n <= 1 {
        return Ok(());
    }
    let mid = n / 2;
    let mut left = items[..mid].to_vec();
    let mut right = items[mid..].to_vec();
    merge_sort(vm, &mut left, cmp, has_cmp)?;
    merge_sort(vm, &mut right, cmp, has_cmp)?;
    let mut i = 0;
    let mut j = 0;
    let mut k = 0;
    // Stable merge: take from `left` on ties (order <= 0) so equal elements
    // preserve their original relative order.
    while i < left.len() && j < right.len() {
        let order = compare_values(vm, &left[i], &right[j], cmp, has_cmp)?;
        if order <= 0 {
            items[k] = left[i].clone();
            i += 1;
        } else {
            items[k] = right[j].clone();
            j += 1;
        }
        k += 1;
    }
    while i < left.len() {
        items[k] = left[i].clone();
        i += 1;
        k += 1;
    }
    while j < right.len() {
        items[k] = right[j].clone();
        j += 1;
        k += 1;
    }
    Ok(())
}

fn compare_values(
    vm: &mut Vm,
    a: &Value,
    b: &Value,
    cmp: &Value,
    has_cmp: bool,
) -> Result<i32, Value> {
    if has_cmp {
        // SortCompare with a user comparator: call it, coerce the result via
        // ToNumber, and treat NaN as 0 (spec). Errors propagate.
        let r = vm.call(cmp.clone(), Value::Undefined, &[a.clone(), b.clone()])?;
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

/// Shared prologue for the callback-taking iteration methods (forEach/map/
/// filter/some/every/reduce/find…): `ToObject(this)`, then `LengthOfArrayLike`
/// (so a `length` getter's side effects fire), then the `IsCallable(callbackfn)`
/// check — in that spec order. Returns `(O, len, callbackfn, thisArg)`.
fn iter_setup(
    vm: &mut Vm,
    this: &Value,
    args: &[Value],
) -> Result<(Value, usize, Value, Value), Value> {
    let o = vm.to_object(this)?;
    let ov = Value::Object(o);
    let len = elements_len(vm, &ov)?;
    let cb = arg(args, 0);
    if !vm.is_callable(&cb) {
        return Err(vm.throw_type("callback is not a function"));
    }
    if len > crate::value::MAX_DENSE_ARRAY {
        return Err(vm.throw_range("array-like length exceeds engine limit"));
    }
    Ok((ov, len, cb, arg(args, 1)))
}

/// `ArraySpeciesCreate(originalArray, length)` (spec 10.4.2.2): build the result
/// array for map/filter/slice/splice/concat via the original's constructor
/// `@@species`, falling back to a plain Array when there is no custom species.
fn array_species_create(vm: &mut Vm, original: &Value, len: usize) -> Result<Value, Value> {
    let is_arr = matches!(original, Value::Object(o) if o.borrow().is_array());
    let mut c = Value::Undefined;
    if is_arr {
        c = vm.get_prop(original, &PropertyKey::str("constructor"))?;
        if matches!(c, Value::Object(_)) {
            let sym = vm.realm.symbol_species.clone();
            c = vm.get_prop(&c, &PropertyKey::Sym(sym))?;
            if matches!(c, Value::Null) {
                c = Value::Undefined;
            }
        }
    }
    if c.is_undefined() {
        if len > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("array length exceeds engine limit"));
        }
        return Ok(Value::Object(vm.new_array(vec![Value::Hole; len])));
    }
    if !vm.is_constructor(&c) {
        return Err(vm.throw_type("constructor's Symbol.species is not a constructor"));
    }
    vm.construct(&c, &[Value::Number(len as f64)], &c)
}

/// Materialize the elements of an array or array-like `this` (length property +
/// indexed access), so the non-mutating prototype methods work generically —
/// including `Array.prototype.method.call(arrayLikeObject, …)`. Holes read as
/// `undefined` (the spec's Get-based view); use `elements_with_holes` for the
/// few methods that must distinguish a hole from a present `undefined`.
fn elements(vm: &mut Vm, this: &Value) -> Result<Vec<Value>, Value> {
    let mut out = elements_with_holes(vm, this)?;
    for v in &mut out {
        if matches!(v, Value::Hole) {
            *v = Value::Undefined;
        }
    }
    Ok(out)
}

/// Like `elements`, but preserves `Value::Hole` in the dense fast path so a
/// caller can tell a hole apart from a present `undefined` (used by
/// `indexOf`/`lastIndexOf`, which skip holes, and `flat`/`flatMap`).
fn elements_with_holes(vm: &mut Vm, this: &Value) -> Result<Vec<Value>, Value> {
    if let Value::Object(o) = this {
        let is_arr = matches!(o.borrow().internal, Internal::Array(_));
        if is_arr {
            if let Internal::Array(a) = &o.borrow().internal {
                return Ok(a.clone());
            }
        }
    }
    let o = vm.to_object(this)?;
    let len_v = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("length"))?;
    let len = vm.to_length(&len_v)?;
    // Cap array-like materialization: a huge `length` property (test262 uses
    // values like 2^32) would otherwise loop forever. Bounding it fails such
    // boundary tests loudly instead of hanging the engine.
    if len > crate::value::MAX_DENSE_ARRAY {
        return Err(vm.throw_range("array-like length exceeds engine limit"));
    }
    let mut out = Vec::with_capacity(len.min(1 << 16));
    for i in 0..len {
        out.push(vm.get_prop(
            &Value::Object(o.clone()),
            &PropertyKey::from_index(i as u32),
        )?);
    }
    Ok(out)
}

/// `(index, value)` pairs for the *present* indices of an array-like receiver.
/// The spec iteration methods (`forEach`/`some`/`every`/`map`/`filter`/`reduce`)
/// skip holes — indices where `HasProperty(O, k)` is false — so an array-like
/// like `{length: 3, 0: …, 2: …}` visits 0 and 2 but not 1. Dense arrays have no
/// holes (every slot is present), so they visit every index.
fn present_elements(vm: &mut Vm, this: &Value) -> Result<Vec<(usize, Value)>, Value> {
    if let Value::Object(o) = this {
        // Dense fast-path only when there are no *reified* index properties (a
        // getter/accessor or a non-default descriptor defined via
        // defineProperty on an index): those shadow the dense slot and must be
        // read through `get_prop` (invoking the getter), which the generic path
        // below does.
        let dense_ok = {
            let b = o.borrow();
            matches!(b.internal, Internal::Array(_))
                && !b.props.keys().any(|k| k.array_index().is_some())
        };
        if dense_ok {
            if let Internal::Array(a) = &o.borrow().internal {
                // Holes are absent: methods that consult HasProperty skip them.
                return Ok(a
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| !matches!(v, Value::Hole))
                    .map(|(i, v)| (i, v.clone()))
                    .collect());
            }
        }
    }
    let o = vm.to_object(this)?;
    let ov = Value::Object(o);
    let len = elements_len(vm, &ov)?;
    if len > crate::value::MAX_DENSE_ARRAY {
        return Err(vm.throw_range("array-like length exceeds engine limit"));
    }
    let mut out = Vec::new();
    for k in 0..len {
        let key = PropertyKey::from_index(k as u32);
        if vm.has_prop(&ov, &key)? {
            out.push((k, vm.get_prop(&ov, &key)?));
        }
    }
    Ok(out)
}

/// The `length` of an array or array-like `this` without materializing every
/// element (used by index-only methods like `at`/`includes`).
fn elements_len(vm: &mut Vm, this: &Value) -> Result<usize, Value> {
    if let Value::Object(o) = this {
        if let Internal::Array(a) = &o.borrow().internal {
            return Ok(a.len());
        }
    }
    let o = vm.to_object(this)?;
    let len_v = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("length"))?;
    vm.to_length(&len_v)
}

fn arr_len(o: &JsObject) -> usize {
    if let Internal::Array(a) = &o.borrow().internal {
        a.len()
    } else {
        0
    }
}
fn arr_get(o: &JsObject, i: usize) -> Value {
    if let Internal::Array(a) = &o.borrow().internal {
        a.get(i).cloned().unwrap_or(Value::Undefined)
    } else {
        Value::Undefined
    }
}
fn arr_clone(o: &JsObject) -> Vec<Value> {
    if let Internal::Array(a) = &o.borrow().internal {
        a.clone()
    } else {
        Vec::new()
    }
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
fn rel_index(vm: &mut Vm, v: &Value, len: isize, default: isize) -> Result<isize, Value> {
    if v.is_undefined() {
        return Ok(default);
    }
    let rel = to_integer_or_infinity(vm, v)?;
    let lenf = len as f64;
    let idx = if rel == f64::NEG_INFINITY {
        0.0
    } else if rel < 0.0 {
        (lenf + rel).max(0.0)
    } else {
        rel.min(lenf)
    };
    Ok(idx as isize)
}

/// Install `next`, `[Symbol.iterator]` on the array/string/map/set iterator
/// prototypes and the base iterator prototype.
fn install_iterator_protos(vm: &mut Vm) {
    // Base %IteratorPrototype% has [Symbol.iterator]() { return this }.
    let base = vm.realm.iterator_proto.clone();
    let sym = vm.realm.symbol_iterator.clone();
    let self_iter = vm.new_native("[Symbol.iterator]", 0, |_vm, this, _a| Ok(this));
    vm.define_value_sym(&base, sym, Value::Object(self_iter));

    for proto in [
        vm.realm.array_iterator_proto.clone(),
        vm.realm.string_iterator_proto.clone(),
        vm.realm.map_iterator_proto.clone(),
        vm.realm.set_iterator_proto.clone(),
    ] {
        vm.define_method(&proto, "next", 0, |vm, this, _args| {
            let o = match &this {
                Value::Object(o) => o.clone(),
                _ => return Err(vm.throw_type("Iterator.next called on non-object")),
            };
            vm.builtin_iterator_next(&o)
        });
    }
}
