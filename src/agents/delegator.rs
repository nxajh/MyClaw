//! `AgentDelegator` trait — interface used by `DelegateTool` to invoke sub-agents.
//!
//! Concrete impl is `DelegationCoordinator` (in scheduler/delegation module),
//! which handles workspace setup (git worktree), sub-session creation, and
//! `process_turn` on the sub-session.
//!
//! `DelegateTool` only depends on this trait, not on the coordinator type—lets
//! us swap the delegation strategy (e.g., a no-op delegator for read-only
//! agents) without touching tool code.

use async_trait::async_trait;

use crate::agents::delegation::{AgentMail, AgentMessage};
use crate::agents::session::Session;

/// Invokes sub-agents on demand.
#[async_trait]
pub trait AgentDelegator: Send + Sync {
    /// Run the named sub-agent on `task`, returning its summary text.
    ///
    /// `parent_session` provides channel / reply_target / sender via
    /// `parent.channel` and `parent.last_message`—the sub-session inherits
    /// these so its streaming events and ask_user calls land on the same UI.
    async fn delegate(
        &self,
        agent_name: &str,
        task: &str,
        parent_session: &Session,
        timeout: Option<u64>,
        allowed_tools: Option<Vec<String>>,
        workspace: Option<&str>,
    ) -> anyhow::Result<String>;

    /// Spawn the sub-agent in the background and return its `session_id`
    /// immediately.
    ///
    /// Completion or failure is reported asynchronously via `DelegationEvent`.
    /// The default implementation returns an error (sync-only delegators).
    fn delegate_async(
        &self,
        _agent_name: &str,
        _task: &str,
        _parent_session: &Session,
        _timeout: Option<u64>,
        _allowed_tools: Option<Vec<String>>,
        _workspace: Option<&str>,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("async delegation not supported"))
    }

    /// List sub-agents the `DelegateTool` may target. Used to construct the
    /// tool's JSON schema (the `agent` parameter's enum).
    ///
    /// Returns `(name, description)` pairs. `description` is the
    /// AGENT.md front-matter description (Markdown stripped).
    fn list_available(&self) -> Vec<(String, Option<String>)>;
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
