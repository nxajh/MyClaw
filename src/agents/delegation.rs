//! Async-delegation event type used by the orchestrator event loop.
//!
//! When `agent_delegate` is called with mode="async", the sub-agent runs in
//! a background tokio task. Completion (or failure) is reported via a
//! `DelegationEvent` sent through an mpsc channel owned by
//! `DelegationCoordinator`; the orchestrator's main event loop picks the
//! event up and routes it back into the parent session's `process_turn`.

/// Events sent from background sub-agents to the Orchestrator.
///
/// RFC v2 §三.C: `parent_session_id` (previously `session_key`) identifies
/// the parent session that spawned the sub-agent — orchestrator routes the
/// completion message back into this session's `process_turn` so the LLM
/// can react to the sub-agent's result.
#[derive(Debug, Clone)]
pub enum DelegationEvent {
    /// Sub-agent completed successfully.
    Completed {
        task_id: String,
        parent_session_id: String,
        reply_target: String,
        summary: String,
        /// How long the sub-agent ran (in seconds).
        duration_secs: u64,
    },
    /// Sub-agent failed.
    Failed {
        task_id: String,
        parent_session_id: String,
        reply_target: String,
        error: String,
    },
}
