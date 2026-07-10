//! ArrayBuffer, SharedArrayBuffer, %TypedArray% (the shared base), the nine
//! concrete typed-array constructors, DataView, and the Atomics namespace. The
//! element storage and indexed `[[Get]]`/`[[Set]]` exotic behavior live in
//! `crate::typed_array` and the VM; this module builds the observable builtin
//! surface (constructors, prototype methods, static helpers) on top of those
//! primitives.

use super::arg;
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    // The canonical `Symbol.species` (installed on `Symbol` by `fundamental`),
    // reused for the ArrayBuffer/%TypedArray% species accessors below.
    let species = vm.realm.symbol_species.clone();

    install_array_buffer(vm, &species);
    install_shared_array_buffer(vm, &species);
    let ta_ctor = install_typed_array_base(vm, &species);
    install_kind_ctors(vm, &ta_ctor);
    install_data_view(vm);
    install_atomics(vm);
}

// =========================================================================
// Symbol.species
// =========================================================================

/// Define `ctor[Symbol.species]` as a (non-enumerable) getter returning the
/// constructor itself. `species` must be the canonical `realm.symbol_species`.
fn define_species_getter(vm: &mut Vm, ctor: &JsObject, species: &JsSymbol) {
    let getter = vm.new_native("get [Symbol.species]", 0, |_vm, this, _a| Ok(this));
    vm.define_accessor_with(
        &Value::Object(ctor.clone()),
        PropertyKey::Sym(species.clone()),
        Some(Value::Object(getter)),
        None,
        false,
    );
}

// =========================================================================
// ArrayBuffer
// =========================================================================

/// Validate that `this` is an ArrayBuffer object whose shared-ness matches
/// `want_shared`, returning its handle or a TypeError naming the surface. This
/// is the brand split between the `ArrayBuffer.prototype` and
/// `SharedArrayBuffer.prototype` method/accessor families (both share the
/// `[[ArrayBufferData]]` slot; only `IsSharedArrayBuffer` tells them apart).
fn ab_receiver(
    vm: &mut Vm,
    this: &Value,
    want_shared: bool,
    what: &str,
) -> Result<JsObject, Value> {
    if let Value::Object(o) = this {
        if matches!(o.borrow().internal, Internal::ArrayBuffer(_))
            && vm.is_shared_buffer(o) == want_shared
        {
            return Ok(o.clone());
        }
    }
    let proto = if want_shared {
        "SharedArrayBuffer"
    } else {
        "ArrayBuffer"
    };
    Err(vm.throw_type(&format!(
        "{proto}.prototype.{what} called on incompatible receiver"
    )))
}

/// Read the byte length of an ArrayBuffer object (0 if detached or not a buffer).
fn buffer_byte_length(o: &JsObject) -> usize {
    match &o.borrow().internal {
        Internal::ArrayBuffer(Some(bytes)) => bytes.len(),
        _ => 0,
    }
}

/// Hidden own-property holding a resizable ArrayBuffer's
/// `[[ArrayBufferMaxByteLength]]` (an internal slot; see the engine-core const).
use crate::typed_array::ARRAY_BUFFER_MAX_SLOT as AB_MAX;

/// The resizable max for a buffer, or `None` when it is fixed-length.
fn ab_max_byte_length(o: &JsObject) -> Option<usize> {
    match o.borrow().props.get(&PropertyKey::str(AB_MAX)) {
        Some(Property {
            kind:
                PropertyKind::Data {
                    value: Value::Number(n),
                    ..
                },
            ..
        }) => Some(*n as usize),
        _ => None,
    }
}

/// ArrayBufferCopyAndDetach: copy `this`'s bytes into a fresh buffer of
/// `newLength` (default: current length), optionally preserving resizability,
/// then detach the source. Backs `transfer`/`transferToFixedLength`.
fn ab_transfer(
    vm: &mut Vm,
    this: &Value,
    args: &[Value],
    preserve_resizable: bool,
) -> Result<Value, Value> {
    let o = ab_receiver(vm, this, false, "transfer")?;
    let (old_bytes, max) = {
        let b = o.borrow();
        match &b.internal {
            Internal::ArrayBuffer(Some(bytes)) => (bytes.clone(), ab_max_byte_length(&o)),
            _ => return Err(vm.throw_type("ArrayBuffer.prototype.transfer: buffer is detached")),
        }
    };
    let new_len = if arg(args, 0).is_undefined() {
        old_bytes.len()
    } else {
        byte_length_arg(vm, &arg(args, 0))?
    };
    let new_buf = vm.new_array_buffer(new_len);
    {
        let mut nb = new_buf.borrow_mut();
        if let Internal::ArrayBuffer(Some(nbytes)) = &mut nb.internal {
            let n = old_bytes.len().min(new_len);
            nbytes[..n].copy_from_slice(&old_bytes[..n]);
        }
    }
    if preserve_resizable {
        if let Some(m) = max {
            new_buf.borrow_mut().props.insert(
                PropertyKey::str(AB_MAX),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(m as f64),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        }
    }
    // Detach the source buffer.
    if let Internal::ArrayBuffer(slot) = &mut o.borrow_mut().internal {
        *slot = None;
    }
    Ok(Value::Object(new_buf))
}

fn install_array_buffer(vm: &mut Vm, species: &JsSymbol) {
    let proto = vm.realm.array_buffer_proto.clone();

    let construct = |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
        let len = byte_length_arg(vm, &arg(args, 0))?;
        let buf = vm.new_array_buffer(len);
        // Optional `{ maxByteLength }` makes the buffer resizable.
        match arg(args, 1) {
            Value::Undefined => {}
            Value::Object(opts) => {
                let mbl = vm.get_prop(&Value::Object(opts), &PropertyKey::str("maxByteLength"))?;
                if !mbl.is_undefined() {
                    let max = byte_length_arg(vm, &mbl)?;
                    if len > max {
                        return Err(vm.throw_range("ArrayBuffer length exceeds maxByteLength"));
                    }
                    buf.borrow_mut().props.insert(
                        PropertyKey::str(AB_MAX),
                        Property {
                            kind: PropertyKind::Data {
                                value: Value::Number(max as f64),
                                writable: false,
                            },
                            enumerable: false,
                            configurable: false,
                        },
                    );
                }
            }
            // GetArrayBufferMaxByteLengthOption: a non-object `options` is
            // ignored (the buffer is non-resizable), not a TypeError.
            _ => {}
        }
        Ok(Value::Object(buf))
    };
    let ctor = vm.new_native_ctor(
        "ArrayBuffer",
        1,
        |vm, _t, _a| Err(vm.throw_type("Constructor ArrayBuffer requires 'new'")),
        construct,
    );
    vm.install_ctor("ArrayBuffer", &ctor, &proto);

    // ArrayBuffer.isView(x)
    vm.define_method(&ctor, "isView", 1, |_vm, _t, args| {
        let v = arg(args, 0);
        let is_view = matches!(
            &v,
            Value::Object(o) if matches!(
                o.borrow().internal,
                Internal::TypedArray(_) | Internal::DataView(_)
            )
        );
        Ok(Value::Bool(is_view))
    });

    // ArrayBuffer[Symbol.species] => ArrayBuffer
    define_species_getter(vm, &ctor, species);

    // get ArrayBuffer.prototype.byteLength (throws on a SharedArrayBuffer)
    let bl_getter = vm.new_native("get byteLength", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, false, "byteLength")?;
        Ok(Value::Number(buffer_byte_length(&o) as f64))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("byteLength"),
        Some(Value::Object(bl_getter)),
        None,
    );

    // get ArrayBuffer.prototype.maxByteLength — the resizable max, or (for a
    // fixed-length buffer) its current byteLength.
    let mbl_getter = vm.new_native("get maxByteLength", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, false, "maxByteLength")?;
        let max = ab_max_byte_length(&o);
        Ok(Value::Number(
            max.unwrap_or_else(|| buffer_byte_length(&o)) as f64
        ))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("maxByteLength"),
        Some(Value::Object(mbl_getter)),
        None,
    );

    // get ArrayBuffer.prototype.resizable
    let rsz_getter = vm.new_native("get resizable", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, false, "resizable")?;
        Ok(Value::Bool(ab_max_byte_length(&o).is_some()))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("resizable"),
        Some(Value::Object(rsz_getter)),
        None,
    );

    // get ArrayBuffer.prototype.detached (throws on a SharedArrayBuffer)
    let det_getter = vm.new_native("get detached", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, false, "detached")?;
        let detached = matches!(o.borrow().internal, Internal::ArrayBuffer(None));
        Ok(Value::Bool(detached))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("detached"),
        Some(Value::Object(det_getter)),
        None,
    );

    // ArrayBuffer.prototype.resize(newByteLength)
    vm.define_method(&proto, "resize", 1, |vm, this, args| {
        let o = ab_receiver(vm, &this, false, "resize")?;
        let max = match ab_max_byte_length(&o) {
            Some(m) => m,
            None => {
                return Err(vm.throw_type("ArrayBuffer.prototype.resize: buffer is not resizable"))
            }
        };
        let new_len = byte_length_arg(vm, &arg(args, 0))?;
        if new_len > max {
            return Err(
                vm.throw_range("ArrayBuffer.prototype.resize: length exceeds maxByteLength")
            );
        }
        let mut b = o.borrow_mut();
        match &mut b.internal {
            Internal::ArrayBuffer(Some(bytes)) => bytes.resize(new_len, 0),
            _ => return Err(vm.throw_type("ArrayBuffer.prototype.resize: buffer is detached")),
        }
        Ok(Value::Undefined)
    });

    // ArrayBuffer.prototype.transfer(newLength?) — copy into a new buffer
    // (preserving resizability) and detach the original.
    vm.define_method(&proto, "transfer", 0, |vm, this, args| {
        ab_transfer(vm, &this, args, true)
    });
    // transferToFixedLength(newLength?) — like transfer but the result is fixed.
    vm.define_method(&proto, "transferToFixedLength", 0, |vm, this, args| {
        ab_transfer(vm, &this, args, false)
    });

    // ArrayBuffer.prototype.slice(begin, end)
    vm.define_method(&proto, "slice", 2, |vm, this, args| {
        let o = ab_receiver(vm, &this, false, "slice")?;
        if matches!(o.borrow().internal, Internal::ArrayBuffer(None)) {
            return Err(vm.throw_type("ArrayBuffer.prototype.slice called on a detached buffer"));
        }
        let len = buffer_byte_length(&o) as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let end = rel_index(vm, &arg(args, 1), len, len)?;
        let new_len = (end - start).max(0) as usize;
        // SpeciesConstructor(O, %ArrayBuffer%), then Construct(ctor, «newLen»),
        // validating the result (spec 25.1.5.3 steps 14–20).
        let default_ctor = vm.get_prop(
            &Value::Object(vm.realm.global.clone()),
            &PropertyKey::str("ArrayBuffer"),
        )?;
        let ctor = ta_species_constructor(vm, &o, &default_ctor)?;
        let new_obj = vm.construct(&ctor, &[Value::Number(new_len as f64)], &ctor)?;
        let new_buf = match &new_obj {
            Value::Object(b) if matches!(b.borrow().internal, Internal::ArrayBuffer(_)) => {
                b.clone()
            }
            _ => {
                return Err(vm.throw_type(
                    "ArrayBuffer.prototype.slice: species did not return an ArrayBuffer",
                ))
            }
        };
        if matches!(new_buf.borrow().internal, Internal::ArrayBuffer(None)) {
            return Err(
                vm.throw_type("ArrayBuffer.prototype.slice: species returned a detached buffer")
            );
        }
        if new_buf.same(&o) {
            return Err(
                vm.throw_type("ArrayBuffer.prototype.slice: species returned the same buffer")
            );
        }
        if (buffer_byte_length(&new_buf) as usize) < new_len {
            return Err(vm.throw_type(
                "ArrayBuffer.prototype.slice: species returned a buffer that is too small",
            ));
        }
        // The species constructor may have detached the source buffer.
        if matches!(o.borrow().internal, Internal::ArrayBuffer(None)) {
            return Err(vm.throw_type("ArrayBuffer.prototype.slice: source buffer was detached"));
        }
        // Copy bytes [start, end) into the new buffer.
        {
            let src = o.borrow();
            if let Internal::ArrayBuffer(Some(src_bytes)) = &src.internal {
                let s = (start as usize).min(src_bytes.len());
                let e = (end.max(0) as usize).min(src_bytes.len());
                if e > s {
                    let slice: Vec<u8> = src_bytes[s..e].to_vec();
                    drop(src);
                    let mut dst = new_buf.borrow_mut();
                    if let Internal::ArrayBuffer(Some(dst_bytes)) = &mut dst.internal {
                        let n = slice.len().min(dst_bytes.len());
                        dst_bytes[..n].copy_from_slice(&slice[..n]);
                    }
                }
            }
        }
        Ok(Value::Object(new_buf))
    });

    // ArrayBuffer.prototype[Symbol.toStringTag] = "ArrayBuffer"
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("ArrayBuffer"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
}

// =========================================================================
// SharedArrayBuffer
// =========================================================================

/// `GetArrayBufferMaxByteLengthOption`: read `options.maxByteLength` (if
/// `options` is an object and the property is not `undefined`), validating it
/// against the buffer's initial `len`. `None` ⇒ the buffer is fixed-length.
fn max_byte_length_option(
    vm: &mut Vm,
    options: &Value,
    len: usize,
) -> Result<Option<usize>, Value> {
    let Value::Object(opts) = options else {
        return Ok(None);
    };
    let mbl = vm.get_prop(
        &Value::Object(opts.clone()),
        &PropertyKey::str("maxByteLength"),
    )?;
    if mbl.is_undefined() {
        return Ok(None);
    }
    let max = byte_length_arg(vm, &mbl)?;
    if len > max {
        return Err(vm.throw_range("buffer length exceeds maxByteLength"));
    }
    Ok(Some(max))
}

fn install_shared_array_buffer(vm: &mut Vm, species: &JsSymbol) {
    let proto = vm.realm.shared_array_buffer_proto.clone();

    let construct = |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
        let len = byte_length_arg(vm, &arg(args, 0))?;
        // Optional `{ maxByteLength }` makes the buffer growable.
        let max = max_byte_length_option(vm, &arg(args, 1), len)?;
        Ok(Value::Object(vm.new_shared_array_buffer(len, max)))
    };
    let ctor = vm.new_native_ctor(
        "SharedArrayBuffer",
        1,
        |vm, _t, _a| Err(vm.throw_type("Constructor SharedArrayBuffer requires 'new'")),
        construct,
    );
    vm.install_ctor("SharedArrayBuffer", &ctor, &proto);

    // SharedArrayBuffer[Symbol.species] => SharedArrayBuffer
    define_species_getter(vm, &ctor, species);

    // get SharedArrayBuffer.prototype.byteLength
    let bl_getter = vm.new_native("get byteLength", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, true, "byteLength")?;
        Ok(Value::Number(buffer_byte_length(&o) as f64))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("byteLength"),
        Some(Value::Object(bl_getter)),
        None,
    );

    // get SharedArrayBuffer.prototype.maxByteLength — the growable max, or (for a
    // fixed-length buffer) its current byteLength.
    let mbl_getter = vm.new_native("get maxByteLength", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, true, "maxByteLength")?;
        let max = ab_max_byte_length(&o);
        Ok(Value::Number(
            max.unwrap_or_else(|| buffer_byte_length(&o)) as f64
        ))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("maxByteLength"),
        Some(Value::Object(mbl_getter)),
        None,
    );

    // get SharedArrayBuffer.prototype.growable
    let grw_getter = vm.new_native("get growable", 0, |vm, this, _a| {
        let o = ab_receiver(vm, &this, true, "growable")?;
        Ok(Value::Bool(ab_max_byte_length(&o).is_some()))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("growable"),
        Some(Value::Object(grw_getter)),
        None,
    );

    // SharedArrayBuffer.prototype.grow(newByteLength) — grow-only (never shrinks).
    vm.define_method(&proto, "grow", 1, |vm, this, args| {
        let o = ab_receiver(vm, &this, true, "grow")?;
        let max = match ab_max_byte_length(&o) {
            Some(m) => m,
            None => {
                return Err(
                    vm.throw_type("SharedArrayBuffer.prototype.grow: buffer is not growable")
                )
            }
        };
        let new_len = byte_length_arg(vm, &arg(args, 0))?;
        if new_len > max {
            return Err(
                vm.throw_range("SharedArrayBuffer.prototype.grow: length exceeds maxByteLength")
            );
        }
        let mut b = o.borrow_mut();
        if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
            if new_len < bytes.len() {
                return Err(vm.throw_range("SharedArrayBuffer.prototype.grow: cannot shrink"));
            }
            bytes.resize(new_len, 0);
        }
        Ok(Value::Undefined)
    });

    // SharedArrayBuffer.prototype.slice(begin, end)
    vm.define_method(&proto, "slice", 2, |vm, this, args| {
        let o = ab_receiver(vm, &this, true, "slice")?;
        let len = buffer_byte_length(&o) as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let end = rel_index(vm, &arg(args, 1), len, len)?;
        let new_len = (end - start).max(0) as usize;
        // SpeciesConstructor(O, %SharedArrayBuffer%), then Construct(ctor, «newLen»).
        let default_ctor = vm.get_prop(
            &Value::Object(vm.realm.global.clone()),
            &PropertyKey::str("SharedArrayBuffer"),
        )?;
        let ctor = ta_species_constructor(vm, &o, &default_ctor)?;
        let new_obj = vm.construct(&ctor, &[Value::Number(new_len as f64)], &ctor)?;
        let new_buf =
            match &new_obj {
                Value::Object(b)
                    if matches!(b.borrow().internal, Internal::ArrayBuffer(_))
                        && vm.is_shared_buffer(b) =>
                {
                    b.clone()
                }
                _ => return Err(vm.throw_type(
                    "SharedArrayBuffer.prototype.slice: species did not return a SharedArrayBuffer",
                )),
            };
        if new_buf.same(&o) {
            return Err(vm.throw_type(
                "SharedArrayBuffer.prototype.slice: species returned the same buffer",
            ));
        }
        if buffer_byte_length(&new_buf) < new_len {
            return Err(vm.throw_type(
                "SharedArrayBuffer.prototype.slice: species returned a buffer that is too small",
            ));
        }
        // Copy bytes [start, end) into the new buffer.
        {
            let src = o.borrow();
            if let Internal::ArrayBuffer(Some(src_bytes)) = &src.internal {
                let s = (start as usize).min(src_bytes.len());
                let e = (end.max(0) as usize).min(src_bytes.len());
                if e > s {
                    let slice: Vec<u8> = src_bytes[s..e].to_vec();
                    drop(src);
                    let mut dst = new_buf.borrow_mut();
                    if let Internal::ArrayBuffer(Some(dst_bytes)) = &mut dst.internal {
                        let n = slice.len().min(dst_bytes.len());
                        dst_bytes[..n].copy_from_slice(&slice[..n]);
                    }
                }
            }
        }
        Ok(Value::Object(new_buf))
    });

    // SharedArrayBuffer.prototype[Symbol.toStringTag] = "SharedArrayBuffer"
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("SharedArrayBuffer"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
}

/// ToIndex-ish coercion for a byte length / count, with the dense-allocation cap.
fn byte_length_arg(vm: &mut Vm, v: &Value) -> Result<usize, Value> {
    let n = to_integer_or_infinity(vm, v)?;
    if n < 0.0 || n.is_infinite() {
        return Err(vm.throw_range("Invalid array buffer length"));
    }
    let len = n as usize;
    if (len as f64) != n {
        return Err(vm.throw_range("Invalid array buffer length"));
    }
    if len > crate::value::MAX_DENSE_ARRAY {
        return Err(vm.throw_range("ArrayBuffer allocation exceeds engine limit"));
    }
    Ok(len)
}

// =========================================================================
// %TypedArray% base prototype + abstract constructor
// =========================================================================

/// Snapshot a typed array's field triple (buffer, byte_offset, length, kind).
/// `length` is the *effective* length (live for length-tracking views).
fn ta_fields(o: &JsObject) -> Option<(JsObject, usize, usize, TAKind)> {
    match &o.borrow().internal {
        Internal::TypedArray(t) => Some((
            t.buffer.clone(),
            t.byte_offset,
            crate::typed_array::ta_eff_length(t),
            t.kind,
        )),
        _ => None,
    }
}

/// Require a typed-array `this`, returning its object handle or a TypeError.
/// This is `ValidateTypedArray`: besides the brand check it throws when the
/// backing buffer is detached (the prototype getters, which tolerate detachment,
/// use their own accessor and never call this).
fn ta_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::TypedArray(_)) => {
            if ta_out_of_bounds(o) {
                return Err(
                    vm.throw_type("Cannot operate on a detached or out-of-bounds TypedArray")
                );
            }
            Ok(o.clone())
        }
        _ => Err(vm.throw_type("Method %TypedArray%.prototype called on incompatible receiver")),
    }
}

/// Materialize the elements of a typed array `this` into a `Vec<Value>` (each a
/// Number or, for BigInt kinds, a BigInt) for the generic helpers.
fn ta_values_v(vm: &Vm, o: &JsObject) -> Vec<Value> {
    let len = vm.ta_length(o).unwrap_or(0);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(vm.ta_get(o, i));
    }
    out
}

/// True if the typed array's backing `ArrayBuffer` is detached.
fn ta_detached(o: &JsObject) -> bool {
    match &o.borrow().internal {
        Internal::TypedArray(t) => {
            matches!(t.buffer.borrow().internal, Internal::ArrayBuffer(None))
        }
        _ => true,
    }
}

/// `IsTypedArrayOutOfBounds`: true if the view's backing buffer is detached, or
/// (after a resizable-buffer shrink) the view no longer fits. A fixed-length
/// view is out of bounds when `byteOffset + byteLength > bufferLength`; a
/// length-tracking view when `byteOffset > bufferLength`.
fn ta_out_of_bounds(o: &JsObject) -> bool {
    match &o.borrow().internal {
        Internal::TypedArray(t) => {
            let buf_len = match &t.buffer.borrow().internal {
                Internal::ArrayBuffer(Some(b)) => b.len(),
                _ => return true, // detached
            };
            if t.length_tracking {
                t.byte_offset > buf_len
            } else {
                t.byte_offset + t.length * t.kind.bytes() > buf_len
            }
        }
        _ => true,
    }
}

/// Post-coercion revalidation: argument coercion can detach or shrink the
/// backing buffer mid-method. A detached/out-of-bounds view is a TypeError;
/// otherwise the CURRENT element length is returned for re-clamping.
fn ta_revalidate(vm: &Vm, o: &JsObject) -> Result<usize, Value> {
    if ta_out_of_bounds(o) {
        return Err(vm.throw_type("TypedArray is detached or out of bounds"));
    }
    Ok(vm.ta_length(o).unwrap_or(0))
}

/// `SpeciesConstructor(O, defaultConstructor)` (spec 7.3.23) for typed arrays.
fn ta_species_constructor(vm: &mut Vm, o: &JsObject, default: &Value) -> Result<Value, Value> {
    let c = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("constructor"))?;
    if c.is_undefined() {
        return Ok(default.clone());
    }
    if !matches!(c, Value::Object(_)) {
        return Err(vm.throw_type("constructor is not an object"));
    }
    let sym = vm.realm.symbol_species.clone();
    let s = vm.get_prop(&c, &PropertyKey::Sym(sym))?;
    if s.is_nullish() {
        return Ok(default.clone());
    }
    if vm.is_constructor(&s) {
        Ok(s)
    } else {
        Err(vm.throw_type("Symbol.species is not a constructor"))
    }
}

/// `TypedArrayCreate(constructor, argumentList)`: construct, then
/// `ValidateTypedArray` and (for a single numeric length arg) a minimum-length
/// check.
fn ta_create(vm: &mut Vm, c: &Value, args: &[Value]) -> Result<JsObject, Value> {
    let result = vm.construct(c, args, c)?;
    let ro = match result {
        Value::Object(o) if vm.ta_kind(&o).is_some() => o,
        _ => return Err(vm.throw_type("derived constructor did not return a TypedArray")),
    };
    if ta_detached(&ro) {
        return Err(vm.throw_type("derived TypedArray has a detached buffer"));
    }
    if args.len() == 1 {
        if let Value::Number(n) = &args[0] {
            if (vm.ta_length(&ro).unwrap_or(0) as f64) < *n {
                return Err(vm.throw_type("derived TypedArray is smaller than required"));
            }
        }
    }
    Ok(ro)
}

/// `TypedArraySpeciesCreate(exemplar, argumentList)` (spec 23.2.4.1): create the
/// result of map/filter/slice/subarray via the exemplar's species constructor.
fn ta_species_create(vm: &mut Vm, exemplar: &JsObject, args: &[Value]) -> Result<JsObject, Value> {
    let kind = match vm.ta_kind(exemplar) {
        Some(k) => k,
        None => return Err(vm.throw_type("not a TypedArray")),
    };
    // The default is the INTRINSIC per-kind constructor (stashed at install
    // time), not whatever `prototype.constructor` currently holds.
    let base = vm.realm.typed_array_proto.clone();
    let ckey = PropertyKey::str(format!("__ctor_{}", kind.name()));
    let default_ctor = match base.borrow().props.get(&ckey).and_then(|p| p.value()) {
        Some(v) => v.clone(),
        None => Value::Undefined,
    };
    let c = ta_species_constructor(vm, exemplar, &default_ctor)?;
    ta_create(vm, &c, args)
}

/// Create a fresh typed array of the same kind as `o`, of `len` elements, backed
/// by a newly-allocated ArrayBuffer.
fn new_same_kind(vm: &mut Vm, kind: TAKind, len: usize) -> Result<JsObject, Value> {
    let bytes = len
        .checked_mul(kind.bytes())
        .ok_or_else(|| vm.throw_range("Invalid typed array length"))?;
    if len > crate::value::MAX_DENSE_ARRAY {
        return Err(vm.throw_range("TypedArray allocation exceeds engine limit"));
    }
    let buf = vm.new_array_buffer(bytes);
    let proto = per_kind_proto(vm, kind);
    Ok(vm.new_typed_array(kind, buf, 0, len, proto))
}

fn install_typed_array_base(vm: &mut Vm, species: &JsSymbol) -> JsObject {
    let proto = vm.realm.typed_array_proto.clone();

    // The abstract %TypedArray% constructor is not directly callable/constructable
    // by user code (per spec it throws), but it carries the shared statics
    // `of`/`from` and the species accessor; the concrete ctors inherit from it.
    let ta_ctor = vm.new_native_ctor(
        "TypedArray",
        0,
        |vm, _t, _a| Err(vm.throw_type("Abstract class TypedArray not directly callable")),
        |vm, _t, _a| Err(vm.throw_type("Abstract class TypedArray not directly constructable")),
    );
    // Wire %TypedArray%.prototype <-> %TypedArray% without exposing a global.
    ta_ctor.borrow_mut().props.insert(
        PropertyKey::str("prototype"),
        Property {
            kind: PropertyKind::Data {
                value: Value::Object(proto.clone()),
                writable: false,
            },
            enumerable: false,
            configurable: false,
        },
    );
    proto.borrow_mut().props.insert(
        PropertyKey::str("constructor"),
        Property::builtin(Value::Object(ta_ctor.clone())),
    );
    define_species_getter(vm, &ta_ctor, species);

    // Shared statics %TypedArray%.of / %TypedArray%.from. Both use the `this`
    // value as the constructor `C` (requiring IsConstructor) and build the
    // result via TypedArrayCreate, so a subclass `this` produces that subclass.
    vm.define_method(&ta_ctor, "of", 0, |vm, this, args| {
        if !vm.is_constructor(&this) {
            return Err(vm.throw_type("%TypedArray%.of requires a constructor this value"));
        }
        let result = ta_create(vm, &this, &[Value::Number(args.len() as f64)])?;
        for (i, a) in args.iter().enumerate() {
            vm.ta_write(&result, i, a)?;
        }
        Ok(Value::Object(result))
    });
    vm.define_method(&ta_ctor, "from", 1, |vm, this, args| {
        if !vm.is_constructor(&this) {
            return Err(vm.throw_type("%TypedArray%.from requires a constructor this value"));
        }
        let src = arg(args, 0);
        let map_fn = arg(args, 1);
        let has_map = !map_fn.is_undefined();
        if has_map && !vm.is_callable(&map_fn) {
            return Err(vm.throw_type("%TypedArray%.from: mapFn is not a function"));
        }
        let this_arg = arg(args, 2);
        let items = if is_iterable(vm, &src)? {
            vm.iterate_to_vec(&src)?
        } else {
            let o = vm.to_object(&src)?;
            let len_v = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("length"))?;
            let len = vm.to_length(&len_v)?;
            if len > crate::value::MAX_DENSE_ARRAY {
                return Err(vm.throw_range("TypedArray allocation exceeds engine limit"));
            }
            let mut v = Vec::with_capacity(len.min(1 << 16));
            for i in 0..len {
                v.push(vm.get_prop(
                    &Value::Object(o.clone()),
                    &PropertyKey::from_index(i as u32),
                )?);
            }
            v
        };
        let result = ta_create(vm, &this, &[Value::Number(items.len() as f64)])?;
        for (i, item) in items.into_iter().enumerate() {
            let val = if has_map {
                vm.call(
                    map_fn.clone(),
                    this_arg.clone(),
                    &[item, Value::Number(i as f64)],
                )?
            } else {
                item
            };
            vm.ta_write(&result, i, &val)?;
        }
        Ok(Value::Object(result))
    });

    install_ta_accessors(vm, &proto);
    install_ta_methods(vm, &proto);

    // [Symbol.toStringTag] getter returns the per-kind name (or undefined).
    let tag = vm.realm.symbol_to_string_tag.clone();
    let tag_getter = vm.new_native("get [Symbol.toStringTag]", 0, |_vm, this, _a| match &this {
        Value::Object(o) => match o.borrow().internal {
            Internal::TypedArray(ref t) => Ok(Value::str(t.kind.name())),
            _ => Ok(Value::Undefined),
        },
        _ => Ok(Value::Undefined),
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::Sym(tag),
        Some(Value::Object(tag_getter)),
        None,
    );

    ta_ctor
}

fn install_ta_accessors(vm: &mut Vm, proto: &JsObject) {
    fn getter(vm: &mut Vm, name: &str, f: fn(&JsObject) -> Value) -> Value {
        Value::Object(vm.new_native(name, 0, move |vm, this, _a| match &this {
            Value::Object(o) if matches!(o.borrow().internal, Internal::TypedArray(_)) => Ok(f(o)),
            _ => Err(vm.throw_type("TypedArray prototype getter called on incompatible receiver")),
        }))
    }

    // length/byteLength/byteOffset report 0 for a detached or out-of-bounds view.
    let g = getter(vm, "get length", |o| {
        if ta_out_of_bounds(o) {
            return Value::Number(0.0);
        }
        Value::Number(ta_fields(o).map(|(_, _, l, _)| l).unwrap_or(0) as f64)
    });
    // Pin the canonical getter so the loop-kernel `LoadLen` entry guard can
    // identity-check that a typed-array base still resolves `.length` to it.
    if let Value::Object(gobj) = &g {
        vm.realm.ta_length_getter = Some(gobj.clone());
    }
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("length"),
        Some(g),
        None,
    );

    let g = getter(vm, "get byteLength", |o| {
        if ta_out_of_bounds(o) {
            return Value::Number(0.0);
        }
        Value::Number(ta_fields(o).map(|(_, _, l, k)| l * k.bytes()).unwrap_or(0) as f64)
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("byteLength"),
        Some(g),
        None,
    );

    let g = getter(vm, "get byteOffset", |o| {
        if ta_out_of_bounds(o) {
            return Value::Number(0.0);
        }
        Value::Number(ta_fields(o).map(|(_, off, _, _)| off).unwrap_or(0) as f64)
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("byteOffset"),
        Some(g),
        None,
    );

    let g = getter(vm, "get buffer", |o| {
        ta_fields(o)
            .map(|(buf, _, _, _)| Value::Object(buf))
            .unwrap_or(Value::Undefined)
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("buffer"),
        Some(g),
        None,
    );
}

fn install_ta_methods(vm: &mut Vm, proto: &JsObject) {
    // at(index)
    vm.define_method(proto, "at", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0) as f64;
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        let k = if rel >= 0.0 { rel } else { len + rel };
        if k < 0.0 || k >= len {
            return Ok(Value::Undefined);
        }
        Ok(vm.ta_get(&o, k as usize))
    });

    // fill(value, start?, end?)
    vm.define_method(proto, "fill", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0) as isize;
        // Coerce the fill value exactly once (per spec), to the kind's primitive.
        let kind = vm.ta_kind(&o).unwrap();
        let value = if kind.is_bigint() {
            Value::bigint(vm.to_bigint(&arg(args, 0))?)
        } else {
            Value::Number(vm.to_number(&arg(args, 0))?)
        };
        let start = rel_index(vm, &arg(args, 1), len, 0)?;
        let end = rel_index(vm, &arg(args, 2), len, len)?;
        // The value/start/end coercions can detach or shrink the buffer:
        // revalidate (TypeError) and re-clamp to the CURRENT length.
        let cur = ta_revalidate(vm, &o)? as isize;
        let mut i = start.min(cur);
        let end = end.min(cur);
        while i < end {
            vm.ta_write(&o, i as usize, &value)?;
            i += 1;
        }
        Ok(this)
    });

    // set(source, offset?)
    vm.define_method(proto, "set", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        // ToIntegerOrInfinity(offset); a negative or non-finite offset is a
        // RangeError (a `+Infinity` offset must not be allowed to saturate into
        // a `usize` and silently wrap the bounds check below).
        let offset_f = to_integer_or_infinity(vm, &arg(args, 1))?;
        if offset_f < 0.0 || !offset_f.is_finite() {
            return Err(vm.throw_range("Start offset is out of bounds"));
        }
        // The offset coercion can detach/shrink the target's buffer: a
        // detached or out-of-bounds target is a TypeError, and the length is
        // read AFTER the coercion.
        let target_len = ta_revalidate(vm, &o)?;
        if offset_f > target_len as f64 {
            return Err(vm.throw_range("Start offset is out of bounds"));
        }
        let offset = offset_f as usize;
        let src = arg(args, 0);
        // A detached or out-of-bounds TYPED-ARRAY source is a TypeError too.
        if let Value::Object(so) = &src {
            if matches!(so.borrow().internal, Internal::TypedArray(_)) && ta_out_of_bounds(so) {
                return Err(vm.throw_type("source TypedArray is detached or out of bounds"));
            }
        }
        // A typed-array source is fully snapshotted before any target element
        // is written (overlapping buffers behave correctly); an array-like
        // source is read and written INTERLEAVED per spec
        // (SetTypedArrayFromArrayLike): Get(src, k) then write, one at a time,
        // so an abrupt Get stops mid-way with earlier writes visible.
        let is_ta_src =
            matches!(&src, Value::Object(so) if matches!(so.borrow().internal, Internal::TypedArray(_)));
        if is_ta_src {
            let so = match &src {
                Value::Object(so) => so.clone(),
                _ => unreachable!(),
            };
            let src_vals = ta_values_v(vm, &so);
            if offset + src_vals.len() > target_len {
                return Err(vm.throw_range("Source is too large"));
            }
            for (i, val) in src_vals.into_iter().enumerate() {
                vm.ta_write(&o, offset + i, &val)?;
            }
        } else {
            let so = vm.to_object(&src)?;
            let sov = Value::Object(so);
            let len_v = vm.get_prop(&sov, &PropertyKey::str("length"))?;
            let len = vm.to_length(&len_v)?;
            // The bounds check precedes any source read.
            if offset + len > target_len {
                return Err(vm.throw_range("Source is too large"));
            }
            for i in 0..len {
                let val = vm.get_prop(&sov, &PropertyKey::from_index(i as u32))?;
                vm.ta_write(&o, offset + i, &val)?;
            }
        }
        Ok(Value::Undefined)
    });

    // subarray(begin?, end?) — a new view sharing the same buffer.
    vm.define_method(proto, "subarray", 2, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let (buffer, byte_offset, length, kind) =
            ta_fields(&o).ok_or_else(|| vm.throw_type("subarray on non-typed-array"))?;
        let len = length as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let end = rel_index(vm, &arg(args, 1), len, len)?;
        let new_len = (end - start).max(0) as usize;
        let new_byte_offset = byte_offset + (start as usize) * kind.bytes();
        // subarray builds the view via TypedArraySpeciesCreate(O, [buffer,
        // byteOffset, length]).
        let result = ta_species_create(
            vm,
            &o,
            &[
                Value::Object(buffer),
                Value::Number(new_byte_offset as f64),
                Value::Number(new_len as f64),
            ],
        )?;
        Ok(Value::Object(result))
    });

    // slice(begin?, end?) — a new typed array copy of the same kind.
    vm.define_method(proto, "slice", 2, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let kind = vm.ta_kind(&o).unwrap();
        let len = vm.ta_length(&o).unwrap_or(0) as isize;
        let start = rel_index(vm, &arg(args, 0), len, 0)?;
        let end = rel_index(vm, &arg(args, 1), len, len)?;
        let count = (end - start).max(0) as usize;
        let _ = kind;
        let result = ta_species_create(vm, &o, &[Value::Number(count as f64)])?;
        if count > 0 {
            // The start/end coercions (or the species constructor) can detach
            // or shrink the source: revalidate (TypeError) and re-clamp.
            let cur = ta_revalidate(vm, &o)?;
            let upto = count.min(cur.saturating_sub(start.max(0) as usize));
            for i in 0..upto {
                let v = vm.ta_get(&o, (start as usize) + i);
                vm.ta_write(&result, i, &v)?;
            }
        }
        Ok(Value::Object(result))
    });

    // copyWithin(target, start, end?)
    vm.define_method(proto, "copyWithin", 2, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0) as isize;
        let target = rel_index(vm, &arg(args, 0), len, 0)?;
        let start = rel_index(vm, &arg(args, 1), len, 0)?;
        let end = rel_index(vm, &arg(args, 2), len, len)?;
        // The index coercions can detach or shrink the buffer: revalidate
        // (TypeError) and re-clamp everything to the CURRENT length.
        let len = (ta_revalidate(vm, &o)? as isize).min(len);
        let target = target.min(len);
        let start = start.min(len);
        let end = end.min(len);
        let count = (end - start).min(len - target).max(0);
        if count > 0 {
            // Snapshot the source range, then write — handles overlap correctly.
            let src: Vec<Value> = (0..count)
                .map(|i| vm.ta_get(&o, (start + i) as usize))
                .collect();
            for (i, v) in src.into_iter().enumerate() {
                vm.ta_write(&o, (target + i as isize) as usize, &v)?;
            }
        }
        Ok(this)
    });

    // map(cb, thisArg?)
    vm.define_method(proto, "map", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let kind = vm.ta_kind(&o).unwrap();
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let len = vm.ta_length(&o).unwrap_or(0);
        let _ = kind;
        let result = ta_species_create(vm, &o, &[Value::Number(len as f64)])?;
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            let mapped = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
            vm.ta_write(&result, i, &mapped)?;
        }
        Ok(Value::Object(result))
    });

    // filter(cb, thisArg?)
    vm.define_method(proto, "filter", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let kind = vm.ta_kind(&o).unwrap();
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let len = vm.ta_length(&o).unwrap_or(0);
        let mut kept: Vec<Value> = Vec::new();
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            let keep = vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v.clone(), Value::Number(i as f64), this.clone()],
            )?;
            if vm.to_boolean(&keep) {
                kept.push(v);
            }
        }
        let _ = kind;
        let result = ta_species_create(vm, &o, &[Value::Number(kept.len() as f64)])?;
        for (i, v) in kept.into_iter().enumerate() {
            vm.ta_write(&result, i, &v)?;
        }
        Ok(Value::Object(result))
    });

    // forEach(cb, thisArg?)
    vm.define_method(proto, "forEach", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let this_arg = arg(args, 1);
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            vm.call(
                cb.clone(),
                this_arg.clone(),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
        }
        Ok(Value::Undefined)
    });

    // reduce(cb, init?)
    vm.define_method(proto, "reduce", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        let mut acc;
        let mut start = 0;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            if len == 0 {
                return Err(vm.throw_type("Reduce of empty array with no initial value"));
            }
            acc = vm.ta_get(&o, 0);
            start = 1;
        }
        for i in start..len {
            let v = vm.ta_get(&o, i);
            acc = vm.call(
                cb.clone(),
                Value::Undefined,
                &[acc, v, Value::Number(i as f64), this.clone()],
            )?;
        }
        Ok(acc)
    });

    // reduceRight(cb, init?)
    vm.define_method(proto, "reduceRight", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        let mut acc;
        let mut start: isize;
        if args.len() >= 2 {
            acc = arg(args, 1);
            start = len as isize - 1;
        } else {
            if len == 0 {
                return Err(vm.throw_type("Reduce of empty array with no initial value"));
            }
            acc = vm.ta_get(&o, len - 1);
            start = len as isize - 2;
        }
        while start >= 0 {
            let i = start as usize;
            let v = vm.ta_get(&o, i);
            acc = vm.call(
                cb.clone(),
                Value::Undefined,
                &[acc, v, Value::Number(i as f64), this.clone()],
            )?;
            start -= 1;
        }
        Ok(acc)
    });

    // some(cb, thisArg?)
    vm.define_method(proto, "some", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            let r = vm.call(
                cb.clone(),
                arg(args, 1),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });

    // every(cb, thisArg?)
    vm.define_method(proto, "every", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("callback is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            let r = vm.call(
                cb.clone(),
                arg(args, 1),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
            if !vm.to_boolean(&r) {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });

    // find(cb, thisArg?)
    vm.define_method(proto, "find", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("predicate is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            let r = vm.call(
                cb.clone(),
                arg(args, 1),
                &[v.clone(), Value::Number(i as f64), this.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(v);
            }
        }
        Ok(Value::Undefined)
    });

    // findIndex(cb, thisArg?)
    vm.define_method(proto, "findIndex", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("predicate is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            let r = vm.call(
                cb.clone(),
                arg(args, 1),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Number(i as f64));
            }
        }
        Ok(Value::Number(-1.0))
    });

    // findLast(cb, thisArg?)
    vm.define_method(proto, "findLast", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("predicate is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in (0..len).rev() {
            let v = vm.ta_get(&o, i);
            let r = vm.call(
                cb.clone(),
                arg(args, 1),
                &[v.clone(), Value::Number(i as f64), this.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(v);
            }
        }
        Ok(Value::Undefined)
    });

    // findLastIndex(cb, thisArg?)
    vm.define_method(proto, "findLastIndex", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("predicate is not a function"));
        }
        let len = vm.ta_length(&o).unwrap_or(0);
        for i in (0..len).rev() {
            let v = vm.ta_get(&o, i);
            let r = vm.call(
                cb.clone(),
                arg(args, 1),
                &[v, Value::Number(i as f64), this.clone()],
            )?;
            if vm.to_boolean(&r) {
                return Ok(Value::Number(i as f64));
            }
        }
        Ok(Value::Number(-1.0))
    });

    // indexOf(searchElement, fromIndex?)
    vm.define_method(proto, "indexOf", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0);
        if len == 0 {
            return Ok(Value::Number(-1.0));
        }
        // Per spec the search element is compared with IsStrictlyEqual; it is not
        // coerced (a mismatched type — including a Number against a BigInt array —
        // simply never matches).
        let target = arg(args, 0);
        let from = to_integer_or_infinity(vm, &arg(args, 1))?;
        let start = if from == f64::INFINITY {
            return Ok(Value::Number(-1.0));
        } else if from == f64::NEG_INFINITY {
            0
        } else if from >= 0.0 {
            from as usize
        } else {
            ((len as f64 + from).max(0.0)) as usize
        };
        for i in start..len {
            // HasProperty per spec: an index that went out of bounds (shrunk
            // resizable buffer) is absent — skipped, never matched.
            if !vm.ta_valid_index(&o, i as f64) {
                continue;
            }
            let el = vm.ta_get(&o, i);
            if vm.strict_equals(&el, &target) {
                return Ok(Value::Number(i as f64));
            }
        }
        Ok(Value::Number(-1.0))
    });

    // lastIndexOf(searchElement, fromIndex?)
    vm.define_method(proto, "lastIndexOf", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0);
        if len == 0 {
            return Ok(Value::Number(-1.0));
        }
        let target = arg(args, 0);
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
        let mut i = start;
        while i >= 0 {
            // HasProperty per spec: out-of-bounds indices are absent (skipped).
            if vm.ta_valid_index(&o, i as f64) {
                let el = vm.ta_get(&o, i as usize);
                if vm.strict_equals(&el, &target) {
                    return Ok(Value::Number(i as f64));
                }
            }
            i -= 1;
        }
        Ok(Value::Number(-1.0))
    });

    // includes(searchElement, fromIndex?)
    vm.define_method(proto, "includes", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0);
        if len == 0 {
            return Ok(Value::Bool(false));
        }
        let target = arg(args, 0);
        let from = to_integer_or_infinity(vm, &arg(args, 1))?;
        let start = if from == f64::INFINITY {
            return Ok(Value::Bool(false));
        } else if from == f64::NEG_INFINITY {
            0
        } else if from >= 0.0 {
            from as usize
        } else {
            ((len as f64 + from).max(0.0)) as usize
        };
        for i in start..len {
            let el = vm.ta_get(&o, i);
            // SameValueZero: NaN matches NaN (only relevant for Number kinds).
            let hit = match (&el, &target) {
                (Value::Number(n), Value::Number(t)) => n == t || (n.is_nan() && t.is_nan()),
                _ => vm.strict_equals(&el, &target),
            };
            if hit {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });

    // join(separator?)
    vm.define_method(proto, "join", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        // The length is read BEFORE the separator coerces (whose side effects
        // may resize/detach the buffer; the part count must not change), then
        // each element is read live — an index that went out of bounds reads
        // as undefined and contributes the empty string.
        let len = vm.ta_length(&o).unwrap_or(0);
        let sep = {
            let s = arg(args, 0);
            if s.is_undefined() {
                ",".to_string()
            } else {
                vm.to_js_string(&s)?.as_str().to_string()
            }
        };
        let mut parts = Vec::with_capacity(len);
        for i in 0..len {
            let v = vm.ta_get(&o, i);
            if v.is_undefined() {
                parts.push(String::new());
            } else {
                parts.push(vm.to_string_lossy(&v));
            }
        }
        Ok(Value::str(parts.join(&sep)))
    });

    // reverse() — in place.
    vm.define_method(proto, "reverse", 0, |vm, this, _args| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0);
        let mut i = 0;
        let mut j = len.saturating_sub(1);
        while i < j {
            let a = vm.ta_get(&o, i);
            let b = vm.ta_get(&o, j);
            vm.ta_write(&o, i, &b)?;
            vm.ta_write(&o, j, &a)?;
            i += 1;
            j -= 1;
        }
        Ok(this)
    });

    // toReversed() — copy.
    vm.define_method(proto, "toReversed", 0, |vm, this, _args| {
        let o = ta_this(vm, &this)?;
        let kind = vm.ta_kind(&o).unwrap();
        let len = vm.ta_length(&o).unwrap_or(0);
        let result = new_same_kind(vm, kind, len)?;
        for i in 0..len {
            let v = vm.ta_get(&o, len - 1 - i);
            vm.ta_write(&result, i, &v)?;
        }
        Ok(Value::Object(result))
    });

    // with(index, value) — copy with one element replaced (ES2023).
    vm.define_method(proto, "with", 2, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let kind = vm.ta_kind(&o).unwrap();
        let len = vm.ta_length(&o).unwrap_or(0);
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        let actual = if rel >= 0.0 { rel } else { len as f64 + rel };
        // ToNumber/ToBigInt the value *before* the range check (spec ordering), so
        // a throwing valueOf is observed and a number/bigint mismatch is a TypeError.
        let value = vm.to_numeric(&arg(args, 1))?;
        let is_bigint = matches!(kind, TAKind::I64 | TAKind::U64);
        if is_bigint != matches!(value, Value::BigInt(_)) {
            return Err(vm.throw_type("Cannot mix BigInt and other types"));
        }
        if actual < 0.0 || actual >= len as f64 {
            return Err(vm.throw_range("Invalid typed array index"));
        }
        let actual = actual as usize;
        let result = new_same_kind(vm, kind, len)?;
        for i in 0..len {
            if i == actual {
                vm.ta_write(&result, i, &value)?;
            } else {
                let v = vm.ta_get(&o, i);
                vm.ta_write(&result, i, &v)?;
            }
        }
        Ok(Value::Object(result))
    });

    // sort(comparator?) — in place, numeric default.
    vm.define_method(proto, "sort", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let cmp = arg(args, 0);
        if !cmp.is_undefined() && !vm.is_callable(&cmp) {
            return Err(vm.throw_type("comparator is not a function"));
        }
        let has_cmp = vm.is_callable(&cmp);
        let mut vals = ta_values_v(vm, &o);
        ta_sort(vm, &mut vals, &cmp, has_cmp)?;
        for (i, v) in vals.into_iter().enumerate() {
            vm.ta_write(&o, i, &v)?;
        }
        Ok(this)
    });

    // toSorted(comparator?) — copy.
    vm.define_method(proto, "toSorted", 1, |vm, this, args| {
        let o = ta_this(vm, &this)?;
        let kind = vm.ta_kind(&o).unwrap();
        let cmp = arg(args, 0);
        if !cmp.is_undefined() && !vm.is_callable(&cmp) {
            return Err(vm.throw_type("comparator is not a function"));
        }
        let has_cmp = vm.is_callable(&cmp);
        let mut vals = ta_values_v(vm, &o);
        ta_sort(vm, &mut vals, &cmp, has_cmp)?;
        let result = new_same_kind(vm, kind, vals.len())?;
        for (i, v) in vals.into_iter().enumerate() {
            vm.ta_write(&result, i, &v)?;
        }
        Ok(Value::Object(result))
    });

    // keys() / values() / entries(): a live iterator over the typed array itself
    // (the array-backed iterator reads `array[i]` element-by-element each step, so
    // mutations during iteration are observed — spec CreateArrayIterator).
    vm.define_method(proto, "keys", 0, |vm, this, _a| {
        let o = ta_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.array_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::ArrayKeys,
        ))
    });
    vm.define_method(proto, "values", 0, |vm, this, _a| {
        let o = ta_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.array_iterator_proto.clone(),
            Some(o),
            None,
            IterKind::ArrayValues,
        ))
    });
    vm.define_method(proto, "entries", 0, |vm, this, _a| {
        let o = ta_this(vm, &this)?;
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

    // %TypedArray%.prototype.toString is the SAME function object as
    // %Array%.prototype.toString (spec 23.2.3.32). Array installs first, so
    // copy its function; fall back to a local join if it is somehow absent.
    let array_to_string = vm
        .realm
        .array_proto
        .borrow()
        .props
        .get(&PropertyKey::str("toString"))
        .and_then(|p| p.value().cloned());
    match array_to_string {
        Some(f) => {
            proto
                .borrow_mut()
                .props
                .insert(PropertyKey::str("toString"), Property::builtin(f));
        }
        None => {
            vm.define_method(proto, "toString", 0, |vm, this, _a| {
                let o = ta_this(vm, &this)?;
                let len = vm.ta_length(&o).unwrap_or(0);
                let mut parts = Vec::with_capacity(len);
                for i in 0..len {
                    let v = vm.ta_get(&o, i);
                    parts.push(vm.to_string_lossy(&v));
                }
                Ok(Value::str(parts.join(",")))
            });
        }
    }
    vm.define_method(proto, "toLocaleString", 0, |vm, this, _a| {
        let o = ta_this(vm, &this)?;
        let len = vm.ta_length(&o).unwrap_or(0);
        let mut out = String::new();
        for i in 0..len {
            if i > 0 {
                out.push(',');
            }
            // R = ToString(? Invoke(element, "toLocaleString")) — observable per
            // element, with abrupt completions propagated.
            let v = vm.ta_get(&o, i);
            let f = vm.get_prop(&v, &PropertyKey::str("toLocaleString"))?;
            let r = vm.call(f, v, &[])?;
            let s = vm.to_js_string(&r)?;
            out.push_str(s.as_str());
        }
        Ok(Value::str(out))
    });
}

/// Snapshot a typed array's elements into a fresh dense JS array (used to back
/// the keys/values/entries iterators).
fn ta_snapshot_array(vm: &mut Vm, o: &JsObject) -> JsObject {
    let len = vm.ta_length(o).unwrap_or(0);
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(vm.ta_get(o, i));
    }
    vm.new_array(elems)
}

/// In-place sort of typed-array element values. Elements are all one kind
/// (Number or BigInt). Default compare is ascending numeric (NaN sorts to the
/// end); an optional comparator is honored. Stable merge sort.
fn ta_sort(vm: &mut Vm, items: &mut Vec<Value>, cmp: &Value, has_cmp: bool) -> Result<(), Value> {
    // The comparator's function kernel, prepared ONCE for the whole sort
    // (see `Vm::prepare_kernel_callback`).
    let mut prep = if has_cmp {
        vm.prepare_kernel_callback(cmp)
    } else {
        None
    };
    // All-Number specialization (every non-BigInt element kind): raw-`f64`
    // merge sort with a primed comparator — the same split/merge structure
    // as `ta_sort_range`, so results match the generic path exactly. See
    // `Array.prototype.sort`'s `merge_sort` for the soundness argument.
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
                super::array::merge_sort_range_f64(vm, &mut nums, &mut aux, 0, n, p, regs_ab)?;
                for (slot, n) in items.iter_mut().zip(nums) {
                    *slot = Value::Number(n);
                }
                return Ok(());
            }
        }
    }
    ta_sort_range(vm, items, cmp, has_cmp, &mut prep)
}

fn ta_sort_range(
    vm: &mut Vm,
    items: &mut Vec<Value>,
    cmp: &Value,
    has_cmp: bool,
    prep: &mut Option<crate::exec::PreparedKernel>,
) -> Result<(), Value> {
    let n = items.len();
    if n <= 1 {
        return Ok(());
    }
    let mid = n / 2;
    let mut left = items[..mid].to_vec();
    let mut right = items[mid..].to_vec();
    ta_sort_range(vm, &mut left, cmp, has_cmp, prep)?;
    ta_sort_range(vm, &mut right, cmp, has_cmp, prep)?;
    let mut i = 0;
    let mut j = 0;
    let mut k = 0;
    while i < left.len() && j < right.len() {
        let order = ta_compare(vm, &left[i], &right[j], cmp, has_cmp, prep)?;
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

fn ta_compare(
    vm: &mut Vm,
    a: &Value,
    b: &Value,
    cmp: &Value,
    has_cmp: bool,
    prep: &mut Option<crate::exec::PreparedKernel>,
) -> Result<i32, Value> {
    if has_cmp {
        // Prepared-kernel comparator (as in Array.prototype.sort's
        // `compare_values`): unboxed registers, result a Number/Bool by
        // construction; a per-call guard miss falls through generically.
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
        let r = vm.call(cmp.clone(), Value::Undefined, &[a.clone(), b.clone()])?;
        // The comparator result is ToNumber'd even for BigInt arrays; a NaN or
        // ±0 result is treated as 0 (equal).
        let n = vm.to_number(&r)?;
        Ok(if n < 0.0 {
            -1
        } else if n > 0.0 {
            1
        } else {
            0
        })
    } else {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => Ok(default_numeric_compare(*x, *y)),
            (Value::BigInt(x), Value::BigInt(y)) => Ok(match x.cmp(y) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Greater => 1,
                std::cmp::Ordering::Equal => 0,
            }),
            _ => Ok(0),
        }
    }
}

fn default_numeric_compare(a: f64, b: f64) -> i32 {
    if a.is_nan() {
        return if b.is_nan() { 0 } else { 1 };
    }
    if b.is_nan() {
        return -1;
    }
    if a < b {
        -1
    } else if a > b {
        1
    } else if a == 0.0 && b == 0.0 {
        // -0 sorts before +0.
        let an = a.is_sign_negative();
        let bn = b.is_sign_negative();
        if an == bn {
            0
        } else if an {
            -1
        } else {
            1
        }
    } else {
        0
    }
}

// =========================================================================
// The nine concrete typed-array constructors.
// =========================================================================

/// Look up (or lazily install) the per-kind prototype, keyed by kind via the
/// constructor's `prototype`. We stash the per-kind protos on the realm-resident
/// `typed_array_proto` under a private string key so they can be recovered from
/// `Vm` alone (the helpers `new_same_kind`/`subarray` need them).
fn per_kind_proto(vm: &mut Vm, kind: TAKind) -> JsObject {
    let key = PropertyKey::str(format!("__proto_{}", kind.name()));
    let base = vm.realm.typed_array_proto.clone();
    if let Some(p) = base.borrow().props.get(&key) {
        if let Some(Value::Object(o)) = p.value() {
            return o.clone();
        }
    }
    // Should have been installed by install_kind_ctors; create a bare fallback.
    let proto = vm.alloc_ordinary(Some(base.clone()));
    base.borrow_mut()
        .props
        .insert(key, Property::builtin(Value::Object(proto.clone())));
    proto
}

fn install_kind_ctors(vm: &mut Vm, ta_ctor: &JsObject) {
    let ta_proto = vm.realm.typed_array_proto.clone();
    for kind in TAKind::all() {
        // Per-kind prototype chained to %TypedArray%.prototype.
        let proto = vm.alloc_ordinary(Some(ta_proto.clone()));
        // Record it on the base so per_kind_proto can recover it from the Vm.
        let key = PropertyKey::str(format!("__proto_{}", kind.name()));
        ta_proto
            .borrow_mut()
            .props
            .insert(key, Property::builtin(Value::Object(proto.clone())));

        let bytes_per = kind.bytes() as f64;
        let name = kind.name();

        let construct = move |vm: &mut Vm, _t: Value, args: &[Value]| -> Result<Value, Value> {
            construct_typed_array(vm, kind, args)
        };
        let ctor = vm.new_native_ctor(
            name,
            3,
            {
                let nm = name.to_string();
                move |vm: &mut Vm, t: Value, a: &[Value]| -> Result<Value, Value> {
                    // Reached only when called WITHOUT `new`. The one legitimate
                    // case is `super(...)` from a subclass — `class Buffer extends
                    // Uint8Array` — where `this` is the already-allocated derived
                    // instance whose prototype chain includes this kind's
                    // prototype. Build the typed array and adopt its exotic
                    // internal slot into `this`, so the subclass instance becomes a
                    // real typed array. (chidori-js models `super()` as a plain
                    // call with the pre-created `this` rather than a construct, so
                    // without this a native superclass would never allocate its
                    // exotic slots.) A bare `Uint8Array()` call has `this`
                    // undefined/global, so it still throws.
                    if let Value::Object(this_obj) = &t {
                        if subclass_instance_of(vm, this_obj, kind) {
                            let constructed = construct_typed_array(vm, kind, a)?;
                            if let Value::Object(c) = &constructed {
                                let internal = std::mem::replace(
                                    &mut c.borrow_mut().internal,
                                    Internal::Ordinary,
                                );
                                this_obj.borrow_mut().internal = internal;
                                return Ok(Value::Undefined);
                            }
                        }
                    }
                    Err(vm.throw_type(&format!("Constructor {nm} requires 'new'")))
                }
            },
            construct,
        );
        // Concrete ctor inherits from %TypedArray%.
        ctor.borrow_mut().proto = Some(ta_ctor.clone());
        vm.install_ctor(name, &ctor, &proto);
        // Record the INTRINSIC ctor on the base proto so TypedArraySpeciesCreate
        // can use it as the default even after `prototype.constructor` is
        // replaced or shadowed (SpeciesConstructor's defaultConstructor is the
        // intrinsic, not the current property value).
        let ckey = PropertyKey::str(format!("__ctor_{}", kind.name()));
        ta_proto
            .borrow_mut()
            .props
            .insert(ckey, Property::builtin(Value::Object(ctor.clone())));

        // BYTES_PER_ELEMENT on both ctor and prototype (non-writable,
        // non-enumerable, non-configurable).
        for target in [&ctor, &proto] {
            target.borrow_mut().props.insert(
                PropertyKey::str("BYTES_PER_ELEMENT"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(bytes_per),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        }

        // `of`/`from` are shared statics inherited from %TypedArray% (installed
        // on the base constructor in `install_typed_array_base`).
    }
}

/// Does `obj`'s prototype chain include this kind's prototype? Used to recognize
/// a `super(...)` call from a subclass (`class Buffer extends Uint8Array`) so the
/// constructor can initialize the derived instance in place.
fn subclass_instance_of(vm: &mut Vm, obj: &JsObject, kind: TAKind) -> bool {
    let target = per_kind_proto(vm, kind);
    let mut cur = obj.borrow().proto.clone();
    while let Some(p) = cur {
        if p.same(&target) {
            return true;
        }
        cur = p.borrow().proto.clone();
    }
    false
}

/// `new T(...)` dispatch: length, buffer(+offset+length), or array-like/iterable.
fn construct_typed_array(vm: &mut Vm, kind: TAKind, args: &[Value]) -> Result<Value, Value> {
    let proto = per_kind_proto(vm, kind);
    let elem = kind.bytes();
    let first = arg(args, 0);

    match &first {
        // new T(buffer, byteOffset?, length?)
        Value::Object(o) if matches!(o.borrow().internal, Internal::ArrayBuffer(_)) => {
            let buffer = o.clone();
            let byte_offset = {
                let off = to_integer_or_infinity(vm, &arg(args, 1))?;
                if off < 0.0 || off.is_infinite() {
                    return Err(vm.throw_range("Invalid typed array offset"));
                }
                off as usize
            };
            if byte_offset % elem != 0 {
                return Err(vm.throw_range("Start offset is not aligned to element size"));
            }
            // IsDetachedBuffer after ToIndex(byteOffset) (whose coercion can
            // itself detach), per spec; the byte length is read after that.
            if matches!(buffer.borrow().internal, Internal::ArrayBuffer(None)) {
                return Err(
                    vm.throw_type("Cannot construct a TypedArray on a detached ArrayBuffer")
                );
            }
            let buf_len = buffer_byte_length(&buffer);
            if byte_offset > buf_len {
                return Err(vm.throw_range("Start offset is out of bounds"));
            }
            let auto_length = arg(args, 2).is_undefined();
            let length = if auto_length {
                let remaining = buf_len - byte_offset;
                // A RESIZABLE buffer's auto-length view is length-tracking:
                // its element count floors freely, with no alignment demand
                // (the requirement applies to fixed buffers only).
                if remaining % elem != 0 && ab_max_byte_length(&buffer).is_none() {
                    return Err(vm.throw_range("Byte length is not aligned to element size"));
                }
                remaining / elem
            } else {
                let l = to_integer_or_infinity(vm, &arg(args, 2))?;
                if l < 0.0 || l.is_infinite() {
                    return Err(vm.throw_range("Invalid typed array length"));
                }
                // The length coercion can detach the buffer too.
                if matches!(buffer.borrow().internal, Internal::ArrayBuffer(None)) {
                    return Err(
                        vm.throw_type("Cannot construct a TypedArray on a detached ArrayBuffer")
                    );
                }
                let l = l as usize;
                if byte_offset + l * elem > buf_len {
                    return Err(vm.throw_range("Invalid typed array length"));
                }
                l
            };
            // An auto-length view on a resizable buffer is length-tracking.
            let tracking = auto_length && ab_max_byte_length(&buffer).is_some();
            let ta = vm.new_typed_array(kind, buffer, byte_offset, length, proto);
            if tracking {
                if let Internal::TypedArray(t) = &mut ta.borrow_mut().internal {
                    t.length_tracking = true;
                }
            }
            Ok(Value::Object(ta))
        }
        // new T(typedArray | arrayLike | iterable)
        Value::Object(o) if matches!(o.borrow().internal, Internal::TypedArray(_)) => {
            let src = o.clone();
            // A detached or out-of-bounds source TypedArray is a TypeError.
            if ta_out_of_bounds(&src) {
                return Err(vm.throw_type("source TypedArray is detached or out of bounds"));
            }
            let vals = ta_values_v(vm, &src);
            let len = vals.len();
            let buf = vm.new_array_buffer(len * elem);
            let ta = vm.new_typed_array(kind, buf, 0, len, proto);
            for (i, v) in vals.into_iter().enumerate() {
                vm.ta_write(&ta, i, &v)?;
            }
            Ok(Value::Object(ta))
        }
        Value::Object(_) => {
            // Iterable or array-like object.
            let items = if is_iterable(vm, &first)? {
                vm.iterate_to_vec(&first)?
            } else {
                let len_v = vm.get_prop(&first, &PropertyKey::str("length"))?;
                let len = vm.to_length(&len_v)?;
                // Reject an excessive `length` *before* materializing the vec — a
                // huge array-like length (test262 uses 2^53) would otherwise loop
                // and allocate until the process OOMs.
                if len > crate::value::MAX_DENSE_ARRAY {
                    return Err(vm.throw_range("TypedArray allocation exceeds engine limit"));
                }
                let mut v = Vec::with_capacity(len.min(1 << 16));
                for i in 0..len {
                    v.push(vm.get_prop(&first, &PropertyKey::from_index(i as u32))?);
                }
                v
            };
            let len = items.len();
            if len > crate::value::MAX_DENSE_ARRAY {
                return Err(vm.throw_range("TypedArray allocation exceeds engine limit"));
            }
            let buf = vm.new_array_buffer(len * elem);
            let ta = vm.new_typed_array(kind, buf, 0, len, proto);
            for (i, item) in items.into_iter().enumerate() {
                vm.ta_write(&ta, i, &item)?;
            }
            Ok(Value::Object(ta))
        }
        // new T(length)  (or new T() => length 0)
        _ => {
            let len = if first.is_undefined() && args.is_empty() {
                0
            } else {
                let n = to_integer_or_infinity(vm, &first)?;
                if n < 0.0 || n.is_infinite() {
                    return Err(vm.throw_range("Invalid typed array length"));
                }
                let len = n as usize;
                if (len as f64) != n {
                    return Err(vm.throw_range("Invalid typed array length"));
                }
                len
            };
            if len > crate::value::MAX_DENSE_ARRAY {
                return Err(vm.throw_range("TypedArray allocation exceeds engine limit"));
            }
            let buf = vm.new_array_buffer(len * elem);
            Ok(Value::Object(vm.new_typed_array(kind, buf, 0, len, proto)))
        }
    }
}

// =========================================================================
// DataView
// =========================================================================

fn install_data_view(vm: &mut Vm) {
    let proto = vm.realm.data_view_proto.clone();

    let construct = |vm: &mut Vm, _t: Value, args: &[Value]| -> Result<Value, Value> {
        let buffer = match arg(args, 0) {
            Value::Object(o) if matches!(o.borrow().internal, Internal::ArrayBuffer(_)) => o,
            _ => {
                return Err(
                    vm.throw_type("First argument to DataView constructor must be an ArrayBuffer")
                )
            }
        };
        let byte_offset = {
            let off = to_integer_or_infinity(vm, &arg(args, 1))?;
            if off < 0.0 || off.is_infinite() {
                return Err(vm.throw_range("Invalid DataView offset"));
            }
            off as usize
        };
        // IsDetachedBuffer after ToIndex(byteOffset), per spec.
        if matches!(buffer.borrow().internal, Internal::ArrayBuffer(None)) {
            return Err(vm.throw_type("Cannot construct a DataView on a detached ArrayBuffer"));
        }
        let buf_len = buffer_byte_length(&buffer);
        if byte_offset > buf_len {
            return Err(vm.throw_range("Start offset is outside the bounds of the buffer"));
        }
        // No explicit length: an auto-length view, which TRACKS a resizable
        // buffer's byte length.
        let (byte_length, length_tracking) = if arg(args, 2).is_undefined() {
            (buf_len - byte_offset, ab_max_byte_length(&buffer).is_some())
        } else {
            let l = to_integer_or_infinity(vm, &arg(args, 2))?;
            if l < 0.0 || l.is_infinite() {
                return Err(vm.throw_range("Invalid DataView length"));
            }
            let l = l as usize;
            if byte_offset + l > buf_len {
                return Err(vm.throw_range("Invalid DataView length"));
            }
            (l, false)
        };
        let dv = vm.alloc(ObjectData::new(
            Some(vm.realm.data_view_proto.clone()),
            Internal::DataView(DataViewData {
                buffer,
                byte_offset,
                byte_length,
                length_tracking,
            }),
        ));
        Ok(Value::Object(dv))
    };
    let ctor = vm.new_native_ctor(
        "DataView",
        1,
        |vm, _t, _a| Err(vm.throw_type("Constructor DataView requires 'new'")),
        construct,
    );
    vm.install_ctor("DataView", &ctor, &proto);

    // Accessors: buffer / byteLength / byteOffset.
    let g = vm.new_native("get buffer", 0, |vm, this, _a| match &this {
        Value::Object(o) => match &o.borrow().internal {
            Internal::DataView(d) => Ok(Value::Object(d.buffer.clone())),
            _ => {
                Err(vm.throw_type("get DataView.prototype.buffer called on incompatible receiver"))
            }
        },
        _ => Err(vm.throw_type("get DataView.prototype.buffer called on incompatible receiver")),
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("buffer"),
        Some(Value::Object(g)),
        None,
    );

    let g = vm.new_native("get byteLength", 0, |vm, this, _a| match &this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::DataView(_)) => {
            // Detached / out-of-bounds views throw; tracking views are live.
            Ok(Value::Number(dv_live_len(vm, o)? as f64))
        }
        _ => {
            Err(vm.throw_type("get DataView.prototype.byteLength called on incompatible receiver"))
        }
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("byteLength"),
        Some(Value::Object(g)),
        None,
    );

    let g = vm.new_native("get byteOffset", 0, |vm, this, _a| match &this {
        Value::Object(o) if matches!(o.borrow().internal, Internal::DataView(_)) => {
            // Detached / out-of-bounds views throw.
            dv_live_len(vm, o)?;
            let off = match &o.borrow().internal {
                Internal::DataView(d) => d.byte_offset,
                _ => 0,
            };
            Ok(Value::Number(off as f64))
        }
        _ => {
            Err(vm.throw_type("get DataView.prototype.byteOffset called on incompatible receiver"))
        }
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("byteOffset"),
        Some(Value::Object(g)),
        None,
    );

    // get/set methods for each element kind.
    define_dv_get(vm, &proto, "getInt8", DvKind::I8);
    define_dv_get(vm, &proto, "getUint8", DvKind::U8);
    define_dv_get(vm, &proto, "getInt16", DvKind::I16);
    define_dv_get(vm, &proto, "getUint16", DvKind::U16);
    define_dv_get(vm, &proto, "getInt32", DvKind::I32);
    define_dv_get(vm, &proto, "getUint32", DvKind::U32);
    define_dv_get(vm, &proto, "getFloat16", DvKind::F16);
    define_dv_get(vm, &proto, "getFloat32", DvKind::F32);
    define_dv_get(vm, &proto, "getFloat64", DvKind::F64);

    define_dv_set(vm, &proto, "setInt8", DvKind::I8);
    define_dv_set(vm, &proto, "setUint8", DvKind::U8);
    define_dv_set(vm, &proto, "setInt16", DvKind::I16);
    define_dv_set(vm, &proto, "setUint16", DvKind::U16);
    define_dv_set(vm, &proto, "setInt32", DvKind::I32);
    define_dv_set(vm, &proto, "setUint32", DvKind::U32);
    define_dv_set(vm, &proto, "setFloat16", DvKind::F16);
    define_dv_set(vm, &proto, "setFloat32", DvKind::F32);
    define_dv_set(vm, &proto, "setFloat64", DvKind::F64);

    define_dv_get_big(vm, &proto, "getBigInt64", TAKind::I64);
    define_dv_get_big(vm, &proto, "getBigUint64", TAKind::U64);
    define_dv_set_big(vm, &proto, "setBigInt64", TAKind::I64);
    define_dv_set_big(vm, &proto, "setBigUint64", TAKind::U64);

    // [Symbol.toStringTag] = "DataView"
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("DataView"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
}

/// Element kinds for DataView accessors (distinct from `TAKind` because clamped
/// uint8 has no DataView counterpart).
#[derive(Clone, Copy)]
enum DvKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F16,
    F32,
    F64,
}

impl DvKind {
    fn bytes(self) -> usize {
        match self {
            DvKind::I8 | DvKind::U8 => 1,
            DvKind::I16 | DvKind::U16 | DvKind::F16 => 2,
            DvKind::I32 | DvKind::U32 | DvKind::F32 => 4,
            DvKind::F64 => 8,
        }
    }
}

/// Snapshot DataView fields without holding a borrow across vm.* calls.
fn dv_fields(o: &JsObject) -> Option<(JsObject, usize, usize)> {
    match &o.borrow().internal {
        Internal::DataView(d) => Some((d.buffer.clone(), d.byte_offset, d.byte_length)),
        _ => None,
    }
}

/// The view's LIVE byte length: TypeError when the buffer is detached or the
/// view no longer fits a shrunk resizable buffer (IsViewOutOfBounds); a
/// length-tracking view follows the buffer's current byte length.
fn dv_live_len(vm: &Vm, o: &JsObject) -> Result<usize, Value> {
    match &o.borrow().internal {
        Internal::DataView(d) => {
            let buf_len = match &d.buffer.borrow().internal {
                Internal::ArrayBuffer(Some(b)) => b.len(),
                _ => {
                    return Err(
                        vm.throw_type("Cannot perform DataView access on a detached ArrayBuffer")
                    )
                }
            };
            if d.byte_offset > buf_len
                || (!d.length_tracking && d.byte_offset + d.byte_length > buf_len)
            {
                return Err(vm.throw_type("DataView is out of bounds"));
            }
            Ok(if d.length_tracking {
                buf_len - d.byte_offset
            } else {
                d.byte_length
            })
        }
        _ => Err(vm.throw_type("not a DataView")),
    }
}

fn define_dv_get(vm: &mut Vm, proto: &JsObject, name: &str, kind: DvKind) {
    vm.define_method(proto, name, 1, move |vm, this, args| {
        let o = match &this {
            Value::Object(o) if matches!(o.borrow().internal, Internal::DataView(_)) => o.clone(),
            _ => {
                return Err(
                    vm.throw_type("DataView.prototype method called on incompatible receiver")
                )
            }
        };
        let (buffer, base_off, _) = dv_fields(&o).ok_or_else(|| vm.throw_type("not a DataView"))?;
        let get_index = {
            let idx = to_integer_or_infinity(vm, &arg(args, 0))?;
            if idx < 0.0 || idx.is_infinite() {
                return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
            }
            idx as usize
        };
        let little_endian = vm.to_boolean(&arg(args, 1));
        // Detached / out-of-bounds view is a TypeError, checked after the
        // index coercion and before the bounds (RangeError).
        let view_len = dv_live_len(vm, &o)?;
        let size = kind.bytes();
        let off = base_off + get_index;
        let buf = buffer.borrow();
        let bytes = match &buf.internal {
            Internal::ArrayBuffer(Some(b)) => b,
            _ => {
                return Err(vm.throw_type("Cannot perform DataView read on a detached ArrayBuffer"))
            }
        };
        if get_index + size > view_len || off + size > bytes.len() {
            return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
        }
        let val = read_dv(bytes, off, kind, little_endian);
        Ok(Value::Number(val))
    });
}

fn define_dv_set(vm: &mut Vm, proto: &JsObject, name: &str, kind: DvKind) {
    vm.define_method(proto, name, 2, move |vm, this, args| {
        let o = match &this {
            Value::Object(o) if matches!(o.borrow().internal, Internal::DataView(_)) => o.clone(),
            _ => {
                return Err(
                    vm.throw_type("DataView.prototype method called on incompatible receiver")
                )
            }
        };
        let (buffer, base_off, _) = dv_fields(&o).ok_or_else(|| vm.throw_type("not a DataView"))?;
        let set_index = {
            let idx = to_integer_or_infinity(vm, &arg(args, 0))?;
            if idx < 0.0 || idx.is_infinite() {
                return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
            }
            idx as usize
        };
        // ToNumber on the value happens before the endian/bounds checks per spec.
        let value = vm.to_number(&arg(args, 1))?;
        let little_endian = vm.to_boolean(&arg(args, 2));
        // Detached / out-of-bounds view is a TypeError, checked after the
        // coercions and before the bounds (RangeError).
        let view_len = dv_live_len(vm, &o)?;
        let size = kind.bytes();
        let off = base_off + set_index;
        let mut buf = buffer.borrow_mut();
        let bytes = match &mut buf.internal {
            Internal::ArrayBuffer(Some(b)) => b,
            _ => {
                return Err(vm.throw_type("Cannot perform DataView write on a detached ArrayBuffer"))
            }
        };
        if set_index + size > view_len || off + size > bytes.len() {
            return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
        }
        write_dv(bytes, off, kind, little_endian, value);
        Ok(Value::Undefined)
    });
}

/// `DataView.prototype.getBigInt64`/`getBigUint64`: read 8 bytes as a BigInt.
fn define_dv_get_big(vm: &mut Vm, proto: &JsObject, name: &str, kind: TAKind) {
    vm.define_method(proto, name, 1, move |vm, this, args| {
        let o = match &this {
            Value::Object(o) if matches!(o.borrow().internal, Internal::DataView(_)) => o.clone(),
            _ => {
                return Err(
                    vm.throw_type("DataView.prototype method called on incompatible receiver")
                )
            }
        };
        let (buffer, base_off, _) = dv_fields(&o).ok_or_else(|| vm.throw_type("not a DataView"))?;
        let get_index = {
            let idx = to_integer_or_infinity(vm, &arg(args, 0))?;
            if idx < 0.0 || idx.is_infinite() {
                return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
            }
            idx as usize
        };
        let little_endian = vm.to_boolean(&arg(args, 1));
        // Detached / out-of-bounds view is a TypeError (after index coercion).
        let view_len = dv_live_len(vm, &o)?;
        let off = base_off + get_index;
        let buf = buffer.borrow();
        // Detached (TypeError) before bounds (RangeError).
        let bytes = match &buf.internal {
            Internal::ArrayBuffer(Some(b)) => b,
            _ => {
                return Err(vm.throw_type("Cannot perform DataView read on a detached ArrayBuffer"))
            }
        };
        if get_index + 8 > view_len || off + 8 > bytes.len() {
            return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
        }
        let mut a = [0u8; 8];
        a.copy_from_slice(&bytes[off..off + 8]);
        let n = if little_endian {
            match kind {
                TAKind::I64 => num_bigint::BigInt::from(i64::from_le_bytes(a)),
                _ => num_bigint::BigInt::from(u64::from_le_bytes(a)),
            }
        } else {
            match kind {
                TAKind::I64 => num_bigint::BigInt::from(i64::from_be_bytes(a)),
                _ => num_bigint::BigInt::from(u64::from_be_bytes(a)),
            }
        };
        Ok(Value::bigint(n))
    });
}

/// `DataView.prototype.setBigInt64`/`setBigUint64`: ToBigInt the value, then
/// write its low 64 bits.
fn define_dv_set_big(vm: &mut Vm, proto: &JsObject, name: &str, kind: TAKind) {
    vm.define_method(proto, name, 2, move |vm, this, args| {
        let o = match &this {
            Value::Object(o) if matches!(o.borrow().internal, Internal::DataView(_)) => o.clone(),
            _ => {
                return Err(
                    vm.throw_type("DataView.prototype method called on incompatible receiver")
                )
            }
        };
        let (buffer, base_off, _) = dv_fields(&o).ok_or_else(|| vm.throw_type("not a DataView"))?;
        let set_index = {
            let idx = to_integer_or_infinity(vm, &arg(args, 0))?;
            if idx < 0.0 || idx.is_infinite() {
                return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
            }
            idx as usize
        };
        // ToBigInt on the value happens before the endian/bounds checks per spec.
        let value = vm.to_bigint(&arg(args, 1))?;
        let little_endian = vm.to_boolean(&arg(args, 2));
        // Detached / out-of-bounds view is a TypeError (after the coercions).
        let view_len = dv_live_len(vm, &o)?;
        let off = base_off + set_index;
        let mut buf = buffer.borrow_mut();
        // Detached (TypeError) before bounds (RangeError).
        let bytes = match &mut buf.internal {
            Internal::ArrayBuffer(Some(b)) => b,
            _ => {
                return Err(vm.throw_type("Cannot perform DataView write on a detached ArrayBuffer"))
            }
        };
        if set_index + 8 > view_len || off + 8 > bytes.len() {
            return Err(vm.throw_range("Offset is outside the bounds of the DataView"));
        }
        let low = crate::typed_array::bigint_low_u64_pub(&value);
        let b = if little_endian {
            match kind {
                TAKind::I64 => (low as i64).to_le_bytes(),
                _ => low.to_le_bytes(),
            }
        } else {
            match kind {
                TAKind::I64 => (low as i64).to_be_bytes(),
                _ => low.to_be_bytes(),
            }
        };
        bytes[off..off + 8].copy_from_slice(&b);
        Ok(Value::Undefined)
    });
}

/// Read a value of `kind` from `bytes` at `off`, honoring endianness (default,
/// per spec, is big-endian when `little_endian` is false).
fn read_dv(bytes: &[u8], off: usize, kind: DvKind, little_endian: bool) -> f64 {
    macro_rules! rd {
        ($t:ty, $n:expr) => {{
            let mut a = [0u8; $n];
            a.copy_from_slice(&bytes[off..off + $n]);
            if little_endian {
                <$t>::from_le_bytes(a)
            } else {
                <$t>::from_be_bytes(a)
            }
        }};
    }
    match kind {
        DvKind::I8 => bytes[off] as i8 as f64,
        DvKind::U8 => bytes[off] as f64,
        DvKind::I16 => rd!(i16, 2) as f64,
        DvKind::U16 => rd!(u16, 2) as f64,
        DvKind::I32 => rd!(i32, 4) as f64,
        DvKind::U32 => rd!(u32, 4) as f64,
        DvKind::F16 => f16_to_f64(rd!(u16, 2)),
        DvKind::F32 => rd!(f32, 4) as f64,
        DvKind::F64 => rd!(f64, 8),
    }
}

/// Decode IEEE-754 binary16 (half precision) to an `f64`.
pub(crate) fn f16_to_f64(h: u16) -> f64 {
    let sign = if (h >> 15) & 1 == 1 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x3ff) as f64;
    if exp == 0 {
        sign * mant * 2f64.powi(-24)
    } else if exp == 0x1f {
        if mant == 0.0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        sign * (1.0 + mant / 1024.0) * 2f64.powi(exp - 15)
    }
}

/// Encode an `f64` to IEEE-754 binary16 bits, rounding to nearest, ties to even.
pub(crate) fn f16_from_f64(value: f64) -> u16 {
    if value.is_nan() {
        return 0x7e00;
    }
    let sign: u16 = if value.is_sign_negative() { 0x8000 } else { 0 };
    let a = value.abs();
    if a.is_infinite() {
        return sign | 0x7c00;
    }
    if a == 0.0 {
        return sign;
    }
    let bits = a.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let mant52 = bits & 0xf_ffff_ffff_ffff;
    let exp16 = exp + 15;
    if exp16 >= 0x1f {
        return sign | 0x7c00; // overflow to infinity
    }
    if exp16 <= 0 {
        if exp < -25 {
            return sign; // underflow to zero
        }
        // Subnormal: round the 53-bit significand into the binary16 grid.
        let full = (1u64 << 52) | mant52;
        let rshift = (28 - exp) as u32;
        if rshift >= 64 {
            return sign;
        }
        let mut m = full >> rshift;
        let rem = full & ((1u64 << rshift) - 1);
        let halfway = 1u64 << (rshift - 1);
        if rem > halfway || (rem == halfway && (m & 1) == 1) {
            m += 1;
        }
        return sign | (m as u16);
    }
    // Normal.
    let rshift = 42u32;
    let mut mant16 = mant52 >> rshift;
    let rem = mant52 & ((1u64 << rshift) - 1);
    let halfway = 1u64 << (rshift - 1);
    let mut e = exp16 as u64;
    if rem > halfway || (rem == halfway && (mant16 & 1) == 1) {
        mant16 += 1;
        if mant16 == 0x400 {
            mant16 = 0;
            e += 1;
            if e >= 0x1f {
                return sign | 0x7c00;
            }
        }
    }
    sign | ((e as u16) << 10) | (mant16 as u16)
}

/// Write `v` of `kind` into `bytes` at `off`, honoring endianness, applying the
/// integer coercions matching the typed-array codec.
fn write_dv(bytes: &mut [u8], off: usize, kind: DvKind, little_endian: bool, v: f64) {
    macro_rules! wr {
        ($e:expr, $n:expr) => {{
            let b = if little_endian {
                ($e).to_le_bytes()
            } else {
                ($e).to_be_bytes()
            };
            bytes[off..off + $n].copy_from_slice(&b);
        }};
    }
    match kind {
        DvKind::I8 => bytes[off] = dv_to_int(v) as i8 as u8,
        DvKind::U8 => bytes[off] = dv_to_int(v) as u8,
        DvKind::I16 => wr!(dv_to_int(v) as i16, 2),
        DvKind::U16 => wr!(dv_to_int(v) as u16, 2),
        DvKind::I32 => wr!(dv_to_int(v) as i32, 4),
        DvKind::U32 => wr!(dv_to_int(v) as u32, 4),
        DvKind::F16 => wr!(f16_from_f64(v), 2),
        DvKind::F32 => wr!(v as f32, 4),
        DvKind::F64 => wr!(v, 8),
    }
}

/// Truncate toward zero into i64 range for integer DataView writes (the element
/// cast then wraps to the target width, matching ToInt32/ToUint8 etc).
fn dv_to_int(v: f64) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    let t = v.trunc();
    if t >= i64::MAX as f64 {
        i64::MAX
    } else if t <= i64::MIN as f64 {
        i64::MIN
    } else {
        t as i64
    }
}

// =========================================================================
// Shared small helpers (mirroring array.rs idioms, kept local).
// =========================================================================

/// ECMAScript `ToIntegerOrInfinity`.
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

// =========================================================================
// Atomics
// =========================================================================
//
// The embedded runtime is single-threaded (the engine is `Rc`-based and
// non-`Send`), so every Atomics operation is a plain sequential read /
// read-modify-write — there is no contention to be atomic against, and that is
// observationally indistinguishable from a real atomic on a single agent.
// `Atomics.wait` therefore reports that the calling agent cannot block (as a
// browser main thread does); the genuinely-concurrent Test262 agent tests are
// skipped separately. Atomics operate on both shared and non-shared integer
// typed arrays (ES2024+); only `wait`/`notify` require a SharedArrayBuffer.

#[derive(Clone, Copy)]
enum AtomicOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Exchange,
}

/// `ToIndex`: a non-negative integer ≤ 2^53−1, else a RangeError.
fn to_index(vm: &mut Vm, v: &Value) -> Result<usize, Value> {
    let n = to_integer_or_infinity(vm, v)?;
    if n < 0.0 || n > 9007199254740991.0 {
        return Err(vm.throw_range("Atomics: index out of range"));
    }
    Ok(n as usize)
}

/// `ValidateIntegerTypedArray(typedArray, waitable)`: the receiver must be a
/// non-detached typed array whose element type is integral. `waitable` narrows
/// that to `Int32Array` / `BigInt64Array` (the only types `wait`/`notify` accept).
fn atomic_ta(vm: &mut Vm, value: &Value, waitable: bool) -> Result<(JsObject, TAKind), Value> {
    let o = match value {
        Value::Object(o) if matches!(o.borrow().internal, Internal::TypedArray(_)) => o.clone(),
        _ => return Err(vm.throw_type("Atomics: argument is not an integer TypedArray")),
    };
    if ta_out_of_bounds(&o) {
        return Err(vm.throw_type("Atomics: TypedArray is detached or out of bounds"));
    }
    let kind = match &o.borrow().internal {
        Internal::TypedArray(t) => t.kind,
        _ => unreachable!(),
    };
    let ok = if waitable {
        matches!(kind, TAKind::I32 | TAKind::I64)
    } else {
        !matches!(kind, TAKind::F32 | TAKind::F64 | TAKind::U8Clamped)
    };
    if !ok {
        return Err(vm.throw_type("Atomics: unsupported TypedArray element type"));
    }
    Ok((o, kind))
}

/// `ValidateAtomicAccess(taRecord, requestIndex)`: coerce the index, bounds-check
/// it against the (live) element count, and return the absolute byte offset of
/// the element within the backing buffer.
fn atomic_byte_index(
    vm: &mut Vm,
    o: &JsObject,
    kind: TAKind,
    index_arg: &Value,
) -> Result<usize, Value> {
    let (_, byte_offset, len, _) = ta_fields(o).unwrap();
    let access = to_index(vm, index_arg)?;
    if access >= len {
        return Err(vm.throw_range("Atomics: index out of bounds"));
    }
    Ok(byte_offset + access * kind.bytes())
}

/// `RevalidateAtomicAccess`: after value/index coercion (which can run user code
/// that detaches or shrinks the buffer), confirm the element still fits.
fn atomic_revalidate(
    vm: &mut Vm,
    o: &JsObject,
    kind: TAKind,
    byte_index: usize,
) -> Result<(), Value> {
    let oob = match &o.borrow().internal {
        Internal::TypedArray(t) => {
            let buf_len = match &t.buffer.borrow().internal {
                Internal::ArrayBuffer(Some(x)) => x.len(),
                _ => 0,
            };
            byte_index + kind.bytes() > buf_len
        }
        _ => true,
    };
    if oob {
        return Err(vm.throw_type("Atomics: typed array became out of bounds"));
    }
    Ok(())
}

/// The backing buffer of a (validated) typed array.
fn ta_buffer(o: &JsObject) -> JsObject {
    match &o.borrow().internal {
        Internal::TypedArray(t) => t.buffer.clone(),
        _ => unreachable!(),
    }
}

/// Wrap a number operand to the element type's raw value (round-trip through the
/// codec — the spec's `NumberToRawBytes` then `RawBytesToNumber`).
fn wrap_int(kind: TAKind, v: f64) -> f64 {
    let mut scratch = [0u8; 8];
    crate::typed_array::encode(&mut scratch, 0, kind, v);
    crate::typed_array::decode(&scratch, 0, kind)
}

fn wrap_big(kind: TAKind, v: &num_bigint::BigInt) -> num_bigint::BigInt {
    let mut scratch = [0u8; 8];
    crate::typed_array::encode_big(&mut scratch, 0, kind, v);
    crate::typed_array::decode_big(&scratch, 0, kind)
}

/// Read-modify-write on an integer (non-BigInt) element; returns the prior value.
fn do_integer_rmw(
    o: &JsObject,
    byte_index: usize,
    kind: TAKind,
    op: AtomicOp,
    operand: f64,
) -> f64 {
    let buffer = ta_buffer(o);
    let mut b = buffer.borrow_mut();
    if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
        let old = crate::typed_array::decode(bytes, byte_index, kind);
        let opnd = wrap_int(kind, operand);
        let (oi, vi) = (old as i64, opnd as i64);
        let res = match op {
            AtomicOp::Add => oi.wrapping_add(vi),
            AtomicOp::Sub => oi.wrapping_sub(vi),
            AtomicOp::And => oi & vi,
            AtomicOp::Or => oi | vi,
            AtomicOp::Xor => oi ^ vi,
            AtomicOp::Exchange => vi,
        };
        crate::typed_array::encode(bytes, byte_index, kind, res as f64);
        old
    } else {
        f64::NAN
    }
}

/// Read-modify-write on a BigInt element; returns the prior value.
fn do_bigint_rmw(
    o: &JsObject,
    byte_index: usize,
    kind: TAKind,
    op: AtomicOp,
    operand: &num_bigint::BigInt,
) -> num_bigint::BigInt {
    let buffer = ta_buffer(o);
    let mut b = buffer.borrow_mut();
    if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
        let old = crate::typed_array::decode_big(bytes, byte_index, kind);
        let v = wrap_big(kind, operand);
        let res = match op {
            AtomicOp::Add => &old + &v,
            AtomicOp::Sub => &old - &v,
            AtomicOp::And => &old & &v,
            AtomicOp::Or => &old | &v,
            AtomicOp::Xor => &old ^ &v,
            AtomicOp::Exchange => v,
        };
        crate::typed_array::encode_big(bytes, byte_index, kind, &res);
        old
    } else {
        num_bigint::BigInt::from(0)
    }
}

fn atomic_rmw(vm: &mut Vm, args: &[Value], op: AtomicOp) -> Result<Value, Value> {
    let (o, kind) = atomic_ta(vm, &arg(args, 0), false)?;
    let byte_index = atomic_byte_index(vm, &o, kind, &arg(args, 1))?;
    if kind.is_bigint() {
        let v = vm.to_bigint(&arg(args, 2))?;
        atomic_revalidate(vm, &o, kind, byte_index)?;
        Ok(Value::bigint(do_bigint_rmw(&o, byte_index, kind, op, &v)))
    } else {
        let v = to_integer_or_infinity(vm, &arg(args, 2))?;
        atomic_revalidate(vm, &o, kind, byte_index)?;
        Ok(Value::Number(do_integer_rmw(&o, byte_index, kind, op, v)))
    }
}

fn atomic_compare_exchange(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let (o, kind) = atomic_ta(vm, &arg(args, 0), false)?;
    let byte_index = atomic_byte_index(vm, &o, kind, &arg(args, 1))?;
    if kind.is_bigint() {
        let expected = vm.to_bigint(&arg(args, 2))?;
        let replacement = vm.to_bigint(&arg(args, 3))?;
        atomic_revalidate(vm, &o, kind, byte_index)?;
        let buffer = ta_buffer(&o);
        let mut b = buffer.borrow_mut();
        if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
            let old = crate::typed_array::decode_big(bytes, byte_index, kind);
            if old == wrap_big(kind, &expected) {
                crate::typed_array::encode_big(bytes, byte_index, kind, &replacement);
            }
            return Ok(Value::bigint(old));
        }
        Ok(Value::bigint(num_bigint::BigInt::from(0)))
    } else {
        let expected = to_integer_or_infinity(vm, &arg(args, 2))?;
        let replacement = to_integer_or_infinity(vm, &arg(args, 3))?;
        atomic_revalidate(vm, &o, kind, byte_index)?;
        let buffer = ta_buffer(&o);
        let mut b = buffer.borrow_mut();
        if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
            let old = crate::typed_array::decode(bytes, byte_index, kind);
            if old == wrap_int(kind, expected) {
                crate::typed_array::encode(bytes, byte_index, kind, replacement);
            }
            return Ok(Value::Number(old));
        }
        Ok(Value::Number(f64::NAN))
    }
}

fn atomic_load(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let (o, kind) = atomic_ta(vm, &arg(args, 0), false)?;
    let byte_index = atomic_byte_index(vm, &o, kind, &arg(args, 1))?;
    atomic_revalidate(vm, &o, kind, byte_index)?;
    let buffer = ta_buffer(&o);
    let b = buffer.borrow();
    if let Internal::ArrayBuffer(Some(bytes)) = &b.internal {
        if kind.is_bigint() {
            return Ok(Value::bigint(crate::typed_array::decode_big(
                bytes, byte_index, kind,
            )));
        }
        return Ok(Value::Number(crate::typed_array::decode(
            bytes, byte_index, kind,
        )));
    }
    Ok(Value::Undefined)
}

fn atomic_store(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let (o, kind) = atomic_ta(vm, &arg(args, 0), false)?;
    let byte_index = atomic_byte_index(vm, &o, kind, &arg(args, 1))?;
    // `store` returns the fully-coerced input value, not the (possibly truncated)
    // value actually written to the element.
    if kind.is_bigint() {
        let v = vm.to_bigint(&arg(args, 2))?;
        atomic_revalidate(vm, &o, kind, byte_index)?;
        let buffer = ta_buffer(&o);
        let mut b = buffer.borrow_mut();
        if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
            crate::typed_array::encode_big(bytes, byte_index, kind, &v);
        }
        Ok(Value::bigint(v))
    } else {
        // ToIntegerOrInfinity is mathematical: `-0` normalizes to `+0`, both for
        // the stored element and for the returned value.
        let v = to_integer_or_infinity(vm, &arg(args, 2))? + 0.0;
        atomic_revalidate(vm, &o, kind, byte_index)?;
        let buffer = ta_buffer(&o);
        let mut b = buffer.borrow_mut();
        if let Internal::ArrayBuffer(Some(bytes)) = &mut b.internal {
            crate::typed_array::encode(bytes, byte_index, kind, v);
        }
        Ok(Value::Number(v))
    }
}

/// `Atomics.pause([N])`: a no-op back-off hint. `N`, if present, must be an
/// integral Number (the spec validates the type but otherwise ignores it).
fn atomic_pause(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let n = arg(args, 0);
    if !n.is_undefined() {
        let integral = matches!(&n, Value::Number(x) if x.is_finite() && x.fract() == 0.0);
        if !integral {
            return Err(vm.throw_type("Atomics.pause: argument must be an integral Number"));
        }
    }
    Ok(Value::Undefined)
}

fn atomic_wait(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let (o, kind) = atomic_ta(vm, &arg(args, 0), true)?;
    // `wait` requires shared memory.
    let shared = match &o.borrow().internal {
        Internal::TypedArray(t) => vm.is_shared_buffer(&t.buffer),
        _ => false,
    };
    if !shared {
        return Err(
            vm.throw_type("Atomics.wait: typed array must be backed by a SharedArrayBuffer")
        );
    }
    let byte_index = atomic_byte_index(vm, &o, kind, &arg(args, 1))?;
    let _ = byte_index;
    // Coerce the comparison value and timeout (observable side effects) before
    // reporting that this agent cannot block.
    if kind.is_bigint() {
        vm.to_bigint(&arg(args, 2))?;
    } else {
        vm.to_int32(&arg(args, 2))?;
    }
    vm.to_number(&arg(args, 3))?;
    Err(vm.throw_type("Atomics.wait cannot be used: the calling agent cannot block"))
}

fn atomic_notify(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let (o, kind) = atomic_ta(vm, &arg(args, 0), true)?;
    atomic_byte_index(vm, &o, kind, &arg(args, 1))?;
    // Coerce the count (may throw) even though no agent is ever waiting here.
    let count = arg(args, 2);
    if !count.is_undefined() {
        to_integer_or_infinity(vm, &count)?;
    }
    Ok(Value::Number(0.0))
}

fn install_atomics(vm: &mut Vm) {
    let atomics = vm.new_object();
    vm.define_method(&atomics, "add", 3, |vm, _t, a| {
        atomic_rmw(vm, a, AtomicOp::Add)
    });
    vm.define_method(&atomics, "sub", 3, |vm, _t, a| {
        atomic_rmw(vm, a, AtomicOp::Sub)
    });
    vm.define_method(&atomics, "and", 3, |vm, _t, a| {
        atomic_rmw(vm, a, AtomicOp::And)
    });
    vm.define_method(&atomics, "or", 3, |vm, _t, a| {
        atomic_rmw(vm, a, AtomicOp::Or)
    });
    vm.define_method(&atomics, "xor", 3, |vm, _t, a| {
        atomic_rmw(vm, a, AtomicOp::Xor)
    });
    vm.define_method(&atomics, "exchange", 3, |vm, _t, a| {
        atomic_rmw(vm, a, AtomicOp::Exchange)
    });
    vm.define_method(&atomics, "compareExchange", 4, |vm, _t, a| {
        atomic_compare_exchange(vm, a)
    });
    vm.define_method(&atomics, "load", 2, |vm, _t, a| atomic_load(vm, a));
    vm.define_method(&atomics, "store", 3, |vm, _t, a| atomic_store(vm, a));
    vm.define_method(&atomics, "isLockFree", 1, |vm, _t, a| {
        let n = to_integer_or_infinity(vm, &arg(a, 0))?;
        Ok(Value::Bool(n == 1.0 || n == 2.0 || n == 4.0 || n == 8.0))
    });
    vm.define_method(&atomics, "wait", 4, |vm, _t, a| atomic_wait(vm, a));
    vm.define_method(&atomics, "notify", 3, |vm, _t, a| atomic_notify(vm, a));
    vm.define_method(&atomics, "pause", 0, |vm, _t, a| atomic_pause(vm, a));

    // Atomics[Symbol.toStringTag] = "Atomics" (non-writable, non-enumerable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    atomics.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Atomics"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
    vm.define_value(&vm.realm.global.clone(), "Atomics", Value::Object(atomics));
}

fn is_iterable(vm: &mut Vm, v: &Value) -> Result<bool, Value> {
    if v.is_nullish() {
        return Ok(false);
    }
    let sym = vm.realm.symbol_iterator.clone();
    let m = vm.get_prop(v, &PropertyKey::Sym(sym))?;
    if m.is_nullish() {
        return Ok(false);
    }
    // GetMethod: a present but non-callable @@iterator is a TypeError, not a
    // silent fall-back to the array-like path.
    if !vm.is_callable(&m) {
        return Err(vm.throw_type("Symbol.iterator is not a function"));
    }
    Ok(true)
}
