//! Filter enums for agent capability scoping.
//!
//! AgentConfig uses three independent filters (`tools` / `skills` / `mcp`) to
//! decide which entries from the global registries are exposed to this agent.
//! Each filter is a `ToolFilter` / `SkillFilter` / `McpFilter` value with the
//! same shape: All / Allow(list) / Deny(list).
//!
//! Resolution: at turn start, Agent.run() iterates the global ToolRegistry,
//! checks each tool's `source()`, and applies the matching filter. The
//! resulting subset is passed to LLM as tool_specs and to ToolExecutor as the
//! lookup pool.

use serde::{Deserialize, Serialize};

/// Generic name-list filter shared by tools / skills / MCP servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NameFilter {
    /// `tools: [all]` or omitted entirely → all allowed.
    AllKeyword(AllKeyword),
    /// `tools: [shell, file_read]` → allow-list.
    Allow(Vec<String>),
    /// `tools: { except: [destructive_op] }` → deny-list.
    Deny(DenyList),
}

/// Marker for the literal `[all]` YAML form.
///
/// AGENT.md writes `tools: [all]` — we parse the first element "all" specially.
/// In code, prefer `NameFilter::all()` constructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllKeyword(pub Vec<String>);

/// Wraps the `except` key for deny semantics in YAML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyList {
    pub except: Vec<String>,
}

impl NameFilter {
    /// Construct an "allow all" filter (matches everything).
    pub fn all() -> Self {
        Self::AllKeyword(AllKeyword(vec!["all".to_string()]))
    }

    /// True if `name` is allowed by this filter.
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::AllKeyword(AllKeyword(list)) => {
                list.iter().any(|n| n == "all") || list.iter().any(|n| n == name)
            }
            Self::Allow(list) => list.iter().any(|n| n == name),
            Self::Deny(DenyList { except }) => except.iter().all(|n| n != name),
        }
    }

    /// True if this filter is the "all" pseudo-value.
    pub fn is_all(&self) -> bool {
        matches!(self, Self::AllKeyword(AllKeyword(list)) if list.iter().any(|n| n == "all"))
    }
}

impl Default for NameFilter {
    fn default() -> Self {
        Self::all()
    }
}

/// Alias for clarity at use sites.
pub type ToolFilter = NameFilter;
/// Alias for clarity at use sites.
pub type SkillFilter = NameFilter;
/// Alias for clarity at use sites.
pub type McpFilter = NameFilter;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_matches_anything() {
        let f = NameFilter::all();
        assert!(f.allows("shell"));
        assert!(f.allows("file_read"));
        assert!(f.is_all());
    }

    #[test]
    fn allow_list_matches_only_listed() {
        let f = NameFilter::Allow(vec!["shell".into(), "file_read".into()]);
        assert!(f.allows("shell"));
        assert!(f.allows("file_read"));
        assert!(!f.allows("file_write"));
        assert!(!f.is_all());
    }

    #[test]
    fn deny_list_matches_everything_not_listed() {
        let f = NameFilter::Deny(DenyList {
            except: vec!["destructive_op".into()],
        });
        assert!(f.allows("shell"));
        assert!(!f.allows("destructive_op"));
    }
}
