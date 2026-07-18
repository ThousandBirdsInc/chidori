//! TypedArray / ArrayBuffer / DataView engine core: the element codec and the
//! `Vm` helpers + exotic indexed-access hooks the builtins build on. The large
//! builtin surface (constructors, prototype methods) lives in
//! `builtins/typedarray.rs`; this module is the trusted plumbing.

use crate::value::*;
use crate::vm::Vm;
use num_bigint::{BigInt, Sign};

/// Hidden own-property name holding a resizable/growable buffer's
/// `[[ArrayBufferMaxByteLength]]`. An internal slot, never a real property:
/// `Vm::own_keys` filters it out of `[[OwnPropertyKeys]]`.
pub const ARRAY_BUFFER_MAX_SLOT: &str = "[[ArrayBufferMaxByteLength]]";

// =========================================================================
// Element codec (little-endian, matching the platform/spec default)
// =========================================================================

/// Decode the element at byte `off` of `bytes` as an `f64` JS number.
pub fn decode(bytes: &[u8], off: usize, kind: TAKind) -> f64 {
    macro_rules! rd {
        ($t:ty, $n:expr) => {{
            let mut a = [0u8; $n];
            let end = off + $n;
            if end <= bytes.len() {
                a.copy_from_slice(&bytes[off..end]);
            }
            <$t>::from_le_bytes(a)
        }};
    }
    match kind {
        TAKind::I8 => rd!(i8, 1) as f64,
        TAKind::U8 | TAKind::U8Clamped => rd!(u8, 1) as f64,
        TAKind::I16 => rd!(i16, 2) as f64,
        TAKind::U16 => rd!(u16, 2) as f64,
        TAKind::I32 => rd!(i32, 4) as f64,
        TAKind::U32 => rd!(u32, 4) as f64,
        TAKind::F32 => rd!(f32, 4) as f64,
        TAKind::F64 => rd!(f64, 8),
        // BigInt kinds are decoded via `decode_big`; never reached here.
        TAKind::I64 | TAKind::U64 => f64::NAN,
    }
}

/// Decode a BigInt element (`BigInt64Array` / `BigUint64Array`) as a `BigInt`.
pub fn decode_big(bytes: &[u8], off: usize, kind: TAKind) -> BigInt {
    let mut a = [0u8; 8];
    if off + 8 <= bytes.len() {
        a.copy_from_slice(&bytes[off..off + 8]);
    }
    match kind {
        TAKind::I64 => BigInt::from(i64::from_le_bytes(a)),
        _ => BigInt::from(u64::from_le_bytes(a)),
    }
}

/// Encode a BigInt element with the kind's modular wraparound (ToBigInt64 /
/// ToBigUint64): reduce modulo 2^64, interpreting the low 64 bits.
pub fn encode_big(bytes: &mut [u8], off: usize, kind: TAKind, v: &BigInt) {
    let low = bigint_low_u64(v);
    let b = match kind {
        TAKind::I64 => (low as i64).to_le_bytes(),
        _ => low.to_le_bytes(),
    };
    if off + 8 <= bytes.len() {
        bytes[off..off + 8].copy_from_slice(&b);
    }
}

/// The low 64 bits of a BigInt (two's-complement for negatives), i.e. `v mod 2^64`.
/// Exposed for DataView's BigInt setters.
pub fn bigint_low_u64_pub(v: &BigInt) -> u64 {
    bigint_low_u64(v)
}

fn bigint_low_u64(v: &BigInt) -> u64 {
    let two_64: BigInt = BigInt::from(1u8) << 64u32;
    let mask: BigInt = &two_64 - 1;
    // Euclidean reduction into [0, 2^64).
    let mut reduced: BigInt = v % &two_64;
    if reduced.sign() == Sign::Minus {
        reduced += &two_64;
    }
    reduced &= &mask;
    let digits = reduced.to_u64_digits().1;
    digits.first().copied().unwrap_or(0)
}

/// Encode `v` into the element at byte `off`, applying the kind's coercion
/// (ToInt8/ToUint8/ToUint8Clamp/…/ identity for floats).
pub fn encode(bytes: &mut [u8], off: usize, kind: TAKind, v: f64) {
    macro_rules! wr {
        ($e:expr, $n:expr) => {{
            let b = ($e).to_le_bytes();
            let end = off + $n;
            if end <= bytes.len() {
                bytes[off..end].copy_from_slice(&b);
            }
        }};
    }
    match kind {
        TAKind::I8 => wr!(to_int(v) as i8, 1),
        TAKind::U8 => wr!(to_int(v) as u8, 1),
        TAKind::U8Clamped => wr!(to_uint8_clamp(v), 1),
        TAKind::I16 => wr!(to_int(v) as i16, 2),
        TAKind::U16 => wr!(to_int(v) as u16, 2),
        TAKind::I32 => wr!(to_int(v) as i32, 4),
        TAKind::U32 => wr!(to_int(v) as u32, 4),
        TAKind::F32 => wr!(v as f32, 4),
        TAKind::F64 => wr!(v, 8),
        // BigInt kinds are written via `encode_big`; never reached here.
        TAKind::I64 | TAKind::U64 => {}
    }
}

/// ToIntegerOrInfinity-then-modulo for integer typed arrays (ToInt32-style
/// wraparound). Returns an i64 that the caller truncates to the element width.
fn to_int(v: f64) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    let t = v.trunc();
    // Wrap into i64 range via modulo 2^64 semantics is overkill; element casts
    // (`as i8` etc.) already wrap. Guard against out-of-i64-range to avoid UB.
    if t >= i64::MAX as f64 {
        i64::MAX
    } else if t <= i64::MIN as f64 {
        i64::MIN
    } else {
        t as i64
    }
}

fn to_uint8_clamp(v: f64) -> u8 {
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= 255.0 {
        255
    } else {
        // round half to even
        v.round_ties_even() as u8
    }
}

/// A typed array's effective element count: its fixed `length`, or — for an
/// auto-length view on a resizable buffer — the count derived from the buffer's
/// current byte length (`floor((bufferByteLength - byteOffset) / elementSize)`,
/// clamped to 0 when the view no longer fits / the buffer is detached).
pub fn ta_eff_length(t: &TypedArrayData) -> usize {
    let buf_len = match &t.buffer.borrow().internal {
        Internal::ArrayBuffer(Some(bytes)) => bytes.len(),
        _ => 0, // detached
    };
    if !t.length_tracking {
        // IsTypedArrayOutOfBounds: a fixed-length view that no longer fits its
        // (shrunk resizable) buffer behaves like a detached one — length 0.
        if t.byte_offset + t.length * t.kind.bytes() > buf_len {
            return 0;
        }
        return t.length;
    }
    buf_len.saturating_sub(t.byte_offset) / t.kind.bytes()
}

/// `IsTypedArrayOutOfBounds` (spec 10.4.5.13): detached, or the view's
/// `[byteOffset, byteOffset + byteLength)` range no longer fits the buffer.
pub fn ta_out_of_bounds(t: &TypedArrayData) -> bool {
    let buf_len = match &t.buffer.borrow().internal {
        Internal::ArrayBuffer(Some(bytes)) => bytes.len(),
        _ => return true, // detached
    };
    if t.byte_offset > buf_len {
        return true;
    }
    if !t.length_tracking && t.byte_offset + t.length * t.kind.bytes() > buf_len {
        return true;
    }
    false
}

impl Vm {
    /// A fresh `ArrayBuffer` of `len` zero bytes.
    pub fn new_array_buffer(&self, len: usize) -> JsObject {
        self.alloc(ObjectData::new(
            Some(self.realm.array_buffer_proto.clone()),
            Internal::ArrayBuffer(Some(vec![0u8; len])),
        ))
    }

    /// `IsSharedArrayBuffer(O)`: true when the buffer carries the engine-private
    /// shared brand. (A SharedArrayBuffer shares the `Internal::ArrayBuffer`
    /// byte storage; only the brand and prototype distinguish it.)
    pub fn is_shared_buffer(&self, o: &JsObject) -> bool {
        o.borrow().own_contains_key(&PropertyKey::Sym(
            self.realm.symbol_array_buffer_shared.clone(),
        ))
    }

    /// A fresh `SharedArrayBuffer` of `len` zero bytes — an `Internal::ArrayBuffer`
    /// stamped with the shared brand and (when `max` is set) the growable-max
    /// internal slot. Both live in `props` but are hidden by `own_keys`.
    pub fn new_shared_array_buffer(&self, len: usize, max: Option<usize>) -> JsObject {
        let o = self.alloc(ObjectData::new(
            Some(self.realm.shared_array_buffer_proto.clone()),
            Internal::ArrayBuffer(Some(vec![0u8; len])),
        ));
        {
            let mut b = o.borrow_mut();
            let slot = |value: Value| Property {
                kind: PropertyKind::Data {
                    value,
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            };
            b.own_insert(
                PropertyKey::Sym(self.realm.symbol_array_buffer_shared.clone()),
                slot(Value::Bool(true)),
            );
            if let Some(m) = max {
                b.own_insert(
                    PropertyKey::str(ARRAY_BUFFER_MAX_SLOT),
                    slot(Value::Number(m as f64)),
                );
            }
        }
        o
    }

    /// A typed array view. `proto` is the per-kind prototype.
    pub fn new_typed_array(
        &self,
        kind: TAKind,
        buffer: JsObject,
        byte_offset: usize,
        length: usize,
        proto: JsObject,
    ) -> JsObject {
        self.alloc(ObjectData::new(
            Some(proto),
            Internal::TypedArray(TypedArrayData {
                buffer,
                byte_offset,
                length,
                kind,
                length_tracking: false,
            }),
        ))
    }

    /// Read element `i` of a typed array as a JS value (`undefined` if the index
    /// is out of range or the buffer is detached).
    pub fn ta_get(&self, obj: &JsObject, i: usize) -> Value {
        let b = obj.borrow();
        if let Internal::TypedArray(t) = &b.internal {
            if i >= ta_eff_length(t) {
                return Value::Undefined;
            }
            let off = t.byte_offset + i * t.kind.bytes();
            let buf = t.buffer.borrow();
            if let Internal::ArrayBuffer(Some(bytes)) = &buf.internal {
                if t.kind.is_bigint() {
                    return Value::bigint(decode_big(bytes, off, t.kind));
                }
                return Value::Number(decode(bytes, off, t.kind));
            }
        }
        Value::Undefined
    }

    /// Write `v` (already a JS number coerced by the caller) to element `i`.
    /// Out-of-range writes are silently ignored (per spec for integer indices).
    /// For BigInt-kind arrays the caller must instead use `ta_write` (a number
    /// here is a no-op).
    pub fn ta_set(&self, obj: &JsObject, i: usize, v: f64) {
        let b = obj.borrow();
        if let Internal::TypedArray(t) = &b.internal {
            if i >= ta_eff_length(t) || t.kind.is_bigint() {
                return;
            }
            let off = t.byte_offset + i * t.kind.bytes();
            let kind = t.kind;
            let mut buf = t.buffer.borrow_mut();
            if let Internal::ArrayBuffer(Some(bytes)) = &mut buf.internal {
                encode(bytes, off, kind, v);
            }
        }
    }

    /// Coercion-aware element write: ToBigInt for BigInt kinds (a Number throws),
    /// ToNumber otherwise. Out-of-range indices are silently ignored.
    pub fn ta_write(&mut self, obj: &JsObject, i: usize, v: &Value) -> Result<(), Value> {
        let kind = match &obj.borrow().internal {
            Internal::TypedArray(t) => t.kind,
            _ => return Ok(()),
        };
        if kind.is_bigint() {
            // ToBigInt must run (and may throw) even when the index is out of range.
            let n = self.to_bigint(v)?;
            let b = obj.borrow();
            if let Internal::TypedArray(t) = &b.internal {
                if i < ta_eff_length(t) {
                    let off = t.byte_offset + i * t.kind.bytes();
                    let mut buf = t.buffer.borrow_mut();
                    if let Internal::ArrayBuffer(Some(bytes)) = &mut buf.internal {
                        encode_big(bytes, off, kind, &n);
                    }
                }
            }
        } else {
            let n = self.to_number(v)?;
            self.ta_set(obj, i, n);
        }
        Ok(())
    }

    pub fn ta_length(&self, obj: &JsObject) -> Option<usize> {
        match &obj.borrow().internal {
            Internal::TypedArray(t) => Some(ta_eff_length(t)),
            _ => None,
        }
    }

    pub fn ta_kind(&self, obj: &JsObject) -> Option<TAKind> {
        match &obj.borrow().internal {
            Internal::TypedArray(t) => Some(t.kind),
            _ => None,
        }
    }

    pub fn is_typed_array(&self, v: &Value) -> bool {
        matches!(v, Value::Object(o) if matches!(o.borrow().internal, Internal::TypedArray(_)))
    }

    /// `IsValidIntegerIndex(O, index)` (spec 10.4.5.14): the integer-indexed
    /// exotic [[Get]]/[[Set]]/[[Has]]/[[DefineOwnProperty]] gate. False for a
    /// detached buffer, a non-integral index, `-0`, or an out-of-range index.
    pub fn ta_valid_index(&self, obj: &JsObject, index: f64) -> bool {
        let b = obj.borrow();
        let t = match &b.internal {
            Internal::TypedArray(t) => t,
            _ => return false,
        };
        if matches!(t.buffer.borrow().internal, Internal::ArrayBuffer(None)) {
            return false; // detached
        }
        if !index.is_finite() || index.fract() != 0.0 {
            return false;
        }
        if index == 0.0 && index.is_sign_negative() {
            return false; // -0
        }
        let len = ta_eff_length(t);
        index >= 0.0 && (index as usize) < len
    }
}
