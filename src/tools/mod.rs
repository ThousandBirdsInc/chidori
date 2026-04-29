use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema for a tool parameter, derived from Starlark function signatures.
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

/// How a tool is executed. File-backed Starlark tools were the original
/// (and still default) path; MCP-backed tools are dispatched to a running
/// MCP server child process via `McpManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolBackend {
    /// The tool's body lives in a local .star file. `source_path` + `source`
    /// on ToolDef hold the evaluable source.
    Starlark,
    /// The tool is remote-hosted by an MCP server.
    Mcp {
        server_id: String,
        remote_name: String,
    },
}

impl Default for ToolBackend {
    fn default() -> Self {
        ToolBackend::Starlark
    }
}

/// A registered tool with its metadata and source.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub params: Vec<ToolParam>,
    pub source_path: PathBuf,
    /// The raw Starlark source code of the tool file.
    pub source: String,
    pub backend: ToolBackend,
}

impl ToolDef {
    /// If this is an MCP-backed tool, return (server_id, remote_name).
    pub fn mcp_backend(&self) -> Option<(String, String)> {
        match &self.backend {
            ToolBackend::Mcp { server_id, remote_name } => {
                Some((server_id.clone(), remote_name.clone()))
            }
            ToolBackend::Starlark => None,
        }
    }
}

impl ToolDef {
    /// Generate a JSON schema suitable for LLM function-calling.
    #[allow(dead_code)]
    pub fn to_json_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for param in &self.params {
            let mut prop = serde_json::Map::new();
            prop.insert(
                "type".to_string(),
                Value::String(param.param_type.clone()),
            );
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

/// Registry of available tools loaded from .star files.
pub struct ToolRegistry {
    tools: HashMap<String, ToolDef>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: ToolDef) {
        self.tools.insert(tool.name.clone(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.tools.get(name)
    }

    pub fn list(&self) -> Vec<&ToolDef> {
        let mut tools: Vec<_> = self.tools.values().collect();
        tools.sort_by_key(|t| &t.name);
        tools
    }

    /// Load all .star files from the given directories and parse tool definitions.
    pub fn load_from_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut registry = Self::new();

        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            let entries = std::fs::read_dir(dir)
                .with_context(|| format!("Failed to read tool directory: {}", dir.display()))?;

            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("star") {
                    match parse_tool_file(&path) {
                        Ok(tool) => {
                            registry.register(tool);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to parse tool file {}: {}",
                                path.display(),
                                e
                            );
                        }
                    }
                }
            }
        }

        Ok(registry)
    }
}

/// Parse a .star tool file to extract the tool name, docstring, and parameters.
///
/// The tool's public name is the filename stem (e.g. `fetch_url.star` →
/// `fetch_url`). The parser scans for the matching `def fetch_url(...)`
/// definition and records its parameters and docstring. Private helpers
/// (whose names start with `_` or don't match the stem) are ignored.
///
/// This is intentionally lightweight — a full Starlark AST walk would be
/// more robust but also more invasive; the name-matches-filename convention
/// keeps tool authorship predictable.
fn parse_tool_file(path: &Path) -> Result<ToolDef> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid tool filename: {}", path.display()))?
        .to_string();

    let expected_prefix = format!("def {}(", stem);

    let mut params = Vec::new();
    let mut description = String::new();
    let mut found = false;

    let lines: Vec<&str> = source.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with(&expected_prefix) {
            continue;
        }
        // Parameters live between the first `(` and the matching `)`. For
        // lightweight parsing we assume they fit on this line, which is the
        // dominant style for tool functions.
        if let Some(paren_start) = trimmed.find('(') {
            if let Some(paren_end) = trimmed.rfind(')') {
                let params_str = &trimmed[paren_start + 1..paren_end];
                params = parse_params(params_str);
            }
        }

        // Docstring: first non-empty line inside the body.
        for next in lines.iter().skip(idx + 1) {
            let t = next.trim();
            if t.is_empty() {
                continue;
            }
            if t.starts_with("\"\"\"") || t.starts_with("'''") {
                let quote = &t[..3];
                if let Some(end) = t[3..].find(quote) {
                    description = t[3..3 + end].to_string();
                } else {
                    description = t[3..].to_string();
                }
            } else if t.starts_with('"') || t.starts_with('\'') {
                let quote = &t[..1];
                if let Some(end) = t[1..].find(quote) {
                    description = t[1..1 + end].to_string();
                }
            }
            break;
        }

        found = true;
        break;
    }

    if !found {
        anyhow::bail!(
            "No `def {}(...)` found in {}. The tool function's name must match the file stem.",
            stem,
            path.display()
        );
    }

    Ok(ToolDef {
        name: stem,
        description,
        params,
        source_path: path.to_path_buf(),
        source,
        backend: ToolBackend::Starlark,
    })
}

/// Parse a simple parameter list like "query, max_results = 5".
fn parse_params(params_str: &str) -> Vec<ToolParam> {
    params_str
        .split(',')
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() {
                return None;
            }

            if let Some((name, default)) = p.split_once('=') {
                let name = name.trim().to_string();
                let default = default.trim();
                Some(ToolParam {
                    name,
                    description: None,
                    param_type: infer_type(default),
                    default: parse_default(default),
                    required: false,
                })
            } else {
                Some(ToolParam {
                    name: p.to_string(),
                    description: None,
                    param_type: "string".to_string(),
                    default: None,
                    required: true,
                })
            }
        })
        .collect()
}

fn infer_type(default_str: &str) -> String {
    if default_str == "True" || default_str == "False" {
        "boolean".to_string()
    } else if default_str.parse::<i64>().is_ok() {
        "integer".to_string()
    } else if default_str.parse::<f64>().is_ok() {
        "number".to_string()
    } else {
        "string".to_string()
    }
}

fn parse_default(default_str: &str) -> Option<Value> {
    if default_str == "None" {
        return Some(Value::Null);
    }
    if default_str == "True" {
        return Some(Value::Bool(true));
    }
    if default_str == "False" {
        return Some(Value::Bool(false));
    }
    if let Ok(i) = default_str.parse::<i64>() {
        return Some(Value::Number(i.into()));
    }
    if let Ok(f) = default_str.parse::<f64>() {
        return Some(serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null));
    }
    // String literal.
    let trimmed = default_str.trim_matches(|c| c == '"' || c == '\'');
    if trimmed != default_str {
        return Some(Value::String(trimmed.to_string()));
    }
    Some(Value::String(default_str.to_string()))
}
