//! MCP server configuration types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Transport protocol for an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// Spawn a local process and communicate over stdin/stdout.
    #[default]
    Stdio,
    /// HTTP POST transport.
    Http,
    /// Server-Sent Events transport.
    Sse,
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Display name for the server (used for tool prefixing).
    #[serde(default)]
    pub name: String,
    /// Command to spawn (for Stdio transport).
    #[serde(default)]
    pub command: String,
    /// Arguments for the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-server tool call timeout (seconds).
    pub tool_timeout_secs: Option<u64>,
    /// Transport protocol.
    #[serde(default)]
    pub transport: McpTransport,
    /// URL for HTTP/SSE transports.
    pub url: Option<String>,
    /// Additional headers for HTTP/SSE transports.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_mcp_stdio() {
        let toml_str = r#"
name = "filesystem"
command = "npx"
args = ["mcp-server-filesystem"]
"#;
        let config: McpServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name, "filesystem");
        assert_eq!(config.command, "npx");
        assert_eq!(config.transport, McpTransport::Stdio);
    }

    #[test]
    fn deserialize_mcp_http() {
        let toml_str = r#"
name = "github"
transport = "http"
url = "https://mcp.github.com/mcp"
tool_timeout_secs = 300

[headers]
Authorization = "Bearer token123"
"#;
        let config: McpServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.transport, McpTransport::Http);
        assert_eq!(config.url.as_deref(), Some("https://mcp.github.com/mcp"));
        assert!(config.headers.contains_key("Authorization"));
        assert_eq!(config.tool_timeout_secs, Some(300));
    }
}
