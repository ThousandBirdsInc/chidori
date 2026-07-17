use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

/// Persist memory under `<base>/.chidori/memory/`. `base` is the run's
/// workspace root (the agent file's directory under `run`/`resume`/`serve`,
/// or `CHIDORI_WORKSPACE_ROOT`), so memory is anchored to the agent — like
/// runs and workspace — instead of to the process's current directory.
/// Running the same agent from a different cwd now sees the same store.
pub fn execute_memory_action(
    base: &Path,
    action: &str,
    namespace: &str,
    key: Option<&str>,
    value: Option<&Value>,
    prefix: &str,
) -> Result<Value> {
    let dir = base.join(".chidori").join("memory");
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
        let base = std::env::temp_dir().join(format!("chidori-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let namespace = format!("test-{}", uuid::Uuid::new_v4());

        execute_memory_action(
            &base,
            "set",
            &namespace,
            Some("user_theme"),
            Some(&serde_json::json!("dark")),
            "",
        )
        .unwrap();
        execute_memory_action(
            &base,
            "set",
            &namespace,
            Some("other"),
            Some(&serde_json::json!(1)),
            "",
        )
        .unwrap();

        let got =
            execute_memory_action(&base, "get", &namespace, Some("user_theme"), None, "").unwrap();
        assert_eq!(got, serde_json::json!("dark"));

        let listed = execute_memory_action(&base, "list", &namespace, None, None, "user_").unwrap();
        assert_eq!(
            listed,
            serde_json::json!([{ "key": "user_theme", "value": "dark" }])
        );

        let deleted =
            execute_memory_action(&base, "delete", &namespace, Some("user_theme"), None, "")
                .unwrap();
        assert_eq!(deleted, serde_json::json!(true));

        execute_memory_action(&base, "clear", &namespace, None, None, "").unwrap();
        let listed = execute_memory_action(&base, "list", &namespace, None, None, "").unwrap();
        assert_eq!(listed, serde_json::json!([]));

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn memory_is_anchored_to_the_base_dir_not_cwd() {
        // The store lives under `<base>/.chidori/memory`, so two different
        // base dirs are two independent stores regardless of the process cwd.
        let base_a = std::env::temp_dir().join(format!("chidori-mem-a-{}", uuid::Uuid::new_v4()));
        let base_b = std::env::temp_dir().join(format!("chidori-mem-b-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base_a).unwrap();
        std::fs::create_dir_all(&base_b).unwrap();

        execute_memory_action(
            &base_a,
            "set",
            "default",
            Some("k"),
            Some(&serde_json::json!("in-a")),
            "",
        )
        .unwrap();

        // base_a's value is readable under base_a...
        let from_a = execute_memory_action(&base_a, "get", "default", Some("k"), None, "").unwrap();
        assert_eq!(from_a, serde_json::json!("in-a"));
        // ...and absent under base_b.
        let from_b = execute_memory_action(&base_b, "get", "default", Some("k"), None, "").unwrap();
        assert_eq!(from_b, Value::Null);
        assert!(base_a.join(".chidori").join("memory").exists());

        let _ = std::fs::remove_dir_all(base_a);
        let _ = std::fs::remove_dir_all(base_b);
    }
}
