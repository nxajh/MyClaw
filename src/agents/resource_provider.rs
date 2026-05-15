use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::config::sub_agent::SubAgentConfig;
use super::workspace::skills::SkillManager;

/// Hot-loadable shared resources, held in Arc for sharing between AgentLoop instances.
///
/// Immutable after construction (skills/sub_agents are interior-mutable via RwLock).
/// Sub-agents get their own ResourceProvider with no change_rx (no hot-reload).
pub(crate) struct ResourceProvider {
    pub(crate) skills: Arc<RwLock<SkillManager>>,
    pub(crate) sub_agents: Arc<RwLock<Vec<SubAgentConfig>>>,
    pub(crate) mcp_instructions: Vec<(String, String)>,
    pub(crate) skills_dir: PathBuf,
    pub(crate) agents_dir: PathBuf,
    /// Absolute path to the memory/ directory (for diff_memory scanning).
    pub(crate) knowledge_dir: String,
    /// Timezone offset in hours (for date injection).
    pub(crate) timezone_offset: i32,
}

impl ResourceProvider {
    pub(crate) fn new(
        skills: Arc<RwLock<SkillManager>>,
        sub_agents: Arc<RwLock<Vec<SubAgentConfig>>>,
        mcp_instructions: Vec<(String, String)>,
        skills_dir: PathBuf,
        agents_dir: PathBuf,
        knowledge_dir: String,
        timezone_offset: i32,
    ) -> Arc<Self> {
        Arc::new(Self {
            skills,
            sub_agents,
            mcp_instructions,
            skills_dir,
            agents_dir,
            knowledge_dir,
            timezone_offset,
        })
    }
}
