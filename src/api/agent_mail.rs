//! agent_mail — agent-to-agent mail contracts (RFC agent-messaging §3), L0.
//!
//! Pure message data ([`AgentMail`], [`AgentMessage`], [`MessageKind`]) plus
//! the [`AgentMessenger`] bus trait shared between the `send_message` tool
//! and the agents runtime (`DelegationCoordinator`). Sunk from
//! `agents::delegation` / `agents::delegator` in #151 phase8+ so the tools
//! layer can hold the contract without reaching into L4; the old
//! the old `agents::` re-export paths stay alive (compat).

use async_trait::async_trait;

/// Kind of a sub-agent → parent message (turn-suspension RFC §2.3).
///
/// Defaults to `Final` so every existing sender keeps today's semantics
/// (mid-flight messages wake the parent); `Progress` is the new opt-in
/// kind for sub-agents that want to report progress without interrupting
/// the parent's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageKind {
    /// Ordinary sub-agent → parent message (default; today's behavior —
    /// injected as `[子代理消息]`).
    #[default]
    Final,
    /// Mid-flight progress report — never injected into the parent context
    /// (suspended or not); folded into `SubResult.progress` when the
    /// suspension collects the task's terminal event.
    Progress,
}

/// A sub-agent → parent message (RFC agent-messaging §3.4/§3.6).
#[derive(Debug, Clone)]
pub struct AgentMessage {
    /// Unique message id (observability / dedup).
    pub msg_id: String,
    /// Display name of the sending sub-agent (its agent name).
    pub sender_name: String,
    /// The sub-agent's own session FQID (identity — NOT the recipient).
    pub sub_session_id: String,
    /// Parent session FQID that spawned the sub-agent.
    pub parent_session_id: String,
    pub text: String,
    /// `Final` (default) wakes/injects; `Progress` is suppressed (§2.3).
    pub kind: MessageKind,
}

/// A parent → sub message sitting in a sub-agent's inbox.
///
/// Delivered via an mpsc channel so the parent's `send_message` tool never
/// needs to touch the sub-agent's locked `Session`. `Agent::run` drains the
/// inbox before every LLM request and renders the batch as a
/// `<system-reminder>`; anything still queued when the sub-agent finishes
/// is drained and attached to its result so it is never silently lost.
#[derive(Debug, Clone)]
pub struct AgentMail {
    /// Unique message id (observability).
    pub msg_id: String,
    /// Display name of the sender (the parent agent's name).
    pub sender_name: String,
    pub text: String,
    /// Unix timestamp (seconds).
    pub timestamp: u64,
}

/// Agent-to-agent message bus used by the `send_message` tool
/// (RFC agent-messaging §3).
///
/// Concrete impl is `DelegationCoordinator` (multi-agent mode only). In
/// single-agent mode the tool runs without a messenger and `recipient`
/// targeting errors out.
#[async_trait]
pub trait AgentMessenger: Send + Sync {
    /// Parent → sub: deliver a message to a running async sub-agent's inbox.
    ///
    /// Returns `Err` with a user-facing message when the session id is
    /// unknown (never spawned, already finished, or sync-only).
    fn send_to_sub_agent(&self, sub_session_id: &str, mail: AgentMail) -> Result<(), String>;

    /// Sub → parent: emit a `DelegationEvent::Message` to wake the parent
    /// agent. Returns `false` when the event channel is not wired.
    async fn send_to_parent(&self, event: AgentMessage) -> bool;
}
