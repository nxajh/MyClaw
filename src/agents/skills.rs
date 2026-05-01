//! Skills manager — registers tools and provides them to AgentLoop.
//!
//! A Skill is a named group of tools. The manager maintains a flat
//! tool-name → tool registry for fast lookup.
//!
//! DDD: SkillsManager depends on `crate::providers::tool::Tool` (Domain trait),
//! not on any Infrastructure concrete type.

use std::collections::HashMap;
use std::sync::Arc;

use crate::providers::capability_tool::Tool;

use super::skill_loader::SkillDefinition;

/// A named collection of tools.
#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,      // 新增：用于匹配和提示
    pub prompt_body: String,        // 新增：注入到 system prompt 的内容
    /// Tools belonging to this skill.
    pub tools: Vec<Arc<dyn Tool>>,
}

impl Skill {
    /// 从 SkillDefinition 创建（无工具，仅用于提示词注入）
    pub fn from_definition(def: &SkillDefinition) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            keywords: def.keywords.clone(),
            prompt_body: def.prompt_body.clone(),
            tools: Vec::new(),
        }
    }

    /// 从 SkillDefinition 创建（带工具）
    pub fn from_definition_with_tools(
        def: &SkillDefinition,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            keywords: def.keywords.clone(),
            prompt_body: def.prompt_body.clone(),
            tools,
        }
    }
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
            keywords: Vec::new(),
            prompt_body: String::new(),
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

    /// 获取所有已加载 Skill 的提示词，用于注入 system prompt
    pub fn skill_prompts(&self) -> Vec<(&str, &str)> {
        self.skills.values()
            .filter(|s| !s.prompt_body.is_empty())
            .map(|s| (s.name.as_str(), s.prompt_body.as_str()))
            .collect()
    }
}
