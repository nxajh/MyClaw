//! mcp — MCP (Model Context Protocol) client.

pub mod client;
pub mod config_types;
pub mod deferred;
pub mod protocol;
pub mod tool;
pub mod tool_trait;

// `transport.rs` is a single file (not a directory)
mod transport;

pub use client::{McpRegistry, McpServer};
pub use config_types::{McpServerConfig, McpTransport};
pub use deferred::{ActivatedToolSet, DeferredMcpToolStub};
pub use protocol::{JsonRpcRequest, JsonRpcResponse, McpToolDef};
pub use tool::McpToolWrapper;
pub use tool_trait::{Tool, ToolResult, ToolSpec};
pub use transport::{create_transport, McpTransportConn};
