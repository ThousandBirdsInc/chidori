//! Number, Math, and JSON.

use super::arg;
use crate::value::*;
use crate::vm::{number_to_string, Vm};

pub fn install(vm: &mut Vm) {
    install_number(vm);
    install_math(vm);
    install_json(vm);

    install_number_extra(vm);
}

/// `Number(value)` coercion: ToNumeric, then map a resulting BigInt to its
/// nearest Number (unlike ToNumber, which throws on BigInt).
fn number_from_value(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    match vm.to_numeric(v)? {
        Value::Number(n) => Ok(n),
        Value::BigInt(b) => Ok(num_traits::ToPrimitive::to_f64(b.as_ref()).unwrap_or(f64::NAN)),
        _ => Ok(f64::NAN),
    }
}

fn num_this(vm: &mut Vm, this: &Value) -> Result<f64, Value> {
    match this {
        Value::Number(n) => Ok(*n),
        Value::Object(o) => {
            if let Internal::Number(n) = &o.borrow().internal {
                Ok(*n)
            } else {
                Err(vm.throw_type("Number.prototype method called on non-number"))
            }
        }
        _ => Err(vm.throw_type("Number.prototype method called on non-number")),
    }
}

/// `ToIntegerOrInfinity` restricted enough for the digit arguments: NaN -> 0,
/// otherwise truncate toward zero (infinities pass through).
fn to_integer_or_infinity(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    let n = vm.to_number(v)?;
    Ok(if n.is_nan() {
        0.0
    } else if n.is_infinite() {
        n
    } else {
        n.trunc()
    })
}

fn install_number(vm: &mut Vm) {
    let proto = vm.realm.number_proto.clone();
    proto.borrow_mut().internal = Internal::Number(0.0);

    let ctor = vm.new_native_ctor(
        "Number",
        1,
        |vm, _t, args| {
            if args.is_empty() {
                Ok(Value::Number(0.0))
            } else {
                Ok(Value::Number(number_from_value(vm, &arg(args, 0))?))
            }
        },
        |vm, _t, args| {
            let n = if args.is_empty() {
                0.0
            } else {
                number_from_value(vm, &arg(args, 0))?
            };
            Ok(Value::Object(vm.alloc(ObjectData::new(
                Some(vm.realm.number_proto.clone()),
                Internal::Number(n),
            ))))
        },
    );
    vm.install_ctor("Number", &ctor, &proto);

    vm.define_constant(&ctor, "MAX_SAFE_INTEGER", Value::Number(9007199254740991.0));
    vm.define_constant(
        &ctor,
        "MIN_SAFE_INTEGER",
        Value::Number(-9007199254740991.0),
    );
    vm.define_constant(&ctor, "MAX_VALUE", Value::Number(f64::MAX));
    vm.define_constant(&ctor, "MIN_VALUE", Value::Number(5e-324));
    vm.define_constant(&ctor, "EPSILON", Value::Number(f64::EPSILON));
    vm.define_constant(&ctor, "POSITIVE_INFINITY", Value::Number(f64::INFINITY));
    vm.define_constant(&ctor, "NEGATIVE_INFINITY", Value::Number(f64::NEG_INFINITY));
    vm.define_constant(&ctor, "NaN", Value::Number(f64::NAN));

    vm.define_method(&ctor, "isInteger", 1, |_vm, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Number(n) if n.is_finite() && n.fract() == 0.0),
        ))
    });
    vm.define_method(&ctor, "isSafeInteger", 1, |_vm, _t, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Number(n)
            if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9007199254740991.0)))
    });
    vm.define_method(&ctor, "isFinite", 1, |_vm, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Number(n) if n.is_finite()),
        ))
    });
    vm.define_method(&ctor, "isNaN", 1, |_vm, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Number(n) if n.is_nan()),
        ))
    });
    vm.define_method(&ctor, "parseFloat", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        Ok(Value::Number(super::parse_float(s.as_str())))
    });
    vm.define_method(&ctor, "parseInt", 2, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        let radix = {
            let r = arg(args, 1);
            if r.is_undefined() {
                0
            } else {
                vm.to_int32(&r)?
            }
        };
        Ok(Value::Number(super::parse_int(s.as_str(), radix)))
    });

    vm.define_method(&proto, "valueOf", 0, |vm, this, _a| {
        Ok(Value::Number(num_this(vm, &this)?))
    });
    vm.define_method(&proto, "toString", 1, |vm, this, args| {
        let n = num_this(vm, &this)?;
        let r = arg(args, 0);
        // The radix argument is coerced *after* the receiver is validated.
        let radix = if r.is_undefined() {
            10.0
        } else {
            to_integer_or_infinity(vm, &r)?
        };
        if !(2.0..=36.0).contains(&radix) {
            return Err(vm.throw_range("toString() radix must be an integer between 2 and 36"));
        }
        let radix = radix as u32;
        if radix == 10 {
            Ok(Value::str(number_to_string(n)))
        } else {
            Ok(Value::str(number_to_radix(n, radix)))
        }
    });
    vm.define_method(&proto, "toFixed", 1, |vm, this, args| {
        let n = num_this(vm, &this)?;
        let f = to_integer_or_infinity(vm, &arg(args, 0))?;
        if !(0.0..=100.0).contains(&f) {
            return Err(vm.throw_range("toFixed() digits argument must be between 0 and 100"));
        }
        let f = f as usize;
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if !n.is_finite() {
            return Ok(Value::str(number_to_string(n)));
        }
        if n.abs() >= 1e21 {
            return Ok(Value::str(number_to_string(n)));
        }
        Ok(Value::str(to_fixed_string(n, f)))
    });
}

// ---- additional Number.prototype methods (installed below) ----

fn install_number_extra(vm: &mut Vm) {
    let proto = vm.realm.number_proto.clone();

    // Number.prototype.toLocaleString — without an Intl implementation this is
    // spec-permitted to behave like toString.
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _args| {
        let n = num_this(vm, &this)?;
        Ok(Value::str(number_to_string(n)))
    });

    // Number.prototype.toExponential(fractionDigits?).
    vm.define_method(&proto, "toExponential", 1, |vm, this, args| {
        let n = num_this(vm, &this)?;
        let f_arg = arg(args, 0);
        // ToIntegerOrInfinity on the argument first (observable side effects),
        // then NaN/Infinity short-circuits per spec ordering.
        let f_undef = f_arg.is_undefined();
        let f = if f_undef {
            0.0
        } else {
            to_integer_or_infinity(vm, &f_arg)?
        };
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if !n.is_finite() {
            return Ok(Value::str(number_to_string(n)));
        }
        if !f_undef && !(0.0..=100.0).contains(&f) {
            return Err(vm.throw_range("toExponential() argument must be between 0 and 100"));
        }
        Ok(Value::str(to_exponential_string(
            n,
            if f_undef { None } else { Some(f as usize) },
        )))
    });

    // Number.prototype.toPrecision(precision?) — corrected significant-digit
    // formatting. Overrides the earlier (fixed-point) definition because
    // define_method inserts by name into the prototype's property map.
    vm.define_method(&proto, "toPrecision", 1, |vm, this, args| {
        let n = num_this(vm, &this)?;
        let p = arg(args, 0);
        if p.is_undefined() {
            return Ok(Value::str(number_to_string(n)));
        }
        let prec = to_integer_or_infinity(vm, &p)?;
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if !n.is_finite() {
            return Ok(Value::str(number_to_string(n)));
        }
        if !(1.0..=100.0).contains(&prec) {
            return Err(vm.throw_range("toPrecision() argument must be between 1 and 100"));
        }
        Ok(Value::str(to_precision_string(n, prec as usize)))
    });
}

/// Convert Rust's exponential formatting (`"1.5e2"`, `"1e0"`, `"5e-3"`) into the
/// JavaScript form (`"1.5e+2"`, `"1e+0"`, `"5e-3"`).
fn normalize_exponential(s: &str) -> String {
    if let Some(idx) = s.find(['e', 'E']) {
        let (mantissa, rest) = s.split_at(idx);
        let exp = &rest[1..]; // skip the 'e'/'E'
        let (sign, digits) = if let Some(d) = exp.strip_prefix('-') {
            ('-', d)
        } else if let Some(d) = exp.strip_prefix('+') {
            ('+', d)
        } else {
            ('+', exp)
        };
        format!("{mantissa}e{sign}{digits}")
    } else {
        s.to_string()
    }
}

/// Round a positive value to a string with exactly `f` fractional digits, using
/// round-half-away-from-zero (the spec selects the larger `n` on ties).
fn to_fixed_string(n: f64, f: usize) -> String {
    let neg = n < 0.0;
    let x = n.abs();
    // Produce a decimal string with `f` fractional digits. We round the exact
    // value: format with extra precision then round the decimal manually to
    // avoid Rust's round-half-to-even.
    let body = round_decimal(x, f);
    // A negative value whose rounded magnitude is zero prints without a sign
    // (e.g. `(-0).toFixed(2) === "0.00"`).
    let is_zero = body.bytes().all(|c| c == b'0' || c == b'.');
    if neg && !is_zero {
        format!("-{body}")
    } else {
        body
    }
}

/// Format positive `x` with exactly `frac` digits after the decimal point,
/// rounding half away from zero (the spec selects the larger candidate on ties).
fn round_decimal(x: f64, frac: usize) -> String {
    // Format with extra guard digits so the produced string is the *exact*
    // shortest decimal of the f64 extended with trailing zeros (Rust's float
    // formatter is exact for the full expansion). With the true tail available
    // we can apply round-half-away ourselves without double-rounding through
    // Rust's round-half-to-even at the cut position.
    const GUARD: usize = 25;
    let extra = format!("{x:.*}", frac + GUARD);
    let (int_part, frac_part) = match extra.split_once('.') {
        Some((i, fp)) => (i.to_string(), fp.to_string()),
        None => (extra.clone(), String::new()),
    };
    let int_len = int_part.len();
    let mut digits: Vec<u8> = int_part
        .bytes()
        .chain(frac_part.bytes())
        .map(|b| b - b'0')
        .collect();
    // Position of the first dropped digit (the guard digit).
    let round_pos = int_len + frac;
    let round_up = if round_pos < digits.len() {
        let guard = digits[round_pos];
        if guard > 5 {
            true
        } else if guard < 5 {
            false
        } else {
            // Tie at the guard digit: round away from zero (always up here since
            // we already work with the magnitude). Any nonzero tail also rounds
            // up; an exact .5 rounds up by the spec's "larger candidate" rule.
            true
        }
    } else {
        false
    };
    // Truncate to the kept positions.
    digits.truncate(round_pos);
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    // A carry may have grown the integer portion by one digit.
    let new_int_len = digits.len() - frac;
    let int_digits: String = digits[..new_int_len]
        .iter()
        .map(|d| (d + b'0') as char)
        .collect();
    let int_digits = if int_digits.is_empty() {
        "0".to_string()
    } else {
        int_digits
    };
    if frac == 0 {
        int_digits
    } else {
        let frac_digits: String = digits[new_int_len..]
            .iter()
            .map(|d| (d + b'0') as char)
            .collect();
        format!("{int_digits}.{frac_digits}")
    }
}

/// Number::toExponential. `n` is finite. `f` is `Some` for a fixed fraction
/// count, `None` to use as many digits as needed.
fn to_exponential_string(n: f64, f: Option<usize>) -> String {
    if n == 0.0 {
        return match f {
            None | Some(0) => "0e+0".to_string(),
            Some(d) => format!("0.{}e+0", "0".repeat(d)),
        };
    }
    let neg = n < 0.0;
    let x = n.abs();
    let body = match f {
        None => {
            let s = format!("{x:e}");
            normalize_exponential(&s)
        }
        Some(d) => {
            let s = format!("{x:.*e}", d);
            normalize_exponential(&s)
        }
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// ECMAScript Number::toString-style fixed-precision (significant digits).
/// `prec` is in `[1, 100]`; `n` must be finite.
fn to_precision_string(n: f64, prec: usize) -> String {
    if n == 0.0 {
        return if prec == 1 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat(prec - 1))
        };
    }
    let neg = n < 0.0;
    let x = n.abs();
    // Round to `prec` significant digits via exponential formatting:
    // `d.dd...e±E` with exactly `prec` significant digits.
    let formatted = format!("{x:.*e}", prec - 1);
    let (mantissa, exp_part) = match formatted.split_once('e') {
        Some(parts) => parts,
        None => return number_to_string(n),
    };
    let e: i32 = exp_part.parse().unwrap_or(0);
    // Collect the `prec` significant digits (drop the decimal point).
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let body = if e < -6 || e >= prec as i32 {
        // Exponential form: d1 "." d2..dp "e" sign |e|.
        let mut m = String::new();
        m.push_str(&digits[..1]);
        if prec > 1 {
            m.push('.');
            m.push_str(&digits[1..]);
        }
        let sign = if e >= 0 { '+' } else { '-' };
        format!("{m}e{sign}{}", e.abs())
    } else if e == prec as i32 - 1 {
        // Integer with exactly `prec` digits.
        digits
    } else if e >= 0 {
        // Decimal point after `e + 1` digits.
        let split = (e + 1) as usize;
        format!("{}.{}", &digits[..split], &digits[split..])
    } else {
        // 0. then -(e+1) leading zeros then all `prec` digits.
        let zeros = (-(e + 1)) as usize;
        format!("0.{}{}", "0".repeat(zeros), digits)
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

fn number_to_radix(n: f64, radix: u32) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let neg = n < 0.0;
    let x = n.abs();
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";

    // Integer part (use f64 arithmetic so we don't truncate huge magnitudes to
    // a u64 incorrectly; build digits from the most-significant end is harder,
    // so accumulate least-significant first then reverse).
    let mut int_val = x.trunc();
    let mut int_str = Vec::new();
    if int_val == 0.0 {
        int_str.push(b'0');
    }
    let r = radix as f64;
    while int_val >= 1.0 {
        let digit = (int_val % r) as usize;
        int_str.push(digits[digit]);
        int_val = (int_val / r).trunc();
    }
    int_str.reverse();
    let mut s = String::from_utf8(int_str).unwrap();

    let mut frac = x.fract();
    if frac > 0.0 {
        s.push('.');
        let mut count = 0;
        // 1100 binary digits is enough to uniquely represent any f64 fraction;
        // cap conservatively to avoid runaway loops on irrational-in-radix
        // fractions while matching typical engine output length.
        while frac > 0.0 && count < 1100 {
            frac *= r;
            let d = frac.trunc() as usize;
            s.push(digits[d.min(35)] as char);
            frac -= d as f64;
            count += 1;
        }
    }
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

fn install_math(vm: &mut Vm) {
    let math = vm.new_object();
    // Math value properties: non-writable, non-enumerable, non-configurable.
    vm.define_constant(&math, "PI", Value::Number(std::f64::consts::PI));
    vm.define_constant(&math, "E", Value::Number(std::f64::consts::E));
    vm.define_constant(&math, "LN2", Value::Number(std::f64::consts::LN_2));
    vm.define_constant(&math, "LN10", Value::Number(std::f64::consts::LN_10));
    vm.define_constant(&math, "LOG2E", Value::Number(std::f64::consts::LOG2_E));
    vm.define_constant(&math, "LOG10E", Value::Number(std::f64::consts::LOG10_E));
    vm.define_constant(&math, "SQRT2", Value::Number(std::f64::consts::SQRT_2));
    vm.define_constant(
        &math,
        "SQRT1_2",
        Value::Number(std::f64::consts::FRAC_1_SQRT_2),
    );

    macro_rules! unary {
        ($name:expr, $f:expr) => {
            vm.define_method(&math, $name, 1, move |vm, _t, args| {
                let n = vm.to_number(&arg(args, 0))?;
                Ok(Value::Number($f(n)))
            });
        };
    }
    unary!("abs", f64::abs);
    unary!("floor", f64::floor);
    unary!("ceil", f64::ceil);
    unary!("round", math_round);
    unary!("trunc", f64::trunc);
    unary!("sign", f64::signum_js);
    unary!("sqrt", f64::sqrt);
    unary!("cbrt", f64::cbrt);
    unary!("exp", f64::exp);
    unary!("expm1", f64::exp_m1);
    unary!("log", math_log);
    unary!("log2", math_log2);
    unary!("log10", math_log10);
    unary!("log1p", math_log1p);
    unary!("sin", f64::sin);
    unary!("cos", f64::cos);
    unary!("tan", f64::tan);
    unary!("asin", f64::asin);
    unary!("acos", f64::acos);
    unary!("atan", f64::atan);
    unary!("sinh", f64::sinh);
    unary!("cosh", f64::cosh);
    unary!("tanh", f64::tanh);
    unary!("asinh", f64::asinh);
    unary!("acosh", f64::acosh);
    unary!("atanh", f64::atanh);
    unary!("fround", |n: f64| {
        if n.is_nan() {
            f64::NAN
        } else {
            n as f32 as f64
        }
    });
    // Math.f16round (ES2025): round to IEEE-754 binary16 and widen back.
    unary!("f16round", |n: f64| {
        if n.is_nan() {
            f64::NAN
        } else {
            super::typedarray::f16_to_f64(super::typedarray::f16_from_f64(n))
        }
    });
    unary!("clz32", |n: f64| {
        let u = crate::vm::to_uint32(n);
        u.leading_zeros() as f64
    });

    // Math.imul(a, b): 32-bit integer multiplication.
    vm.define_method(&math, "imul", 2, |vm, _t, args| {
        let a = vm.to_int32(&arg(args, 0))?;
        let b = vm.to_int32(&arg(args, 1))?;
        Ok(Value::Number(a.wrapping_mul(b) as f64))
    });

    vm.define_method(&math, "pow", 2, |vm, _t, args| {
        let a = vm.to_number(&arg(args, 0))?;
        let b = vm.to_number(&arg(args, 1))?;
        Ok(Value::Number(math_pow(a, b)))
    });
    vm.define_method(&math, "atan2", 2, |vm, _t, args| {
        let a = vm.to_number(&arg(args, 0))?;
        let b = vm.to_number(&arg(args, 1))?;
        Ok(Value::Number(a.atan2(b)))
    });
    vm.define_method(&math, "hypot", 2, |vm, _t, args| {
        // Coerce all arguments first (spec: ToNumber each in order). NaN/Infinity
        // handling: if any is ±Infinity the result is +Infinity (even if another
        // is NaN); otherwise if any is NaN the result is NaN.
        let mut nums = Vec::with_capacity(args.len());
        for a in args {
            nums.push(vm.to_number(a)?);
        }
        if nums.iter().any(|n| n.is_infinite()) {
            return Ok(Value::Number(f64::INFINITY));
        }
        if nums.iter().any(|n| n.is_nan()) {
            return Ok(Value::Number(f64::NAN));
        }
        let mut sum = 0.0_f64;
        for n in nums {
            sum += n * n;
        }
        Ok(Value::Number(sum.sqrt()))
    });
    vm.define_method(&math, "max", 2, |vm, _t, args| {
        // Coerce all first, then reduce.
        let mut nums = Vec::with_capacity(args.len());
        for a in args {
            nums.push(vm.to_number(a)?);
        }
        let mut m = f64::NEG_INFINITY;
        for n in nums {
            if n.is_nan() {
                return Ok(Value::Number(f64::NAN));
            }
            // +0 is considered larger than -0.
            if n > m || (n == 0.0 && m == 0.0 && n.is_sign_positive()) {
                m = n;
            }
        }
        Ok(Value::Number(m))
    });
    vm.define_method(&math, "min", 2, |vm, _t, args| {
        let mut nums = Vec::with_capacity(args.len());
        for a in args {
            nums.push(vm.to_number(a)?);
        }
        let mut m = f64::INFINITY;
        for n in nums {
            if n.is_nan() {
                return Ok(Value::Number(f64::NAN));
            }
            // -0 is considered smaller than +0.
            if n < m || (n == 0.0 && m == 0.0 && n.is_sign_negative()) {
                m = n;
            }
        }
        Ok(Value::Number(m))
    });
    // Math.random is a host effect under replay; here it is deterministic-by-host.
    // Until a host clock/RNG is installed, fall back to a fixed sequence so pure
    // engine tests stay reproducible. The replay layer overrides this.
    vm.define_method(&math, "random", 0, |vm, _t, _a| {
        vm.rng_state = vm
            .rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bits = (vm.rng_state >> 11) as f64;
        Ok(Value::Number(bits / (1u64 << 53) as f64))
    });

    // Math.sumPrecise(iterable) (ES2026 proposal): the exactly-rounded sum of
    // an iterable of Numbers (Shewchuk's non-overlapping partials). The empty
    // sum is -0; any non-Number element is a TypeError that closes the
    // iterator; mixed infinities (or any NaN) give NaN.
    vm.define_method(&math, "sumPrecise", 1, |vm, _t, args| {
        let iterable = arg(args, 0);
        if iterable.is_nullish() {
            return Err(vm.throw_type("Math.sumPrecise requires an iterable"));
        }
        let it = vm.get_iterator(&iterable)?;
        let mut partials: Vec<f64> = Vec::new();
        let mut all_minus_zero = true;
        let mut nan = false;
        let mut pos_inf = false;
        let mut neg_inf = false;
        loop {
            let v = match vm.iterator_step(&it)? {
                Some(v) => v,
                None => break,
            };
            let n = match v {
                Value::Number(n) => n,
                _ => {
                    let _ = vm.iterator_close(&it);
                    return Err(vm.throw_type("Math.sumPrecise: every value must be a Number"));
                }
            };
            vm.native_tick()?;
            all_minus_zero = all_minus_zero && n == 0.0 && n.is_sign_negative();
            if n.is_nan() {
                nan = true;
                continue;
            }
            if n == f64::INFINITY {
                pos_inf = true;
                continue;
            }
            if n == f64::NEG_INFINITY {
                neg_inf = true;
                continue;
            }
            // Two-sum accumulation into non-overlapping partials.
            let mut x = n;
            let mut keep = 0usize;
            for j in 0..partials.len() {
                let mut y = partials[j];
                if x.abs() < y.abs() {
                    std::mem::swap(&mut x, &mut y);
                }
                let hi = x + y;
                let lo = y - (hi - x);
                if lo != 0.0 {
                    partials[keep] = lo;
                    keep += 1;
                }
                x = hi;
            }
            partials.truncate(keep);
            partials.push(x);
        }
        if nan || (pos_inf && neg_inf) {
            return Ok(Value::Number(f64::NAN));
        }
        if pos_inf {
            return Ok(Value::Number(f64::INFINITY));
        }
        if neg_inf {
            return Ok(Value::Number(f64::NEG_INFINITY));
        }
        if partials.is_empty() {
            return Ok(Value::Number(if all_minus_zero { -0.0 } else { 0.0 }));
        }
        Ok(Value::Number(partials.iter().sum()))
    });

    // Math[Symbol.toStringTag] = "Math" (non-writable, non-enumerable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    math.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Math"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    // Register the canonical Math object and the kernel-supported methods so
    // the typed loop kernels (`kernel.rs`) can identity-check them at entry:
    // a kernel using `Math.max` runs only while the global `Math` binding and
    // the method are still these exact objects (methods are writable).
    vm.realm.math_object = Some(math.clone());
    vm.realm.math_kernel = crate::bytecode::KMath::ALL
        .iter()
        .map(|k| {
            match math
                .borrow()
                .props
                .get(&PropertyKey::str(k.name()))
                .and_then(|p| p.value().cloned())
            {
                Some(Value::Object(o)) => o,
                _ => unreachable!("Math.{} installed above", k.name()),
            }
        })
        .collect();
    vm.define_value(&vm.realm.global.clone(), "Math", Value::Object(math));
}

/// Math.round: round half toward +Infinity, preserving the sign of zero and the
/// edge case where the result is in `(-1, -0]` (e.g. `round(-0.5) === -0`).
/// The 2-argument folds of `Math.max`/`Math.min` — EXACTLY the builtin
/// closures' logic over two elements, shared with the typed loop kernels so
/// kernel results are bit-identical (NaN poisoning, +0 beats -0, etc.).
pub(crate) fn math_max2(a: f64, b: f64) -> f64 {
    let mut m = f64::NEG_INFINITY;
    for n in [a, b] {
        if n.is_nan() {
            return f64::NAN;
        }
        if n > m || (n == 0.0 && m == 0.0 && n.is_sign_positive()) {
            m = n;
        }
    }
    m
}

pub(crate) fn math_min2(a: f64, b: f64) -> f64 {
    let mut m = f64::INFINITY;
    for n in [a, b] {
        if n.is_nan() {
            return f64::NAN;
        }
        if n < m || (n == 0.0 && m == 0.0 && n.is_sign_negative()) {
            m = n;
        }
    }
    m
}

pub(crate) fn math_imul2(a: f64, b: f64) -> f64 {
    crate::vm::to_int32(a).wrapping_mul(crate::vm::to_int32(b)) as f64
}

pub(crate) fn math_sign(n: f64) -> f64 {
    f64::signum_js(n)
}

pub(crate) fn math_fround(n: f64) -> f64 {
    if n.is_nan() {
        f64::NAN
    } else {
        n as f32 as f64
    }
}

pub(crate) fn math_round(n: f64) -> f64 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return n;
    }
    // (n + 0.5).floor() is correct for positives but mishandles the sign of
    // results that should be -0, and large-magnitude values where adding 0.5
    // is exact anyway.
    if n > 0.0 && n < 0.5 {
        return 0.0;
    }
    if (-0.5..0.0).contains(&n) {
        // -0 result, preserving sign.
        return -0.0;
    }
    let r = (n + 0.5).floor();
    // For very large numbers, n + 0.5 == n, so floor(n) == n which is fine.
    r
}

/// Math.pow with ECMAScript exponentiation special cases that `f64::powf` does
/// not match (notably `pow(1, ±Inf)` and `pow(±1, NaN)` -> NaN).
pub(crate) fn math_pow(base: f64, exp: f64) -> f64 {
    if exp.is_nan() {
        return f64::NAN;
    }
    if exp == 0.0 {
        // pow(x, ±0) is 1 even when x is NaN.
        return 1.0;
    }
    if base.is_nan() {
        return f64::NAN;
    }
    if exp.is_infinite() {
        let ab = base.abs();
        if ab == 1.0 {
            // pow(±1, ±Inf) is NaN.
            return f64::NAN;
        }
        if ab > 1.0 {
            return if exp > 0.0 { f64::INFINITY } else { 0.0 };
        } else {
            return if exp > 0.0 { 0.0 } else { f64::INFINITY };
        }
    }
    base.powf(exp)
}

/// Math.log family: return NaN for arguments outside the real domain rather than
/// relying on platform-specific behavior.
fn math_log(n: f64) -> f64 {
    if n < 0.0 {
        f64::NAN
    } else {
        n.ln()
    }
}
fn math_log2(n: f64) -> f64 {
    if n < 0.0 {
        f64::NAN
    } else {
        n.log2()
    }
}
fn math_log10(n: f64) -> f64 {
    if n < 0.0 {
        f64::NAN
    } else {
        n.log10()
    }
}
fn math_log1p(n: f64) -> f64 {
    if n < -1.0 {
        f64::NAN
    } else {
        n.ln_1p()
    }
}

trait SignumJs {
    fn signum_js(self) -> f64;
}
impl SignumJs for f64 {
    fn signum_js(self) -> f64 {
        if self.is_nan() {
            f64::NAN
        } else if self == 0.0 {
            self // preserves -0/+0
        } else if self > 0.0 {
            1.0
        } else {
            -1.0
        }
    }
}

// ---- JSON ----

fn install_json(vm: &mut Vm) {
    let json = vm.new_object();
    vm.define_method(&json, "parse", 2, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        let mut p = JsonParser {
            bytes: s.as_str().as_bytes(),
            pos: 0,
            src: s.as_str(),
            keys: Default::default(),
        };
        p.skip_ws();
        let v = p.parse_value(vm)?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(vm.throw_syntax("Unexpected non-whitespace character after JSON"));
        }
        // reviver
        let reviver = arg(args, 1);
        if vm.is_callable(&reviver) {
            let holder = vm.new_object();
            holder
                .borrow_mut()
                .props
                .insert(PropertyKey::str(""), Property::data(v));
            return json_revive(vm, &holder, "", &reviver);
        }
        Ok(v)
    });
    vm.define_method(&json, "stringify", 3, |vm, _t, args| {
        let value = arg(args, 0);
        let replacer = arg(args, 1);
        // Build the property allowlist (array replacer) and capture the function
        // replacer if present.
        let (rep_fn, rep_list) = json_build_replacer(vm, &replacer)?;
        let indent = json_indent(vm, &arg(args, 2))?;
        let mut state = StringifyState {
            indent,
            rep_fn,
            rep_list,
            seen: Vec::new(),
        };
        // Wrap with a holder whose "" key is the value, and invoke from there so
        // a top-level toJSON / replacer call sees key "".
        let holder = vm.new_object();
        holder
            .borrow_mut()
            .props
            .insert(PropertyKey::str(""), Property::data(value));
        let mut out = String::with_capacity(128);
        let root_key = JsString::from("");
        if json_stringify(
            vm,
            &Value::Object(holder),
            &root_key,
            "",
            &mut state,
            &mut out,
        )? {
            Ok(Value::str(out))
        } else {
            Ok(Value::Undefined)
        }
    });
    // JSON[Symbol.toStringTag] = "JSON" (non-writable, non-enumerable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    json.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("JSON"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );
    vm.define_value(&vm.realm.global.clone(), "JSON", Value::Object(json));
}

struct StringifyState {
    indent: String,
    rep_fn: Option<Value>,
    rep_list: Option<Vec<JsString>>,
    seen: Vec<usize>,
}

/// Resolve the replacer argument into an optional function and an optional
/// property allowlist (deduplicated, in order).
fn json_build_replacer(
    vm: &mut Vm,
    replacer: &Value,
) -> Result<(Option<Value>, Option<Vec<JsString>>), Value> {
    if vm.is_callable(replacer) {
        return Ok((Some(replacer.clone()), None));
    }
    if let Value::Object(o) = replacer {
        // IsArray pierces proxies (a revoked proxy throws); a proxy array
        // replacer reads its length and elements through [[Get]] traps.
        let is_array = crate::builtins::fundamental::is_array_exotic(vm, o)?;
        if is_array {
            let len_v = vm.get_prop(replacer, &PropertyKey::str("length"))?;
            let len = vm.to_length(&len_v)?;
            let mut list: Vec<JsString> = Vec::new();
            for i in 0..len {
                let el = vm.get_prop(replacer, &PropertyKey::from_index(i as u32))?;
                // A property name is added for String/Number entries (and the
                // boxed forms thereof).
                let name = match &el {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(JsString::from(number_to_string(*n))),
                    Value::Object(obj) => {
                        let cls = obj.borrow().class_name();
                        if cls == "String" || cls == "Number" {
                            Some(vm.to_js_string(&el)?)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(name) = name {
                    if !list.contains(&name) {
                        list.push(name);
                    }
                }
            }
            return Ok((None, Some(list)));
        }
    }
    Ok((None, None))
}

fn json_indent(vm: &mut Vm, v: &Value) -> Result<String, Value> {
    // A boxed Number/String replacer-space unwraps to its primitive.
    let v = if let Value::Object(o) = v {
        let cls = o.borrow().class_name();
        match cls {
            "Number" => Value::Number(vm.to_number(v)?),
            "String" => Value::String(vm.to_js_string(v)?),
            _ => v.clone(),
        }
    } else {
        v.clone()
    };
    Ok(match &v {
        Value::Number(n) => {
            let count = (*n as i64).clamp(0, 10).max(0) as usize;
            " ".repeat(count)
        }
        Value::String(s) => s.as_str().chars().take(10).collect(),
        _ => String::new(),
    })
}

/// Spec `InternalizeJSONProperty`: walk via Get / [[Delete]] /
/// CreateDataProperty — proxy traps fire and their abrupt completions
/// propagate; a CreateDataProperty/Delete that merely FAILS is ignored.
fn json_revive(vm: &mut Vm, holder: &JsObject, key: &str, reviver: &Value) -> Result<Value, Value> {
    let val = vm.get_prop(&Value::Object(holder.clone()), &PropertyKey::str(key))?;
    if let Value::Object(o) = &val {
        // IsArray pierces proxies (a revoked proxy is a TypeError).
        let is_array = super::fundamental::is_array_exotic(vm, o)?;
        if is_array {
            let len_v = vm.get_prop(&val, &PropertyKey::str("length"))?;
            let len = vm.to_length(&len_v)?;
            for i in 0..len {
                let k = i.to_string();
                let pk = PropertyKey::str(&k);
                let new = json_revive(vm, o, &k, reviver)?;
                if new.is_undefined() {
                    vm.delete_prop(&val, &pk)?;
                } else {
                    json_create_data(vm, o, &pk, new)?;
                }
            }
        } else {
            // EnumerableOwnPropertyNames: the proxy ownKeys /
            // getOwnPropertyDescriptor traps are consulted (and may throw).
            let keys: Vec<JsString> = if vm.is_proxy(o) {
                let mut out = Vec::new();
                for k in vm.own_property_keys(o)? {
                    if let PropertyKey::Str(s) = k {
                        let pk = PropertyKey::Str(s.clone());
                        let desc = vm.proxy_get_own_descriptor(o, &pk)?;
                        if matches!(&desc, Value::Object(_)) {
                            let e = vm.get_prop(&desc, &PropertyKey::str("enumerable"))?;
                            if vm.to_boolean(&e) {
                                out.push(s);
                            }
                        }
                    }
                }
                out
            } else {
                vm.enumerable_own_string_keys(o)
            };
            for k in keys {
                let pk = PropertyKey::Str(k.clone());
                let new = json_revive(vm, o, k.as_str(), reviver)?;
                if new.is_undefined() {
                    vm.delete_prop(&val, &pk)?;
                } else {
                    json_create_data(vm, o, &pk, new)?;
                }
            }
        }
    }
    vm.call(
        reviver.clone(),
        Value::Object(holder.clone()),
        &[Value::str(key), val],
    )
}

/// `CreateDataProperty(o, key, v)` for the reviver walk: a definition that
/// returns false is ignored; an abrupt completion propagates.
fn json_create_data(vm: &mut Vm, o: &JsObject, key: &PropertyKey, v: Value) -> Result<(), Value> {
    let desc = vm.new_object();
    let dv = Value::Object(desc);
    vm.set_prop(&dv, &PropertyKey::str("value"), v)?;
    for f in ["writable", "enumerable", "configurable"] {
        vm.set_prop(&dv, &PropertyKey::str(f), Value::Bool(true))?;
    }
    if vm.is_proxy(o) {
        let _ = vm.proxy_define_property(o, key, dv)?;
    } else {
        let d = super::fundamental::to_property_descriptor(vm, &dv)?;
        let _ = super::fundamental::define_own_property(vm, o, key, &d, false)?;
    }
    Ok(())
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    src: &'a str,
    /// Distinct object keys seen so far, interned by SOURCE SLICE: the
    /// dominant JSON shape — an array of same-structure records — repeats
    /// each key once per record, and the naive path paid two allocations
    /// per occurrence (the parsed `String`, then its `Rc<str>` copy inside
    /// `PropertyKey::str`). A hit here is one `Rc` bump. Escaped keys (rare)
    /// take the general string parser uninterned.
    keys: std::collections::HashMap<
        &'a str,
        PropertyKey,
        std::hash::BuildHasherDefault<crate::fxhash::FxHasher>,
    >,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }
    fn parse_value(&mut self, vm: &mut Vm) -> Result<Value, Value> {
        self.skip_ws();
        if self.pos >= self.bytes.len() {
            return Err(vm.throw_syntax("Unexpected end of JSON input"));
        }
        match self.bytes[self.pos] {
            b'{' => self.parse_object(vm),
            b'[' => self.parse_array(vm),
            b'"' => Ok(Value::String(self.parse_string(vm)?)),
            b't' => self.parse_lit(vm, "true", Value::Bool(true)),
            b'f' => self.parse_lit(vm, "false", Value::Bool(false)),
            b'n' => self.parse_lit(vm, "null", Value::Null),
            b'-' | b'0'..=b'9' => self.parse_number(vm),
            _ => Err(vm.throw_syntax("Unexpected token in JSON")),
        }
    }
    fn parse_lit(&mut self, vm: &mut Vm, lit: &str, val: Value) -> Result<Value, Value> {
        if self.src[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(val)
        } else {
            Err(vm.throw_syntax("Unexpected token in JSON"))
        }
    }
    fn parse_number(&mut self, vm: &mut Vm) -> Result<Value, Value> {
        // Validate the JSON number grammar strictly:
        //   -? (0 | [1-9][0-9]*) ( . [0-9]+ )? ( [eE] [+-]? [0-9]+ )?
        let start = self.pos;
        let b = self.bytes;
        let mut i = self.pos;
        let n = b.len();
        if i < n && b[i] == b'-' {
            i += 1;
        }
        // Integer part.
        if i < n && b[i] == b'0' {
            i += 1;
        } else if i < n && (b'1'..=b'9').contains(&b[i]) {
            i += 1;
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            return Err(vm.throw_syntax("Invalid number in JSON"));
        }
        // Fraction.
        if i < n && b[i] == b'.' {
            i += 1;
            if i >= n || !b[i].is_ascii_digit() {
                return Err(vm.throw_syntax("Invalid number in JSON"));
            }
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        // Exponent.
        if i < n && (b[i] == b'e' || b[i] == b'E') {
            i += 1;
            if i < n && (b[i] == b'+' || b[i] == b'-') {
                i += 1;
            }
            if i >= n || !b[i].is_ascii_digit() {
                return Err(vm.throw_syntax("Invalid number in JSON"));
            }
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        self.pos = i;
        self.src[start..i]
            .parse::<f64>()
            .map(Value::Number)
            .map_err(|_| vm.throw_syntax("Invalid number in JSON"))
    }
    fn parse_string(&mut self, vm: &mut Vm) -> Result<JsString, Value> {
        self.pos += 1; // opening quote
                       // Fast path: a string with no escapes and no control characters —
                       // the overwhelmingly common case — is ONE slice copy instead of
                       // per-char pushes. (Multi-byte UTF-8 passes through: continuation
                       // bytes are ≥ 0x80 and match none of the terminators.)
        let start = self.pos;
        let mut i = self.pos;
        while i < self.bytes.len() {
            match self.bytes[i] {
                b'"' => {
                    self.pos = i + 1;
                    // One allocation: straight from the source slice into the
                    // Rc backing (the old String round-trip copied twice).
                    return Ok(JsString::new(&self.src[start..i]));
                }
                b'\\' | 0x00..=0x1f => break,
                _ => i += 1,
            }
        }
        // Slow path: seed with the clean prefix, continue escape-aware.
        let mut s = String::with_capacity((i - start) + 16);
        s.push_str(&self.src[start..i]);
        self.pos = i;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(JsString::from(s));
                }
                b'\\' => {
                    self.pos += 1;
                    if self.pos >= self.bytes.len() {
                        break;
                    }
                    match self.bytes[self.pos] {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let code = self.parse_hex4(vm)?;
                            // Handle surrogate pairs.
                            if (0xD800..=0xDBFF).contains(&code) {
                                // High surrogate: look for a following low surrogate.
                                if self.pos + 2 < self.bytes.len()
                                    && self.bytes[self.pos + 1] == b'\\'
                                    && self.bytes[self.pos + 2] == b'u'
                                {
                                    let save = self.pos;
                                    self.pos += 2; // consume "\u"
                                    let low = self.parse_hex4(vm)?;
                                    if (0xDC00..=0xDFFF).contains(&low) {
                                        let combined =
                                            0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                        s.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                                    } else {
                                        // Not a low surrogate: emit replacement for the
                                        // lone high surrogate and reparse from `save`.
                                        s.push('\u{fffd}');
                                        self.pos = save;
                                    }
                                } else {
                                    s.push('\u{fffd}');
                                }
                            } else if (0xDC00..=0xDFFF).contains(&code) {
                                // Lone low surrogate.
                                s.push('\u{fffd}');
                            } else {
                                s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => return Err(vm.throw_syntax("Invalid escape in JSON")),
                    }
                    self.pos += 1;
                }
                0x00..=0x1f => {
                    // Control characters must be escaped in JSON strings.
                    return Err(vm.throw_syntax("Bad control character in JSON string"));
                }
                _ => {
                    // Multi-byte safe: copy the char.
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    s.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Err(vm.throw_syntax("Unterminated string in JSON"))
    }
    /// Reads the 4 hex digits following a `\u` (with `self.pos` on the `u`).
    /// Leaves `self.pos` on the last hex digit.
    fn parse_hex4(&mut self, vm: &mut Vm) -> Result<u32, Value> {
        if self.pos + 4 >= self.bytes.len() {
            return Err(vm.throw_syntax("Invalid unicode escape in JSON"));
        }
        let hex = &self.src[self.pos + 1..self.pos + 5];
        let code = u32::from_str_radix(hex, 16)
            .map_err(|_| vm.throw_syntax("Invalid unicode escape in JSON"))?;
        self.pos += 4;
        Ok(code)
    }
    fn parse_array(&mut self, vm: &mut Vm) -> Result<Value, Value> {
        self.pos += 1;
        let mut elems = Vec::new();
        self.skip_ws();
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b']' {
            self.pos += 1;
            return Ok(Value::Object(vm.new_array(elems)));
        }
        loop {
            let v = self.parse_value(vm)?;
            elems.push(v);
            self.skip_ws();
            if self.pos >= self.bytes.len() {
                return Err(vm.throw_syntax("Unexpected end of JSON input"));
            }
            match self.bytes[self.pos] {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(vm.throw_syntax("Expected ',' or ']' in JSON array")),
            }
        }
        Ok(Value::Object(vm.new_array(elems)))
    }
    /// An object key: the unescaped fast path is interned by source slice
    /// (see the `keys` field) and, across parses, through the Vm-level
    /// `json_keys` cache (same-shaped documents parsed repeatedly reuse one
    /// `Rc<str>` per distinct key — the poll/roundtrip pattern); anything
    /// needing escape processing falls back to the general string parser.
    /// `self.pos` is on the opening quote.
    fn parse_key(&mut self, vm: &mut Vm) -> Result<PropertyKey, Value> {
        let start = self.pos + 1;
        let mut i = start;
        while i < self.bytes.len() {
            match self.bytes[i] {
                b'"' => {
                    let s = &self.src[start..i];
                    self.pos = i + 1;
                    if let Some(k) = self.keys.get(s) {
                        return Ok(k.clone());
                    }
                    let k = if let Some(k) = vm.json_keys.get(s) {
                        k.clone()
                    } else {
                        let k = PropertyKey::str(s);
                        // Bounded: a pathological stream of distinct keys
                        // resets the cache rather than growing it.
                        if vm.json_keys.len() >= 4096 {
                            vm.json_keys.clear();
                        }
                        vm.json_keys.insert(s.into(), k.clone());
                        k
                    };
                    self.keys.insert(s, k.clone());
                    return Ok(k);
                }
                b'\\' | 0x00..=0x1f => break,
                _ => i += 1,
            }
        }
        Ok(PropertyKey::Str(self.parse_string(vm)?))
    }

    fn parse_object(&mut self, vm: &mut Vm) -> Result<Value, Value> {
        self.pos += 1;
        let obj = vm.new_object();
        self.skip_ws();
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'}' {
            self.pos += 1;
            return Ok(Value::Object(obj));
        }
        loop {
            self.skip_ws();
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'"' {
                return Err(vm.throw_syntax("Expected string key in JSON object"));
            }
            let key = self.parse_key(vm)?;
            self.skip_ws();
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b':' {
                return Err(vm.throw_syntax("Expected ':' in JSON object"));
            }
            self.pos += 1;
            let v = self.parse_value(vm)?;
            {
                let mut b = obj.borrow_mut();
                // One up-front table allocation covers the common ≤8-member
                // object instead of the 0→3→7 growth (two allocs + a rehash).
                if b.props.capacity() == 0 {
                    b.props.reserve(8);
                }
                b.props.insert(key, Property::data(v));
            }
            self.skip_ws();
            if self.pos >= self.bytes.len() {
                return Err(vm.throw_syntax("Unexpected end of JSON input"));
            }
            match self.bytes[self.pos] {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(vm.throw_syntax("Expected ',' or '}' in JSON object")),
            }
        }
        Ok(Value::Object(obj))
    }
}

/// QuoteJSONString, appended to `out`. Only `"`, `\` and control characters
/// need escaping — all single ASCII bytes — so the scan copies maximal
/// clean RUNS (multi-byte UTF-8 included wholesale; every continuation byte
/// is ≥ 0x80 and never matches) instead of pushing char by char.
fn json_quote_into(s: &str, out: &mut String) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut run = 0usize;
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'"' || c == b'\\' || c < 0x20 {
            out.push_str(&s[run..i]);
            match c {
                b'"' => out.push_str("\\\""),
                b'\\' => out.push_str("\\\\"),
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                b'\t' => out.push_str("\\t"),
                0x08 => out.push_str("\\b"),
                0x0c => out.push_str("\\f"),
                c => {
                    use std::fmt::Write;
                    let _ = write!(out, "\\u{:04x}", c);
                }
            }
            run = i + 1;
        }
    }
    out.push_str(&s[run..]);
    out.push('"');
}

/// SerializeJSONProperty: stringify `holder[key]` APPENDED to `out`,
/// applying toJSON and the function replacer (which both observe `key`).
/// Returns `false` when the value is to be omitted (undefined / function /
/// symbol after transforms) — the caller then truncates whatever prefix it
/// wrote. One shared buffer for the whole tree: no per-node Strings, no
/// joins, no format! assembly (the old shape was ~40% of the round-trip
/// profile in allocator + fmt machinery). Every spec-visible effect
/// ([[Get]] order, toJSON/replacer calls, proxy traps) is unchanged.
fn json_stringify(
    vm: &mut Vm,
    holder: &Value,
    key: &JsString,
    cur_indent: &str,
    state: &mut StringifyState,
    out: &mut String,
) -> Result<bool, Value> {
    let mut value = vm.get_prop(holder, &PropertyKey::Str(key.clone()))?;

    // toJSON: looked up for an Object OR a BigInt value (spec
    // SerializeJSONProperty step 2), called with the value as `this`.
    if matches!(&value, Value::Object(_) | Value::BigInt(_)) {
        let to_json = vm.get_prop(&value, &PropertyKey::str("toJSON"))?;
        if vm.is_callable(&to_json) {
            let this = value.clone();
            value = vm.call(to_json, this, &[Value::String(key.clone())])?;
        }
    }

    // Function replacer.
    if let Some(rep) = &state.rep_fn {
        let rep = rep.clone();
        value = vm.call(rep, holder.clone(), &[Value::String(key.clone()), value])?;
    }

    // Unwrap boxed primitives (Number/String/Boolean) to their primitive value.
    if let Value::Object(o) = &value {
        // Determine the boxed class and (for Boolean) its primitive without
        // holding the borrow across the reassignment below.
        let (cls, boxed_bool, boxed_bigint) = {
            let b = o.borrow();
            let cls = b.class_name();
            let bb = if let Internal::Boolean(v) = &b.internal {
                Some(*v)
            } else {
                None
            };
            let bi = if let Internal::BigIntObj(n) = &b.internal {
                Some(n.clone())
            } else {
                None
            };
            (cls, bb, bi)
        };
        let boxed = value.clone();
        match cls {
            "Number" => value = Value::Number(vm.to_number(&boxed)?),
            "String" => value = Value::String(vm.to_js_string(&boxed)?),
            "Boolean" => value = Value::Bool(boxed_bool.unwrap_or(false)),
            // A boxed BigInt unwraps to its primitive (step 4.d), which step
            // 10 then rejects with a TypeError.
            "BigInt" => {
                if let Some(n) = boxed_bigint {
                    value = Value::BigInt(n);
                }
            }
            _ => {}
        }
    }

    if let Value::BigInt(_) = &value {
        return Err(vm.throw_type("Do not know how to serialize a BigInt"));
    }
    Ok(match &value {
        Value::Undefined
        | Value::Uninitialized
        | Value::Hole
        | Value::Symbol(_)
        | Value::BigInt(_) => false,
        Value::Null => {
            out.push_str("null");
            true
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            true
        }
        Value::Number(n) => {
            if n.is_finite() {
                crate::vm::push_number_string(*n, out);
            } else {
                out.push_str("null");
            }
            true
        }
        Value::String(s) => {
            json_quote_into(s.as_str(), out);
            true
        }
        Value::Object(o) => {
            if o.borrow().is_callable() {
                return Ok(false);
            }
            let id = o.ptr_id();
            if state.seen.contains(&id) {
                return Err(vm.throw_type("Converting circular structure to JSON"));
            }
            state.seen.push(id);
            let is_array = crate::builtins::fundamental::is_array_exotic(vm, o)?;
            // Pretty-print separators — ALL empty in the common compact
            // mode, so assembly below is pure buffer pushes.
            let (new_indent, nl, close_nl, sp) = if state.indent.is_empty() {
                (String::new(), String::new(), String::new(), "")
            } else {
                let ni = format!("{cur_indent}{}", state.indent);
                let nl = format!("\n{ni}");
                (ni, nl, format!("\n{cur_indent}"), " ")
            };
            if is_array {
                // SerializeJSONArray reads `length` via [[Get]] (a proxy trap
                // fires) and ToLength; elements go through [[Get]] too.
                let len_v = vm.get_prop(&value, &PropertyKey::str("length"))?;
                let len = vm.to_length(&len_v)?;
                out.push('[');
                if len != 0 {
                    for i in 0..len {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push_str(&nl);
                        let k = JsString::from(i.to_string());
                        if !json_stringify(vm, &value, &k, &new_indent, state, out)? {
                            out.push_str("null");
                        }
                    }
                    out.push_str(&close_nl);
                }
                out.push(']');
            } else {
                // Property key list: the allowlist if present, else all enumerable
                // own string keys.
                let keys: Vec<JsString> = if let Some(list) = &state.rep_list {
                    list.clone()
                } else {
                    // EnumerableOwnPropertyNames via [[OwnPropertyKeys]] +
                    // [[GetOwnProperty]] (proxy-aware); JSON keeps string keys.
                    vm.enumerable_own_keys_dyn(o)?
                        .into_iter()
                        .filter_map(|k| match k {
                            PropertyKey::Str(s) => Some(s),
                            PropertyKey::Sym(_) => None,
                        })
                        .collect()
                };
                out.push('{');
                let mut first = true;
                for k in keys {
                    // Write the member prefix, then the value; an OMITTED
                    // member (undefined/function/symbol) truncates the
                    // prefix back off. Side effects (getters, toJSON, the
                    // replacer) still ran — exactly as the spec orders.
                    let mark = out.len();
                    if !first {
                        out.push(',');
                    }
                    out.push_str(&nl);
                    json_quote_into(k.as_str(), out);
                    out.push(':');
                    out.push_str(sp);
                    if json_stringify(vm, &value, &k, &new_indent, state, out)? {
                        first = false;
                    } else {
                        out.truncate(mark);
                    }
                }
                if !first {
                    out.push_str(&close_nl);
                }
                out.push('}');
            }
            state.seen.pop();
            true
        }
    })
}
