use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use super::workspace::skills::SkillManager;
use super::AgentRegistry;

/// Hot-loadable shared resources, held in Arc for sharing between
/// CompactionExecutor instances. Most fields are kept for future
/// summarizer features (hot-reload of skills/agents during compaction,
/// memory dir lookups) — `knowledge_dir` is the only one actively read
/// today by `build_memory_prompt`.
#[allow(dead_code)]
pub(crate) struct ResourceProvider {
    pub(crate) skills: Arc<RwLock<SkillManager>>,
    pub(crate) sub_agents: Arc<AgentRegistry>,
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
        sub_agents: Arc<AgentRegistry>,
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
