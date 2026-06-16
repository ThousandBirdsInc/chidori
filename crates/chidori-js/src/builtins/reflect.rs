//! The `Reflect` namespace object (ECMA-262 §28.1).
//!
//! `Reflect` is an ordinary object (prototype = %Object.prototype%) exposing the
//! reflective object operations as static methods. Each method that takes a
//! `target` throws a `TypeError` when `target` is not an object, matching the
//! spec's `RequireInternalSlot`-style checks.

use super::arg;
use super::fundamental::{
    define_own_property, object_set_prototype_of, own_property_descriptor, to_property_descriptor,
};
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    // Reflect is a plain object whose [[Prototype]] is %Object.prototype%.
    let reflect = vm.new_object();

    // Reflect.get(target, propertyKey [, receiver])
    vm.define_method(&reflect, "get", 2, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        let receiver = if args.len() > 2 {
            arg(args, 2)
        } else {
            Value::Object(target.clone())
        };
        reflect_get(vm, &target, &key, receiver)
    });

    // Reflect.set(target, propertyKey, V [, receiver]) -> Boolean
    vm.define_method(&reflect, "set", 3, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        let value = arg(args, 2);
        let receiver = if args.len() > 3 {
            arg(args, 3)
        } else {
            Value::Object(target.clone())
        };
        let ok = reflect_set(vm, &target, &key, value, receiver)?;
        Ok(Value::Bool(ok))
    });

    // Reflect.has(target, propertyKey) -> Boolean
    vm.define_method(&reflect, "has", 2, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        let has = vm.has_prop(&Value::Object(target), &key)?;
        Ok(Value::Bool(has))
    });

    // Reflect.deleteProperty(target, propertyKey) -> Boolean
    vm.define_method(&reflect, "deleteProperty", 2, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        let ok = vm.delete_prop(&Value::Object(target), &key)?;
        Ok(Value::Bool(ok))
    });

    // Reflect.ownKeys(target) -> Array of own string AND symbol keys
    vm.define_method(&reflect, "ownKeys", 1, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let keys: Vec<Value> = vm
            .own_property_keys(&target)?
            .into_iter()
            .map(|k| match k {
                PropertyKey::Str(s) => Value::String(s),
                PropertyKey::Sym(s) => Value::Symbol(s),
            })
            .collect();
        Ok(Value::Object(vm.new_array(keys)))
    });

    // Reflect.getPrototypeOf(target) -> Object | Null
    vm.define_method(&reflect, "getPrototypeOf", 1, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        if vm.is_proxy(&target) {
            return vm.proxy_get_prototype_of(&target);
        }
        let proto = target.borrow().proto.clone();
        Ok(match proto {
            Some(p) => Value::Object(p),
            None => Value::Null,
        })
    });

    // Reflect.setPrototypeOf(target, proto) -> Boolean
    vm.define_method(&reflect, "setPrototypeOf", 2, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let proto = match arg(args, 1) {
            Value::Object(p) => Some(p),
            Value::Null => None,
            _ => {
                return Err(
                    vm.throw_type("Reflect.setPrototypeOf: prototype must be an object or null")
                )
            }
        };
        // [[SetPrototypeOf]]: cycle + extensibility checks (and the Proxy trap).
        Ok(Value::Bool(object_set_prototype_of(vm, &target, proto)?))
    });

    // Reflect.defineProperty(target, propertyKey, attributes) -> Boolean
    vm.define_method(&reflect, "defineProperty", 3, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        let desc = arg(args, 2);
        // ToPropertyDescriptor requires an object.
        if !matches!(desc, Value::Object(_)) {
            return Err(vm.throw_type("Property description must be an object"));
        }
        if vm.is_proxy(&target) {
            return Ok(Value::Bool(vm.proxy_define_property(&target, &key, desc)?));
        }
        // Full OrdinaryDefineOwnProperty (validation, array-length/index exotics,
        // [[Extensible]]) with Throw=false: return its success boolean.
        let d = to_property_descriptor(vm, &desc)?;
        let ok = define_own_property(vm, &target, &key, &d, false)?;
        Ok(Value::Bool(ok))
    });

    // Reflect.getOwnPropertyDescriptor(target, propertyKey)
    vm.define_method(
        &reflect,
        "getOwnPropertyDescriptor",
        2,
        |vm, _this, args| {
            let target = require_object(vm, &arg(args, 0))?;
            let key = vm.to_property_key(&arg(args, 1))?;
            if vm.is_proxy(&target) {
                return vm.proxy_get_own_descriptor(&target, &key);
            }
            let prop = own_property_descriptor(&target, &key);
            match prop {
                None => Ok(Value::Undefined),
                Some(p) => Ok(descriptor_to_object(vm, &p)),
            }
        },
    );

    // Reflect.isExtensible(target) -> Boolean
    vm.define_method(&reflect, "isExtensible", 1, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        if vm.is_proxy(&target) {
            return Ok(Value::Bool(vm.proxy_is_extensible(&target)?));
        }
        let ext = target.borrow().extensible;
        Ok(Value::Bool(ext))
    });

    // Reflect.preventExtensions(target) -> Boolean
    vm.define_method(&reflect, "preventExtensions", 1, |vm, _this, args| {
        let target = require_object(vm, &arg(args, 0))?;
        if vm.is_proxy(&target) {
            return Ok(Value::Bool(vm.proxy_prevent_extensions(&target)?));
        }
        target.borrow_mut().extensible = false;
        Ok(Value::Bool(true))
    });

    // Reflect.apply(target, thisArgument, argumentsList)
    vm.define_method(&reflect, "apply", 3, |vm, _this, args| {
        let target = arg(args, 0);
        if !vm.is_callable(&target) {
            return Err(vm.throw_type("Reflect.apply target is not a function"));
        }
        let this_arg = arg(args, 1);
        let list = create_list_from_array_like(vm, &arg(args, 2))?;
        vm.call(target, this_arg, &list)
    });

    // Reflect.construct(target, argumentsList [, newTarget])
    vm.define_method(&reflect, "construct", 2, |vm, _this, args| {
        let target = arg(args, 0);
        if !vm.is_constructor(&target) {
            return Err(vm.throw_type("Reflect.construct target is not a constructor"));
        }
        let list = create_list_from_array_like(vm, &arg(args, 1))?;
        let new_target = if args.len() > 2 {
            let nt = arg(args, 2);
            if !vm.is_constructor(&nt) {
                return Err(vm.throw_type("Reflect.construct newTarget is not a constructor"));
            }
            nt
        } else {
            target.clone()
        };
        vm.construct(&target, &list, &new_target)
    });

    // Reflect[Symbol.toStringTag] = "Reflect" (non-enumerable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    vm.define_value_sym(&reflect, tag, Value::str("Reflect"));

    // Install as the (non-enumerable, writable, configurable) global `Reflect`.
    let global = vm.realm.global.clone();
    vm.define_value(&global, "Reflect", Value::Object(reflect));
}

/// `RequireObjectCoercible`-adjacent helper: returns the object or throws a
/// `TypeError`. Used by every Reflect method that operates on a target object.
fn require_object(vm: &mut Vm, v: &Value) -> Result<JsObject, Value> {
    match v {
        Value::Object(o) => Ok(o.clone()),
        _ => Err(vm.throw_type("Reflect target must be an object")),
    }
}

/// `CreateListFromArrayLike`: read `length`, then elements `0..length`.
fn create_list_from_array_like(vm: &mut Vm, v: &Value) -> Result<Vec<Value>, Value> {
    let o = match v {
        Value::Object(o) => o.clone(),
        _ => return Err(vm.throw_type("Reflect arguments list must be an object")),
    };
    let len_v = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("length"))?;
    let len = vm.to_length(&len_v)?;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(vm.get_prop(
            &Value::Object(o.clone()),
            &PropertyKey::from_index(i as u32),
        )?);
    }
    Ok(out)
}

/// Receiver-aware `[[Get]]`. When `receiver` is the target itself this matches
/// `vm.get_prop`; when a distinct receiver is supplied (Reflect.get's 3rd arg)
/// the receiver is threaded as `this` into any accessor getter, per spec.
fn reflect_get(
    vm: &mut Vm,
    target: &JsObject,
    key: &PropertyKey,
    receiver: Value,
) -> Result<Value, Value> {
    if vm.is_proxy(target) {
        return vm.proxy_get(target, key, receiver);
    }
    // Fast path: no distinct receiver -> ordinary get.
    if let Value::Object(r) = &receiver {
        if r.same(target) {
            return vm.get_prop(&Value::Object(target.clone()), key);
        }
    }
    let mut cur = target.clone();
    loop {
        let found = {
            let b = cur.borrow();
            if let Internal::Array(arr) = &b.internal {
                if let Some("length") = key.as_str() {
                    return Ok(Value::Number(arr.len() as f64));
                }
                if let Some(idx) = key.array_index() {
                    if let Some(v) = arr.get(idx as usize) {
                        return Ok(v.clone());
                    }
                }
            }
            if let Internal::StringObj(s) = &b.internal {
                if let Some("length") = key.as_str() {
                    return Ok(Value::Number(s.len_utf16() as f64));
                }
                if let Some(idx) = key.array_index() {
                    if let Some(u) = s.code_unit_at(idx as usize) {
                        return Ok(Value::String(JsString::from_code_units(&[u])));
                    }
                }
            }
            b.props.get(key).cloned()
        };
        match found {
            Some(prop) => match prop.kind {
                PropertyKind::Data { value, .. } => return Ok(value),
                PropertyKind::Accessor { get, .. } => {
                    return match get {
                        Some(getter) => vm.call(getter, receiver, &[]),
                        None => Ok(Value::Undefined),
                    }
                }
            },
            None => {
                let proto = cur.borrow().proto.clone();
                match proto {
                    Some(p) => cur = p,
                    None => return Ok(Value::Undefined),
                }
            }
        }
    }
}

/// Receiver-aware `[[Set]]`, returning whether the set succeeded. When the
/// receiver is the target this delegates to `vm.set_prop`. With a distinct
/// receiver, accessor setters receive `receiver` as `this`; a plain data write
/// is performed against the receiver object.
pub(crate) fn reflect_set(
    vm: &mut Vm,
    target: &JsObject,
    key: &PropertyKey,
    value: Value,
    receiver: Value,
) -> Result<bool, Value> {
    if vm.is_proxy(target) {
        return vm.proxy_set(target, key, value, receiver);
    }
    // Module Namespace exotic [[Set]]: always false, for any key.
    if matches!(
        target.borrow().internal,
        crate::value::Internal::ModuleNamespace(_)
    ) {
        return Ok(false);
    }
    // Fast path: receiver === target -> ordinary set (engine semantics).
    if let Value::Object(r) = &receiver {
        if r.same(target) {
            return set_ordinary(vm, &Value::Object(target.clone()), key, value);
        }
    }
    // Walk the chain looking for an accessor or a data property.
    let mut cur = target.clone();
    loop {
        // TypedArray integer-indexed exotic [[Set]] (10.4.5.5): a canonical
        // numeric key is handled by the typed array wherever it sits on the
        // chain — receiver===O writes the element (coercion side effects
        // included, out-of-bounds absorbed); otherwise an out-of-bounds index
        // is absorbed and a valid one behaves like a writable data property.
        if vm.ta_kind(&cur).is_some() {
            let n: Option<f64> = if let Some(i) = key.array_index() {
                Some(i as f64)
            } else {
                key.as_str().and_then(|s| {
                    if crate::vm::is_canonical_numeric(s) {
                        Some(s.parse::<f64>().unwrap_or(f64::NAN))
                    } else {
                        None
                    }
                })
            };
            if let Some(n) = n {
                if matches!(&receiver, Value::Object(r) if r.same(&cur)) {
                    // Engine [[Set]] on the typed array itself covers element
                    // writes and non-index numeric coercion.
                    vm.set_prop(&Value::Object(cur), key, value)?;
                    return Ok(true);
                }
                if !vm.ta_valid_index(&cur, n) {
                    return Ok(true);
                }
                break;
            }
        }
        let kind = {
            let b = cur.borrow();
            b.props.get(key).map(|p| match &p.kind {
                PropertyKind::Accessor { set, .. } => DescKind::Accessor(set.clone()),
                PropertyKind::Data { writable, .. } => DescKind::Data(*writable),
            })
        };
        match kind {
            Some(DescKind::Accessor(set)) => {
                return match set {
                    Some(setter) => {
                        vm.call(setter, receiver, &[value])?;
                        Ok(true)
                    }
                    None => Ok(false),
                };
            }
            Some(DescKind::Data(writable)) => {
                if !writable {
                    return Ok(false);
                }
                // Writable data property found on the chain: define on receiver.
                break;
            }
            None => {
                let proto = cur.borrow().proto.clone();
                match proto {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
    }
    // OrdinarySetWithOwnDescriptor step 3: finish on the RECEIVER with
    // [[GetOwnProperty]] + [[DefineOwnProperty]] — never a recursive [[Set]],
    // which would loop forever when a proxy receiver's `set` trap itself
    // calls `Reflect.set(target, key, value, receiver)`.
    let robj = match &receiver {
        Value::Object(o) => o.clone(),
        // Primitive receivers are not writable targets.
        _ => return Ok(false),
    };
    let new_desc = |vm: &mut Vm, value: Value, full: bool| -> Result<Value, Value> {
        let d = vm.new_object();
        let dv = Value::Object(d);
        vm.set_prop(&dv, &PropertyKey::str("value"), value)?;
        if full {
            for f in ["writable", "enumerable", "configurable"] {
                vm.set_prop(&dv, &PropertyKey::str(f), Value::Bool(true))?;
            }
        }
        Ok(dv)
    };
    if vm.is_proxy(&robj) {
        let existing = vm.proxy_get_own_descriptor(&robj, key)?;
        let dv = if let Value::Object(_) = &existing {
            let g = vm.get_prop(&existing, &PropertyKey::str("get"))?;
            let s = vm.get_prop(&existing, &PropertyKey::str("set"))?;
            if !g.is_undefined() || !s.is_undefined() {
                return Ok(false);
            }
            let w = vm.get_prop(&existing, &PropertyKey::str("writable"))?;
            if !vm.to_boolean(&w) {
                return Ok(false);
            }
            // Existing writable data property: define {value} only.
            new_desc(vm, value, false)?
        } else {
            // Absent: CreateDataProperty(receiver, key, value).
            new_desc(vm, value, true)?
        };
        return vm.proxy_define_property(&robj, key, dv);
    }
    match own_property_descriptor(&robj, key) {
        Some(p) => match &p.kind {
            PropertyKind::Accessor { .. } => Ok(false),
            PropertyKind::Data { writable, .. } => {
                if !*writable {
                    return Ok(false);
                }
                let dv = new_desc(vm, value, false)?;
                let d = to_property_descriptor(vm, &dv)?;
                define_own_property(vm, &robj, key, &d, false)
            }
        },
        None => {
            // CreateDataProperty(receiver, key, value): a define, so the
            // receiver's prototype chain is NOT consulted.
            let dv = new_desc(vm, value, true)?;
            let d = to_property_descriptor(vm, &dv)?;
            define_own_property(vm, &robj, key, &d, false)
        }
    }
}

enum DescKind {
    Accessor(Option<Value>),
    Data(bool),
}

/// Define-or-update an own data property on `receiver`, returning success.
fn set_ordinary(
    vm: &mut Vm,
    receiver: &Value,
    key: &PropertyKey,
    value: Value,
) -> Result<bool, Value> {
    let obj = match receiver {
        Value::Object(o) => o.clone(),
        // Primitives are not writable targets.
        _ => return Ok(false),
    };
    // Delegate the array/exotic and writability handling to the engine.
    vm.set_prop(&Value::Object(obj), key, value)?;
    Ok(true)
}

// --- Descriptor helpers (replicated from builtins/fundamental.rs; no shared
// pub helper exists) -------------------------------------------------------

fn descriptor_to_object(vm: &mut Vm, p: &Property) -> Value {
    let o = vm.new_object();
    {
        let mut b = o.borrow_mut();
        match &p.kind {
            PropertyKind::Data { value, writable } => {
                b.props
                    .insert(PropertyKey::str("value"), Property::data(value.clone()));
                b.props.insert(
                    PropertyKey::str("writable"),
                    Property::data(Value::Bool(*writable)),
                );
            }
            PropertyKind::Accessor { get, set } => {
                b.props.insert(
                    PropertyKey::str("get"),
                    Property::data(get.clone().unwrap_or(Value::Undefined)),
                );
                b.props.insert(
                    PropertyKey::str("set"),
                    Property::data(set.clone().unwrap_or(Value::Undefined)),
                );
            }
        }
        b.props.insert(
            PropertyKey::str("enumerable"),
            Property::data(Value::Bool(p.enumerable)),
        );
        b.props.insert(
            PropertyKey::str("configurable"),
            Property::data(Value::Bool(p.configurable)),
        );
    }
    Value::Object(o)
}
