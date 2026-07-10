//! The `BigInt` constructor, its prototype, and the `asIntN`/`asUintN` statics.
//!
//! BigInt values are an engine primitive (`Value::BigInt`); the wrapper object
//! form (`Object(BigInt)`) carries an `Internal::BigIntObj` slot so prototype
//! methods can recover the underlying value via `this_bigint`.

use super::arg;
use crate::value::*;
use crate::vm::{Hint, Vm};
use num_bigint::BigInt;
use num_traits::Signed;

pub fn install(vm: &mut Vm) {
    let proto = vm.realm.bigint_proto.clone();

    // BigInt is callable but NOT a constructor: `new BigInt(...)` throws.
    let ctor = vm.new_native_ctor(
        "BigInt",
        1,
        |vm, _t, args| {
            let v = arg(args, 0);
            Ok(Value::bigint(to_bigint_ctor(vm, &v)?))
        },
        |vm, _t, _args| Err(vm.throw_type("BigInt is not a constructor")),
    );
    vm.install_ctor("BigInt", &ctor, &proto);

    // BigInt.asIntN(bits, bigint) / BigInt.asUintN(bits, bigint).
    vm.define_method(&ctor, "asIntN", 2, |vm, _t, args| {
        let bits = to_index(vm, &arg(args, 0))?;
        let v = vm.to_bigint(&arg(args, 1))?;
        Ok(Value::bigint(as_int_n(bits, v, true)))
    });
    vm.define_method(&ctor, "asUintN", 2, |vm, _t, args| {
        let bits = to_index(vm, &arg(args, 0))?;
        let v = vm.to_bigint(&arg(args, 1))?;
        Ok(Value::bigint(as_int_n(bits, v, false)))
    });

    // BigInt.prototype.toString(radix?).
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let n = this_bigint(vm, &this)?;
        let r = arg(args, 0);
        let radix = if r.is_undefined() {
            10u32
        } else {
            let rn = vm.to_number(&r)?;
            if !(2.0..=36.0).contains(&rn) {
                return Err(vm.throw_range("toString() radix must be between 2 and 36"));
            }
            rn as u32
        };
        Ok(Value::str(n.to_str_radix(radix)))
    });

    // No Intl: toLocaleString defers to a base-10 toString.
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let n = this_bigint(vm, &this)?;
        Ok(Value::str(n.to_string()))
    });

    vm.define_method(&proto, "valueOf", 0, |vm, this, _a| {
        Ok(Value::bigint(this_bigint(vm, &this)?))
    });

    // BigInt.prototype[Symbol.toStringTag] = "BigInt" (non-writable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("BigInt"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
}

/// `thisBigIntValue(this)`: the receiver is a BigInt primitive or a wrapper
/// object carrying `Internal::BigIntObj`.
fn this_bigint(vm: &mut Vm, this: &Value) -> Result<BigInt, Value> {
    match this {
        Value::BigInt(n) => Ok((**n).clone()),
        Value::Object(o) => {
            if let Internal::BigIntObj(n) = &o.borrow().internal {
                Ok((**n).clone())
            } else {
                Err(vm.throw_type("BigInt.prototype method called on non-BigInt"))
            }
        }
        _ => Err(vm.throw_type("BigInt.prototype method called on non-BigInt")),
    }
}

/// The `BigInt(value)` function: like `ToBigInt`, but a Number is converted via
/// `NumberToBigInt` (integer required, else RangeError).
fn to_bigint_ctor(vm: &mut Vm, v: &Value) -> Result<BigInt, Value> {
    let p = vm.to_primitive(v, Hint::Number)?;
    if let Value::Number(n) = p {
        return number_to_bigint(vm, n);
    }
    vm.to_bigint(&p)
}

fn number_to_bigint(vm: &mut Vm, n: f64) -> Result<BigInt, Value> {
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(
            vm.throw_range("The number is not a safe integer and cannot be converted to a BigInt")
        );
    }
    <BigInt as num_traits::FromPrimitive>::from_f64(n)
        .ok_or_else(|| vm.throw_range("Cannot convert number to a BigInt"))
}

/// `ToIndex`: a non-negative integer in `[0, 2^53-1]`. Bounded further here to
/// keep `asIntN`/`asUintN` from allocating an unbounded modulus.
fn to_index(vm: &mut Vm, v: &Value) -> Result<u64, Value> {
    let n = vm.to_number(v)?;
    let i = if n.is_nan() { 0.0 } else { n.trunc() };
    if !(0.0..=9007199254740991.0).contains(&i) {
        return Err(vm.throw_range("Index out of range"));
    }
    let bits = i as u64;
    if bits > (1 << 20) {
        return Err(vm.throw_range("asIntN/asUintN bit width too large"));
    }
    Ok(bits)
}

/// BigInt.asIntN/asUintN core: reduce `v` modulo `2^bits`, optionally
/// re-interpreting the top bit as a sign for the signed (`asIntN`) form.
fn as_int_n(bits: u64, v: BigInt, signed: bool) -> BigInt {
    if bits == 0 {
        return BigInt::from(0);
    }
    let bits = bits as usize;
    let modulus = BigInt::from(1) << bits;
    // Euclidean remainder in [0, modulus).
    let mut r = v % &modulus;
    if r.is_negative() {
        r += &modulus;
    }
    if signed {
        let half = BigInt::from(1) << (bits - 1);
        if r >= half {
            r -= &modulus;
        }
    }
    r
}
