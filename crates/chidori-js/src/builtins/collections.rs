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

/// Populate `target`'s `Internal::Map` from a Map-constructor iterable argument.
/// Close `it` and return `e` (the spec's IfAbruptCloseIterator).
fn close_with(vm: &mut Vm, it: &Value, e: Value) -> Value {
    let _ = vm.iterator_close(it);
    e
}

fn init_map_entries(vm: &mut Vm, target: &JsObject, init: &Value) -> Result<(), Value> {
    if init.is_nullish() {
        return Ok(());
    }
    // AddEntriesFromIterable: the OBSERVABLE `set` method is fetched once
    // (and must be callable — before any iteration), then invoked per entry;
    // an abrupt entry read or adder call closes the iterator.
    let tv = Value::Object(target.clone());
    let adder = vm.get_prop(&tv, &PropertyKey::str("set"))?;
    if !vm.is_callable(&adder) {
        return Err(vm.throw_type("Map constructor: 'set' is not callable"));
    }
    let it = vm.get_iterator(init)?;
    let next = vm.get_prop(&it, &PropertyKey::str("next"))?;
    loop {
        vm.native_tick()?;
        let res = vm.call(next.clone(), it.clone(), &[])?;
        if !matches!(res, Value::Object(_)) {
            return Err(vm.throw_type("iterator result is not an object"));
        }
        let done = vm.get_prop(&res, &PropertyKey::str("done"))?;
        if vm.to_boolean(&done) {
            return Ok(());
        }
        let item = vm.get_prop(&res, &PropertyKey::str("value"))?;
        // Each entry must be an Object; primitives (incl. strings) are rejected.
        if !matches!(item, Value::Object(_)) {
            let e = vm.throw_type("Iterator value is not an entry object");
            return Err(close_with(vm, &it, e));
        }
        let k = match vm.get_prop(&item, &PropertyKey::from_index(0)) {
            Ok(v) => v,
            Err(e) => return Err(close_with(vm, &it, e)),
        };
        let v = match vm.get_prop(&item, &PropertyKey::from_index(1)) {
            Ok(v) => v,
            Err(e) => return Err(close_with(vm, &it, e)),
        };
        if let Err(e) = vm.call(adder.clone(), tv.clone(), &[k, v]) {
            return Err(close_with(vm, &it, e));
        }
    }
}

/// Populate `target`'s `Internal::Set` from a Set-constructor iterable argument
/// (AddEntriesFromIterable shape: observable `add`, per-element calls,
/// abrupt completions close the iterator).
fn init_set_entries(vm: &mut Vm, target: &JsObject, init: &Value) -> Result<(), Value> {
    if init.is_nullish() {
        return Ok(());
    }
    let tv = Value::Object(target.clone());
    let adder = vm.get_prop(&tv, &PropertyKey::str("add"))?;
    if !vm.is_callable(&adder) {
        return Err(vm.throw_type("Set constructor: 'add' is not callable"));
    }
    let it = vm.get_iterator(init)?;
    let next = vm.get_prop(&it, &PropertyKey::str("next"))?;
    loop {
        vm.native_tick()?;
        let res = vm.call(next.clone(), it.clone(), &[])?;
        if !matches!(res, Value::Object(_)) {
            return Err(vm.throw_type("iterator result is not an object"));
        }
        let done = vm.get_prop(&res, &PropertyKey::str("done"))?;
        if vm.to_boolean(&done) {
            return Ok(());
        }
        let item = vm.get_prop(&res, &PropertyKey::str("value"))?;
        if let Err(e) = vm.call(adder.clone(), tv.clone(), &[item]) {
            return Err(close_with(vm, &it, e));
        }
    }
}

/// AddEntriesFromIterable for the Weak collections: fetch the observable
/// `adder` method (must be callable), then per item either unpack an `[k, v]`
/// entry object (`paired`) or pass the item through; abrupt completions
/// close the iterator.
fn init_weak_entries(
    vm: &mut Vm,
    target: &JsObject,
    init: &Value,
    adder_name: &str,
    paired: bool,
) -> Result<(), Value> {
    if init.is_nullish() {
        return Ok(());
    }
    let tv = Value::Object(target.clone());
    let adder = vm.get_prop(&tv, &PropertyKey::str(adder_name))?;
    if !vm.is_callable(&adder) {
        return Err(vm.throw_type(&format!("'{adder_name}' is not callable")));
    }
    let it = vm.get_iterator(init)?;
    let next = vm.get_prop(&it, &PropertyKey::str("next"))?;
    loop {
        vm.native_tick()?;
        let res = vm.call(next.clone(), it.clone(), &[])?;
        if !matches!(res, Value::Object(_)) {
            return Err(vm.throw_type("iterator result is not an object"));
        }
        let done = vm.get_prop(&res, &PropertyKey::str("done"))?;
        if vm.to_boolean(&done) {
            return Ok(());
        }
        let item = vm.get_prop(&res, &PropertyKey::str("value"))?;
        let call_args: Vec<Value> = if paired {
            if !matches!(item, Value::Object(_)) {
                let e = vm.throw_type("Iterator value is not an entry object");
                return Err(close_with(vm, &it, e));
            }
            let k = match vm.get_prop(&item, &PropertyKey::from_index(0)) {
                Ok(v) => v,
                Err(e) => return Err(close_with(vm, &it, e)),
            };
            let v = match vm.get_prop(&item, &PropertyKey::from_index(1)) {
                Ok(v) => v,
                Err(e) => return Err(close_with(vm, &it, e)),
            };
            vec![k, v]
        } else {
            vec![item]
        };
        if let Err(e) = vm.call(adder.clone(), tv.clone(), &call_args) {
            return Err(close_with(vm, &it, e));
        }
    }
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
            // AddEntriesFromIterable through the observable `set` method —
            // same shape as the Map constructor.
            init_weak_entries(vm, &m, &arg(args, 0), "set", true)?;
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
            // Per-element calls through the observable `add` method.
            init_weak_entries(vm, &s, &arg(args, 0), "add", false)?;
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
        |vm, _t, _args| Err(vm.throw_type("Constructor Map requires 'new'")),
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
        // LIVE iteration (no snapshot): entries added during the walk are
        // visited; a deleted-then-re-added entry moves to the end and is
        // visited again — same index discipline as the builtin iterators.
        let mut i = 0usize;
        loop {
            let entry = {
                if let Internal::Map(m) = &o.borrow().internal {
                    m.get_index(i).map(|(k, v)| (k.0.clone(), v.clone()))
                } else {
                    None
                }
            };
            match entry {
                Some((k, v)) => {
                    vm.call(cb.clone(), this_arg.clone(), &[v, k, this.clone()])?;
                    i += 1;
                }
                None => break,
            }
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
        |vm, _t, _args| Err(vm.throw_type("Constructor Set requires 'new'")),
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
        // LIVE iteration (no snapshot) — see Map.prototype.forEach above.
        let mut i = 0usize;
        loop {
            let entry = {
                if let Internal::Set(s) = &o.borrow().internal {
                    s.get_index(i).map(|(k, _)| k.0.clone())
                } else {
                    None
                }
            };
            match entry {
                Some(v) => {
                    vm.call(cb.clone(), this_arg.clone(), &[v.clone(), v, this.clone()])?;
                    i += 1;
                }
                None => break,
            }
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
            // -0 from a set-like canonicalizes to +0 on insertion.
            let k = if matches!(&k, Value::Number(n) if *n == 0.0) {
                Value::Number(0.0)
            } else {
                k
            };
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
            // LIVE walk of `this` (a `has` side effect that deletes a
            // not-yet-visited element makes it skipped).
            for_each_live_set_key(vm, &this, |vm, k| {
                if record.has(vm, &k)? && !result.iter().any(|e| same_value_zero(e, &k)) {
                    result.push(k);
                }
                Ok(true)
            })?;
        } else {
            for k in record.keys_to_vec(vm)? {
                // -0 from a set-like canonicalizes to +0 on insertion.
                let k = if matches!(&k, Value::Number(n) if *n == 0.0) {
                    Value::Number(0.0)
                } else {
                    k
                };
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
            // Smaller `this`: probe `other.has` for each element, walking
            // `this` LIVE (mid-walk deletions are skipped).
            let mut kept = Vec::new();
            for_each_live_set_key(vm, &this, |vm, k| {
                if !record.has(vm, &k)? {
                    kept.push(k);
                }
                Ok(true)
            })?;
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
        let mut subset = true;
        for_each_live_set_key(vm, &this, |vm, k| {
            if !record.has(vm, &k)? {
                subset = false;
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(Value::Bool(subset))
    });
    vm.define_method(&proto, "isSupersetOf", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        // A smaller set cannot be a superset.
        if (this_keys.len() as f64) < record.size {
            return Ok(Value::Bool(false));
        }
        let mut superset = true;
        record.for_each_key(vm, |vm, k| {
            if !set_has(vm, &this, &k)? {
                superset = false;
                return Ok(false); // early exit closes the keys iterator
            }
            Ok(true)
        })?;
        Ok(Value::Bool(superset))
    });
    vm.define_method(&proto, "isDisjointFrom", 1, |vm, this, args| {
        set_this(vm, &this)?;
        let record = get_set_record(vm, &arg(args, 0))?;
        let this_keys = set_keys_snapshot(vm, &this)?;
        // Iterate the smaller collection (size-based branch), mirroring the spec.
        if (this_keys.len() as f64) <= record.size {
            let mut disjoint = true;
            for_each_live_set_key(vm, &this, |vm, k| {
                if record.has(vm, &k)? {
                    disjoint = false;
                    return Ok(false);
                }
                Ok(true)
            })?;
            if !disjoint {
                return Ok(Value::Bool(false));
            }
        } else {
            let mut disjoint = true;
            record.for_each_key(vm, |vm, k| {
                if set_has(vm, &this, &k)? {
                    disjoint = false;
                    return Ok(false); // early exit closes the keys iterator
                }
                Ok(true)
            })?;
            if !disjoint {
                return Ok(Value::Bool(false));
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
/// Iterate `this`'s [[SetData]] LIVE (index-based, like the builtin
/// iterators): elements deleted mid-walk by a `has`/`keys` side effect are
/// skipped, appended ones visited. `f` returns `false` to stop early.
fn for_each_live_set_key(
    vm: &mut Vm,
    set: &Value,
    mut f: impl FnMut(&mut Vm, Value) -> Result<bool, Value>,
) -> Result<(), Value> {
    let o = match set {
        Value::Object(o) => o.clone(),
        _ => return Ok(()),
    };
    let mut i = 0usize;
    loop {
        let k = {
            if let Internal::Set(s) = &o.borrow().internal {
                s.get_index(i).map(|(k, _)| k.0.clone())
            } else {
                None
            }
        };
        match k {
            Some(k) => {
                if !f(vm, k)? {
                    return Ok(());
                }
                i += 1;
            }
            None => return Ok(()),
        }
    }
}

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

    /// Iterate `keys()` incrementally (cached next method); `f` returning
    /// `false` stops the walk and CLOSES the iterator (the spec's early-exit
    /// IteratorClose, observable via the iterator's `return`).
    fn for_each_key(
        &self,
        vm: &mut Vm,
        mut f: impl FnMut(&mut Vm, Value) -> Result<bool, Value>,
    ) -> Result<(), Value> {
        let it = vm.call(self.keys.clone(), self.obj.clone(), &[])?;
        if !matches!(it, Value::Object(_)) {
            return Err(vm.throw_type("set-like keys() did not return an object"));
        }
        let next = vm.get_prop(&it, &PropertyKey::str("next"))?;
        loop {
            vm.native_tick()?;
            let res = vm.call(next.clone(), it.clone(), &[])?;
            if !matches!(res, Value::Object(_)) {
                return Err(vm.throw_type("iterator result is not an object"));
            }
            let done = vm.get_prop(&res, &PropertyKey::str("done"))?;
            if vm.to_boolean(&done) {
                return Ok(());
            }
            let v = vm.get_prop(&res, &PropertyKey::str("value"))?;
            if !f(vm, v)? {
                let _ = vm.iterator_close(&it);
                return Ok(());
            }
        }
    }

    /// Invoke `this.[[Keys]]()` and drain the returned iterator to a `Vec`.
    fn keys_to_vec(&self, vm: &mut Vm) -> Result<Vec<Value>, Value> {
        let it = vm.call(self.keys.clone(), self.obj.clone(), &[])?;
        if !matches!(it, Value::Object(_)) {
            return Err(vm.throw_type("set-like keys() did not return an object"));
        }
        // Iterator record: Get "next" exactly once, call the cached method
        // per step (the spec's GetIteratorFromMethod + IteratorStepValue).
        let next = vm.get_prop(&it, &PropertyKey::str("next"))?;
        let mut out = Vec::new();
        loop {
            let res = vm.call(next.clone(), it.clone(), &[])?;
            if !matches!(res, Value::Object(_)) {
                return Err(vm.throw_type("iterator result is not an object"));
            }
            let done = vm.get_prop(&res, &PropertyKey::str("done"))?;
            if vm.to_boolean(&done) {
                break;
            }
            out.push(vm.get_prop(&res, &PropertyKey::str("value"))?);
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
