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
    pub source_layer: String,
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
            source_layer: def.source_layer.clone(),
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
    user_skills: HashMap<String, HashMap<String, Skill>>,
    agent_skills: HashMap<String, Skill>,
    shared_skills: HashMap<String, Skill>,
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            user_skills: HashMap::new(),
            agent_skills: HashMap::new(),
            shared_skills: HashMap::new(),
        }
    }

    /// Register a skill (defaults to agent skills for backward compat)
    pub fn register(&mut self, skill: Skill) {
        self.agent_skills.insert(skill.name.clone(), skill);
    }

    /// Number of registered agent + shared skills (excluding users).
    pub fn skill_count(&self) -> usize {
        self.agent_skills.len() + self.shared_skills.len()
    }

    /// Number of skills for a given owner.
    pub fn total_skill_count(&self, owner: Option<&str>) -> usize {
        let mut count = self.agent_skills.len() + self.shared_skills.len();
        if let Some(o) = owner {
            if let Some(u) = self.user_skills.get(o) {
                count += u.len();
            }
        }
        count
    }

    /// Iterate over all skills for an owner (user > agent > shared).
    pub fn skills_iter<'a>(&'a self, owner: Option<&'a str>) -> impl Iterator<Item = (&'a str, &'a Skill)> {
        let mut all = HashMap::new();
        for (k, v) in &self.shared_skills {
            all.insert(k.as_str(), v);
        }
        for (k, v) in &self.agent_skills {
            all.insert(k.as_str(), v);
        }
        if let Some(o) = owner {
            if let Some(u) = self.user_skills.get(o) {
                for (k, v) in u {
                    all.insert(k.as_str(), v);
                }
            }
        }
        all.into_iter()
    }

    /// Iterate only agent-invocable skills for an owner (user > agent > shared).
    pub fn agent_skills_iter<'a>(&'a self, owner: Option<&'a str>) -> impl Iterator<Item = (&'a str, &'a Skill)> {
        self.skills_iter(owner).filter(|(_, s)| s.agent_invocable)
    }

    /// Get skill directory path by name.
    pub fn skill_dir(&self, name: &str, owner: Option<&str>) -> Option<&Path> {
        self.get(name, owner).and_then(|s| s.skill_dir.as_deref())
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str, owner: Option<&str>) -> Option<&Skill> {
        if let Some(o) = owner {
            if let Some(u) = self.user_skills.get(o) {
                if let Some(s) = u.get(name) {
                    return Some(s);
                }
            }
        }
        if let Some(s) = self.agent_skills.get(name) {
            return Some(s);
        }
        if let Some(s) = self.shared_skills.get(name) {
            return Some(s);
        }
        None
    }

    /// Hot-reload: replace all skill definitions from definitions.
    pub fn reload_from_definitions(
        &mut self,
        user_skills_map: HashMap<String, Vec<SkillDefinition>>,
        agent_defs: Vec<SkillDefinition>,
        shared_defs: Vec<SkillDefinition>,
    ) {
        self.user_skills.clear();
        for (user_id, defs) in user_skills_map {
            let mut map = HashMap::new();
            for def in defs {
                map.insert(def.name.clone(), Skill::from_definition(&def));
            }
            self.user_skills.insert(user_id, map);
        }

        self.agent_skills.clear();
        for def in agent_defs {
            self.agent_skills.insert(def.name.clone(), Skill::from_definition(&def));
        }

        self.shared_skills.clear();
        for def in shared_defs {
            self.shared_skills.insert(def.name.clone(), Skill::from_definition(&def));
        }
    }

    /// Register a single parsed definition (conversion encapsulated here).
    pub fn register_definition(&mut self, def: &SkillDefinition) {
        self.register(Skill::from_definition(def));
    }

    /// Get all skill prompts (name, prompt_body) for system prompt injection.
    pub fn skill_prompts<'a>(&'a self, owner: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        self.skills_iter(owner)
            .filter(|(_, s)| !s.prompt_body.is_empty())
            .map(|(n, s)| (n, s.prompt_body.as_str()))
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
    fn find(&self, name: &str, owner: Option<&str>) -> Option<crate::api::skill_registry::SkillView> {
        use crate::api::skill_registry::SkillView;
        self.read().get(name, owner).map(|s| SkillView {
            name: s.name.clone(),
            description: s.description.clone(),
            prompt_body: s.prompt_body.clone(),
            agent_invocable: s.agent_invocable,
            skill_dir: s.skill_dir.clone(),
        })
    }

    fn skill_names(&self, owner: Option<&str>) -> Vec<String> {
        self.read().skills_iter(owner).map(|(n, _)| n.to_string()).collect()
    }

    fn skill_dir(&self, name: &str, owner: Option<&str>) -> Option<std::path::PathBuf> {
        self.read().skill_dir(name, owner).map(|p| p.to_path_buf())
    }

    fn list(&self, owner: Option<&str>) -> Vec<crate::api::skill_registry::SkillSummary> {
        use crate::api::skill_registry::SkillSummary;
        self.read()
            .skills_iter(owner)
            .map(|(n, s)| SkillSummary {
                name: n.to_string(),
                description: s.description.clone(),
                version: s.version.clone(),
                when_to_use: s.when_to_use.clone(),
                argument_hint: s.argument_hint.clone(),
                agent_invocable: s.agent_invocable,
                user_invocable: s.user_invocable,
                skill_dir: s.skill_dir.clone(),
                source_layer: s.source_layer.clone(),
            })
            .collect()
    }

    fn reload_layered(&self, user_skills_dir: Option<&Path>, skills_dir: &Path, agents_skills_dir: Option<&Path>) {
        let user_skills_map = if let Some(base) = user_skills_dir.and_then(|p| p.parent()) {
            super::skill_loader::load_all_users_skills(base)
        } else {
            std::collections::HashMap::new()
        };
        let agent_defs = super::skill_loader::load_skills_from_dir(skills_dir);
        let shared_defs = if let Some(d) = agents_skills_dir {
            super::skill_loader::load_skills_from_dir(d)
        } else {
            Vec::new()
        };
        self.write().reload_from_definitions(user_skills_map, agent_defs, shared_defs);
    }
}
