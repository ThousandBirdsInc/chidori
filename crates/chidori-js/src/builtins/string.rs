//! String constructor and `String.prototype`.
//!
//! Note: strings are indexed by Unicode scalar (code point), not UTF-16 code
//! unit. This diverges from the spec for astral-plane characters and lone
//! surrogates — an explicit conformance long-tail item (plan P5). It is
//! internally consistent (length, indexing, iteration all agree).
//!
//! RegExp-aware methods (`match`, `matchAll`, `search`, `replace`,
//! `replaceAll`, `split`) dispatch through the well-known symbol protocol
//! (`Symbol.match`/`replace`/`split`/`search`/`matchAll`). When the argument
//! exposes such a method (e.g. a RegExp or a user RegExp subclass), it is
//! invoked with the string as the argument; otherwise the method falls back to
//! the direct string behavior (treating the argument as a literal pattern, or
//! building a RegExp from it where the spec requires). The RegExp prototype
//! symbol methods themselves live in `crate::builtins::regexp_builtin`.

use super::arg;
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    let proto = vm.realm.string_proto.clone();
    proto.borrow_mut().internal = Internal::StringObj(JsString::new(""));

    let ctor = vm.new_native_ctor(
        "String",
        1,
        |vm, _t, args| {
            if args.is_empty() {
                Ok(Value::str(""))
            } else {
                if let Value::Symbol(s) = &args[0] {
                    return Ok(Value::str(format!(
                        "Symbol({})",
                        s.description().unwrap_or("")
                    )));
                }
                Ok(Value::String(vm.to_js_string(&args[0])?))
            }
        },
        |vm, _t, args| {
            let s = if args.is_empty() {
                JsString::new("")
            } else {
                vm.to_js_string(&args[0])?
            };
            Ok(Value::Object(vm.new_string_object(s)))
        },
    );
    vm.install_ctor("String", &ctor, &proto);

    vm.define_method(&ctor, "fromCharCode", 1, |vm, _t, args| {
        let mut s = String::new();
        for a in args {
            let n = vm.to_uint32(a)? as u16;
            s.push(char::from_u32(n as u32).unwrap_or('\u{fffd}'));
        }
        Ok(Value::str(s))
    });
    vm.define_method(&ctor, "fromCodePoint", 1, |vm, _t, args| {
        let mut s = String::new();
        for a in args {
            let num = vm.to_number(a)?;
            // CodePoint must be an integer in [0, 0x10FFFF].
            if num.is_nan() || num != num.trunc() || num < 0.0 || num > 0x10_FFFF as f64 {
                return Err(vm.throw_range(&format!(
                    "Invalid code point {}",
                    crate::vm::number_to_string(num)
                )));
            }
            s.push(char::from_u32(num as u32).unwrap_or('\u{fffd}'));
        }
        Ok(Value::str(s))
    });

    // String.raw(template, ...substitutions)
    vm.define_method(&ctor, "raw", 1, |vm, _t, args| {
        let cooked = arg(args, 0);
        let cooked_obj = vm.to_object(&cooked)?;
        let raw_val = vm.get_prop(&Value::Object(cooked_obj), &PropertyKey::str("raw"))?;
        let raw_obj = vm.to_object(&raw_val)?;
        let len_val = vm.get_prop(&Value::Object(raw_obj.clone()), &PropertyKey::str("length"))?;
        let len = vm.to_length(&len_val)?;
        if len == 0 {
            return Ok(Value::str(""));
        }
        // Substitutions are args[1..]; a gap with no matching substitution uses
        // the empty string (spec step 12.f/g), NOT `undefined`.
        let num_subs = args.len().saturating_sub(1);
        let mut out = String::new();
        for i in 0..len {
            let seg = vm.get_prop(
                &Value::Object(raw_obj.clone()),
                &PropertyKey::from_index(i as u32),
            )?;
            // ToString is observable and may throw (e.g. a Symbol segment) — use
            // the throwing conversion, not the lossy one.
            let s = vm.to_js_string(&seg)?;
            out.push_str(s.as_str());
            // The last segment has no following substitution.
            if i + 1 < len && i < num_subs {
                let sub = arg(args, i + 1);
                let s = vm.to_js_string(&sub)?;
                out.push_str(s.as_str());
            }
        }
        Ok(Value::str(out))
    });

    install_proto(vm, &proto);
}

/// ECMAScript's trim set: `WhiteSpace` ∪ `LineTerminator`. This matches Unicode
/// White_Space except it excludes U+0085 (NEL, not JS whitespace) and includes
/// U+FEFF (ZWNBSP/BOM, which JS trims).
fn is_js_ws(c: char) -> bool {
    c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{0085}')
}

/// `ToIntegerOrInfinity(value)` clamped to `[0, len]` (an undefined value gives
/// 0). Used for the `position`/`endPosition` arguments of the search methods.
fn clamp_pos(vm: &mut Vm, v: &Value, len: usize) -> Result<usize, Value> {
    if v.is_undefined() {
        return Ok(0);
    }
    let n = vm.to_number(v)?;
    Ok(if n.is_nan() || n <= 0.0 {
        0
    } else if n >= len as f64 {
        len
    } else {
        n as usize
    })
}

/// Spec `IsRegExp(argument)`: an object whose `@@match` is defined uses
/// `ToBoolean(@@match)`; otherwise it is a RegExp only if it has the internal
/// `[[RegExpMatcher]]` (the engine's brand).
fn is_regexp_spec(vm: &mut Vm, v: &Value) -> Result<bool, Value> {
    if !matches!(v, Value::Object(_)) {
        return Ok(false);
    }
    let sym = vm.realm.symbol_match.clone();
    let m = vm.get_prop(v, &PropertyKey::Sym(sym))?;
    if !m.is_undefined() {
        return Ok(vm.to_boolean(&m));
    }
    Ok(crate::regexp::is_regexp(v))
}

fn str_this(vm: &mut Vm, this: &Value) -> Result<String, Value> {
    if this.is_nullish() {
        return Err(vm.throw_type("String.prototype method called on null or undefined"));
    }
    match this {
        Value::String(s) => Ok(s.as_str().to_string()),
        Value::Object(o) => {
            if let Internal::StringObj(s) = &o.borrow().internal {
                return Ok(s.as_str().to_string());
            }
            Ok(vm.to_js_string(this)?.as_str().to_string())
        }
        _ => Ok(vm.to_js_string(this)?.as_str().to_string()),
    }
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// `ToIntegerOrInfinity(value)` as an f64 (NaN -> 0, truncation towards 0,
/// infinities preserved).
fn to_integer_or_infinity(vm: &mut Vm, v: &Value) -> Result<f64, Value> {
    let n = vm.to_number(v)?;
    Ok(if n.is_nan() { 0.0 } else { n.trunc() })
}

/// Fallback path of `match`/`matchAll`/`search`: RegExpCreate from the
/// argument, then Invoke(rx, @@sym, «S») — observable: a patched
/// `RegExp.prototype[@@sym]` is called, and its absence is a TypeError.
fn invoke_regexp_sym(
    vm: &mut Vm,
    regexp: &Value,
    flags: &str,
    sym: JsSymbol,
    s: String,
) -> Result<Value, Value> {
    let src = if regexp.is_undefined() {
        String::new()
    } else {
        vm.to_js_string(regexp)?.as_str().to_string()
    };
    let re = vm.make_regexp(&src, flags)?;
    let m = vm.get_prop(&re, &PropertyKey::Sym(sym))?;
    if !vm.is_callable(&m) {
        return Err(vm.throw_type("RegExp prototype method is not callable"));
    }
    vm.call(m, re, &[Value::str(s)])
}

/// `GetMethod(V, key)` (spec 7.3.11): get the property; `undefined`/`null`
/// yields `None`; a non-callable result throws a TypeError; otherwise return the
/// callable. `V` must be a value whose properties can be read (object-coercible).
fn get_method(vm: &mut Vm, v: &Value, key: &PropertyKey) -> Result<Option<Value>, Value> {
    let m = vm.get_prop(v, key)?;
    if m.is_nullish() {
        return Ok(None);
    }
    if !vm.is_callable(&m) {
        return Err(vm.throw_type("property is not a function"));
    }
    Ok(Some(m))
}

fn install_proto(vm: &mut Vm, proto: &JsObject) {
    // thisStringValue: a primitive string or a String wrapper object — any
    // other receiver is a TypeError (NOT generic ToString coercion).
    fn this_string_value(vm: &mut Vm, this: &Value) -> Result<Value, Value> {
        match this {
            Value::String(s) => Ok(Value::String(s.clone())),
            Value::Object(o) => {
                if let Internal::StringObj(s) = &o.borrow().internal {
                    return Ok(Value::String(s.clone()));
                }
                Err(vm.throw_type("String.prototype.toString requires that 'this' be a String"))
            }
            _ => Err(vm.throw_type("String.prototype.toString requires that 'this' be a String")),
        }
    }
    vm.define_method(proto, "toString", 0, |vm, this, _a| {
        this_string_value(vm, &this)
    });
    vm.define_method(proto, "valueOf", 0, |vm, this, _a| {
        this_string_value(vm, &this)
    });
    vm.define_method(proto, "charAt", 1, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let i = to_integer_or_infinity(vm, &arg(args, 0))?;
        Ok(Value::str(if i >= 0.0 && i < s.len() as f64 {
            s[i as usize].to_string()
        } else {
            String::new()
        }))
    });
    vm.define_method(proto, "charCodeAt", 1, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let i = to_integer_or_infinity(vm, &arg(args, 0))?;
        if i >= 0.0 && i < s.len() as f64 {
            Ok(Value::Number(s[i as usize] as u32 as f64))
        } else {
            Ok(Value::Number(f64::NAN))
        }
    });
    vm.define_method(proto, "codePointAt", 1, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let i = to_integer_or_infinity(vm, &arg(args, 0))?;
        if i >= 0.0 && i < s.len() as f64 {
            // Scalar-indexed model: each char is a full code point.
            Ok(Value::Number(s[i as usize] as u32 as f64))
        } else {
            Ok(Value::Undefined)
        }
    });
    vm.define_method(proto, "at", 1, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let rel = to_integer_or_infinity(vm, &arg(args, 0))?;
        if !rel.is_finite() && rel > 0.0 {
            return Ok(Value::Undefined);
        }
        let mut i = if rel.is_finite() {
            rel as isize
        } else {
            isize::MIN / 2
        };
        if i < 0 {
            i += s.len() as isize;
        }
        if i >= 0 && (i as usize) < s.len() {
            Ok(Value::str(s[i as usize].to_string()))
        } else {
            Ok(Value::Undefined)
        }
    });
    vm.define_method(proto, "indexOf", 1, |vm, this, args| {
        let s = str_this(vm, &this)?;
        // Coercion order: ToString(searchString) BEFORE ToInteger(position).
        let needle = vm.to_js_string(&arg(args, 0))?.as_str().to_string();
        let s_chars = chars(&s);
        // ToIntegerOrInfinity(position), clamped to [0, len] (+Infinity finds
        // nothing but an empty needle at the very end finds the clamp point).
        let pos =
            to_integer_or_infinity(vm, &arg(args, 1))?.clamp(0.0, s_chars.len() as f64) as usize;
        let byte_start: usize = s_chars[..pos].iter().map(|c| c.len_utf8()).sum();
        Ok(Value::Number(match s[byte_start..].find(&needle) {
            Some(byte) => s[..byte_start + byte].chars().count() as f64,
            None => -1.0,
        }))
    });
    vm.define_method(proto, "lastIndexOf", 1, |vm, this, args| {
        let s = str_this(vm, &this)?;
        // Coercion order: ToString(searchString) BEFORE ToNumber(position).
        let needle = vm.to_js_string(&arg(args, 0))?.as_str().to_string();
        let num_pos = vm.to_number(&arg(args, 1))?;
        let s_chars = chars(&s);
        let len = s_chars.len() as f64;
        // NaN position means +Infinity (search from the very end).
        let last = if num_pos.is_nan() {
            len
        } else {
            num_pos.trunc().clamp(0.0, len)
        } as usize;
        // Largest start <= last where the needle matches.
        let needle_chars = chars(&needle);
        let mut found = -1.0;
        for start in (0..=last.min(s_chars.len())).rev() {
            if s_chars[start..].starts_with(&needle_chars[..]) {
                found = start as f64;
                break;
            }
        }
        Ok(Value::Number(found))
    });
    vm.define_method(proto, "includes", 1, |vm, this, args| {
        let s = str_this(vm, &this)?;
        if is_regexp_spec(vm, &arg(args, 0))? {
            return Err(vm.throw_type(
                "First argument to String.prototype.includes must not be a regular expression",
            ));
        }
        let needle = vm.to_js_string(&arg(args, 0))?.as_str().to_string();
        let s_chars = chars(&s);
        let pos = clamp_pos(vm, &arg(args, 1), s_chars.len())?;
        let byte_start: usize = s_chars[..pos].iter().map(|c| c.len_utf8()).sum();
        Ok(Value::Bool(s[byte_start..].contains(&needle)))
    });
    vm.define_method(proto, "startsWith", 1, |vm, this, args| {
        let s = str_this(vm, &this)?;
        if is_regexp_spec(vm, &arg(args, 0))? {
            return Err(vm.throw_type(
                "First argument to String.prototype.startsWith must not be a regular expression",
            ));
        }
        let needle = vm.to_js_string(&arg(args, 0))?.as_str().to_string();
        let s_chars = chars(&s);
        let pos = clamp_pos(vm, &arg(args, 1), s_chars.len())?;
        let byte_start: usize = s_chars[..pos].iter().map(|c| c.len_utf8()).sum();
        Ok(Value::Bool(s[byte_start..].starts_with(&needle)))
    });
    vm.define_method(proto, "endsWith", 1, |vm, this, args| {
        let s = str_this(vm, &this)?;
        if is_regexp_spec(vm, &arg(args, 0))? {
            return Err(vm.throw_type(
                "First argument to String.prototype.endsWith must not be a regular expression",
            ));
        }
        let needle = vm.to_js_string(&arg(args, 0))?.as_str().to_string();
        let s_chars = chars(&s);
        let end = {
            let p = arg(args, 1);
            if p.is_undefined() {
                s_chars.len()
            } else {
                clamp_pos(vm, &p, s_chars.len())?
            }
        };
        let byte_end: usize = s_chars[..end].iter().map(|c| c.len_utf8()).sum();
        Ok(Value::Bool(s[..byte_end].ends_with(&needle)))
    });
    vm.define_method(proto, "slice", 2, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let len = s.len() as isize;
        let start = rel(vm, &arg(args, 0), len, 0)?;
        let end = rel(vm, &arg(args, 1), len, len)?;
        Ok(Value::str(if start < end {
            s[start as usize..end as usize].iter().collect::<String>()
        } else {
            String::new()
        }))
    });
    vm.define_method(proto, "substring", 2, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let len = s.len() as isize;
        let mut start = clamp_idx(vm, &arg(args, 0), len, 0)?;
        let mut end = clamp_idx(vm, &arg(args, 1), len, len)?;
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        Ok(Value::str(
            s[start as usize..end as usize].iter().collect::<String>(),
        ))
    });
    vm.define_method(proto, "substr", 2, |vm, this, args| {
        let s = chars(&str_this(vm, &this)?);
        let len = s.len() as isize;
        let mut start = vm.to_int32(&arg(args, 0))? as isize;
        if start < 0 {
            start = (len + start).max(0);
        }
        let count = {
            let c = arg(args, 1);
            if c.is_undefined() {
                len - start
            } else {
                vm.to_int32(&c)? as isize
            }
        }
        .max(0);
        let end = (start + count).min(len);
        Ok(Value::str(if start < end {
            s[start as usize..end as usize].iter().collect::<String>()
        } else {
            String::new()
        }))
    });
    vm.define_method(proto, "toUpperCase", 0, |vm, this, _a| {
        Ok(Value::str(str_this(vm, &this)?.to_uppercase()))
    });
    vm.define_method(proto, "toLowerCase", 0, |vm, this, _a| {
        Ok(Value::str(str_this(vm, &this)?.to_lowercase()))
    });
    vm.define_method(proto, "toLocaleUpperCase", 0, |vm, this, _a| {
        Ok(Value::str(str_this(vm, &this)?.to_uppercase()))
    });
    vm.define_method(proto, "toLocaleLowerCase", 0, |vm, this, _a| {
        Ok(Value::str(str_this(vm, &this)?.to_lowercase()))
    });
    vm.define_method(proto, "trim", 0, |vm, this, _a| {
        Ok(Value::str(
            str_this(vm, &this)?.trim_matches(is_js_ws).to_string(),
        ))
    });
    vm.define_method(proto, "trimStart", 0, |vm, this, _a| {
        Ok(Value::str(
            str_this(vm, &this)?
                .trim_start_matches(is_js_ws)
                .to_string(),
        ))
    });
    vm.define_method(proto, "trimEnd", 0, |vm, this, _a| {
        Ok(Value::str(
            str_this(vm, &this)?.trim_end_matches(is_js_ws).to_string(),
        ))
    });
    vm.define_method(proto, "repeat", 1, |vm, this, args| {
        let s = str_this(vm, &this)?;
        let n = vm.to_number(&arg(args, 0))?;
        // ToIntegerOrInfinity: NaN -> 0; negative or +Infinity -> RangeError.
        let count = if n.is_nan() { 0.0 } else { n.trunc() };
        if count < 0.0 || count.is_infinite() {
            return Err(vm.throw_range("Invalid count value"));
        }
        // Reject (without allocating) any result that would exceed the string cap.
        let total = s
            .as_str()
            .len()
            .checked_mul(count as usize)
            .filter(|&t| t <= crate::value::MAX_STRING_LEN);
        if total.is_none() {
            return Err(vm.throw_range("Invalid string length"));
        }
        Ok(Value::str(s.repeat(count as usize)))
    });
    vm.define_method(proto, "padStart", 1, |vm, this, args| {
        pad(vm, &this, args, true)
    });
    vm.define_method(proto, "padEnd", 1, |vm, this, args| {
        pad(vm, &this, args, false)
    });
    vm.define_method(proto, "concat", 1, |vm, this, args| {
        let mut s = str_this(vm, &this)?;
        for a in args {
            let part = vm.to_js_string(a)?;
            s.push_str(part.as_str());
        }
        Ok(Value::str(s))
    });
    vm.define_method(proto, "normalize", 0, |vm, this, args| {
        use unicode_normalization::UnicodeNormalization;
        let s = str_this(vm, &this)?;
        // `form` coerces with ToString (a Symbol form is a TypeError, a
        // throwing toString propagates); undefined defaults to "NFC".
        let form_v = arg(args, 0);
        let form = if form_v.is_undefined() {
            "NFC".to_string()
        } else {
            vm.to_js_string(&form_v)?.as_str().to_string()
        };
        let out: String = match form.as_str() {
            "NFC" => s.chars().nfc().collect(),
            "NFD" => s.chars().nfd().collect(),
            "NFKC" => s.chars().nfkc().collect(),
            "NFKD" => s.chars().nfkd().collect(),
            _ => {
                return Err(
                    vm.throw_range("The normalization form should be one of NFC, NFD, NFKC, NFKD")
                )
            }
        };
        Ok(Value::str(out))
    });
    vm.define_method(proto, "localeCompare", 1, |vm, this, args| {
        // Ordinal comparison over NFC-normalized code points: no collation
        // tables, but canonically-equivalent strings compare equal (the spec
        // requires consistency with canonical equivalence).
        use unicode_normalization::UnicodeNormalization;
        let s: String = str_this(vm, &this)?.chars().nfc().collect();
        let that: String = vm
            .to_js_string(&arg(args, 0))?
            .as_str()
            .chars()
            .nfc()
            .collect();
        let ord = s.cmp(&that);
        Ok(Value::Number(match ord {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });
    vm.define_method(proto, "isWellFormed", 0, |vm, this, _a| {
        // The scalar-string model cannot represent lone surrogates, so every
        // string is well-formed.
        let _ = str_this(vm, &this)?;
        Ok(Value::Bool(true))
    });
    vm.define_method(proto, "toWellFormed", 0, |vm, this, _a| {
        Ok(Value::str(str_this(vm, &this)?))
    });

    // String.prototype.split(separator, limit). Dispatches to
    // separator[@@split](this, limit) when present (e.g. for a RegExp); else
    // performs a literal string split.
    vm.define_method(proto, "split", 2, |vm, this, args| {
        if this.is_nullish() {
            return Err(vm.throw_type("String.prototype.split called on null or undefined"));
        }
        let sep = arg(args, 0);
        let limit_arg = arg(args, 1);
        // Symbol dispatch first — before ToString(this): separator[@@split](O, limit).
        if matches!(sep, Value::Object(_)) {
            let key = PropertyKey::Sym(vm.realm.symbol_split.clone());
            if let Some(m) = get_method(vm, &sep, &key)? {
                return vm.call(m, sep.clone(), &[this.clone(), limit_arg]);
            }
        }
        let s = str_this(vm, &this)?;
        let limit = if limit_arg.is_undefined() {
            u32::MAX as usize
        } else {
            vm.to_uint32(&limit_arg)? as usize
        };
        // ToString(separator) is observable and happens before the limit==0 check.
        let sep_undefined = sep.is_undefined();
        let sep_s = if sep_undefined {
            String::new()
        } else {
            vm.to_string_lossy(&sep)
        };
        if limit == 0 {
            return Ok(Value::Object(vm.new_array(vec![])));
        }
        if sep_undefined {
            return Ok(Value::Object(vm.new_array(vec![Value::str(s)])));
        }
        let mut out = Vec::new();
        if sep_s.is_empty() {
            for c in s.chars() {
                if out.len() >= limit {
                    break;
                }
                out.push(Value::str(c.to_string()));
            }
        } else {
            for part in s.split(&sep_s) {
                if out.len() >= limit {
                    break;
                }
                out.push(Value::str(part.to_string()));
            }
        }
        Ok(Value::Object(vm.new_array(out)))
    });

    // String.prototype.replace(searchValue, replaceValue). Dispatches to
    // searchValue[@@replace](this, replaceValue) when present; else literal
    // string replacement of the first occurrence.
    vm.define_method(proto, "replace", 2, |vm, this, args| {
        if this.is_nullish() {
            return Err(vm.throw_type("String.prototype.replace called on null or undefined"));
        }
        let search = arg(args, 0);
        let repl = arg(args, 1);
        if matches!(search, Value::Object(_)) {
            let key = PropertyKey::Sym(vm.realm.symbol_replace.clone());
            if let Some(m) = get_method(vm, &search, &key)? {
                // The @@replace receives the (coercible) `this`, not its string.
                return vm.call(m, search.clone(), &[this.clone(), repl]);
            }
        }
        replace_impl(vm, &this, args, false)
    });

    // String.prototype.replaceAll(searchValue, replaceValue). A RegExp
    // searchValue must be global (else TypeError) before symbol dispatch.
    vm.define_method(proto, "replaceAll", 2, |vm, this, args| {
        if this.is_nullish() {
            return Err(vm.throw_type("String.prototype.replaceAll called on null or undefined"));
        }
        let search = arg(args, 0);
        let repl = arg(args, 1);
        if matches!(search, Value::Object(_)) {
            // IsRegExp(searchValue): a non-global RegExp-like is a TypeError.
            if is_regexp_spec(vm, &search)? {
                let flags_v = vm.get_prop(&search, &PropertyKey::str("flags"))?;
                if flags_v.is_nullish() {
                    return Err(vm.throw_type("flags is null or undefined"));
                }
                let flags = vm.to_js_string(&flags_v)?;
                if !flags.as_str().contains('g') {
                    return Err(vm.throw_type("replaceAll must be called with a global RegExp"));
                }
            }
            let key = PropertyKey::Sym(vm.realm.symbol_replace.clone());
            if let Some(m) = get_method(vm, &search, &key)? {
                return vm.call(m, search.clone(), &[this.clone(), repl]);
            }
        }
        replace_impl(vm, &this, args, true)
    });

    // String.prototype.search(regexp): index of first match, or -1. Dispatches
    // to regexp[@@search](this) when present; else builds a RegExp from the
    // argument and uses its @@search.
    vm.define_method(proto, "search", 1, |vm, this, args| {
        if this.is_nullish() {
            return Err(vm.throw_type("String.prototype.search called on null or undefined"));
        }
        let regexp = arg(args, 0);
        if matches!(regexp, Value::Object(_)) {
            let key = PropertyKey::Sym(vm.realm.symbol_search.clone());
            if let Some(m) = get_method(vm, &regexp, &key)? {
                return vm.call(m, regexp.clone(), &[this.clone()]);
            }
        }
        // Fall back: RegExpCreate then Invoke(rx, @@search, «S»).
        let s = str_this(vm, &this)?;
        let sym = vm.realm.symbol_search.clone();
        invoke_regexp_sym(vm, &regexp, "", sym, s)
    });

    // String.prototype.match(regexp): dispatches to regexp[@@match](this) when
    // present; else builds a RegExp from the argument and uses its @@match.
    vm.define_method(proto, "match", 1, |vm, this, args| {
        // RequireObjectCoercible, then @@match dispatch with the original `this`
        // (NOT its string) — ToString(this) is only observed on the fallback path.
        if this.is_nullish() {
            return Err(vm.throw_type("String.prototype.match called on null or undefined"));
        }
        let regexp = arg(args, 0);
        if matches!(regexp, Value::Object(_)) {
            let key = PropertyKey::Sym(vm.realm.symbol_match.clone());
            if let Some(m) = get_method(vm, &regexp, &key)? {
                return vm.call(m, regexp.clone(), &[this.clone()]);
            }
        }
        let s = str_this(vm, &this)?;
        let sym = vm.realm.symbol_match.clone();
        invoke_regexp_sym(vm, &regexp, "", sym, s)
    });

    // String.prototype.matchAll(regexp): dispatches to regexp[@@matchAll](this)
    // when present; else builds a global RegExp from the argument and uses its
    // @@matchAll. A RegExp argument that lacks the global flag is a TypeError.
    vm.define_method(proto, "matchAll", 1, |vm, this, args| {
        if this.is_nullish() {
            return Err(vm.throw_type("String.prototype.matchAll called on null or undefined"));
        }
        let regexp = arg(args, 0);
        if matches!(regexp, Value::Object(_)) {
            // IsRegExp + a non-global flags string is a TypeError (spec 22.1.3.13),
            // checked via Get(regexp, "flags") before any @@matchAll dispatch.
            if is_regexp_spec(vm, &regexp)? {
                let flags_v = vm.get_prop(&regexp, &PropertyKey::str("flags"))?;
                if flags_v.is_nullish() {
                    return Err(vm.throw_type("flags is null or undefined"));
                }
                let flags = vm.to_js_string(&flags_v)?;
                if !flags.as_str().contains('g') {
                    return Err(vm.throw_type("matchAll must be called with a global RegExp"));
                }
            }
            let key = PropertyKey::Sym(vm.realm.symbol_match_all.clone());
            if let Some(m) = get_method(vm, &regexp, &key)? {
                return vm.call(m, regexp.clone(), &[this.clone()]);
            }
        }
        // Fall back: S = ToString(this), then RegExpCreate(regexp, "g") and
        // Invoke(rx, @@matchAll, «S»).
        let s = str_this(vm, &this)?;
        let sym = vm.realm.symbol_match_all.clone();
        invoke_regexp_sym(vm, &regexp, "g", sym, s)
    });

    // [Symbol.iterator]
    let sym = vm.realm.symbol_iterator.clone();
    vm.define_method(proto, "[Symbol.iterator]", 0, |vm, this, _a| {
        let s = str_this(vm, &this)?;
        Ok(vm.make_iterator(
            &vm.realm.string_iterator_proto.clone(),
            None,
            Some(JsString::new(s)),
            IterKind::StringChars,
        ))
    });
    let it = vm
        .get_prop(
            &Value::Object(proto.clone()),
            &PropertyKey::str("[Symbol.iterator]"),
        )
        .unwrap();
    proto
        .borrow_mut()
        .props
        .shift_remove(&PropertyKey::str("[Symbol.iterator]"));
    vm.define_value_sym(proto, sym, it);
}

fn pad(vm: &mut Vm, this: &Value, args: &[Value], start: bool) -> Result<Value, Value> {
    let s: Vec<char> = chars(&str_this(vm, this)?);
    let target = vm.to_length(&arg(args, 0))?;
    if s.len() >= target {
        return Ok(Value::str(s.into_iter().collect::<String>()));
    }
    if target > crate::value::MAX_STRING_LEN {
        return Err(vm.throw_range("Invalid string length"));
    }
    let filler = {
        let f = arg(args, 1);
        if f.is_undefined() {
            " ".to_string()
        } else {
            vm.to_js_string(&f)?.as_str().to_string()
        }
    };
    if filler.is_empty() {
        return Ok(Value::str(s.into_iter().collect::<String>()));
    }
    let pad_len = target - s.len();
    let fill_chars: Vec<char> = filler.chars().collect();
    let pad: String = (0..pad_len)
        .map(|i| fill_chars[i % fill_chars.len()])
        .collect();
    let base: String = s.into_iter().collect();
    Ok(Value::str(if start {
        format!("{pad}{base}")
    } else {
        format!("{base}{pad}")
    }))
}

fn replace_impl(vm: &mut Vm, this: &Value, args: &[Value], all: bool) -> Result<Value, Value> {
    let s = str_this(vm, this)?;
    // Spec order: ToString(searchValue), IsCallable(replaceValue), then a
    // non-functional replaceValue is coerced ONCE — eagerly, before any match
    // is attempted — with coercion errors propagating.
    let pattern = vm.to_js_string(&arg(args, 0))?.as_str().to_string();
    let repl = arg(args, 1);
    let is_fn = vm.is_callable(&repl);
    let repl_str: Option<String> = if is_fn {
        None
    } else {
        Some(vm.to_js_string(&repl)?.as_str().to_string())
    };

    let mut result = String::new();
    let mut rest = s.as_str();
    let mut replaced_any = false;
    loop {
        if pattern.is_empty() {
            // An empty pattern matches at the start; `replaceAll` matches at
            // EVERY position (between all characters and at the end).
            let replacement = compute_replacement(vm, &repl, repl_str.as_deref(), "", &s, 0)?;
            result.push_str(&replacement);
            if all {
                let mut byte = 0usize;
                for ch in s.chars() {
                    result.push(ch);
                    byte += ch.len_utf8();
                    let replacement =
                        compute_replacement(vm, &repl, repl_str.as_deref(), "", &s, byte)?;
                    result.push_str(&replacement);
                }
            } else {
                result.push_str(rest);
            }
            break;
        }
        match rest.find(&pattern) {
            Some(pos) if !replaced_any || all => {
                result.push_str(&rest[..pos]);
                let matched = &rest[pos..pos + pattern.len()];
                let offset = s.len() - rest.len() + pos;
                let replacement =
                    compute_replacement(vm, &repl, repl_str.as_deref(), matched, &s, offset)?;
                result.push_str(&replacement);
                rest = &rest[pos + pattern.len()..];
                replaced_any = true;
                if !all {
                    result.push_str(rest);
                    break;
                }
            }
            _ => {
                result.push_str(rest);
                break;
            }
        }
    }
    Ok(Value::str(result))
}

fn compute_replacement(
    vm: &mut Vm,
    repl: &Value,
    repl_str: Option<&str>,
    matched: &str,
    whole: &str,
    offset: usize,
) -> Result<String, Value> {
    if repl_str.is_none() {
        let r = vm.call(
            repl.clone(),
            Value::Undefined,
            &[
                Value::str(matched),
                Value::Number(whole[..offset].chars().count() as f64),
                Value::str(whole),
            ],
        )?;
        Ok(vm.to_js_string(&r)?.as_str().to_string())
    } else {
        let rs = repl_str.unwrap_or_default().to_string();
        // Handle $& (matched), $$ (literal $), $` (prefix), $' (suffix).
        let mut out = String::new();
        let mut chars = rs.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' {
                match chars.peek() {
                    Some('&') => {
                        out.push_str(matched);
                        chars.next();
                    }
                    Some('$') => {
                        out.push('$');
                        chars.next();
                    }
                    Some('`') => {
                        out.push_str(&whole[..offset]);
                        chars.next();
                    }
                    Some('\'') => {
                        out.push_str(&whole[offset + matched.len()..]);
                        chars.next();
                    }
                    _ => out.push('$'),
                }
            } else {
                out.push(c);
            }
        }
        Ok(out)
    }
}

fn rel(vm: &mut Vm, v: &Value, len: isize, default: isize) -> Result<isize, Value> {
    if v.is_undefined() {
        return Ok(default);
    }
    // ToIntegerOrInfinity: ±Infinity clamps to the ends (never wraps to 0).
    let mut i = to_integer_or_infinity(vm, v)?;
    if i < 0.0 {
        i += len as f64;
    }
    Ok(i.clamp(0.0, len as f64) as isize)
}

fn clamp_idx(vm: &mut Vm, v: &Value, len: isize, default: isize) -> Result<isize, Value> {
    if v.is_undefined() {
        return Ok(default);
    }
    let i = to_integer_or_infinity(vm, v)?;
    Ok(i.clamp(0.0, len as f64) as isize)
}

/// Coerce a value to a RegExp object for the String fallback paths
/// (match/search): pass through an existing RegExp, otherwise build one from its
/// string source.
#[allow(dead_code)]
fn coerce_regexp(vm: &mut Vm, v: &Value) -> Result<JsObject, Value> {
    if crate::regexp::is_regexp(v) {
        if let Value::Object(o) = v {
            return Ok(o.clone());
        }
    }
    // ToString(pattern) is observable and may throw (e.g. a poisoned `toString`).
    let src = if v.is_undefined() {
        String::new()
    } else {
        vm.to_js_string(v)?.as_str().to_string()
    };
    match vm.make_regexp(&src, "")? {
        Value::Object(o) => Ok(o),
        _ => Err(vm.throw_type("RegExp coercion failed")),
    }
}
