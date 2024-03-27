use crate::execution::primitives::serialized_value::RkyvSerializedValue;

fn functions_from_payload(payload: RkyvSerializedValue) {
    if let RkyvSerializedValue::Object(ref payload_map) = payload {
        if let Some(RkyvSerializedValue::Object(functions_map)) = payload_map.get("functions") {}
    }
}