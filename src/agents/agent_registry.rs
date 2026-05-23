//! AgentRegistry — name → SubAgentConfig lookup.
//!
//! RFC v2 §三.A: replaces the ad-hoc `Arc<RwLock<Vec<SubAgentConfig>>>` with
//! a dedicated type that offers O(1) lookup and a single `reload_from_dir`
//! entry point. WorkspaceWatcher (D25) calls reload_from_dir on
//! `agents/` directory changes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::config::sub_agent::SubAgentConfig;

/// Thread-safe registry of agent definitions loaded from
/// `workspace/agents/<name>/AGENT.md`.
///
/// The HashMap is wrapped in an inner Arc<RwLock<...>> so multiple consumers
/// (DelegationCoordinator, ResourceProvider, Agent factory) can share the
/// same live view and pick up reloads without re-cloning.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<RwLock<HashMap<String, SubAgentConfig>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build a registry pre-populated with the given configs.
    pub fn from_vec(configs: Vec<SubAgentConfig>) -> Self {
        let map: HashMap<_, _> = configs
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();
        Self {
            inner: Arc::new(RwLock::new(map)),
        }
    }

    /// Look up an agent by name.
    pub fn get(&self, name: &str) -> Option<SubAgentConfig> {
        self.inner.read().get(name).cloned()
    }

    /// True if an agent with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.inner.read().contains_key(name)
    }

    /// All registered agent configs as a cloned snapshot.
    pub fn values_cloned(&self) -> Vec<SubAgentConfig> {
        self.inner.read().values().cloned().collect()
    }

    /// All registered agent names (sorted for stable test output).
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.read().keys().cloned().collect();
        names.sort();
        names
    }

    /// Number of registered agents.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Replace the entire contents atomically (used by `reload_from_dir`).
    pub fn replace_all(&self, configs: Vec<SubAgentConfig>) {
        let mut map = self.inner.write();
        map.clear();
        for c in configs {
            map.insert(c.name.clone(), c);
        }
    }

    /// Reload from `agents/` directory. Returns the count.
    /// Errors during load are logged; registry is left untouched in that case.
    pub fn reload_from_dir(&self, agents_dir: &Path) -> usize {
        let configs = crate::agents::workspace::agent_loader::load_agents_from_dir(agents_dir);
        let count = configs.len();
        self.replace_all(configs);
        count
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sub_agent::SubAgentConfig;

    fn dummy(name: &str) -> SubAgentConfig {
        SubAgentConfig {
            name: name.to_string(),
            description: None,
            system_prompt: String::new(),
            tools: Vec::new(),
            skills: Default::default(),
            mcp: Default::default(),
            model: None,
            max_tool_calls: None,
            isolation: Default::default(),
        }
    }

    #[test]
    fn from_vec_indexes_by_name() {
        let r = AgentRegistry::from_vec(vec![dummy("coder"), dummy("reviewer")]);
        assert!(r.contains("coder"));
        assert!(r.contains("reviewer"));
        assert!(!r.contains("missing"));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn replace_all_overwrites() {
        let r = AgentRegistry::from_vec(vec![dummy("a"), dummy("b")]);
        r.replace_all(vec![dummy("c")]);
        assert_eq!(r.names(), vec!["c"]);
    }
}
