use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

pub fn execute_memory_action(
    action: &str,
    namespace: &str,
    key: Option<&str>,
    value: Option<&Value>,
    prefix: &str,
) -> Result<Value> {
    let dir = PathBuf::from(".chidori").join("memory");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", sanitize_namespace(namespace)));

    let load = || -> Result<serde_json::Map<String, Value>> {
        if !file.exists() {
            return Ok(serde_json::Map::new());
        }
        let text = std::fs::read_to_string(&file)?;
        if text.trim().is_empty() {
            return Ok(serde_json::Map::new());
        }
        match serde_json::from_str::<Value>(&text)? {
            Value::Object(map) => Ok(map),
            _ => Ok(serde_json::Map::new()),
        }
    };

    let save = |map: &serde_json::Map<String, Value>| -> Result<()> {
        let text = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
        std::fs::write(&file, text)?;
        Ok(())
    };

    match action {
        "get" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("memory(\"get\") requires key"))?;
            let map = load()?;
            Ok(map.get(key).cloned().unwrap_or(Value::Null))
        }
        "set" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("memory(\"set\") requires key"))?;
            let value = value
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("memory(\"set\") requires value"))?;
            let mut map = load()?;
            map.insert(key.to_string(), value);
            save(&map)?;
            Ok(Value::Null)
        }
        "delete" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("memory(\"delete\") requires key"))?;
            let mut map = load()?;
            let existed = map.remove(key).is_some();
            save(&map)?;
            Ok(Value::Bool(existed))
        }
        "list" => {
            let map = load()?;
            let items: Vec<Value> = map
                .into_iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| json!({ "key": k, "value": v }))
                .collect();
            Ok(Value::Array(items))
        }
        "clear" => {
            save(&serde_json::Map::new())?;
            Ok(Value::Null)
        }
        other => Err(anyhow::anyhow!(
            "Unknown memory action: {other}. Expected get | set | delete | list | clear"
        )),
    }
}

pub fn sanitize_namespace(namespace: &str) -> String {
    namespace
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_list_delete_and_clear_work() {
        let namespace = format!("test-{}", uuid::Uuid::new_v4());

        execute_memory_action(
            "set",
            &namespace,
            Some("user_theme"),
            Some(&serde_json::json!("dark")),
            "",
        )
        .unwrap();
        execute_memory_action(
            "set",
            &namespace,
            Some("other"),
            Some(&serde_json::json!(1)),
            "",
        )
        .unwrap();

        let got = execute_memory_action("get", &namespace, Some("user_theme"), None, "").unwrap();
        assert_eq!(got, serde_json::json!("dark"));

        let listed = execute_memory_action("list", &namespace, None, None, "user_").unwrap();
        assert_eq!(
            listed,
            serde_json::json!([{ "key": "user_theme", "value": "dark" }])
        );

        let deleted =
            execute_memory_action("delete", &namespace, Some("user_theme"), None, "").unwrap();
        assert_eq!(deleted, serde_json::json!(true));

        execute_memory_action("clear", &namespace, None, None, "").unwrap();
        let listed = execute_memory_action("list", &namespace, None, None, "").unwrap();
        assert_eq!(listed, serde_json::json!([]));
    }
}
