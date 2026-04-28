//! Local definitions for MCP server configuration types.
//!
//! Replaces the external `zeroclaw_config::schema` dependency.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Transport protocol for an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    pub name: String,
    /// Command to spawn (for Stdio transport).
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
