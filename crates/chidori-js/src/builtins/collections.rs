//! Map and Set. Both use insertion-ordered `IndexMap` storage with
//! `SameValueZero` keys, so iteration order is deterministic and
//! address-independent (determinism contract).
//!
//! Set additionally implements the ES2024 set-operation methods (`union`,
//! `intersection`, `difference`, `symmetricDifference`, `isSubsetOf`,
//! `isSupersetOf`, `isDisjointFrom`). Per spec these consume a "set-like"
//! argument (an object exposing a numeric `size`, a callable `has`, and a
//! callable `keys` returning an iterator) rather than requiring a real Set;
//! `get_set_record` performs the spec's GetSetRecord coercion.

use super::arg;
use crate::value::*;
use crate::vm::Vm;
use indexmap::IndexMap;

pub fn install(vm: &mut Vm) {
    install_map(vm);
    install_set(vm);
    install_weakmap(vm);
    install_weakset(vm);
}

/// When a native collection constructor is invoked *without* `new`, the only
/// valid case is a `super(...)` from a subclass (`class S extends Set {}`): then
/// `this` is the already-allocated derived instance whose prototype chain
/// includes the builtin's `proto`. Return it so the constructor can initialize
/// its internal slot in place. A bare `Set()`/`Map()` call (this = undefined or
/// the global) returns `None`, so the caller throws "requires 'new'".
fn super_target(this: &Value, proto: &JsObject) -> Option<JsObject> {
    let o = match this {
        Value::Object(o) => o.clone(),
        _ => return None,
    };
    let mut cur = o.borrow().proto.clone();
    while let Some(p) = cur {
        if p.same(proto) {
            return Some(o.clone());
        }
        cur = p.borrow().proto.clone();
    }
    None
}

/// Populate `target`'s `Internal::Map` from a Map-constructor iterable argument.
fn init_map_entries(vm: &mut Vm, target: &JsObject, init: &Value) -> Result<(), Value> {
    if init.is_nullish() {
        return Ok(());
    }
    let items = vm.iterate_to_vec(init)?;
    for item in items {
        // Each entry must be an Object; primitives (incl. strings) are rejected.
        if !matches!(item, Value::Object(_)) {
            return Err(vm.throw_type("Iterator value is not an entry object"));
        }
        let k = vm.get_prop(&item, &PropertyKey::from_index(0))?;
        let v = vm.get_prop(&item, &PropertyKey::from_index(1))?;
        if let Internal::Map(map) = &mut target.borrow_mut().internal {
            map.insert(MapKey(k), v);
        }
    }
    Ok(())
}

/// Populate `target`'s `Internal::Set` from a Set-constructor iterable argument.
fn init_set_entries(vm: &mut Vm, target: &JsObject, init: &Value) -> Result<(), Value> {
    if init.is_nullish() {
        return Ok(());
    }
    let items = vm.iterate_to_vec(init)?;
    for item in items {
        if let Internal::Set(set) = &mut target.borrow_mut().internal {
            set.insert(MapKey(item), ());
        }
    }
    Ok(())
}

/// A value that "can be held weakly": an object or a (non-registered) symbol.
/// Anything else is an invalid WeakMap/WeakSet key/value → TypeError.
fn can_be_held_weakly(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Symbol(_))
}

fn weakmap_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::WeakMap(_)) => Ok(o.clone()),
        _ => Err(vm.throw_type("Method WeakMap.prototype called on incompatible receiver")),
    }
}

fn weakset_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::WeakSet(_)) => Ok(o.clone()),
        _ => Err(vm.throw_type("Method WeakSet.prototype called on incompatible receiver")),
    }
}

fn install_weakmap(vm: &mut Vm) {
    let proto = vm.realm.weak_map_proto.clone();
    let ctor = vm.new_native_ctor(
        "WeakMap",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor WeakMap requires 'new'")),
        |vm, _t, args| {
            let m = vm.alloc(ObjectData::new(
                Some(vm.realm.weak_map_proto.clone()),
                Internal::WeakMap(IndexMap::new()),
            ));
            let init = arg(args, 0);
            if !init.is_nullish() {
                let items = vm.iterate_to_vec(&init)?;
                for item in items {
                    if !matches!(item, Value::Object(_)) {
                        return Err(vm.throw_type("Iterator value is not an entry object"));
                    }
                    let k = vm.get_prop(&item, &PropertyKey::from_index(0))?;
                    let v = vm.get_prop(&item, &PropertyKey::from_index(1))?;
                    if !can_be_held_weakly(&k) {
                        return Err(vm.throw_type("Invalid value used as weak map key"));
                    }
                    if let Internal::WeakMap(map) = &mut m.borrow_mut().internal {
                        map.insert(MapKey(k), v);
                    }
                }
            }
            Ok(Value::Object(m))
        },
    );
    vm.install_ctor("WeakMap", &ctor, &proto);

    vm.define_method(&proto, "set", 2, |vm, this, args| {
        let o = weakmap_this(vm, &this)?;
        let k = arg(args, 0);
        if !can_be_held_weakly(&k) {
            return Err(vm.throw_type("Invalid value used as weak map key"));
        }
        if let Internal::WeakMap(m) = &mut o.borrow_mut().internal {
            m.insert(MapKey(k), arg(args, 1));
        }
        Ok(this)
    });
    vm.define_method(&proto, "get", 1, |vm, this, args| {
        let o = weakmap_this(vm, &this)?;
        let b = o.borrow();
        if let Internal::WeakMap(m) = &b.internal {
            Ok(m.get(&MapKey(arg(args, 0)))
                .cloned()
                .unwrap_or(Value::Undefined))
        } else {
            Ok(Value::Undefined)
        }
    });
    vm.define_method(&proto, "has", 1, |vm, this, args| {
        let o = weakmap_this(vm, &this)?;
        let b = o.borrow();
        if let Internal::WeakMap(m) = &b.internal {
            Ok(Value::Bool(m.contains_key(&MapKey(arg(args, 0)))))
        } else {
            Ok(Value::Bool(false))
        }
    });
    vm.define_method(&proto, "delete", 1, |vm, this, args| {
        let o = weakmap_this(vm, &this)?;
        let removed = if let Internal::WeakMap(m) = &mut o.borrow_mut().internal {
            m.shift_remove(&MapKey(arg(args, 0))).is_some()
        } else {
            false
        };
        Ok(Value::Bool(removed))
    });
    define_to_string_tag(vm, &proto, "WeakMap");
}

fn install_weakset(vm: &mut Vm) {
    let proto = vm.realm.weak_set_proto.clone();
    let ctor = vm.new_native_ctor(
        "WeakSet",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor WeakSet requires 'new'")),
        |vm, _t, args| {
            let s = vm.alloc(ObjectData::new(
                Some(vm.realm.weak_set_proto.clone()),
                Internal::WeakSet(IndexMap::new()),
            ));
            let init = arg(args, 0);
            if !init.is_nullish() {
                let items = vm.iterate_to_vec(&init)?;
                for item in items {
                    if !can_be_held_weakly(&item) {
                        return Err(vm.throw_type("Invalid value used in weak set"));
                    }
                    if let Internal::WeakSet(set) = &mut s.borrow_mut().internal {
                        set.insert(MapKey(item), ());
                    }
                }
            }
            Ok(Value::Object(s))
        },
    );
    vm.install_ctor("WeakSet", &ctor, &proto);

    vm.define_method(&proto, "add", 1, |vm, this, args| {
        let o = weakset_this(vm, &this)?;
        let v = arg(args, 0);
        if !can_be_held_weakly(&v) {
            return Err(vm.throw_type("Invalid value used in weak set"));
        }
        if let Internal::WeakSet(s) = &mut o.borrow_mut().internal {
            s.insert(MapKey(v), ());
        }
        Ok(this)
    });
    vm.define_method(&proto, "has", 1, |vm, this, args| {
        let o = weakset_this(vm, &this)?;
        let b = o.borrow();
        if let Internal::WeakSet(s) = &b.internal {
            Ok(Value::Bool(s.contains_key(&MapKey(arg(args, 0)))))
        } else {
            Ok(Value::Bool(false))
        }
    });
    vm.define_method(&proto, "delete", 1, |vm, this, args| {
        let o = weakset_this(vm, &this)?;
        let removed = if let Internal::WeakSet(s) = &mut o.borrow_mut().internal {
            s.shift_remove(&MapKey(arg(args, 0))).is_some()
        } else {
            false
        };
        Ok(Value::Bool(removed))
    });
    define_to_string_tag(vm, &proto, "WeakSet");
}

fn install_map(vm: &mut Vm) {
    let proto = vm.realm.map_proto.clone();
    let ctor = vm.new_native_ctor(
        "Map",
        0,
        |vm, t, args| {
            // Only reachable via `super(...)` from a subclass; initialize in place.
            let proto = vm.realm.map_proto.clone();
            let target = super_target(&t, &proto)
                .ok_or_else(|| vm.throw_type("Constructor Map requires 'new'"))?;
            target.borrow_mut().internal = Internal::Map(IndexMap::new());
            init_map_entries(vm, &target, &arg(args, 0))?;
            Ok(Value::Undefined)
        },
        |vm, _t, args| {
            let m = vm.alloc(ObjectData::new(
                Some(vm.realm.map_proto.clone()),
                Internal::Map(IndexMap::new()),
            ));
            init_map_entries(vm, &m, &arg(args, 0))?;
            Ok(Value::Object(m))
        },
    );
    vm.install_ctor("Map", &ctor, &proto);
    vm.install_species(&ctor);

    vm.define_method(&ctor, "groupBy", 2, |vm, _t, args| {
        let items = arg(args, 0);
        let cb = arg(args, 1);
        if items.is_nullish() {
            return Err(vm.throw_type("Map.groupBy called on null or undefined"));
        }
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("Map.groupBy callback is not a function"));
        }
        // Group by the callback result using Map key equality (SameValueZero,
        // with -0 canonicalized to +0), preserving first-seen key order.
        let values = vm.iterate_to_vec(&items)?;
        let mut groups: IndexMap<MapKey, Vec<Value>> = IndexMap::new();
        for (i, v) in values.into_iter().enumerate() {
            let mut key = vm.call(
                cb.clone(),
                Value::Undefined,
                &[v.clone(), Value::Number(i as f64)],
            )?;
            if matches!(&key, Value::Number(n) if *n == 0.0) {
                key = Value::Number(0.0); // canonicalize -0 to +0
            }
            groups.entry(MapKey(key)).or_default().push(v);
        }
        let mut map: IndexMap<MapKey, Value> = IndexMap::new();
        for (k, elements) in groups {
            let arr = vm.new_array(elements);
            map.insert(k, Value::Object(arr));
        }
        let m = vm.alloc(ObjectData::new(
            Some(vm.realm.map_proto.clone()),
            Internal::Map(map),
        ));
        Ok(Value::Object(m))
    });

    vm.define_method(&proto, "set", 2, |vm, this, args| {
        let o = map_this(vm, &this)?;
        if let Internal::Map(m) = &mut o.borrow_mut().internal {
            m.insert(MapKey(arg(args, 0)), arg(args, 1));
        }
        Ok(this)
    });
    vm.define_method(&proto, "get", 1, |vm, this, args| {
        let o = map_this(vm, &this)?;
        let b = o.borrow();
        if let Internal::Map(m) = &b.internal {
            Ok(m.get(&MapKey(arg(args, 0)))
                .cloned()
                .unwrap_or(Value::Undefined))
        } else {
            Ok(Value::Undefined)
        }
    });
    vm.define_method(&proto, "has", 1, |vm, this, args| {
        let o = map_this(vm, &this)?;
        let b = o.borrow();
        if let Internal::Map(m) = &b.internal {
            Ok(Value::Bool(m.contains_key(&MapKey(arg(args, 0)))))
        } else {
            Ok(Value::Bool(false))
        }
    });
    vm.define_method(&proto, "delete", 1, |vm, this, args| {
        let o = map_this(vm, &this)?;
        let removed = if let Internal::Map(m) = &mut o.borrow_mut().internal {
            m.shift_remove(&MapKey(arg(args, 0))).is_some()
        } else {
            false
        };
        Ok(Value::Bool(removed))
    });
    vm.define_method(&proto, "clear", 0, |vm, this, _a| {
        let o = map_this(vm, &this)?;
        if let Internal::Map(m) = &mut o.borrow_mut().internal {
            m.clear();
        }
        Ok(Value::Undefined)
    });
    vm.define_method(&proto, "forEach", 1, |vm, this, args| {
        let o = map_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("Map.prototype.forEach callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let entries: Vec<(Value, Value)> = {
            if let Internal::Map(m) = &o.borrow().internal {
                m.iter().map(|(k, v)| (k.0.clone(), v.clone())).collect()
            } else {
                vec![]
            }
        };
        for (k, v) in entries {
            vm.call(cb.clone(), this_arg.clone(), &[v, k, this.clone()])?;
        }
        Ok(Value::Undefined)
    });
    define_size_getter(vm, &proto, true);
    vm.define_method(&proto, "keys", 0, |vm, this, _a| {
        let o = map_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.map_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::MapKeys,
        ))
    });
    vm.define_method(&proto, "values", 0, |vm, this, _a| {
        let o = map_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.map_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::MapValues,
        ))
    });
    vm.define_method(&proto, "entries", 0, |vm, this, _a| {
        let o = map_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.map_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::MapEntries,
        ))
    });
    let entries = vm
        .get_prop(&Value::Object(proto.clone()), &PropertyKey::str("entries"))
        .unwrap();
    let sym = vm.realm.symbol_iterator.clone();
    vm.define_value_sym(&proto, sym, entries);

    define_to_string_tag(vm, &proto, "Map");
}

fn install_set(vm: &mut Vm) {
    let proto = vm.realm.set_proto.clone();
    let ctor = vm.new_native_ctor(
        "Set",
        0,
        |vm, t, args| {
            // Only reachable via `super(...)` from a subclass; initialize in place.
            let proto = vm.realm.set_proto.clone();
            let target = super_target(&t, &proto)
                .ok_or_else(|| vm.throw_type("Constructor Set requires 'new'"))?;
            target.borrow_mut().internal = Internal::Set(IndexMap::new());
            init_set_entries(vm, &target, &arg(args, 0))?;
            Ok(Value::Undefined)
        },
        |vm, _t, args| {
            let s = vm.alloc(ObjectData::new(
                Some(vm.realm.set_proto.clone()),
                Internal::Set(IndexMap::new()),
            ));
            init_set_entries(vm, &s, &arg(args, 0))?;
            Ok(Value::Object(s))
        },
    );
    vm.install_ctor("Set", &ctor, &proto);
    vm.install_species(&ctor);

    vm.define_method(&proto, "add", 1, |vm, this, args| {
        let o = set_this(vm, &this)?;
        if let Internal::Set(s) = &mut o.borrow_mut().internal {
            s.insert(MapKey(arg(args, 0)), ());
        }
        Ok(this)
    });
    vm.define_method(&proto, "has", 1, |vm, this, args| {
        let o = set_this(vm, &this)?;
        let b = o.borrow();
        if let Internal::Set(s) = &b.internal {
            Ok(Value::Bool(s.contains_key(&MapKey(arg(args, 0)))))
        } else {
            Ok(Value::Bool(false))
        }
    });
    vm.define_method(&proto, "delete", 1, |vm, this, args| {
        let o = set_this(vm, &this)?;
        let removed = if let Internal::Set(s) = &mut o.borrow_mut().internal {
            s.shift_remove(&MapKey(arg(args, 0))).is_some()
        } else {
            false
        };
        Ok(Value::Bool(removed))
    });
    vm.define_method(&proto, "clear", 0, |vm, this, _a| {
        let o = set_this(vm, &this)?;
        if let Internal::Set(s) = &mut o.borrow_mut().internal {
            s.clear();
        }
        Ok(Value::Undefined)
    });
    vm.define_method(&proto, "forEach", 1, |vm, this, args| {
        let o = set_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("Set.prototype.forEach callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let items: Vec<Value> = {
            if let Internal::Set(s) = &o.borrow().internal {
                s.keys().map(|k| k.0.clone()).collect()
            } else {
                vec![]
            }
        };
        for v in items {
            vm.call(cb.clone(), this_arg.clone(), &[v.clone(), v, this.clone()])?;
        }
        Ok(Value::Undefined)
    });
    define_size_getter(vm, &proto, false);
    vm.define_method(&proto, "values", 0, |vm, this, _a| {
        let o = set_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.set_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::SetValues,
        ))
    });
    vm.define_method(&proto, "entries", 0, |vm, this, _a| {
        let o = set_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.set_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::SetEntries,
        ))
    });

    // ES2024 set-operation methods. Each takes a "set-like" argument.
    vm.define_method(&proto, "union", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        // Result starts as a copy of `this`, then adds every key of `other`.
        let mut result = set_keys_snapshot(vm, &this)?;
        let other_keys = record.keys_to_vec(vm)?;
        for k in other_keys {
            if !result.iter().any(|e| same_value_zero(e, &k)) {
                result.push(k);
            }
        }
        Ok(new_set(vm, result))
    });
    vm.define_method(&proto, "intersection", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        let mut result = Vec::new();
        // Spec branches on size: iterate the *smaller* collection. When `this` is
        // smaller, probe `other.has`; otherwise iterate `other.keys()` and probe
        // `this` internally (so `other.has` is not invoked).
        if (this_keys.len() as f64) <= record.size {
            for k in this_keys {
                if record.has(vm, &k)? && !result.iter().any(|e| same_value_zero(e, &k)) {
                    result.push(k);
                }
            }
        } else {
            for k in record.keys_to_vec(vm)? {
                if set_has(vm, &this, &k)? && !result.iter().any(|e| same_value_zero(e, &k)) {
                    result.push(k);
                }
            }
        }
        Ok(new_set(vm, result))
    });
    vm.define_method(&proto, "difference", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        let mut result = this_keys.clone();
        if (this_keys.len() as f64) <= record.size {
            // Smaller `this`: probe `other.has` for each of this's elements.
            let mut kept = Vec::new();
            for k in this_keys {
                if !record.has(vm, &k)? {
                    kept.push(k);
                }
            }
            result = kept;
        } else {
            // Larger `this`: iterate `other.keys()` and remove matches from the
            // copy — per spec this must NOT call `other.has`.
            for k in record.keys_to_vec(vm)? {
                if let Some(pos) = result.iter().position(|e| same_value_zero(e, &k)) {
                    result.remove(pos);
                }
            }
        }
        Ok(new_set(vm, result))
    });
    vm.define_method(&proto, "symmetricDifference", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        // Start from a copy of `this`, then iterate `other.keys()` (never calls
        // `other.has`): an element in both is removed, one only in `other` added.
        let mut result = this_keys;
        let other_keys = record.keys_to_vec(vm)?;
        for k in other_keys {
            let k = if matches!(&k, Value::Number(n) if *n == 0.0) {
                Value::Number(0.0) // canonicalize -0 → +0
            } else {
                k
            };
            if let Some(pos) = result.iter().position(|e| same_value_zero(e, &k)) {
                result.remove(pos);
            } else {
                result.push(k);
            }
        }
        Ok(new_set(vm, result))
    });
    vm.define_method(&proto, "isSubsetOf", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        // A larger set cannot be a subset (spec short-circuits before probing).
        if (this_keys.len() as f64) > record.size {
            return Ok(Value::Bool(false));
        }
        for k in this_keys {
            if !record.has(vm, &k)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    vm.define_method(&proto, "isSupersetOf", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        // A smaller set cannot be a superset.
        if (this_keys.len() as f64) < record.size {
            return Ok(Value::Bool(false));
        }
        for k in record.keys_to_vec(vm)? {
            if !set_has(vm, &this, &k)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    vm.define_method(&proto, "isDisjointFrom", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        // Iterate the smaller collection (size-based branch), mirroring the spec.
        if (this_keys.len() as f64) <= record.size {
            for k in this_keys {
                if record.has(vm, &k)? {
                    return Ok(Value::Bool(false));
                }
            }
        } else {
            for k in record.keys_to_vec(vm)? {
                if set_has(vm, &this, &k)? {
                    return Ok(Value::Bool(false));
                }
            }
        }
        Ok(Value::Bool(true))
    });

    let values = vm
        .get_prop(&Value::Object(proto.clone()), &PropertyKey::str("values"))
        .unwrap();
    vm.define_value(&proto, "keys", values.clone());
    let sym = vm.realm.symbol_iterator.clone();
    vm.define_value_sym(&proto, sym, values);

    define_to_string_tag(vm, &proto, "Set");
}

/// Define a `size` accessor (getter only) on `proto`. The getter throws a
/// TypeError when invoked on a receiver lacking the appropriate internal slot.
fn define_size_getter(vm: &mut Vm, proto: &JsObject, is_map: bool) {
    let getter = vm.new_native("get size", 0, move |vm, this, _a| {
        let len = match &this {
            Value::Object(o) => match &o.borrow().internal {
                Internal::Map(m) if is_map => m.len(),
                Internal::Set(s) if !is_map => s.len(),
                _ => {
                    return Err(vm.throw_type(if is_map {
                        "get Map.prototype.size called on incompatible receiver"
                    } else {
                        "get Set.prototype.size called on incompatible receiver"
                    }))
                }
            },
            _ => {
                return Err(vm.throw_type(if is_map {
                    "get Map.prototype.size called on incompatible receiver"
                } else {
                    "get Set.prototype.size called on incompatible receiver"
                }))
            }
        };
        Ok(Value::Number(len as f64))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("size"),
        Some(Value::Object(getter)),
        None,
    );
}

/// Install a non-enumerable, non-writable, configurable `Symbol.toStringTag`.
fn define_to_string_tag(vm: &mut Vm, proto: &JsObject, tag: &str) {
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

fn map_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::Map(_)) => Ok(o.clone()),
        _ => Err(vm.throw_type("Method Map.prototype called on incompatible receiver")),
    }
}

fn set_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::Set(_)) => Ok(o.clone()),
        _ => Err(vm.throw_type("Method Set.prototype called on incompatible receiver")),
    }
}

/// Snapshot the elements of a Set receiver to a `Vec` (no live borrow held).
fn set_keys_snapshot(vm: &mut Vm, this: &Value) -> Result<Vec<Value>, Value> {
    let o = set_this(vm, this)?;
    let keys = if let Internal::Set(s) = &o.borrow().internal {
        s.keys().map(|k| k.0.clone()).collect()
    } else {
        Vec::new()
    };
    Ok(keys)
}

/// SameValueZero membership test against a Set receiver.
fn set_has(vm: &mut Vm, this: &Value, key: &Value) -> Result<bool, Value> {
    let o = set_this(vm, this)?;
    let found = if let Internal::Set(s) = &o.borrow().internal {
        s.contains_key(&MapKey(key.clone()))
    } else {
        false
    };
    Ok(found)
}

/// Build a fresh `Set` from a list of values (deduplicated by SameValueZero,
/// preserving first-insertion order).
fn new_set(vm: &mut Vm, values: Vec<Value>) -> Value {
    let mut map: IndexMap<MapKey, ()> = IndexMap::new();
    for v in values {
        map.insert(MapKey(v), ());
    }
    Value::Object(vm.alloc(ObjectData::new(
        Some(vm.realm.set_proto.clone()),
        Internal::Set(map),
    )))
}

/// A spec "Set Record": the coerced view of a set-like argument used by the
/// ES2024 set-operation methods.
struct SetRecord {
    obj: Value,
    size: f64,
    has: Value,
    keys: Value,
}

impl SetRecord {
    /// Invoke `this.[[Has]](key)` and coerce the result to a boolean.
    fn has(&self, vm: &mut Vm, key: &Value) -> Result<bool, Value> {
        let r = vm.call(self.has.clone(), self.obj.clone(), &[key.clone()])?;
        Ok(vm.to_boolean(&r))
    }

    /// Invoke `this.[[Keys]]()` and drain the returned iterator to a `Vec`.
    fn keys_to_vec(&self, vm: &mut Vm) -> Result<Vec<Value>, Value> {
        let it = vm.call(self.keys.clone(), self.obj.clone(), &[])?;
        if !matches!(it, Value::Object(_)) {
            return Err(vm.throw_type("set-like keys() did not return an object"));
        }
        let mut out = Vec::new();
        loop {
            match vm.iterator_step(&it)? {
                Some(v) => out.push(v),
                None => break,
            }
        }
        Ok(out)
    }
}

/// GetSetRecord(obj): coerce a set-like argument. Requires an Object with a
/// numeric `size`, a callable `has`, and a callable `keys`. Throws TypeError
/// (or RangeError for a negative size) otherwise.
fn get_set_record(vm: &mut Vm, obj: &Value) -> Result<SetRecord, Value> {
    if !matches!(obj, Value::Object(_)) {
        return Err(vm.throw_type("argument is not an object"));
    }
    // size: ToNumber, reject NaN; ToIntegerOrInfinity, reject negative.
    let raw_size = vm.get_prop(obj, &PropertyKey::str("size"))?;
    let num_size = vm.to_number(&raw_size)?;
    if num_size.is_nan() {
        return Err(vm.throw_type("set-like size is NaN"));
    }
    let int_size = if num_size.is_infinite() {
        num_size
    } else {
        num_size.trunc()
    };
    if int_size < 0.0 {
        return Err(vm.throw_range("set-like size is negative"));
    }
    let has = vm.get_prop(obj, &PropertyKey::str("has"))?;
    if !vm.is_callable(&has) {
        return Err(vm.throw_type("set-like has is not callable"));
    }
    let keys = vm.get_prop(obj, &PropertyKey::str("keys"))?;
    if !vm.is_callable(&keys) {
        return Err(vm.throw_type("set-like keys is not callable"));
    }
    Ok(SetRecord {
        obj: obj.clone(),
        size: int_size,
        has,
        keys,
    })
}
