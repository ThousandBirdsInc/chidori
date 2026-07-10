use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::snapshot::SourceFingerprint;

/// Schema for a tool parameter, derived from tool metadata or signatures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub param_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    pub required: bool,
}

/// How a tool is executed. File-backed tools are local `.ts` files; MCP-backed
/// tools are dispatched to a running MCP server child process via `McpManager`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolBackend {
    /// The tool's body lives in a local .ts file with `export const tool` and
    /// `export async function run(args, chidori)`.
    #[default]
    TypeScript,
    /// The tool is remote-hosted by an MCP server.
    Mcp {
        server_id: String,
        remote_name: String,
    },
    /// The tool is implemented as a Rust callback registered by an embedding
    /// application.
    Native,
}

/// A registered tool with its metadata and execution backend.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub params: Vec<ToolParam>,
    pub source_path: PathBuf,
    #[allow(dead_code)]
    pub source_fingerprint: Option<SourceFingerprint>,
    pub backend: ToolBackend,
}

pub type NativeToolHandler = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;

impl ToolDef {
    /// Generate a JSON schema suitable for LLM function-calling.
    #[allow(dead_code)]
    pub fn to_json_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for param in &self.params {
            let mut prop = serde_json::Map::new();
            prop.insert("type".to_string(), Value::String(param.param_type.clone()));
            if let Some(ref desc) = param.description {
                prop.insert("description".to_string(), Value::String(desc.clone()));
            }
            properties.insert(param.name.clone(), Value::Object(prop));
            if param.required {
                required.push(Value::String(param.name.clone()));
            }
        }

        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
            }
        })
    }
}

/// Registry of available tools loaded from local files and MCP servers.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolDef>,
    native_handlers: HashMap<String, NativeToolHandler>,
    /// Tool files that were found but failed to load (path, reason). Retained so
    /// a later "Unknown tool" error can explain *why* an expected tool isn't
    /// available, instead of the failure being a silent stderr warn.
    load_errors: Vec<(PathBuf, String)>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            native_handlers: HashMap::new(),
            load_errors: Vec::new(),
        }
    }

    pub fn register(&mut self, tool: ToolDef) {
        self.tools.insert(tool.name.clone(), tool);
    }

    #[allow(dead_code)]
    pub fn register_native(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        params: Vec<ToolParam>,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) {
        let name = name.into();
        self.native_handlers.insert(name.clone(), Arc::new(handler));
        self.register(ToolDef {
            name: name.clone(),
            description: description.into(),
            params,
            source_path: PathBuf::from(format!("native:{name}")),
            source_fingerprint: None,
            backend: ToolBackend::Native,
        });
    }

    pub fn dispatch_native(&self, name: &str, args: Value) -> Result<Value> {
        let Some(tool) = self.tools.get(name) else {
            anyhow::bail!("{}", self.describe_miss(name));
        };
        if tool.backend != ToolBackend::Native {
            anyhow::bail!("tool `{name}` is not registered as a native tool");
        }
        let Some(handler) = self.native_handlers.get(name) else {
            anyhow::bail!("native tool `{name}` has no registered handler");
        };
        handler(args)
    }

    /// Build an actionable "unknown tool" message: the tools that ARE
    /// registered plus any tool files that failed to load and why.
    pub fn describe_miss(&self, name: &str) -> String {
        let mut msg = format!("Unknown tool: {name}");
        let mut available: Vec<&str> = self.tools.keys().map(String::as_str).collect();
        available.sort_unstable();
        if available.is_empty() {
            msg.push_str(" (no tools are registered)");
        } else {
            msg.push_str(&format!(" (available: {})", available.join(", ")));
        }
        if !self.load_errors.is_empty() {
            let failed: Vec<String> = self
                .load_errors
                .iter()
                .map(|(path, reason)| {
                    let file = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<tool>");
                    let first_line = reason.lines().next().unwrap_or(reason);
                    format!("{file}: {first_line}")
                })
                .collect();
            msg.push_str(&format!(
                ". {} tool file(s) failed to load — {}",
                failed.len(),
                failed.join("; ")
            ));
        }
        msg
    }

    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.tools.get(name)
    }

    pub fn list(&self) -> Vec<&ToolDef> {
        let mut tools: Vec<_> = self.tools.values().collect();
        tools.sort_by_key(|t| &t.name);
        tools
    }

    /// Load all .ts files from the given directories and parse tool definitions.
    pub fn load_from_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut registry = Self::new();

        for dir in dirs {
            if !dir.exists() {
                tracing::info!(dir = %dir.display(), "tool directory does not exist");
                continue;
            }
            let entries = std::fs::read_dir(dir)
                .with_context(|| format!("Failed to read tool directory: {}", dir.display()))?;

            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext == "ts" {
                    match parse_tool_file(&path) {
                        Ok(tool) => {
                            registry.register(tool);
                        }
                        Err(e) => {
                            let reason = format!("{e:#}");
                            tracing::warn!(
                                path = %path.display(),
                                error = %reason,
                                "failed to parse tool file"
                            );
                            registry.load_errors.push((path.clone(), reason));
                        }
                    }
                }
            }
        }

        let tool_names: Vec<String> = registry
            .list()
            .into_iter()
            .map(|tool| tool.name.clone())
            .collect();
        tracing::info!(
            dirs = ?dirs,
            tools = ?tool_names,
            tool_count = tool_names.len(),
            "loaded tool registry"
        );

        Ok(registry)
    }

    /// [`ToolRegistry::load_from_dirs`] behind a process-wide fingerprint
    /// cache. The server builds a registry for EVERY agent run, re-reading and
    /// re-parsing each tool file from disk; here the walk only stats the
    /// files and reuses the parsed registry while nothing changed. Editing a
    /// tool file still takes effect on the next run — the fingerprint
    /// (path, mtime, len per `.ts` file) changes with it.
    pub fn load_from_dirs_cached(dirs: &[PathBuf]) -> Result<Arc<Self>> {
        type Fingerprint = Vec<(PathBuf, Option<std::time::SystemTime>, u64)>;
        type Cache = Mutex<HashMap<Vec<PathBuf>, (Fingerprint, Arc<ToolRegistry>)>>;
        static CACHE: OnceLock<Cache> = OnceLock::new();

        let mut fingerprint: Fingerprint = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("ts") {
                    let meta = entry.metadata().ok();
                    fingerprint.push((
                        path,
                        meta.as_ref().and_then(|m| m.modified().ok()),
                        meta.map(|m| m.len()).unwrap_or(0),
                    ));
                }
            }
        }
        fingerprint.sort();

        let cache = CACHE.get_or_init(Default::default);
        let key: Vec<PathBuf> = dirs.to_vec();
        if let Some((cached_fp, registry)) = cache.lock().unwrap().get(&key) {
            if *cached_fp == fingerprint {
                return Ok(registry.clone());
            }
        }
        let registry = Arc::new(Self::load_from_dirs(dirs)?);
        cache
            .lock()
            .unwrap()
            .insert(key, (fingerprint, registry.clone()));
        Ok(registry)
    }
}

fn parse_tool_file(path: &Path) -> Result<ToolDef> {
    parse_typescript_tool_file(path)
}

fn parse_typescript_tool_file(path: &Path) -> Result<ToolDef> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let source_fingerprint = SourceFingerprint::from_source(path, &source);
    let policy = crate::runtime::snapshot::RuntimePolicy::durable_default("tool-discovery");
    let checked = crate::runtime::typescript::tools::check_tool_source(path, &source, &policy)?;
    let params = params_from_json_schema(&checked.tool.parameters);

    Ok(ToolDef {
        name: checked.tool.name,
        description: checked.tool.description,
        params,
        source_path: path.to_path_buf(),
        source_fingerprint: Some(source_fingerprint),
        backend: ToolBackend::TypeScript,
    })
}

fn params_from_json_schema(schema: &Value) -> Vec<ToolParam> {
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut params: Vec<ToolParam> = properties
        .iter()
        .map(|(name, property)| ToolParam {
            name: name.clone(),
            description: property
                .get("description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            param_type: property
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("string")
                .to_string(),
            default: property.get("default").cloned(),
            required: required.contains(name.as_str()),
        })
        .collect();
    params.sort_by(|a, b| a.name.cmp(&b.name));
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_typescript_tool_metadata() {
        let dir =
            std::env::temp_dir().join(format!("chidori-ts-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("web_search.ts"),
            r#"
                import type { Chidori, ToolDefinition } from "chidori:agent";

                export const tool: ToolDefinition = {
                  name: "web_search",
                  description: "Search the web",
                  parameters: {
                    type: "object",
                    properties: {
                      query: { type: "string", description: "Search query" },
                      limit: { type: "integer", default: 3 },
                    },
                    required: ["query"],
                  },
                };

                export async function run(args: { query: string }, chidori: Chidori) {
                  return { query: args.query };
                }
            "#,
        )
        .unwrap();

        let registry = ToolRegistry::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        let tool = registry.get("web_search").unwrap();

        assert_eq!(tool.backend, ToolBackend::TypeScript);
        assert_eq!(
            tool.source_fingerprint.as_ref().unwrap(),
            &crate::runtime::snapshot::SourceFingerprint::from_source(
                dir.join("web_search.ts"),
                &std::fs::read_to_string(dir.join("web_search.ts")).unwrap()
            )
        );
        assert_eq!(tool.description, "Search the web");
        assert_eq!(tool.params.len(), 2);
        assert!(tool.params.iter().any(|param| param.name == "query"
            && param.required
            && param.description.as_deref() == Some("Search query")));
        assert!(tool.params.iter().any(|param| param.name == "limit"
            && !param.required
            && param.default == Some(Value::Number(3.into()))));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registry_ignores_starlark_tool_files_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-ignore-star-tool-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("legacy.star"),
            r#"
                def legacy(query):
                    "Legacy tool"
                    return query
            "#,
        )
        .unwrap();

        let registry = ToolRegistry::load_from_dirs(std::slice::from_ref(&dir)).unwrap();

        assert!(registry.get("legacy").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registry_dispatches_native_tool_handler() {
        let mut registry = ToolRegistry::new();
        registry.register_native(
            "echo",
            "Echo input",
            vec![ToolParam {
                name: "value".to_string(),
                description: Some("Value to echo".to_string()),
                param_type: "string".to_string(),
                default: None,
                required: true,
            }],
            Ok,
        );

        let tool = registry.get("echo").unwrap();
        assert_eq!(tool.backend, ToolBackend::Native);
        assert_eq!(tool.source_path, PathBuf::from("native:echo"));
        assert_eq!(
            registry
                .dispatch_native("echo", serde_json::json!({ "value": "ok" }))
                .unwrap(),
            serde_json::json!({ "value": "ok" })
        );
    }
}
