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
            // No @@asyncIterator: take the SYNC iterator and wrap it in an
            // async-from-sync iterator, which is what gives each step's
            // `value` its Await (and closes the sync iterator when that value
            // is a rejected promise).
            let sync = self.get_iterator(v)?;
            let next = self.get_prop(&sync, &crate::names::key_next())?;
            return Ok(self.create_async_from_sync_iterator(sync, next));
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
        let next = self.get_prop(it, &crate::names::key_next())?;
        let res = self.call(next, it.clone(), &[])?;
        if !matches!(res, Value::Object(_)) {
            return Err(self.throw_type("iterator result is not an object"));
        }
        let done = self.get_prop(&res, &crate::names::key_done())?;
        if self.to_boolean(&done) {
            Ok(None)
        } else {
            let value = self.get_prop(&res, &crate::names::key_value())?;
            Ok(Some(value))
        }
    }

    /// Drain an iterable to a Vec. Fast-paths dense arrays.
    pub fn iterate_to_vec(&mut self, v: &Value) -> Result<Vec<Value>, Value> {
        // Fast path: dense array with the default iterator.
        if let Value::Object(o) = v {
            let is_plain_array = {
                let b = o.borrow();
                // `own_is_empty` excludes a reified index entry (which shadows
                // the dense slot) and a reified `length` (a sparse tail, whose
                // elements are not in the vec at all).
                b.array_is_dense() && b.own_is_empty()
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
        // The fast path replaces the whole iteration protocol, so it is only
        // safe while every observable step of it is still the intrinsic one:
        // no own `@@iterator` on the array, the prototype is the realm's
        // `Array.prototype`, its `@@iterator` is still the canonical `values`,
        // and `%ArrayIteratorPrototype%.next` is still the canonical `next`.
        let sym = self.realm.symbol_iterator.clone();
        let key = PropertyKey::Sym(sym);
        {
            let b = o.borrow();
            if b.own_contains_key(&key) {
                return false;
            }
            match &b.proto {
                Some(p) if p.same(&self.realm.array_proto) => {}
                _ => return false,
            }
        }
        let canonical_values = match &self.realm.array_values {
            Some(f) => f.clone(),
            None => return false,
        };
        match self
            .realm
            .array_proto
            .borrow()
            .own_get(&key)
            .and_then(|p| p.value().cloned())
        {
            Some(Value::Object(f)) if f.same(&canonical_values) => {}
            _ => return false,
        }
        let canonical_next = match self.realm.builtin_iter_next.first() {
            Some(f) => f.clone(),
            None => return false,
        };
        matches!(
            self.realm
                .array_iterator_proto
                .borrow()
                .own_get(&PropertyKey::str("next"))
                .and_then(|p| p.value().cloned()),
            Some(Value::Object(f)) if f.same(&canonical_next)
        )
    }

    pub fn iterator_close(&mut self, it: &Value) -> Result<(), Value> {
        let ret = self.get_prop(it, &crate::names::key_return())?;
        if self.is_callable(&ret) {
            let _ = self.call(ret, it.clone(), &[]);
        }
        Ok(())
    }

    /// `IteratorClose(iteratorRecord, completion)` in full: `Get(iterator,
    /// "return")` and the call it performs are both observable, and *which*
    /// error wins depends on `completion`.
    ///
    /// * `completion = Err(e)` — `e` always wins; errors from `return` (and a
    ///   non-object result) are swallowed.
    /// * `completion = Ok(())` — an error from `return` propagates, and a
    ///   non-object result is a TypeError.
    pub fn iterator_close_completion(
        &mut self,
        it: &Value,
        completion: Result<(), Value>,
    ) -> Result<(), Value> {
        let inner = (|vm: &mut Vm| -> Result<(), Value> {
            let ret = vm.get_prop(it, &crate::names::key_return())?;
            if ret.is_nullish() {
                return Ok(());
            }
            if !vm.is_callable(&ret) {
                return Err(vm.throw_type("iterator return is not a function"));
            }
            let r = vm.call(ret, it.clone(), &[])?;
            if !matches!(r, Value::Object(_)) {
                return Err(vm.throw_type("iterator return result is not an object"));
            }
            Ok(())
        })(self);
        match completion {
            Err(e) => Err(e),
            Ok(()) => inner,
        }
    }

    /// Build an iterator-result object `{ value, done }`.
    pub fn make_iter_result(&self, value: Value, done: bool) -> Value {
        let o = self.new_object();
        o.borrow_mut()
            .own_insert(crate::names::key_value(), Property::data(value));
        o.borrow_mut()
            .own_insert(crate::names::key_done(), Property::data(Value::Bool(done)));
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
        Ok(match self.builtin_iterator_step(it)? {
            Some(v) => self.make_iter_result(v, false),
            None => self.make_iter_result(Value::Undefined, true),
        })
    }

    /// Advance a built-in iterator, returning `Some(value)` or `None` when
    /// done — the allocation-free core of [`builtin_iterator_next`].
    /// `Op::IteratorStepValue` calls this directly when the loop's `next` is
    /// the pinned canonical, skipping the `{value, done}` result object.
    pub fn builtin_iterator_step(&mut self, it: &JsObject) -> Result<Option<Value>, Value> {
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
                return Ok(None);
            }
            let v = self.get_index(&base, idx as u32)?;
            if let Internal::Iterator(st) = &mut it.borrow_mut().internal {
                st.index += 1;
            }
            return Ok(Some(self.iter_entry(kind, idx, v)));
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
        // An own accessor shadowing an array index: its getter is user code,
        // so it runs OUTSIDE the iterator borrow (set in the array arm below).
        let mut pending_get: Option<(JsObject, usize, IterKind)> = None;
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
                                    if tb.is_array() {
                                        // `length` may exceed the dense store
                                        // (a sparse tail); a reified `props`
                                        // entry shadows the dense slot.
                                        let len = tb.array_length() as usize;
                                        // An own ACCESSOR at the index must run
                                        // its getter (Get(arr, idx) per spec).
                                        let shadow_prop = if tb.own_is_empty() {
                                            None
                                        } else {
                                            tb.own_get(&PropertyKey::from_index(idx as u32))
                                                .cloned()
                                        };
                                        if idx < len
                                            && matches!(
                                                &shadow_prop,
                                                Some(Property {
                                                    kind: PropertyKind::Accessor { .. },
                                                    ..
                                                })
                                            )
                                        {
                                            pending_get = Some((t.clone(), idx, kind));
                                        }
                                        let shadow = shadow_prop.and_then(|p| p.value().cloned());
                                        let v = if idx >= len {
                                            None
                                        } else if let Some(v) = shadow {
                                            Some(v)
                                        } else {
                                            // Holes (and absent sparse slots)
                                            // iterate as undefined.
                                            Some(match &tb.internal {
                                                Internal::Array(a) => match a.get(idx) {
                                                    Some(Value::Hole) | None => Value::Undefined,
                                                    Some(v) => v.clone(),
                                                },
                                                _ => Value::Undefined,
                                            })
                                        };
                                        (len, v)
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
            Out::Done => None,
            Out::Value(v) => {
                if let Some((target, idx, kind)) = pending_get {
                    let got = self
                        .get_prop(&Value::Object(target), &PropertyKey::from_index(idx as u32))?;
                    Some(self.iter_entry(kind, idx, got))
                } else {
                    Some(v)
                }
            }
        })
    }

    /// One sync for-of protocol round for `Op::IteratorStepValue`: `next` is
    /// the iterator record's cached next method, `it` the iterator. Returns
    /// `(value, done)` with `value == undefined` when done. When `next` is a
    /// pinned canonical builtin-iterator `next` and `it` a builtin iterator,
    /// the step runs inline with no call frame or result object; otherwise
    /// the generic path performs exactly the observable sequence this op
    /// replaced: `Call(next)`, the iterator-result type check, `Get(done)`,
    /// and `Get(value)` only when not done.
    pub fn iterator_step_value(&mut self, next: Value, it: Value) -> Result<(Value, bool), Value> {
        if let (Value::Object(nf), Value::Object(io)) = (&next, &it) {
            if matches!(io.borrow().internal, Internal::Iterator(_))
                && self.realm.builtin_iter_next.iter().any(|c| nf.ptr_eq(c))
            {
                let io = io.clone();
                return Ok(match self.builtin_iterator_step(&io)? {
                    Some(v) => (v, false),
                    None => (Value::Undefined, true),
                });
            }
        }
        let res = self.call(next, it, &[])?;
        if !matches!(res, Value::Object(_)) {
            return Err(self.throw_type("Iterator result is not an object"));
        }
        let done = self.get_prop(&res, &crate::names::key_done())?;
        if self.to_boolean(&done) {
            Ok((Value::Undefined, true))
        } else {
            Ok((self.get_prop(&res, &crate::names::key_value())?, false))
        }
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
    /// in deterministic order, de-duplicated, skipping shadowed keys. The
    /// returned buffer comes from the Vm's `forin_pool` (`ForInPop` parks it
    /// back) — a glue loop for-inning a fresh object per iteration reuses
    /// one allocation instead of a malloc/free per loop entry.
    pub fn for_in_keys(&mut self, v: &Value) -> Result<Vec<JsString>, Value> {
        let mut out = self.forin_pool.pop().unwrap_or_default();
        debug_assert!(out.is_empty());
        let obj = match v {
            Value::Object(o) => o.clone(),
            Value::Undefined | Value::Null => return Ok(out),
            _ => self.to_object(v)?,
        };
        // FAST PATH — the overwhelmingly common shape: an ORDINARY receiver
        // whose prototype chain CONTRIBUTES no enumerable string key (every
        // standard prototype is fully non-enumerable). One object's own
        // enumerable string keys are unique by construction, and a chain
        // that contributes nothing makes shadowing irrelevant — so the
        // shadow set and the per-level `own_keys` key-clone Vecs (together
        // ~14% of glue-shaped workloads) are skipped entirely. Anything
        // else — proxy, exotic index sources, an enumerable proto key —
        // falls back to the generic walk below, unchanged.
        if self.for_in_keys_fast(&obj, &mut out) {
            return Ok(out);
        }
        out.clear();
        // Dedup by `JsString` (an insert clones an `Rc`, not the bytes) under
        // the deterministic Fx hasher — the std SipHash `HashSet<String>` it
        // replaces paid a fresh `String` per key per chain level.
        // `mutable_key_type` is a false positive: `JsString`'s interior
        // `Cell` is a code-unit-count cache that participates in neither
        // `Hash` nor `Eq` (both go through `wtf8_bytes`), the same contract
        // the `props` maps already rely on.
        #[allow(clippy::mutable_key_type)]
        let mut seen: std::collections::HashSet<
            JsString,
            std::hash::BuildHasherDefault<crate::fxhash::FxHasher>,
        > = Default::default();
        let mut cur = Some(obj);
        while let Some(o) = cur {
            if self.is_proxy(&o) {
                // Proxy: own keys via the `ownKeys` trap, enumerability via the
                // `getOwnPropertyDescriptor` trap, prototype via `getPrototypeOf`.
                for k in self.own_property_keys(&o)? {
                    if let PropertyKey::Str(s) = &k {
                        if !seen.insert(s.clone()) {
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
            // A namespace level's per-key [[GetOwnProperty]] throws on a TDZ
            // export before any key is yielded.
            crate::builtins::fundamental::ns_tdz_check_all(self, &o)?;
            for k in self.enumerable_own_string_keys(&o) {
                if seen.insert(k.clone()) {
                    out.push(k);
                }
            }
            // Record even non-enumerable own keys as "seen" so they shadow.
            for k in self.own_keys(&o) {
                if let PropertyKey::Str(s) = k {
                    seen.insert(s);
                }
            }
            cur = o.borrow().proto.clone();
        }
        Ok(out)
    }

    /// Park a for-in enumerator's key buffer back in the pool (cleared —
    /// the pool never extends a key's lifetime past the `ForInPop` that
    /// parked it). Capacity-less buffers (empty enumerations that never
    /// grew) and a full pool just drop.
    pub(crate) fn park_forin_vec(&mut self, mut keys: Vec<JsString>) {
        keys.clear();
        if self.forin_pool.len() < 8 && keys.capacity() > 0 {
            self.forin_pool.push(keys);
        }
    }

    /// `for_in_keys`' allocation-free fast path, filling the caller's
    /// (pooled) buffer. `true` iff the receiver is an ordinary object and NO
    /// prototype-chain level contributes an enumerable string key — then
    /// the receiver's own enumerable string keys (index-likes sorted first,
    /// per `[[OwnPropertyKeys]]`) ARE the for-in keys, no shadow set
    /// needed. `false` = use the generic walk (the caller clears the
    /// buffer); the split is decided by the same facts that walk reads, so
    /// both produce identical keys where this path applies.
    fn for_in_keys_fast(&self, obj: &JsObject, out: &mut Vec<JsString>) -> bool {
        let mut ints: Vec<u32> = Vec::new();
        let proto = {
            let b = obj.borrow();
            // Ordinary receivers only: exotic internals (dense elements,
            // string indices, typed arrays, proxies, namespace exports)
            // synthesize own keys that live outside `props`.
            if !matches!(b.internal, Internal::Ordinary) {
                return false;
            }
            for (k, p) in b.own_iter() {
                if let PropertyKey::Str(s) = k {
                    // Internal-slot keys are non-enumerable by contract, so
                    // `p.enumerable` alone excludes them, as on the generic
                    // path.
                    if !p.enumerable {
                        continue;
                    }
                    match k.array_index() {
                        Some(i) => ints.push(i),
                        None => out.push(s.clone()),
                    }
                }
            }
            b.proto.clone()
        };
        // Index-like keys enumerate first, ascending (map keys are unique,
        // so no dedup); the plain names keep insertion order after them.
        if !ints.is_empty() {
            ints.sort_unstable();
            let mut merged: Vec<JsString> = Vec::with_capacity(ints.len() + out.len());
            for i in ints {
                match PropertyKey::from_index(i) {
                    PropertyKey::Str(s) => merged.push(s),
                    PropertyKey::Sym(_) => unreachable!("index keys are strings"),
                }
            }
            merged.append(out);
            std::mem::swap(out, &mut merged);
        }
        // The rest of the chain must contribute NOTHING: no enumerable
        // string key in `props`, and no internal that synthesizes own
        // enumerable keys. Any contribution (or a proxy, whose traps must
        // run) sends the whole walk down the generic path.
        let mut cur = proto;
        while let Some(o) = cur {
            let b = o.borrow();
            match &b.internal {
                Internal::Proxy(_)
                | Internal::StringObj(_)
                | Internal::TypedArray(_)
                | Internal::ModuleNamespace(_) => return false,
                // An EMPTY dense array (Array.prototype!) contributes only
                // its non-enumerable `length`; any element is enumerable.
                Internal::Array(arr) if arr.iter().any(|v| !matches!(v, Value::Hole)) => {
                    return false;
                }
                _ => {}
            }
            for (k, p) in b.own_iter() {
                if p.enumerable && matches!(k, PropertyKey::Str(_)) {
                    return false;
                }
            }
            let next = b.proto.clone();
            drop(b);
            cur = next;
        }
        true
    }
}

// =========================================================================
// %AsyncFromSyncIteratorPrototype% (ECMA-262 27.1.4)
// =========================================================================
//
// `GetIterator(obj, async)` on an object that only has `@@iterator` wraps the
// sync iterator in one of these. Every method returns a promise and every
// step's `value` goes through `PromiseResolve(%Promise%, value)` — which is
// what makes `for await (const x of [Promise.resolve(1)])` see `1`, and what
// closes the sync iterator when a step's value is a rejected promise.

impl Vm {
    /// The `[[SyncIteratorRecord]]` slot: a 2-element array `[iterator, next]`.
    fn sync_record_key(&self) -> PropertyKey {
        PropertyKey::Sym(self.realm.symbol_sync_iterator_record.clone())
    }

    /// `CreateAsyncFromSyncIterator(syncIteratorRecord)`.
    pub fn create_async_from_sync_iterator(&mut self, iterator: Value, next: Value) -> Value {
        let rec = self.new_array(vec![iterator, next]);
        let proto = self.realm.async_from_sync_iterator_proto.clone();
        let o = self.alloc_ordinary(Some(proto));
        let key = self.sync_record_key();
        o.borrow_mut()
            .own_insert(key, Property::builtin(Value::Object(rec)));
        Value::Object(o)
    }

    /// The `(iterator, nextMethod)` of an async-from-sync wrapper `this`.
    fn sync_iterator_record(&self, this: &Value) -> Option<(Value, Value)> {
        let Value::Object(o) = this else {
            return None;
        };
        let key = self.sync_record_key();
        let rec = match &o.borrow().own_get(&key)?.kind {
            PropertyKind::Data {
                value: Value::Object(a),
                ..
            } => a.clone(),
            _ => return None,
        };
        let b = rec.borrow();
        match &b.internal {
            Internal::Array(a) if a.len() == 2 => Some((a[0].clone(), a[1].clone())),
            _ => None,
        }
    }

    /// `AsyncFromSyncIteratorContinuation(result, capability, syncIteratorRecord,
    /// closeOnRejection)` — unwrap the step result's `value` through a promise
    /// and re-package it as `{ value, done }`.
    fn async_from_sync_continuation(
        &mut self,
        result: Value,
        promise: JsObject,
        sync_iterator: Value,
        close_on_rejection: bool,
    ) -> Result<Value, Value> {
        // Steps 1-4: IteratorComplete / IteratorValue, each IfAbruptRejectPromise.
        let outcome = (|vm: &mut Vm| -> Result<(bool, Value), Value> {
            let done = vm.get_prop(&result, &crate::names::key_done())?;
            let done = vm.to_boolean(&done);
            let value = vm.get_prop(&result, &crate::names::key_value())?;
            Ok((done, value))
        })(self);
        let (done, value) = match outcome {
            Ok(v) => v,
            Err(e) => {
                self.reject_promise(&promise, e);
                return Ok(Value::Object(promise));
            }
        };
        // Steps 5-7: PromiseResolve(%Promise%, value) is observable; when it
        // throws mid-iteration the sync iterator is closed first.
        let wrapper = match self.promise_resolve_intrinsic(value) {
            Ok(w) => w,
            Err(e) => {
                let e = if !done && close_on_rejection {
                    self.iterator_close_completion(&sync_iterator, Err(e))
                        .err()
                        .unwrap_or_else(|| self.throw_type("internal: iterator close"))
                } else {
                    e
                };
                self.reject_promise(&promise, e);
                return Ok(Value::Object(promise));
            }
        };
        // Steps 8-9: onFulfilled re-packages the awaited value.
        let on_f = self.new_native("", 1, move |vm, _t, a| {
            let v = a.first().cloned().unwrap_or(Value::Undefined);
            Ok(vm.make_iter_result(v, done))
        });
        // Steps 11-12: mid-iteration, a rejected value closes the sync
        // iterator before the rejection propagates.
        let on_r = if done || !close_on_rejection {
            Value::Undefined
        } else {
            let it = sync_iterator;
            Value::Object(self.new_native("", 1, move |vm, _t, a| {
                let e = a.first().cloned().unwrap_or(Value::Undefined);
                Err(vm
                    .iterator_close_completion(&it, Err(e))
                    .err()
                    .unwrap_or_else(|| vm.throw_type("internal: iterator close")))
            }))
        };
        // Step 13-14: PerformPromiseThen(valueWrapper, …, promiseCapability).
        self.promise_then_into(&wrapper, Value::Object(on_f), on_r, promise.clone());
        Ok(Value::Object(promise))
    }
}

/// Install `%AsyncFromSyncIteratorPrototype%`'s `next`/`return`/`throw`.
pub fn install(vm: &mut Vm) {
    let proto = vm.realm.async_from_sync_iterator_proto.clone();

    vm.define_method(&proto, "next", 1, |vm, this, args| {
        let Some((sync_iter, next)) = vm.sync_iterator_record(&this) else {
            return Err(vm.throw_type("not an async-from-sync iterator"));
        };
        let cap = vm.new_promise();
        // IteratorNext(record, value) — `value` is forwarded only when present.
        let result = vm
            .call(next, sync_iter.clone(), &args[..args.len().min(1)])
            .and_then(|r| {
                if matches!(r, Value::Object(_)) {
                    Ok(r)
                } else {
                    Err(vm.throw_type("iterator result is not an object"))
                }
            });
        match result {
            Ok(r) => vm.async_from_sync_continuation(r, cap, sync_iter, true),
            Err(e) => {
                vm.reject_promise(&cap, e);
                Ok(Value::Object(cap))
            }
        }
    });

    vm.define_method(&proto, "return", 1, |vm, this, args| {
        let Some((sync_iter, _next)) = vm.sync_iterator_record(&this) else {
            return Err(vm.throw_type("not an async-from-sync iterator"));
        };
        let cap = vm.new_promise();
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        let outcome = (|vm: &mut Vm| -> Result<Option<Value>, Value> {
            let ret = vm.get_prop(&sync_iter, &crate::names::key_return())?;
            if ret.is_nullish() {
                return Ok(None);
            }
            if !vm.is_callable(&ret) {
                return Err(vm.throw_type("iterator return is not a function"));
            }
            let r = vm.call(ret, sync_iter.clone(), &args[..args.len().min(1)])?;
            if !matches!(r, Value::Object(_)) {
                return Err(vm.throw_type("iterator return result is not an object"));
            }
            Ok(Some(r))
        })(vm);
        match outcome {
            // No `return` method: fulfil with `{ value, done: true }` — the
            // sync iterator is simply not closed.
            Ok(None) => {
                let r = vm.make_iter_result(value, true);
                vm.resolve_promise(&cap, r);
                Ok(Value::Object(cap))
            }
            // closeOnRejection is FALSE here: the iterator is already closing.
            Ok(Some(r)) => vm.async_from_sync_continuation(r, cap, sync_iter, false),
            Err(e) => {
                vm.reject_promise(&cap, e);
                Ok(Value::Object(cap))
            }
        }
    });

    vm.define_method(&proto, "throw", 1, |vm, this, args| {
        let Some((sync_iter, _next)) = vm.sync_iterator_record(&this) else {
            return Err(vm.throw_type("not an async-from-sync iterator"));
        };
        let cap = vm.new_promise();
        let outcome = (|vm: &mut Vm| -> Result<Option<Value>, Value> {
            let thr = vm.get_prop(&sync_iter, &PropertyKey::str("throw"))?;
            if thr.is_nullish() {
                return Ok(None);
            }
            if !vm.is_callable(&thr) {
                return Err(vm.throw_type("iterator throw is not a function"));
            }
            let r = vm.call(thr, sync_iter.clone(), &args[..args.len().min(1)])?;
            if !matches!(r, Value::Object(_)) {
                return Err(vm.throw_type("iterator throw result is not an object"));
            }
            Ok(Some(r))
        })(vm);
        match outcome {
            // No `throw` method: close the sync iterator, then reject with a
            // TypeError (an error from the close takes precedence).
            Ok(None) => {
                let e = match vm.iterator_close_completion(&sync_iter, Ok(())) {
                    Ok(()) => vm.throw_type("The iterator does not provide a 'throw' method"),
                    Err(e) => e,
                };
                vm.reject_promise(&cap, e);
                Ok(Value::Object(cap))
            }
            Ok(Some(r)) => vm.async_from_sync_continuation(r, cap, sync_iter, true),
            Err(e) => {
                vm.reject_promise(&cap, e);
                Ok(Value::Object(cap))
            }
        }
    });
}
