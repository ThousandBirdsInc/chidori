//! Remote MCP client over **Streamable HTTP** (the MCP spec's HTTP transport,
//! protocol 2025-03-26). Mirrors the stdio [`super::client::McpClient`] surface
//! — `tools()` + `call_tool()` — but speaks JSON-RPC 2.0 over HTTP POST instead
//! of a child's stdin/stdout. Only the subset the framework uses is
//! implemented: `initialize` / `notifications/initialized` / `tools/list` /
//! `tools/call`.
//!
//! Two cross-repo contracts make this safe (mcp-http-transport-chidori.md §3):
//!
//! - **The bearer is a placeholder, not a token.** `auth_token` is a
//!   `__CHIDORI_SECRET__<id>__` value; this client substitutes the real token
//!   through the host secret broker ([`crate::runtime::secret_env`]), locked to
//!   the server's own host, so a token is never sent to any other host and the
//!   guest never sees it. (Highest-risk integration point per the design doc —
//!   substitution happens in [`McpHttpClient::connect`].)
//! - **Scope challenges surface as structured errors.** A `401` / `403` /
//!   `WWW-Authenticate` becomes a `{ mcpError: { kind, server, … } }` JSON error
//!   (not an opaque string) so agent-builder's broker can trigger step-up
//!   re-auth or report the missing scope during generation.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::client::RemoteTool;
use super::config::McpServerConfig;

/// Pinned protocol version for the HTTP transport. Newer than the stdio
/// client's `2024-11-05`; 2025-03-26 is the revision that introduced Streamable
/// HTTP. (Open question 4 in the design doc — negotiation can come later.)
const PROTOCOL_VERSION: &str = "2025-03-26";

pub struct McpHttpClient {
    client: reqwest::Client,
    url: String,
    /// The server id, used only to label structured errors.
    server_id: String,
    /// Realized `Authorization` header value (`Bearer <real-token>`), already
    /// run through the secret broker. `None` when the server needs no auth.
    auth_header: Option<String>,
    /// Streamable-HTTP session id returned by `initialize`, echoed on every
    /// later request via `Mcp-Session-Id`.
    session_id: tokio::sync::Mutex<Option<String>>,
    tools: Vec<RemoteTool>,
}

impl McpHttpClient {
    /// Connect to a remote HTTP MCP server: substitute the bearer placeholder
    /// host-side, run the JSON-RPC handshake, and discover its tools.
    pub async fn connect(server_id: &str, cfg: McpServerConfig) -> Result<Self> {
        let url = cfg
            .url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| anyhow!("http MCP server `{server_id}` has no url"))?;

        // Resolve the bearer placeholder → real token through the host secret
        // broker, audience-locked to the server's own host. A placeholder bound
        // to a different host fails closed here (never sent), satisfying the
        // spec's "MUST NOT send tokens to any server other than the one that
        // issued them."
        let auth_header = match cfg.auth_token.as_deref().filter(|t| !t.is_empty()) {
            Some(token) => {
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .ok_or_else(|| anyhow!("http MCP server `{server_id}` has an unparseable url"))?;
                let store = crate::runtime::secret_env::SecretStore::global();
                let header = store
                    .substitute_str(&format!("Bearer {token}"), &host)
                    .map_err(|err| anyhow!("MCP `{server_id}` auth: {err}"))?;
                Some(header)
            }
            None => None,
        };

        let client = reqwest::Client::builder()
            .build()
            .context("building MCP http client")?;

        let mut this = Self {
            client,
            url,
            server_id: server_id.to_string(),
            auth_header,
            session_id: tokio::sync::Mutex::new(None),
            tools: Vec::new(),
        };

        // Handshake.
        this.post_rpc(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "chidori", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await?;
        this.post_notification("notifications/initialized", Value::Null)
            .await?;

        // Discover tools.
        let list = this.post_rpc("tools/list", json!({})).await?;
        this.tools = list
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();

        Ok(this)
    }

    pub fn tools(&self) -> &[RemoteTool] {
        &self.tools
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        let resp = self
            .post_rpc(
                "tools/call",
                json!({ "name": name, "arguments": args }),
            )
            .await?;
        // Collapse `{ content: [{type, text}, ...] }` to a single value for
        // agent ergonomics — identical to the stdio client.
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

    /// Apply the standard headers (auth, accept, session) to a request builder.
    fn apply_headers(&self, mut req: reqwest::RequestBuilder, session: &Option<String>) -> reqwest::RequestBuilder {
        // Streamable HTTP requires the client to accept both a single JSON
        // response and an SSE stream.
        req = req.header(reqwest::header::ACCEPT, "application/json, text/event-stream");
        if let Some(auth) = &self.auth_header {
            req = req.header(reqwest::header::AUTHORIZATION, auth);
        }
        if let Some(sid) = session {
            req = req.header("Mcp-Session-Id", sid);
        }
        req
    }

    /// POST a JSON-RPC request and return the matching response object (the
    /// whole `{ jsonrpc, id, result|error }`). Surfaces `401`/`403` as a
    /// structured `mcpError`; a JSON-RPC `error` member becomes a normal error.
    async fn post_rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = 1;
        let payload = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let session = self.session_id.lock().await.clone();
        let req = self.apply_headers(
            self.client.post(&self.url).json(&payload),
            &session,
        );

        let resp = tokio::time::timeout(std::time::Duration::from_secs(30), req.send())
            .await
            .map_err(|_| anyhow!("MCP `{}` to `{}` timed out", method, self.server_id))?
            .map_err(|err| self.transport_error(method, &err.to_string()))?;

        let status = resp.status();
        // Capture / refresh the session id the server assigns at initialize.
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        {
            *self.session_id.lock().await = Some(sid);
        }

        if status.as_u16() == 401 || status.as_u16() == 403 {
            let www = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            return Err(self.scope_error(status.as_u16(), www));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|err| self.transport_error(method, &err.to_string()))?;

        let response = if content_type.contains("text/event-stream") {
            parse_sse_response(&text, id)
                .ok_or_else(|| self.transport_error(method, "no JSON-RPC response in SSE stream"))?
        } else if text.trim().is_empty() {
            // A notification-only POST can legitimately return 202 with no body.
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        } else {
            serde_json::from_str::<Value>(&text)
                .map_err(|err| self.transport_error(method, &format!("invalid JSON-RPC body: {err}")))?
        };

        if let Some(err) = response.get("error") {
            return Err(anyhow!("MCP `{}` error: {}", method, err));
        }
        Ok(response)
    }

    /// Fire-and-forget notification (no response correlation needed).
    async fn post_notification(&self, method: &str, params: Value) -> Result<()> {
        let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let session = self.session_id.lock().await.clone();
        let req = self.apply_headers(
            self.client.post(&self.url).json(&payload),
            &session,
        );
        // Best-effort: a notification failure shouldn't abort the handshake.
        let _ = req.send().await;
        Ok(())
    }

    /// Structured `401`/`403` scope challenge (mcp-http-transport-chidori.md
    /// §3.3), serialized as the error message so it rides the existing
    /// tool-error channel and agent-builder can parse it.
    fn scope_error(&self, status: u16, www_authenticate: Option<String>) -> anyhow::Error {
        let kind = if status == 403 {
            "insufficient_scope"
        } else {
            "unauthorized"
        };
        let required_scopes = www_authenticate
            .as_deref()
            .and_then(parse_required_scopes)
            .unwrap_or_default();
        let value = json!({ "mcpError": {
            "kind": kind,
            "server": self.server_id,
            "wwwAuthenticate": www_authenticate,
            "requiredScopes": required_scopes,
        }});
        anyhow!(value.to_string())
    }

    fn transport_error(&self, method: &str, message: &str) -> anyhow::Error {
        let value = json!({ "mcpError": {
            "kind": "transport",
            "server": self.server_id,
            "message": format!("`{method}`: {message}"),
        }});
        anyhow!(value.to_string())
    }
}

/// Pull `scope="a b c"` out of a `WWW-Authenticate` challenge, if present.
fn parse_required_scopes(www_authenticate: &str) -> Option<Vec<String>> {
    let idx = www_authenticate.find("scope=")?;
    let rest = &www_authenticate[idx + "scope=".len()..];
    let rest = rest.trim_start_matches('"');
    let end = rest.find('"').unwrap_or(rest.len());
    let scopes: Vec<String> = rest[..end]
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

/// Find the JSON-RPC response with `id == want_id` in an SSE stream body. Each
/// event's `data:` payload is a JSON-RPC message; we pick the one correlating
/// to our request (servers may interleave notifications before the response).
fn parse_sse_response(body: &str, want_id: i64) -> Option<Value> {
    let mut last_with_result: Option<Value> = None;
    for line in body.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if value.get("id").and_then(Value::as_i64) == Some(want_id) {
            return Some(value);
        }
        if value.get("result").is_some() || value.get("error").is_some() {
            last_with_result = Some(value);
        }
    }
    last_with_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scopes_from_www_authenticate() {
        let www = r#"Bearer error="insufficient_scope", scope="tasks:read tasks:write""#;
        assert_eq!(
            parse_required_scopes(www),
            Some(vec!["tasks:read".to_string(), "tasks:write".to_string()])
        );
    }

    #[test]
    fn no_scope_clause_yields_none() {
        assert_eq!(parse_required_scopes(r#"Bearer realm="x""#), None);
    }

    #[test]
    fn sse_picks_matching_id() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let v = parse_sse_response(body, 1).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    /// A minimal JSON-RPC-over-HTTP MCP server: one request per connection
    /// (`Connection: close`, so reqwest reconnects), routed by JSON-RPC method.
    /// Replies to initialize / tools/list / tools/call; ignores notifications.
    fn spawn_mock_mcp_server() -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
                let result = match method {
                    "initialize" => json!({"protocolVersion": PROTOCOL_VERSION, "serverInfo": {"name": "mock"}}),
                    "tools/list" => json!({"tools": [{
                        "name": "echo",
                        "description": "echoes",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
                    }]}),
                    "tools/call" => json!({"content": [{"type": "text", "text": "echoed!"}]}),
                    _ => {
                        // Notification (no id): 202 with no body.
                        let resp = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes());
                        continue;
                    }
                };
                let payload = json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: sess-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(), payload,
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn connect_discovers_tools_and_calls_them() {
        let addr = spawn_mock_mcp_server();
        let cfg = McpServerConfig {
            command: String::new(),
            args: vec![],
            env: Default::default(),
            enabled: true,
            transport: super::super::config::McpTransport::Http,
            url: Some(format!("http://{addr}/mcp")),
            auth_token: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = McpHttpClient::connect("mock", cfg).await.unwrap();
            assert_eq!(client.tools().len(), 1);
            assert_eq!(client.tools()[0].name, "echo");
            let out = client
                .call_tool("echo", &json!({"text": "hi"}))
                .await
                .unwrap();
            // Single text content block collapses to a string.
            assert_eq!(out, Value::String("echoed!".to_string()));
        });
    }
}
