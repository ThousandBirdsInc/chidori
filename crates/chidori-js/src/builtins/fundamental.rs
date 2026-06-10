//! Object, Function, Symbol, Error hierarchy, and Boolean.

use super::arg;
use crate::regexp::is_regexp;
use crate::value::*;
use crate::vm::{ErrorKind, Vm};

pub fn install(vm: &mut Vm) {
    install_object(vm);
    install_object_extra(vm);
    install_function(vm);
    install_symbol(vm);
    install_boolean(vm);
    install_errors(vm);
}

fn this_object(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    vm.to_object(this)
}

// =========================================================================
// Property-descriptor parsing and the OrdinaryDefineOwnProperty algorithm.
// =========================================================================

/// A parsed property descriptor (ToPropertyDescriptor). `None` fields mean the
/// field was absent (used for partial-descriptor merging).
#[derive(Clone, Default)]
pub(crate) struct PropDesc {
    value: Option<Value>,
    writable: Option<bool>,
    get: Option<Value>, // present field; the value may be Value::Undefined
    set: Option<Value>,
    has_get: bool,
    has_set: bool,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}

impl PropDesc {
    fn is_accessor(&self) -> bool {
        self.has_get || self.has_set
    }
    fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }
    fn is_generic(&self) -> bool {
        !self.is_accessor() && !self.is_data()
    }
}

/// ToPropertyDescriptor (spec 6.2.6.5). Validates the shape: `Desc` must be an
/// object, `get`/`set` must be callable or undefined.
pub(crate) fn to_property_descriptor(vm: &mut Vm, desc: &Value) -> Result<PropDesc, Value> {
    if !matches!(desc, Value::Object(_)) {
        return Err(vm.throw_type("Property description must be an object"));
    }
    let mut d = PropDesc::default();

    if vm.has_prop(desc, &PropertyKey::str("enumerable"))? {
        let v = vm.get_prop(desc, &PropertyKey::str("enumerable"))?;
        d.enumerable = Some(vm.to_boolean(&v));
    }
    if vm.has_prop(desc, &PropertyKey::str("configurable"))? {
        let v = vm.get_prop(desc, &PropertyKey::str("configurable"))?;
        d.configurable = Some(vm.to_boolean(&v));
    }
    if vm.has_prop(desc, &PropertyKey::str("value"))? {
        d.value = Some(vm.get_prop(desc, &PropertyKey::str("value"))?);
    }
    if vm.has_prop(desc, &PropertyKey::str("writable"))? {
        let v = vm.get_prop(desc, &PropertyKey::str("writable"))?;
        d.writable = Some(vm.to_boolean(&v));
    }
    if vm.has_prop(desc, &PropertyKey::str("get"))? {
        let g = vm.get_prop(desc, &PropertyKey::str("get"))?;
        if !g.is_undefined() && !vm.is_callable(&g) {
            return Err(vm.throw_type("Getter must be a function or undefined"));
        }
        d.has_get = true;
        d.get = Some(g);
    }
    if vm.has_prop(desc, &PropertyKey::str("set"))? {
        let s = vm.get_prop(desc, &PropertyKey::str("set"))?;
        if !s.is_undefined() && !vm.is_callable(&s) {
            return Err(vm.throw_type("Setter must be a function or undefined"));
        }
        d.has_set = true;
        d.set = Some(s);
    }
    if d.is_accessor() && d.is_data() {
        return Err(
            vm.throw_type("Invalid property descriptor. Cannot both specify accessors and a value or writable attribute")
        );
    }
    Ok(d)
}

/// CompletePropertyDescriptor-ish conversion to a concrete `Property` for a
/// brand-new property (absent fields default to false / undefined).
fn complete_to_property(d: &PropDesc) -> Property {
    if d.is_accessor() {
        Property {
            kind: PropertyKind::Accessor {
                get: match &d.get {
                    Some(g) if !g.is_undefined() => Some(g.clone()),
                    _ => None,
                },
                set: match &d.set {
                    Some(s) if !s.is_undefined() => Some(s.clone()),
                    _ => None,
                },
            },
            enumerable: d.enumerable.unwrap_or(false),
            configurable: d.configurable.unwrap_or(false),
        }
    } else {
        Property {
            kind: PropertyKind::Data {
                value: d.value.clone().unwrap_or(Value::Undefined),
                writable: d.writable.unwrap_or(false),
            },
            enumerable: d.enumerable.unwrap_or(false),
            configurable: d.configurable.unwrap_or(false),
        }
    }
}

/// Reify an array's exotic own index/length property into a real `Property`
/// (so it can participate in the ordinary define-own algorithm). Returns the
/// current descriptor or `None` if absent.
fn array_exotic_current(b: &ObjectData, key: &PropertyKey) -> Option<Property> {
    if let Internal::Array(arr) = &b.internal {
        if let Some("length") = key.as_str() {
            return Some(Property {
                kind: PropertyKind::Data {
                    value: Value::Number(arr.len() as f64),
                    // length is writable unless the props map records otherwise.
                    writable: b
                        .props
                        .get(key)
                        .map(
                            |p| matches!(&p.kind, PropertyKind::Data { writable, .. } if *writable),
                        )
                        .unwrap_or(true),
                },
                enumerable: false,
                configurable: false,
            });
        }
        if let Some(idx) = key.array_index() {
            if let Some(v) = arr.get(idx as usize) {
                return Some(Property {
                    kind: PropertyKind::Data {
                        value: v.clone(),
                        writable: true,
                    },
                    enumerable: true,
                    configurable: true,
                });
            }
        }
    }
    None
}

/// OrdinaryDefineOwnProperty + ValidateAndApplyPropertyDescriptor (spec
/// 10.1.6). Returns `Ok(true)` on success; throws on the configurable
/// invariants when `throw_on_fail` is set, otherwise returns `Ok(false)`.
pub(crate) fn define_own_property(
    vm: &mut Vm,
    obj: &JsObject,
    key: &PropertyKey,
    d: &PropDesc,
    throw_on_fail: bool,
) -> Result<bool, Value> {
    // Integer-indexed exotic [[DefineOwnProperty]] (spec 10.4.5.3): a canonical
    // numeric index on a TypedArray is validated and written through the element
    // setter; it never becomes an ordinary `props` entry.
    if vm.ta_kind(obj).is_some() {
        if let Some(n) = canonical_numeric_index(key) {
            return define_typed_array_index(vm, obj, n, d, throw_on_fail);
        }
    }
    // ArraySetLength coercion (steps 3–5) runs BEFORE any descriptor validation:
    // `ToUint32` then `ToNumber` can invoke a user `valueOf` that itself redefines
    // `length`, and a newLen≠numberLen mismatch is a RangeError that must precede
    // the configurable/writable checks. Substitute the coerced value, then snapshot
    // `current` afterwards so post-valueOf state is what gets validated.
    let obj_is_array = matches!(obj.borrow().internal, Internal::Array(_));
    let mut coerced_desc;
    let d: &PropDesc = if obj_is_array && key.as_str() == Some("length") && d.value.is_some() {
        let v = d.value.as_ref().unwrap();
        let new_len = vm.to_uint32(v)?;
        let number_len = vm.to_number(v)?;
        if (new_len as f64) != number_len {
            return Err(vm.throw_range("Invalid array length"));
        }
        coerced_desc = d.clone();
        coerced_desc.value = Some(Value::Number(new_len as f64));
        &coerced_desc
    } else {
        d
    };
    // Snapshot current state without holding the borrow across vm.* calls.
    let (current, extensible, is_array) = {
        let b = obj.borrow();
        let cur = match b.props.get(key) {
            Some(p) => Some(p.clone()),
            None => array_exotic_current(&b, key),
        };
        (cur, b.extensible, matches!(b.internal, Internal::Array(_)))
    };

    let fail = |vm: &Vm| -> Result<bool, Value> {
        if throw_on_fail {
            Err(vm.throw_type("Cannot redefine property"))
        } else {
            Ok(false)
        }
    };

    let current = match current {
        None => {
            // New property: require extensibility.
            if !extensible {
                return fail(vm);
            }
            let prop = complete_to_property(d);
            store_property(vm, obj, key, prop, is_array)?;
            return Ok(true);
        }
        Some(c) => c,
    };

    // Every field of d is absent => no-op success.
    if d.value.is_none()
        && d.writable.is_none()
        && !d.has_get
        && !d.has_set
        && d.enumerable.is_none()
        && d.configurable.is_none()
    {
        return Ok(true);
    }

    let cur_configurable = current.configurable;
    let cur_enumerable = current.enumerable;
    let cur_is_accessor = matches!(current.kind, PropertyKind::Accessor { .. });

    // Non-configurable invariants.
    if !cur_configurable {
        if d.configurable == Some(true) {
            return fail(vm);
        }
        if let Some(e) = d.enumerable {
            if e != cur_enumerable {
                return fail(vm);
            }
        }
    }

    if d.is_generic() {
        // Only enumerable/configurable changes; already validated above.
    } else if cur_is_accessor != d.is_accessor() {
        // Changing the category (data<->accessor) requires configurable.
        if !cur_configurable {
            return fail(vm);
        }
    } else if !cur_is_accessor {
        // Both data descriptors.
        if !cur_configurable {
            let cur_writable =
                matches!(&current.kind, PropertyKind::Data { writable, .. } if *writable);
            if !cur_writable {
                if d.writable == Some(true) {
                    return fail(vm);
                }
                if let Some(nv) = &d.value {
                    let cur_val = match &current.kind {
                        PropertyKind::Data { value, .. } => value.clone(),
                        _ => Value::Undefined,
                    };
                    if !same_value(nv, &cur_val) {
                        return fail(vm);
                    }
                }
            }
        }
    } else {
        // Both accessor descriptors.
        if !cur_configurable {
            let (cur_get, cur_set) = match &current.kind {
                PropertyKind::Accessor { get, set } => (get.clone(), set.clone()),
                _ => (None, None),
            };
            if d.has_set {
                let new_set = d.set.clone().unwrap_or(Value::Undefined);
                let cur_set_v = cur_set.unwrap_or(Value::Undefined);
                if !same_value(&new_set, &cur_set_v) {
                    return fail(vm);
                }
            }
            if d.has_get {
                let new_get = d.get.clone().unwrap_or(Value::Undefined);
                let cur_get_v = cur_get.unwrap_or(Value::Undefined);
                if !same_value(&new_get, &cur_get_v) {
                    return fail(vm);
                }
            }
        }
    }

    // ArraySetLength: reducing an array's `length` past a non-configurable
    // element is not allowed. Shrink only down to the highest such index + 1
    // (dropping the deletable tail) and then report failure.
    if is_array && key.as_str() == Some("length") {
        if let Some(v) = &d.value {
            let n = vm.to_number(v)?;
            let new_len = n as usize;
            if (new_len as f64) != n || n < 0.0 {
                return Err(vm.throw_range("Invalid array length"));
            }
            let block = {
                let b = obj.borrow();
                let cur_len = match &b.internal {
                    Internal::Array(arr) => arr.len(),
                    _ => 0,
                };
                let mut block: Option<usize> = None;
                if new_len < cur_len {
                    for (k, p) in b.props.iter() {
                        if let Some(idx) = k.array_index() {
                            let idx = idx as usize;
                            if idx >= new_len && idx < cur_len && !p.configurable {
                                block = Some(block.map_or(idx, |b| b.max(idx)));
                            }
                        }
                    }
                }
                block
            };
            if let Some(k) = block {
                let mut b = obj.borrow_mut();
                if let Internal::Array(arr) = &mut b.internal {
                    arr.resize(k + 1, Value::Hole);
                }
                let drop_keys: Vec<PropertyKey> = b
                    .props
                    .keys()
                    .filter(|kk| kk.array_index().is_some_and(|i| (i as usize) > k))
                    .cloned()
                    .collect();
                for kk in drop_keys {
                    b.props.shift_remove(&kk);
                }
                drop(b);
                return fail(vm);
            }
        }
    }

    // Apply: build the merged property from current + provided fields.
    let merged = merge_descriptor(&current, d);
    store_property(vm, obj, key, merged, is_array)?;
    Ok(true)
}

/// The canonical numeric index value for a property key, if it is one: an array
/// index key, or a string key that round-trips through `Number→String`
/// (including `"-0"`, `"NaN"`, `"Infinity"`). Used for the TypedArray exotics.
pub(crate) fn canonical_numeric_index(key: &PropertyKey) -> Option<f64> {
    if let Some(idx) = key.array_index() {
        return Some(idx as f64);
    }
    let s = key.as_str()?;
    if crate::vm::is_canonical_numeric(s) {
        s.parse::<f64>().ok()
    } else {
        None
    }
}

/// Integer-indexed exotic [[DefineOwnProperty]] for a TypedArray element index.
fn define_typed_array_index(
    vm: &mut Vm,
    obj: &JsObject,
    n: f64,
    d: &PropDesc,
    throw_on_fail: bool,
) -> Result<bool, Value> {
    let fail = |vm: &Vm| -> Result<bool, Value> {
        if throw_on_fail {
            Err(vm.throw_type("Cannot redefine typed array index"))
        } else {
            Ok(false)
        }
    };
    if !vm.ta_valid_index(obj, n)
        || d.configurable == Some(false)
        || d.enumerable == Some(false)
        || d.is_accessor()
        || d.writable == Some(false)
    {
        return fail(vm);
    }
    if let Some(v) = d.value.clone() {
        vm.ta_write(obj, n as usize, &v)?;
    }
    Ok(true)
}

/// `CreateDataPropertyOrThrow(O, P, V)`: define a default data property (all
/// attributes true) via [[DefineOwnProperty]], throwing TypeError if it fails.
pub(crate) fn create_data_property_or_throw(
    vm: &mut Vm,
    obj: &JsObject,
    key: &PropertyKey,
    value: Value,
) -> Result<(), Value> {
    let ok = if vm.is_proxy(obj) {
        let desc = vm.new_object();
        vm.set_prop(
            &Value::Object(desc.clone()),
            &PropertyKey::str("value"),
            value,
        )?;
        let t = Value::Bool(true);
        vm.set_prop(
            &Value::Object(desc.clone()),
            &PropertyKey::str("writable"),
            t.clone(),
        )?;
        vm.set_prop(
            &Value::Object(desc.clone()),
            &PropertyKey::str("enumerable"),
            t.clone(),
        )?;
        vm.set_prop(
            &Value::Object(desc.clone()),
            &PropertyKey::str("configurable"),
            t,
        )?;
        vm.proxy_define_property(obj, key, Value::Object(desc))?
    } else {
        let d = PropDesc {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        };
        define_own_property(vm, obj, key, &d, false)?
    };
    if !ok {
        return Err(vm.throw_type("Cannot create property"));
    }
    Ok(())
}

/// Merge a partial descriptor `d` onto an existing `Property`.
fn merge_descriptor(current: &Property, d: &PropDesc) -> Property {
    let enumerable = d.enumerable.unwrap_or(current.enumerable);
    let configurable = d.configurable.unwrap_or(current.configurable);

    let kind = if d.is_accessor() {
        // Result is an accessor; keep current accessor fields when absent, but
        // if current was a data property those default to undefined.
        let (cur_get, cur_set) = match &current.kind {
            PropertyKind::Accessor { get, set } => (get.clone(), set.clone()),
            _ => (None, None),
        };
        let get = if d.has_get {
            match &d.get {
                Some(g) if !g.is_undefined() => Some(g.clone()),
                _ => None,
            }
        } else {
            cur_get
        };
        let set = if d.has_set {
            match &d.set {
                Some(s) if !s.is_undefined() => Some(s.clone()),
                _ => None,
            }
        } else {
            cur_set
        };
        PropertyKind::Accessor { get, set }
    } else if d.is_data() {
        // Result is a data property.
        let (cur_val, cur_writable) = match &current.kind {
            PropertyKind::Data { value, writable } => (value.clone(), *writable),
            // Converting accessor -> data: absent value/writable default to
            // undefined/false.
            _ => (Value::Undefined, false),
        };
        PropertyKind::Data {
            value: d.value.clone().unwrap_or(cur_val),
            writable: d.writable.unwrap_or(cur_writable),
        }
    } else {
        // Generic: keep current kind unchanged.
        current.kind.clone()
    };

    Property {
        kind,
        enumerable,
        configurable,
    }
}

/// Write a fully-formed `Property` into an object, routing array index/length
/// writes through the dense backing store when possible.
fn store_property(
    vm: &mut Vm,
    obj: &JsObject,
    key: &PropertyKey,
    prop: Property,
    is_array: bool,
) -> Result<(), Value> {
    if is_array {
        // Array length: a plain writable data length just resizes the backing
        // store; a non-default attribute set is recorded in props as a marker.
        if let Some("length") = key.as_str() {
            if let PropertyKind::Data { value, writable } = &prop.kind {
                let n = vm.to_number(value)?;
                let len = n as usize;
                {
                    let mut b = obj.borrow_mut();
                    if let Internal::Array(arr) = &mut b.internal {
                        if (len as f64) != n || n < 0.0 {
                            return Err(vm.throw_range("Invalid array length"));
                        }
                        if len > crate::value::MAX_DENSE_ARRAY {
                            return Err(vm.throw_range("Array allocation exceeds engine limit"));
                        }
                        // Growing length introduces holes, not undefined slots.
                        arr.resize(len, Value::Hole);
                    }
                }
                // Record non-writable length as a marker property so that
                // freeze/isFrozen and the writable invariant are observable.
                if !*writable {
                    obj.borrow_mut().props.insert(
                        key.clone(),
                        Property {
                            kind: PropertyKind::Data {
                                value: Value::Number(len as f64),
                                writable: false,
                            },
                            enumerable: false,
                            configurable: false,
                        },
                    );
                } else {
                    obj.borrow_mut().props.shift_remove(key);
                }
                return Ok(());
            }
        }
        // Array index with a plain default data descriptor -> dense store. A
        // non-default attribute set falls through to be reified into `props`
        // below; vm.rs get/set/delete consult that entry, shadowing the dense slot.
        if let Some(idx) = key.array_index() {
            if let PropertyKind::Data { value, writable } = &prop.kind {
                if *writable && prop.enumerable && prop.configurable {
                    let idx = idx as usize;
                    let mut b = obj.borrow_mut();
                    let is_arr = matches!(b.internal, Internal::Array(_));
                    if is_arr {
                        if let Internal::Array(arr) = &mut b.internal {
                            if idx >= arr.len() {
                                if idx >= crate::value::MAX_DENSE_ARRAY {
                                    drop(b);
                                    return Err(vm.throw_range("Array index exceeds engine limit"));
                                }
                                arr.resize(idx + 1, Value::Undefined);
                            }
                            arr[idx] = value.clone();
                        }
                        b.props.shift_remove(key);
                        return Ok(());
                    }
                }
            }
        }
    }
    // Array exotic [[DefineOwnProperty]]: defining an own index property at or
    // past `length` must update `length` to index+1. Grow the dense backing with
    // holes so the (length == arr.len()) invariant holds; the `props` entry we
    // insert below shadows that hole at this index.
    if is_array {
        if let Some(idx) = key.array_index() {
            let idx = idx as usize;
            let mut b = obj.borrow_mut();
            if let Internal::Array(arr) = &mut b.internal {
                if idx >= arr.len() {
                    if idx >= crate::value::MAX_DENSE_ARRAY {
                        drop(b);
                        return Err(vm.throw_range("Array index exceeds engine limit"));
                    }
                    arr.resize(idx + 1, Value::Hole);
                }
            }
        }
    }
    obj.borrow_mut().props.insert(key.clone(), prop);
    Ok(())
}

fn install_object(vm: &mut Vm) {
    let proto = vm.realm.object_proto.clone();

    vm.define_method(&proto, "hasOwnProperty", 1, |vm, this, args| {
        let key = vm.to_property_key(&arg(args, 0))?;
        let o = this_object(vm, &this)?;
        let has = own_property_exists(&o, &key);
        Ok(Value::Bool(has))
    });

    vm.define_method(&proto, "isPrototypeOf", 1, |_vm, this, args| {
        let v = arg(args, 0);
        let target = match &this {
            Value::Object(o) => o.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        let mut cur = match &v {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        while let Some(p) = cur {
            if p.same(&target) {
                return Ok(Value::Bool(true));
            }
            cur = p.borrow().proto.clone();
        }
        Ok(Value::Bool(false))
    });

    vm.define_method(&proto, "propertyIsEnumerable", 1, |vm, this, args| {
        let key = vm.to_property_key(&arg(args, 0))?;
        let o = this_object(vm, &this)?;
        let b = o.borrow();
        let e = match b.props.get(&key) {
            Some(p) => p.enumerable,
            None => match &b.internal {
                Internal::Array(arr) => key
                    .array_index()
                    .and_then(|i| arr.get(i as usize))
                    .map(|v| !matches!(v, Value::Hole))
                    .unwrap_or(false),
                Internal::StringObj(s) => key
                    .array_index()
                    .map(|i| (i as usize) < s.as_str().chars().count())
                    .unwrap_or(false),
                _ => false,
            },
        };
        Ok(Value::Bool(e))
    });

    vm.define_method(&proto, "valueOf", 0, |vm, this, _args| {
        Ok(Value::Object(this_object(vm, &this)?))
    });

    // Annex B legacy accessor helpers.
    vm.define_method(&proto, "__defineGetter__", 2, |vm, this, args| {
        let o = this_object(vm, &this)?;
        let getter = arg(args, 1);
        if !vm.is_callable(&getter) {
            return Err(vm.throw_type("Object.prototype.__defineGetter__: Expecting function"));
        }
        let key = vm.to_property_key(&arg(args, 0))?;
        // Spec: DefinePropertyOrThrow(O, key, {[[Get]]: getter,
        // [[Enumerable]]: true, [[Configurable]]: true}). The partial descriptor
        // has no [[Set]] field, so an existing setter is preserved.
        define_accessor_or_throw(vm, &o, key, getter, true)?;
        Ok(Value::Undefined)
    });
    vm.define_method(&proto, "__defineSetter__", 2, |vm, this, args| {
        let o = this_object(vm, &this)?;
        let setter = arg(args, 1);
        if !vm.is_callable(&setter) {
            return Err(vm.throw_type("Object.prototype.__defineSetter__: Expecting function"));
        }
        let key = vm.to_property_key(&arg(args, 0))?;
        // Spec: DefinePropertyOrThrow(O, key, {[[Set]]: setter,
        // [[Enumerable]]: true, [[Configurable]]: true}). No [[Get]] field, so an
        // existing getter is preserved.
        define_accessor_or_throw(vm, &o, key, setter, false)?;
        Ok(Value::Undefined)
    });
    vm.define_method(&proto, "__lookupGetter__", 1, |vm, this, args| {
        let o = this_object(vm, &this)?;
        let key = vm.to_property_key(&arg(args, 0))?;
        lookup_accessor(vm, &o, &key, true)
    });
    vm.define_method(&proto, "__lookupSetter__", 1, |vm, this, args| {
        let o = this_object(vm, &this)?;
        let key = vm.to_property_key(&arg(args, 0))?;
        lookup_accessor(vm, &o, &key, false)
    });

    // Annex B: Object.prototype.__proto__ is a (non-enumerable, configurable)
    // accessor backing the object's [[Prototype]].
    let proto_get = vm.new_native("get __proto__", 0, |vm, this, _args| {
        let o = vm.to_object(&this)?;
        if vm.is_proxy(&o) {
            return vm.proxy_get_prototype_of(&o);
        }
        let proto = o.borrow().proto.clone();
        Ok(match proto {
            Some(p) => Value::Object(p),
            None => Value::Null,
        })
    });
    let proto_set = vm.new_native("set __proto__", 1, |vm, this, args| {
        // RequireObjectCoercible(this).
        if this.is_nullish() {
            return Err(vm.throw_type("Object.prototype.__proto__ called on null or undefined"));
        }
        let proto = match arg(args, 0) {
            Value::Object(p) => Some(p),
            Value::Null => None,
            // Neither Object nor Null: per spec, return undefined (a no-op).
            _ => return Ok(Value::Undefined),
        };
        if let Value::Object(o) = &this {
            if !object_set_prototype_of(vm, o, proto)? {
                return Err(vm.throw_type("Object.prototype.__proto__: cannot set prototype"));
            }
        }
        Ok(Value::Undefined)
    });
    proto.borrow_mut().props.insert(
        PropertyKey::str("__proto__"),
        Property {
            kind: PropertyKind::Accessor {
                get: Some(Value::Object(proto_get)),
                set: Some(Value::Object(proto_set)),
            },
            enumerable: false,
            configurable: true,
        },
    );

    // Object.prototype.toString honoring a string-valued Symbol.toStringTag
    // (spec 20.1.3.6). For null/undefined this returns the fixed tags.
    vm.define_method(&proto, "toString", 0, |vm, this, _args| {
        match &this {
            Value::Undefined => return Ok(Value::str("[object Undefined]")),
            Value::Null => return Ok(Value::str("[object Null]")),
            _ => {}
        }
        let o = vm.to_object(&this)?;
        // Spec 20.1.3.6: IsArray (proxy-aware) → Callable → other internal slots.
        let builtin_tag = if is_array_exotic(vm, &o)? {
            "Array"
        } else if vm.is_callable(&Value::Object(o.clone())) {
            "Function"
        } else if is_regexp(&Value::Object(o.clone())) {
            "RegExp"
        } else {
            let b = o.borrow();
            match &b.internal {
                Internal::Error => "Error",
                Internal::Boolean(_) => "Boolean",
                Internal::Number(_) => "Number",
                Internal::StringObj(_) => "String",
                Internal::Date(_) => "Date",
                Internal::Arguments => "Arguments",
                _ => "Object",
            }
        };
        let tag_sym = vm.realm.symbol_to_string_tag.clone();
        let tag_val = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::Sym(tag_sym))?;
        let tag = match &tag_val {
            Value::String(s) => s.as_str().to_string(),
            _ => builtin_tag.to_string(),
        };
        Ok(Value::str(format!("[object {tag}]")))
    });
    // toLocaleString defers to this.toString().
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _args| {
        let f = vm.get_prop(&this, &PropertyKey::str("toString"))?;
        vm.call(f, this, &[])
    });

    // Object constructor.
    let ctor = vm.new_native_ctor(
        "Object",
        1,
        |vm, _this, args| {
            let v = arg(args, 0);
            if v.is_nullish() {
                Ok(Value::Object(vm.new_object()))
            } else {
                Ok(Value::Object(vm.to_object(&v)?))
            }
        },
        |vm, _this, args| {
            let v = arg(args, 0);
            if v.is_nullish() {
                Ok(Value::Object(vm.new_object()))
            } else {
                Ok(Value::Object(vm.to_object(&v)?))
            }
        },
    );
    vm.install_ctor("Object", &ctor, &proto);

    vm.define_method(&ctor, "keys", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let keys: Vec<Value> = enumerable_own_strings_dyn(vm, &o)?
            .into_iter()
            .map(Value::String)
            .collect();
        Ok(Value::Object(vm.new_array(keys)))
    });
    vm.define_method(&ctor, "values", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        // EnumerableOwnProperties: per key, [[GetOwnProperty]] then (if
        // enumerable) [[Get]], interleaved — observable through a Proxy.
        let keys = vm.own_property_keys(&o)?;
        let mut out = Vec::new();
        for k in keys {
            if !matches!(k, PropertyKey::Str(_)) {
                continue;
            }
            if vm.own_key_enumerable(&o, &k)? {
                out.push(vm.get_prop(&Value::Object(o.clone()), &k)?);
            }
        }
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(&ctor, "entries", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let keys = vm.own_property_keys(&o)?;
        let mut out = Vec::new();
        for k in keys {
            let ks = match &k {
                PropertyKey::Str(s) => s.clone(),
                _ => continue,
            };
            if vm.own_key_enumerable(&o, &k)? {
                let val = vm.get_prop(&Value::Object(o.clone()), &k)?;
                out.push(Value::Object(vm.new_array(vec![Value::String(ks), val])));
            }
        }
        Ok(Value::Object(vm.new_array(out)))
    });
    vm.define_method(&ctor, "groupBy", 2, |vm, _t, args| {
        let items = arg(args, 0);
        let cb = arg(args, 1);
        if items.is_nullish() {
            return Err(vm.throw_type("Object.groupBy called on null or undefined"));
        }
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("Object.groupBy callback is not a function"));
        }
        // Group elements by `ToPropertyKey(callback(value, index))`, preserving
        // first-seen key order and per-group element order.
        let values = vm.iterate_to_vec(&items)?;
        let mut groups: indexmap::IndexMap<PropertyKey, Vec<Value>> = indexmap::IndexMap::new();
        for (i, v) in values.into_iter().enumerate() {
            let key_v = vm.call(
                cb.clone(),
                Value::Undefined,
                &[v.clone(), Value::Number(i as f64)],
            )?;
            let key = vm.to_property_key(&key_v)?;
            groups.entry(key).or_default().push(v);
        }
        // Result is a null-prototype object; each group's elements become an Array.
        let obj = JsObject::ordinary(None);
        for (key, elements) in groups {
            let arr = vm.new_array(elements);
            obj.borrow_mut()
                .props
                .insert(key, Property::data(Value::Object(arr)));
        }
        Ok(Value::Object(obj))
    });
    vm.define_method(&ctor, "assign", 2, |vm, _t, args| {
        let target = vm.to_object(&arg(args, 0))?;
        for src in &args[1.min(args.len())..] {
            if src.is_nullish() {
                continue;
            }
            let so = vm.to_object(src)?;
            // Copy every enumerable own key (string and symbol), invoking
            // getters on the source and setters on the target. Proxy-aware.
            for k in vm.enumerable_own_keys_dyn(&so)? {
                let val = vm.get_prop(&Value::Object(so.clone()), &k)?;
                // Object.assign uses Set(target, key, value, true): a failed
                // write (read-only / non-extensible target) throws a TypeError.
                vm.set_prop_strict(&Value::Object(target.clone()), &k, val)?;
            }
        }
        Ok(Value::Object(target))
    });
    vm.define_method(&ctor, "freeze", 1, |_vm, _t, args| {
        if let Value::Object(o) = arg(args, 0) {
            let mut b = o.borrow_mut();
            b.extensible = false;
            // For arrays, record a non-writable length marker so isFrozen and
            // the writable invariant are observable.
            if let Internal::Array(arr) = &b.internal {
                let len = arr.len();
                b.props.insert(
                    PropertyKey::str("length"),
                    Property {
                        kind: PropertyKind::Data {
                            value: Value::Number(len as f64),
                            writable: false,
                        },
                        enumerable: false,
                        configurable: false,
                    },
                );
            }
            for (_, p) in b.props.iter_mut() {
                p.configurable = false;
                if let PropertyKind::Data { writable, .. } = &mut p.kind {
                    *writable = false;
                }
            }
            drop(b);
            return Ok(Value::Object(o));
        }
        Ok(arg(args, 0))
    });
    vm.define_method(&ctor, "isFrozen", 1, |_vm, _t, args| {
        match arg(args, 0) {
            Value::Object(o) => {
                let b = o.borrow();
                if b.extensible {
                    return Ok(Value::Bool(false));
                }
                // Arrays with any live dense elements are not frozen (those
                // index slots are writable/configurable data properties).
                if let Internal::Array(arr) = &b.internal {
                    if !arr.is_empty() {
                        return Ok(Value::Bool(false));
                    }
                    // Length must be recorded non-writable.
                    let len_ok = b
                        .props
                        .get(&PropertyKey::str("length"))
                        .map(
                            |p| matches!(&p.kind, PropertyKind::Data { writable, .. } if !writable),
                        )
                        .unwrap_or(false);
                    if !len_ok {
                        return Ok(Value::Bool(false));
                    }
                }
                let frozen = b.props.values().all(|p| {
                    !p.configurable
                        && match &p.kind {
                            PropertyKind::Data { writable, .. } => !writable,
                            PropertyKind::Accessor { .. } => true,
                        }
                });
                Ok(Value::Bool(frozen))
            }
            _ => Ok(Value::Bool(true)),
        }
    });
    vm.define_method(&ctor, "preventExtensions", 1, |vm, _t, args| {
        if let Value::Object(o) = arg(args, 0) {
            if vm.is_proxy(&o) {
                vm.proxy_prevent_extensions(&o)?;
                return Ok(Value::Object(o));
            }
            o.borrow_mut().extensible = false;
            return Ok(Value::Object(o));
        }
        Ok(arg(args, 0))
    });
    vm.define_method(&ctor, "isExtensible", 1, |vm, _t, args| {
        match arg(args, 0) {
            Value::Object(o) if vm.is_proxy(&o) => Ok(Value::Bool(vm.proxy_is_extensible(&o)?)),
            Value::Object(o) => Ok(Value::Bool(o.borrow().extensible)),
            _ => Ok(Value::Bool(false)),
        }
    });
    vm.define_method(&ctor, "create", 2, |vm, _t, args| {
        let proto = match arg(args, 0) {
            Value::Object(o) => Some(o),
            Value::Null => None,
            _ => return Err(vm.throw_type("Object prototype may only be an Object or null")),
        };
        let obj = JsObject::ordinary(proto);
        let props = arg(args, 1);
        if !props.is_undefined() {
            define_properties(vm, &obj, &props)?;
        }
        Ok(Value::Object(obj))
    });
    vm.define_method(&ctor, "getPrototypeOf", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        if vm.is_proxy(&o) {
            return vm.proxy_get_prototype_of(&o);
        }
        let proto = o.borrow().proto.clone();
        Ok(match proto {
            Some(p) => Value::Object(p),
            None => Value::Null,
        })
    });
    vm.define_method(&ctor, "setPrototypeOf", 2, |vm, _t, args| {
        let v = arg(args, 0);
        // RequireObjectCoercible(O).
        if v.is_nullish() {
            return Err(vm.throw_type("Object.setPrototypeOf called on null or undefined"));
        }
        let proto = match arg(args, 1) {
            Value::Object(p) => Some(p),
            Value::Null => None,
            _ => return Err(vm.throw_type("Object prototype may only be an Object or null")),
        };
        if let Value::Object(o) = &v {
            if !object_set_prototype_of(vm, o, proto)? {
                return Err(vm.throw_type("Object.setPrototypeOf: cannot set prototype"));
            }
        }
        Ok(v)
    });
    vm.define_method(&ctor, "defineProperty", 3, |vm, _t, args| {
        let o = match arg(args, 0) {
            Value::Object(o) => o,
            _ => return Err(vm.throw_type("Object.defineProperty called on non-object")),
        };
        let key = vm.to_property_key(&arg(args, 1))?;
        let desc = arg(args, 2);
        if vm.is_proxy(&o) {
            // Validate the descriptor shape, then dispatch the trap with the raw
            // attributes object.
            to_property_descriptor(vm, &desc)?;
            if !vm.proxy_define_property(&o, &key, desc)? {
                return Err(vm.throw_type("proxy defineProperty trap returned falsish"));
            }
            return Ok(Value::Object(o));
        }
        let d = to_property_descriptor(vm, &desc)?;
        define_own_property(vm, &o, &key, &d, true)?;
        Ok(Value::Object(o))
    });
    vm.define_method(&ctor, "defineProperties", 2, |vm, _t, args| {
        let o = match arg(args, 0) {
            Value::Object(o) => o,
            _ => return Err(vm.throw_type("Object.defineProperties called on non-object")),
        };
        define_properties(vm, &o, &arg(args, 1))?;
        Ok(Value::Object(o))
    });
    vm.define_method(&ctor, "getOwnPropertyNames", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let names: Vec<Value> = vm
            .own_property_keys(&o)?
            .into_iter()
            .filter_map(|k| match k {
                PropertyKey::Str(s) => Some(Value::String(s)),
                PropertyKey::Sym(_) => None,
            })
            .collect();
        Ok(Value::Object(vm.new_array(names)))
    });
    vm.define_method(&ctor, "getOwnPropertyDescriptor", 2, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        if vm.is_proxy(&o) {
            return vm.proxy_get_own_descriptor(&o, &key);
        }
        let prop = own_property_descriptor(&o, &key);
        match prop {
            None => Ok(Value::Undefined),
            Some(p) => Ok(descriptor_to_object(vm, &p)),
        }
    });
    vm.define_method(&ctor, "fromEntries", 1, |vm, _t, args| {
        // RequireObjectCoercible.
        if arg(args, 0).is_nullish() {
            return Err(vm.throw_type("Object.fromEntries requires an iterable"));
        }
        let obj = vm.new_object();
        // Iterate lazily and process each entry as it is produced (the spec's
        // observable order). On any error while handling an entry, the iterator
        // is closed before the error propagates.
        let iter = vm.get_iterator(&arg(args, 0))?;
        loop {
            let item = match vm.iterator_step(&iter)? {
                Some(v) => v,
                None => break,
            };
            // Each step's body is fallible; close the iterator on error.
            let handle = (|vm: &mut Vm| -> Result<(), Value> {
                if !matches!(item, Value::Object(_)) {
                    return Err(vm.throw_type("Iterator value is not an entry object"));
                }
                let k = vm.get_prop(&item, &PropertyKey::from_index(0))?;
                let v = vm.get_prop(&item, &PropertyKey::from_index(1))?;
                let key = vm.to_property_key(&k)?;
                let d = PropDesc {
                    value: Some(v),
                    writable: Some(true),
                    enumerable: Some(true),
                    configurable: Some(true),
                    ..Default::default()
                };
                define_own_property(vm, &obj, &key, &d, true)?;
                Ok(())
            })(vm);
            if let Err(e) = handle {
                let _ = vm.iterator_close(&iter);
                return Err(e);
            }
        }
        Ok(Value::Object(obj))
    });
    vm.define_method(&ctor, "is", 2, |_vm, _t, args| {
        Ok(Value::Bool(same_value(&arg(args, 0), &arg(args, 1))))
    });
}

fn install_object_extra(vm: &mut Vm) {
    let ctor = match vm.get_prop(
        &Value::Object(vm.realm.global.clone()),
        &PropertyKey::str("Object"),
    ) {
        Ok(Value::Object(o)) => o,
        _ => return,
    };
    vm.define_method(&ctor, "getOwnPropertyDescriptors", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let result = vm.new_object();
        for key in vm.own_keys(&o) {
            let prop = own_property_descriptor(&o, &key);
            if let Some(p) = prop {
                let desc = descriptor_to_object(vm, &p);
                result.borrow_mut().props.insert(key, Property::data(desc));
            }
        }
        Ok(Value::Object(result))
    });
    vm.define_method(&ctor, "getOwnPropertySymbols", 1, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let syms: Vec<Value> = vm
            .own_property_keys(&o)?
            .into_iter()
            .filter_map(|k| match k {
                PropertyKey::Sym(s) => Some(Value::Symbol(s)),
                PropertyKey::Str(_) => None,
            })
            .collect();
        Ok(Value::Object(vm.new_array(syms)))
    });
    vm.define_method(&ctor, "seal", 1, |_vm, _t, args| {
        if let Value::Object(o) = arg(args, 0) {
            let mut b = o.borrow_mut();
            b.extensible = false;
            // For arrays, reify a non-configurable length marker.
            if let Internal::Array(arr) = &b.internal {
                let len = arr.len();
                let writable = b
                    .props
                    .get(&PropertyKey::str("length"))
                    .map(|p| matches!(&p.kind, PropertyKind::Data { writable, .. } if *writable))
                    .unwrap_or(true);
                b.props.insert(
                    PropertyKey::str("length"),
                    Property {
                        kind: PropertyKind::Data {
                            value: Value::Number(len as f64),
                            writable,
                        },
                        enumerable: false,
                        configurable: false,
                    },
                );
            }
            for (_, p) in b.props.iter_mut() {
                p.configurable = false;
            }
            drop(b);
            return Ok(Value::Object(o));
        }
        Ok(arg(args, 0))
    });
    vm.define_method(&ctor, "isSealed", 1, |_vm, _t, args| {
        match arg(args, 0) {
            Value::Object(o) => {
                let b = o.borrow();
                if b.extensible {
                    return Ok(Value::Bool(false));
                }
                // Arrays with dense elements have configurable index props.
                if let Internal::Array(arr) = &b.internal {
                    if !arr.is_empty() {
                        return Ok(Value::Bool(false));
                    }
                    let len_present = b
                        .props
                        .get(&PropertyKey::str("length"))
                        .map(|p| !p.configurable)
                        .unwrap_or(false);
                    if !len_present {
                        return Ok(Value::Bool(false));
                    }
                }
                let sealed = b.props.values().all(|p| !p.configurable);
                Ok(Value::Bool(sealed))
            }
            _ => Ok(Value::Bool(true)),
        }
    });
    vm.define_method(&ctor, "hasOwn", 2, |vm, _t, args| {
        let o = vm.to_object(&arg(args, 0))?;
        let key = vm.to_property_key(&arg(args, 1))?;
        Ok(Value::Bool(own_property_exists(&o, &key)))
    });
}

/// A private name (`#x`), modeled internally as a `"#x"` string key. These must
/// be invisible to all reflection (hasOwnProperty, getOwnPropertyDescriptor, ...).
pub(crate) fn is_private_key(key: &PropertyKey) -> bool {
    matches!(key.as_str(), Some(s) if s.starts_with('#'))
}

/// Whether `o` has an own property `key` (string/symbol or array/string
/// exotic index/length).
/// `__lookupGetter__`/`__lookupSetter__`: walk the prototype chain; at the first
/// object that owns `key`, return its getter (`want_get`) or setter accessor —
/// or `undefined` if that own property is a data property.
fn lookup_accessor(
    vm: &mut Vm,
    o: &JsObject,
    key: &PropertyKey,
    want_get: bool,
) -> Result<Value, Value> {
    let mut cur = Some(o.clone());
    while let Some(obj) = cur {
        // For a Proxy, [[GetOwnProperty]] and [[GetPrototypeOf]] dispatch to the
        // handler traps (which may throw — that propagates out).
        if vm.is_proxy(&obj) {
            let desc = vm.proxy_get_own_descriptor(&obj, key)?;
            if matches!(desc, Value::Object(_)) {
                let is_accessor = vm.has_prop(&desc, &PropertyKey::str("get"))?
                    || vm.has_prop(&desc, &PropertyKey::str("set"))?;
                if is_accessor {
                    let want = if want_get { "get" } else { "set" };
                    return vm.get_prop(&desc, &PropertyKey::str(want));
                }
                return Ok(Value::Undefined);
            }
            cur = match vm.proxy_get_prototype_of(&obj)? {
                Value::Object(p) => Some(p),
                _ => None,
            };
            continue;
        }
        let next = {
            let b = obj.borrow();
            if let Some(p) = b.props.get(key) {
                return Ok(match &p.kind {
                    PropertyKind::Accessor { get, set } => {
                        let f = if want_get { get } else { set };
                        f.clone().unwrap_or(Value::Undefined)
                    }
                    PropertyKind::Data { .. } => Value::Undefined,
                });
            }
            b.proto.clone()
        };
        cur = next;
    }
    Ok(Value::Undefined)
}

/// `DefinePropertyOrThrow(O, key, {accessor, enumerable: true, configurable:
/// true})` for `__defineGetter__`/`__defineSetter__`. Dispatches to the Proxy
/// `defineProperty` trap when `o` is a proxy, otherwise the ordinary algorithm.
fn define_accessor_or_throw(
    vm: &mut Vm,
    o: &JsObject,
    key: PropertyKey,
    accessor: Value,
    want_get: bool,
) -> Result<(), Value> {
    if vm.is_proxy(o) {
        let attrs = vm.new_object();
        let av = Value::Object(attrs.clone());
        let slot = if want_get { "get" } else { "set" };
        vm.set_prop(&av, &PropertyKey::str(slot), accessor)?;
        vm.set_prop(&av, &PropertyKey::str("enumerable"), Value::Bool(true))?;
        vm.set_prop(&av, &PropertyKey::str("configurable"), Value::Bool(true))?;
        if !vm.proxy_define_property(o, &key, av)? {
            return Err(vm.throw_type("Cannot redefine property"));
        }
        return Ok(());
    }
    let mut d = PropDesc {
        enumerable: Some(true),
        configurable: Some(true),
        ..PropDesc::default()
    };
    if want_get {
        d.get = Some(accessor);
        d.has_get = true;
    } else {
        d.set = Some(accessor);
        d.has_set = true;
    }
    define_own_property(vm, o, &key, &d, true)?;
    Ok(())
}

/// IsArray (spec 7.2.2): true for an Array exotic object, or a non-revoked
/// Proxy whose target is (transitively) an Array. Throws on a revoked Proxy.
fn is_array_exotic(vm: &Vm, o: &JsObject) -> Result<bool, Value> {
    let mut cur = o.clone();
    loop {
        let target = {
            let b = cur.borrow();
            match &b.internal {
                Internal::Array(_) => return Ok(true),
                Internal::Proxy(p) => {
                    if p.revoked {
                        return Err(vm.throw_type(
                            "Cannot perform 'IsArray' on a proxy that has been revoked",
                        ));
                    }
                    p.target.clone()
                }
                _ => return Ok(false),
            }
        };
        cur = target;
    }
}

/// OrdinarySetPrototypeOf (spec 10.1.2): rejects (returns false) a no-op-failing
/// change on a non-extensible object, and rejects prototype cycles. Returns true
/// on success (and mutates `o`'s prototype).
fn ordinary_set_prototype_of(vm: &Vm, o: &JsObject, proto: Option<JsObject>) -> bool {
    let current = o.borrow().proto.clone();
    let same = match (&proto, &current) {
        (None, None) => true,
        (Some(a), Some(b)) => a.same(b),
        _ => false,
    };
    if same {
        return true;
    }
    if !o.borrow().extensible {
        return false;
    }
    // Walk the proposed chain looking for `o` itself (a cycle). Stop at a Proxy,
    // whose [[GetPrototypeOf]] is not the ordinary one.
    let mut p = proto.clone();
    while let Some(pp) = p {
        if pp.same(o) {
            return false;
        }
        if vm.is_proxy(&pp) {
            break;
        }
        let next = pp.borrow().proto.clone();
        p = next;
    }
    o.borrow_mut().proto = proto;
    true
}

/// `O.[[SetPrototypeOf]](V)`: dispatches to the Proxy trap or the ordinary
/// algorithm. `proto` is the (already validated) Object-or-null prototype.
pub(crate) fn object_set_prototype_of(
    vm: &mut Vm,
    o: &JsObject,
    proto: Option<JsObject>,
) -> Result<bool, Value> {
    if vm.is_proxy(o) {
        let pv = proto.map(Value::Object).unwrap_or(Value::Null);
        return vm.proxy_set_prototype_of(o, pv);
    }
    Ok(ordinary_set_prototype_of(vm, o, proto))
}

/// EnumerableOwnPropertyNames (string keys): proxy-aware. For a Proxy it calls
/// `[[OwnPropertyKeys]]` (the trap) then `[[GetOwnProperty]]` (the trap) per key
/// to test enumerability; ordinary objects use the fast direct path.
fn enumerable_own_strings_dyn(vm: &mut Vm, o: &JsObject) -> Result<Vec<JsString>, Value> {
    if !vm.is_proxy(o) {
        return Ok(vm.enumerable_own_string_keys(o));
    }
    let keys = vm.own_property_keys(o)?;
    let mut out = Vec::new();
    for k in keys {
        if let PropertyKey::Str(s) = &k {
            let desc = vm.proxy_get_own_descriptor(o, &k)?;
            let enumerable = match &desc {
                Value::Object(_) => {
                    let e = vm.get_prop(&desc, &PropertyKey::str("enumerable"))?;
                    vm.to_boolean(&e)
                }
                _ => false,
            };
            if enumerable {
                out.push(s.clone());
            }
        }
    }
    Ok(out)
}

fn own_property_exists(o: &JsObject, key: &PropertyKey) -> bool {
    if is_private_key(key) {
        return false;
    }
    let b = o.borrow();
    if b.props.contains_key(key) {
        return true;
    }
    match &b.internal {
        Internal::Array(arr) => {
            if let Some("length") = key.as_str() {
                return true;
            }
            if let Some(i) = key.array_index() {
                // A hole is not an own property.
                if let Some(v) = arr.get(i as usize) {
                    if !matches!(v, Value::Hole) {
                        return true;
                    }
                }
            }
            false
        }
        Internal::StringObj(s) => {
            if let Some("length") = key.as_str() {
                return true;
            }
            if let Some(i) = key.array_index() {
                if (i as usize) < s.as_str().chars().count() {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// The own property descriptor for `key`, including array/string exotic
/// index/length slots reified into `Property` values.
pub(crate) fn own_property_descriptor(o: &JsObject, key: &PropertyKey) -> Option<Property> {
    if is_private_key(key) {
        return None;
    }
    let b = o.borrow();
    if let Some(p) = b.props.get(key) {
        return Some(p.clone());
    }
    match &b.internal {
        Internal::Array(arr) => {
            if let Some("length") = key.as_str() {
                return Some(Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(arr.len() as f64),
                        writable: true,
                    },
                    enumerable: false,
                    configurable: false,
                });
            }
            if let Some(idx) = key.array_index() {
                if let Some(v) = arr.get(idx as usize) {
                    // A hole has no own descriptor.
                    if !matches!(v, Value::Hole) {
                        return Some(Property {
                            kind: PropertyKind::Data {
                                value: v.clone(),
                                writable: true,
                            },
                            enumerable: true,
                            configurable: true,
                        });
                    }
                }
            }
            None
        }
        Internal::StringObj(s) => {
            if let Some("length") = key.as_str() {
                return Some(Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(s.as_str().chars().count() as f64),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                });
            }
            if let Some(idx) = key.array_index() {
                if let Some(c) = s.as_str().chars().nth(idx as usize) {
                    return Some(Property {
                        kind: PropertyKind::Data {
                            value: Value::str(c.to_string()),
                            writable: false,
                        },
                        enumerable: true,
                        configurable: false,
                    });
                }
            }
            None
        }
        _ => None,
    }
}

fn define_properties(vm: &mut Vm, obj: &JsObject, props: &Value) -> Result<(), Value> {
    let po = vm.to_object(props)?;
    // Collect (key, parsed descriptor) for every enumerable own key (including
    // symbols) first, then apply — per ObjectDefineProperties.
    let keys = vm.own_keys(&po);
    let mut descriptors: Vec<(PropertyKey, PropDesc)> = Vec::new();
    for k in keys {
        let enumerable = {
            let b = po.borrow();
            match b.props.get(&k) {
                Some(p) => p.enumerable,
                None => match &b.internal {
                    Internal::Array(arr) => k
                        .array_index()
                        .map(|i| (i as usize) < arr.len())
                        .unwrap_or(false),
                    Internal::StringObj(s) => k
                        .array_index()
                        .map(|i| (i as usize) < s.as_str().chars().count())
                        .unwrap_or(false),
                    _ => false,
                },
            }
        };
        if !enumerable {
            continue;
        }
        let desc = vm.get_prop(&Value::Object(po.clone()), &k)?;
        let d = to_property_descriptor(vm, &desc)?;
        descriptors.push((k, d));
    }
    for (k, d) in descriptors {
        define_own_property(vm, obj, &k, &d, true)?;
    }
    Ok(())
}

/// FromPropertyDescriptor (spec 6.2.6.4).
pub(crate) fn descriptor_to_object(vm: &mut Vm, p: &Property) -> Value {
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

fn install_function(vm: &mut Vm) {
    let proto = vm.realm.function_proto.clone();

    vm.define_method(&proto, "call", 1, |vm, this, args| {
        if !vm.is_callable(&this) {
            return Err(vm.throw_type("Function.prototype.call called on non-callable"));
        }
        let this_arg = arg(args, 0);
        let rest = if args.len() > 1 { &args[1..] } else { &[] };
        vm.call(this, this_arg, rest)
    });
    vm.define_method(&proto, "apply", 2, |vm, this, args| {
        if !vm.is_callable(&this) {
            return Err(vm.throw_type("Function.prototype.apply called on non-callable"));
        }
        let this_arg = arg(args, 0);
        let list = arg(args, 1);
        let call_args = if list.is_nullish() {
            Vec::new()
        } else {
            array_like_to_vec(vm, &list)?
        };
        vm.call(this, this_arg, &call_args)
    });
    vm.define_method(&proto, "bind", 1, |vm, this, args| {
        let target = match &this {
            Value::Object(o) if o.borrow().is_callable() => o.clone(),
            _ => return Err(vm.throw_type("Bind must be called on a function")),
        };
        let bound_this = arg(args, 0);
        let bound_args = if args.len() > 1 {
            args[1..].to_vec()
        } else {
            Vec::new()
        };
        // length = max(0, target.length - bound_args.len()), if target.length
        // is a number; otherwise 0.
        let target_len = vm
            .get_prop(&this, &PropertyKey::str("length"))
            .ok()
            .and_then(|v| match v {
                Value::Number(n) if n.is_finite() => Some(n.max(0.0).floor()),
                _ => None,
            })
            .unwrap_or(0.0);
        let bound_len = (target_len - bound_args.len() as f64).max(0.0);
        // name = "bound " + target.name (target.name coerced to string).
        let target_name = vm
            .get_prop(&this, &PropertyKey::str("name"))
            .ok()
            .map(|v| match v {
                Value::String(s) => s.as_str().to_string(),
                _ => String::new(),
            })
            .unwrap_or_default();

        let bound = JsObject::new(ObjectData::new(
            Some(vm.realm.function_proto.clone()),
            Internal::Function(FunctionInner::Bound(BoundFunction {
                target,
                bound_this,
                bound_args,
            })),
        ));
        {
            let mut b = bound.borrow_mut();
            b.props.insert(
                PropertyKey::str("length"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(bound_len),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
            b.props.insert(
                PropertyKey::str("name"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::str(format!("bound {target_name}")),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
        }
        Ok(Value::Object(bound))
    });
    vm.define_method(&proto, "toString", 0, |vm, this, _args| {
        if !vm.is_callable(&this) {
            return Err(vm.throw_type("Function.prototype.toString called on non-callable"));
        }
        let name = vm
            .get_prop(&this, &PropertyKey::str("name"))
            .ok()
            .map(|v| vm.to_string_lossy(&v))
            .unwrap_or_default();
        Ok(Value::str(format!("function {name}() {{ [native code] }}")))
    });
    // Function.prototype itself is callable (returns undefined).
    proto.borrow_mut().internal = Internal::Function(FunctionInner::Native(NativeFunction {
        name: std::rc::Rc::from(""),
        length: 0,
        func: std::rc::Rc::new(|_vm, _t, _a| Ok(Value::Undefined)),
        construct: None,
    }));

    let has_instance = vm.realm.symbol_has_instance.clone();
    let f = vm.new_native("[Symbol.hasInstance]", 1, |vm, this, args| {
        let r = vm.instance_of_ordinary(&arg(args, 0), &this)?;
        Ok(Value::Bool(r))
    });
    vm.define_value_sym(&proto, has_instance, Value::Object(f));
}

fn array_like_to_vec(vm: &mut Vm, v: &Value) -> Result<Vec<Value>, Value> {
    let o = vm.to_object(v)?;
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

fn install_symbol(vm: &mut Vm) {
    let proto = vm.realm.symbol_proto.clone();

    let ctor = vm.new_native("Symbol", 0, |vm, _this, args| {
        let desc = arg(args, 0);
        let d = if desc.is_undefined() {
            None
        } else {
            Some(vm.to_string_lossy(&desc))
        };
        let s = vm.alloc_symbol(d.as_deref());
        Ok(Value::Symbol(s))
    });
    // Symbol is not a constructor.
    vm.install_ctor("Symbol", &ctor, &proto);

    // Well-known symbols as static properties.
    let pairs = [
        ("iterator", vm.realm.symbol_iterator.clone()),
        ("asyncIterator", vm.realm.symbol_async_iterator.clone()),
        ("toPrimitive", vm.realm.symbol_to_primitive.clone()),
        ("toStringTag", vm.realm.symbol_to_string_tag.clone()),
        ("hasInstance", vm.realm.symbol_has_instance.clone()),
        ("match", vm.realm.symbol_match.clone()),
        ("replace", vm.realm.symbol_replace.clone()),
        ("search", vm.realm.symbol_search.clone()),
        ("split", vm.realm.symbol_split.clone()),
        ("matchAll", vm.realm.symbol_match_all.clone()),
        ("species", vm.realm.symbol_species.clone()),
        ("unscopables", vm.realm.symbol_unscopables.clone()),
        (
            "isConcatSpreadable",
            vm.realm.symbol_is_concat_spreadable.clone(),
        ),
        ("dispose", vm.realm.symbol_dispose.clone()),
        ("asyncDispose", vm.realm.symbol_async_dispose.clone()),
    ];
    for (name, sym) in pairs {
        // Well-known symbols are non-writable, non-enumerable, non-configurable.
        ctor.borrow_mut().props.insert(
            PropertyKey::str(name),
            Property {
                kind: PropertyKind::Data {
                    value: Value::Symbol(sym),
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            },
        );
    }

    vm.define_method(&ctor, "for", 1, |vm, _t, args| {
        let key = vm.to_string_lossy(&arg(args, 0));
        if let Some(s) = vm.realm.symbol_registry.get(&key) {
            return Ok(Value::Symbol(s.clone()));
        }
        let s = vm.alloc_symbol(Some(&key));
        vm.realm.symbol_registry.insert(key, s.clone());
        Ok(Value::Symbol(s))
    });
    vm.define_method(&ctor, "keyFor", 1, |vm, _t, args| match arg(args, 0) {
        Value::Symbol(s) => {
            for (k, v) in &vm.realm.symbol_registry {
                if v == &s {
                    return Ok(Value::str(k.clone()));
                }
            }
            Ok(Value::Undefined)
        }
        _ => Err(vm.throw_type("Symbol.keyFor requires a symbol")),
    });

    vm.define_method(&proto, "toString", 0, |vm, this, _args| {
        if let Value::Symbol(s) = sym_this(&this) {
            return Ok(Value::str(format!(
                "Symbol({})",
                s.description().unwrap_or("")
            )));
        }
        Err(vm.throw_type("Symbol.prototype.toString requires a symbol"))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, this, _args| {
        match sym_this(&this) {
            Value::Symbol(s) => Ok(Value::Symbol(s)),
            _ => Err(vm.throw_type("Symbol.prototype.valueOf requires a symbol")),
        }
    });

    // Symbol.prototype.description accessor getter.
    let description_getter =
        vm.new_native("get description", 0, |vm, this, _args| {
            match sym_this(&this) {
                Value::Symbol(s) => Ok(match s.description() {
                    Some(d) => Value::str(d),
                    None => Value::Undefined,
                }),
                _ => Err(vm.throw_type("Symbol.prototype.description requires a symbol")),
            }
        });
    proto.borrow_mut().props.insert(
        PropertyKey::str("description"),
        Property {
            kind: PropertyKind::Accessor {
                get: Some(Value::Object(description_getter)),
                set: None,
            },
            enumerable: false,
            configurable: true,
        },
    );

    // Symbol.prototype[Symbol.toPrimitive] returns the symbol (any hint).
    let to_primitive_sym = vm.realm.symbol_to_primitive.clone();
    let to_primitive_fn = vm.new_native(
        "[Symbol.toPrimitive]",
        1,
        |vm, this, _args| match sym_this(&this) {
            Value::Symbol(s) => Ok(Value::Symbol(s)),
            _ => Err(vm.throw_type("Symbol.prototype[Symbol.toPrimitive] requires a symbol")),
        },
    );
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(to_primitive_sym),
        Property {
            kind: PropertyKind::Data {
                value: Value::Object(to_primitive_fn),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    // Symbol.prototype[Symbol.toStringTag] = "Symbol".
    let to_string_tag_sym = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(to_string_tag_sym),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Symbol"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
}

fn sym_this(this: &Value) -> Value {
    match this {
        Value::Symbol(_) => this.clone(),
        Value::Object(o) => {
            if let Internal::Symbol(s) = &o.borrow().internal {
                Value::Symbol(s.clone())
            } else {
                Value::Undefined
            }
        }
        _ => Value::Undefined,
    }
}

fn install_boolean(vm: &mut Vm) {
    let proto = vm.realm.boolean_proto.clone();
    proto.borrow_mut().internal = Internal::Boolean(false);
    let ctor = vm.new_native_ctor(
        "Boolean",
        1,
        |vm, _t, args| Ok(Value::Bool(vm.to_boolean(&arg(args, 0)))),
        |vm, _t, args| {
            let b = vm.to_boolean(&arg(args, 0));
            Ok(Value::Object(JsObject::new(ObjectData::new(
                Some(vm.realm.boolean_proto.clone()),
                Internal::Boolean(b),
            ))))
        },
    );
    vm.install_ctor("Boolean", &ctor, &proto);
    vm.define_method(&proto, "toString", 0, |vm, this, _a| {
        let b = bool_this(vm, &this)?;
        Ok(Value::str(if b { "true" } else { "false" }))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, this, _a| {
        Ok(Value::Bool(bool_this(vm, &this)?))
    });
}

fn bool_this(vm: &mut Vm, this: &Value) -> Result<bool, Value> {
    match this {
        Value::Bool(b) => Ok(*b),
        Value::Object(o) => {
            if let Internal::Boolean(b) = &o.borrow().internal {
                Ok(*b)
            } else {
                Err(vm.throw_type("Boolean.prototype method called on incompatible receiver"))
            }
        }
        _ => Err(vm.throw_type("Boolean.prototype method called on incompatible receiver")),
    }
}

fn install_errors(vm: &mut Vm) {
    // Native-error subtypes that already have realm-resident prototypes.
    let native_kinds = [
        (ErrorKind::Error, "Error"),
        (ErrorKind::Type, "TypeError"),
        (ErrorKind::Range, "RangeError"),
        (ErrorKind::Reference, "ReferenceError"),
        (ErrorKind::Syntax, "SyntaxError"),
        (ErrorKind::Uri, "URIError"),
    ];
    let mut error_ctor: Option<JsObject> = None;
    for (kind, name) in native_kinds {
        let proto = error_proto(vm, kind);
        let ctor = install_error_kind(vm, &proto, name);
        if matches!(kind, ErrorKind::Error) {
            error_ctor = Some(ctor);
        } else if let Some(ec) = &error_ctor {
            // A NativeError constructor's [[Prototype]] is the Error constructor
            // (e.g. `Object.getPrototypeOf(RangeError) === Error`).
            ctor.borrow_mut().proto = Some(ec.clone());
        }
    }

    // EvalError lacks a realm-resident prototype (nothing throws it internally);
    // create an ordinary prototype chaining to Error.prototype.
    let error_proto = vm.realm.error_proto.clone();
    for name in ["EvalError"] {
        let proto = JsObject::ordinary(Some(error_proto.clone()));
        let ctor = install_error_kind(vm, &proto, name);
        // Subtype ctor inherits from the Error constructor.
        if let Some(ec) = &error_ctor {
            ctor.borrow_mut().proto = Some(ec.clone());
        }
    }

    // Error.isError(arg): true iff arg is an object with an [[ErrorData]]
    // internal slot (the engine's `Internal::Error`).
    if let Some(ec) = &error_ctor {
        vm.define_method(ec, "isError", 1, |_vm, _t, args| {
            let is = matches!(
                arg(args, 0),
                Value::Object(o) if matches!(o.borrow().internal, Internal::Error)
            );
            Ok(Value::Bool(is))
        });
    }

    // AggregateError(errors, message): collects an iterable of errors.
    install_aggregate_error(vm, error_ctor.as_ref());
    // SuppressedError(error, suppressed, message): a disposal error that wraps an
    // error thrown while another was already in flight.
    install_suppressed_error(vm, error_ctor.as_ref());
}

/// `SuppressedError(error, suppressed, message)` — installs `error` and
/// `suppressed` own data properties plus the usual error message/stack.
fn install_suppressed_error(vm: &mut Vm, error_ctor: Option<&JsObject>) {
    let proto = JsObject::ordinary(Some(vm.realm.error_proto.clone()));
    proto.borrow_mut().internal = Internal::Error;
    proto.borrow_mut().props.insert(
        PropertyKey::str("name"),
        Property::builtin(Value::str("SuppressedError")),
    );
    proto.borrow_mut().props.insert(
        PropertyKey::str("message"),
        Property::builtin(Value::str("")),
    );
    vm.define_method(&proto, "toString", 0, error_to_string);

    let proto_for_ctor = proto.clone();
    let build = move |vm: &mut Vm, _t: Value, args: &[Value]| -> Result<Value, Value> {
        let o = JsObject::new(ObjectData::new(
            Some(proto_for_ctor.clone()),
            Internal::Error,
        ));
        // 3rd argument is the message.
        let msg = arg(args, 2);
        if !msg.is_undefined() {
            let s = vm.to_js_string(&msg)?;
            o.borrow_mut().props.insert(
                PropertyKey::str("message"),
                Property::builtin(Value::String(s)),
            );
        }
        // `error` (1st arg) and `suppressed` (2nd arg) are non-enumerable,
        // writable, configurable own data properties.
        o.borrow_mut()
            .props
            .insert(PropertyKey::str("error"), Property::builtin(arg(args, 0)));
        o.borrow_mut().props.insert(
            PropertyKey::str("suppressed"),
            Property::builtin(arg(args, 1)),
        );
        install_error_stack(vm, &o);
        Ok(Value::Object(o))
    };
    let build_call = build.clone();
    let ctor = vm.new_native_ctor(
        "SuppressedError",
        3,
        move |vm, t, args| build_call(vm, t, args),
        move |vm, t, args| build(vm, t, args),
    );
    if let Some(ec) = error_ctor {
        ctor.borrow_mut().proto = Some(ec.clone());
    }
    vm.install_ctor("SuppressedError", &ctor, &proto);
}

/// Install one Error-family constructor + its prototype wiring. Returns the
/// constructor object.
fn install_error_kind(vm: &mut Vm, proto: &JsObject, name: &str) -> JsObject {
    proto.borrow_mut().internal = Internal::Error;
    // name and message are non-enumerable data properties on the prototype.
    proto.borrow_mut().props.insert(
        PropertyKey::str("name"),
        Property::builtin(Value::str(name)),
    );
    proto.borrow_mut().props.insert(
        PropertyKey::str("message"),
        Property::builtin(Value::str("")),
    );
    vm.define_method(proto, "toString", 0, error_to_string);

    let proto_for_ctor = proto.clone();
    let ctor = vm.new_native_ctor(
        name,
        1,
        {
            let p = proto_for_ctor.clone();
            move |vm, _t, args| make_error_obj(vm, &p, args)
        },
        {
            let p = proto_for_ctor.clone();
            move |vm, _t, args| make_error_obj(vm, &p, args)
        },
    );
    vm.install_ctor(name, &ctor, proto);
    ctor
}

/// Error.prototype.toString (spec 20.5.3.4).
fn error_to_string(vm: &mut Vm, this: Value, _args: &[Value]) -> Result<Value, Value> {
    if !matches!(this, Value::Object(_)) {
        return Err(vm.throw_type("Error.prototype.toString called on non-object"));
    }
    let name = vm.get_prop(&this, &PropertyKey::str("name"))?;
    let name = if name.is_undefined() {
        "Error".to_string()
    } else {
        vm.to_js_string(&name)?.as_str().to_string()
    };
    let msg = vm.get_prop(&this, &PropertyKey::str("message"))?;
    let msg = if msg.is_undefined() {
        String::new()
    } else {
        vm.to_js_string(&msg)?.as_str().to_string()
    };
    Ok(Value::str(if name.is_empty() {
        msg
    } else if msg.is_empty() {
        name
    } else {
        format!("{name}: {msg}")
    }))
}

fn make_error_obj(vm: &mut Vm, proto: &JsObject, args: &[Value]) -> Result<Value, Value> {
    let o = JsObject::new(ObjectData::new(Some(proto.clone()), Internal::Error));
    let msg = arg(args, 0);
    if !msg.is_undefined() {
        // ToString(message) is observable and throws for e.g. a Symbol.
        let s = vm.to_js_string(&msg)?;
        o.borrow_mut().props.insert(
            PropertyKey::str("message"),
            Property::builtin(Value::String(s)),
        );
    }
    install_error_options(vm, &o, arg(args, 1))?;
    install_error_stack(vm, &o);
    Ok(Value::Object(o))
}

/// Apply `options.cause` when an options object carrying `cause` is supplied.
/// `HasProperty`/`Get` on `options` are observable and may throw (propagated).
fn install_error_options(vm: &mut Vm, o: &JsObject, options: Value) -> Result<(), Value> {
    if let Value::Object(opts) = options {
        let has_cause = vm.has_prop(&Value::Object(opts.clone()), &PropertyKey::str("cause"))?;
        if has_cause {
            let cause = vm.get_prop(&Value::Object(opts.clone()), &PropertyKey::str("cause"))?;
            o.borrow_mut()
                .props
                .insert(PropertyKey::str("cause"), Property::builtin(cause));
        }
    }
    Ok(())
}

/// Best-effort `stack` property (non-standard, but widely depended upon).
fn install_error_stack(vm: &mut Vm, o: &JsObject) {
    let name = vm
        .get_prop(&Value::Object(o.clone()), &PropertyKey::str("name"))
        .map(|v| vm.to_string_lossy(&v))
        .unwrap_or_else(|_| "Error".into());
    let m = o
        .borrow()
        .props
        .get(&PropertyKey::str("message"))
        .and_then(|p| p.value().cloned())
        .map(|v| vm.to_string_lossy(&v))
        .unwrap_or_default();
    o.borrow_mut().props.insert(
        PropertyKey::str("stack"),
        Property::builtin(Value::str(if m.is_empty() {
            name
        } else {
            format!("{name}: {m}")
        })),
    );
}

/// AggregateError(errors, message [, options]).
fn install_aggregate_error(vm: &mut Vm, error_ctor: Option<&JsObject>) {
    let proto = JsObject::ordinary(Some(vm.realm.error_proto.clone()));
    proto.borrow_mut().internal = Internal::Error;
    proto.borrow_mut().props.insert(
        PropertyKey::str("name"),
        Property::builtin(Value::str("AggregateError")),
    );
    proto.borrow_mut().props.insert(
        PropertyKey::str("message"),
        Property::builtin(Value::str("")),
    );
    vm.define_method(&proto, "toString", 0, error_to_string);

    let proto_for_ctor = proto.clone();
    let build = move |vm: &mut Vm, _t: Value, args: &[Value]| -> Result<Value, Value> {
        let o = JsObject::new(ObjectData::new(
            Some(proto_for_ctor.clone()),
            Internal::Error,
        ));
        // 2nd argument is the message.
        let msg = arg(args, 1);
        if !msg.is_undefined() {
            let s = vm.to_js_string(&msg)?;
            o.borrow_mut().props.insert(
                PropertyKey::str("message"),
                Property::builtin(Value::String(s)),
            );
        }
        install_error_options(vm, &o, arg(args, 2))?;
        // 1st argument is the iterable of errors.
        let errors = vm.iterate_to_vec(&arg(args, 0))?;
        let arr = vm.new_array(errors);
        o.borrow_mut().props.insert(
            PropertyKey::str("errors"),
            Property::builtin(Value::Object(arr)),
        );
        install_error_stack(vm, &o);
        Ok(Value::Object(o))
    };
    let build_call = build.clone();
    let ctor = vm.new_native_ctor(
        "AggregateError",
        2,
        move |vm, t, args| build_call(vm, t, args),
        move |vm, t, args| build(vm, t, args),
    );
    if let Some(ec) = error_ctor {
        ctor.borrow_mut().proto = Some(ec.clone());
    }
    vm.install_ctor("AggregateError", &ctor, &proto);
}

fn error_proto(vm: &Vm, kind: ErrorKind) -> JsObject {
    match kind {
        ErrorKind::Error => vm.realm.error_proto.clone(),
        ErrorKind::Type => vm.realm.type_error_proto.clone(),
        ErrorKind::Range => vm.realm.range_error_proto.clone(),
        ErrorKind::Reference => vm.realm.reference_error_proto.clone(),
        ErrorKind::Syntax => vm.realm.syntax_error_proto.clone(),
        ErrorKind::Uri => vm.realm.uri_error_proto.clone(),
    }
}

impl Vm {
    /// OrdinaryHasInstance, used by `Function.prototype[Symbol.hasInstance]`.
    pub fn instance_of_ordinary(&mut self, obj: &Value, ctor: &Value) -> Result<bool, Value> {
        let cobj = match ctor {
            Value::Object(o) if o.borrow().is_callable() => o.clone(),
            _ => return Ok(false),
        };
        // Bound function: use target.
        if let Internal::Function(FunctionInner::Bound(b)) = &cobj.borrow().internal {
            let t = Value::Object(b.target.clone());
            return self.instance_of_ordinary(obj, &t);
        }
        let target_proto = self.get_prop(ctor, &PropertyKey::str("prototype"))?;
        let target_proto = match target_proto {
            Value::Object(o) => o,
            _ => return Err(self.throw_type("prototype is not an object")),
        };
        let mut cur = match obj {
            Value::Object(o) => o.borrow().proto.clone(),
            _ => return Ok(false),
        };
        while let Some(p) = cur {
            if p.same(&target_proto) {
                return Ok(true);
            }
            cur = p.borrow().proto.clone();
        }
        Ok(false)
    }
}
