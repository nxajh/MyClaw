use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use super::AgentRegistry;
use super::workspace::skills::SkillManager;

/// Hot-loadable shared resources, held in Arc for sharing between
/// CompactionExecutor instances. Most fields are kept for future
/// summarizer features (hot-reload of skills/agents during compaction,
/// memory dir lookups) — `memory_root` is the only one actively read
/// today by `build_memory_prompt`.
#[allow(dead_code)]
pub struct ResourceProvider {
    pub(crate) skills: Arc<RwLock<SkillManager>>,
    pub(crate) sub_agents: Arc<AgentRegistry>,
    pub(crate) mcp_instructions: Vec<(String, String)>,
    pub(crate) skills_dir: PathBuf,
    pub(crate) agents_dir: PathBuf,
    /// Absolute path to the memory/ directory (for diff_memory scanning).
    pub(crate) memory_root: String,
    /// Timezone offset in hours (for date injection).
    pub(crate) timezone_offset: i32,
}

impl ResourceProvider {
    pub fn new(
        skills: Arc<RwLock<SkillManager>>,
        sub_agents: Arc<AgentRegistry>,
        mcp_instructions: Vec<(String, String)>,
        skills_dir: PathBuf,
        agents_dir: PathBuf,
        memory_root: String,
        timezone_offset: i32,
    ) -> Arc<Self> {
        Arc::new(Self {
            skills,
            sub_agents,
            mcp_instructions,
            skills_dir,
            agents_dir,
            memory_root,
            timezone_offset,
        })
    }

    /// Configured timezone offset in hours (from `[prompt] timezone_offset`).
    /// Used for date injection in the per-turn attachment diff.
    pub fn timezone_offset(&self) -> i32 {
        self.timezone_offset
    }
}
