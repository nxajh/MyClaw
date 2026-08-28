//! `impl AgentMessenger` — the agent-to-agent message bus (parent ↔ sub).

use crate::agents::delegation::{AgentMail, AgentMessage, DelegationEvent, MessageKind};

use super::DelegationCoordinator;

/// Agent-to-agent message bus (RFC agent-messaging §3).
///
/// `send_to_sub_agent` routes through the per-task inbox registered by
/// `spawn_delegate_async`; `send_to_parent` reuses the `DelegationEvent`
/// channel so the orchestrator wakes the parent session exactly like a
/// completion event (queued behind the turn lock — never preempting).
#[async_trait::async_trait]
impl crate::agents::AgentMessenger for DelegationCoordinator {
    fn send_to_sub_agent(&self, sub_session_id: &str, mail: AgentMail) -> Result<(), String> {
        match self.mailboxes.get(sub_session_id) {
            Some(tx) => tx
                .try_send(mail)
                .map_err(|e| format!("消息投递失败（子代理收件箱已满或已关闭）：{}", e)),
            None => Err(format!(
                "sub_session_id '{}' 不存在或子代理已结束（仅 async 子代理可接收消息）",
                sub_session_id
            )),
        }
    }

    async fn send_to_parent(&self, event: AgentMessage) -> bool {
        // Sync delegations have no running-table entry (the parent is blocked
        // inside the tool call awaiting the result), so there is no notice
        // consumer on the event channel: the message must ride back in the
        // tool result instead of a delayed notice. Record the final text for
        // the sync completion branch and skip the broadcast — the
        // `[子代理消息]` injection path is async-only (2026-08-14, B).
        if self.running.get(&event.sub_session_id).is_none() {
            if event.kind != MessageKind::Progress {
                if let Some(msgs) = self.sent_messages.get(&event.sub_session_id) {
                    msgs.write().unwrap().push(event.text.clone());
                } else {
                    self.sent_messages.insert(
                        event.sub_session_id.clone(),
                        std::sync::RwLock::new(vec![event.text.clone()]),
                    );
                }
            }
            return true;
        }
        match self.event_sender() {
            Some(tx) => {
                let sub_session_id = event.sub_session_id.clone();
                let delivered = tx.send(DelegationEvent::Message(event)).await.is_ok();
                if delivered {
                    // Bump the per-task message counter so the completion
                    // wrapper can de-duplicate the summary (④).
                    if let Some(entry) = self.running.get(&sub_session_id) {
                        entry
                            .messages_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                delivered
            }
            None => {
                tracing::warn!(
                    sub_session_id = %event.sub_session_id,
                    "cannot deliver sub-agent message: delegation event channel not wired"
                );
                false
            }
        }
    }
}
