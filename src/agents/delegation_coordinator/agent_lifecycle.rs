//! #151 Phase 8+ `AgentLifecycle` facade impl (L3 tools consume this trait).

use crate::api::delegation::RunningAgentInfo;

use super::DelegationCoordinator;

// ── #151 Phase 8+ AgentLifecycle facade ─────────────────────────────────────
// tools (L3) 经 `crate::api::agent_lifecycle::AgentLifecycle` 使用本类型；
// impl 放在 L4 模块内（依赖方向合法），组合根继续传具体 Arc。

#[async_trait::async_trait]
impl crate::api::agent_lifecycle::AgentLifecycle for DelegationCoordinator {
    async fn cancel(&self, sub_session_id: &str) -> bool {
        DelegationCoordinator::cancel(self, sub_session_id).await
    }

    fn running_records(&self) -> Vec<RunningAgentInfo> {
        DelegationCoordinator::running_records(self)
    }

    fn resume_timed_out(
        &self,
        sub_session_id: &str,
        extra_secs: Option<u64>,
    ) -> anyhow::Result<String> {
        DelegationCoordinator::resume_timed_out(self, sub_session_id, extra_secs)
    }

    fn timed_out_resumables(&self) -> Vec<crate::api::agent_lifecycle::ResumableAgent> {
        use crate::api::agent_lifecycle::ResumableAgent;
        self.timed_out_checkpoints()
            .into_iter()
            .map(|cp| ResumableAgent {
                sub_session_id: cp.sub_session_id,
                agent_name: cp.agent_name,
                started_at_rfc3339: cp.started_at.to_rfc3339(),
            })
            .collect()
    }
}
