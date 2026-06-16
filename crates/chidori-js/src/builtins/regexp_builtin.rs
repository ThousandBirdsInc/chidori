//! `RegExp` constructor and `RegExp.prototype` (exec / test / toString, the flag
//! accessor getters, and the well-known symbol protocol). The matcher itself
//! lives in `crate::regexp`.
//!
//! The realm now exposes the `Symbol.match` / `Symbol.replace` / `Symbol.search`
//! / `Symbol.split` / `Symbol.matchAll` well-known symbols (see `realm.rs`), so
//! `RegExp.prototype` installs `@@match`, `@@matchAll`, `@@replace`, `@@search`,
//! and `@@split` methods here (spec 22.2.6). `String.prototype.{match, matchAll,
//! replace, replaceAll, search, split}` dispatch through these symbol methods
//! when the argument carries one (spec 22.1.3), which is what makes user RegExp
//! subclasses (and the corresponding Test262 cases) pass; when the argument is a
//! plain string the String methods fall back to their direct behavior.
//!
//! All of these build on the spec-correct `exec` semantics implemented here
//! (`regexp_exec_impl`): `lastIndex` get/set, global+sticky advancement, capture
//! groups with `.index`, `.input`, and `groups`.

use super::arg;
use crate::regexp::{
    is_regexp, regex_exec, regexp_group_names, regexp_source_flags, ReMatch, REGEXP_MARK,
};
use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    let proto = vm.realm.regexp_proto.clone();

    // RegExp(pattern, flags?) / RegExp(regexpObj[, flags]).
    let ctor = vm.new_native_ctor(
        "RegExp",
        2,
        |vm, _t, args| construct_regexp(vm, args),
        |vm, _t, args| construct_regexp(vm, args),
    );
    vm.install_ctor("RegExp", &ctor, &proto);
    vm.install_species(&ctor);

    // RegExp.escape(S) — ES2025. A pure string transform that escapes `S` so it
    // matches literally in a RegExp. `S` must be a String (no ToString coercion).
    vm.define_method(&ctor, "escape", 1, |vm, _t, args| {
        let v = arg(args, 0);
        if !matches!(v, Value::String(_)) {
            return Err(vm.throw_type("RegExp.escape requires a string argument"));
        }
        let s = vm.to_string_lossy(&v);
        Ok(Value::str(regexp_escape(&s)))
    });

    // RegExp.prototype.exec
    vm.define_method(&proto, "exec", 1, |vm, this, args| {
        let re = regexp_this(vm, &this)?;
        let s = vm.to_js_string(&arg(args, 0))?;
        regexp_exec_impl(vm, &re, &s)
    });

    // RegExp.prototype.test = !!RegExpExec(this, S) — generic over the receiver.
    vm.define_method(&proto, "test", 1, |vm, this, args| {
        require_object(vm, &this, "test")?;
        let s = vm.to_js_string(&arg(args, 0))?;
        let res = regexp_exec_abstract(vm, &this, &s)?;
        Ok(Value::Bool(!res.is_null()))
    });

    // RegExp.prototype.toString -> "/source/flags"
    vm.define_method(&proto, "toString", 0, |vm, this, _a| {
        let o = match &this {
            Value::Object(o) => o.clone(),
            _ => return Err(vm.throw_type("RegExp.prototype.toString called on non-object")),
        };
        let source = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("source"))?;
        let flags = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("flags"))?;
        let src = vm.to_string_lossy(&source);
        let fl = vm.to_string_lossy(&flags);
        Ok(Value::str(format!("/{src}/{fl}")))
    });

    // Accessor getters. `source` reports the (escaped-empty -> "(?:)") pattern;
    // `flags` assembles the canonical "dgimsuyv" order from the stored flags;
    // each individual flag getter inspects the stored flags string.
    //
    // When the receiver is `RegExp.prototype` itself (the bare intrinsic, which
    // is not a real RegExp), the spec mandates: `source` -> "(?:)",
    // `flags` -> "", and every boolean flag getter -> `undefined` (NOT false).
    define_string_getter(vm, &proto, "source", |_vm, src, _fl| Value::str(src));
    // `flags` is a generic getter: it reads the individual flag properties off
    // the receiver (any object) and assembles the string in canonical order.
    let flags_getter = vm.new_native("get flags", 0, |vm, this, _a| {
        let o = match &this {
            Value::Object(o) => o.clone(),
            _ => return Err(vm.throw_type("RegExp.prototype.flags getter called on non-object")),
        };
        let ov = Value::Object(o);
        let mut out = String::new();
        for (prop, ch) in [
            ("hasIndices", 'd'),
            ("global", 'g'),
            ("ignoreCase", 'i'),
            ("multiline", 'm'),
            ("dotAll", 's'),
            ("unicode", 'u'),
            ("unicodeSets", 'v'),
            ("sticky", 'y'),
        ] {
            let v = vm.get_prop(&ov, &PropertyKey::str(prop))?;
            if vm.to_boolean(&v) {
                out.push(ch);
            }
        }
        Ok(Value::str(out))
    });
    install_accessor(&proto, "flags", flags_getter);
    define_flag_getter(vm, &proto, "hasIndices", 'd');
    define_flag_getter(vm, &proto, "global", 'g');
    define_flag_getter(vm, &proto, "ignoreCase", 'i');
    define_flag_getter(vm, &proto, "multiline", 'm');
    define_flag_getter(vm, &proto, "dotAll", 's');
    define_flag_getter(vm, &proto, "unicode", 'u');
    define_flag_getter(vm, &proto, "unicodeSets", 'v');
    define_flag_getter(vm, &proto, "sticky", 'y');

    install_symbol_protocol(vm, &proto);
}

/// Install `RegExp.prototype[@@match/@@matchAll/@@replace/@@search/@@split]`.
fn install_symbol_protocol(vm: &mut Vm, proto: &JsObject) {
    // [Symbol.match](string) — generic over the receiver.
    let f = vm.new_native("[Symbol.match]", 1, |vm, this, args| {
        require_object(vm, &this, "Symbol.match")?;
        let s = vm.to_js_string(&arg(args, 0))?;
        sym_match_generic(vm, &this, &s)
    });
    let sym = vm.realm.symbol_match.clone();
    vm.define_value_sym(proto, sym, Value::Object(f));

    // [Symbol.matchAll](string) — generic over the receiver.
    let f = vm.new_native("[Symbol.matchAll]", 1, |vm, this, args| {
        require_object(vm, &this, "Symbol.matchAll")?;
        let s = vm.to_js_string(&arg(args, 0))?;
        sym_match_all_generic(vm, &this, &s)
    });
    let sym = vm.realm.symbol_match_all.clone();
    vm.define_value_sym(proto, sym, Value::Object(f));

    // [Symbol.replace](string, replaceValue) — generic over the receiver.
    let f = vm.new_native("[Symbol.replace]", 2, |vm, this, args| {
        require_object(vm, &this, "Symbol.replace")?;
        let s = vm.to_js_string(&arg(args, 0))?;
        sym_replace_generic(vm, &this, &s, &arg(args, 1))
    });
    let sym = vm.realm.symbol_replace.clone();
    vm.define_value_sym(proto, sym, Value::Object(f));

    // [Symbol.search](string) — generic over the receiver.
    let f = vm.new_native("[Symbol.search]", 1, |vm, this, args| {
        require_object(vm, &this, "Symbol.search")?;
        let s = vm.to_js_string(&arg(args, 0))?;
        sym_search_generic(vm, &this, &s)
    });
    let sym = vm.realm.symbol_search.clone();
    vm.define_value_sym(proto, sym, Value::Object(f));

    // [Symbol.split](string, limit) — generic over the receiver (spec
    // 22.2.6.14): a fresh sticky splitter is built via the species
    // constructor, and matching runs through the `exec` protocol so RegExp
    // subclasses and RegExp-like plain objects both work.
    let f = vm.new_native("[Symbol.split]", 2, |vm, this, args| {
        require_object(vm, &this, "Symbol.split")?;
        let s = vm.to_js_string(&arg(args, 0))?;
        let default_ctor = vm.get_prop(
            &Value::Object(vm.realm.regexp_proto.clone()),
            &PropertyKey::str("constructor"),
        )?;
        let c = species_constructor(vm, &this, &default_ctor)?;
        let flags_v = vm.get_prop(&this, &PropertyKey::str("flags"))?;
        let flags = vm.to_js_string(&flags_v)?.as_str().to_string();
        let unicode = flags.contains('u') || flags.contains('v');
        let new_flags = if flags.contains('y') {
            flags
        } else {
            format!("{flags}y")
        };
        let splitter = vm.construct(&c, &[this.clone(), Value::str(&new_flags)], &c)?;
        let limit_arg = arg(args, 1);
        let lim = if limit_arg.is_undefined() {
            u32::MAX as usize
        } else {
            vm.to_uint32(&limit_arg)? as usize
        };
        let mut out: Vec<Value> = Vec::new();
        if lim == 0 {
            return Ok(Value::Object(vm.new_array(out)));
        }
        let units = s.to_utf16_vec();
        let size = units.len();
        if size == 0 {
            let z = regexp_exec_abstract(vm, &splitter, &s)?;
            if z.is_null() {
                out.push(Value::String(s.clone()));
            }
            return Ok(Value::Object(vm.new_array(out)));
        }
        let li_key = PropertyKey::str("lastIndex");
        let mut p = 0usize;
        let mut q = 0usize;
        while q < size {
            vm.set_prop_strict(&splitter, &li_key, Value::Number(q as f64))?;
            let z = regexp_exec_abstract(vm, &splitter, &s)?;
            if z.is_null() {
                q = advance_string_index(&units, q, unicode);
            } else {
                let li = vm.get_prop(&splitter, &li_key)?;
                let e = vm.to_length(&li)?.min(size);
                if e == p {
                    q = advance_string_index(&units, q, unicode);
                } else {
                    out.push(Value::String(JsString::from_code_units(&units[p..q])));
                    if out.len() == lim {
                        return Ok(Value::Object(vm.new_array(out)));
                    }
                    p = e;
                    let len_v = vm.get_prop(&z, &PropertyKey::str("length"))?;
                    let ncap = vm.to_length(&len_v)?.saturating_sub(1);
                    for i in 1..=ncap {
                        let cap = vm.get_prop(&z, &PropertyKey::from_index(i as u32))?;
                        out.push(cap);
                        if out.len() == lim {
                            return Ok(Value::Object(vm.new_array(out)));
                        }
                    }
                    q = p;
                }
            }
        }
        out.push(Value::String(JsString::from_code_units(&units[p..size])));
        Ok(Value::Object(vm.new_array(out)))
    });
    let sym = vm.realm.symbol_split.clone();
    vm.define_value_sym(proto, sym, Value::Object(f));
}

/// Validate a flags string: reject any unknown flag character and any
/// duplicate. Returns the (unchanged) flags on success, or an error message.
fn validate_flags(flags: &str) -> Result<(), String> {
    let mut seen = [false; 128];
    for c in flags.chars() {
        let ok = matches!(c, 'd' | 'g' | 'i' | 'm' | 's' | 'u' | 'v' | 'y');
        if !ok {
            return Err("Invalid regular expression flags".to_string());
        }
        let idx = c as usize;
        if idx < 128 && seen[idx] {
            return Err("Invalid regular expression flags".to_string());
        }
        if idx < 128 {
            seen[idx] = true;
        }
    }
    Ok(())
}

/// `new RegExp(pattern, flags?)`. If `pattern` is itself a RegExp, copy its
/// source (and its flags, unless an explicit `flags` argument overrides them).
/// Flags are validated (duplicate or unknown -> SyntaxError) and `lastIndex` is
/// installed as a writable data property initialized to 0 (by `make_regexp`).
fn construct_regexp(vm: &mut Vm, args: &[Value]) -> Result<Value, Value> {
    let pat_arg = arg(args, 0);
    let flags_arg = arg(args, 1);

    // RegExp(regexp) / RegExp(regexp, flags): copy source, override flags.
    if is_regexp(&pat_arg) {
        if let Value::Object(o) = &pat_arg {
            let (source, existing_flags) = regexp_source_flags(o);
            let flags = if flags_arg.is_undefined() {
                existing_flags
            } else {
                vm.to_string_lossy(&flags_arg)
            };
            if let Err(msg) = validate_flags(&flags) {
                return Err(vm.throw_syntax(&msg));
            }
            return vm.make_regexp(&source, &flags);
        }
    }

    let pattern = if pat_arg.is_undefined() {
        String::new()
    } else {
        vm.to_string_lossy(&pat_arg)
    };
    let flags = if flags_arg.is_undefined() {
        String::new()
    } else {
        vm.to_string_lossy(&flags_arg)
    };
    if let Err(msg) = validate_flags(&flags) {
        return Err(vm.throw_syntax(&msg));
    }
    vm.make_regexp(&pattern, &flags)
}

/// Require `this` to be an Object (for the generic `@@match`/`@@replace`/etc.),
/// throwing a TypeError otherwise.
fn require_object(vm: &mut Vm, this: &Value, method: &str) -> Result<(), Value> {
    if matches!(this, Value::Object(_)) {
        Ok(())
    } else {
        Err(vm.throw_type(&format!(
            "RegExp.prototype[{method}] called on a non-object"
        )))
    }
}

/// Coerce `this` to a RegExp object, throwing if it is not one.
/// `RegExp.escape(S)` per ES2025: escape every code point of `s` so the result
/// matches `s` literally in a RegExp. A leading ASCII alphanumeric is hex-escaped
/// so the output can't merge with preceding text.
fn regexp_escape(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == 0 && c.is_ascii_alphanumeric() {
            // Always ≤ 0x7F here, so a 2-digit \xHH suffices.
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push_str(&encode_for_regexp_escape(c));
        }
    }
    out
}

/// EncodeForRegExpEscape(c) (ES2025).
fn encode_for_regexp_escape(c: char) -> String {
    // SyntaxCharacter ∪ "/": escape with a backslash.
    const SYNTAX: &str = "^$\\.*+?()[]{}|/";
    if SYNTAX.contains(c) {
        return format!("\\{c}");
    }
    // ControlEscape code points get their mnemonic escape.
    match c {
        '\u{09}' => return "\\t".to_string(),
        '\u{0A}' => return "\\n".to_string(),
        '\u{0B}' => return "\\v".to_string(),
        '\u{0C}' => return "\\f".to_string(),
        '\u{0D}' => return "\\r".to_string(),
        _ => {}
    }
    // Other punctuators + whitespace + line terminators are hex-escaped so the
    // result stays a single, unambiguous, ASCII-safe token.
    const OTHER_PUNCTUATORS: &str = ",-=<>#&!%:;@~'`\"";
    let is_ws_or_lt = matches!(
        c,
        '\u{20}' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    );
    if OTHER_PUNCTUATORS.contains(c) || is_ws_or_lt {
        let cp = c as u32;
        if cp <= 0xFF {
            return format!("\\x{cp:02x}");
        } else if cp <= 0xFFFF {
            return format!("\\u{cp:04x}");
        } else {
            let v = cp - 0x10000;
            let hi = 0xD800 + (v >> 10);
            let lo = 0xDC00 + (v & 0x3FF);
            return format!("\\u{hi:04x}\\u{lo:04x}");
        }
    }
    c.to_string()
}

fn regexp_this(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    match this {
        Value::Object(o)
            if o.borrow()
                .props
                .contains_key(&PropertyKey::str(REGEXP_MARK)) =>
        {
            Ok(o.clone())
        }
        _ => Err(vm.throw_type("Method RegExp.prototype called on incompatible receiver")),
    }
}

/// Core exec (RegExpBuiltinExec): honors `lastIndex` for global/sticky regexps,
/// updates it after a match, resets it to 0 on a failed global/sticky match,
/// and returns a match array with `.index`, `.input`, and `groups` (always
/// `undefined` since named groups are not supported), or `null`.
pub fn regexp_exec_impl(vm: &mut Vm, re: &JsObject, input: &JsString) -> Result<Value, Value> {
    let (source, flags) = regexp_source_flags(re);
    let global = flags.contains('g');
    let sticky = flags.contains('y');

    let units: Vec<u16> = input.to_utf16_vec();

    // Read lastIndex via Get (it is a writable data property), then ToLength.
    let last_index = {
        let li = vm.get_prop(&Value::Object(re.clone()), &PropertyKey::str("lastIndex"))?;
        vm.to_length(&li)?
    };

    // Only the g and y flags observe and advance lastIndex; otherwise scan from 0.
    let start = if global || sticky { last_index } else { 0 };

    if start > units.len() {
        if global || sticky {
            vm.set_prop(
                &Value::Object(re.clone()),
                &PropertyKey::str("lastIndex"),
                Value::Number(0.0),
            )?;
        }
        return Ok(Value::Null);
    }

    // For a sticky regexp the matcher itself enforces a match exactly at
    // `start`; for plain/global it scans forward from `start`.
    let m = regex_exec(&source, &flags, &units, start);
    match m {
        None => {
            if global || sticky {
                vm.set_prop(
                    &Value::Object(re.clone()),
                    &PropertyKey::str("lastIndex"),
                    Value::Number(0.0),
                )?;
            }
            Ok(Value::Null)
        }
        Some(mat) => {
            if global || sticky {
                vm.set_prop(
                    &Value::Object(re.clone()),
                    &PropertyKey::str("lastIndex"),
                    Value::Number(mat.end as f64),
                )?;
            }
            let names = regexp_group_names(&source, &flags);
            let has_indices = flags.contains('d');
            Ok(build_match_array(
                vm,
                &units,
                &mat,
                input,
                &names,
                has_indices,
            ))
        }
    }
}

/// Abstract `RegExpExec(R, S)`: if `R` has a callable `exec`, call it (validating
/// the result is Object or Null); otherwise fall back to the builtin exec, which
/// requires `R` to be a real RegExp. Lets the generic `@@match`/`@@replace`/etc.
/// honor a user-overridden `exec`.
fn regexp_exec_abstract(vm: &mut Vm, rx: &Value, s: &JsString) -> Result<Value, Value> {
    let exec = vm.get_prop(rx, &PropertyKey::str("exec"))?;
    if vm.is_callable(&exec) {
        let result = vm.call(exec, rx.clone(), &[Value::String(s.clone())])?;
        if !matches!(result, Value::Object(_) | Value::Null) {
            return Err(vm.throw_type("RegExp exec method returned a non-object, non-null value"));
        }
        return Ok(result);
    }
    let o = match rx {
        Value::Object(o)
            if o.borrow()
                .props
                .contains_key(&PropertyKey::str(REGEXP_MARK)) =>
        {
            o.clone()
        }
        _ => return Err(vm.throw_type("Method RegExp.prototype called on incompatible receiver")),
    };
    regexp_exec_impl(vm, &o, s)
}

/// Spec `AdvanceStringIndex`: in unicode mode, step over a whole surrogate pair
/// (`+2`) when `index` begins one; otherwise advance one code unit.
fn advance_string_index(units: &[u16], index: usize, unicode: bool) -> usize {
    if unicode
        && index + 1 < units.len()
        && (0xD800..=0xDBFF).contains(&units[index])
        && (0xDC00..=0xDFFF).contains(&units[index + 1])
    {
        index + 2
    } else {
        index + 1
    }
}

/// Generic `RegExp.prototype[@@match]` (spec 22.2.6.8): operates on any receiver
/// via `RegExpExec` and the `global`/`unicode`/`lastIndex` properties.
pub fn sym_match_generic(vm: &mut Vm, rx: &Value, s: &JsString) -> Result<Value, Value> {
    let g = vm.get_prop(rx, &PropertyKey::str("global"))?;
    let global = vm.to_boolean(&g);
    if !global {
        return regexp_exec_abstract(vm, rx, s);
    }
    let u = vm.get_prop(rx, &PropertyKey::str("unicode"))?;
    let unicode = vm.to_boolean(&u);
    let units = s.to_utf16_vec();
    vm.set_prop(rx, &PropertyKey::str("lastIndex"), Value::Number(0.0))?;
    let mut out: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_abstract(vm, rx, s)?;
        if result.is_null() {
            break;
        }
        let m0 = vm.get_prop(&result, &PropertyKey::from_index(0))?;
        let match_str = vm.to_js_string(&m0)?;
        let empty = match_str.len_utf16() == 0;
        out.push(Value::String(match_str));
        if empty {
            let li = vm.get_prop(rx, &PropertyKey::str("lastIndex"))?;
            let li = vm.to_length(&li)?;
            let next = advance_string_index(&units, li, unicode);
            vm.set_prop(
                rx,
                &PropertyKey::str("lastIndex"),
                Value::Number(next as f64),
            )?;
        }
    }
    if out.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(Value::Object(vm.new_array(out)))
    }
}

/// Generic `RegExp.prototype[@@search]` (spec 22.2.6.12).
pub fn sym_search_generic(vm: &mut Vm, rx: &Value, s: &JsString) -> Result<Value, Value> {
    let prev = vm.get_prop(rx, &PropertyKey::str("lastIndex"))?;
    if !crate::value::same_value(&prev, &Value::Number(0.0)) {
        vm.set_prop(rx, &PropertyKey::str("lastIndex"), Value::Number(0.0))?;
    }
    let result = regexp_exec_abstract(vm, rx, s)?;
    let cur = vm.get_prop(rx, &PropertyKey::str("lastIndex"))?;
    if !crate::value::same_value(&cur, &prev) {
        vm.set_prop(rx, &PropertyKey::str("lastIndex"), prev)?;
    }
    if result.is_null() {
        Ok(Value::Number(-1.0))
    } else {
        vm.get_prop(&result, &PropertyKey::str("index"))
    }
}

/// `SpeciesConstructor(O, defaultConstructor)` (spec 7.3.23).
fn species_constructor(vm: &mut Vm, o: &Value, default_ctor: &Value) -> Result<Value, Value> {
    let c = vm.get_prop(o, &PropertyKey::str("constructor"))?;
    if c.is_undefined() {
        return Ok(default_ctor.clone());
    }
    if !matches!(c, Value::Object(_)) {
        return Err(vm.throw_type("constructor is not an object"));
    }
    let sym = vm.realm.symbol_species.clone();
    let s = vm.get_prop(&c, &PropertyKey::Sym(sym))?;
    if s.is_nullish() {
        return Ok(default_ctor.clone());
    }
    if vm.is_constructor(&s) {
        Ok(s)
    } else {
        Err(vm.throw_type("Symbol.species is not a constructor"))
    }
}

/// Generic `RegExp.prototype[@@matchAll]` (spec 22.2.6.9): build a fresh matcher
/// via the species constructor, copy `lastIndex`, and return a lazy
/// RegExpStringIterator over it.
pub fn sym_match_all_generic(vm: &mut Vm, rx: &Value, s: &JsString) -> Result<Value, Value> {
    let default_ctor = vm.get_prop(
        &Value::Object(vm.realm.regexp_proto.clone()),
        &PropertyKey::str("constructor"),
    )?;
    let c = species_constructor(vm, rx, &default_ctor)?;
    let flags_v = vm.get_prop(rx, &PropertyKey::str("flags"))?;
    let flags = vm.to_js_string(&flags_v)?.as_str().to_string();
    let matcher = vm.construct(&c, &[rx.clone(), Value::str(&flags)], &c)?;
    let li_v = vm.get_prop(rx, &PropertyKey::str("lastIndex"))?;
    let li = vm.to_length(&li_v)?;
    vm.set_prop(
        &matcher,
        &PropertyKey::str("lastIndex"),
        Value::Number(li as f64),
    )?;
    let global = flags.contains('g');
    let unicode = flags.contains('u') || flags.contains('v');
    Ok(make_regexp_string_iterator(vm, matcher, s, global, unicode))
}

/// Build a lazy RegExpStringIterator: each `next()` runs `RegExpExec(matcher, S)`
/// and advances `lastIndex` past an empty match when global.
fn make_regexp_string_iterator(
    vm: &mut Vm,
    matcher: Value,
    s: &JsString,
    global: bool,
    unicode: bool,
) -> Value {
    use std::cell::RefCell;
    use std::rc::Rc;
    let proto = vm.realm.iterator_proto.clone();
    let iter = vm.new_object_proto(Some(proto));
    // (matcher, S, global, unicode, done)
    let state: Rc<RefCell<(Value, JsString, bool, bool, bool)>> = Rc::new(RefCell::new((
        matcher,
        s.clone(),
        global,
        unicode,
        false,
    )));
    let next = vm.new_native("next", 0, move |vm, _this, _a| {
        let (matcher, s, global, unicode, done) = {
            let st = state.borrow();
            (st.0.clone(), st.1.clone(), st.2, st.3, st.4)
        };
        if done {
            return Ok(vm.make_iter_result(Value::Undefined, true));
        }
        let result = regexp_exec_abstract(vm, &matcher, &s)?;
        if result.is_null() {
            state.borrow_mut().4 = true;
            return Ok(vm.make_iter_result(Value::Undefined, true));
        }
        if !global {
            state.borrow_mut().4 = true;
            return Ok(vm.make_iter_result(result, false));
        }
        let m0 = vm.get_prop(&result, &PropertyKey::from_index(0))?;
        let match_str = vm.to_js_string(&m0)?;
        if match_str.len_utf16() == 0 {
            let li = vm.get_prop(&matcher, &PropertyKey::str("lastIndex"))?;
            let li = vm.to_length(&li)?;
            let next_i = advance_string_index(&s.to_utf16_vec(), li, unicode);
            vm.set_prop(
                &matcher,
                &PropertyKey::str("lastIndex"),
                Value::Number(next_i as f64),
            )?;
        }
        Ok(vm.make_iter_result(result, false))
    });
    iter.borrow_mut().props.insert(
        PropertyKey::str("next"),
        Property::builtin(Value::Object(next)),
    );
    Value::Object(iter)
}

/// Generic `RegExp.prototype[@@replace]` (spec 22.2.6.11): honors a user
/// `exec`, the `global`/`unicode` flags, and reads each result's
/// `index`/`length`/captures/`groups` via property access.
pub fn sym_replace_generic(
    vm: &mut Vm,
    rx: &Value,
    s: &JsString,
    repl: &Value,
) -> Result<Value, Value> {
    let units = s.to_utf16_vec();
    let length_s = units.len();
    let functional = vm.is_callable(repl);
    // The replacement template is itself a string of code units (its `$…`
    // syntax is ASCII, but the literal text may carry surrogates).
    let templ: Vec<u16> = if functional {
        Vec::new()
    } else {
        vm.to_js_string(repl)?.to_utf16_vec()
    };
    let g = vm.get_prop(rx, &PropertyKey::str("global"))?;
    let global = vm.to_boolean(&g);
    let unicode = if global {
        let u = vm.get_prop(rx, &PropertyKey::str("unicode"))?;
        let unicode = vm.to_boolean(&u);
        vm.set_prop(rx, &PropertyKey::str("lastIndex"), Value::Number(0.0))?;
        unicode
    } else {
        false
    };
    // Collect all results (one for non-global, every match for global).
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_abstract(vm, rx, s)?;
        if result.is_null() {
            break;
        }
        results.push(result.clone());
        if !global {
            break;
        }
        let m0 = vm.get_prop(&result, &PropertyKey::from_index(0))?;
        let match_str = vm.to_js_string(&m0)?;
        if match_str.len_utf16() == 0 {
            let li = vm.get_prop(rx, &PropertyKey::str("lastIndex"))?;
            let li = vm.to_length(&li)?;
            let next = advance_string_index(&units, li, unicode);
            vm.set_prop(
                rx,
                &PropertyKey::str("lastIndex"),
                Value::Number(next as f64),
            )?;
        }
    }
    let mut accumulated: Vec<u16> = Vec::new();
    let mut next_pos = 0usize;
    for result in &results {
        let len_v = vm.get_prop(result, &PropertyKey::str("length"))?;
        let result_length = vm.to_length(&len_v)?;
        let n_captures = result_length.saturating_sub(1);
        let m0 = vm.get_prop(result, &PropertyKey::from_index(0))?;
        let matched_js = vm.to_js_string(&m0)?;
        let matched: Vec<u16> = matched_js.to_utf16_vec();
        let match_length = matched.len();
        let pos_v = vm.get_prop(result, &PropertyKey::str("index"))?;
        let position = vm.to_length(&pos_v)?.min(length_s);
        let mut captures: Vec<Option<Vec<u16>>> = Vec::with_capacity(n_captures);
        for n in 1..=n_captures.max(1) {
            if n > n_captures {
                break;
            }
            let cap = vm.get_prop(result, &PropertyKey::from_index(n as u32))?;
            if cap.is_undefined() {
                captures.push(None);
            } else {
                captures.push(Some(vm.to_js_string(&cap)?.to_utf16_vec()));
            }
        }
        let named = vm.get_prop(result, &PropertyKey::str("groups"))?;
        let replacement: Vec<u16> = if functional {
            let mut args: Vec<Value> = Vec::with_capacity(captures.len() + 4);
            args.push(Value::String(matched_js.clone()));
            for c in &captures {
                args.push(match c {
                    Some(cs) => Value::String(JsString::from_code_units(cs)),
                    None => Value::Undefined,
                });
            }
            args.push(Value::Number(position as f64));
            args.push(Value::String(s.clone()));
            if !named.is_undefined() {
                args.push(named.clone());
            }
            let r = vm.call(repl.clone(), Value::Undefined, &args)?;
            vm.to_js_string(&r)?.to_utf16_vec()
        } else {
            let named_obj = if named.is_undefined() {
                Value::Undefined
            } else {
                Value::Object(vm.to_object(&named)?)
            };
            get_substitution(vm, &matched, &units, position, &captures, &named_obj, &templ)?
        };
        if position >= next_pos {
            accumulated.extend_from_slice(&units[next_pos..position]);
            accumulated.extend_from_slice(&replacement);
            next_pos = position + match_length;
        }
    }
    if next_pos < length_s {
        accumulated.extend_from_slice(&units[next_pos..]);
    }
    Ok(Value::String(JsString::from_code_units(&accumulated)))
}

/// GetSubstitution (spec 22.2.6.10.1): expand `$`-substitutions in a string
/// replacement, given the already-stringified `captures` and an optional
/// `named` captures object for `$<name>`.
fn get_substitution(
    vm: &mut Vm,
    matched: &[u16],
    units: &[u16],
    position: usize,
    captures: &[Option<Vec<u16>>],
    named: &Value,
    templ: &[u16],
) -> Result<Vec<u16>, Value> {
    // The `$…` substitution syntax is all ASCII, so it is read directly off the
    // code units; the inserted segments (match, captures, `$\`` / `$'` slices)
    // carry through as code units so surrogates survive.
    const DOLLAR: u16 = b'$' as u16;
    let is_digit = |u: u16| (b'0' as u16..=b'9' as u16).contains(&u);
    let tail = position + matched.len();
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < templ.len() {
        if templ[i] == DOLLAR && i + 1 < templ.len() {
            let c = templ[i + 1];
            match c {
                _ if c == DOLLAR => {
                    out.push(DOLLAR);
                    i += 2;
                }
                _ if c == b'&' as u16 => {
                    out.extend_from_slice(matched);
                    i += 2;
                }
                _ if c == b'`' as u16 => {
                    out.extend_from_slice(&units[..position]);
                    i += 2;
                }
                _ if c == b'\'' as u16 => {
                    if tail < units.len() {
                        out.extend_from_slice(&units[tail..]);
                    }
                    i += 2;
                }
                _ if c == b'<' as u16 && !named.is_undefined() => {
                    match templ[i + 2..].iter().position(|&ch| ch == b'>' as u16) {
                        Some(rel) => {
                            let gt = i + 2 + rel;
                            let name = String::from_utf16_lossy(&templ[i + 2..gt]);
                            let cap = vm.get_prop(named, &PropertyKey::str(&name))?;
                            if !cap.is_undefined() {
                                out.extend(vm.to_js_string(&cap)?.code_units());
                            }
                            i = gt + 1;
                        }
                        None => {
                            out.push(DOLLAR);
                            i += 1;
                        }
                    }
                }
                _ if is_digit(c) => {
                    let mut n = (c - b'0' as u16) as usize;
                    let mut consumed = 2;
                    // Prefer the two-digit form when it names a valid capture.
                    if i + 2 < templ.len() && is_digit(templ[i + 2]) {
                        let nn = n * 10 + (templ[i + 2] - b'0' as u16) as usize;
                        if nn >= 1 && nn <= captures.len() {
                            n = nn;
                            consumed = 3;
                        }
                    }
                    if n >= 1 && n <= captures.len() {
                        if let Some(Some(cap)) = captures.get(n - 1) {
                            out.extend_from_slice(cap);
                        }
                        i += consumed;
                    } else {
                        out.push(DOLLAR);
                        i += 1;
                    }
                }
                _ => {
                    out.push(DOLLAR);
                    i += 1;
                }
            }
        } else {
            out.push(templ[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Build the `[matched, group1, ...]` array with own `index`, `input`, and
/// `groups` properties. Absent capture groups are `undefined` elements. When the
/// pattern declares named groups, `groups` is a null-prototype object mapping
/// each name to its captured substring (or `undefined`); otherwise it is
/// `undefined` (the property is always present).
pub fn build_match_array(
    vm: &mut Vm,
    units: &[u16],
    mat: &ReMatch,
    input: &JsString,
    names: &[(String, usize)],
    has_indices: bool,
) -> Value {
    let mut elems: Vec<Value> = Vec::with_capacity(mat.groups.len());
    for g in &mat.groups {
        match g {
            Some((s, e)) => {
                elems.push(Value::String(JsString::from_code_units(&units[*s..*e])));
            }
            None => elems.push(Value::Undefined),
        }
    }
    // `groups`: undefined when there are no named groups, else a null-proto
    // object keyed by name (in declaration order).
    let groups = if names.is_empty() {
        Value::Undefined
    } else {
        let obj = vm.alloc_ordinary(None);
        {
            let mut gb = obj.borrow_mut();
            for (name, idx) in names {
                let val = match mat.groups.get(*idx).and_then(|g| *g) {
                    Some((s, e)) => Value::String(JsString::from_code_units(&units[s..e])),
                    None => Value::Undefined,
                };
                gb.props.insert(PropertyKey::str(name), Property::data(val));
            }
        }
        Value::Object(obj)
    };
    // `d` flag: an `indices` array of `[start, end]` pairs per group (plus a
    // `groups` sub-object for named groups) — MakeMatchIndicesIndexPairArray.
    let indices = if has_indices {
        let pair = |s: usize, e: usize, vm: &mut Vm| {
            Value::Object(vm.new_array(vec![Value::Number(s as f64), Value::Number(e as f64)]))
        };
        let mut idx_elems: Vec<Value> = Vec::with_capacity(mat.groups.len());
        for g in &mat.groups {
            match g {
                Some((s, e)) => {
                    let (s, e) = (*s, *e);
                    idx_elems.push(pair(s, e, vm));
                }
                None => idx_elems.push(Value::Undefined),
            }
        }
        let idx_groups = if names.is_empty() {
            Value::Undefined
        } else {
            let obj = vm.alloc_ordinary(None);
            for (name, gidx) in names {
                let val = match mat.groups.get(*gidx).and_then(|g| *g) {
                    Some((s, e)) => pair(s, e, vm),
                    None => Value::Undefined,
                };
                obj.borrow_mut()
                    .props
                    .insert(PropertyKey::str(name), Property::data(val));
            }
            Value::Object(obj)
        };
        let idx_arr = vm.new_array(idx_elems);
        idx_arr
            .borrow_mut()
            .props
            .insert(PropertyKey::str("groups"), Property::data(idx_groups));
        Some(Value::Object(idx_arr))
    } else {
        None
    };
    let arr = vm.new_array(elems);
    {
        let mut b = arr.borrow_mut();
        b.props.insert(
            PropertyKey::str("index"),
            Property::data(Value::Number(mat.start as f64)),
        );
        b.props.insert(
            PropertyKey::str("input"),
            Property::data(Value::String(input.clone())),
        );
        b.props
            .insert(PropertyKey::str("groups"), Property::data(groups));
        if let Some(indices) = indices {
            b.props
                .insert(PropertyKey::str("indices"), Property::data(indices));
        }
    }
    Value::Object(arr)
}

// =========================================================================
// Symbol protocol implementations (operate on a branded RegExp `re`)
// =========================================================================


/// Install a string-valued accessor getter (`source`, `flags`) on the
/// prototype. On the bare `RegExp.prototype` receiver the canonical empty
/// values are reported.
fn define_string_getter(
    vm: &mut Vm,
    proto: &JsObject,
    name: &str,
    f: fn(&mut Vm, String, String) -> Value,
) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let o = match &this {
            Value::Object(o)
                if o.borrow()
                    .props
                    .contains_key(&PropertyKey::str(REGEXP_MARK)) =>
            {
                o.clone()
            }
            // RegExp.prototype itself reports the canonical empty source/flags.
            Value::Object(o) if o.same(&vm.realm.regexp_proto) => {
                return Ok(f(vm, "(?:)".to_string(), String::new()));
            }
            _ => return Err(vm.throw_type("RegExp getter called on incompatible receiver")),
        };
        let (source, flags) = regexp_source_flags(&o);
        Ok(f(vm, source, flags))
    });
    install_accessor(proto, name, getter);
}

/// Install a boolean flag accessor getter (`global`, `ignoreCase`, ...) keyed
/// off a single flag character. On the bare `RegExp.prototype` receiver the
/// getter returns `undefined` (per spec), NOT `false`.
fn define_flag_getter(vm: &mut Vm, proto: &JsObject, name: &str, flag: char) {
    let getter = vm.new_native(&format!("get {name}"), 0, move |vm, this, _a| {
        let o = match &this {
            Value::Object(o)
                if o.borrow()
                    .props
                    .contains_key(&PropertyKey::str(REGEXP_MARK)) =>
            {
                o.clone()
            }
            // RegExp.prototype itself is not a real RegExp: report undefined.
            Value::Object(o) if o.same(&vm.realm.regexp_proto) => {
                return Ok(Value::Undefined);
            }
            _ => return Err(vm.throw_type("RegExp getter called on incompatible receiver")),
        };
        let (_source, flags) = regexp_source_flags(&o);
        Ok(Value::Bool(flags.contains(flag)))
    });
    install_accessor(proto, name, getter);
}

/// Insert a non-enumerable, configurable accessor property with the given
/// getter and no setter.
fn install_accessor(proto: &JsObject, name: &str, getter: JsObject) {
    proto.borrow_mut().props.insert(
        PropertyKey::str(name),
        Property {
            kind: PropertyKind::Accessor {
                get: Some(Value::Object(getter)),
                set: None,
            },
            enumerable: false,
            configurable: true,
        },
    );
}
