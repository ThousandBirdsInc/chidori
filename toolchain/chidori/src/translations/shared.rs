use serde_json::Value as JsonValue;
use std::collections::VecDeque;
use prompt_graph_core::proto2::SerializedValue;
use prompt_graph_core::templates::json_value_to_serialized_value;

pub fn json_value_to_paths(
    d: &JsonValue,
) -> Vec<(Vec<String>, SerializedValue)> {
    let mut paths = Vec::new();
    let mut queue: VecDeque<(Vec<String>, &JsonValue)> = VecDeque::new();
    queue.push_back((Vec::new(), d));

    while let Some((mut path, dict)) = queue.pop_front() {
        match dict {
            JsonValue::Object(map) => {
                for (key, val) in map {
                    let key_str = key.clone();
                    path.push(key_str.clone());
                    match val {
                        JsonValue::Object(_) => {
                            queue.push_back((path.clone(), val));
                        },
                        _ => {
                            paths.push((path.clone(), json_value_to_serialized_value(&val)));
                        }
                    }
                    path.pop();
                }
            },
            JsonValue::Array(arr) => {
                for (i, val) in arr.iter().enumerate() {
                    path.push(i.to_string());
                    match val {
                        JsonValue::Object(_) => {
                            queue.push_back((path.clone(), val));
                        },
                        _ => {
                            paths.push((path.clone(), json_value_to_serialized_value(&val)));
                        }
                    }
                    path.pop();
                }
            },
            _ => panic!("Root should be a JSON object but was {:?}", d),
        }
    }

    paths
}