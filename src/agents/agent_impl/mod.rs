//! Legacy `Agent` factory + `AgentConfig` — held over from the pre-RFC v2
//! architecture as a passive bag of construction data the Orchestrator,
//! daemon, commands, scheduler, and webhook server still read from
//! (`registry()` / `tools()` / `skills()` / `sub_agent_configs()` /
//! `cached_system_prompt()` / `config().prompt_config` /
//! `compact_threshold()`).
//!
//! `Agent2` (`src/agents/agent.rs`) is the RFC v2 per-turn executor;
//! the legacy `AgentLoop` it replaced has been deleted (H45). This
//! module is on track for deletion once its accessors are moved onto
//! `AgentRuntime` and the callers are updated.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::providers::ProviderRegistry;
use crate::config::agent::ContextConfig;

use super::skills::SkillManager;
use super::tool_registry::ToolRegistry;
use crate::agents::prompt::{SystemPromptBuilder, SystemPromptConfig};

/// AgentConfig — passive bag of construction data still consumed by
/// Orchestrator + commands.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_tool_calls: usize,
    pub prompt_config: SystemPromptConfig,
    pub context: ContextConfig,
    pub loop_breaker_threshold: usize,
    pub tool_timeout_secs: u64,
    pub model_override: Option<String>,
    pub thinking_override: Option<crate::providers::ThinkingConfig>,
    pub timezone_offset: i32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 100,
            prompt_config: SystemPromptConfig::default(),
            context: ContextConfig::default(),
            loop_breaker_threshold: 3,
            tool_timeout_secs: 180,
            model_override: None,
            thinking_override: None,
            timezone_offset: 8,
        }
    }
}

/// Agent — shared construction data accessor.
#[derive(Clone)]
pub struct Agent {
    registry: Arc<dyn ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    skills: Arc<RwLock<SkillManager>>,
    config: AgentConfig,
    system_prompt: String,
    model_override: Option<String>,
    mcp_instructions: Vec<(String, String)>,
    sub_agent_configs: super::AgentRegistry,
    skills_dir: PathBuf,
    agents_dir: PathBuf,
}

impl Agent {
    pub fn new(
        registry: Arc<dyn ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        skills: Arc<RwLock<SkillManager>>,
        config: AgentConfig,
    ) -> Self {
        Self {
            registry,
            tools,
            skills,
            config,
            system_prompt: String::new(),
            model_override: None,
            mcp_instructions: Vec::new(),
            sub_agent_configs: super::AgentRegistry::new(),
            skills_dir: PathBuf::new(),
            agents_dir: PathBuf::new(),
        }
    }

    pub fn registry(&self) -> &Arc<dyn ProviderRegistry> { &self.registry }
    pub fn tools(&self) -> &Arc<super::tool_registry::ToolRegistry> { &self.tools }
    pub fn skills(&self) -> &Arc<RwLock<SkillManager>> { &self.skills }
    pub fn config(&self) -> &AgentConfig { &self.config }
    pub fn cached_system_prompt(&self) -> &str { &self.system_prompt }

    pub fn sub_agent_configs(&self) -> &super::AgentRegistry { &self.sub_agent_configs }
    pub fn workspace_dir(&self) -> &str { &self.config.prompt_config.workspace_dir }
    pub fn compact_threshold(&self) -> f64 { self.config.context.compact_threshold }

    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn with_model(mut self, model: String) -> Self {
        self.model_override = Some(model);
        self
    }

    pub fn with_mcp_instructions(mut self, instructions: Vec<(String, String)>) -> Self {
        self.mcp_instructions = instructions;
        self
    }

    pub fn with_sub_agent_configs(mut self, configs: super::AgentRegistry) -> Self {
        self.sub_agent_configs = configs;
        self
    }

    pub fn with_workspace_dirs(mut self, skills_dir: PathBuf, agents_dir: PathBuf) -> Self {
        self.skills_dir = skills_dir;
        self.agents_dir = agents_dir;
        self
    }

    /// Build the system prompt if one wasn't supplied via
    /// `with_system_prompt`. Used by callers that need the assembled
    /// prompt text but didn't pre-build it.
    #[allow(dead_code)]
    pub fn build_system_prompt(&self) -> String {
        if !self.system_prompt.is_empty() {
            return self.system_prompt.clone();
        }
        let skills = self.skills.read();
        SystemPromptBuilder::new(self.config.prompt_config.clone()).build(&skills)
    }
}
