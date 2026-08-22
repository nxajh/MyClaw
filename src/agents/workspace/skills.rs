//! Skill manager — skill definitions and prompt injection.
//!
//! A Skill is behavioral guidance for the model (when to trigger, how to use),
//! NOT an executable tool. Tools live in ToolRegistry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::skill_loader::SkillDefinition;

/// A skill definition (loaded from SKILL.md).
#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub summary: Option<String>,
    pub keywords: Vec<String>,
    pub prompt_body: String,
    pub version: Option<String>,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub arguments: Vec<String>,
    pub user_invocable: bool,
    pub agent_invocable: bool,
    pub skill_dir: Option<PathBuf>,
}

impl Skill {
    /// Create from a SkillDefinition.
    pub fn from_definition(def: &SkillDefinition) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            summary: def.summary.clone(),
            keywords: def.keywords.clone(),
            prompt_body: def.prompt_body.clone(),
            version: def.version.clone(),
            when_to_use: def.when_to_use.clone(),
            argument_hint: def.argument_hint.clone(),
            arguments: def.arguments.clone(),
            user_invocable: def.user_invocable,
            agent_invocable: def.agent_invocable,
            skill_dir: def.source_path.parent().map(|p| p.to_path_buf()),
        }
    }

    /// The short text injected into the system prompt every time this
    /// skill is announced (issue #112) — the author's `summary` field if
    /// present, else a truncated `description` so unmigrated SKILL.md
    /// files keep working. Bounded either way: per-turn injection cost no
    /// longer grows with however many trigger-word-stuffed sentences an
    /// author packs into `description`, which stays available in full via
    /// `skill_view` and the `/skills` listing.
    pub fn injected_summary(&self) -> String {
        match &self.summary {
            Some(s) if !s.trim().is_empty() => crate::str_utils::truncate_line(s, 80),
            _ => crate::str_utils::truncate_line(&self.description, 60),
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

    /// Iterate only agent-invocable skills (for attachment injection).
    pub fn agent_skills_iter(&self) -> impl Iterator<Item = (&str, &Skill)> {
        self.skills
            .iter()
            .filter(|(_, s)| s.agent_invocable)
            .map(|(k, v)| (k.as_str(), v))
    }

    /// Get skill directory path by name.
    pub fn skill_dir(&self, name: &str) -> Option<&Path> {
        self.skills.get(name).and_then(|s| s.skill_dir.as_deref())
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Hot-reload: replace all skill definitions.
    pub fn reload(&mut self, new_skills: Vec<Skill>) {
        self.skills.clear();
        for skill in new_skills {
            self.skills.insert(skill.name.clone(), skill);
        }
    }

    /// Get all skill prompts (name, prompt_body) for system prompt injection.
    pub fn skill_prompts(&self) -> Vec<(&str, &str)> {
        self.skills
            .values()
            .filter(|s| !s.prompt_body.is_empty())
            .map(|s| (s.name.as_str(), s.prompt_body.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(description: &str, summary: Option<&str>) -> Skill {
        Skill {
            name: "x".to_string(),
            description: description.to_string(),
            summary: summary.map(str::to_string),
            keywords: vec![],
            prompt_body: String::new(),
            version: None,
            when_to_use: None,
            argument_hint: None,
            arguments: vec![],
            user_invocable: true,
            agent_invocable: true,
            skill_dir: None,
        }
    }

    /// issue #112: an explicit `summary` is what gets injected, verbatim
    /// when short enough — not the (possibly much longer) `description`.
    #[test]
    fn injected_summary_prefers_explicit_summary() {
        let s = skill(
            "Long description with lots of trigger words: foo, bar, baz, qux, quux.",
            Some("Get the weather."),
        );
        assert_eq!(s.injected_summary(), "Get the weather.");
    }

    #[test]
    fn injected_summary_falls_back_to_description_when_absent() {
        let s = skill("Short desc", None);
        assert_eq!(s.injected_summary(), "Short desc");
    }

    #[test]
    fn injected_summary_truncates_long_fallback_description() {
        let long = "a".repeat(200);
        let s = skill(&long, None);
        let injected = s.injected_summary();
        assert!(injected.chars().count() <= 63, "got len {}", injected.chars().count()); // 60 + "..."
        assert!(injected.ends_with("..."));
    }

    #[test]
    fn injected_summary_truncates_oversized_explicit_summary() {
        // A misused summary field is still bounded — the whole point is a
        // predictable per-turn injection cost regardless of author input.
        let long_summary = "b".repeat(200);
        let s = skill("short", Some(&long_summary));
        let injected = s.injected_summary();
        assert!(injected.chars().count() <= 83, "got len {}", injected.chars().count()); // 80 + "..."
        assert!(injected.ends_with("..."));
    }

    #[test]
    fn injected_summary_treats_blank_summary_as_absent() {
        let s = skill("fallback desc", Some("   "));
        assert_eq!(s.injected_summary(), "fallback desc");
    }
}
