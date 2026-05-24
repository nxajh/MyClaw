//! Agent execution — bridges the RFC v2 Agent interface to the existing AgentLoop.
//!
//! C17: `Agent { config: SubAgentConfig }` — the agent is just its configuration.
//! C18: `Agent::run(session, ctx, runtime)` — execution happens here.
//!
//! During the migration period (until H45 deletes AgentLoop), this module
//! constructs a temporary AgentLoop from the runtime resources and delegates
//! to it. After H45, the logic will be inlined here.

use std::sync::Arc;

use crate::config::sub_agent::SubAgentConfig;
use crate::agents::session::Session;
use crate::agents::session::PersistHook;
use crate::agents::tool_registry::ToolRegistry;
use crate::agents::workspace::skills::SkillManager;
use crate::agents::loop_breaker::LoopBreakerConfig;
use crate::agents::compaction_policy::CompactionPolicy;
use crate::agents::compaction_executor::CompactionExecutor;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::resource_provider::ResourceProvider;
use crate::agents::request_builder::RequestBuilder;
use crate::agents::prompt::SystemPromptBuilder;
use crate::providers::ProviderRegistry;

use super::runtime::AgentRuntime;
use super::agent_impl::{AgentConfig, AgentLoop};
use super::loop_breaker::LoopBreaker;

/// RFC v2 Agent — identity is just its configuration.
#[derive(Debug, Clone)]
pub struct Agent {
    pub config: SubAgentConfig,
}

impl Agent {
    pub fn new(config: SubAgentConfig) -> Self {
        Self { config }
    }

    /// Execute a single turn: user message → LLM → tool calls → response.
    ///
    /// C18: bridges to AgentLoop. Will be inlined after H45.
    pub async fn run(
        &self,
        session: Session,
        user_message: &str,
        image_urls: Option<Vec<String>>,
        image_base64: Option<Vec<String>>,
        runtime: &AgentRuntime,
        persist_hook: Option<Arc<dyn PersistHook>>,
    ) -> anyhow::Result<(Session, String)> {
        let agent_config = self.build_agent_config(runtime);
        let system_prompt = self.build_system_prompt(runtime, &agent_config);

        let resources = ResourceProvider::new(
            Arc::clone(runtime.skills()),
            runtime.sub_agent_configs(),
            runtime.mcp_instructions(),
            runtime.skills_dir(),
            runtime.agents_dir(),
            runtime.knowledge_dir.to_string_lossy().to_string(),
            agent_config.timezone_offset,
        );
        let request_builder = RequestBuilder::new(system_prompt, Arc::clone(&resources));

        let mut loop_ = AgentLoop {
            registry: Arc::clone(runtime.registry()),
            compactor: CompactionExecutor::new(
                Arc::clone(runtime.registry()),
                Arc::clone(&resources),
                Arc::clone(runtime.tools()),
            ),
            tool_executor: ToolExecutor::new(Arc::clone(runtime.tools()), agent_config.tool_timeout_secs),
            config: agent_config,
            session,
            request_builder,
            loop_breaker: LoopBreaker::new(LoopBreakerConfig {
                max_tool_calls: self.config.max_tool_calls.unwrap_or(50),
                exact_repeat_threshold: 3,
                ..LoopBreakerConfig::default()
            }),
            policy: CompactionPolicy::from_context_config(&crate::config::agent::ContextConfig::default()),
            persist_hook,
            pending_retry_message: None,
        };

        let result = loop_.run(user_message, image_urls, image_base64).await;

        match result {
            Ok(text) => Ok((loop_.session, text)),
            Err(e) => Err(e),
        }
    }

    fn build_agent_config(&self, runtime: &AgentRuntime) -> AgentConfig {
        let mut config = AgentConfig::default();
        if let Some(max) = self.config.max_tool_calls {
            config.max_tool_calls = max;
        }
        if let Some(ref model) = self.config.model {
            config.model_override = Some(model.clone());
        }
        config.tool_timeout_secs = runtime.tool_timeout_secs;
        config
    }

    fn build_system_prompt(&self, runtime: &AgentRuntime, agent_config: &AgentConfig) -> String {
        if !self.config.system_prompt.is_empty() {
            self.config.system_prompt.clone()
        } else {
            let skills = runtime.skills().read();
            let builder = SystemPromptBuilder::new(agent_config.prompt_config.clone());
            builder.build(&skills)
        }
    }

    /// Build tool specs filtered by this agent's config.
    pub fn filtered_tool_specs(
        &self,
        runtime: &AgentRuntime,
    ) -> Vec<crate::providers::capability_chat::ToolSpec> {
        runtime.tools()
            .all_tools()
            .iter()
            .filter(|t| {
                let source = t.source();
                match &source {
                    crate::providers::capability_tool::ToolSource::Builtin => {
                        self.config.allows_tool(t.name())
                    }
                    crate::providers::capability_tool::ToolSource::Skill { name } => {
                        self.config.allows_skill(name)
                    }
                    crate::providers::capability_tool::ToolSource::Mcp { server } => {
                        self.config.allows_mcp(server)
                    }
                }
            })
            .map(|t| {
                let spec = t.spec();
                crate::providers::capability_chat::ToolSpec {
                    name: spec.name,
                    description: Some(spec.description),
                    input_schema: spec.parameters,
                }
            })
            .collect()
    }
}
