//! Sub-agent configuration — defines specialized agents for multi-agent orchestration.
//!
//! Each sub-agent has its own system prompt and a restricted tool set,
//! allowing the router (default) agent to delegate tasks to specialists.
//!
//! # Configuration
//!
//! ```toml
//! [[agents]]
//! name = "coder"
//! system_prompt = "You are an expert programmer. Write clean, idiomatic code."
//! tools = ["shell", "file_read", "file_write", "file_edit", "glob_search", "content_search"]
//! max_tool_calls = 30
//! isolation = "worktree"  # optional: "shared" (default) or "worktree"
//!
//! [[agents]]
//! name = "researcher"
//! system_prompt = "You are a research specialist. Find and summarize information."
//! tools = ["web_search", "web_fetch", "http_request", "memory_store", "memory_recall"]
//! max_tool_calls = 20
//! ```

use serde::{Deserialize, Serialize};

/// File system isolation level for a sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentIsolation {
    /// Share workspace_dir with the main agent — no extra isolation.
    Shared,
    /// Use a git worktree for the sub-agent — isolated working directory.
    Worktree,
}

impl Default for AgentIsolation {
    fn default() -> Self {
        Self::Shared
    }
}

/// A sub-agent definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentConfig {
    /// Unique name for this sub-agent (used in agent_delegate tool call).
    pub name: String,

    /// System prompt for this sub-agent.
    pub system_prompt: String,

    /// Tools this sub-agent is allowed to use (whitelist).
    /// If empty, the sub-agent has no tools (text-only).
    #[serde(default)]
    pub tools: Vec<String>,

    /// Hard cap on tool calls per delegation. Defaults to the parent agent's limit.
    #[serde(default)]
    pub max_tool_calls: Option<usize>,

    /// Optional description shown to the router agent in the agent_delegate tool.
    #[serde(default)]
    pub description: Option<String>,

    /// Optional model override — use a specific model instead of the default chat provider.
    /// Useful for routing summarization to cheaper models.
    #[serde(default)]
    pub model: Option<String>,

    /// File system isolation level. Defaults to "shared".
    #[serde(default)]
    pub isolation: AgentIsolation,
}

impl SubAgentConfig {
    /// Returns the description, falling back to system_prompt (truncated).
    pub fn description(&self) -> &str {
        self.description.as_deref().unwrap_or_else(|| {
            // Use system_prompt as fallback; truncate for display
            if self.system_prompt.len() > 80 {
                &self.system_prompt[..80]
            } else {
                &self.system_prompt
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_sub_agent() {
        let toml_str = r#"
        name = "coder"
        system_prompt = "You are a programmer."
        tools = ["shell", "file_read"]
        max_tool_calls = 30
        description = "Writes and edits code"
        "#;
        let config: SubAgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name, "coder");
        assert_eq!(config.tools, vec!["shell", "file_read"]);
        assert_eq!(config.max_tool_calls, Some(30));
    }

    #[test]
    fn deserialize_with_isolation() {
        let toml_str = r#"
        name = "coder"
        system_prompt = "You are a programmer."
        isolation = "worktree"
        "#;
        let config: SubAgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.isolation, AgentIsolation::Worktree);
    }

    #[test]
    fn default_isolation_is_shared() {
        let config = SubAgentConfig {
            name: "test".to_string(),
            system_prompt: String::new(),
            tools: vec![],
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::default(),
        };
        assert_eq!(config.isolation, AgentIsolation::Shared);
    }
}
