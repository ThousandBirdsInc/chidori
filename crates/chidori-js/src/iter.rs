//! Iteration protocol helpers: `Symbol.iterator`, iterator stepping, draining an
//! iterable to a `Vec`, iterator close, and `for-in` key collection.

use crate::value::*;
use crate::vm::Vm;

impl Vm {
    /// Get an iterator object by invoking `obj[Symbol.iterator]()`.
    pub fn get_iterator(&mut self, v: &Value) -> Result<Value, Value> {
        let sym = self.realm.symbol_iterator.clone();
        let method = self.get_prop(v, &PropertyKey::Sym(sym))?;
        if !self.is_callable(&method) {
            return Err(self.throw_type(&format!("{} is not iterable", v.type_of())));
        }
        let it = self.call(method, v.clone(), &[])?;
        if !matches!(it, Value::Object(_)) {
            return Err(self.throw_type("iterator method did not return an object"));
        }
        Ok(it)
    }

    /// Get an async iterator via `obj[Symbol.asyncIterator]()`, falling back to
    /// the sync iterator (wrapped) when no async iterator exists.
    pub fn get_async_iterator(&mut self, v: &Value) -> Result<Value, Value> {
        let sym = self.realm.symbol_async_iterator.clone();
        // GetMethod(obj, @@asyncIterator): absent/null → fall back to sync; a
        // present-but-non-callable value is a TypeError (and must NOT then probe
        // @@iterator — spec GetIterator with hint=async / GetMethod).
        let method = self.get_prop(v, &PropertyKey::Sym(sym))?;
        if method.is_nullish() {
            // Fall back to the sync iterator; for-await awaits each next() result,
            // so a sync iterator (incl. arrays of promises) works.
            return self.get_iterator(v);
        }
        if !self.is_callable(&method) {
            return Err(self.throw_type("Symbol.asyncIterator is not a function"));
        }
        let it = self.call(method, v.clone(), &[])?;
        if !matches!(it, Value::Object(_)) {
            return Err(self.throw_type("async iterator method did not return an object"));
        }
        Ok(it)
    }

    /// Step an iterator: returns `Some(value)` or `None` when done.
    pub fn iterator_step(&mut self, it: &Value) -> Result<Option<Value>, Value> {
        let next = self.get_prop(it, &PropertyKey::str("next"))?;
        let res = self.call(next, it.clone(), &[])?;
        if !matches!(res, Value::Object(_)) {
            return Err(self.throw_type("iterator result is not an object"));
        }
        let done = self.get_prop(&res, &PropertyKey::str("done"))?;
        if self.to_boolean(&done) {
            Ok(None)
        } else {
            let value = self.get_prop(&res, &PropertyKey::str("value"))?;
            Ok(Some(value))
        }
    }

    /// Drain an iterable to a Vec. Fast-paths dense arrays.
    pub fn iterate_to_vec(&mut self, v: &Value) -> Result<Vec<Value>, Value> {
        // Fast path: dense array with the default iterator.
        if let Value::Object(o) = v {
            let is_plain_array = {
                let b = o.borrow();
                matches!(b.internal, Internal::Array(_))
            };
            if is_plain_array && self.has_default_array_iterator(o) {
                if let Internal::Array(arr) = &o.borrow().internal {
                    // The array iterator reads holes as undefined (Get over
                    // 0..length), so the produced list is fully dense.
                    return Ok(arr
                        .iter()
                        .map(|v| {
                            if matches!(v, Value::Hole) {
                                Value::Undefined
                            } else {
                                v.clone()
                            }
                        })
                        .collect());
                }
            }
        }
        if let Value::String(s) = v {
            return Ok(s
                .code_point_strings()
                .into_iter()
                .map(Value::String)
                .collect());
        }
        let it = self.get_iterator(v)?;
        let mut out = Vec::new();
        while let Some(val) = self.iterator_step(&it)? {
            out.push(val);
        }
        Ok(out)
    }

    fn has_default_array_iterator(&self, o: &JsObject) -> bool {
        // If the object's own + proto chain Symbol.iterator is the built-in
        // Array.prototype[Symbol.iterator], the fast path is safe. We approximate
        // by checking the prototype is the realm array_proto and no own override.
        let sym = self.realm.symbol_iterator.clone();
        let key = PropertyKey::Sym(sym);
        let b = o.borrow();
        if b.props.contains_key(&key) {
            return false;
        }
        match &b.proto {
            Some(p) => p.same(&self.realm.array_proto),
            None => false,
        }
    }

    pub fn iterator_close(&mut self, it: &Value) -> Result<(), Value> {
        let ret = self.get_prop(it, &PropertyKey::str("return"))?;
        if self.is_callable(&ret) {
            let _ = self.call(ret, it.clone(), &[]);
        }
        Ok(())
    }

    /// Build an iterator-result object `{ value, done }`.
    pub fn make_iter_result(&self, value: Value, done: bool) -> Value {
        let o = self.new_object();
        o.borrow_mut()
            .props
            .insert(PropertyKey::str("value"), Property::data(value));
        o.borrow_mut()
            .props
            .insert(PropertyKey::str("done"), Property::data(Value::Bool(done)));
        Value::Object(o)
    }

    /// Construct a built-in iterator object.
    pub fn make_iterator(
        &self,
        proto: &JsObject,
        target: Option<JsObject>,
        string: Option<JsString>,
        kind: IterKind,
    ) -> Value {
        Value::Object(self.alloc(ObjectData::new(
            Some(proto.clone()),
            Internal::Iterator(IterState {
                target,
                string,
                index: 0,
                kind,
                done: false,
            }),
        )))
    }

    /// Advance a built-in iterator, returning an iterator-result object.
    pub fn builtin_iterator_next(&mut self, it: &JsObject) -> Result<Value, Value> {
        // An Array* iterator over an array-like that is neither a dense array
        // nor a typed array (e.g. the `arguments` object): step it via generic
        // length/index reads, OUTSIDE the iterator borrow (the reads can run
        // user getters).
        let generic = {
            let b = it.borrow();
            match &b.internal {
                Internal::Iterator(st)
                    if !st.done
                        && matches!(
                            st.kind,
                            IterKind::ArrayKeys | IterKind::ArrayValues | IterKind::ArrayEntries
                        ) =>
                {
                    st.target.as_ref().and_then(|t| {
                        let ti = &t.borrow().internal;
                        if matches!(ti, Internal::Array(_) | Internal::TypedArray(_)) {
                            None
                        } else {
                            Some((t.clone(), st.index, st.kind))
                        }
                    })
                }
                _ => None,
            }
        };
        if let Some((target, idx, kind)) = generic {
            let base = Value::Object(target);
            let len_v = self.get_prop(&base, &PropertyKey::str("length"))?;
            let len = self.to_length(&len_v)?;
            if idx >= len {
                if let Internal::Iterator(st) = &mut it.borrow_mut().internal {
                    st.done = true;
                }
                return Ok(self.make_iter_result(Value::Undefined, true));
            }
            let v = self.get_index(&base, idx as u32)?;
            if let Internal::Iterator(st) = &mut it.borrow_mut().internal {
                st.index += 1;
            }
            let entry = self.iter_entry(kind, idx, v);
            return Ok(self.make_iter_result(entry, false));
        }
        // %ArrayIterator%.next over a typed array re-validates the view each
        // step: a detached or out-of-bounds (shrunk resizable buffer) view is
        // a TypeError, not a quiet `done`.
        let ta_oob = {
            let b = it.borrow();
            match &b.internal {
                Internal::Iterator(st) if !st.done => st.target.as_ref().is_some_and(|t| match &t
                    .borrow()
                    .internal
                {
                    Internal::TypedArray(td) => crate::typed_array::ta_out_of_bounds(td),
                    _ => false,
                }),
                _ => false,
            }
        };
        if ta_oob {
            return Err(self.throw_type("TypedArray is detached or out of bounds"));
        }
        // Read + advance under a short borrow; build result after.
        enum Out {
            Done,
            Value(Value),
        }
        let out = {
            let mut b = it.borrow_mut();
            let st = match &mut b.internal {
                Internal::Iterator(s) => s,
                _ => return Err(self.throw_type("not an iterator")),
            };
            if st.done {
                Out::Done
            } else {
                let idx = st.index;
                let kind = st.kind;
                let res = match kind {
                    IterKind::StringChars => {
                        // `index` is a UTF-16 code-unit offset; each step yields
                        // one code point (combining a surrogate pair), preserving
                        // lone surrogates as a single one-unit string.
                        let s = st.string.clone().unwrap_or_else(|| JsString::new(""));
                        let units = s.to_utf16_vec();
                        if idx < units.len() {
                            let end = crate::value::next_code_point_boundary(&units, idx);
                            st.index = end;
                            Some(Value::String(JsString::from_code_units(&units[idx..end])))
                        } else {
                            None
                        }
                    }
                    IterKind::ArrayKeys | IterKind::ArrayValues | IterKind::ArrayEntries => {
                        let target = st.target.clone();
                        match &target {
                            Some(t) => {
                                let is_ta = matches!(t.borrow().internal, Internal::TypedArray(_));
                                let (len, val) = if is_ta {
                                    // Live read: length and element are re-read each
                                    // step so mutations during iteration are seen.
                                    let len = self.ta_length(t).unwrap_or(0);
                                    let val = if idx < len {
                                        self.ta_get(t, idx)
                                    } else {
                                        Value::Undefined
                                    };
                                    (len, Some(val))
                                } else {
                                    let tb = t.borrow();
                                    if let Internal::Array(a) = &tb.internal {
                                        // Holes iterate as undefined.
                                        let v = a.get(idx).map(|v| {
                                            if matches!(v, Value::Hole) {
                                                Value::Undefined
                                            } else {
                                                v.clone()
                                            }
                                        });
                                        (a.len(), v)
                                    } else {
                                        (0, None)
                                    }
                                };
                                if idx >= len {
                                    None
                                } else {
                                    st.index += 1;
                                    Some(self.iter_entry(
                                        kind,
                                        idx,
                                        val.unwrap_or(Value::Undefined),
                                    ))
                                }
                            }
                            None => None,
                        }
                    }
                    IterKind::MapKeys
                    | IterKind::MapValues
                    | IterKind::MapEntries
                    | IterKind::SetValues
                    | IterKind::SetEntries => {
                        let target = st.target.clone();
                        let entry = target.as_ref().and_then(|t| {
                            let tb = t.borrow();
                            match &tb.internal {
                                Internal::Map(m) => {
                                    m.get_index(idx).map(|(k, v)| (k.0.clone(), v.clone()))
                                }
                                Internal::Set(s) => {
                                    s.get_index(idx).map(|(k, _)| (k.0.clone(), k.0.clone()))
                                }
                                _ => None,
                            }
                        });
                        match entry {
                            Some((k, v)) => {
                                st.index += 1;
                                Some(self.map_entry(kind, k, v))
                            }
                            None => None,
                        }
                    }
                };
                match res {
                    Some(v) => Out::Value(v),
                    None => {
                        st.done = true;
                        Out::Done
                    }
                }
            }
        };
        Ok(match out {
            Out::Done => self.make_iter_result(Value::Undefined, true),
            Out::Value(v) => self.make_iter_result(v, false),
        })
    }

    fn iter_entry(&self, kind: IterKind, index: usize, value: Value) -> Value {
        match kind {
            IterKind::ArrayKeys => Value::Number(index as f64),
            IterKind::ArrayValues => value,
            IterKind::ArrayEntries => {
                Value::Object(self.new_array(vec![Value::Number(index as f64), value]))
            }
            _ => value,
        }
    }

    fn map_entry(&self, kind: IterKind, k: Value, v: Value) -> Value {
        match kind {
            IterKind::MapKeys => k,
            IterKind::MapValues | IterKind::SetValues => v,
            IterKind::MapEntries | IterKind::SetEntries => {
                Value::Object(self.new_array(vec![k, v]))
            }
            _ => v,
        }
    }

    /// Collect enumerable string keys across the prototype chain for `for-in`,
    /// in deterministic order, de-duplicated, skipping shadowed keys.
    pub fn for_in_keys(&mut self, v: &Value) -> Result<Vec<JsString>, Value> {
        let mut out: Vec<JsString> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let obj = match v {
            Value::Object(o) => o.clone(),
            Value::Undefined | Value::Null => return Ok(out),
            _ => self.to_object(v)?,
        };
        let mut cur = Some(obj);
        while let Some(o) = cur {
            if self.is_proxy(&o) {
                // Proxy: own keys via the `ownKeys` trap, enumerability via the
                // `getOwnPropertyDescriptor` trap, prototype via `getPrototypeOf`.
                for k in self.own_property_keys(&o)? {
                    if let PropertyKey::Str(s) = &k {
                        if !seen.insert(s.as_str().to_string()) {
                            continue; // shadowed by a nearer object
                        }
                        let desc = self.proxy_get_own_descriptor(&o, &k)?;
                        let enumerable = match &desc {
                            Value::Object(_) => {
                                let e = self.get_prop(&desc, &PropertyKey::str("enumerable"))?;
                                self.to_boolean(&e)
                            }
                            _ => false,
                        };
                        if enumerable {
                            out.push(s.clone());
                        }
                    }
                }
                cur = match self.proxy_get_prototype_of(&o)? {
                    Value::Object(p) => Some(p),
                    _ => None,
                };
                continue;
            }
            for k in self.enumerable_own_string_keys(&o) {
                if seen.insert(k.as_str().to_string()) {
                    out.push(k);
                }
            }
            // Record even non-enumerable own keys as "seen" so they shadow.
            for k in self.own_keys(&o) {
                if let PropertyKey::Str(s) = k {
                    seen.insert(s.as_str().to_string());
                }
            }
            cur = o.borrow().proto.clone();
        }
        Ok(out)
    }
}
