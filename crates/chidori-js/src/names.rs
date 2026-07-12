//! Interned well-known name strings.
//!
//! Protocol names the interpreter looks up on its own initiative — iterator
//! stepping (`next`/`done`/`value`/`return`), promise resolution (`then`),
//! `ToPrimitive` (`valueOf`/`toString`), `typeof` results — used to be built
//! fresh (`Rc::from(str)`) at every use, so a `for..of` loop paid several
//! heap allocations per iteration just to spell the protocol. This table
//! builds each name once per thread; a use is an `Rc` bump.
//!
//! Interning also feeds the pointer-equality fast path in
//! [`JsString::eq`](crate::value::JsString): a property-map probe with an
//! interned key that lands on a slot inserted with the same interned string
//! confirms equality without touching the bytes.
//!
//! Thread-local rather than global because [`JsString`] is `Rc`-backed (the
//! engine is single-threaded per VM), matching the `EMPTY_RC_STR` precedent
//! in `value.rs`. Determinism is unaffected: the strings are identical to
//! the ones previously built inline, only their allocation is shared.

use crate::value::{JsString, PropertyKey};

macro_rules! well_known {
    ($($fn_name:ident => $lit:literal),+ $(,)?) => {
        struct Table {
            $($fn_name: JsString,)+
        }

        thread_local! {
            static TABLE: Table = Table {
                $($fn_name: JsString::new($lit),)+
            };
        }

        $(
            #[doc = concat!("Interned `\"", $lit, "\"`.")]
            pub fn $fn_name() -> JsString {
                TABLE.with(|t| t.$fn_name.clone())
            }
        )+
    };
}

well_known! {
    // Iterator protocol — read per step of every non-kernelized `for..of`,
    // spread, destructuring, and collection constructor.
    next => "next",
    done => "done",
    value => "value",
    ret => "return",
    // Promise resolution probes `then` once per settled value / `await`.
    then => "then",
    // OrdinaryToPrimitive method order — every `+`/`==`/relational compare
    // and template interpolation involving an object.
    value_of => "valueOf",
    to_string => "toString",
    // `typeof` results (all eight possible values).
    tof_undefined => "undefined",
    tof_object => "object",
    tof_boolean => "boolean",
    tof_number => "number",
    tof_string => "string",
    tof_symbol => "symbol",
    tof_function => "function",
    tof_bigint => "bigint",
}

/// Interned key: `"next"`.
pub fn key_next() -> PropertyKey {
    PropertyKey::Str(next())
}
/// Interned key: `"done"`.
pub fn key_done() -> PropertyKey {
    PropertyKey::Str(done())
}
/// Interned key: `"value"`.
pub fn key_value() -> PropertyKey {
    PropertyKey::Str(value())
}
/// Interned key: `"return"`.
pub fn key_return() -> PropertyKey {
    PropertyKey::Str(ret())
}
/// Interned key: `"then"`.
pub fn key_then() -> PropertyKey {
    PropertyKey::Str(then())
}
/// Interned key: `"valueOf"`.
pub fn key_value_of() -> PropertyKey {
    PropertyKey::Str(value_of())
}
/// Interned key: `"toString"`.
pub fn key_to_string() -> PropertyKey {
    PropertyKey::Str(to_string())
}

/// The interned string for a [`Value::type_of`](crate::value::Value::type_of)
/// result. `t` is one of the eight static `typeof` strings; anything else
/// (there is none today) falls back to a fresh allocation rather than
/// panicking.
pub fn typeof_result(t: &'static str) -> JsString {
    match t {
        "undefined" => tof_undefined(),
        "object" => tof_object(),
        "boolean" => tof_boolean(),
        "number" => tof_number(),
        "string" => tof_string(),
        "symbol" => tof_symbol(),
        "function" => tof_function(),
        "bigint" => tof_bigint(),
        _ => JsString::new(t),
    }
}

/// The interned typeof-name string for an arbitrary `&str`, or `None` when
/// it is not one of the eight. Lets the compiler build `"number"`-class
/// STRING CONSTANTS from the same allocation the interpreter's `typeof`
/// results use, so an unfused `typeof x === "number"` confirms equality on
/// the `JsString` pointer fast path instead of comparing bytes.
pub fn typeof_name(s: &str) -> Option<JsString> {
    Some(match s {
        "undefined" => tof_undefined(),
        "object" => tof_object(),
        "boolean" => tof_boolean(),
        "number" => tof_number(),
        "string" => tof_string(),
        "symbol" => tof_symbol(),
        "function" => tof_function(),
        "bigint" => tof_bigint(),
        _ => return None,
    })
}

/// The `&'static str` type tag matching an arbitrary string, when it is one
/// of the eight `typeof` names — the compile-time half of the fused
/// typeof-dispatch test (`ROp::TypeofBr`): `Value::type_of` returns exactly
/// these statics, so content equality of a typeof result against such a
/// literal is `type_of(v) == tag`.
pub fn typeof_tag(s: &str) -> Option<&'static str> {
    Some(match s {
        "undefined" => "undefined",
        "object" => "object",
        "boolean" => "boolean",
        "number" => "number",
        "string" => "string",
        "symbol" => "symbol",
        "function" => "function",
        "bigint" => "bigint",
        _ => return None,
    })
}
