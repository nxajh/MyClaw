//! mcp — MCP (Model Context Protocol) client infrastructure.
//!
//! Provides protocol types, transport implementations, and tool wrappers for
//! connecting to external MCP servers.
//!
//! ## Architecture
//!
//! ```text
//! application/runtime
//!         |
//!         v
//! infrastructure/mcp   ← MCP client + protocol + transports
//!         |
//!         +-- protocol.rs   : JSON-RPC 2.0 types
//!         +-- transport.rs : Stdio, HTTP, SSE transports
//!         +-- client.rs     : McpServer + McpRegistry
//!         +-- tool.rs       : McpToolWrapper (wraps MCP tool as Tool)
//!         +-- deferred.rs   : Deferred MCP tool loading stubs
//! ```
//!
//! ## Public API
//!
//! - [`protocol`] — [`JsonRpcRequest`], [`JsonRpcResponse`], [`McpToolDef`], [`MCP_PROTOCOL_VERSION`]
//! - [`transport`] — [`McpTransportConn`], [`create_transport`], [`StdioTransport`], [`HttpTransport`]
//! - [`client`] — [`McpServer`], [`McpRegistry`]
//! - [`tool`] — [`McpToolWrapper`]
//! - [`deferred`] — [`DeferredMcpToolStub`], [`ActivatedToolSet`]

pub mod client;
pub mod config_types;
pub mod deferred;
pub mod protocol;
pub mod tool;
pub mod tool_trait;

/// Explicitly use transport.rs (not transport/mod.rs) to avoid the
/// "file for module `transport` found at both" error.
#[path = "transport.rs"]
pub mod transport;

// Re-exports for convenience
pub use client::{McpRegistry, McpServer};
pub use config_types::{McpServerConfig, McpTransport};
pub use deferred::{ActivatedToolSet, DeferredMcpToolStub};
pub use protocol::{JsonRpcRequest, JsonRpcResponse, McpToolDef};
pub use tool::McpToolWrapper;
pub use tool_trait::{Tool, ToolResult, ToolSpec};
pub use transport::create_transport;
pub use transport::McpTransportConn;