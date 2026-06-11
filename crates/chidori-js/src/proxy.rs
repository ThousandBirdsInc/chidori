//! Proxy exotic objects: the internal-method trap dispatch that the central VM
//! operations (`get_prop`/`set_prop`/`has_prop`/`delete_prop`/`own_property_keys`/
//! `call`/`construct`) and the `Object`/`Reflect` builtins forward to when their
//! target is a `Proxy`.
//!
//! Each trap follows the spec shape: look up the named trap on the handler; if it
//! is `undefined`/`null`, perform the ordinary operation on the target; otherwise
//! invoke the trap and enforce the (most commonly tested) invariants.

use crate::builtins::fundamental::{
    define_own_property, descriptor_to_object, own_property_descriptor, to_property_descriptor,
};
use crate::value::*;
use crate::vm::Vm;

/// A `PropertyKey` as the `Value` passed to traps (string or symbol).
fn key_to_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::Str(s) => Value::String(s.clone()),
        PropertyKey::Sym(s) => Value::Symbol(s.clone()),
    }
}

impl Vm {
    /// Allocate a Proxy exotic object wrapping `target` with `handler`.
    pub fn new_proxy(&self, target: JsObject, handler: JsObject) -> JsObject {
        // Proxies have no [[Prototype]] of their own; lookups are trapped.
        self.alloc(ObjectData::new(
            None,
            Internal::Proxy(ProxyData {
                target,
                handler,
                revoked: false,
            }),
        ))
    }

    /// Is this object a (non-revoked or revoked) Proxy?
    pub fn is_proxy(&self, o: &JsObject) -> bool {
        matches!(o.borrow().internal, Internal::Proxy(_))
    }

    /// Resolve a Proxy's `(target, handler)`, throwing if it has been revoked.
    pub fn proxy_parts(&mut self, o: &JsObject) -> Result<(JsObject, JsObject), Value> {
        match &o.borrow().internal {
            Internal::Proxy(p) => {
                if p.revoked {
                    Err(self.throw_type("Cannot perform operation on a revoked proxy"))
                } else {
                    Ok((p.target.clone(), p.handler.clone()))
                }
            }
            _ => Err(self.throw_type("not a Proxy")),
        }
    }

    /// Look up a trap on the handler: `None` if absent/undefined/null, `Some(fn)`
    /// if callable, else a TypeError.
    fn proxy_trap(&mut self, handler: &JsObject, name: &str) -> Result<Option<Value>, Value> {
        let t = self.get_prop(&Value::Object(handler.clone()), &PropertyKey::str(name))?;
        if t.is_undefined() || t.is_null() {
            Ok(None)
        } else if self.is_callable(&t) {
            Ok(Some(t))
        } else {
            Err(self.throw_type(&format!("proxy handler's '{name}' trap is not a function")))
        }
    }

    // ---- the trapped internal methods ----

    pub fn proxy_get(
        &mut self,
        o: &JsObject,
        key: &PropertyKey,
        receiver: Value,
    ) -> Result<Value, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "get")? {
            None => self.get_from_object(&target, key, receiver),
            Some(trap) => {
                let result = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone()), key_to_value(key), receiver],
                )?;
                // Invariant: a non-configurable, non-writable own data property
                // must report its exact value; a non-configurable accessor with no
                // getter must report undefined.
                if let Some(p) = own_property_descriptor(&target, key) {
                    if !p.configurable {
                        match &p.kind {
                            PropertyKind::Data { value, writable } if !writable => {
                                if !same_value(&result, value) {
                                    return Err(self.throw_type(
                                        "proxy get trap violated invariant for a non-configurable, non-writable data property",
                                    ));
                                }
                            }
                            PropertyKind::Accessor { get, .. } if get.is_none() => {
                                if !result.is_undefined() {
                                    return Err(self.throw_type(
                                        "proxy get trap must report undefined for a non-configurable accessor with no getter",
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(result)
            }
        }
    }

    pub fn proxy_set(
        &mut self,
        o: &JsObject,
        key: &PropertyKey,
        value: Value,
        receiver: Value,
    ) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "set")? {
            None => self.ordinary_set(&target, key, value, receiver),
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[
                        Value::Object(target.clone()),
                        key_to_value(key),
                        value.clone(),
                        receiver,
                    ],
                )?;
                if !self.to_boolean(&r) {
                    return Ok(false);
                }
                // Invariant (10.5.9): a non-configurable, non-writable target data
                // property can't be reported as set to a different value; a
                // non-configurable accessor with no setter can't be written.
                if let Some(p) = own_property_descriptor(&target, key) {
                    if !p.configurable {
                        match &p.kind {
                            PropertyKind::Data {
                                value: tv,
                                writable,
                            } if !writable => {
                                if !same_value(&value, tv) {
                                    return Err(self.throw_type(
                                        "proxy set: cannot change a non-configurable, non-writable property to a different value",
                                    ));
                                }
                            }
                            PropertyKind::Accessor { set, .. } if set.is_none() => {
                                return Err(self.throw_type(
                                    "proxy set: cannot set a non-configurable accessor property that has no setter",
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                Ok(true)
            }
        }
    }

    pub fn proxy_has(&mut self, o: &JsObject, key: &PropertyKey) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "has")? {
            None => self.has_prop(&Value::Object(target), key),
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone()), key_to_value(key)],
                )?;
                let b = self.to_boolean(&r);
                if !b {
                    // Invariant: cannot hide a non-configurable own key, nor any
                    // own key of a non-extensible target.
                    if let Some(p) = own_property_descriptor(&target, key) {
                        let extensible = target.borrow().extensible;
                        if !p.configurable {
                            return Err(self.throw_type(
                                "proxy has trap returned false for a non-configurable property",
                            ));
                        }
                        if !extensible {
                            return Err(self.throw_type(
                                "proxy has trap returned false for a property of a non-extensible target",
                            ));
                        }
                    }
                }
                Ok(b)
            }
        }
    }

    pub fn proxy_delete(&mut self, o: &JsObject, key: &PropertyKey) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "deleteProperty")? {
            None => self.delete_prop(&Value::Object(target), key),
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone()), key_to_value(key)],
                )?;
                let b = self.to_boolean(&r);
                if b {
                    if let Some(p) = own_property_descriptor(&target, key) {
                        if !p.configurable {
                            return Err(self.throw_type(
                                "proxy deleteProperty trap cannot delete a non-configurable property",
                            ));
                        }
                        // A property of a non-extensible target cannot be deleted.
                        if !target.borrow().extensible {
                            return Err(self.throw_type(
                                "proxy deleteProperty trap cannot delete a property of a non-extensible target",
                            ));
                        }
                    }
                }
                Ok(b)
            }
        }
    }

    pub fn proxy_own_keys(&mut self, o: &JsObject) -> Result<Vec<PropertyKey>, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "ownKeys")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_own_keys(&target);
                }
                Ok(self.own_keys(&target))
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone())],
                )?;
                // The trap must return an array-like of strings/symbols. Build the
                // key list, rejecting non-key entries and duplicates.
                let arr = match &r {
                    Value::Object(_) => r.clone(),
                    _ => return Err(self.throw_type("proxy ownKeys trap must return an Array")),
                };
                let len_v = self.get_prop(&arr, &PropertyKey::str("length"))?;
                let len = self.to_length(&len_v)?;
                let mut keys: Vec<PropertyKey> = Vec::with_capacity(len.min(1 << 16));
                let mut seen: Vec<PropertyKey> = Vec::new();
                for i in 0..len {
                    let el = self.get_prop(&arr, &PropertyKey::from_index(i as u32))?;
                    match el {
                        Value::String(_) | Value::Symbol(_) => {
                            let k = self.to_property_key(&el)?;
                            if seen.iter().any(|s| keys_eq(s, &k)) {
                                return Err(
                                    self.throw_type("proxy ownKeys trap returned duplicate keys")
                                );
                            }
                            seen.push(k.clone());
                            keys.push(k);
                        }
                        _ => {
                            return Err(
                                self.throw_type("proxy ownKeys trap returned a non-key value")
                            )
                        }
                    }
                }
                // Invariants (10.5.11): reconcile the trap result with the target's
                // own keys. Non-configurable keys must appear; a non-extensible
                // target's result must be exactly its own keys.
                let extensible = target.borrow().extensible;
                let target_keys = self.own_keys(&target);
                let mut nonconfig: Vec<PropertyKey> = Vec::new();
                let mut config: Vec<PropertyKey> = Vec::new();
                for tk in &target_keys {
                    let is_config = own_property_descriptor(&target, tk)
                        .map(|p| p.configurable)
                        .unwrap_or(true);
                    if is_config {
                        config.push(tk.clone());
                    } else {
                        nonconfig.push(tk.clone());
                    }
                }
                if extensible && nonconfig.is_empty() {
                    return Ok(keys);
                }
                let mut unchecked = keys.clone();
                for k in &nonconfig {
                    match unchecked.iter().position(|u| keys_eq(u, k)) {
                        Some(pos) => {
                            unchecked.remove(pos);
                        }
                        None => {
                            return Err(self.throw_type(
                                "proxy ownKeys: a non-configurable target key is missing from the result",
                            ))
                        }
                    }
                }
                if extensible {
                    return Ok(keys);
                }
                for k in &config {
                    match unchecked.iter().position(|u| keys_eq(u, k)) {
                        Some(pos) => {
                            unchecked.remove(pos);
                        }
                        None => {
                            return Err(self.throw_type(
                                "proxy ownKeys: a target key is missing from the result of a non-extensible target",
                            ))
                        }
                    }
                }
                if !unchecked.is_empty() {
                    return Err(self.throw_type(
                        "proxy ownKeys: result contains keys absent from a non-extensible target",
                    ));
                }
                Ok(keys)
            }
        }
    }

    pub fn proxy_call(
        &mut self,
        o: &JsObject,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "apply")? {
            None => self.call(Value::Object(target), this, args),
            Some(trap) => {
                let args_arr = self.new_array(args.to_vec());
                self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target), this, Value::Object(args_arr)],
                )
            }
        }
    }

    pub fn proxy_construct(
        &mut self,
        o: &JsObject,
        args: &[Value],
        new_target: Value,
    ) -> Result<Value, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "construct")? {
            None => self.construct(&Value::Object(target), args, &new_target),
            Some(trap) => {
                let args_arr = self.new_array(args.to_vec());
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target), Value::Object(args_arr), new_target],
                )?;
                if !matches!(r, Value::Object(_)) {
                    return Err(self.throw_type("proxy construct trap must return an object"));
                }
                Ok(r)
            }
        }
    }

    pub fn proxy_get_prototype_of(&mut self, o: &JsObject) -> Result<Value, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "getPrototypeOf")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_get_prototype_of(&target);
                }
                Ok(target
                    .borrow()
                    .proto
                    .clone()
                    .map(Value::Object)
                    .unwrap_or(Value::Null))
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone())],
                )?;
                if !matches!(r, Value::Object(_) | Value::Null) {
                    return Err(
                        self.throw_type("proxy getPrototypeOf trap must return an object or null")
                    );
                }
                // Invariant (10.5.1): if the target is non-extensible, the trap
                // result must be the SameValue as the target's actual prototype.
                if !target.borrow().extensible {
                    let current = target
                        .borrow()
                        .proto
                        .clone()
                        .map(Value::Object)
                        .unwrap_or(Value::Null);
                    if !same_value(&r, &current) {
                        return Err(self.throw_type(
                            "proxy getPrototypeOf trap result must match the prototype of a non-extensible target",
                        ));
                    }
                }
                Ok(r)
            }
        }
    }

    pub fn proxy_set_prototype_of(&mut self, o: &JsObject, proto: Value) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "setPrototypeOf")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_set_prototype_of(&target, proto);
                }
                if !target.borrow().extensible {
                    // A non-extensible target's prototype is fixed: succeed only if
                    // the value matches the current prototype.
                    let current = target
                        .borrow()
                        .proto
                        .clone()
                        .map(Value::Object)
                        .unwrap_or(Value::Null);
                    return Ok(same_value(&proto, &current));
                }
                let p = match &proto {
                    Value::Object(po) => Some(po.clone()),
                    _ => None,
                };
                target.borrow_mut().proto = p;
                Ok(true)
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone()), proto.clone()],
                )?;
                if !self.to_boolean(&r) {
                    return Ok(false);
                }
                // Invariant (10.5.7): query the target's [[IsExtensible]] (which may
                // be a trap); if extensible, no further check. Otherwise the trap's
                // new prototype must SameValue the target's [[GetPrototypeOf]].
                let extensible = self.proxy_or_ordinary_is_extensible(&target)?;
                if extensible {
                    return Ok(true);
                }
                let current = self.proxy_or_ordinary_get_prototype_of(&target)?;
                if !same_value(&proto, &current) {
                    return Err(self.throw_type(
                        "proxy setPrototypeOf: cannot change the prototype of a non-extensible target",
                    ));
                }
                Ok(true)
            }
        }
    }

    pub fn proxy_is_extensible(&mut self, o: &JsObject) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "isExtensible")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_is_extensible(&target);
                }
                Ok(target.borrow().extensible)
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone())],
                )?;
                let b = self.to_boolean(&r);
                if b != target.borrow().extensible {
                    return Err(
                        self.throw_type("proxy isExtensible trap result must match the target")
                    );
                }
                Ok(b)
            }
        }
    }

    pub fn proxy_prevent_extensions(&mut self, o: &JsObject) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "preventExtensions")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_prevent_extensions(&target);
                }
                target.borrow_mut().extensible = false;
                Ok(true)
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone())],
                )?;
                let b = self.to_boolean(&r);
                if b && target.borrow().extensible {
                    return Err(self.throw_type(
                        "proxy preventExtensions trap returned true but the target is still extensible",
                    ));
                }
                Ok(b)
            }
        }
    }

    /// `[[GetOwnProperty]]` trap. Returns the descriptor object (or `undefined`).
    pub fn proxy_get_own_descriptor(
        &mut self,
        o: &JsObject,
        key: &PropertyKey,
    ) -> Result<Value, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "getOwnPropertyDescriptor")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_get_own_descriptor(&target, key);
                }
                Ok(match own_property_descriptor(&target, key) {
                    Some(p) => descriptor_to_object(self, &p),
                    None => Value::Undefined,
                })
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[Value::Object(target.clone()), key_to_value(key)],
                )?;
                if !matches!(r, Value::Object(_) | Value::Undefined) {
                    return Err(self.throw_type(
                        "proxy getOwnPropertyDescriptor trap must return an object or undefined",
                    ));
                }
                // [[GetOwnProperty]] invariants (10.5.5).
                let target_desc = own_property_descriptor(&target, key);
                let extensible = target.borrow().extensible;
                if r.is_undefined() {
                    return match &target_desc {
                        None => Ok(Value::Undefined),
                        Some(p) if !p.configurable => Err(self.throw_type(
                            "proxy [[GetOwnProperty]]: a non-configurable property cannot be reported as non-existent",
                        )),
                        Some(_) if !extensible => Err(self.throw_type(
                            "proxy [[GetOwnProperty]]: a property of a non-extensible target cannot be reported as non-existent",
                        )),
                        Some(_) => Ok(Value::Undefined),
                    };
                }
                // Validate the descriptor shape (get/set callable, etc.).
                to_property_descriptor(self, &r)?;
                // A property absent from a non-extensible target cannot be reported.
                if target_desc.is_none() && !extensible {
                    return Err(self.throw_type(
                        "proxy [[GetOwnProperty]]: cannot report a new property on a non-extensible target",
                    ));
                }
                // IsCompatiblePropertyDescriptor against the target descriptor.
                if let Some(p) = &target_desc {
                    if !self.is_compatible_descriptor(&r, p, extensible)? {
                        return Err(self.throw_type(
                            "proxy [[GetOwnProperty]]: the reported descriptor is incompatible with the target property",
                        ));
                    }
                }
                let rc = self.get_prop(&r, &PropertyKey::str("configurable"))?;
                let result_configurable = self.to_boolean(&rc);
                if !result_configurable {
                    match &target_desc {
                        None => return Err(self.throw_type(
                            "proxy [[GetOwnProperty]]: cannot report a non-existent property as non-configurable",
                        )),
                        Some(p) if p.configurable => return Err(self.throw_type(
                            "proxy [[GetOwnProperty]]: cannot report a configurable property as non-configurable",
                        )),
                        // A non-configurable, non-writable result cannot be reported
                        // for a non-configurable, writable target data property.
                        Some(p) => {
                            if let PropertyKind::Data { writable: true, .. } = &p.kind {
                                let rw = self.get_prop(&r, &PropertyKey::str("writable"))?;
                                if !self.to_boolean(&rw) {
                                    return Err(self.throw_type(
                                        "proxy [[GetOwnProperty]]: cannot report a non-writable descriptor for a writable non-configurable target property",
                                    ));
                                }
                            }
                        }
                    }
                }
                Ok(r)
            }
        }
    }

    /// IsCompatiblePropertyDescriptor(Extensible, Desc, Current) restricted to the
    /// cases reachable here (a `current` property is always present). `desc` is the
    /// raw descriptor object the trap returned/was handed. Returns `false` when the
    /// proposed change would violate the invariants of a non-configurable property.
    fn is_compatible_descriptor(
        &mut self,
        desc: &Value,
        current: &Property,
        _extensible: bool,
    ) -> Result<bool, Value> {
        // A configurable target property accepts any change.
        if current.configurable {
            return Ok(true);
        }
        let k = |s: &str| PropertyKey::str(s);
        let has = |vm: &mut Vm, s: &str| vm.has_prop(desc, &k(s));

        // configurable must not be flipped to true.
        if has(self, "configurable")? {
            let c = self.get_prop(desc, &k("configurable"))?;
            if self.to_boolean(&c) {
                return Ok(false);
            }
        }
        // enumerable, if present, must match.
        if has(self, "enumerable")? {
            let e = self.get_prop(desc, &k("enumerable"))?;
            if self.to_boolean(&e) != current.enumerable {
                return Ok(false);
            }
        }
        let desc_is_accessor = has(self, "get")? || has(self, "set")?;
        let desc_is_data = has(self, "value")? || has(self, "writable")?;
        // Generic descriptor (only enumerable/configurable): compatible.
        if !desc_is_accessor && !desc_is_data {
            return Ok(true);
        }
        match &current.kind {
            PropertyKind::Data {
                value: cur_val,
                writable: cur_writable,
            } => {
                // Cannot change a data property into an accessor.
                if desc_is_accessor {
                    return Ok(false);
                }
                if !*cur_writable {
                    // Non-writable: writable must stay false and value must match.
                    if has(self, "writable")? {
                        let w = self.get_prop(desc, &k("writable"))?;
                        if self.to_boolean(&w) {
                            return Ok(false);
                        }
                    }
                    if has(self, "value")? {
                        let v = self.get_prop(desc, &k("value"))?;
                        if !same_value(&v, cur_val) {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            }
            PropertyKind::Accessor {
                get: cur_get,
                set: cur_set,
            } => {
                // Cannot change an accessor into a data property.
                if desc_is_data {
                    return Ok(false);
                }
                let undef = Value::Undefined;
                if has(self, "get")? {
                    let g = self.get_prop(desc, &k("get"))?;
                    let cg = cur_get.clone().unwrap_or(undef.clone());
                    if !same_value(&g, &cg) {
                        return Ok(false);
                    }
                }
                if has(self, "set")? {
                    let s = self.get_prop(desc, &k("set"))?;
                    let cs = cur_set.clone().unwrap_or(undef);
                    if !same_value(&s, &cs) {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// `[[IsExtensible]]` of an object, dispatching the trap if it is a Proxy.
    fn proxy_or_ordinary_is_extensible(&mut self, o: &JsObject) -> Result<bool, Value> {
        if matches!(o.borrow().internal, Internal::Proxy(_)) {
            self.proxy_is_extensible(o)
        } else {
            Ok(o.borrow().extensible)
        }
    }

    /// `[[GetPrototypeOf]]` of an object, dispatching the trap if it is a Proxy.
    fn proxy_or_ordinary_get_prototype_of(&mut self, o: &JsObject) -> Result<Value, Value> {
        if matches!(o.borrow().internal, Internal::Proxy(_)) {
            self.proxy_get_prototype_of(o)
        } else {
            Ok(o.borrow()
                .proto
                .clone()
                .map(Value::Object)
                .unwrap_or(Value::Null))
        }
    }

    /// Ordinary `[[Set]]` returning the boolean success flag, used when a Proxy
    /// `set` trap is absent and the operation forwards to the target. If the
    /// target is itself a Proxy, dispatch its `[[Set]]` (trap or recursion).
    fn ordinary_set(
        &mut self,
        target: &JsObject,
        key: &PropertyKey,
        value: Value,
        receiver: Value,
    ) -> Result<bool, Value> {
        if matches!(target.borrow().internal, Internal::Proxy(_)) {
            return self.proxy_set(target, key, value, receiver);
        }
        // Walk the prototype chain looking for an own/inherited descriptor that
        // determines whether the write succeeds (accessor with/without setter,
        // writable/non-writable data property). A proxy encountered on the chain
        // re-dispatches its own `[[Set]]`.
        let mut cur = target.clone();
        loop {
            if matches!(cur.borrow().internal, Internal::Proxy(_)) {
                return self.proxy_set(&cur, key, value, receiver);
            }
            let own = own_property_descriptor(&cur, key);
            match own {
                Some(p) => match p.kind {
                    PropertyKind::Accessor { set, .. } => {
                        return match set {
                            Some(setter) => {
                                self.call(setter, receiver, &[value])?;
                                Ok(true)
                            }
                            None => Ok(false),
                        };
                    }
                    PropertyKind::Data { writable, .. } => {
                        if !writable {
                            return Ok(false);
                        }
                        // A writable data property exists on the chain: the write
                        // creates/updates an own data property on the receiver.
                        break;
                    }
                },
                None => {
                    let proto = cur.borrow().proto.clone();
                    match proto {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
            }
        }
        // CreateDataProperty / OrdinarySetWithOwnDescriptor on the receiver.
        match &receiver {
            Value::Object(r) => {
                if matches!(r.borrow().internal, Internal::Proxy(_)) {
                    // Receiver is a proxy. Per OrdinarySetWithOwnDescriptor, define
                    // the data property via its [[DefineOwnProperty]]: a fresh key
                    // is a full default data property; updating an existing data
                    // property only writes the value.
                    let existing = self.proxy_get_own_descriptor(r, key)?;
                    let d = self.new_object();
                    {
                        let mut b = d.borrow_mut();
                        b.props
                            .insert(PropertyKey::str("value"), Property::data(value.clone()));
                        if existing.is_undefined() {
                            b.props.insert(
                                PropertyKey::str("writable"),
                                Property::data(Value::Bool(true)),
                            );
                            b.props.insert(
                                PropertyKey::str("enumerable"),
                                Property::data(Value::Bool(true)),
                            );
                            b.props.insert(
                                PropertyKey::str("configurable"),
                                Property::data(Value::Bool(true)),
                            );
                        }
                    }
                    return self.proxy_define_property(r, key, Value::Object(d));
                }
                if !r.borrow().extensible && own_property_descriptor(r, key).is_none() {
                    return Ok(false);
                }
                // Reuse the engine's ordinary (sloppy) set, which honours
                // array/exotic handling and writability.
                self.set_prop(&Value::Object(r.clone()), key, value)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// `[[DefineOwnProperty]]` trap. `desc` is the user-supplied attributes object.
    pub fn proxy_define_property(
        &mut self,
        o: &JsObject,
        key: &PropertyKey,
        desc: Value,
    ) -> Result<bool, Value> {
        let (target, handler) = self.proxy_parts(o)?;
        match self.proxy_trap(&handler, "defineProperty")? {
            None => {
                if matches!(target.borrow().internal, Internal::Proxy(_)) {
                    return self.proxy_define_property(&target, key, desc);
                }
                // Forward to the ordinary OrdinaryDefineOwnProperty on the target.
                let d = to_property_descriptor(self, &desc)?;
                define_own_property(self, &target, key, &d, false)
            }
            Some(trap) => {
                let r = self.call(
                    trap,
                    Value::Object(handler),
                    &[
                        Value::Object(target.clone()),
                        key_to_value(key),
                        desc.clone(),
                    ],
                )?;
                if !self.to_boolean(&r) {
                    return Ok(false);
                }
                // [[DefineOwnProperty]] invariants (10.5.6).
                let target_desc = own_property_descriptor(&target, key);
                let extensible = target.borrow().extensible;
                let setting_config_false = {
                    if self.has_prop(&desc, &PropertyKey::str("configurable"))? {
                        let c = self.get_prop(&desc, &PropertyKey::str("configurable"))?;
                        !self.to_boolean(&c)
                    } else {
                        false
                    }
                };
                match &target_desc {
                    None => {
                        if !extensible {
                            return Err(self.throw_type(
                                "proxy [[DefineOwnProperty]]: cannot add a property to a non-extensible target",
                            ));
                        }
                        if setting_config_false {
                            return Err(self.throw_type(
                                "proxy [[DefineOwnProperty]]: cannot define a non-configurable property absent from the target",
                            ));
                        }
                    }
                    Some(p) => {
                        if setting_config_false && p.configurable {
                            return Err(self.throw_type(
                                "proxy [[DefineOwnProperty]]: cannot redefine a configurable target property as non-configurable",
                            ));
                        }
                        // IsCompatiblePropertyDescriptor against the existing target
                        // descriptor (step 16.b).
                        if !self.is_compatible_descriptor(&desc, p, extensible)? {
                            return Err(self.throw_type(
                                "proxy [[DefineOwnProperty]]: the trap reported a descriptor incompatible with the target property",
                            ));
                        }
                        // Step 16.c: a non-configurable, writable target data
                        // property cannot be redefined as non-writable.
                        if let PropertyKind::Data { writable, .. } = &p.kind {
                            if !p.configurable && *writable {
                                if self.has_prop(&desc, &PropertyKey::str("writable"))? {
                                    let w = self.get_prop(&desc, &PropertyKey::str("writable"))?;
                                    if !self.to_boolean(&w) {
                                        return Err(self.throw_type(
                                            "proxy [[DefineOwnProperty]]: cannot redefine a non-configurable writable property as non-writable",
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(true)
            }
        }
    }
}

/// Install the `Proxy` constructor and `Proxy.revocable`.
pub fn install(vm: &mut Vm) {
    fn obj_arg(vm: &mut Vm, args: &[Value], i: usize, what: &str) -> Result<JsObject, Value> {
        match args.get(i) {
            Some(Value::Object(o)) => Ok(o.clone()),
            _ => Err(vm.throw_type(&format!("Cannot create proxy with a non-object as {what}"))),
        }
    }

    let ctor = vm.new_native_ctor(
        "Proxy",
        2,
        |vm, _t, _a| Err(vm.throw_type("Constructor Proxy requires 'new'")),
        |vm, _t, args| {
            let target = obj_arg(vm, args, 0, "target")?;
            let handler = obj_arg(vm, args, 1, "handler")?;
            Ok(Value::Object(vm.new_proxy(target, handler)))
        },
    );

    // `Proxy.revocable(target, handler)` -> { proxy, revoke }.
    vm.define_method(&ctor, "revocable", 2, |vm, _t, args| {
        let target = obj_arg(vm, args, 0, "target")?;
        let handler = obj_arg(vm, args, 1, "handler")?;
        let proxy = vm.new_proxy(target, handler);
        let revoke = {
            let p = proxy.clone();
            vm.new_native("", 0, move |_vm, _t, _a| {
                if let Internal::Proxy(pd) = &mut p.borrow_mut().internal {
                    pd.revoked = true;
                }
                Ok(Value::Undefined)
            })
        };
        let result = vm.new_object();
        {
            let mut b = result.borrow_mut();
            b.props.insert(
                PropertyKey::str("proxy"),
                Property::data(Value::Object(proxy)),
            );
            b.props.insert(
                PropertyKey::str("revoke"),
                Property::data(Value::Object(revoke)),
            );
        }
        Ok(Value::Object(result))
    });

    // `Proxy` is an exotic constructor with no `prototype` property; bind it as a
    // plain global rather than via `install_ctor`.
    vm.define_value(&vm.realm.global.clone(), "Proxy", Value::Object(ctor));
}

fn keys_eq(a: &PropertyKey, b: &PropertyKey) -> bool {
    match (a, b) {
        (PropertyKey::Str(x), PropertyKey::Str(y)) => x.as_str() == y.as_str(),
        (PropertyKey::Sym(x), PropertyKey::Sym(y)) => x == y,
        _ => false,
    }
}
