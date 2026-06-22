//! MCP server configuration. JSON format matches the Claude Desktop
//! `mcp_servers` shape so users can lift their existing config:
//!
//! ```json
//! {
//!   "servers": {
//!     "fs": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
//!       "env": {}
//!     },
//!     "asana": {
//!       "transport": "http",
//!       "url": "https://mcp.asana.com/sse",
//!       "authToken": "__CHIDORI_SECRET__<id>__"
//!     }
//!   }
//! }
//! ```
//!
//! A server is either **stdio** (a `command` to spawn, the default) or **http**
//! (a remote Streamable-HTTP server at `url`). The `transport` discriminant
//! defaults to `stdio` with `#[serde(default)]` so every pre-existing stdio
//! config — and every existing consumer of this struct — keeps parsing
//! unchanged. For http servers `authToken` is a **bearer placeholder**
//! (`__CHIDORI_SECRET__<id>__`), never a raw token: the real value rides
//! `CHIDORI_SECRET_ENV` and is substituted host-side, audience-locked to the
//! server's host. See app-agent-builder docs/design/mcp-http-transport-chidori.md.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Which wire transport an MCP server speaks. Defaults to `stdio` for
/// back-compat with every config written before HTTP support landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// stdio transport: the executable to spawn. Empty for http servers.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// stdio (default) or http.
    #[serde(default)]
    pub transport: McpTransport,
    /// http transport: the canonical server URI requests are POSTed to.
    #[serde(default)]
    pub url: Option<String>,
    /// http transport: a `__CHIDORI_SECRET__<id>__` bearer placeholder. The
    /// secret broker substitutes the real token host-side; it is never a raw
    /// token and never reaches the guest.
    #[serde(default)]
    pub auth_token: Option<String>,
}

impl McpServerConfig {
    /// Validate transport-specific required fields. Returns a human-readable
    /// reason when the config can't be started, so `start_from_config` can skip
    /// it with a clear warning rather than panicking or spawning nonsense.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self.transport {
            McpTransport::Stdio => {
                if self.command.trim().is_empty() {
                    return Err("stdio server requires a non-empty `command`".to_string());
                }
            }
            McpTransport::Http => match self.url.as_deref() {
                None | Some("") => {
                    return Err("http server requires a `url`".to_string());
                }
                Some(_) => {}
            },
        }
        Ok(())
    }
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServersConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

impl McpServersConfig {
    pub fn load_from_env() -> Result<Self> {
        let Ok(path) = std::env::var("CHIDORI_MCP_CONFIG") else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading CHIDORI_MCP_CONFIG at {}", path))?;
        let cfg: McpServersConfig = serde_json::from_str(&text)
            .with_context(|| format!("parsing MCP config at {}", path))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_stdio_config_parses_unchanged() {
        let cfg: McpServerConfig = serde_json::from_str(
            r#"{"command":"npx","args":["-y","srv"],"env":{},"enabled":true}"#,
        )
        .unwrap();
        assert_eq!(cfg.transport, McpTransport::Stdio);
        assert_eq!(cfg.command, "npx");
        assert!(cfg.url.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn http_config_parses_with_placeholder() {
        let cfg: McpServerConfig = serde_json::from_str(
            r#"{"transport":"http","url":"https://mcp.asana.com/sse","authToken":"__CHIDORI_SECRET__x__"}"#,
        )
        .unwrap();
        assert_eq!(cfg.transport, McpTransport::Http);
        assert_eq!(cfg.url.as_deref(), Some("https://mcp.asana.com/sse"));
        assert_eq!(cfg.auth_token.as_deref(), Some("__CHIDORI_SECRET__x__"));
        assert!(cfg.command.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn http_without_url_is_invalid() {
        let cfg: McpServerConfig =
            serde_json::from_str(r#"{"transport":"http"}"#).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn stdio_without_command_is_invalid() {
        let cfg: McpServerConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(cfg.validate().is_err());
    }
}
