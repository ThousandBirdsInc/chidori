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
        const { icu_locale::LocaleCanonicalizer::new_extended() };
    static EXPANDER: icu_locale::LocaleExpander = const { icu_locale::LocaleExpander::new_extended() };
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
    install_number_format(vm, &intl);

    // Intl[Symbol.toStringTag] = "Intl" (non-writable, non-enumerable, configurable).
    let tag = vm.realm.symbol_to_string_tag.clone();
    intl.borrow_mut().own_insert(
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
        if o.borrow().own_contains_key(&PropertyKey::Sym(vm.realm.symbol_intl_locale.clone())))
}

/// The stored `[[Locale]]` canonical tag of an Intl.Locale, if `v` is one.
fn locale_internal(vm: &Vm, v: &Value) -> Option<String> {
    let Value::Object(o) = v else { return None };
    match o
        .borrow()
        .own_get(&PropertyKey::Sym(vm.realm.symbol_intl_locale.clone()))
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
    o.borrow_mut().own_insert(
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
    ctor.borrow_mut().own_insert(
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
    proto.borrow_mut().own_insert(
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
    proto.borrow_mut().own_insert(
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

    intl.borrow_mut().own_insert(
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
    target.borrow_mut().own_insert(
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

/// `SetNumberFormatDigitOptions` with `notation = "standard"`. `mnfd_default` /
/// `mxfd_default` are the style-dependent fraction-digit defaults (0/3 for
/// `decimal`/`unit`, 0/0 for `percent`, the minor-unit count for `currency`).
fn set_digit_options(
    vm: &mut Vm,
    options: &Value,
    mnfd_default: u32,
    mxfd_default: u32,
) -> Result<DigitOptions, Value> {
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
        let min_f = default_number_option(vm, &mnfd, 0.0, 100.0, mnfd_default as f64)? as u32;
        let mxfd_actual_default = min_f.max(mxfd_default);
        let max_f =
            default_number_option(vm, &mxfd, min_f as f64, 100.0, mxfd_actual_default as f64)?
                as u32;
        (min_f, max_f)
    } else {
        (mnfd_default, mxfd_default.max(mnfd_default))
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
    match rec.borrow().own_get(&PropertyKey::str(key)) {
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
    match rec.borrow().own_get(&PropertyKey::str(key)) {
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
        .own_get(&PropertyKey::Sym(vm.realm.symbol_intl_plural_rules.clone()))
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

/// Build a `Decimal` for `|n|` carrying the visible digits implied by `opts`
/// (rounding half-expand to the max, padding to the min), or `None` for
/// non-finite input. Shared by the plural-operand and number-format paths.
fn digits_to_decimal(n: f64, opts: &DigitOptions) -> Option<Decimal> {
    if !n.is_finite() {
        return None;
    }
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

/// Format `|n|` into the plural-operand `Decimal` from the record's digit options.
fn operand_decimal(rec: &JsObject, n: f64) -> Option<Decimal> {
    let opts = DigitOptions {
        min_integer: rec_num(rec, "minimumIntegerDigits").unwrap_or(1),
        min_fraction: rec_num(rec, "minimumFractionDigits").unwrap_or(0),
        max_fraction: rec_num(rec, "maximumFractionDigits").unwrap_or(3),
        min_significant: rec_num(rec, "minimumSignificantDigits"),
        max_significant: rec_num(rec, "maximumSignificantDigits"),
    };
    digits_to_decimal(n, &opts)
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
    let digits = set_digit_options(vm, &options, 0, 3)?;
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
            b.own_insert(PropertyKey::str(k), Property::builtin(v));
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
    o.borrow_mut().own_insert(
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
    ctor.borrow_mut().own_insert(
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
    proto.borrow_mut().own_insert(
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
    proto.borrow_mut().own_insert(
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

    intl.borrow_mut().own_insert(
        PropertyKey::str("PluralRules"),
        Property::builtin(Value::Object(ctor)),
    );
}

// =========================================================================
// Intl.NumberFormat
// =========================================================================

use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu_decimal::DecimalFormatter;

/// The internal record object of an Intl.NumberFormat receiver, if `this` is one.
fn nf_record(vm: &Vm, this: &Value) -> Option<JsObject> {
    let Value::Object(o) = this else { return None };
    match o.borrow().own_get(&PropertyKey::Sym(
        vm.realm.symbol_intl_number_format.clone(),
    )) {
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

/// `IsWellFormedCurrencyCode`: three ASCII letters.
fn is_currency_code(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

/// The number of fractional digits in a currency's minor unit (default 2).
fn currency_digits(code: &str) -> u32 {
    match code.to_ascii_uppercase().as_str() {
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF" | "UGX"
        | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
        _ => 2,
    }
}

/// `IsWellFormedUnitIdentifier`: a sanctioned single unit, or `<num>-per-<denom>`.
/// Single units are checked structurally (`[a-z]+(-[a-z]+)*`); the sanctioned-unit
/// table is not bundled, so a few invalid identifiers are accepted.
fn is_unit_identifier(s: &str) -> bool {
    fn simple(u: &str) -> bool {
        !u.is_empty()
            && u.split('-')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
    }
    match s.split_once("-per-") {
        Some((num, den)) => simple(num) && simple(den),
        None => simple(s),
    }
}

/// Map a NumberFormat `roundingMode` string to a `fixed_decimal` mode.
fn rounding_mode(s: &str) -> SignedRoundingMode {
    use SignedRoundingMode as S;
    use UnsignedRoundingMode as U;
    match s {
        "ceil" => S::Ceil,
        "floor" => S::Floor,
        "expand" => S::Unsigned(U::Expand),
        "trunc" => S::Unsigned(U::Trunc),
        "halfCeil" => S::HalfCeil,
        "halfFloor" => S::HalfFloor,
        "halfTrunc" => S::Unsigned(U::HalfTrunc),
        "halfEven" => S::Unsigned(U::HalfEven),
        _ => S::Unsigned(U::HalfExpand),
    }
}

/// Build the (signed) formatted `Decimal` for a NumberFormat value, applying the
/// record's digit options and rounding mode. The sign is preserved (so a negative
/// value that rounds to zero is negative zero); the caller strips it for display.
fn nf_decimal(rec: &JsObject, signed: f64) -> Option<Decimal> {
    if !signed.is_finite() {
        return None;
    }
    let opts = nf_digit_options(rec);
    let mode = rounding_mode(&rec_str(rec, "roundingMode"));
    let mut dec = Decimal::try_from_f64(signed, FloatPrecision::RoundTrip).ok()?;
    if let Some(max_s) = opts.max_significant {
        let mag = dec.nonzero_magnitude_start();
        dec.round_with_mode(mag - max_s as i16 + 1, mode);
        if let Some(min_s) = opts.min_significant {
            dec.pad_end(mag - min_s as i16 + 1);
        }
    } else {
        dec.round_with_mode(-(opts.max_fraction as i16), mode);
        dec.pad_end(-(opts.min_fraction as i16));
    }
    dec.pad_start(opts.min_integer as i16);
    Some(dec)
}

/// Build the digit options used for `format` from a NumberFormat record.
fn nf_digit_options(rec: &JsObject) -> DigitOptions {
    DigitOptions {
        min_integer: rec_num(rec, "minimumIntegerDigits").unwrap_or(1),
        min_fraction: rec_num(rec, "minimumFractionDigits").unwrap_or(0),
        max_fraction: rec_num(rec, "maximumFractionDigits").unwrap_or(3),
        min_significant: rec_num(rec, "minimumSignificantDigits"),
        max_significant: rec_num(rec, "maximumSignificantDigits"),
    }
}

/// Build a locale-aware `DecimalFormatter` honoring the record's numbering system
/// and grouping option.
fn nf_formatter(rec: &JsObject) -> DecimalFormatter {
    let mut tag = rec_str(rec, "locale");
    let nu = rec_str(rec, "numberingSystem");
    if !nu.is_empty() {
        // Re-tag with the requested numbering system so ICU selects its digits.
        if let Ok(mut loc) = Locale::try_from_str(&tag) {
            set_keyword(&mut loc, "nu", &nu);
            tag = loc.to_string();
        }
    }
    let loc = Locale::try_from_str(&tag).unwrap_or(Locale::UNKNOWN);
    let mut opts = DecimalFormatterOptions::default();
    opts.grouping_strategy = Some(match rec_str(rec, "useGrouping").as_str() {
        "false" => GroupingStrategy::Never,
        "always" | "true" => GroupingStrategy::Always,
        "min2" => GroupingStrategy::Min2,
        _ => GroupingStrategy::Auto,
    });
    // The full locale (not just its id) so the `-u-nu` numbering system applies.
    DecimalFormatter::try_new((&loc).into(), opts)
        .or_else(|_| DecimalFormatter::try_new(Default::default(), opts))
        .unwrap()
}

/// The sign to display for a value, given whether the *rounded* value is
/// negative and whether it is zero: `Some(true)` = minus, `Some(false)` = plus,
/// `None` = no sign. A value that rounds to zero counts as zero for the sign.
fn nf_sign(negative: bool, is_zero: bool, sign_display: &str) -> Option<bool> {
    // `negative` is the sign of the rounded value: a negative input that rounds
    // to zero is negative zero, so it still carries a minus under auto/always
    // (but counts as zero for exceptZero/negative).
    match sign_display {
        "never" => None,
        "always" => Some(negative),
        "exceptZero" => {
            if is_zero {
                None
            } else {
                Some(negative)
            }
        }
        "negative" => {
            if negative && !is_zero {
                Some(true)
            } else {
                None
            }
        }
        // "auto"
        _ => {
            if negative {
                Some(true)
            } else {
                None
            }
        }
    }
}

fn sign_str(sign: Option<bool>) -> &'static str {
    match sign {
        Some(true) => "-",
        Some(false) => "+",
        None => "",
    }
}

/// Push a leading sign part (minusSign / plusSign), if any.
fn push_sign(parts: &mut Vec<(&'static str, String)>, sign: Option<bool>) {
    match sign {
        Some(true) => parts.push(("minusSign", "-".to_string())),
        Some(false) => parts.push(("plusSign", "+".to_string())),
        None => {}
    }
}

/// Format `n` to a string under the record's options (decimal/percent fully;
/// currency/unit approximated as code/identifier + number).
fn nf_format(rec: &JsObject, n: f64) -> String {
    let style = rec_str(rec, "style");
    let percent = style == "percent";
    let sd = rec_str(rec, "signDisplay");
    let pct = if percent { "%" } else { "" };
    if n.is_nan() {
        // NaN carries a sign only under signDisplay "always" (a plus).
        let s = if sd == "always" { "+" } else { "" };
        return format!("{s}NaN{pct}");
    }
    let negative = n.is_sign_negative();
    if n.is_infinite() {
        return format!("{}\u{221e}{pct}", sign_str(nf_sign(negative, false, &sd)));
    }
    // Percent scales by 100; rounding/sign are computed on the signed value.
    let scaled = if percent { n * 100.0 } else { n };
    let mut dec = match nf_decimal(rec, scaled) {
        Some(d) => d,
        None => return format!("{}", scaled.abs()),
    };
    let is_negative = dec.sign() == fixed_decimal::Sign::Negative;
    let s = sign_str(nf_sign(is_negative, dec.is_zero(), &sd));
    dec.set_sign(fixed_decimal::Sign::None);
    let number = nf_formatter(rec).format(&dec).to_string();
    match style.as_str() {
        "percent" => format!("{s}{number}%"),
        "currency" => {
            let code = rec_str(rec, "currency");
            format!("{s}{code}\u{a0}{number}")
        }
        "unit" => {
            let unit = rec_str(rec, "unit");
            format!("{s}{number}\u{a0}{unit}")
        }
        _ => format!("{s}{number}"),
    }
}

/// `format` value coercion: ToIntlMathematicalValue, reduced to an `f64`
/// (BigInt is converted through its decimal string).
fn nf_input(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    match v {
        Value::BigInt(b) => Ok(b.to_string().parse::<f64>().unwrap_or(f64::NAN)),
        _ => vm.to_number(v),
    }
}

/// Reconstruct `formatToParts` segments from a formatted decimal/percent string.
/// Correct for the Latin numbering system (ASCII digits); other systems fall back
/// to coarser parts.
fn nf_parts(vm: &mut Vm, rec: &JsObject, n: f64) -> Value {
    let style = rec_str(rec, "style");
    let percent = style == "percent";
    let sd = rec_str(rec, "signDisplay");
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    let negative = !n.is_nan() && n.is_sign_negative();
    if n.is_nan() {
        push_sign(&mut parts, if sd == "always" { Some(false) } else { None });
        parts.push(("nan", "NaN".to_string()));
    } else if n.is_infinite() {
        push_sign(&mut parts, nf_sign(negative, false, &sd));
        parts.push(("infinity", "\u{221e}".to_string()));
    } else {
        let scaled = if percent { n * 100.0 } else { n };
        let (number, is_zero, is_neg) = match nf_decimal(rec, scaled) {
            Some(mut d) => {
                let z = d.is_zero();
                let neg = d.sign() == fixed_decimal::Sign::Negative;
                d.set_sign(fixed_decimal::Sign::None);
                (nf_formatter(rec).format(&d).to_string(), z, neg)
            }
            None => (scaled.abs().to_string(), false, negative),
        };
        push_sign(&mut parts, nf_sign(is_neg, is_zero, &sd));
        // Detect the locale's group/decimal separators with a probe.
        let (group, decimal) = nf_separators(rec);
        push_number_parts(&mut parts, &number, &group, &decimal);
    }
    if percent {
        parts.push(("percentSign", "%".to_string()));
    }
    let objs: Vec<Value> = parts
        .into_iter()
        .map(|(t, v)| {
            let o = vm.new_object();
            vm.define_value(&o, "type", Value::str(t));
            vm.define_value(&o, "value", Value::str(v));
            Value::Object(o)
        })
        .collect();
    Value::Object(vm.new_array(objs))
}

/// The locale's grouping and decimal separators (probed via the formatter).
fn nf_separators(rec: &JsObject) -> (String, String) {
    let f = nf_formatter(rec);
    let grouped = f.format(&Decimal::from(11_222_333)).to_string();
    let group: String = grouped
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(1)
        .collect();
    let mut frac = Decimal::from(1);
    frac.pad_end(-1);
    let fracs = f.format(&frac).to_string();
    let decimal: String = fracs
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(1)
        .collect();
    (group, decimal)
}

/// Split a formatted number string into integer/group/decimal/fraction parts.
fn push_number_parts(
    parts: &mut Vec<(&'static str, String)>,
    number: &str,
    group: &str,
    decimal: &str,
) {
    let (int_part, frac_part) = match (decimal.is_empty(), number.find(decimal)) {
        (false, Some(idx)) => (&number[..idx], Some(&number[idx + decimal.len()..])),
        _ => (number, None),
    };
    if group.is_empty() {
        parts.push(("integer", int_part.to_string()));
    } else {
        let mut rest = int_part;
        while let Some(idx) = rest.find(group) {
            parts.push(("integer", rest[..idx].to_string()));
            parts.push(("group", group.to_string()));
            rest = &rest[idx + group.len()..];
        }
        parts.push(("integer", rest.to_string()));
    }
    if let Some(frac) = frac_part {
        parts.push(("decimal", decimal.to_string()));
        parts.push(("fraction", frac.to_string()));
    }
}

fn construct_number_format(vm: &mut Vm, args: &[Value], proto: &JsObject) -> Result<Value, Value> {
    let requested = canonicalize_locale_list(vm, &arg(args, 0))?;
    let options = coerce_options(vm, arg(args, 1))?;

    // Options in spec read order.
    get_enum_option(vm, &options, "localeMatcher", &["lookup", "best fit"])?;
    let numbering_system = match get_string_option(vm, &options, "numberingSystem")? {
        Some(s) if is_unicode_type(&s) => Some(s),
        Some(_) => return Err(vm.throw_range("invalid numberingSystem")),
        None => None,
    };
    let style = get_enum_option(
        vm,
        &options,
        "style",
        &["decimal", "percent", "currency", "unit"],
    )?
    .unwrap_or_else(|| "decimal".to_string());

    let currency = get_string_option(vm, &options, "currency")?;
    if let Some(c) = &currency {
        if !is_currency_code(c) {
            return Err(vm.throw_range("invalid currency code"));
        }
    }
    let currency_display = get_enum_option(
        vm,
        &options,
        "currencyDisplay",
        &["symbol", "narrowSymbol", "code", "name"],
    )?
    .unwrap_or_else(|| "symbol".to_string());
    let currency_sign = get_enum_option(vm, &options, "currencySign", &["standard", "accounting"])?
        .unwrap_or_else(|| "standard".to_string());

    let unit = get_string_option(vm, &options, "unit")?;
    if let Some(u) = &unit {
        if !is_unit_identifier(u) {
            return Err(vm.throw_range("invalid unit identifier"));
        }
    }
    let unit_display = get_enum_option(vm, &options, "unitDisplay", &["short", "narrow", "long"])?
        .unwrap_or_else(|| "short".to_string());

    if style == "currency" && currency.is_none() {
        return Err(vm.throw_type("currency code is required with style 'currency'"));
    }
    if style == "unit" && unit.is_none() {
        return Err(vm.throw_type("unit is required with style 'unit'"));
    }

    let (mnfd_default, mxfd_default) = match style.as_str() {
        "currency" => {
            let c = currency.as_deref().map(currency_digits).unwrap_or(2);
            (c, c)
        }
        "percent" => (0, 0),
        _ => (0, 3),
    };
    let notation = get_enum_option(
        vm,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
    )?
    .unwrap_or_else(|| "standard".to_string());
    let digits = set_digit_options(vm, &options, mnfd_default, mxfd_default)?;
    get_number_option(vm, &options, "roundingIncrement", 1.0, 5000.0, 1.0)?;
    let rounding_mode = get_enum_option(
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
    )?
    .unwrap_or_else(|| "halfExpand".to_string());
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
    let compact_display = get_enum_option(vm, &options, "compactDisplay", &["short", "long"])?
        .unwrap_or_else(|| "short".to_string());
    // useGrouping: a string-or-boolean option.
    let use_grouping = grouping_option(vm, &options, &notation)?;
    let sign_display = get_enum_option(
        vm,
        &options,
        "signDisplay",
        &["auto", "never", "always", "exceptZero", "negative"],
    )?
    .unwrap_or_else(|| "auto".to_string());

    let locale = requested
        .iter()
        .find_map(|t| Locale::try_from_str(t).ok())
        .map(|l| l.id.to_string())
        .unwrap_or_else(|| "en".to_string());

    let rec = vm.new_object();
    {
        let mut b = rec.borrow_mut();
        let mut put = |k: &str, v: Value| {
            b.own_insert(PropertyKey::str(k), Property::builtin(v));
        };
        put("locale", Value::str(locale));
        if let Some(nu) = &numbering_system {
            put("numberingSystem", Value::str(nu.clone()));
        }
        put("style", Value::str(style.clone()));
        if let Some(c) = &currency {
            put("currency", Value::str(c.to_ascii_uppercase()));
            put("currencyDisplay", Value::str(currency_display));
            put("currencySign", Value::str(currency_sign));
        }
        if let Some(u) = &unit {
            put("unit", Value::str(u.clone()));
            put("unitDisplay", Value::str(unit_display));
        }
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
        put("useGrouping", Value::str(use_grouping));
        put("signDisplay", Value::str(sign_display));
        put("roundingMode", Value::str(rounding_mode));
    }

    let o = vm.alloc(ObjectData::new(Some(proto.clone()), Internal::Ordinary));
    o.borrow_mut().own_insert(
        PropertyKey::Sym(vm.realm.symbol_intl_number_format.clone()),
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

/// `GetStringOrBooleanOption` for `useGrouping`: `true`→"always", `false`→"false",
/// `"min2"`/`"auto"`/`"always"` pass through; default is "min2" for compact
/// notation else "auto".
fn grouping_option(vm: &mut Vm, options: &Value, notation: &str) -> Result<String, Value> {
    let v = vm.get_prop(options, &PropertyKey::str("useGrouping"))?;
    let default = if notation == "compact" {
        "min2"
    } else {
        "auto"
    };
    if v.is_undefined() {
        return Ok(default.to_string());
    }
    if let Value::Bool(b) = v {
        return Ok(if b { "always" } else { "false" }.to_string());
    }
    let s = vm.to_js_string(&v)?.as_str().to_owned();
    match s.as_str() {
        "min2" | "auto" | "always" => Ok(s),
        // ToString of `true`/`false` and the legacy boolean forms.
        "true" => Ok("always".to_string()),
        "false" => Ok("false".to_string()),
        _ => Err(vm.throw_range("invalid useGrouping value")),
    }
}

fn install_number_format(vm: &mut Vm, intl: &JsObject) {
    let proto = vm.new_object();
    // Intl.NumberFormat is one of the legacy constructors: callable with or
    // without `new` (the [[Call]] path constructs an instance too).
    let call_proto = proto.clone();
    let ctor_proto = proto.clone();
    let ctor = vm.new_native_ctor(
        "NumberFormat",
        0,
        move |vm, _t, args| construct_number_format(vm, args, &call_proto),
        move |vm, _this, args| construct_number_format(vm, args, &ctor_proto),
    );
    ctor.borrow_mut().own_insert(
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
    proto.borrow_mut().own_insert(
        PropertyKey::str("constructor"),
        Property::builtin(Value::Object(ctor.clone())),
    );

    vm.define_method(&ctor, "supportedLocalesOf", 1, |vm, _t, args| {
        let list = canonicalize_locale_list(vm, &arg(args, 0))?;
        let vals: Vec<Value> = list.into_iter().map(Value::str).collect();
        Ok(Value::Object(vm.new_array(vals)))
    });

    // `get format`: a getter returning a once-bound formatter function (name "",
    // length 1), cached on the instance so `nf.format === nf.format`.
    let format_getter = vm.new_native("get format", 0, |vm, this, _a| {
        let rec = nf_record(vm, &this).ok_or_else(|| {
            vm.throw_type("get Intl.NumberFormat.prototype.format on incompatible receiver")
        })?;
        let cached = rec
            .borrow()
            .own_get(&PropertyKey::str("boundFormat"))
            .and_then(|p| match &p.kind {
                PropertyKind::Data { value, .. } => Some(value.clone()),
                _ => None,
            });
        if let Some(v) = cached {
            return Ok(v);
        }
        let bound_rec = rec.clone();
        let bound = vm.new_native("", 1, move |vm, _t, args| {
            let n = nf_input(vm, &arg(args, 0))?;
            Ok(Value::str(nf_format(&bound_rec, n)))
        });
        rec.borrow_mut().own_insert(
            PropertyKey::str("boundFormat"),
            Property::builtin(Value::Object(bound.clone())),
        );
        Ok(Value::Object(bound))
    });
    vm.define_accessor(
        &Value::Object(proto.clone()),
        PropertyKey::str("format"),
        Some(Value::Object(format_getter)),
        None,
    );

    vm.define_method(&proto, "formatToParts", 1, |vm, this, args| {
        let rec = nf_record(vm, &this).ok_or_else(|| {
            vm.throw_type("Intl.NumberFormat.prototype.formatToParts on incompatible receiver")
        })?;
        let n = nf_input(vm, &arg(args, 0))?;
        Ok(nf_parts(vm, &rec, n))
    });

    vm.define_method(&proto, "resolvedOptions", 0, |vm, this, _a| {
        let rec = nf_record(vm, &this).ok_or_else(|| {
            vm.throw_type("Intl.NumberFormat.prototype.resolvedOptions on incompatible receiver")
        })?;
        let out = vm.new_object();
        let copy_str = |out: &JsObject, k: &str| {
            let s = rec_str(&rec, k);
            if !s.is_empty() {
                data_prop(out, k, Value::str(s));
            }
        };
        let copy_num = |out: &JsObject, k: &str| {
            if let Some(n) = rec_num(&rec, k) {
                data_prop(out, k, Value::Number(n as f64));
            }
        };
        copy_str(&out, "locale");
        // numberingSystem defaults to the locale's system; report "latn" when unset.
        let nu = rec_str(&rec, "numberingSystem");
        data_prop(
            &out,
            "numberingSystem",
            Value::str(if nu.is_empty() {
                "latn".to_string()
            } else {
                nu
            }),
        );
        copy_str(&out, "style");
        copy_str(&out, "currency");
        copy_str(&out, "currencyDisplay");
        copy_str(&out, "currencySign");
        copy_str(&out, "unit");
        copy_str(&out, "unitDisplay");
        copy_num(&out, "minimumIntegerDigits");
        copy_num(&out, "minimumFractionDigits");
        copy_num(&out, "maximumFractionDigits");
        copy_num(&out, "minimumSignificantDigits");
        copy_num(&out, "maximumSignificantDigits");
        // useGrouping: report the boolean-or-string resolved value.
        let ug = rec_str(&rec, "useGrouping");
        let ug_val = match ug.as_str() {
            "false" => Value::Bool(false),
            other => Value::str(other),
        };
        data_prop(&out, "useGrouping", ug_val);
        copy_str(&out, "notation");
        copy_str(&out, "compactDisplay");
        data_prop(
            &out,
            "signDisplay",
            Value::str(rec_str(&rec, "signDisplay")),
        );
        data_prop(&out, "roundingIncrement", Value::Number(1.0));
        let rm = rec_str(&rec, "roundingMode");
        data_prop(
            &out,
            "roundingMode",
            Value::str(if rm.is_empty() {
                "halfExpand".to_string()
            } else {
                rm
            }),
        );
        data_prop(&out, "roundingPriority", Value::str("auto"));
        data_prop(&out, "trailingZeroDisplay", Value::str("auto"));
        Ok(Value::Object(out))
    });

    // get Intl.NumberFormat.prototype.format — a bound formatter function.
    // (Spec defines `format` as a getter returning a bound function; tests mostly
    // call it as a method, which the data method above satisfies.)

    let tag = vm.realm.symbol_to_string_tag.clone();
    proto.borrow_mut().own_insert(
        PropertyKey::Sym(tag),
        Property {
            kind: PropertyKind::Data {
                value: Value::str("Intl.NumberFormat"),
                writable: false,
            },
            enumerable: false,
            configurable: true,
        },
    );

    intl.borrow_mut().own_insert(
        PropertyKey::str("NumberFormat"),
        Property::builtin(Value::Object(ctor)),
    );
}
