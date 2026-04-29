//! Minimal stdio MCP client. Speaks JSON-RPC 2.0 over the child process's
//! stdin/stdout. Implements only what the framework actually uses:
//!
//!   initialize        — handshake + server metadata
//!   tools/list        — discover tools at startup
//!   tools/call        — invoke a tool on behalf of an agent
//!
//! Each request is written as a single line of JSON terminated by \n; the
//! server's responses are read the same way. This matches the line-delimited
//! framing used by the reference MCP servers. A single background reader task
//! dispatches responses to in-flight requests by their integer id.

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

use super::config::McpServerConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_schema", rename = "inputSchema")]
    pub input_schema: Value,
}

fn default_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

pub struct McpClient {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Arc<Mutex<std::collections::HashMap<i64, oneshot::Sender<Value>>>>,
    tools: Vec<RemoteTool>,
    /// Holding the child keeps the process alive for the life of the client.
    _child: Mutex<Child>,
}

impl McpClient {
    pub async fn spawn(cfg: McpServerConfig) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{}`", cfg.command))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("child stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout unavailable"))?;

        let pending: Arc<Mutex<std::collections::HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Reader task: parse one JSON object per line, dispatch by id.
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                    tracing::warn!("MCP: dropping non-JSON line: {}", trimmed);
                    continue;
                };
                if let Some(id) = val.get("id").and_then(|v| v.as_i64()) {
                    if let Some(tx) = reader_pending.lock().await.remove(&id) {
                        let _ = tx.send(val);
                    }
                } else {
                    // Notification / log / progress — ignore for now.
                }
            }
        });

        let client = Self {
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending,
            tools: Vec::new(),
            _child: Mutex::new(child),
        };

        // Handshake.
        let _init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "app-agent", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        client
            .notification("notifications/initialized", Value::Null)
            .await?;

        // Discover tools.
        let list = client.request("tools/list", json!({})).await?;
        let tools: Vec<RemoteTool> = list
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();

        Ok(Self { tools, ..client })
    }

    pub fn tools(&self) -> &[RemoteTool] {
        &self.tools
    }

    /// Send a request/response exchange. Blocks until the server replies.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&payload)?;
        line.push('\n');
        self.stdin.lock().await.write_all(line.as_bytes()).await?;

        let response = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("MCP `{}` timed out", method))??;
        if let Some(err) = response.get("error") {
            return Err(anyhow!("MCP `{}` error: {}", method, err));
        }
        Ok(response)
    }

    /// Fire-and-forget notification (no response expected).
    async fn notification(&self, method: &str, params: Value) -> Result<()> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&payload)?;
        line.push('\n');
        self.stdin.lock().await.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        let resp = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": args,
                }),
            )
            .await?;
        // MCP returns `{ content: [{type, text, ...}, ...] }`; collapse it to
        // a single JSON value for agent ergonomics. If there's a single text
        // block, return it as a string; otherwise return the raw content array.
        let content = resp
            .get("result")
            .and_then(|r| r.get("content"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Value::Array(ref items) = content {
            if items.len() == 1 {
                if let Some(text) = items[0].get("text").and_then(|v| v.as_str()) {
                    return Ok(Value::String(text.to_string()));
                }
            }
        }
        Ok(content)
    }
}
