use rkyv::{
    archived_root, check_archived_root,
    ser::{serializers::AllocSerializer, Serializer},
    Archive, Deserialize, Serialize,
};
use std::collections::HashMap;

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq)]
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
