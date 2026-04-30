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
//!     }
//!   }
//! }
//! ```

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
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
