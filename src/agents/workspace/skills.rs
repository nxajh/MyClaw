//! Skill manager — skill definitions and prompt injection.
//!
//! A Skill is behavioral guidance for the model (when to trigger, how to use),
//! NOT an executable tool. Tools live in ToolRegistry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::skill_loader::SkillDefinition;

/// Injection safety valve (issue #123): the Agent Skills standard's own
/// `description` length ceiling (agentskills.io). This is a backstop
/// against a runaway author-written description, not a design knob —
/// well-written skills (compact, third person, what + when + a handful of
/// trigger phrases; long-tail trigger words belong in the skill body
/// instead) never come close to it.
const MAX_INJECTED_DESCRIPTION_CHARS: usize = 1024;

/// A skill definition (loaded from SKILL.md).
#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
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

    /// The text injected into the system prompt every time this skill is
    /// announced (issue #123: reverts #119's non-standard `summary` field
    /// split — the Agent Skills standard injects `description` in full,
    /// with no separate local-only field, so skills stay portable across
    /// agent implementations sharing `~/.agents/skills`, issue #83).
    /// Capped at `MAX_INJECTED_DESCRIPTION_CHARS` as a safety valve; logs a
    /// warning on the rare description that actually hits it.
    pub fn injected_summary(&self) -> String {
        let char_count = self.description.chars().count();
        if char_count <= MAX_INJECTED_DESCRIPTION_CHARS {
            return self.description.clone();
        }
        tracing::warn!(
            skill = %self.name,
            len = char_count,
            cap = MAX_INJECTED_DESCRIPTION_CHARS,
            "skill description exceeds the injection safety cap; truncating"
        );
        format!(
            "{}...",
            crate::str_utils::truncate_chars(
                &self.description,
                MAX_INJECTED_DESCRIPTION_CHARS.saturating_sub(3)
            )
        )
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

    /// Hot-reload from raw parsed definitions — the def→runtime conversion
    /// stays inside the agents layer so callers (e.g. skill_manage_tool)
    /// never construct `Skill` directly.
    pub fn reload_from_definitions(&mut self, defs: Vec<SkillDefinition>) {
        let skills = defs.iter().map(Skill::from_definition).collect();
        self.reload(skills);
    }

    /// Register a single parsed definition (conversion encapsulated here).
    pub fn register_definition(&mut self, def: &SkillDefinition) {
        self.register(Skill::from_definition(def));
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

    fn skill(description: &str) -> Skill {
        Skill {
            name: "x".to_string(),
            description: description.to_string(),
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

    /// issue #123: with the `summary` field reverted, `injected_summary()`
    /// is just the full `description` — the Agent Skills standard has no
    /// separate injected-vs-full-text split.
    #[test]
    fn injected_summary_is_full_description_when_under_cap() {
        let s = skill("Get current weather conditions and forecasts.");
        assert_eq!(
            s.injected_summary(),
            "Get current weather conditions and forecasts."
        );
    }

    #[test]
    fn injected_summary_preserves_multiple_lines_under_cap() {
        // Regression guard: the old implementation used a helper that
        // collapsed to the first line, which would have silently dropped
        // this second line even though the whole description is tiny.
        let s = skill("What it does.\nWhen to use it.");
        assert_eq!(s.injected_summary(), "What it does.\nWhen to use it.");
    }

    #[test]
    fn injected_summary_truncates_only_past_the_safety_cap() {
        // A description right at the standard's own 1024-char ceiling is
        // untouched...
        let at_cap = "a".repeat(MAX_INJECTED_DESCRIPTION_CHARS);
        let s = skill(&at_cap);
        assert_eq!(s.injected_summary(), at_cap);

        // ...one character past it triggers truncation.
        let over_cap = "a".repeat(MAX_INJECTED_DESCRIPTION_CHARS + 1);
        let s = skill(&over_cap);
        let injected = s.injected_summary();
        assert_eq!(injected.chars().count(), MAX_INJECTED_DESCRIPTION_CHARS);
        assert!(injected.ends_with("..."));
    }
}


// ── #151 Phase 8+ SkillRegistry facade ───────────────────────────────────────
// skill 工具（L3）经 api::skill_registry 只看到 find/names/list/reload_layered
// 方法面；Skill→SkillView/SkillSummary 的 DTO 转换与 #174 的
// reload_from_definitions 收敛点（def 解析）都留在本层。组合根继续传共享的
// Arc<RwLock<SkillManager>>——与系统提示词注入用的是同一个活实例。

impl crate::api::skill_registry::SkillRegistry for parking_lot::RwLock<SkillManager> {
    fn find(&self, name: &str) -> Option<crate::api::skill_registry::SkillView> {
        use crate::api::skill_registry::SkillView;
        self.read().get(name).map(|s| SkillView {
            name: s.name.clone(),
            description: s.description.clone(),
            prompt_body: s.prompt_body.clone(),
            agent_invocable: s.agent_invocable,
            skill_dir: s.skill_dir.clone(),
        })
    }

    fn skill_names(&self) -> Vec<String> {
        self.read().skills_iter().map(|(n, _)| n.to_string()).collect()
    }

    fn skill_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        self.read().skill_dir(name).map(|p| p.to_path_buf())
    }

    fn list(&self) -> Vec<crate::api::skill_registry::SkillSummary> {
        use crate::api::skill_registry::SkillSummary;
        self.read()
            .skills_iter()
            .map(|(n, s)| SkillSummary {
                name: n.to_string(),
                description: s.description.clone(),
                version: s.version.clone(),
                when_to_use: s.when_to_use.clone(),
                argument_hint: s.argument_hint.clone(),
                agent_invocable: s.agent_invocable,
                user_invocable: s.user_invocable,
                skill_dir: s.skill_dir.clone(),
            })
            .collect()
    }

    fn reload_layered(&self, skills_dir: &Path, agents_skills_dir: Option<&Path>) {
        let defs = super::skill_loader::load_skills_layered(skills_dir, agents_skills_dir);
        self.write().reload_from_definitions(defs);
    }
}
