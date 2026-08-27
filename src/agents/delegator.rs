//! `AgentDelegator` trait — interface used by `DelegateTool` to invoke sub-agents.
//!
//! Concrete impl is `DelegationCoordinator` (in scheduler/delegation module),
//! which handles workspace setup (git worktree), sub-session creation, and
//! `process_turn` on the sub-session.
//!
//! `DelegateTool` only depends on this trait, not on the coordinator type—lets
//! us swap the delegation strategy (e.g., a no-op delegator for read-only
//! agents) without touching tool code.


pub use crate::api::delegation::AgentDelegator;

pub use crate::api::agent_mail::AgentMessenger;
