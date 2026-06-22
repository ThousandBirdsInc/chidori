//! The `Temporal` namespace. The spec arithmetic, parsing, and validation are
//! delegated to `temporal_rs` (the TC39-proposal reference implementation in
//! Rust); this module builds the observable JS surface — constructors,
//! prototype methods/accessors, and the JS↔`temporal_rs` value bridge — on top
//! of it. Each `Temporal.*` instance stores its backing value in an
//! `Internal::Temporal` slot. Types are added incrementally; `Temporal.Duration`
//! is implemented here.

use super::arg;
use crate::value::*;
use crate::vm::Vm;

use std::str::FromStr;
use temporal_rs::options::{
    RoundingIncrement, RoundingMode, RoundingOptions, ToStringRoundingOptions, Unit,
};
use temporal_rs::parsers::Precision;
use temporal_rs::{Duration, Sign};

/// Parse a Temporal unit string (singular or plural), RangeError otherwise.
fn parse_unit(vm: &mut Vm, s: &str) -> Result<Unit, Value> {
    Unit::from_str(s).map_err(|_| vm.throw_range(&format!("invalid Temporal unit: {s}")))
}

/// Read a unit-valued option (`None` when absent).
fn get_unit_option(vm: &mut Vm, obj: &Value, name: &str) -> Result<Option<Unit>, Value> {
    let v = vm.get_prop(obj, &PropertyKey::str(name))?;
    if v.is_undefined() {
        return Ok(None);
    }
    let s = vm.to_js_string(&v)?.as_str().to_owned();
    Ok(Some(parse_unit(vm, &s)?))
}

/// Read the `roundingMode` option (`None` when absent).
fn get_rounding_mode(vm: &mut Vm, obj: &Value) -> Result<Option<RoundingMode>, Value> {
    let v = vm.get_prop(obj, &PropertyKey::str("roundingMode"))?;
    if v.is_undefined() {
        return Ok(None);
    }
    let s = vm.to_js_string(&v)?.as_str().to_owned();
    RoundingMode::from_str(&s)
        .map(Some)
        .map_err(|_| vm.throw_range(&format!("invalid roundingMode: {s}")))
}

/// Read the `roundingIncrement` option (`None` when absent).
fn get_increment(vm: &mut Vm, obj: &Value) -> Result<Option<RoundingIncrement>, Value> {
    let v = vm.get_prop(obj, &PropertyKey::str("roundingIncrement"))?;
    if v.is_undefined() {
        return Ok(None);
    }
    let n = vm.to_number(&v)?;
    if !n.is_finite() || n.fract() != 0.0 || n < 1.0 || n > 1.0e9 {
        return Err(vm.throw_range("invalid roundingIncrement"));
    }
    RoundingIncrement::try_new(n as u32)
        .map(Some)
        .map_err(|e| temporal_err(vm, e))
}

pub fn install(vm: &mut Vm) {
    let temporal = vm.new_object();

    install_duration(vm, &temporal);

    // Temporal[Symbol.toStringTag] = "Temporal" (non-writable, non-enumerable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    temporal.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Temporal"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    vm.define_value(
        &vm.realm.global.clone(),
        "Temporal",
        Value::Object(temporal),
    );
}

// =========================================================================
// Shared bridge helpers
// =========================================================================

/// Allocate a `Temporal.*` instance with prototype `proto` wrapping `slot`.
fn new_temporal(vm: &Vm, slot: TemporalSlot, proto: &JsObject) -> Value {
    Value::Object(vm.alloc(ObjectData::new(
        Some(proto.clone()),
        Internal::Temporal(Box::new(slot)),
    )))
}

/// Map a `temporal_rs` error to a JS exception. `temporal_rs` reports both
/// RangeError- and TypeError-class failures; its `RangeError` kind is by far the
/// most common, so unmapped errors become RangeError (the spec's default here).
fn temporal_err(vm: &mut Vm, e: temporal_rs::TemporalError) -> Value {
    let msg = e.to_string();
    match e.kind() {
        temporal_rs::error::ErrorKind::Type => vm.throw_type(&msg),
        _ => vm.throw_range(&msg),
    }
}

/// `ToIntegerIfIntegral`: ToNumber, then require a finite integer (RangeError
/// otherwise); `-0` normalizes to `+0`.
fn to_integer_if_integral(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    let n = vm.to_number(v)?;
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(vm.throw_range("Temporal: value must be an integer"));
    }
    Ok(if n == 0.0 { 0.0 } else { n })
}

/// A constructor argument that defaults to 0 when `undefined`, else
/// `ToIntegerIfIntegral`.
fn integer_arg(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    if v.is_undefined() {
        Ok(0.0)
    } else {
        to_integer_if_integral(vm, v)
    }
}

// =========================================================================
// Temporal.Duration
// =========================================================================

/// The `temporal_rs::Duration` backing a `Temporal.Duration` receiver.
fn this_duration(vm: &mut Vm, this: &Value) -> Result<Duration, Value> {
    if let Value::Object(o) = this {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::Duration(d) = slot.as_ref() {
                return Ok(d.clone());
            }
        }
    }
    Err(vm.throw_type("receiver is not a Temporal.Duration"))
}

/// The ten duration fields read from an object in alphabetical order (the spec's
/// observable `Get` order), each `None` when absent. Returns the values and
/// whether *any* field was present.
#[allow(clippy::type_complexity)]
fn read_duration_fields(vm: &mut Vm, obj: &Value) -> Result<([Option<f64>; 10], bool), Value> {
    // Indices: 0 years,1 months,2 weeks,3 days,4 hours,5 minutes,6 seconds,
    // 7 milliseconds,8 microseconds,9 nanoseconds.
    const ALPHA: &[(&str, usize)] = &[
        ("days", 3),
        ("hours", 4),
        ("microseconds", 8),
        ("milliseconds", 7),
        ("minutes", 5),
        ("months", 1),
        ("nanoseconds", 9),
        ("seconds", 6),
        ("weeks", 2),
        ("years", 0),
    ];
    let mut out = [None; 10];
    let mut any = false;
    for (name, idx) in ALPHA {
        let v = vm.get_prop(obj, &PropertyKey::str(*name))?;
        if !v.is_undefined() {
            any = true;
            out[*idx] = Some(to_integer_if_integral(vm, &v)?);
        }
    }
    Ok((out, any))
}

/// Build a `Duration` from ten field values (years..nanoseconds), mapping a
/// validation failure to a RangeError.
fn duration_from_values(vm: &mut Vm, f: [f64; 10]) -> Result<Duration, Value> {
    Duration::new(
        f[0] as i64,
        f[1] as i64,
        f[2] as i64,
        f[3] as i64,
        f[4] as i64,
        f[5] as i64,
        f[6] as i64,
        f[7] as i64,
        f[8] as i128,
        f[9] as i128,
    )
    .map_err(|e| temporal_err(vm, e))
}

/// `ToTemporalDuration`: a Duration receiver is copied; a string is parsed; an
/// object's fields are read (alphabetical order; at least one required).
fn to_temporal_duration(vm: &mut Vm, v: &Value) -> Result<Duration, Value> {
    if let Value::Object(o) = v {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::Duration(d) = slot.as_ref() {
                return Ok(d.clone());
            }
        }
    }
    match v {
        Value::Object(_) => {
            let (fields, any) = read_duration_fields(vm, v)?;
            if !any {
                return Err(vm.throw_type("Temporal.Duration: no recognized fields"));
            }
            let vals = fields.map(|o| o.unwrap_or(0.0));
            duration_from_values(vm, vals)
        }
        Value::String(s) => {
            Duration::from_utf8(s.as_str().as_bytes()).map_err(|e| temporal_err(vm, e))
        }
        _ => Err(vm.throw_type("Temporal.Duration: expected a Duration, string, or object")),
    }
}

fn construct_duration(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let mut f = [0.0f64; 10];
    for (i, item) in f.iter_mut().enumerate() {
        *item = integer_arg(vm, &arg(args, i))?;
    }
    let d = duration_from_values(vm, f)?;
    Ok(new_temporal(vm, TemporalSlot::Duration(d), proto))
}

fn install_duration(vm: &mut Vm, temporal: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "Duration",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor Temporal.Duration requires 'new'")),
        move |vm, _this, args| construct_duration(vm, args, &ctor_proto),
    );
    ctor.borrow_mut().props.insert(
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
        Property::builtin(Value::Object(ctor.clone())),
    );

    // Temporal.Duration.from(item)
    let from_proto = proto.clone();
    vm.define_method(&ctor, "from", 1, move |vm, _t, args| {
        let d = to_temporal_duration(vm, &arg(args, 0))?;
        Ok(new_temporal(vm, TemporalSlot::Duration(d), &from_proto))
    });
    // Temporal.Duration.compare(one, two[, options]) — without relativeTo.
    vm.define_method(&ctor, "compare", 2, |vm, _t, args| {
        let one = to_temporal_duration(vm, &arg(args, 0))?;
        let two = to_temporal_duration(vm, &arg(args, 1))?;
        let ord = one.compare(&two, None).map_err(|e| temporal_err(vm, e))?;
        Ok(Value::Number(match ord {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });

    // Field accessors (years..nanoseconds), plus sign and blank.
    define_duration_getter(vm, &proto, "years", |d| Value::Number(d.years() as f64));
    define_duration_getter(vm, &proto, "months", |d| Value::Number(d.months() as f64));
    define_duration_getter(vm, &proto, "weeks", |d| Value::Number(d.weeks() as f64));
    define_duration_getter(vm, &proto, "days", |d| Value::Number(d.days() as f64));
    define_duration_getter(vm, &proto, "hours", |d| Value::Number(d.hours() as f64));
    define_duration_getter(vm, &proto, "minutes", |d| Value::Number(d.minutes() as f64));
    define_duration_getter(vm, &proto, "seconds", |d| Value::Number(d.seconds() as f64));
    define_duration_getter(vm, &proto, "milliseconds", |d| {
        Value::Number(d.milliseconds() as f64)
    });
    define_duration_getter(vm, &proto, "microseconds", |d| {
        Value::Number(d.microseconds() as f64)
    });
    define_duration_getter(vm, &proto, "nanoseconds", |d| {
        Value::Number(d.nanoseconds() as f64)
    });
    define_duration_getter(vm, &proto, "sign", |d| {
        Value::Number(match d.sign() {
            Sign::Positive => 1.0,
            Sign::Negative => -1.0,
            Sign::Zero => 0.0,
        })
    });
    define_duration_getter(vm, &proto, "blank", |d| Value::Bool(d.is_zero()));

    vm.define_method(
        &proto,
        "negated",
        0,
        dur_method(|_vm, d, _a| Ok(d.negated())),
    );
    vm.define_method(&proto, "abs", 0, dur_method(|_vm, d, _a| Ok(d.abs())));
    vm.define_method(
        &proto,
        "add",
        1,
        dur_method(|vm, d, args| {
            let other = to_temporal_duration(vm, &arg(args, 0))?;
            d.add(&other).map_err(|e| temporal_err(vm, e))
        }),
    );
    vm.define_method(
        &proto,
        "subtract",
        1,
        dur_method(|vm, d, args| {
            let other = to_temporal_duration(vm, &arg(args, 0))?;
            d.subtract(&other).map_err(|e| temporal_err(vm, e))
        }),
    );

    // with(partialDuration): override only the provided fields.
    let with_proto = proto.clone();
    vm.define_method(&proto, "with", 1, move |vm, this, args| {
        let d = this_duration(vm, &this)?;
        let item = arg(args, 0);
        if !matches!(item, Value::Object(_)) {
            return Err(
                vm.throw_type("Temporal.Duration.prototype.with: argument must be an object")
            );
        }
        let (fields, any) = read_duration_fields(vm, &item)?;
        if !any {
            return Err(vm.throw_type("Temporal.Duration.prototype.with: no recognized fields"));
        }
        let cur = [
            d.years() as f64,
            d.months() as f64,
            d.weeks() as f64,
            d.days() as f64,
            d.hours() as f64,
            d.minutes() as f64,
            d.seconds() as f64,
            d.milliseconds() as f64,
            d.microseconds() as f64,
            d.nanoseconds() as f64,
        ];
        let mut merged = cur;
        for i in 0..10 {
            if let Some(v) = fields[i] {
                merged[i] = v;
            }
        }
        let nd = duration_from_values(vm, merged)?;
        Ok(new_temporal(vm, TemporalSlot::Duration(nd), &with_proto))
    });

    // round(roundTo): roundTo is a smallestUnit string or an options object.
    // relativeTo is not yet supported, so calendar-unit rounding that needs it
    // surfaces the underlying RangeError.
    let round_proto = proto.clone();
    vm.define_method(&proto, "round", 1, move |vm, this, args| {
        let d = this_duration(vm, &this)?;
        let opt = arg(args, 0);
        let mut ro = RoundingOptions::default();
        match &opt {
            Value::Undefined => {
                return Err(vm.throw_type("Temporal.Duration.prototype.round requires an argument"))
            }
            Value::String(s) => ro.smallest_unit = Some(parse_unit(vm, s.as_str())?),
            Value::Object(_) => {
                ro.largest_unit = get_unit_option(vm, &opt, "largestUnit")?;
                ro.increment = get_increment(vm, &opt)?;
                ro.rounding_mode = get_rounding_mode(vm, &opt)?;
                ro.smallest_unit = get_unit_option(vm, &opt, "smallestUnit")?;
            }
            _ => return Err(vm.throw_type("Temporal.Duration.prototype.round: invalid options")),
        }
        let nd = d.round(ro, None).map_err(|e| temporal_err(vm, e))?;
        Ok(new_temporal(vm, TemporalSlot::Duration(nd), &round_proto))
    });

    // total(totalOf): totalOf is a unit string or an options object with `unit`.
    vm.define_method(&proto, "total", 1, |vm, this, args| {
        let d = this_duration(vm, &this)?;
        let opt = arg(args, 0);
        let unit = match &opt {
            Value::String(s) => parse_unit(vm, s.as_str())?,
            Value::Object(_) => get_unit_option(vm, &opt, "unit")?.ok_or_else(|| {
                vm.throw_range("Temporal.Duration.prototype.total: unit is required")
            })?,
            _ => return Err(vm.throw_type("Temporal.Duration.prototype.total: invalid argument")),
        };
        let total = d.total(unit, None).map_err(|e| temporal_err(vm, e))?;
        Ok(Value::Number(total.as_inner()))
    });

    // toString([options]): smallestUnit / fractionalSecondDigits / roundingMode.
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let d = this_duration(vm, &this)?;
        let opts = duration_to_string_options(vm, &arg(args, 0))?;
        let s = d
            .as_temporal_string(opts)
            .map_err(|e| temporal_err(vm, e))?;
        Ok(Value::str(s))
    });
    vm.define_method(&proto, "toJSON", 0, |vm, this, _a| {
        let d = this_duration(vm, &this)?;
        let s = d
            .as_temporal_string(Default::default())
            .map_err(|e| temporal_err(vm, e))?;
        Ok(Value::str(s))
    });
    // toLocaleString: no Intl.DurationFormat yet — defer to the ISO string.
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let d = this_duration(vm, &this)?;
        let s = d
            .as_temporal_string(Default::default())
            .map_err(|e| temporal_err(vm, e))?;
        Ok(Value::str(s))
    });
    // valueOf: Temporal types are not comparable with relational operators.
    vm.define_method(&proto, "valueOf", 0, |vm, _this, _a| {
        Err(vm.throw_type("Temporal.Duration: use compare() instead of relational operators"))
    });

    // Temporal.Duration.prototype[Symbol.toStringTag] = "Temporal.Duration"
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Temporal.Duration"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    temporal.borrow_mut().props.insert(
        PropertyKey::str("Duration"),
        Property::builtin(Value::Object(ctor)),
    );
}

/// Build `ToStringRoundingOptions` from a Duration.toString options argument.
fn duration_to_string_options(vm: &mut Vm, opt: &Value) -> Result<ToStringRoundingOptions, Value> {
    let mut tso = ToStringRoundingOptions::default();
    if opt.is_undefined() {
        return Ok(tso);
    }
    if !matches!(opt, Value::Object(_)) {
        return Err(
            vm.throw_type("Temporal.Duration.prototype.toString: options must be an object")
        );
    }
    tso.smallest_unit = get_unit_option(vm, opt, "smallestUnit")?;
    tso.rounding_mode = get_rounding_mode(vm, opt)?;
    let fsd = vm.get_prop(opt, &PropertyKey::str("fractionalSecondDigits"))?;
    if !fsd.is_undefined() {
        tso.precision = if let Value::String(s) = &fsd {
            if s.as_str() == "auto" {
                Precision::Auto
            } else {
                return Err(vm.throw_range("invalid fractionalSecondDigits"));
            }
        } else {
            let n = vm.to_number(&fsd)?;
            if !n.is_finite() || n.fract() != 0.0 || !(0.0..=9.0).contains(&n) {
                return Err(vm.throw_range("invalid fractionalSecondDigits"));
            }
            Precision::Digit(n as u8)
        };
    }
    Ok(tso)
}

/// Wrap a `Duration`-returning method body into a native function: validate the
/// receiver, run `body`, and re-wrap the result as a new Temporal.Duration.
fn dur_method(
    body: fn(&mut Vm, Duration, &[Value]) -> Result<Duration, Value>,
) -> impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> {
    move |vm, this, args| {
        let d = this_duration(vm, &this)?;
        // The result inherits the receiver's prototype chain root
        // (%Temporal.Duration.prototype%), found via the receiver.
        let proto = duration_proto(&this);
        let nd = body(vm, d, args)?;
        match proto {
            Some(p) => Ok(new_temporal(vm, TemporalSlot::Duration(nd), &p)),
            None => Err(vm.throw_type("receiver is not a Temporal.Duration")),
        }
    }
}

/// The `[[Prototype]]` of a Temporal.Duration receiver (for result construction).
fn duration_proto(this: &Value) -> Option<JsObject> {
    if let Value::Object(o) = this {
        return o.borrow().proto.clone();
    }
    None
}

/// Define a non-enumerable, configurable `get`-only accessor reading the
/// receiver's Duration.
fn define_duration_getter(
    vm: &mut Vm,
    proto: &JsObject,
    name: &str,
    project: fn(&Duration) -> Value,
) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let d = this_duration(vm, &this)?;
        Ok(project(&d))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str(name),
        Some(Value::Object(getter)),
        None,
    );
}
