//! Tool execution trait — core domain concept.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agents::session::Session;

/// Origin of a registered tool — used by `AgentConfig.tools` / `.skills` /
/// `.mcp` filters to decide which tools an agent may see.
///
/// RFC v2 §三.B: each tool reports its source via `Tool::source()`, and
/// `Agent.run` filters the global ToolRegistry through the agent's three
/// filters before passing tool_specs to the LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// Hard-coded tools (shell, file_read, memory_*, etc.).
    Builtin,
    /// Loaded from `workspace/skills/<name>/SKILL.md`.
    Skill { name: String },
    /// Loaded from an MCP server (`mcp_servers` config).
    Mcp { server: String },
}

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
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (unique identifier).
    fn name(&self) -> &str;

    /// Human-readable description.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Origin of this tool. Default `Builtin` covers hard-coded tools;
    /// skills / MCP wrappers must override.
    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    /// Maximum output tokens before framework-level truncation kicks in.
    /// Override per-tool for tighter/looser limits. Default: 10,000.
    fn max_output_tokens(&self) -> usize {
        10_000
    }

    /// Execute the tool with the given arguments and session context.
    async fn execute(&self, args: serde_json::Value, _session: &Session) -> anyhow::Result<ToolResult>;

    /// Build a [`ToolSpec`] from this tool's metadata.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }
}
