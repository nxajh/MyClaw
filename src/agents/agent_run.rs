//! Agent execution — bridges the RFC v2 Agent interface to the existing AgentLoop.
//!
//! C17: `Agent { config: SubAgentConfig }` — the agent is just its configuration.
//! C18: `Agent::run(session, ctx, runtime)` — execution happens here.
//!
//! During the migration period (until H45 deletes AgentLoop), this module
//! constructs a temporary AgentLoop from the runtime resources and delegates
//! to it. After H45, the logic will be inlined here.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::config::sub_agent::SubAgentConfig;
use crate::agents::session::Session;
use crate::agents::session::PersistHook;
use crate::agents::tool_registry::ToolRegistry;
use crate::agents::workspace::skills::SkillManager;
use crate::agents::loop_breaker::LoopBreakerConfig;
use crate::agents::context_engine::ContextEngine;
use crate::agents::tool_executor::ToolExecutor;
use crate::agents::resource_provider::ResourceProvider;
use crate::agents::request_builder::RequestBuilder;
use crate::agents::prompt::{SystemPromptBuilder, SystemPromptConfig};
use crate::agents::attachment::AttachmentManager;
use crate::providers::ProviderRegistry;
use crate::config::agent::ContextConfig;

use super::turn::{TurnContext, TurnResult};
use super::runtime::AgentRuntime;

/// RFC v2 Agent — identity is just its configuration.
///
/// All runtime resources (tools, skills, providers) live in `AgentRuntime`
/// and are passed to `run()` at invocation time.
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
    /// This is the main entry point per RFC v2 §三.C.
    /// Currently bridges to AgentLoop; will be inlined after H45.
    pub async fn run(
        &self,
        session: &mut Session,
        _ctx: TurnContext<'_>,
        _runtime: &AgentRuntime,
    ) -> anyhow::Result<TurnResult> {
        // Bridge: create a temporary AgentLoop and delegate.
        // This will be replaced with direct implementation in H45.
        let agent_config = self.build_agent_config();
        let system_prompt = self.config.system_prompt.clone();

        // Build the loop using the old factory pattern.
        // Note: runtime resources are wired through the Agent struct's
        // existing factory methods. Full runtime integration is C18 final.
        todo!("Bridge to AgentLoop pending E29+F36 integration")
    }

    /// Build an AgentConfig from the SubAgentConfig + runtime defaults.
    fn build_agent_config(&self) -> crate::agents::agent_impl::AgentConfig {
        let mut config = crate::agents::agent_impl::AgentConfig::default();
        if let Some(max) = self.config.max_tool_calls {
            config.max_tool_calls = max;
        }
        if let Some(ref model) = self.config.model {
            config.model_override = Some(model.clone());
        }
        config
    }

    /// Build tool specs filtered by this agent's config.
    pub fn filtered_tool_specs(
        &self,
        runtime: &AgentRuntime,
    ) -> Vec<crate::providers::capability_chat::ToolSpec> {
        runtime.tools
            .all_tools()
            .iter()
            .filter(|t| {
                let source = t.source();
                match &source {
                    crate::providers::capability_tool::ToolSource::Builtin => {
                        // Legacy filter: tools list is a whitelist; empty = none.
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
