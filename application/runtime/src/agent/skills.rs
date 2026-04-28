//! Skills manager — registers tools and provides them to AgentLoop.
//!
//! A Skill is a named group of tools. The manager maintains a flat
//! tool-name → tool registry for fast lookup.
//!
//! DDD: SkillsManager depends on `myclaw_capability::tool::Tool` (Domain trait),
//! not on any Infrastructure concrete type.

use std::collections::HashMap;
use std::sync::Arc;

use myclaw_capability::tool::Tool;

/// A named collection of tools.
#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Tools belonging to this skill.
    pub tools: Vec<Arc<dyn Tool>>,
}

/// SkillsManager maintains the global tool registry.
pub struct SkillsManager {
    /// skill_name → Skill
    skills: HashMap<String, Skill>,
    /// tool_name → (skill_name, tool)
    tool_index: HashMap<String, (String, Arc<dyn Tool>)>,
}

impl Default for SkillsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillsManager {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            tool_index: HashMap::new(),
        }
    }

    /// Register a skill. Panics if a tool with the same name is already registered.
    pub fn register(&mut self, skill: Skill) {
        for tool in &skill.tools {
            let existing = self.tool_index.insert(
                tool.name().to_string(),
                (skill.name.clone(), Arc::clone(tool)),
            );
            if let Some((existing_skill, _)) = existing {
                panic!(
                    "Tool '{}' is already registered by skill '{}'",
                    tool.name(),
                    existing_skill
                );
            }
        }
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Register a single tool as an anonymous skill.
    pub fn register_tool(&mut self, name: &str, tool: Arc<dyn Tool>) {
        let skill = Skill {
            name: name.to_string(),
            description: format!("Builtin tool: {}", name),
            tools: vec![tool],
        };
        self.register(skill);
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tool_index.get(name).map(|(_, t)| Arc::clone(t))
    }

    /// Get all registered tools.
    pub fn all_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tool_index.values().map(|(_, t)| Arc::clone(t)).collect()
    }

    /// Number of registered tools.
    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }

    /// Number of registered skills.
    pub fn skill_count(&self) -> usize {
        self.skills.len()
    }

    /// Iterate over all skills (name, &Skill).
    pub fn skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)> {
        self.skills.iter().map(|(k, v)| (k.as_str(), v))
    }
}
