//! Tool execution trait — core domain concept.
//!
//! Defines the interface for all agent-callable tools. This trait belongs in the
//! Domain Layer per DDD: it is a pure behavioural contract with no infrastructure
//! dependencies.
//!
//! Infrastructure Layer (infrastructure/tools, infrastructure/mcp) provides
//! concrete implementations.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Result of executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the tool executed successfully.
    pub success: bool,
    /// Tool output (text or JSON string).
    pub output: String,
    /// Error message if success is false.
    pub error: Option<String>,
}

/// Specification for a tool (used in system prompts and registries).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name (unique identifier).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
}

/// Trait for agent-callable tools.
///
/// Implementations live in:
/// - `infrastructure/tools/` — built-in tools (shell, file_ops, web, etc.)
/// - `infrastructure/mcp/` — MCP tool wrappers
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (unique identifier).
    fn name(&self) -> &str;

    /// Human-readable description.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult>;

    /// Build a [`ToolSpec`] from this tool's metadata.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }
}
