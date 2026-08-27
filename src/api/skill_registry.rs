//! skill_registry — L0 facade over `agents::SkillManager`.
//!
//! #151 Phase 8+: the skill tools (L3) must not reference `crate::agents`.
//! They only need a read view (find / names / listing metadata) plus the
//! #174 `reload_from_definitions` convergence point for post-edit
//! refresh. `SkillManager` (with its def→runtime conversion) stays in the
//! agents layer; `RwLock<SkillManager>` implements this trait there, and
//! the composition root keeps passing the shared `Arc<RwLock<SkillManager>>`
//! unchanged (generic constructors coerce to `Arc<dyn SkillRegistry>`),
//! so the live registry remains the very instance the system prompt is
//! built from.

use std::path::{Path, PathBuf};

/// Full view of one skill as needed by `skill_view`: identity, body, and
/// the on-disk directory for `${SKILL_DIR}` substitution / file reads.
#[derive(Debug, Clone)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    pub prompt_body: String,
    pub agent_invocable: bool,
    pub skill_dir: Option<PathBuf>,
}

/// Listing metadata as rendered by `skills_list`.
#[derive(Debug, Clone)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub agent_invocable: bool,
    pub user_invocable: bool,
}

/// Facade over the live skill registry.
pub trait SkillRegistry: Send + Sync {
    /// Full view of a skill by name (`skill_view`'s lookup).
    fn find(&self, name: &str) -> Option<SkillView>;

    /// All registered skill names (not-found listings).
    fn skill_names(&self) -> Vec<String>;

    /// Listing metadata for every skill (`skills_list`).
    fn list(&self) -> Vec<SkillSummary>;

    /// Post-edit hot reload: layered load (`workspace/skills` + shared
    /// `~/.agents/skills`) then wholesale replace — the #174
    /// `reload_from_definitions` convergence point, def parsing kept in
    /// the agents layer.
    fn reload_layered(&self, skills_dir: &Path, agents_skills_dir: Option<&Path>);
}

/// In-memory double for tests — register full views; listing metadata is
/// derived so `skills_list` assertions exercise the same shape.
#[derive(Default)]
pub struct InMemorySkillRegistry {
    skills: std::sync::RwLock<Vec<SkillView>>,
}

impl InMemorySkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, view: SkillView) {
        let mut skills = self.skills.write().unwrap();
        skills.retain(|s| s.name != view.name);
        skills.push(view);
    }
}

impl SkillRegistry for InMemorySkillRegistry {
    fn find(&self, name: &str) -> Option<SkillView> {
        self.skills.read().unwrap().iter().find(|s| s.name == name).cloned()
    }

    fn skill_names(&self) -> Vec<String> {
        self.skills.read().unwrap().iter().map(|s| s.name.clone()).collect()
    }

    fn list(&self) -> Vec<SkillSummary> {
        self.skills
            .read()
            .unwrap()
            .iter()
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                version: None,
                when_to_use: None,
                argument_hint: None,
                agent_invocable: s.agent_invocable,
                user_invocable: true,
            })
            .collect()
    }

    fn reload_layered(&self, _skills_dir: &Path, _agents_skills_dir: Option<&Path>) {
        // Test double: no on-disk layering to perform.
    }
}
