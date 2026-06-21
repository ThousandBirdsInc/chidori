//! The `Intl` namespace: `Intl.getCanonicalLocales` and the `Intl.Locale`
//! constructor + prototype. Locale identifier parsing, canonicalization, and
//! likely-subtags maximize/minimize are delegated to ICU4X (`icu_locale_core`
//! for the data model and `icu_locale` for the CLDR-data-backed
//! canonicalizer / expander). The locale-info accessors (`getCalendars`,
//! `getTextInfo`, `getWeekInfo`, …) belong to the separate `Intl.Locale-info`
//! proposal and are not implemented here.

use super::arg;
use crate::value::*;
use crate::vm::Vm;

use icu_locale_core::extensions::unicode::{Key, Value as UValue};
use icu_locale_core::subtags::{Language, Region, Script, Variant, Variants};
use icu_locale_core::Locale;
use std::str::FromStr;

thread_local! {
    static CANONICALIZER: icu_locale::LocaleCanonicalizer =
        icu_locale::LocaleCanonicalizer::new_extended();
    static EXPANDER: icu_locale::LocaleExpander = icu_locale::LocaleExpander::new_extended();
}

pub fn install(vm: &mut Vm) {
    let intl = vm.new_object();

    vm.define_method(&intl, "getCanonicalLocales", 1, |vm, _t, args| {
        let list = canonicalize_locale_list(vm, &arg(args, 0))?;
        let vals: Vec<Value> = list.into_iter().map(Value::str).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });

    install_locale(vm, &intl);
    install_plural_rules(vm, &intl);

    // Intl[Symbol.toStringTag] = "Intl" (non-writable, non-enumerable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    intl.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Intl"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    vm.define_value(&vm.realm.global.clone(), "Intl", Value::Object(intl));
}

// =========================================================================
// Locale-list canonicalization (shared by getCanonicalLocales and the
// formatter constructors' `locales` argument)
// =========================================================================

/// `CanonicalizeUnicodeLocaleId`: structural validity + CLDR canonicalization,
/// returning the canonical tag, or `None` when the tag is not structurally valid.
fn canonicalize_tag(tag: &str) -> Option<String> {
    let mut loc = Locale::try_from_str(tag).ok()?;
    CANONICALIZER.with(|c| c.canonicalize(&mut loc));
    Some(loc.to_string())
}

/// `CanonicalizeLocaleList(locales)` (ECMA-402): coerce `locales` to a
/// deduplicated list of canonical, structurally valid language tags.
fn canonicalize_locale_list(vm: &mut Vm, locales: &Value) -> Result<Vec<String>, Value> {
    let mut seen: Vec<String> = Vec::new();
    if locales.is_undefined() {
        return Ok(seen);
    }
    // A String primitive or an Intl.Locale is treated as a single-element list;
    // anything else is coerced to an array-like object and iterated. Each element
    // is read and processed in order (so an element's `toString` side effect is
    // visible to later `HasProperty`/`Get` calls — the spec's observable order).
    if matches!(locales, Value::String(_)) || is_locale(vm, locales) {
        process_locale(vm, locales, &mut seen)?;
        return Ok(seen);
    }
    let o = Value::Object(vm.to_object(locales)?);
    let len_v = vm.get_prop(&o, &PropertyKey::str("length"))?;
    let len = to_length(vm, &len_v)?;
    for k in 0..len {
        let key = PropertyKey::str(k.to_string());
        if vm.has_prop(&o, &key)? {
            let kv = vm.get_prop(&o, &key)?;
            process_locale(vm, &kv, &mut seen)?;
        }
    }
    Ok(seen)
}

/// Validate one `locales`-list element (String or Intl.Locale or
/// `ToString`-able object), canonicalize it, and append if unseen.
fn process_locale(vm: &mut Vm, kv: &Value, seen: &mut Vec<String>) -> Result<(), Value> {
    let tag = if let Some(loc) = locale_internal(vm, kv) {
        loc
    } else if matches!(kv, Value::String(_) | Value::Object(_)) {
        vm.to_js_string(kv)?.as_str().to_owned()
    } else {
        return Err(vm.throw_type("locale list elements must be strings or objects"));
    };
    let canon = canonicalize_tag(&tag)
        .ok_or_else(|| vm.throw_range(&format!("invalid language tag: {tag}")))?;
    if !seen.contains(&canon) {
        seen.push(canon);
    }
    Ok(())
}

/// `ToLength`: clamp to `[0, 2^53 - 1]`.
fn to_length(vm: &mut Vm, v: &Value) -> Result<u64, Value> {
    let n = vm.to_number(v)?;
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    let n = n.trunc().min(9007199254740991.0);
    Ok(n as u64)
}

// =========================================================================
// Intl.Locale
// =========================================================================

/// True when `v` is an object carrying the `[[InitializedLocale]]` brand.
fn is_locale(vm: &Vm, v: &Value) -> bool {
    matches!(v, Value::Object(o)
        if o.borrow().props.contains_key(&PropertyKey::Sym(vm.realm.symbol_intl_locale.clone())))
}

/// The stored `[[Locale]]` canonical tag of an Intl.Locale, if `v` is one.
fn locale_internal(vm: &Vm, v: &Value) -> Option<String> {
    let Value::Object(o) = v else { return None };
    match o
        .borrow()
        .props
        .get(&PropertyKey::Sym(vm.realm.symbol_intl_locale.clone()))
    {
        Some(Property {
            kind:
                PropertyKind::Data {
                    value: Value::String(s),
                    ..
                },
            ..
        }) => Some(s.as_str().to_owned()),
        _ => None,
    }
}

/// Parse the (always-valid) stored `[[Locale]]` of an Intl.Locale receiver.
fn locale_of(vm: &mut Vm, this: &Value) -> Result<Locale, Value> {
    match locale_internal(vm, this) {
        // The stored `[[Locale]]` is always a canonical tag we produced.
        Some(s) => Ok(Locale::try_from_str(&s).unwrap_or(Locale::UNKNOWN)),
        None => Err(vm.throw_type("method called on a non-Intl.Locale object")),
    }
}

/// Allocate a fresh Intl.Locale object with prototype `proto`, branded with the
/// canonical `tag` string.
fn new_locale_object(vm: &Vm, tag: String, proto: &JsObject) -> Value {
    let o = vm.alloc(ObjectData::new(Some(proto.clone()), Internal::Ordinary));
    o.borrow_mut().props.insert(
        PropertyKey::Sym(vm.realm.symbol_intl_locale.clone()),
        Property {
            kind: PropertyKind::Data {
                value: Value::str(tag),
                writable: false,
            },
            enumerable: false,
            configurable: false,
        },
    );
    Value::Object(o)
}

/// `GetOption(options, prop, "string", undefined, undefined)`: `Get` then
/// `ToString`, or `None` when absent.
fn get_string_option(vm: &mut Vm, options: &Value, prop: &str) -> Result<Option<String>, Value> {
    let v = vm.get_prop(options, &PropertyKey::str(prop))?;
    if v.is_undefined() {
        return Ok(None);
    }
    Ok(Some(vm.to_js_string(&v)?.as_str().to_owned()))
}

/// `GetOption` with a fixed set of allowed string values (RangeError otherwise).
fn get_enum_option(
    vm: &mut Vm,
    options: &Value,
    prop: &str,
    allowed: &[&str],
) -> Result<Option<String>, Value> {
    match get_string_option(vm, options, prop)? {
        None => Ok(None),
        Some(s) => {
            if allowed.contains(&s.as_str()) {
                Ok(Some(s))
            } else {
                Err(vm.throw_range(&format!("invalid value '{s}' for option '{prop}'")))
            }
        }
    }
}

/// Whether `s` is a well-formed Unicode extension `type` value
/// (one or more `3*8alphanum` subtags joined by `-`).
fn is_unicode_type(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|seg| {
            (3..=8).contains(&seg.len()) && seg.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Apply a validated keyword option (`ca`, `co`, `nu`) to the locale.
fn apply_type_keyword(
    vm: &mut Vm,
    loc: &mut Locale,
    options: &Value,
    prop: &str,
    key: &str,
) -> Result<(), Value> {
    if let Some(s) = get_string_option(vm, options, prop)? {
        if !is_unicode_type(&s) {
            return Err(vm.throw_range(&format!("invalid value '{s}' for option '{prop}'")));
        }
        set_keyword(loc, key, &s);
    }
    Ok(())
}

fn set_keyword(loc: &mut Locale, key: &str, value: &str) {
    if let (Ok(k), Ok(v)) = (Key::from_str(key), UValue::from_str(value)) {
        loc.extensions.unicode.keywords.set(k, v);
    }
}

fn get_keyword(loc: &Locale, key: &str) -> Option<String> {
    let k = Key::from_str(key).ok()?;
    loc.extensions
        .unicode
        .keywords
        .get(&k)
        .map(|v| v.to_string())
}

/// `Intl.Locale(tag [, options])`.
fn construct_locale(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let tag_arg = arg(args, 0);
    // Step 7: tag must be a String or Object.
    let mut tag = if let Some(loc) = locale_internal(vm, &tag_arg) {
        loc
    } else if matches!(tag_arg, Value::String(_) | Value::Object(_)) {
        vm.to_js_string(&tag_arg)?.as_str().to_owned()
    } else {
        return Err(vm.throw_type("Intl.Locale: first argument must be a string or object"));
    };

    // CoerceOptionsToObject: undefined → an empty (null-proto) options bag.
    let opts_arg = arg(args, 1);
    let options = if opts_arg.is_undefined() {
        Value::Object(vm.new_object())
    } else {
        Value::Object(vm.to_object(&opts_arg)?)
    };

    // ApplyOptionsToTag: validate the tag, then override language/script/region.
    let mut loc = Locale::try_from_str(&tag)
        .map_err(|_| vm.throw_range(&format!("invalid language tag: {tag}")))?;

    if let Some(s) = get_string_option(vm, &options, "language")? {
        loc.id.language =
            Language::from_str(&s).map_err(|_| vm.throw_range("invalid 'language' option"))?;
    }
    if let Some(s) = get_string_option(vm, &options, "script")? {
        loc.id.script =
            Some(Script::from_str(&s).map_err(|_| vm.throw_range("invalid 'script' option"))?);
    }
    if let Some(s) = get_string_option(vm, &options, "region")? {
        loc.id.region =
            Some(Region::from_str(&s).map_err(|_| vm.throw_range("invalid 'region' option"))?);
    }
    if let Some(s) = get_string_option(vm, &options, "variants")? {
        // A `-`-joined list of variant subtags; icu re-sorts them on Display.
        // Duplicates are a structurally invalid tag (RangeError).
        let mut vars = Vec::new();
        for seg in s.split('-') {
            let v =
                Variant::from_str(seg).map_err(|_| vm.throw_range("invalid 'variants' option"))?;
            if vars.contains(&v) {
                return Err(vm.throw_range("duplicate subtag in 'variants' option"));
            }
            vars.push(v);
        }
        loc.id.variants = Variants::from_vec_unchecked(vars);
    }

    // Unicode extension keyword options, read in spec order: ca, co, hc, kf, kn,
    // nu (constructor-getter-order observes each `Get`).
    apply_type_keyword(vm, &mut loc, &options, "calendar", "ca")?;
    apply_type_keyword(vm, &mut loc, &options, "collation", "co")?;
    if let Some(hc) = get_enum_option(vm, &options, "hourCycle", &["h11", "h12", "h23", "h24"])? {
        set_keyword(&mut loc, "hc", &hc);
    }
    if let Some(kf) = get_enum_option(vm, &options, "caseFirst", &["upper", "lower", "false"])? {
        set_keyword(&mut loc, "kf", &kf);
    }
    // numeric: a boolean mapped onto the `kn` keyword.
    let numeric_v = vm.get_prop(&options, &PropertyKey::str("numeric"))?;
    if !numeric_v.is_undefined() {
        let on = vm.to_boolean(&numeric_v);
        set_keyword(&mut loc, "kn", if on { "true" } else { "false" });
    }
    apply_type_keyword(vm, &mut loc, &options, "numberingSystem", "nu")?;

    CANONICALIZER.with(|c| c.canonicalize(&mut loc));
    tag = loc.to_string();
    Ok(new_locale_object(vm, tag, proto))
}

fn install_locale(vm: &mut Vm, intl: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "Locale",
        1,
        |vm, _t, _a| Err(vm.throw_type("Constructor Intl.Locale requires 'new'")),
        move |vm, _this, args| construct_locale(vm, args, &ctor_proto),
    );

    // Intl.Locale.prototype (non-writable, non-enumerable, non-configurable) and
    // its back-reference.
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

    // Accessors.
    define_locale_getter(vm, &proto, "baseName", |_vm, loc| {
        Some(Value::str(loc.id.to_string()))
    });
    define_locale_getter(vm, &proto, "language", |_vm, loc| {
        Some(Value::str(loc.id.language.to_string()))
    });
    define_locale_getter(vm, &proto, "script", |_vm, loc| {
        loc.id.script.map(|s| Value::str(s.to_string()))
    });
    define_locale_getter(vm, &proto, "region", |_vm, loc| {
        loc.id.region.map(|r| Value::str(r.to_string()))
    });
    define_locale_getter(vm, &proto, "variants", |_vm, loc| {
        if loc.id.variants.is_empty() {
            None
        } else {
            let v: Vec<String> = loc.id.variants.iter().map(|x| x.to_string()).collect();
            Some(Value::str(v.join("-")))
        }
    });
    define_locale_getter(vm, &proto, "calendar", |_vm, loc| {
        get_keyword(loc, "ca").map(Value::str)
    });
    define_locale_getter(vm, &proto, "collation", |_vm, loc| {
        get_keyword(loc, "co").map(Value::str)
    });
    define_locale_getter(vm, &proto, "hourCycle", |_vm, loc| {
        get_keyword(loc, "hc").map(Value::str)
    });
    define_locale_getter(vm, &proto, "caseFirst", |_vm, loc| {
        get_keyword(loc, "kf").map(Value::str)
    });
    define_locale_getter(vm, &proto, "numeric", |_vm, loc| {
        Some(Value::Bool(matches!(
            get_keyword(loc, "kn").as_deref(),
            Some("") | Some("true")
        )))
    });
    define_locale_getter(vm, &proto, "numberingSystem", |_vm, loc| {
        get_keyword(loc, "nu").map(Value::str)
    });

    // Methods.
    let proto_max = proto.clone();
    vm.define_method(&proto, "maximize", 0, move |vm, this, _a| {
        let mut loc = locale_of(vm, &this)?;
        EXPANDER.with(|e| {
            e.maximize(&mut loc.id);
        });
        CANONICALIZER.with(|c| c.canonicalize(&mut loc));
        Ok(new_locale_object(vm, loc.to_string(), &proto_max))
    });
    let proto_min = proto.clone();
    vm.define_method(&proto, "minimize", 0, move |vm, this, _a| {
        let mut loc = locale_of(vm, &this)?;
        EXPANDER.with(|e| {
            e.minimize(&mut loc.id);
        });
        CANONICALIZER.with(|c| c.canonicalize(&mut loc));
        Ok(new_locale_object(vm, loc.to_string(), &proto_min))
    });
    vm.define_method(
        &proto,
        "toString",
        0,
        |vm, this, _a| match locale_internal(vm, &this) {
            Some(s) => Ok(Value::str(s)),
            None => {
                Err(vm.throw_type("Intl.Locale.prototype.toString called on incompatible receiver"))
            }
        },
    );

    // Intl.Locale.prototype[Symbol.toStringTag] = "Intl.Locale"
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Intl.Locale"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    intl.borrow_mut().props.insert(
        PropertyKey::str("Locale"),
        Property::builtin(Value::Object(ctor)),
    );
}

/// Define a non-enumerable, configurable `get`-only accessor on the Locale
/// prototype whose body parses the receiver and projects a field.
fn define_locale_getter(
    vm: &mut Vm,
    proto: &JsObject,
    name: &str,
    project: fn(&mut Vm, &Locale) -> Option<Value>,
) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let loc = locale_of(vm, &this)?;
        Ok(project(vm, &loc).unwrap_or(Value::Undefined))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str(name),
        Some(Value::Object(getter)),
        None,
    );
}

// =========================================================================
// Intl.PluralRules
// =========================================================================

use fixed_decimal::{Decimal, FloatPrecision, SignedRoundingMode, UnsignedRoundingMode};
use icu_plurals::{PluralCategory, PluralRuleType, PluralRules};

/// `CoerceOptionsToObject`: undefined → an empty **null-prototype** options bag
/// (so a polluted `Object.prototype` is not consulted), else `ToObject`.
fn coerce_options(vm: &mut Vm, options: Value) -> Result<Value, Value> {
    if options.is_undefined() {
        Ok(Value::Object(
            vm.alloc(ObjectData::new(None, Internal::Ordinary)),
        ))
    } else {
        Ok(Value::Object(vm.to_object(&options)?))
    }
}

/// Insert an enumerable, writable, configurable data property (the
/// `CreateDataPropertyOrThrow` used to build a `resolvedOptions` result).
fn data_prop(target: &JsObject, key: &str, value: Value) {
    target.borrow_mut().props.insert(
        PropertyKey::str(key),
        Property {
            kind: PropertyKind::Data {
                value,
                writable: true,
            },
            enumerable: true,
            configurable: true,
        },
    );
}

/// `DefaultNumberOption`: coerce, range-check `[min, max]`, and floor.
fn default_number_option(
    vm: &mut Vm,
    v: &Value,
    min: f64,
    max: f64,
    default: f64,
) -> Result<f64, Value> {
    if v.is_undefined() {
        return Ok(default);
    }
    let n = vm.to_number(v)?;
    if n.is_nan() || n < min || n > max {
        return Err(vm.throw_range("numeric option out of range"));
    }
    Ok(n.floor())
}

/// `GetNumberOption(options, prop, min, max, default)`.
fn get_number_option(
    vm: &mut Vm,
    options: &Value,
    prop: &str,
    min: f64,
    max: f64,
    default: f64,
) -> Result<f64, Value> {
    let v = vm.get_prop(options, &PropertyKey::str(prop))?;
    default_number_option(vm, &v, min, max, default)
}

/// The resolved digit options of a number-formatting consumer (the subset
/// `Intl.PluralRules` needs: integer/fraction digits and the optional
/// significant-digit override).
#[derive(Clone, Copy)]
struct DigitOptions {
    min_integer: u32,
    min_fraction: u32,
    max_fraction: u32,
    min_significant: Option<u32>,
    max_significant: Option<u32>,
}

/// `SetNumberFormatDigitOptions` with `mnfdDefault = 0`, `mxfdDefault = 3`,
/// `notation = "standard"` (the `Intl.PluralRules` configuration).
fn set_digit_options(vm: &mut Vm, options: &Value) -> Result<DigitOptions, Value> {
    let min_integer =
        get_number_option(vm, options, "minimumIntegerDigits", 1.0, 21.0, 1.0)? as u32;
    let mnfd = vm.get_prop(options, &PropertyKey::str("minimumFractionDigits"))?;
    let mxfd = vm.get_prop(options, &PropertyKey::str("maximumFractionDigits"))?;
    let mnsd = vm.get_prop(options, &PropertyKey::str("minimumSignificantDigits"))?;
    let mxsd = vm.get_prop(options, &PropertyKey::str("maximumSignificantDigits"))?;

    if !mnsd.is_undefined() || !mxsd.is_undefined() {
        let min_s = default_number_option(vm, &mnsd, 1.0, 21.0, 1.0)? as u32;
        let max_s = default_number_option(vm, &mxsd, min_s as f64, 21.0, 21.0)? as u32;
        return Ok(DigitOptions {
            min_integer,
            min_fraction: 0,
            max_fraction: 0,
            min_significant: Some(min_s),
            max_significant: Some(max_s),
        });
    }
    let (min_fraction, max_fraction) = if !mnfd.is_undefined() || !mxfd.is_undefined() {
        let min_f = default_number_option(vm, &mnfd, 0.0, 100.0, 0.0)? as u32;
        let mxfd_default = min_f.max(3);
        let max_f =
            default_number_option(vm, &mxfd, min_f as f64, 100.0, mxfd_default as f64)? as u32;
        (min_f, max_f)
    } else {
        (0, 3)
    };
    Ok(DigitOptions {
        min_integer,
        min_fraction,
        max_fraction,
        min_significant: None,
        max_significant: None,
    })
}

/// Read a string field of the internal PluralRules record.
fn rec_str(rec: &JsObject, key: &str) -> String {
    match rec.borrow().props.get(&PropertyKey::str(key)) {
        Some(Property {
            kind:
                PropertyKind::Data {
                    value: Value::String(s),
                    ..
                },
            ..
        }) => s.as_str().to_owned(),
        _ => String::new(),
    }
}

/// Read a numeric field of the internal PluralRules record (`None` if absent).
fn rec_num(rec: &JsObject, key: &str) -> Option<u32> {
    match rec.borrow().props.get(&PropertyKey::str(key)) {
        Some(Property {
            kind:
                PropertyKind::Data {
                    value: Value::Number(n),
                    ..
                },
            ..
        }) => Some(*n as u32),
        _ => None,
    }
}

/// The internal record object of an Intl.PluralRules receiver, if `this` is one.
fn plural_record(vm: &Vm, this: &Value) -> Option<JsObject> {
    let Value::Object(o) = this else { return None };
    match o
        .borrow()
        .props
        .get(&PropertyKey::Sym(vm.realm.symbol_intl_plural_rules.clone()))
    {
        Some(Property {
            kind:
                PropertyKind::Data {
                    value: Value::Object(rec),
                    ..
                },
            ..
        }) => Some(rec.clone()),
        _ => None,
    }
}

fn rule_type(rec: &JsObject) -> PluralRuleType {
    if rec_str(rec, "type") == "ordinal" {
        PluralRuleType::Ordinal
    } else {
        PluralRuleType::Cardinal
    }
}

fn build_rules(rec: &JsObject) -> PluralRules {
    let loc = Locale::try_from_str(&rec_str(rec, "locale")).unwrap_or(Locale::UNKNOWN);
    PluralRules::try_new((&loc.id).into(), rule_type(rec).into()).unwrap_or_else(|_| {
        PluralRules::try_new(Default::default(), rule_type(rec).into()).unwrap()
    })
}

fn category_name(c: PluralCategory) -> &'static str {
    match c {
        PluralCategory::Zero => "zero",
        PluralCategory::One => "one",
        PluralCategory::Two => "two",
        PluralCategory::Few => "few",
        PluralCategory::Many => "many",
        PluralCategory::Other => "other",
    }
}

/// Format `|n|` into a `Decimal` carrying the visible digits implied by the
/// record's digit options (the plural-operand input), or `None` for non-finite.
fn operand_decimal(rec: &JsObject, n: f64) -> Option<Decimal> {
    if !n.is_finite() {
        return None;
    }
    let opts = DigitOptions {
        min_integer: rec_num(rec, "minimumIntegerDigits").unwrap_or(1),
        min_fraction: rec_num(rec, "minimumFractionDigits").unwrap_or(0),
        max_fraction: rec_num(rec, "maximumFractionDigits").unwrap_or(3),
        min_significant: rec_num(rec, "minimumSignificantDigits"),
        max_significant: rec_num(rec, "maximumSignificantDigits"),
    };
    let mut dec = Decimal::try_from_f64(n.abs(), FloatPrecision::RoundTrip).ok()?;
    let half_expand = SignedRoundingMode::Unsigned(UnsignedRoundingMode::HalfExpand);
    if let Some(max_s) = opts.max_significant {
        let mag = dec.nonzero_magnitude_start();
        dec.round_with_mode(mag - max_s as i16 + 1, half_expand);
        if let Some(min_s) = opts.min_significant {
            dec.pad_end(mag - min_s as i16 + 1);
        }
    } else {
        dec.round_with_mode(-(opts.max_fraction as i16), half_expand);
        dec.pad_end(-(opts.min_fraction as i16));
    }
    dec.pad_start(opts.min_integer as i16);
    Some(dec)
}

/// Resolve the plural category name of `n` under the record's options.
fn resolve_plural(rec: &JsObject, n: f64) -> &'static str {
    match operand_decimal(rec, n) {
        Some(dec) => category_name(build_rules(rec).category_for(&dec)),
        None => "other",
    }
}

fn construct_plural_rules(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let requested = canonicalize_locale_list(vm, &arg(args, 0))?;
    let options = coerce_options(vm, arg(args, 1))?;

    // Options are read in spec order (constructor-option-read-order observes
    // every `Get`); most are validated then ignored by this implementation.
    get_enum_option(vm, &options, "localeMatcher", &["lookup", "best fit"])?;
    let typ = get_enum_option(vm, &options, "type", &["cardinal", "ordinal"])?
        .unwrap_or_else(|| "cardinal".to_string());
    let notation = get_enum_option(
        vm,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
    )?
    .unwrap_or_else(|| "standard".to_string());
    let compact_display = get_enum_option(vm, &options, "compactDisplay", &["short", "long"])?
        .unwrap_or_else(|| "short".to_string());
    let digits = set_digit_options(vm, &options)?;
    // Rounding options (read for order/validation; defaults are reported as-is).
    get_number_option(vm, &options, "roundingIncrement", 1.0, 5000.0, 1.0)?;
    get_enum_option(
        vm,
        &options,
        "roundingMode",
        &[
            "ceil",
            "floor",
            "expand",
            "trunc",
            "halfCeil",
            "halfFloor",
            "halfExpand",
            "halfTrunc",
            "halfEven",
        ],
    )?;
    get_enum_option(
        vm,
        &options,
        "roundingPriority",
        &["auto", "morePrecision", "lessPrecision"],
    )?;
    get_enum_option(
        vm,
        &options,
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
    )?;

    // ResolveLocale (lookup): the first requested tag that parses, else the
    // default. ICU4X supplies plural data for the language, falling back to
    // root, so the language-level base name is the resolved locale.
    let locale = requested
        .iter()
        .find_map(|t| Locale::try_from_str(t).ok())
        .map(|l| l.id.to_string())
        .unwrap_or_else(|| "en".to_string());

    let rec = vm.new_object();
    {
        let mut b = rec.borrow_mut();
        let mut put = |k: &str, v: Value| {
            b.props.insert(PropertyKey::str(k), Property::builtin(v));
        };
        put("locale", Value::str(locale));
        put("type", Value::str(typ));
        put("notation", Value::str(notation.clone()));
        if notation == "compact" {
            put("compactDisplay", Value::str(compact_display));
        }
        put(
            "minimumIntegerDigits",
            Value::Number(digits.min_integer as f64),
        );
        if let (Some(min_s), Some(max_s)) = (digits.min_significant, digits.max_significant) {
            put("minimumSignificantDigits", Value::Number(min_s as f64));
            put("maximumSignificantDigits", Value::Number(max_s as f64));
        } else {
            put(
                "minimumFractionDigits",
                Value::Number(digits.min_fraction as f64),
            );
            put(
                "maximumFractionDigits",
                Value::Number(digits.max_fraction as f64),
            );
        }
    }

    let o = vm.alloc(ObjectData::new(Some(proto.clone()), Internal::Ordinary));
    o.borrow_mut().props.insert(
        PropertyKey::Sym(vm.realm.symbol_intl_plural_rules.clone()),
        Property {
            kind: PropertyKind::Data {
                value: Value::Object(rec),
                writable: false,
            },
            enumerable: false,
            configurable: false,
        },
    );
    Ok(Value::Object(o))
}

fn install_plural_rules(vm: &mut Vm, intl: &JsObject) {
    let proto = vm.new_object();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "PluralRules",
        0,
        |vm, _t, _a| Err(vm.throw_type("Constructor Intl.PluralRules requires 'new'")),
        move |vm, _this, args| construct_plural_rules(vm, args, &ctor_proto),
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

    // Intl.PluralRules.supportedLocalesOf(locales) — the canonicalized requested
    // list (ICU4X supports every language via root fallback).
    vm.define_method(&ctor, "supportedLocalesOf", 1, |vm, _t, args| {
        let list = canonicalize_locale_list(vm, &arg(args, 0))?;
        let vals: Vec<Value> = list.into_iter().map(Value::str).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });

    vm.define_method(&proto, "select", 1, |vm, this, args| {
        let rec = plural_record(vm, &this).ok_or_else(|| {
            vm.throw_type("Intl.PluralRules.prototype.select on incompatible receiver")
        })?;
        let n = vm.to_number(&arg(args, 0))?;
        Ok(Value::str(resolve_plural(&rec, n)))
    });

    vm.define_method(&proto, "selectRange", 2, |vm, this, args| {
        let rec = plural_record(vm, &this).ok_or_else(|| {
            vm.throw_type("Intl.PluralRules.prototype.selectRange on incompatible receiver")
        })?;
        if arg(args, 0).is_undefined() || arg(args, 1).is_undefined() {
            return Err(
                vm.throw_type("Intl.PluralRules.prototype.selectRange: start and end are required")
            );
        }
        let x = vm.to_number(&arg(args, 0))?;
        let y = vm.to_number(&arg(args, 1))?;
        if x.is_nan() || y.is_nan() {
            return Err(
                vm.throw_range("Intl.PluralRules.prototype.selectRange: arguments must be numbers")
            );
        }
        // PluralRuleSelectRange proper needs the CLDR plural-range table (only in
        // ICU4X's `unstable` surface); approximate with the end value's category.
        Ok(Value::str(resolve_plural(&rec, y)))
    });

    vm.define_method(&proto, "resolvedOptions", 0, |vm, this, _a| {
        let rec = plural_record(vm, &this).ok_or_else(|| {
            vm.throw_type("Intl.PluralRules.prototype.resolvedOptions on incompatible receiver")
        })?;
        let out = vm.new_object();
        // Enumerable data properties, emitted in the spec's resolvedOptions order:
        // locale, type, notation, [compactDisplay,] minimumIntegerDigits,
        // {fraction | significant} digits, pluralCategories, rounding*.
        data_prop(&out, "locale", Value::str(rec_str(&rec, "locale")));
        data_prop(&out, "type", Value::str(rec_str(&rec, "type")));
        let notation = rec_str(&rec, "notation");
        data_prop(&out, "notation", Value::str(notation.clone()));
        if notation == "compact" {
            data_prop(
                &out,
                "compactDisplay",
                Value::str(rec_str(&rec, "compactDisplay")),
            );
        }
        data_prop(
            &out,
            "minimumIntegerDigits",
            Value::Number(rec_num(&rec, "minimumIntegerDigits").unwrap_or(1) as f64),
        );
        if let (Some(min_s), Some(max_s)) = (
            rec_num(&rec, "minimumSignificantDigits"),
            rec_num(&rec, "maximumSignificantDigits"),
        ) {
            data_prop(
                &out,
                "minimumSignificantDigits",
                Value::Number(min_s as f64),
            );
            data_prop(
                &out,
                "maximumSignificantDigits",
                Value::Number(max_s as f64),
            );
        } else {
            data_prop(
                &out,
                "minimumFractionDigits",
                Value::Number(rec_num(&rec, "minimumFractionDigits").unwrap_or(0) as f64),
            );
            data_prop(
                &out,
                "maximumFractionDigits",
                Value::Number(rec_num(&rec, "maximumFractionDigits").unwrap_or(3) as f64),
            );
        }
        // pluralCategories: the locale's categories, in canonical order.
        let rules = build_rules(&rec);
        let present: Vec<PluralCategory> = rules.categories().collect();
        let order = [
            PluralCategory::Zero,
            PluralCategory::One,
            PluralCategory::Two,
            PluralCategory::Few,
            PluralCategory::Many,
            PluralCategory::Other,
        ];
        let cats: Vec<Value> = order
            .iter()
            .filter(|c| present.contains(c))
            .map(|c| Value::str(category_name(*c)))
            .collect();
        let arr = vm.new_array(cats);
        data_prop(&out, "pluralCategories", Value::Object(arr));
        data_prop(&out, "roundingIncrement", Value::Number(1.0));
        data_prop(&out, "roundingMode", Value::str("halfExpand"));
        data_prop(&out, "roundingPriority", Value::str("auto"));
        data_prop(&out, "trailingZeroDisplay", Value::str("auto"));
        Ok(Value::Object(out))
    });

    // Intl.PluralRules.prototype[Symbol.toStringTag] = "Intl.PluralRules"
    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().props.insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Intl.PluralRules"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    intl.borrow_mut().props.insert(
        PropertyKey::str("PluralRules"),
        Property::builtin(Value::Object(ctor)),
    );
}
