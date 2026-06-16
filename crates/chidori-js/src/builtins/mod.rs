//! Builtin installation. Each submodule installs one area of the standard
//! library onto the realm's intrinsic prototypes and global object. Submodules
//! are independent (they only read shared realm protos), which keeps them easy to
//! expand in parallel toward the conformance target (plan P5).

mod array;
mod async_builtins;
mod bigint;
mod collections;
mod disposable;

mod date;
pub(crate) mod fundamental;
mod numbers;

mod regexp_builtin;

pub(crate) mod reflect;
mod string;
mod typedarray;

use crate::value::*;
use crate::vm::Vm;

pub fn install(vm: &mut Vm) {
    fundamental::install(vm);
    install_restricted_properties(vm);
    numbers::install(vm);
    bigint::install(vm);
    array::install(vm);
    string::install(vm);

    regexp_builtin::install(vm);
    collections::install(vm);

    date::install(vm);
    async_builtins::install(vm);

    reflect::install(vm);
    typedarray::install(vm);
    crate::proxy::install(vm);
    disposable::install(vm);
    install_globals(vm);
}

/// Build the unique-per-realm %ThrowTypeError% intrinsic and poison
/// `Function.prototype.caller` / `Function.prototype.arguments` with it
/// (spec 10.2.4 / 20.2.3): both are accessor properties whose get and set
/// throw a TypeError. %ThrowTypeError% itself is non-extensible with
/// non-configurable, non-writable `length` (0) and `name` ("").
fn install_restricted_properties(vm: &mut Vm) {
    use std::rc::Rc;
    fn frozen(value: Value) -> Property {
        Property {
            kind: PropertyKind::Data {
                value,
                writable: false,
            },
            enumerable: false,
            configurable: false,
        }
    }
    let nf = NativeFunction {
        name: Rc::from(""),
        length: 0,
        func: Rc::new(|vm: &mut Vm, _t: Value, _a: &[Value]| {
            Err(vm.throw_type(
                "'caller', 'callee', and 'arguments' properties may not be accessed on \
                 strict mode functions or the arguments objects for calls to them",
            ))
        }),
        construct: None,
    };
    let tte = vm.alloc(ObjectData::new(
        Some(vm.realm.function_proto.clone()),
        Internal::Function(FunctionInner::Native(nf)),
    ));
    {
        let mut b = tte.borrow_mut();
        b.props
            .insert(PropertyKey::str("length"), frozen(Value::Number(0.0)));
        b.props
            .insert(PropertyKey::str("name"), frozen(Value::str("")));
        b.extensible = false;
    }
    vm.realm.throw_type_error = tte.clone();

    let poison = Property {
        kind: PropertyKind::Accessor {
            get: Some(Value::Object(tte.clone())),
            set: Some(Value::Object(tte)),
        },
        enumerable: false,
        configurable: true,
    };
    let fp = vm.realm.function_proto.clone();
    let mut b = fp.borrow_mut();
    b.props.insert(PropertyKey::str("caller"), poison.clone());
    b.props.insert(PropertyKey::str("arguments"), poison);
}

// ---- function-kind intrinsics (%GeneratorFunction% etc.) ----

/// Compile `(<prefix>(<params>) { <body> })` — the dynamic constructor body for a
/// generator/async function — and return the resulting function object.
fn compile_kind_function(vm: &mut Vm, prefix: &str, args: &[Value]) -> Result<Value, Value> {
    let (params, body) = if args.is_empty() {
        (String::new(), String::new())
    } else {
        let body = vm.to_string_lossy(&args[args.len() - 1]);
        let parts: Vec<String> = args[..args.len() - 1]
            .iter()
            .map(|a| vm.to_string_lossy(a))
            .collect();
        (parts.join(","), body)
    };
    let src = format!("({prefix}({params}\n) {{\n{body}\n}})");
    match crate::compiler::compile_script(&src) {
        Ok(proto) => {
            let f = vm.make_closure(std::rc::Rc::new(proto), Vec::new());
            vm.call(Value::Object(f), Value::Undefined, &[])
        }
        Err(msg) => Err(vm.throw_syntax(msg.trim_start_matches("SyntaxError: "))),
    }
}

/// Wire one function-kind intrinsic: build its (non-global) constructor, link it
/// to `func_proto` both ways, give `func_proto` its `Symbol.toStringTag` and (for
/// generators) its `.prototype` → instance prototype, and link the instance
/// prototype's `constructor`/`Symbol.toStringTag`.
fn install_kind_function(
    vm: &mut Vm,
    function_ctor: &JsObject,
    src_prefix: &'static str,
    ctor_name: &str,
    func_proto: JsObject,
    instance: Option<(JsObject, &str)>,
) {
    // Non-writable, non-enumerable, configurable data property.
    fn ro_c(value: Value) -> Property {
        Property {
            kind: PropertyKind::Data {
                value,
                writable: false,
            },
            enumerable: false,
            configurable: true,
        }
    }
    let tag_key = PropertyKey::Sym(vm.realm.symbol_to_string_tag.clone());

    let ctor = vm.new_native_ctor(
        ctor_name,
        1,
        move |vm, _t, args| compile_kind_function(vm, src_prefix, args),
        move |vm, _t, args| compile_kind_function(vm, src_prefix, args),
    );
    // [[Prototype]] of the constructor is %Function% (the Function constructor).
    ctor.borrow_mut().proto = Some(function_ctor.clone());
    // Constructor.prototype = func_proto (non-writable, non-enumerable, non-config).
    ctor.borrow_mut().props.insert(
        PropertyKey::str("prototype"),
        Property {
            kind: PropertyKind::Data {
                value: Value::Object(func_proto.clone()),
                writable: false,
            },
            enumerable: false,
            configurable: false,
        },
    );
    {
        let mut fp = func_proto.borrow_mut();
        fp.props
            .insert(PropertyKey::str("constructor"), ro_c(Value::Object(ctor)));
        fp.props
            .insert(tag_key.clone(), ro_c(Value::str(ctor_name)));
        if let Some((ip, _)) = &instance {
            fp.props.insert(
                PropertyKey::str("prototype"),
                ro_c(Value::Object(ip.clone())),
            );
        }
    }
    if let Some((ip, instance_tag)) = instance {
        let mut ipb = ip.borrow_mut();
        ipb.props.insert(
            PropertyKey::str("constructor"),
            ro_c(Value::Object(func_proto)),
        );
        ipb.props.insert(tag_key, ro_c(Value::str(instance_tag)));
    }
}

// ---- shared helpers ----

pub(crate) fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

/// Render a value for `console.log`/debugging (a small `util.inspect`).
pub(crate) fn inspect(vm: &mut Vm, v: &Value, depth: usize, seen: &mut Vec<usize>) -> String {
    match v {
        Value::Undefined | Value::Uninitialized | Value::Hole => "undefined".into(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => crate::vm::number_to_string(*n),
        Value::String(s) => {
            if depth == 0 {
                s.as_str().to_string()
            } else {
                format!("'{}'", s.as_str())
            }
        }
        Value::Symbol(s) => format!("Symbol({})", s.description().unwrap_or("")),
        Value::BigInt(n) => format!("{n}n"),
        Value::Object(o) => inspect_object(vm, o, depth, seen),
    }
}

fn inspect_object(vm: &mut Vm, o: &JsObject, depth: usize, seen: &mut Vec<usize>) -> String {
    let id = o.ptr_id();
    if seen.contains(&id) {
        return "[Circular]".into();
    }
    if depth > 6 {
        return "[Object]".into();
    }
    // Functions.
    {
        let b = o.borrow();
        if b.is_callable() {
            let name = b
                .props
                .get(&PropertyKey::str("name"))
                .and_then(|p| p.value().cloned())
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            if name.is_empty() {
                return "[Function (anonymous)]".into();
            }
            return format!("[Function: {name}]");
        }
        match &b.internal {
            Internal::Error => {
                drop(b);
                let name = vm
                    .get_prop(&Value::Object(o.clone()), &PropertyKey::str("name"))
                    .ok()
                    .map(|v| vm.to_string_lossy(&v))
                    .unwrap_or_else(|| "Error".into());
                let msg = vm
                    .get_prop(&Value::Object(o.clone()), &PropertyKey::str("message"))
                    .ok()
                    .map(|v| vm.to_string_lossy(&v))
                    .unwrap_or_default();
                return if msg.is_empty() {
                    name
                } else {
                    format!("{name}: {msg}")
                };
            }
            Internal::Promise(_) => return "Promise { <state> }".into(),
            _ => {}
        }
    }
    seen.push(id);
    let result = {
        let is_array = matches!(o.borrow().internal, Internal::Array(_));
        if is_array {
            let elems = if let Internal::Array(a) = &o.borrow().internal {
                a.clone()
            } else {
                vec![]
            };
            let parts: Vec<String> = elems
                .iter()
                .map(|e| inspect(vm, e, depth + 1, seen))
                .collect();
            format!("[ {} ]", parts.join(", ")).replace("[  ]", "[]")
        } else {
            let keys = vm.enumerable_own_string_keys(o);
            let mut parts = Vec::new();
            for k in keys {
                let val = vm
                    .get_prop(&Value::Object(o.clone()), &PropertyKey::Str(k.clone()))
                    .unwrap_or(Value::Undefined);
                let vs = inspect(vm, &val, depth + 1, seen);
                let key_disp = if is_ident(k.as_str()) {
                    k.as_str().to_string()
                } else {
                    format!("'{}'", k.as_str())
                };
                parts.push(format!("{key_disp}: {vs}"));
            }
            if parts.is_empty() {
                "{}".into()
            } else {
                format!("{{ {} }}", parts.join(", "))
            }
        }
    };
    seen.pop();
    result
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn install_globals(vm: &mut Vm) {
    let global = vm.realm.global.clone();

    // Value globals.
    vm.define_value(&global, "globalThis", Value::Object(global.clone()));
    // undefined / NaN / Infinity are non-writable, non-enumerable, non-configurable.
    vm.define_constant(&global, "undefined", Value::Undefined);
    vm.define_constant(&global, "NaN", Value::Number(f64::NAN));
    vm.define_constant(&global, "Infinity", Value::Number(f64::INFINITY));

    // console.
    let console = vm.new_object();
    for level in ["log", "info", "debug", "warn", "error", "trace"] {
        vm.define_method(&console, level, 0, move |vm, _this, args| {
            let mut parts = Vec::new();
            for a in args {
                let mut seen = Vec::new();
                parts.push(inspect(vm, a, 0, &mut seen));
            }
            vm.console_log.push(parts.join(" "));
            Ok(Value::Undefined)
        });
    }
    vm.define_value(&global, "console", Value::Object(console));

    // parseInt / parseFloat / isNaN / isFinite.
    vm.define_method(&global, "parseInt", 2, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        let radix = {
            let r = arg(args, 1);
            if r.is_undefined() {
                0
            } else {
                vm.to_int32(&r)?
            }
        };
        Ok(Value::Number(parse_int(s.as_str(), radix)))
    });
    vm.define_method(&global, "parseFloat", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        Ok(Value::Number(parse_float(s.as_str())))
    });
    vm.define_method(&global, "isNaN", 1, |vm, _t, args| {
        let n = vm.to_number(&arg(args, 0))?;
        Ok(Value::Bool(n.is_nan()))
    });
    vm.define_method(&global, "isFinite", 1, |vm, _t, args| {
        let n = vm.to_number(&arg(args, 0))?;
        Ok(Value::Bool(n.is_finite()))
    });

    // URI handling: encodeURI / decodeURI / encodeURIComponent /
    // decodeURIComponent. Percent-encoding operates on the UTF-8 bytes of the
    // string (Rust strings are already valid UTF-8, so lone surrogates cannot
    // occur). Malformed decode input throws a generic Error (the realm has no
    // URIError intrinsic).
    vm.define_method(&global, "encodeURI", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        Ok(Value::str(uri_encode(s.as_str(), URI_UNESCAPED_URI)))
    });
    vm.define_method(&global, "encodeURIComponent", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        Ok(Value::str(uri_encode(s.as_str(), URI_UNESCAPED_COMPONENT)))
    });
    vm.define_method(&global, "decodeURI", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        match uri_decode_units(&s.to_utf16_vec(), URI_RESERVED_URI) {
            Ok(units) => Ok(Value::String(JsString::from_code_units(&units))),
            Err(msg) => Err(vm.make_error(crate::vm::ErrorKind::Uri, msg)),
        }
    });
    vm.define_method(&global, "decodeURIComponent", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        match uri_decode_units(&s.to_utf16_vec(), "") {
            Ok(units) => Ok(Value::String(JsString::from_code_units(&units))),
            Err(msg) => Err(vm.make_error(crate::vm::ErrorKind::Uri, msg)),
        }
    });

    // structuredClone (deep clone via JSON-ish round-trip for plain data).
    vm.define_method(&global, "structuredClone", 1, |vm, _t, args| {
        let v = arg(args, 0);
        deep_clone(vm, &v)
    });

    // eval (global-scope / indirect semantics): compile the string as a script
    // and run it against the global object. Direct-eval access to the caller's
    // local scope is not modeled (a documented gap), but global-scope eval — what
    // `(0,eval)(s)` and `$262.evalScript` use — works.
    vm.define_method(&global, "eval", 1, |vm, _t, args| {
        let v = arg(args, 0);
        let src = match &v {
            Value::String(s) => s.as_str().to_string(),
            _ => return Ok(v), // non-string eval returns its argument unchanged
        };
        match crate::compiler::compile_indirect_eval(&src) {
            Ok(proto) => {
                let f = vm.make_closure(std::rc::Rc::new(proto), Vec::new());
                vm.call(Value::Object(f), Value::Undefined, &[])
            }
            Err(msg) => Err(vm.throw_syntax(msg.trim_start_matches("SyntaxError: "))),
        }
    });
    // Remember the %eval% intrinsic's identity: `Op::DirectEval` performs
    // direct-eval semantics only when the callee IS this object.
    if let Ok(Value::Object(ef)) =
        vm.get_prop(&Value::Object(global.clone()), &PropertyKey::str("eval"))
    {
        vm.realm.eval_fn = Some(ef);
    }

    // Function constructor: `new Function(p1, ..., body)` compiles a function
    // from source. Defined here (not in fundamental.rs) because it needs the
    // compiler. Bound as the `.constructor` of Function.prototype too.
    let make_function = |vm: &mut Vm, _t: Value, args: &[Value]| -> Result<Value, Value> {
        let (params, body) = if args.is_empty() {
            (String::new(), String::new())
        } else {
            // ToString is observable and may throw (a body/param object with a
            // poisoned toString propagates its error, not a SyntaxError).
            let body = vm.to_js_string(&args[args.len() - 1])?.as_str().to_string();
            let mut parts = Vec::new();
            for a in &args[..args.len() - 1] {
                parts.push(vm.to_js_string(a)?.as_str().to_string());
            }
            (parts.join(","), body)
        };
        let src = format!("(function anonymous({params}\n) {{\n{body}\n}})");
        match crate::compiler::compile_script(&src) {
            Ok(proto) => {
                let f = vm.make_closure(std::rc::Rc::new(proto), Vec::new());
                vm.call(Value::Object(f), Value::Undefined, &[])
            }
            Err(msg) => Err(vm.throw_syntax(msg.trim_start_matches("SyntaxError: "))),
        }
    };
    let function_ctor = vm.new_native_ctor("Function", 1, make_function, make_function);
    let function_proto = vm.realm.function_proto.clone();
    vm.install_ctor("Function", &function_ctor, &function_proto);

    // %GeneratorFunction%, %AsyncFunction%, %AsyncGeneratorFunction% — the dynamic
    // constructors for generator/async functions. They are reachable only via the
    // prototype chain of a generator/async function (e.g.
    // `Object.getPrototypeOf(function*(){}).constructor`), never as globals.
    install_kind_function(
        vm,
        &function_ctor,
        "function* anonymous",
        "GeneratorFunction",
        vm.realm.generator_function_proto.clone(),
        Some((vm.realm.generator_proto.clone(), "Generator")),
    );
    install_kind_function(
        vm,
        &function_ctor,
        "async function anonymous",
        "AsyncFunction",
        vm.realm.async_function_proto.clone(),
        None,
    );
    install_kind_function(
        vm,
        &function_ctor,
        "async function* anonymous",
        "AsyncGeneratorFunction",
        vm.realm.async_generator_function_proto.clone(),
        Some((vm.realm.async_generator_proto.clone(), "AsyncGenerator")),
    );

    // queueMicrotask.
    vm.define_method(&global, "queueMicrotask", 1, |vm, _t, args| {
        let cb = arg(args, 0);
        if !vm.is_callable(&cb) {
            return Err(vm.throw_type("queueMicrotask requires a function"));
        }
        vm.microtasks
            .push_back(crate::vm::Microtask::Job(Box::new(move |vm: &mut Vm| {
                vm.call(cb, Value::Undefined, &[]).map(|_| ())
            })));
        Ok(Value::Undefined)
    });
}

fn deep_clone(vm: &mut Vm, v: &Value) -> Result<Value, Value> {
    match v {
        Value::Object(o) => {
            let is_array = matches!(o.borrow().internal, Internal::Array(_));
            if is_array {
                let elems = if let Internal::Array(a) = &o.borrow().internal {
                    a.clone()
                } else {
                    vec![]
                };
                let mut out = Vec::with_capacity(elems.len());
                for e in &elems {
                    out.push(deep_clone(vm, e)?);
                }
                Ok(Value::Object(vm.new_array(out)))
            } else {
                let no = vm.new_object();
                for k in vm.enumerable_own_string_keys(o) {
                    let val = vm.get_prop(v, &PropertyKey::Str(k.clone()))?;
                    let cloned = deep_clone(vm, &val)?;
                    no.borrow_mut()
                        .props
                        .insert(PropertyKey::Str(k), Property::data(cloned));
                }
                Ok(Value::Object(no))
            }
        }
        _ => Ok(v.clone()),
    }
}

pub(crate) fn parse_int(s: &str, mut radix: i32) -> f64 {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut sign = 1.0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        if bytes[i] == b'-' {
            sign = -1.0;
        }
        i += 1;
    }
    if radix == 0 {
        radix = 10;
    }
    if radix == 16 || radix == 0 {
        if i + 1 < bytes.len() && bytes[i] == b'0' && (bytes[i + 1] | 32) == b'x' {
            i += 2;
            radix = 16;
        }
    } else if i + 1 < bytes.len() && bytes[i] == b'0' && (bytes[i + 1] | 32) == b'x' && radix == 16
    {
        i += 2;
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let start = i;
    let mut acc = 0.0;
    while i < bytes.len() {
        let c = bytes[i];
        let d = match c {
            b'0'..=b'9' => (c - b'0') as i32,
            b'a'..=b'z' => (c - b'a' + 10) as i32,
            b'A'..=b'Z' => (c - b'A' + 10) as i32,
            _ => break,
        };
        if d >= radix {
            break;
        }
        acc = acc * radix as f64 + d as f64;
        i += 1;
    }
    if i == start {
        return f64::NAN;
    }
    sign * acc
}

pub(crate) fn parse_float(s: &str) -> f64 {
    let s = s.trim_start();
    if s.starts_with("Infinity") || s.starts_with("+Infinity") {
        return f64::INFINITY;
    }
    if s.starts_with("-Infinity") {
        return f64::NEG_INFINITY;
    }
    // Find the longest valid float prefix.
    let bytes = s.as_bytes();
    let mut end = 0;
    let mut seen_dot = false;
    let mut seen_e = false;
    let mut seen_digit = false;
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                seen_digit = true;
                i += 1;
                end = i;
            }
            b'.' if !seen_dot && !seen_e => {
                seen_dot = true;
                i += 1;
            }
            b'e' | b'E' if !seen_e && seen_digit => {
                seen_e = true;
                i += 1;
                if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    if !seen_digit {
        return f64::NAN;
    }
    s[..end].parse::<f64>().unwrap_or(f64::NAN)
}

// ---- URI encoding/decoding helpers ----

// Characters that are NOT percent-encoded, per spec, beyond the always-allowed
// `A-Za-z0-9`. encodeURIComponent keeps only `uriMark`; encodeURI also keeps
// `uriReserved` plus `#`.
const URI_UNESCAPED_COMPONENT: &str = "-_.!~*'()";
const URI_UNESCAPED_URI: &str = "-_.!~*'();/?:@&=+$,#";
// During decodeURI, percent-escapes of these reserved characters are left as-is
// (not decoded). decodeURIComponent uses an empty preserved set.
const URI_RESERVED_URI: &str = ";/?:@&=+$,#";

fn uri_is_unreserved(b: u8, extra: &str) -> bool {
    b.is_ascii_alphanumeric() || extra.as_bytes().contains(&b)
}

fn uri_encode(s: &str, unescaped: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if uri_is_unreserved(b, unescaped) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0xf));
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Code-unit-preserving `Decode`: every non-`%` UTF-16 code unit is copied
/// through unchanged — including a lone surrogate, which a byte-oriented decode
/// over the lossy `&str` view would drop. `%XX` escape runs (always ASCII)
/// decode as UTF-8 to code points, then to code units.
fn uri_decode_units(units: &[u16], preserved: &str) -> Result<Vec<u16>, &'static str> {
    // Read the octet of a `%XX` escape at `pos` (which must point at `%`); the
    // escape characters are ASCII, so they live in the low byte of each unit.
    let octet = |pos: usize| -> Result<u8, &'static str> {
        if pos + 2 >= units.len() || units[pos] != b'%' as u16 {
            return Err("URI malformed");
        }
        let hi = (units[pos + 1] < 0x100)
            .then(|| hex_val(units[pos + 1] as u8))
            .flatten()
            .ok_or("URI malformed")?;
        let lo = (units[pos + 2] < 0x100)
            .then(|| hex_val(units[pos + 2] as u8))
            .flatten()
            .ok_or("URI malformed")?;
        Ok((hi << 4) | lo)
    };
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        if units[i] != b'%' as u16 {
            out.push(units[i]);
            i += 1;
            continue;
        }
        let b0 = octet(i)?;
        if b0 < 0x80 {
            if preserved.as_bytes().contains(&b0) {
                out.extend_from_slice(&units[i..i + 3]); // keep "%XX" verbatim
            } else {
                out.push(b0 as u16);
            }
            i += 3;
            continue;
        }
        let extra = if b0 & 0xe0 == 0xc0 {
            1
        } else if b0 & 0xf0 == 0xe0 {
            2
        } else if b0 & 0xf8 == 0xf0 {
            3
        } else {
            return Err("URI malformed");
        };
        let mut seq = vec![b0];
        let mut j = i + 3;
        for _ in 0..extra {
            let cont = octet(j)?;
            if cont & 0xc0 != 0x80 {
                return Err("URI malformed");
            }
            seq.push(cont);
            j += 3;
        }
        match std::str::from_utf8(&seq) {
            Ok(valid) => out.extend(valid.encode_utf16()),
            Err(_) => return Err("URI malformed"),
        }
        i = j;
    }
    Ok(out)
}
