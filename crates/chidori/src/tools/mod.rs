use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
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

/// How a tool is executed. Agents define tools in-VM with `defineTool(...)`
/// (executed by the agent's own runtime, never registered here); the registry
/// holds only tools that come from *outside* the agent: MCP-server tools and
/// Rust-native tools registered by an embedding application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolBackend {
    /// The tool is remote-hosted by an MCP server.
    Mcp {
        server_id: String,
        remote_name: String,
    },
    /// The tool is implemented as a Rust callback registered by an embedding
    /// application.
    #[default]
    Native,
}

/// A registered tool with its metadata and execution backend.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub params: Vec<ToolParam>,
    #[allow(dead_code)]
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

/// Registry of tools that come from outside the agent — MCP-server tools and
/// Rust-native tools. Agents define their own tools in-VM with `defineTool`;
/// those never enter this registry (they carry their schema inline and run in
/// the agent's own VM). An empty registry is the norm for a plain agent run.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolDef>,
    native_handlers: HashMap<String, NativeToolHandler>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
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
    /// registered, plus a hint that agent tools are defined with `defineTool`
    /// (they are never looked up here — an unknown NAME means an MCP/native
    /// tool that isn't registered, most often because the name is wrong or no
    /// MCP server provides it).
    pub fn describe_miss(&self, name: &str) -> String {
        let mut msg = format!("Unknown tool: {name}");
        let mut available: Vec<&str> = self.tools.keys().map(String::as_str).collect();
        available.sort_unstable();
        if available.is_empty() {
            msg.push_str(
                " — no MCP/native tools are registered. Agent-defined tools use \
                 `defineTool({...})` and are passed to `prompt({ tools: [handle] })` \
                 directly (not by name).",
            );
        } else {
            msg.push_str(&format!(" (registered: {})", available.join(", ")));
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn describe_miss_points_at_definetool_when_registry_is_empty() {
        let registry = ToolRegistry::new();
        let msg = registry.describe_miss("nope");
        assert!(msg.contains("defineTool"), "{msg}");
        assert!(msg.contains("no MCP/native tools are registered"), "{msg}");
    }
}
