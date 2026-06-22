//! MCP (Model Context Protocol) client integration.
//!
//! Spawns MCP servers as child processes, speaks JSON-RPC 2.0 over stdio,
//! discovers their tools at startup, and exposes them through the framework's
//! ToolRegistry so agents can invoke them via `tool("name", ...)` or expose
//! them to the LLM via `prompt(tools=[...])`.
//!
//! Wire protocol is hand-rolled rather than pulling in a full MCP SDK: the
//! subset we need (initialize / tools/list / tools/call) is small and the
//! SDK surface would add a large dependency.

pub mod client;
pub mod config;
pub mod http_client;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::Mutex;

pub use client::{McpClient, RemoteTool};
pub use config::{McpServersConfig, McpTransport};
pub use http_client::McpHttpClient;

use crate::tools::{ToolDef, ToolParam};

/// One connected MCP server, behind a single interface regardless of transport.
/// An stdio server is a spawned child; an http server is a remote endpoint. The
/// `<server_id>__<tool>` naming, `ToolBackend::Mcp` dispatch, and the catalog
/// contract are identical for both, so engine/bindings/scheduler are untouched.
pub enum McpClientHandle {
    Stdio(McpClient),
    Http(McpHttpClient),
}

impl McpClientHandle {
    pub fn tools(&self) -> &[RemoteTool] {
        match self {
            McpClientHandle::Stdio(c) => c.tools(),
            McpClientHandle::Http(c) => c.tools(),
        }
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match self {
            McpClientHandle::Stdio(c) => c.call_tool(name, args).await,
            McpClientHandle::Http(c) => c.call_tool(name, args).await,
        }
    }
}

/// Runtime manager for all connected MCP servers. Shared across all agent
/// runs via `HostState.mcp`. Calls are dispatched by `server_id`.
pub struct McpManager {
    servers: Mutex<HashMap<String, Arc<McpClientHandle>>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
        }
    }

    /// Load MCP server config and start every enabled server. Returns the
    /// combined list of ToolDefs — one per remote tool — so the caller can
    /// merge them into the ToolRegistry.
    pub async fn start_from_config(&self, cfg: &McpServersConfig) -> Result<Vec<ToolDef>> {
        let mut defs = Vec::new();
        for (id, server) in &cfg.servers {
            if !server.enabled {
                continue;
            }
            if let Err(reason) = server.validate() {
                tracing::warn!("MCP server `{}` skipped: {}", id, reason);
                continue;
            }
            // stdio spawns a child; http connects to a remote endpoint. Both
            // yield the same `McpClientHandle` and the same ToolDefs.
            let handle = match server.transport {
                McpTransport::Stdio => McpClient::spawn(server.clone()).await.map(McpClientHandle::Stdio),
                McpTransport::Http => McpHttpClient::connect(id, server.clone())
                    .await
                    .map(McpClientHandle::Http),
            };
            match handle {
                Ok(handle) => {
                    let handle = Arc::new(handle);
                    for remote in handle.tools().iter() {
                        defs.push(tool_def_for(id, remote));
                    }
                    self.servers.lock().await.insert(id.clone(), handle);
                }
                Err(e) => {
                    tracing::warn!("MCP server `{}` failed to start: {}", id, e);
                }
            }
        }
        Ok(defs)
    }

    /// Invoke `tools/call` on a previously registered server.
    pub async fn call_tool(
        &self,
        server_id: &str,
        remote_name: &str,
        args: &Value,
    ) -> Result<Value> {
        let client = {
            let map = self.servers.lock().await;
            map.get(server_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("MCP server `{}` is not connected", server_id))?
        };
        client.call_tool(remote_name, args).await
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `ToolDef` for one remote tool. Transport-agnostic — an http tool
/// is exposed exactly like an stdio one (`<server_id>__<tool>`, backend
/// `ToolBackend::Mcp`), so the rest of the runtime never learns the difference.
fn tool_def_for(server_id: &str, remote: &RemoteTool) -> ToolDef {
    let params: Vec<ToolParam> = remote
        .input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|props| {
            let required: Vec<&str> = remote
                .input_schema
                .get("required")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            props
                .iter()
                .map(|(name, schema)| ToolParam {
                    name: name.clone(),
                    description: schema
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    param_type: schema
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("string")
                        .to_string(),
                    default: None,
                    required: required.contains(&name.as_str()),
                })
                .collect()
        })
        .unwrap_or_default();
    ToolDef {
        name: format!("{}__{}", server_id, remote.name),
        description: remote.description.clone().unwrap_or_default(),
        params,
        source_path: std::path::PathBuf::new(),
        source_fingerprint: None,
        backend: crate::tools::ToolBackend::Mcp {
            server_id: server_id.to_string(),
            remote_name: remote.name.clone(),
        },
    }
}
