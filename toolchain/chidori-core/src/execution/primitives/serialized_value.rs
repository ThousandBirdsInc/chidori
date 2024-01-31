use rkyv::{
    archived_root, check_archived_root,
    ser::{serializers::AllocSerializer, Serializer},
    Archive, Deserialize, Serialize,
};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Clone)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum RkyvSerializedValue {
    StreamPointer(u32),
    FunctionPointer(u32),
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

    pub fn build(self) -> RkyvSerializedValue {
        RkyvSerializedValue::Object(self.object)
    }
}

impl std::fmt::Display for RkyvSerializedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RkyvSerializedValue::StreamPointer(_) => write!(f, "StreamPointer"),
            RkyvSerializedValue::FunctionPointer(_) => write!(f, "FunctionPointer"),
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
        RkyvSerializedValue::FunctionPointer(_) => Value::Null,
        RkyvSerializedValue::StreamPointer(_) => Value::Null,
        RkyvSerializedValue::Null => Value::Null,
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
