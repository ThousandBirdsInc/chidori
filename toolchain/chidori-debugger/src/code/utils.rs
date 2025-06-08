use chidori_core::chidori_prompt_format::templating::templates::{SchemaItem, SchemaItemType};
use serde_json::{Map, Value};

pub fn populate_json_content(schema: &SchemaItem, existing_content: Option<&Value>) -> Value {
    match schema.ty {
        SchemaItemType::Object => {
            let mut obj = match existing_content {
                Some(Value::Object(map)) => map.clone(),
                _ => Map::new(),
            };

            // Remove keys that are not in the schema
            obj.retain(|key, _| schema.items.contains_key(key));

            for (key, item) in &schema.items {
                let existing_value = obj.get(key).cloned();
                obj.insert(key.clone(), populate_json_content(item, existing_value.as_ref()));
            }
            Value::Object(obj)
        },
        SchemaItemType::Array => {
            match existing_content {
                Some(Value::Array(arr)) => {
                    if let Some((_, item)) = schema.items.iter().next() {
                        Value::Array(arr.iter().map(|v| populate_json_content(item, Some(v))).collect())
                    } else {
                        Value::Array(vec![])
                    }
                },
                _ => {
                    if let Some((_, item)) = schema.items.iter().next() {
                        Value::Array(vec![populate_json_content(item, None)])
                    } else {
                        Value::Array(vec![])
                    }
                }
            }
        },
        SchemaItemType::String => {
            match existing_content {
                Some(Value::String(s)) => Value::String(s.clone()),
                _ => Value::String(String::new()),
            }
        },
    }
} 