//! Skill manager — skill definitions and prompt injection.
//!
//! A Skill is behavioral guidance for the model (when to trigger, how to use),
//! NOT an executable tool. Tools live in ToolRegistry.

use std::collections::HashMap;

use super::skill_loader::SkillDefinition;

/// A skill definition (loaded from SKILL.md).
#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub prompt_body: String,
}

impl Skill {
    /// Create from a SkillDefinition.
    pub fn from_definition(def: &SkillDefinition) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            keywords: def.keywords.clone(),
            prompt_body: def.prompt_body.clone(),
        }
    }
}

/// SkillManager manages skill definitions for system prompt injection.
pub struct SkillManager {
    skills: HashMap<String, Skill>,
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Register a skill.
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Number of registered skills.
    pub fn skill_count(&self) -> usize {
        self.skills.len()
    }

    /// Iterate over all skills (name, &Skill).
    pub fn skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)> {
        self.skills.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Get all skill prompts (name, prompt_body) for system prompt injection.
    pub fn skill_prompts(&self) -> Vec<(&str, &str)> {
        self.skills.values()
            .filter(|s| !s.prompt_body.is_empty())
            .map(|s| (s.name.as_str(), s.prompt_body.as_str()))
            .collect()
    }
}
