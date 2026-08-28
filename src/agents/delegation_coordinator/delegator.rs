//! `impl AgentDelegator` — the tool-facing sync/async delegation entry
//! points (`agent_delegate` / `agent_delegate async`).

use super::DelegationCoordinator;
use super::lifecycle::resolve_timeout;

/// `DelegationCoordinator` implements the canonical [`AgentDelegator`] trait
/// (the legacy `TaskDelegator` dual-impl was removed in H47). The `delegate`
/// method here carries the parent `&Session` so callees can read
/// `reply_target`, `owner` (for per-user scoping), and `last_message` from
/// the same value.
#[async_trait::async_trait]
impl crate::agents::AgentDelegator for DelegationCoordinator {
    async fn delegate(
        &self,
        agent_name: &str,
        task: &str,
        parent_ctx: &crate::api::tool::ToolContext,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        let config = self.find_agent(agent_name);
        let config_timeout = config.as_ref().and_then(|a| a.config.timeout);
        let timeout_secs = resolve_timeout(timeout, config_timeout);
        // Create the sub-session context up front — the sub-session id is
        // the agent's identity, created before the delegation starts (same
        // unified path as the async `spawn_delegate_async`).
        let (sub_ctx, _sub_session_id) = self
            .session_manager
            .create_sub_session_context(&parent_ctx.session_id, agent_name)?;
        self.delegate_with_parent(
            agent_name,
            task,
            &parent_ctx.session_id,
            sub_ctx,
            timeout_secs,
            None,
            allowed_tools,
            workspace,
        )
        .await
    }

    fn delegate_async(
        &self,
        agent_name: &str,
        task: &str,
        parent_ctx: &crate::api::tool::ToolContext,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        let config = self.find_agent(agent_name);
        let config_timeout = config.as_ref().and_then(|a| a.config.timeout);
        let timeout_secs = resolve_timeout(timeout, config_timeout);
        self.spawn_delegate_async(
            agent_name,
            task,
            &parent_ctx.session_id,
            timeout_secs,
            allowed_tools,
            workspace,
        )
    }

    fn list_available(&self) -> Vec<(String, Option<String>)> {
        self.configs
            .values_cloned()
            .into_iter()
            .map(|a| (a.config.name.clone(), a.config.description.clone()))
            .collect()
    }
}
