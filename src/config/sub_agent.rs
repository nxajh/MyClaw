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
//! tools = ["search_web", "http_request", "memory_search", "memory_view"]
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
///
/// RFC v2 §三.B: an agent's config is the union of its AGENT.md front-matter
/// fields plus the body (`system_prompt`). The three filters (`tools` /
/// `skills` / `mcp`) decide which globally-registered capabilities this agent
/// is allowed to see; `Agent.run` snapshots the filtered ToolRegistry at
/// turn start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentConfig {
    /// Unique name for this sub-agent (used in agent_delegate tool call).
    pub name: String,

    /// System prompt for this sub-agent (body of AGENT.md).
    pub system_prompt: String,

    /// Tools this sub-agent is allowed to use. RFC v2 §三.A:
    /// `ToolFilter` (`[all]` / explicit allow-list /
    /// `{ except: [...] }` deny-list). Default = `[all]`.
    #[serde(default)]
    pub tools: crate::config::filters::ToolFilter,

    /// Skills this sub-agent may see in system reminders / load via skill_view.
    /// RFC v2 §三.B: NameFilter form `[all]` / `[skill_a, skill_b]` /
    /// `{ except: [...] }`. Default `all`.
    #[serde(default)]
    pub skills: crate::config::filters::SkillFilter,

    /// MCP server names whose tools are exposed to this sub-agent.
    /// Default `all`. Per-tool filtering still applies via `tools`.
    #[serde(default)]
    pub mcp: crate::config::filters::McpFilter,

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
    /// True if `tool_name` is allowed by this agent's tool filter.
    pub fn allows_tool(&self, tool_name: &str) -> bool {
        self.tools.allows(tool_name)
    }

    /// True if `skill_name` is allowed by this agent's skill filter.
    pub fn allows_skill(&self, skill_name: &str) -> bool {
        self.skills.allows(skill_name)
    }

    /// True if MCP `server_name`'s tools are allowed by this agent.
    pub fn allows_mcp(&self, server_name: &str) -> bool {
        self.mcp.allows(server_name)
    }
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
        assert!(config.tools.allows("shell"));
        assert!(config.tools.allows("file_read"));
        assert!(!config.tools.allows("file_write"));
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
            tools: Default::default(),
            skills: Default::default(),
            mcp: Default::default(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::default(),
        };
        assert_eq!(config.isolation, AgentIsolation::Shared);
    }

    #[test]
    fn allows_skill_default_is_all() {
        let config = SubAgentConfig {
            name: "test".to_string(),
            system_prompt: String::new(),
            tools: Default::default(),
            skills: Default::default(),
            mcp: Default::default(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::default(),
        };
        assert!(config.allows_skill("anything"));
        assert!(config.allows_mcp("anything"));
    }

    #[test]
    fn allows_tool_whitelist() {
        let config = SubAgentConfig {
            name: "t".to_string(),
            system_prompt: String::new(),
            tools: crate::config::filters::ToolFilter::Allow(vec![
                "shell".to_string(),
                "file_read".to_string(),
            ]),
            skills: Default::default(),
            mcp: Default::default(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::default(),
        };
        assert!(config.allows_tool("shell"));
        assert!(config.allows_tool("file_read"));
        assert!(!config.allows_tool("file_write"));
    }
}
