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
    DifferenceSettings, Overflow, RoundingIncrement, RoundingMode, RoundingOptions,
    ToStringRoundingOptions, Unit,
};
use temporal_rs::parsers::Precision;
use temporal_rs::partial::PartialTime;
use temporal_rs::{Duration, PlainTime, Sign};

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

/// `GetTemporalOverflowOption`: `undefined`/object only; reads `overflow`
/// (`constrain`/`reject`).
fn get_overflow(vm: &mut Vm, opt: &Value) -> Result<Option<Overflow>, Value> {
    if opt.is_undefined() {
        return Ok(None);
    }
    if !matches!(opt, Value::Object(_)) {
        return Err(vm.throw_type("Temporal: options must be an object or undefined"));
    }
    let v = vm.get_prop(opt, &PropertyKey::str("overflow"))?;
    if v.is_undefined() {
        return Ok(None);
    }
    match vm.to_js_string(&v)?.as_str() {
        "constrain" => Ok(Some(Overflow::Constrain)),
        "reject" => Ok(Some(Overflow::Reject)),
        other => Err(vm.throw_range(&format!("invalid overflow: {other}"))),
    }
}

/// Parse `RoundingOptions` from a `round` argument (a smallestUnit string or an
/// options object; `largestUnit`/`roundingIncrement`/`roundingMode` optional).
fn get_rounding_options(vm: &mut Vm, opt: &Value) -> Result<RoundingOptions, Value> {
    let mut ro = RoundingOptions::default();
    match opt {
        Value::Undefined => return Err(vm.throw_type("round requires an argument")),
        Value::String(s) => ro.smallest_unit = Some(parse_unit(vm, s.as_str())?),
        Value::Object(_) => {
            ro.largest_unit = get_unit_option(vm, opt, "largestUnit")?;
            ro.increment = get_increment(vm, opt)?;
            ro.rounding_mode = get_rounding_mode(vm, opt)?;
            ro.smallest_unit = get_unit_option(vm, opt, "smallestUnit")?;
        }
        _ => return Err(vm.throw_type("round: invalid options")),
    }
    Ok(ro)
}

/// `ToRelativeTemporalObject`: read `relativeTo` from an options object as a
/// PlainDate (a Date instance, a date-fields object, or an ISO string).
/// ZonedDateTime `relativeTo` is not yet supported.
fn get_relative_to(
    vm: &mut Vm,
    opt: &Value,
) -> Result<Option<temporal_rs::options::RelativeTo>, Value> {
    use temporal_rs::options::RelativeTo;
    if !matches!(opt, Value::Object(_)) {
        return Ok(None);
    }
    let v = vm.get_prop(opt, &PropertyKey::str("relativeTo"))?;
    if v.is_undefined() {
        return Ok(None);
    }
    if let Value::Object(o) = &v {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::PlainDate(d) = slot.as_ref() {
                return Ok(Some(RelativeTo::from(d.clone())));
            }
        }
    }
    let d = to_temporal_date(vm, &v, None)?;
    Ok(Some(RelativeTo::from(d)))
}

/// Parse `DifferenceSettings` (the `until`/`since` options object).
fn get_difference_settings(vm: &mut Vm, opt: &Value) -> Result<DifferenceSettings, Value> {
    let mut s = DifferenceSettings::default();
    if opt.is_undefined() {
        return Ok(s);
    }
    if !matches!(opt, Value::Object(_)) {
        return Err(vm.throw_type("until/since: options must be an object"));
    }
    s.largest_unit = get_unit_option(vm, opt, "largestUnit")?;
    s.increment = get_increment(vm, opt)?;
    s.rounding_mode = get_rounding_mode(vm, opt)?;
    s.smallest_unit = get_unit_option(vm, opt, "smallestUnit")?;
    Ok(s)
}

pub fn install(vm: &mut Vm) {
    let temporal = vm.new_object();

    install_duration(vm, &temporal);
    install_plain_time(vm, &temporal);
    install_plain_date(vm, &temporal);
    install_instant(vm, &temporal);
    install_plain_date_time(vm, &temporal);

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
        let rel = get_relative_to(vm, &arg(args, 2))?;
        let ord = one.compare(&two, rel).map_err(|e| temporal_err(vm, e))?;
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
        let rel = get_relative_to(vm, &opt)?;
        let nd = d.round(ro, rel).map_err(|e| temporal_err(vm, e))?;
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
        let rel = get_relative_to(vm, &opt)?;
        let total = d.total(unit, rel).map_err(|e| temporal_err(vm, e))?;
        Ok(Value::Number(total.as_inner()))
    });

    // toString([options]): smallestUnit / fractionalSecondDigits / roundingMode.
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let d = this_duration(vm, &this)?;
        let opts = to_string_rounding_options(vm, &arg(args, 0))?;
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
fn to_string_rounding_options(vm: &mut Vm, opt: &Value) -> Result<ToStringRoundingOptions, Value> {
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
        // A Number must be an integer in 0..=9; any non-Number must stringify to
        // exactly "auto" (so e.g. `null` is a RangeError, not 0).
        tso.precision = if let Value::Number(n) = fsd {
            if !n.is_finite() || n.fract() != 0.0 || !(0.0..=9.0).contains(&n) {
                return Err(vm.throw_range("invalid fractionalSecondDigits"));
            }
            Precision::Digit(n as u8)
        } else if vm.to_js_string(&fsd)?.as_str() == "auto" {
            Precision::Auto
        } else {
            return Err(vm.throw_range("invalid fractionalSecondDigits"));
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

// =========================================================================
// Temporal.PlainTime
// =========================================================================

/// Fetch an intrinsic `Temporal.<name>.prototype` (for constructing method
/// results, including cross-type ones like `until` → Duration).
fn intrinsic_proto(vm: &mut Vm, name: &str) -> Result<JsObject, Value> {
    let temporal = vm.get_prop(
        &Value::Object(vm.realm.global.clone()),
        &PropertyKey::str("Temporal"),
    )?;
    let ctor = vm.get_prop(&temporal, &PropertyKey::str(name))?;
    let proto = vm.get_prop(&ctor, &PropertyKey::str("prototype"))?;
    match proto {
        Value::Object(o) => Ok(o),
        _ => Err(vm.throw_type("missing Temporal prototype")),
    }
}

/// `ToIntegerWithTruncation`: ToNumber, reject non-finite (RangeError), truncate.
fn to_integer_with_truncation(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    let n = vm.to_number(v)?;
    if !n.is_finite() {
        return Err(vm.throw_range("Temporal: value must be finite"));
    }
    Ok(n.trunc())
}

/// The `temporal_rs::PlainTime` backing a receiver.
fn this_plain_time(vm: &mut Vm, this: &Value) -> Result<PlainTime, Value> {
    if let Value::Object(o) = this {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::PlainTime(t) = slot.as_ref() {
                return Ok(*t);
            }
        }
    }
    Err(vm.throw_type("receiver is not a Temporal.PlainTime"))
}

/// Nanosecond-of-day, for `compare`.
fn time_ns(t: &PlainTime) -> i64 {
    t.hour() as i64 * 3_600_000_000_000
        + t.minute() as i64 * 60_000_000_000
        + t.second() as i64 * 1_000_000_000
        + t.millisecond() as i64 * 1_000_000
        + t.microsecond() as i64 * 1_000
        + t.nanosecond() as i64
}

/// Read a `PartialTime` from an object in alphabetical field order; also returns
/// whether any field was present.
fn read_partial_time(vm: &mut Vm, obj: &Value) -> Result<(PartialTime, bool), Value> {
    let mut pt = PartialTime {
        hour: None,
        minute: None,
        second: None,
        millisecond: None,
        microsecond: None,
        nanosecond: None,
    };
    let mut any = false;
    for name in [
        "hour",
        "microsecond",
        "millisecond",
        "minute",
        "nanosecond",
        "second",
    ] {
        let v = vm.get_prop(obj, &PropertyKey::str(name))?;
        if v.is_undefined() {
            continue;
        }
        any = true;
        let n = to_integer_with_truncation(vm, &v)?;
        match name {
            "hour" => pt.hour = Some(n as u8),
            "minute" => pt.minute = Some(n as u8),
            "second" => pt.second = Some(n as u8),
            "millisecond" => pt.millisecond = Some(n as u16),
            "microsecond" => pt.microsecond = Some(n as u16),
            "nanosecond" => pt.nanosecond = Some(n as u16),
            _ => {}
        }
    }
    Ok((pt, any))
}

/// `ToTemporalTime`: a PlainTime is copied; a string is parsed; an object's
/// fields build a PartialTime resolved with `overflow`.
fn to_temporal_time(
    vm: &mut Vm,
    v: &Value,
    overflow: Option<Overflow>,
) -> Result<PlainTime, Value> {
    if let Value::Object(o) = v {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::PlainTime(t) = slot.as_ref() {
                return Ok(*t);
            }
        }
    }
    match v {
        Value::Object(_) => {
            let (pt, any) = read_partial_time(vm, v)?;
            if !any {
                return Err(vm.throw_type("Temporal.PlainTime: object has no time fields"));
            }
            PlainTime::from_partial(pt, overflow).map_err(|e| temporal_err(vm, e))
        }
        Value::String(s) => {
            PlainTime::from_utf8(s.as_str().as_bytes()).map_err(|e| temporal_err(vm, e))
        }
        _ => Err(vm.throw_type("Temporal.PlainTime: expected a time, string, or object")),
    }
}

fn construct_plain_time(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let mut f = [0.0f64; 6];
    for (i, item) in f.iter_mut().enumerate() {
        let a = arg(args, i);
        *item = if a.is_undefined() {
            0.0
        } else {
            to_integer_with_truncation(vm, &a)?
        };
    }
    // All PlainTime fields are non-negative; a negative would otherwise be hidden
    // by the saturating float→uint cast below.
    if f.iter().any(|&x| x < 0.0) {
        return Err(vm.throw_range("Temporal.PlainTime: fields must be non-negative"));
    }
    let t = PlainTime::try_new(
        f[0] as u8,
        f[1] as u8,
        f[2] as u8,
        f[3] as u16,
        f[4] as u16,
        f[5] as u16,
    )
    .map_err(|e| temporal_err(vm, e))?;
    Ok(new_temporal(vm, TemporalSlot::PlainTime(t), proto))
}

fn install_plain_time(vm: &mut Vm, temporal: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "PlainTime",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor Temporal.PlainTime requires 'new'")),
        move |vm, _this, args| construct_plain_time(vm, args, &ctor_proto),
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

    // Temporal.PlainTime.from(item[, options])
    vm.define_method(&ctor, "from", 1, |vm, _t, args| {
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let t = to_temporal_time(vm, &arg(args, 0), overflow)?;
        let proto = intrinsic_proto(vm, "PlainTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainTime(t), &proto))
    });
    // Temporal.PlainTime.compare(one, two)
    vm.define_method(&ctor, "compare", 2, |vm, _t, args| {
        let a = to_temporal_time(vm, &arg(args, 0), None)?;
        let b = to_temporal_time(vm, &arg(args, 1), None)?;
        Ok(Value::Number(match time_ns(&a).cmp(&time_ns(&b)) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });

    define_time_getter(vm, &proto, "hour", |t| t.hour() as f64);
    define_time_getter(vm, &proto, "minute", |t| t.minute() as f64);
    define_time_getter(vm, &proto, "second", |t| t.second() as f64);
    define_time_getter(vm, &proto, "millisecond", |t| t.millisecond() as f64);
    define_time_getter(vm, &proto, "microsecond", |t| t.microsecond() as f64);
    define_time_getter(vm, &proto, "nanosecond", |t| t.nanosecond() as f64);

    vm.define_method(&proto, "add", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let d = to_temporal_duration(vm, &arg(args, 0))?;
        let nt = t.add(&d).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainTime(nt), &proto))
    });
    vm.define_method(&proto, "subtract", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let d = to_temporal_duration(vm, &arg(args, 0))?;
        let nt = t.subtract(&d).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainTime(nt), &proto))
    });
    vm.define_method(&proto, "with", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let item = arg(args, 0);
        if !matches!(item, Value::Object(_)) {
            return Err(
                vm.throw_type("Temporal.PlainTime.prototype.with: argument must be an object")
            );
        }
        let (pt, any) = read_partial_time(vm, &item)?;
        if !any {
            return Err(vm.throw_type("Temporal.PlainTime.prototype.with: no recognized fields"));
        }
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nt = t.with(pt, overflow).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainTime(nt), &proto))
    });
    vm.define_method(&proto, "round", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let ro = get_rounding_options(vm, &arg(args, 0))?;
        let nt = t.round(ro).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainTime(nt), &proto))
    });
    vm.define_method(&proto, "until", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let other = to_temporal_time(vm, &arg(args, 0), None)?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let d = t.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(d), &proto))
    });
    vm.define_method(&proto, "since", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let other = to_temporal_time(vm, &arg(args, 0), None)?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let d = t.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(d), &proto))
    });
    vm.define_method(&proto, "equals", 1, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let other = to_temporal_time(vm, &arg(args, 0), None)?;
        Ok(Value::Bool(time_ns(&t) == time_ns(&other)))
    });
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let t = this_plain_time(vm, &this)?;
        let opts = to_string_rounding_options(vm, &arg(args, 0))?;
        Ok(Value::str(
            t.to_ixdtf_string(opts).map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "toJSON", 0, |vm, this, _a| {
        let t = this_plain_time(vm, &this)?;
        Ok(Value::str(
            t.to_ixdtf_string(Default::default())
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let t = this_plain_time(vm, &this)?;
        Ok(Value::str(
            t.to_ixdtf_string(Default::default())
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, _this, _a| {
        Err(vm.throw_type("Temporal.PlainTime: use compare() instead of relational operators"))
    });

    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Temporal.PlainTime"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    temporal.borrow_mut().props.insert(
        PropertyKey::str("PlainTime"),
        Property::builtin(Value::Object(ctor)),
    );
}

fn define_time_getter(vm: &mut Vm, proto: &JsObject, name: &str, project: fn(&PlainTime) -> f64) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let t = this_plain_time(vm, &this)?;
        Ok(Value::Number(project(&t)))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str(name),
        Some(Value::Object(getter)),
        None,
    );
}

// =========================================================================
// Temporal.PlainDate
// =========================================================================

use temporal_rs::options::DisplayCalendar;
use temporal_rs::partial::PartialDate;
use temporal_rs::{Calendar, MonthCode, PlainDate};

/// The `temporal_rs::PlainDate` backing a receiver.
fn this_plain_date(vm: &mut Vm, this: &Value) -> Result<PlainDate, Value> {
    if let Value::Object(o) = this {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::PlainDate(d) = slot.as_ref() {
                return Ok(d.clone());
            }
        }
    }
    Err(vm.throw_type("receiver is not a Temporal.PlainDate"))
}

/// `ToTemporalCalendarIdentifier`: undefined → ISO; a string → parsed calendar.
fn to_calendar(vm: &mut Vm, v: &Value) -> Result<Calendar, Value> {
    if v.is_undefined() {
        return Ok(Calendar::ISO);
    }
    match v {
        Value::String(s) => Calendar::from_str(s.as_str()).map_err(|e| temporal_err(vm, e)),
        _ => Err(vm.throw_type("Temporal: invalid calendar")),
    }
}

/// Read the date `calendar` plus its calendar fields (era/eraYear/year/month/
/// monthCode/day, alphabetical) from an object into a `PartialDate`.
fn read_partial_date(vm: &mut Vm, obj: &Value) -> Result<PartialDate, Value> {
    let cal_v = vm.get_prop(obj, &PropertyKey::str("calendar"))?;
    let calendar = to_calendar(vm, &cal_v)?;
    let mut p = PartialDate::new().with_calendar(calendar);
    let day = vm.get_prop(obj, &PropertyKey::str("day"))?;
    if !day.is_undefined() {
        p.calendar_fields.day = Some(to_integer_with_truncation(vm, &day)? as u8);
    }
    let era = vm.get_prop(obj, &PropertyKey::str("era"))?;
    if !era.is_undefined() {
        let s = vm.to_js_string(&era)?.as_str().to_owned();
        p.calendar_fields.era = s.parse().ok();
    }
    let era_year = vm.get_prop(obj, &PropertyKey::str("eraYear"))?;
    if !era_year.is_undefined() {
        p.calendar_fields.era_year = Some(to_integer_with_truncation(vm, &era_year)? as i32);
    }
    let month = vm.get_prop(obj, &PropertyKey::str("month"))?;
    if !month.is_undefined() {
        p.calendar_fields.month = Some(to_integer_with_truncation(vm, &month)? as u8);
    }
    let month_code = vm.get_prop(obj, &PropertyKey::str("monthCode"))?;
    if !month_code.is_undefined() {
        let s = vm.to_js_string(&month_code)?.as_str().to_owned();
        p.calendar_fields.month_code =
            Some(MonthCode::from_str(&s).map_err(|e| temporal_err(vm, e))?);
    }
    let year = vm.get_prop(obj, &PropertyKey::str("year"))?;
    if !year.is_undefined() {
        p.calendar_fields.year = Some(to_integer_with_truncation(vm, &year)? as i32);
    }
    Ok(p)
}

/// `ToTemporalDate`: a PlainDate is copied; a string is parsed; an object's
/// calendar fields build a date resolved with `overflow`.
fn to_temporal_date(
    vm: &mut Vm,
    v: &Value,
    overflow: Option<Overflow>,
) -> Result<PlainDate, Value> {
    if let Value::Object(o) = v {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::PlainDate(d) = slot.as_ref() {
                return Ok(d.clone());
            }
        }
    }
    match v {
        Value::Object(_) => {
            let p = read_partial_date(vm, v)?;
            PlainDate::from_partial(p, overflow).map_err(|e| temporal_err(vm, e))
        }
        Value::String(s) => {
            PlainDate::from_utf8(s.as_str().as_bytes()).map_err(|e| temporal_err(vm, e))
        }
        _ => Err(vm.throw_type("Temporal.PlainDate: expected a date, string, or object")),
    }
}

/// Read the `calendarName` display option for `toString`.
fn get_display_calendar(vm: &mut Vm, opt: &Value) -> Result<DisplayCalendar, Value> {
    if opt.is_undefined() {
        return Ok(DisplayCalendar::Auto);
    }
    if !matches!(opt, Value::Object(_)) {
        return Err(vm.throw_type("toString: options must be an object"));
    }
    let v = vm.get_prop(opt, &PropertyKey::str("calendarName"))?;
    if v.is_undefined() {
        return Ok(DisplayCalendar::Auto);
    }
    match vm.to_js_string(&v)?.as_str() {
        "auto" => Ok(DisplayCalendar::Auto),
        "always" => Ok(DisplayCalendar::Always),
        "never" => Ok(DisplayCalendar::Never),
        "critical" => Ok(DisplayCalendar::Critical),
        other => Err(vm.throw_range(&format!("invalid calendarName: {other}"))),
    }
}

fn construct_plain_date(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let year = to_integer_with_truncation(vm, &arg(args, 0))?;
    let month = to_integer_with_truncation(vm, &arg(args, 1))?;
    let day = to_integer_with_truncation(vm, &arg(args, 2))?;
    let calendar = to_calendar(vm, &arg(args, 3))?;
    if !(i32::MIN as f64..=i32::MAX as f64).contains(&year) || month < 0.0 || day < 0.0 {
        return Err(vm.throw_range("Temporal.PlainDate: field out of range"));
    }
    let d = PlainDate::try_new(year as i32, month as u8, day as u8, calendar)
        .map_err(|e| temporal_err(vm, e))?;
    Ok(new_temporal(vm, TemporalSlot::PlainDate(d), proto))
}

fn install_plain_date(vm: &mut Vm, temporal: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "PlainDate",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor Temporal.PlainDate requires 'new'")),
        move |vm, _this, args| construct_plain_date(vm, args, &ctor_proto),
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

    vm.define_method(&ctor, "from", 1, |vm, _t, args| {
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let d = to_temporal_date(vm, &arg(args, 0), overflow)?;
        let proto = intrinsic_proto(vm, "PlainDate")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDate(d), &proto))
    });
    vm.define_method(&ctor, "compare", 2, |vm, _t, args| {
        let a = to_temporal_date(vm, &arg(args, 0), None)?;
        let b = to_temporal_date(vm, &arg(args, 1), None)?;
        Ok(Value::Number(match a.compare_iso(&b) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });

    define_date_getter(vm, &proto, "year", |d| Value::Number(d.year() as f64));
    define_date_getter(vm, &proto, "month", |d| Value::Number(d.month() as f64));
    define_date_getter(vm, &proto, "monthCode", |d| {
        Value::str(d.month_code().as_str())
    });
    define_date_getter(vm, &proto, "day", |d| Value::Number(d.day() as f64));
    define_date_getter(vm, &proto, "dayOfWeek", |d| {
        Value::Number(d.day_of_week() as f64)
    });
    define_date_getter(vm, &proto, "dayOfYear", |d| {
        Value::Number(d.day_of_year() as f64)
    });
    define_date_getter(vm, &proto, "weekOfYear", |d| {
        d.week_of_year()
            .map(|w| Value::Number(w as f64))
            .unwrap_or(Value::Undefined)
    });
    define_date_getter(vm, &proto, "yearOfWeek", |d| {
        d.year_of_week()
            .map(|y| Value::Number(y as f64))
            .unwrap_or(Value::Undefined)
    });
    define_date_getter(vm, &proto, "daysInWeek", |d| {
        Value::Number(d.days_in_week() as f64)
    });
    define_date_getter(vm, &proto, "daysInMonth", |d| {
        Value::Number(d.days_in_month() as f64)
    });
    define_date_getter(vm, &proto, "daysInYear", |d| {
        Value::Number(d.days_in_year() as f64)
    });
    define_date_getter(vm, &proto, "monthsInYear", |d| {
        Value::Number(d.months_in_year() as f64)
    });
    define_date_getter(vm, &proto, "inLeapYear", |d| Value::Bool(d.in_leap_year()));
    define_date_getter(vm, &proto, "era", |d| {
        d.era()
            .map(|e| Value::str(e.as_str()))
            .unwrap_or(Value::Undefined)
    });
    define_date_getter(vm, &proto, "eraYear", |d| {
        d.era_year()
            .map(|y| Value::Number(y as f64))
            .unwrap_or(Value::Undefined)
    });
    define_date_getter(vm, &proto, "calendarId", |d| {
        Value::str(d.calendar().identifier())
    });

    vm.define_method(&proto, "add", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let dur = to_temporal_duration(vm, &arg(args, 0))?;
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nd = d.add(&dur, overflow).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDate")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDate(nd), &proto))
    });
    vm.define_method(&proto, "subtract", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let dur = to_temporal_duration(vm, &arg(args, 0))?;
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nd = d
            .subtract(&dur, overflow)
            .map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDate")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDate(nd), &proto))
    });
    vm.define_method(&proto, "with", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let item = arg(args, 0);
        if !matches!(item, Value::Object(_)) {
            return Err(
                vm.throw_type("Temporal.PlainDate.prototype.with: argument must be an object")
            );
        }
        // `with` reuses the receiver's calendar; only the provided fields change.
        let mut p = read_partial_date(vm, &item)?;
        p.calendar = d.calendar().clone();
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nd = d
            .with(p.calendar_fields, overflow)
            .map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDate")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDate(nd), &proto))
    });
    vm.define_method(&proto, "withCalendar", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let cal = to_calendar(vm, &arg(args, 0))?;
        let nd = d.with_calendar(cal);
        let proto = intrinsic_proto(vm, "PlainDate")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDate(nd), &proto))
    });
    vm.define_method(&proto, "until", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let other = to_temporal_date(vm, &arg(args, 0), None)?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let dur = d.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(dur), &proto))
    });
    vm.define_method(&proto, "since", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let other = to_temporal_date(vm, &arg(args, 0), None)?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let dur = d.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(dur), &proto))
    });
    vm.define_method(&proto, "equals", 1, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let other = to_temporal_date(vm, &arg(args, 0), None)?;
        let eq = d.compare_iso(&other) == std::cmp::Ordering::Equal
            && d.calendar().identifier() == other.calendar().identifier();
        Ok(Value::Bool(eq))
    });
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let d = this_plain_date(vm, &this)?;
        let dc = get_display_calendar(vm, &arg(args, 0))?;
        Ok(Value::str(d.to_ixdtf_string(dc)))
    });
    vm.define_method(&proto, "toJSON", 0, |vm, this, _a| {
        let d = this_plain_date(vm, &this)?;
        Ok(Value::str(d.to_ixdtf_string(DisplayCalendar::Auto)))
    });
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let d = this_plain_date(vm, &this)?;
        Ok(Value::str(d.to_ixdtf_string(DisplayCalendar::Auto)))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, _this, _a| {
        Err(vm.throw_type("Temporal.PlainDate: use compare() instead of relational operators"))
    });

    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Temporal.PlainDate"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    temporal.borrow_mut().props.insert(
        PropertyKey::str("PlainDate"),
        Property::builtin(Value::Object(ctor)),
    );
}

fn define_date_getter(vm: &mut Vm, proto: &JsObject, name: &str, project: fn(&PlainDate) -> Value) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let d = this_plain_date(vm, &this)?;
        Ok(project(&d))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str(name),
        Some(Value::Object(getter)),
        None,
    );
}

// =========================================================================
// Temporal.Instant
// =========================================================================

use num_traits::ToPrimitive;
use temporal_rs::Instant;

/// The `temporal_rs::Instant` backing a receiver.
fn this_instant(vm: &mut Vm, this: &Value) -> Result<Instant, Value> {
    if let Value::Object(o) = this {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::Instant(i) = slot.as_ref() {
                return Ok(*i);
            }
        }
    }
    Err(vm.throw_type("receiver is not a Temporal.Instant"))
}

/// `ToBigInt` then narrow to `i128` (out-of-range epoch nanoseconds RangeError).
fn to_epoch_ns(vm: &mut Vm, v: &Value) -> Result<i128, Value> {
    let big = vm.to_bigint(v)?;
    big.to_i128()
        .ok_or_else(|| vm.throw_range("Temporal.Instant: epoch nanoseconds out of range"))
}

/// `ToTemporalInstant`: an Instant is copied; a string is parsed.
fn to_temporal_instant(vm: &mut Vm, v: &Value) -> Result<Instant, Value> {
    if let Value::Object(o) = v {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::Instant(i) = slot.as_ref() {
                return Ok(*i);
            }
        }
    }
    match v {
        Value::String(s) => {
            Instant::from_utf8(s.as_str().as_bytes()).map_err(|e| temporal_err(vm, e))
        }
        _ => Err(vm.throw_type("Temporal.Instant: expected an Instant or string")),
    }
}

fn construct_instant(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let ns = to_epoch_ns(vm, &arg(args, 0))?;
    let i = Instant::try_new(ns).map_err(|e| temporal_err(vm, e))?;
    Ok(new_temporal(vm, TemporalSlot::Instant(i), proto))
}

fn install_instant(vm: &mut Vm, temporal: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "Instant",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor Temporal.Instant requires 'new'")),
        move |vm, _this, args| construct_instant(vm, args, &ctor_proto),
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

    vm.define_method(&ctor, "from", 1, |vm, _t, args| {
        let i = to_temporal_instant(vm, &arg(args, 0))?;
        let proto = intrinsic_proto(vm, "Instant")?;
        Ok(new_temporal(vm, TemporalSlot::Instant(i), &proto))
    });
    vm.define_method(&ctor, "fromEpochMilliseconds", 1, |vm, _t, args| {
        let n = vm.to_number(&arg(args, 0))?;
        if !n.is_finite() || n.fract() != 0.0 {
            return Err(vm.throw_range("Temporal.Instant.fromEpochMilliseconds: not an integer"));
        }
        let i = Instant::from_epoch_milliseconds(n as i64).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Instant")?;
        Ok(new_temporal(vm, TemporalSlot::Instant(i), &proto))
    });
    vm.define_method(&ctor, "fromEpochNanoseconds", 1, |vm, _t, args| {
        let ns = to_epoch_ns(vm, &arg(args, 0))?;
        let i = Instant::try_new(ns).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Instant")?;
        Ok(new_temporal(vm, TemporalSlot::Instant(i), &proto))
    });
    vm.define_method(&ctor, "compare", 2, |vm, _t, args| {
        let a = to_temporal_instant(vm, &arg(args, 0))?;
        let b = to_temporal_instant(vm, &arg(args, 1))?;
        Ok(Value::Number(match a.as_i128().cmp(&b.as_i128()) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });

    let epoch_ms = vm.new_native("get epochMilliseconds", 0, |vm, this, _a| {
        let i = this_instant(vm, &this)?;
        Ok(Value::Number(i.epoch_milliseconds() as f64))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("epochMilliseconds"),
        Some(Value::Object(epoch_ms)),
        None,
    );
    let epoch_ns = vm.new_native("get epochNanoseconds", 0, |vm, this, _a| {
        let i = this_instant(vm, &this)?;
        Ok(Value::bigint(num_bigint::BigInt::from(i.as_i128())))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("epochNanoseconds"),
        Some(Value::Object(epoch_ns)),
        None,
    );

    vm.define_method(&proto, "add", 1, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let d = to_temporal_duration(vm, &arg(args, 0))?;
        let ni = i.add(&d).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Instant")?;
        Ok(new_temporal(vm, TemporalSlot::Instant(ni), &proto))
    });
    vm.define_method(&proto, "subtract", 1, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let d = to_temporal_duration(vm, &arg(args, 0))?;
        let ni = i.subtract(&d).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Instant")?;
        Ok(new_temporal(vm, TemporalSlot::Instant(ni), &proto))
    });
    vm.define_method(&proto, "until", 1, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let other = to_temporal_instant(vm, &arg(args, 0))?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let d = i.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(d), &proto))
    });
    vm.define_method(&proto, "since", 1, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let other = to_temporal_instant(vm, &arg(args, 0))?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let d = i.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(d), &proto))
    });
    vm.define_method(&proto, "round", 1, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let ro = get_rounding_options(vm, &arg(args, 0))?;
        let ni = i.round(ro).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Instant")?;
        Ok(new_temporal(vm, TemporalSlot::Instant(ni), &proto))
    });
    vm.define_method(&proto, "equals", 1, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let other = to_temporal_instant(vm, &arg(args, 0))?;
        Ok(Value::Bool(i.as_i128() == other.as_i128()))
    });
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let i = this_instant(vm, &this)?;
        let opts = to_string_rounding_options(vm, &arg(args, 0))?;
        // The `timeZone` toString option (UTC offset rendering) is not yet wired.
        Ok(Value::str(
            i.to_ixdtf_string(None, opts)
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "toJSON", 0, |vm, this, _a| {
        let i = this_instant(vm, &this)?;
        Ok(Value::str(
            i.to_ixdtf_string(None, Default::default())
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let i = this_instant(vm, &this)?;
        Ok(Value::str(
            i.to_ixdtf_string(None, Default::default())
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, _this, _a| {
        Err(vm.throw_type("Temporal.Instant: use compare() instead of relational operators"))
    });

    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Temporal.Instant"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    temporal.borrow_mut().props.insert(
        PropertyKey::str("Instant"),
        Property::builtin(Value::Object(ctor)),
    );
}

// =========================================================================
// Temporal.PlainDateTime
// =========================================================================

use temporal_rs::fields::DateTimeFields;
use temporal_rs::partial::PartialDateTime;
use temporal_rs::PlainDateTime;

/// The `temporal_rs::PlainDateTime` backing a receiver.
fn this_plain_date_time(vm: &mut Vm, this: &Value) -> Result<PlainDateTime, Value> {
    if let Value::Object(o) = this {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            if let TemporalSlot::PlainDateTime(d) = slot.as_ref() {
                return Ok(d.clone());
            }
        }
    }
    Err(vm.throw_type("receiver is not a Temporal.PlainDateTime"))
}

/// `ToTemporalDateTime`: a PlainDateTime is copied; a PlainDate is widened to
/// midnight; a string is parsed; an object's date+time fields build one.
fn to_temporal_date_time(
    vm: &mut Vm,
    v: &Value,
    overflow: Option<Overflow>,
) -> Result<PlainDateTime, Value> {
    if let Value::Object(o) = v {
        if let Internal::Temporal(slot) = &o.borrow().internal {
            match slot.as_ref() {
                TemporalSlot::PlainDateTime(d) => return Ok(d.clone()),
                TemporalSlot::PlainDate(d) => {
                    return PlainDateTime::from_date_and_time(d.clone(), PlainTime::default())
                        .map_err(|e| temporal_err(vm, e));
                }
                _ => {}
            }
        }
    }
    match v {
        Value::Object(_) => {
            let date = read_partial_date(vm, v)?;
            let (time, _) = read_partial_time(vm, v)?;
            let partial = PartialDateTime {
                fields: DateTimeFields {
                    calendar_fields: date.calendar_fields,
                    time,
                },
                calendar: date.calendar,
            };
            PlainDateTime::from_partial(partial, overflow).map_err(|e| temporal_err(vm, e))
        }
        Value::String(s) => {
            PlainDateTime::from_utf8(s.as_str().as_bytes()).map_err(|e| temporal_err(vm, e))
        }
        _ => Err(vm.throw_type("Temporal.PlainDateTime: expected a date-time, string, or object")),
    }
}

fn construct_plain_date_time(
    vm: &mut Vm,
    args: &[Value],
    proto: &JsObject,
) -> Result<Value, Value> {
    let year = to_integer_with_truncation(vm, &arg(args, 0))?;
    let month = to_integer_with_truncation(vm, &arg(args, 1))?;
    let day = to_integer_with_truncation(vm, &arg(args, 2))?;
    let mut t = [0.0f64; 6];
    for (i, item) in t.iter_mut().enumerate() {
        let a = arg(args, 3 + i);
        *item = if a.is_undefined() {
            0.0
        } else {
            to_integer_with_truncation(vm, &a)?
        };
    }
    let calendar = to_calendar(vm, &arg(args, 9))?;
    if !(i32::MIN as f64..=i32::MAX as f64).contains(&year)
        || month < 0.0
        || day < 0.0
        || t.iter().any(|&x| x < 0.0)
    {
        return Err(vm.throw_range("Temporal.PlainDateTime: field out of range"));
    }
    let d = PlainDateTime::try_new(
        year as i32,
        month as u8,
        day as u8,
        t[0] as u8,
        t[1] as u8,
        t[2] as u8,
        t[3] as u16,
        t[4] as u16,
        t[5] as u16,
        calendar,
    )
    .map_err(|e| temporal_err(vm, e))?;
    Ok(new_temporal(vm, TemporalSlot::PlainDateTime(d), proto))
}

fn install_plain_date_time(vm: &mut Vm, temporal: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "PlainDateTime",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor Temporal.PlainDateTime requires 'new'")),
        move |vm, _this, args| construct_plain_date_time(vm, args, &ctor_proto),
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

    vm.define_method(&ctor, "from", 1, |vm, _t, args| {
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let d = to_temporal_date_time(vm, &arg(args, 0), overflow)?;
        let proto = intrinsic_proto(vm, "PlainDateTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDateTime(d), &proto))
    });
    vm.define_method(&ctor, "compare", 2, |vm, _t, args| {
        let a = to_temporal_date_time(vm, &arg(args, 0), None)?;
        let b = to_temporal_date_time(vm, &arg(args, 1), None)?;
        Ok(Value::Number(match a.compare_iso(&b) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });

    define_dt_getter(vm, &proto, "year", |d| Value::Number(d.year() as f64));
    define_dt_getter(vm, &proto, "month", |d| Value::Number(d.month() as f64));
    define_dt_getter(vm, &proto, "monthCode", |d| {
        Value::str(d.month_code().as_str())
    });
    define_dt_getter(vm, &proto, "day", |d| Value::Number(d.day() as f64));
    define_dt_getter(vm, &proto, "hour", |d| Value::Number(d.hour() as f64));
    define_dt_getter(vm, &proto, "minute", |d| Value::Number(d.minute() as f64));
    define_dt_getter(vm, &proto, "second", |d| Value::Number(d.second() as f64));
    define_dt_getter(vm, &proto, "millisecond", |d| {
        Value::Number(d.millisecond() as f64)
    });
    define_dt_getter(vm, &proto, "microsecond", |d| {
        Value::Number(d.microsecond() as f64)
    });
    define_dt_getter(vm, &proto, "nanosecond", |d| {
        Value::Number(d.nanosecond() as f64)
    });
    define_dt_getter(vm, &proto, "dayOfWeek", |d| {
        Value::Number(d.day_of_week() as f64)
    });
    define_dt_getter(vm, &proto, "dayOfYear", |d| {
        Value::Number(d.day_of_year() as f64)
    });
    define_dt_getter(vm, &proto, "weekOfYear", |d| {
        d.week_of_year()
            .map(|w| Value::Number(w as f64))
            .unwrap_or(Value::Undefined)
    });
    define_dt_getter(vm, &proto, "yearOfWeek", |d| {
        d.year_of_week()
            .map(|y| Value::Number(y as f64))
            .unwrap_or(Value::Undefined)
    });
    define_dt_getter(vm, &proto, "daysInWeek", |d| {
        Value::Number(d.days_in_week() as f64)
    });
    define_dt_getter(vm, &proto, "daysInMonth", |d| {
        Value::Number(d.days_in_month() as f64)
    });
    define_dt_getter(vm, &proto, "daysInYear", |d| {
        Value::Number(d.days_in_year() as f64)
    });
    define_dt_getter(vm, &proto, "monthsInYear", |d| {
        Value::Number(d.months_in_year() as f64)
    });
    define_dt_getter(vm, &proto, "inLeapYear", |d| Value::Bool(d.in_leap_year()));
    define_dt_getter(vm, &proto, "era", |d| {
        d.era()
            .map(|e| Value::str(e.as_str()))
            .unwrap_or(Value::Undefined)
    });
    define_dt_getter(vm, &proto, "eraYear", |d| {
        d.era_year()
            .map(|y| Value::Number(y as f64))
            .unwrap_or(Value::Undefined)
    });
    define_dt_getter(vm, &proto, "calendarId", |d| {
        Value::str(d.calendar().identifier())
    });

    vm.define_method(&proto, "add", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let dur = to_temporal_duration(vm, &arg(args, 0))?;
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nd = d.add(&dur, overflow).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDateTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDateTime(nd), &proto))
    });
    vm.define_method(&proto, "subtract", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let dur = to_temporal_duration(vm, &arg(args, 0))?;
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nd = d
            .subtract(&dur, overflow)
            .map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDateTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDateTime(nd), &proto))
    });
    vm.define_method(&proto, "with", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let item = arg(args, 0);
        if !matches!(item, Value::Object(_)) {
            return Err(
                vm.throw_type("Temporal.PlainDateTime.prototype.with: argument must be an object")
            );
        }
        let date = read_partial_date(vm, &item)?;
        let (time, _) = read_partial_time(vm, &item)?;
        let fields = DateTimeFields {
            calendar_fields: date.calendar_fields,
            time,
        };
        let overflow = get_overflow(vm, &arg(args, 1))?;
        let nd = d.with(fields, overflow).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDateTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDateTime(nd), &proto))
    });
    vm.define_method(&proto, "until", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let other = to_temporal_date_time(vm, &arg(args, 0), None)?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let dur = d.until(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(dur), &proto))
    });
    vm.define_method(&proto, "since", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let other = to_temporal_date_time(vm, &arg(args, 0), None)?;
        let settings = get_difference_settings(vm, &arg(args, 1))?;
        let dur = d.since(&other, settings).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "Duration")?;
        Ok(new_temporal(vm, TemporalSlot::Duration(dur), &proto))
    });
    vm.define_method(&proto, "round", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let ro = get_rounding_options(vm, &arg(args, 0))?;
        let nd = d.round(ro).map_err(|e| temporal_err(vm, e))?;
        let proto = intrinsic_proto(vm, "PlainDateTime")?;
        Ok(new_temporal(vm, TemporalSlot::PlainDateTime(nd), &proto))
    });
    vm.define_method(&proto, "equals", 1, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let other = to_temporal_date_time(vm, &arg(args, 0), None)?;
        let eq = d.compare_iso(&other) == std::cmp::Ordering::Equal
            && d.calendar().identifier() == other.calendar().identifier();
        Ok(Value::Bool(eq))
    });
    vm.define_method(&proto, "toPlainDate", 0, |vm, this, _a| {
        let d = this_plain_date_time(vm, &this)?;
        let proto = intrinsic_proto(vm, "PlainDate")?;
        Ok(new_temporal(
            vm,
            TemporalSlot::PlainDate(d.to_plain_date()),
            &proto,
        ))
    });
    vm.define_method(&proto, "toPlainTime", 0, |vm, this, _a| {
        let d = this_plain_date_time(vm, &this)?;
        let proto = intrinsic_proto(vm, "PlainTime")?;
        Ok(new_temporal(
            vm,
            TemporalSlot::PlainTime(d.to_plain_time()),
            &proto,
        ))
    });
    vm.define_method(&proto, "toString", 0, |vm, this, args| {
        let d = this_plain_date_time(vm, &this)?;
        let opts = to_string_rounding_options(vm, &arg(args, 0))?;
        let dc = get_display_calendar(vm, &arg(args, 0))?;
        Ok(Value::str(
            d.to_ixdtf_string(opts, dc)
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "toJSON", 0, |vm, this, _a| {
        let d = this_plain_date_time(vm, &this)?;
        Ok(Value::str(
            d.to_ixdtf_string(Default::default(), DisplayCalendar::Auto)
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let d = this_plain_date_time(vm, &this)?;
        Ok(Value::str(
            d.to_ixdtf_string(Default::default(), DisplayCalendar::Auto)
                .map_err(|e| temporal_err(vm, e))?,
        ))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, _this, _a| {
        Err(vm.throw_type("Temporal.PlainDateTime: use compare() instead of relational operators"))
    });

    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Temporal.PlainDateTime"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    temporal.borrow_mut().props.insert(
        PropertyKey::str("PlainDateTime"),
        Property::builtin(Value::Object(ctor)),
    );
}

fn define_dt_getter(
    vm: &mut Vm,
    proto: &JsObject,
    name: &str,
    project: fn(&PlainDateTime) -> Value,
) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let d = this_plain_date_time(vm, &this)?;
        Ok(project(&d))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str(name),
        Some(Value::Object(getter)),
        None,
    );
}
