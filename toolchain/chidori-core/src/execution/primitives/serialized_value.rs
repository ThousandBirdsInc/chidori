use crate::cells::CellTypes;
use rkyv::{
    archived_root, check_archived_root,
    ser::{serializers::AllocSerializer, Serializer},
    Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize,
};
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum RkyvSerializedValue {
    StreamPointer(u32),

    /// Function pointers are to a specific cell and function name
    FunctionPointer(usize, String),

    Cell(CellTypes),

    // TODO: add Embedding
    Set(

        #[omit_bounds]
        #[archive_attr(omit_bounds)]
        HashSet<RkyvSerializedValue>
    ),

    Float(f32),
    Number(i32),
    String(String),
    Boolean(bool),
    Null,

    Array(
        #[omit_bounds]
        #[archive_attr(omit_bounds)]
        Vec<RkyvSerializedValue>,
    ),

    Object(
        #[omit_bounds]
        #[archive_attr(omit_bounds)]
        HashMap<String, RkyvSerializedValue>,
    ),
}

pub struct RkyvObjectBuilder {
    object: HashMap<String, RkyvSerializedValue>,
}

impl RkyvObjectBuilder {
    pub fn new() -> Self {
        RkyvObjectBuilder {
            object: HashMap::new(),
        }
    }

    pub fn insert_string(mut self, key: &str, value: String) -> Self {
        self.object
            .insert(key.to_string(), RkyvSerializedValue::String(value));
        self
    }

    pub fn insert_number(mut self, key: &str, value: i32) -> Self {
        self.object
            .insert(key.to_string(), RkyvSerializedValue::Number(value));
        self
    }

    pub fn insert_boolean(mut self, key: &str, value: bool) -> Self {
        self.object
            .insert(key.to_string(), RkyvSerializedValue::Boolean(value));
        self
    }

    // Method to insert nested objects
    pub fn insert_object(mut self, key: &str, value: RkyvObjectBuilder) -> Self {
        self.object
            .insert(key.to_string(), RkyvSerializedValue::Object(value.object));
        self
    }

    pub fn insert_value(mut self, key: &str, value: RkyvSerializedValue) -> Self {
        self.object.insert(key.to_string(), value);
        self
    }

    pub fn build(self) -> RkyvSerializedValue {
        RkyvSerializedValue::Object(self.object)
    }
}

impl std::cmp::Eq for RkyvSerializedValue {
}

impl std::cmp::PartialEq for RkyvSerializedValue {
    fn eq(&self, other: &Self) -> bool {
        if core::mem::discriminant(self) != core::mem::discriminant(other) {
            return false;
        }
        match self {
            RkyvSerializedValue::StreamPointer(a) => {
                match other {
                    RkyvSerializedValue::StreamPointer(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::FunctionPointer(a, b) => {
                match other {
                    RkyvSerializedValue::FunctionPointer(aa,bb ) => { a == aa && b == bb }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Cell(a) => {
                match other {
                    RkyvSerializedValue::Cell(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Set(a) => {
                match other {
                    RkyvSerializedValue::Set(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Float(a) => {
                match other {
                    RkyvSerializedValue::Float(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Number(a) => {
                match other {
                    RkyvSerializedValue::Number(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::String(a) => {
                match other {
                    RkyvSerializedValue::String(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Boolean(a) => {
                match other {
                    RkyvSerializedValue::Boolean(aa) => { a == aa }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Null => {
                match other {
                    RkyvSerializedValue::Null => { true }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Array(a) => {
                match other {
                    RkyvSerializedValue::Array(aa) => {
                        a == aa
                    }
                    _ => unreachable!()
                }
            }
            RkyvSerializedValue::Object(a) => {
                match other {
                    RkyvSerializedValue::Object(aa) => {
                        a == aa
                    }
                    _ => unreachable!()
                }
            }
        }
    }
}

impl std::hash::Hash for RkyvSerializedValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            RkyvSerializedValue::StreamPointer(pointer) => {
                pointer.hash(state);
            }
            RkyvSerializedValue::FunctionPointer(cell_idx, func_name) => {
                cell_idx.hash(state);
                func_name.hash(state);
            }
            RkyvSerializedValue::Cell(cell_type) => {
                unimplemented!();
            }
            RkyvSerializedValue::Set(set) => {
                for item in set {
                    item.hash(state);
                }
            }
            RkyvSerializedValue::Float(f) => {
                f.to_bits().hash(state);
            }
            RkyvSerializedValue::Number(n) => {
                n.hash(state);
            }
            RkyvSerializedValue::String(s) => {
                s.hash(state);
            }
            RkyvSerializedValue::Boolean(b) => {
                b.hash(state);
            }
            RkyvSerializedValue::Null => {
                0.hash(state); // Hash a constant for Null
            }
            RkyvSerializedValue::Array(arr) => {
                for item in arr {
                    item.hash(state);
                }
            }
            RkyvSerializedValue::Object(obj) => {
                let mut items: Vec<_> = obj.iter().collect();
                items.sort_by_key(|item| item.0);
                items.hash(state);
            }
        }
    }
}

impl std::cmp::PartialEq for ArchivedRkyvSerializedValue {
    fn eq(&self, other: &Self) -> bool {
        todo!()
    }
}

impl std::cmp::Eq for ArchivedRkyvSerializedValue {
}

impl std::hash::Hash for ArchivedRkyvSerializedValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        todo!()
    }
}

impl std::fmt::Display for RkyvSerializedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RkyvSerializedValue::StreamPointer(_) => write!(f, "StreamPointer"),
            RkyvSerializedValue::FunctionPointer(_, _) => write!(f, "FunctionPointer"),
            RkyvSerializedValue::Cell(_) => write!(f, "Cell"),
            RkyvSerializedValue::Float(_) => write!(f, "Float"),
            RkyvSerializedValue::Number(_) => write!(f, "Number"),
            RkyvSerializedValue::String(_) => write!(f, "String"),
            RkyvSerializedValue::Boolean(_) => write!(f, "Boolean"),
            RkyvSerializedValue::Null => write!(f, "Null"),
            RkyvSerializedValue::Array(vec) => {
                let shapes: Vec<String> = vec.iter().map(|item| item.to_string()).collect();
                write!(f, "Array[{}]", shapes.join(", "))
            }
            RkyvSerializedValue::Object(map) => {
                let shapes: Vec<String> = map
                    .iter()
                    .map(|(key, value)| format!("{}: {}", key, value))
                    .collect();
                write!(f, "Object{{{}}}", shapes.join(", "))
            }
            RkyvSerializedValue::Set(set) => {
                let shapes: Vec<String> = set
                    .iter()
                    .map(|value| format!("{}", value))
                    .collect();
                write!(f, "Set{{{}}}", shapes.join(", "))
            }
        }
    }
}

impl From<RkyvSerializedValue> for Vec<u8> {
    fn from(item: RkyvSerializedValue) -> Self {
        serialize_to_vec(&item)
    }
}

pub fn serialize_to_vec(v: &RkyvSerializedValue) -> Vec<u8> {
    let mut serializer = AllocSerializer::<4096>::default();
    serializer.serialize_value(v).unwrap();
    let buf = serializer.into_serializer().into_inner();
    buf.to_vec()
}

pub fn deserialize_from_buf(v: &[u8]) -> RkyvSerializedValue {
    let rkyv1 = unsafe { archived_root::<RkyvSerializedValue>(v) };
    let arg1: RkyvSerializedValue = rkyv1.deserialize(&mut rkyv::Infallible).unwrap();
    arg1
}

pub fn serialized_value_to_json_value(v: &RkyvSerializedValue) -> Value {
    match &v {

        RkyvSerializedValue::Float(f) => Value::Number(f.to_string().parse().unwrap()),
        RkyvSerializedValue::Number(n) => Value::Number(n.to_string().parse().unwrap()),
        RkyvSerializedValue::String(s) => Value::String(s.to_string()),
        RkyvSerializedValue::Boolean(b) => Value::Bool(*b),
        RkyvSerializedValue::Array(a) => Value::Array(
            a.iter()
                .map(|v| serialized_value_to_json_value(v))
                .collect(),
        ),
        RkyvSerializedValue::Object(a) => Value::Object(
            a.iter()
                .map(|(k, v)| (k.clone(), serialized_value_to_json_value(v)))
                .collect(),
        ),
        RkyvSerializedValue::FunctionPointer(_, _) => Value::Null,
        RkyvSerializedValue::StreamPointer(_) => Value::Null,
        RkyvSerializedValue::Cell(_) => Value::Null,
        RkyvSerializedValue::Null => Value::Null,
        RkyvSerializedValue::Set(a) => {
            a.iter()
                .map(|v| serialized_value_to_json_value(v))
                .collect()
        }
    }
}

/// Convert a serde_json::Value into a SerializedValue
pub fn json_value_to_serialized_value(jval: &Value) -> RkyvSerializedValue {
    match jval {
        Value::Number(n) => {
            if n.is_i64() {
                RkyvSerializedValue::Number(n.as_i64().unwrap() as i32)
            } else if n.is_f64() {
                RkyvSerializedValue::Float(n.as_f64().unwrap() as f32)
            } else {
                panic!("Invalid number value")
            }
        }
        Value::String(s) => RkyvSerializedValue::String(s.clone()),
        Value::Bool(b) => RkyvSerializedValue::Boolean(*b),
        Value::Array(a) => RkyvSerializedValue::Array(
            a.iter()
                .map(|v| json_value_to_serialized_value(v))
                .collect(),
        ),
        Value::Object(o) => {
            let mut map = HashMap::new();
            for (k, v) in o {
                map.insert(k.clone(), json_value_to_serialized_value(v));
            }
            RkyvSerializedValue::Object(map)
        }
        Value::Null => RkyvSerializedValue::Null,
        _ => panic!("Invalid value type"),
    }
}

// Implementing Serialize for RkyvSerializedValue
impl SerdeSerialize for RkyvSerializedValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Convert self to a serde_json::Value and then serialize that
        let value = serialized_value_to_json_value(self); // Use your existing function
        value.serialize(serializer)
    }
}

// Implementing Deserialize for RkyvSerializedValue
impl<'de> SerdeDeserialize<'de> for RkyvSerializedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize into a serde_json::Value first
        let value = SerdeDeserialize::deserialize(deserializer)?;

        // Convert the serde_json::Value to RkyvSerializedValue
        Ok(json_value_to_serialized_value(&value)) // Use your existing function
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkyv::{
        archived_root,
        ser::{serializers::AllocSerializer, Serializer},
        with::{ArchiveWith, DeserializeWith, SerializeWith},
        Deserialize, Infallible,
    };

    fn round_trip(value: RkyvSerializedValue) -> () {
        let mut serializer = AllocSerializer::<4096>::default();
        serializer.serialize_value(&value).unwrap();
        let buf = serializer.into_serializer().into_inner();
        let archived_value = unsafe { archived_root::<RkyvSerializedValue>(&buf) };
        check_archived_root::<RkyvSerializedValue>(&buf).unwrap();
        let deserialized: RkyvSerializedValue =
            archived_value.deserialize(&mut rkyv::Infallible).unwrap();
        assert_eq!(deserialized, value);
    }

    #[ignore]
    #[test]
    fn test_float() {
        let value = RkyvSerializedValue::Float(42.0);
        round_trip(value);
    }

    #[test]
    fn test_number() {
        let value = RkyvSerializedValue::Number(42);
        round_trip(value);
    }

    #[test]
    fn test_string() {
        let value = RkyvSerializedValue::String("Hello".to_string());
        round_trip(value);
    }

    #[test]
    fn test_boolean() {
        let value = RkyvSerializedValue::Boolean(true);
        round_trip(value);
    }

    #[test]
    fn test_array() {
        let value = RkyvSerializedValue::Array(vec![
            RkyvSerializedValue::Number(42),
            RkyvSerializedValue::Boolean(true),
        ]);
        round_trip(value);
    }

    #[test]
    fn test_object() {
        let mut map = HashMap::new();
        map.insert(
            "key".to_string(),
            RkyvSerializedValue::String("value".to_string()),
        );
        let value = RkyvSerializedValue::Object(map);
        round_trip(value);
    }

    #[test]
    fn test_serialize_to_vec() {
        let value = RkyvSerializedValue::String("Hello".to_string());
        let serialized_vec = serialize_to_vec(&value);

        // Verify if serialized_vec is non-empty, or any other conditions.
        assert!(!serialized_vec.is_empty());
    }

    #[test]
    fn test_deserialize_from_vec() {
        let value = RkyvSerializedValue::String("Hello".to_string());
        let serialized_vec = serialize_to_vec(&value);
        let deserialized_value = deserialize_from_buf(&serialized_vec);

        // Verify if deserialized_value matches the original value.
        assert_eq!(value, deserialized_value);
    }

    #[test]
    fn test_serialize_deserialize_cycle() {
        let value = RkyvSerializedValue::String("Hello".to_string());
        let serialized_vec = serialize_to_vec(&value);
        let deserialized_value = deserialize_from_buf(&serialized_vec);

        // Verify if deserialization after serialization yields the original value.
        assert_eq!(value, deserialized_value);

        // Further tests to ensure that serialization -> deserialization is an identity operation.
        let reserialized_vec = serialize_to_vec(&deserialized_value);
        assert_eq!(serialized_vec, reserialized_vec);
    }
}
