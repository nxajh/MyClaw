//! Sub-agent delegator — creates and runs specialized sub-agents on demand.
//!
//! Implements `TaskDelegator` by creating temporary `AgentLoop` instances
//! with restricted tool sets and specialized system prompts.
//!
//! Also provides `delegate_async` for non-blocking background execution.

use std::sync::Arc;

use crate::agents::delegation::{DelegationEvent, DelegationManager};
use crate::agents::skills::SkillsManager;
use crate::agents::session_manager::Session;
use crate::config::sub_agent::SubAgentConfig;
use crate::providers::ServiceRegistry;
use crate::tools::TaskDelegator;

/// Holds sub-agent configs and creates temporary AgentLoops for delegation.
#[derive(Clone)]
pub struct SubAgentDelegator {
    /// Sub-agent configurations, keyed by name.
    configs: Arc<Vec<SubAgentConfig>>,
    /// Shared service registry (for LLM access).
    registry: Arc<dyn ServiceRegistry>,
    /// Parent skills manager (tools are filtered per sub-agent).
    skills: Arc<SkillsManager>,
    /// Default max_tool_calls from parent agent config.
    default_max_tool_calls: usize,
}

impl SubAgentDelegator {
    pub fn new(
        configs: Vec<SubAgentConfig>,
        registry: Arc<dyn ServiceRegistry>,
        skills: Arc<SkillsManager>,
        default_max_tool_calls: usize,
    ) -> Self {
        Self {
            configs: Arc::new(configs),
            registry,
            skills,
            default_max_tool_calls,
        }
    }

    fn find_config(&self, name: &str) -> Option<&SubAgentConfig> {
        self.configs.iter().find(|c| c.name == name)
    }

    /// Build a filtered SkillsManager containing only the allowed tools.
    fn build_filtered_skills(&self, allowed_tools: &[String]) -> SkillsManager {
        let mut filtered = SkillsManager::new();
        for tool_name in allowed_tools {
            if let Some(tool) = self.skills.get(tool_name) {
                filtered.register_tool(tool_name, tool);
            } else {
                tracing::warn!(tool = %tool_name, "sub-agent references unknown tool, skipping");
            }
        }
        filtered
    }

    /// Delegate a task asynchronously — spawns sub-agent in a background tokio task.
    ///
    /// Returns the `task_id` immediately. When the sub-agent completes, it sends
    /// a `DelegationEvent` via the `DelegationManager`'s channel, which the
    /// Orchestrator listens for.
    pub fn delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        session_key: &str,
        reply_target: &str,
        delegation_manager: &DelegationManager,
    ) -> anyhow::Result<String> {
        let config = self.find_config(agent_name)
            .ok_or_else(|| {
                let available: Vec<&str> = self.configs.iter().map(|c| c.name.as_str()).collect();
                anyhow::anyhow!(
                    "Unknown sub-agent '{}'. Available: {}",
                    agent_name,
                    available.join(", ")
                )
            })?;

        let task_id = format!("del_{}", uuid::Uuid::new_v4());

        tracing::info!(
            agent = %config.name,
            task_id = %task_id,
            task_len = task.len(),
            "spawning sub-agent in background"
        );

        // Clone everything needed for the spawned task.
        let configs = self.configs.clone();
        let registry = self.registry.clone();
        let skills = self.skills.clone();
        let default_max_tool_calls = self.default_max_tool_calls;
        let config_clone = config.clone();
        let task_owned = task.to_string();
        let session_key_owned = session_key.to_string();
        let reply_target_owned = reply_target.to_string();
        let event_tx = delegation_manager.event_sender();
        let task_id_clone = task_id.clone();

        let handle = tokio::spawn(async move {
            // Build a new SubAgentDelegator inside the spawned task
            // (all fields are Arc/Clone, so this is cheap).
            let sub_delegator = SubAgentDelegator {
                configs,
                registry,
                skills,
                default_max_tool_calls,
            };

            let result = sub_delegator.delegate(&config_clone.name, &task_owned).await;

            match result {
                Ok(summary) => {
                    tracing::info!(task_id = %task_id_clone, "sub-agent completed successfully");
                    let _ = event_tx.send(DelegationEvent::Completed {
                        task_id: task_id_clone.clone(),
                        session_key: session_key_owned,
                        reply_target: reply_target_owned,
                        summary,
                    }).await;
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id_clone, err = %e, "sub-agent failed");
                    let _ = event_tx.send(DelegationEvent::Failed {
                        task_id: task_id_clone.clone(),
                        session_key: session_key_owned,
                        reply_target: reply_target_owned,
                        error: e.to_string(),
                    }).await;
                }
            }
        });

        delegation_manager.register(task_id.clone(), handle);
        Ok(task_id)
    }
}

#[async_trait::async_trait]
impl TaskDelegator for SubAgentDelegator {
    async fn delegate(&self, agent_name: &str, task: &str) -> anyhow::Result<String> {
        let config = self.find_config(agent_name)
            .ok_or_else(|| {
                let available: Vec<&str> = self.configs.iter().map(|c| c.name.as_str()).collect();
                anyhow::anyhow!(
                    "Unknown sub-agent '{}'. Available: {}",
                    agent_name,
                    available.join(", ")
                )
            })?;

        tracing::info!(
            agent = %config.name,
            tools = ?config.tools,
            task_len = task.len(),
            "creating sub-agent for delegation"
        );

        // Build a filtered skills manager with only the allowed tools.
        let skills = self.build_filtered_skills(&config.tools);

        // Build the tool specs for the system prompt.
        let tool_names: Vec<String> = skills.all_tools().iter().map(|t| t.name().to_string()).collect();

        // Build system prompt using the sub-agent's prompt + tool list.
        let system_prompt = if config.system_prompt.is_empty() {
            format!("You are a specialized agent named '{}'.\nAvailable tools: {}",
                config.name, tool_names.join(", "))
        } else {
            format!("{}\n\nAvailable tools: {}", config.system_prompt, tool_names.join(", "))
        };

        // Create a throwaway session for this delegation.
        let session_key = format!("__subagent:{}:{}", config.name, uuid::Uuid::new_v4());
        let session = Session::new(session_key);

        // Create a temporary AgentLoop.
        let agent_config = crate::agents::AgentConfig {
            max_tool_calls: config.max_tool_calls.unwrap_or(self.default_max_tool_calls),
            max_history: 100,
            prompt_config: crate::agents::SystemPromptConfig {
                workspace_dir: String::new(),
                model_name: String::new(),
                autonomy: crate::agents::AutonomyLevel::Full,
                skills_mode: crate::agents::SkillsPromptInjectionMode::Compact,
                compact: true,
                max_chars: 0,
                bootstrap_max_chars: 0,
                native_tools: true,
                channel_name: None,
                host_info: None,
            },
        };

        // We need to create an AgentLoop manually since we don't have an Agent factory.
        // AgentLoop is not Clone and needs registry, skills, etc.
        // Use a temporary Agent to create the loop.
        let agent = crate::agents::Agent::new(
            self.registry.clone(),
            Arc::new(skills),
            agent_config,
        );
        let mut loop_ = agent.with_system_prompt(system_prompt).loop_for(session);

        tracing::info!(agent = %config.name, "sub-agent started");
        let result = loop_.run(task, None).await;
        match &result {
            Ok(text) => tracing::info!(agent = %config.name, text_len = text.len(), "sub-agent completed"),
            Err(e) => tracing::warn!(agent = %config.name, err = %e, "sub-agent failed"),
        }
        result
    }

    fn available_agents(&self) -> Vec<(String, String)> {
        self.configs
            .iter()
            .map(|c| {
                let desc = c.description.as_deref().unwrap_or("");
                (c.name.clone(), desc.to_string())
            })
            .collect()
    }
}
