//! Conversions between engine `Value`s and `serde_json::Value` (the host-effect
//! boundary type, matching the existing `SnapshotCapableJsEngine` seam), plus
//! error formatting.

use serde_json::Value as Json;

use crate::value::*;
use crate::vm::Vm;

impl Vm {
    /// Render a thrown value for host/error reporting (`Name: message` for Error
    /// objects, else its string form).
    pub fn error_to_string(&mut self, err: &Value) -> String {
        if let Value::Object(o) = err {
            if matches!(o.borrow().internal, Internal::Error) {
                let name = self
                    .get_prop(err, &PropertyKey::str("name"))
                    .map(|v| self.to_string_lossy(&v))
                    .unwrap_or_else(|_| "Error".into());
                let msg = self
                    .get_prop(err, &PropertyKey::str("message"))
                    .map(|v| self.to_string_lossy(&v))
                    .unwrap_or_default();
                return if msg.is_empty() {
                    name
                } else {
                    format!("{name}: {msg}")
                };
            }
        }
        self.to_string_lossy(err)
    }

    /// Engine value → JSON (host boundary). Mirrors `JSON.stringify` semantics
    /// for plain data; functions/symbols/undefined become `null`.
    pub fn value_to_json(&mut self, v: &Value) -> Json {
        let mut seen = Vec::new();
        self.value_to_json_inner(v, &mut seen)
    }

    fn value_to_json_inner(&mut self, v: &Value, seen: &mut Vec<usize>) -> Json {
        match v {
            Value::Undefined | Value::Uninitialized | Value::Hole | Value::Symbol(_) | Value::BigInt(_) => {
                Json::Null
            }
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => {
                if !n.is_finite() {
                    Json::Null
                } else if n.fract() == 0.0 && n.abs() < 9.007199254740992e15 {
                    // Whole number: emit as an integer (matches JSON.stringify and
                    // avoids `42.0` vs `42` representation mismatches at the host
                    // boundary).
                    Json::Number((*n as i64).into())
                } else {
                    serde_json::Number::from_f64(*n)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                }
            }
            Value::String(s) => Json::String(s.as_str().to_string()),
            Value::Object(o) => {
                if o.borrow().is_callable() {
                    return Json::Null;
                }
                let id = o.ptr_id();
                if seen.contains(&id) {
                    return Json::Null;
                }
                seen.push(id);
                let is_array = matches!(o.borrow().internal, Internal::Array(_));
                let result = if is_array {
                    let elems = if let Internal::Array(a) = &o.borrow().internal {
                        a.clone()
                    } else {
                        vec![]
                    };
                    Json::Array(
                        elems
                            .iter()
                            .map(|e| self.value_to_json_inner(e, seen))
                            .collect(),
                    )
                } else {
                    let keys = self.enumerable_own_string_keys(o);
                    let mut map = serde_json::Map::new();
                    for k in keys {
                        let val = self
                            .get_prop(v, &PropertyKey::Str(k.clone()))
                            .unwrap_or(Value::Undefined);
                        if !matches!(val, Value::Undefined) {
                            map.insert(k.as_str().to_string(), self.value_to_json_inner(&val, seen));
                        }
                    }
                    Json::Object(map)
                };
                seen.pop();
                result
            }
        }
    }

    /// JSON (host boundary) → engine value.
    pub fn json_to_value(&self, j: &Json) -> Value {
        match j {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            Json::String(s) => Value::str(s),
            Json::Array(a) => {
                let elems: Vec<Value> = a.iter().map(|e| self.json_to_value(e)).collect();
                Value::Object(self.new_array(elems))
            }
            Json::Object(m) => {
                let o = self.new_object();
                {
                    let mut b = o.borrow_mut();
                    for (k, val) in m {
                        b.props
                            .insert(PropertyKey::str(k), Property::data(self.json_to_value(val)));
                    }
                }
                Value::Object(o)
            }
        }
    }
}
