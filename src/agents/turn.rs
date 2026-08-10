//! Per-turn execution types.
//!
//! `TurnContext` is the "already-resolved decision bundle" passed to `Agent.run()`.
//! `SessionContext.process_turn()` is the resolution boundary—it parses
//! SessionOverride > [agent] defaults > built-ins into concrete scalars, then
//! constructs a TurnContext and hands it to `Agent.run()`.
//!
//! `TurnResult` is what `Agent.run()` returns. The final text was already sent
//! via `channel.push_event` (streaming) or will be sent via `channel.send` by
//! the caller; `text` is included for logging / Collect-mode use.
//!
//! `TurnSuspension` / `SubResult` / `SubStatus` implement 方案 C (turn 挂起延续,
//! docs/turn-suspension-rfc.md): after an `agent_delegate(mode="async")` the
//! parent's turn suspends instead of ending; each sub-agent terminal event
//! resumes the parent (one `process_turn` each); the turn truly ends only when
//! every pending task has been collected. Suspension state is attached to
//! `SessionContext` and serialized for restart recovery (P1-1).

use serde::{Deserialize, Serialize};

use crate::config::agent::{PermissionMode, RunMode};
use crate::providers::{StopReason, ThinkingConfig};

/// Sub-agent terminal status recorded in a suspended turn's result list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubStatus {
    /// Sub-agent finished normally (wrapper detected success).
    Completed,
    /// Sub-agent died from a run-internal error (provider failure, tool
    /// bail, panic bubbling) — not a timeout, not a kill.
    Failed,
    /// Sub-agent was killed by its wall-clock timeout.
    TimedOut,
}

/// One collected sub-agent outcome, appended to `TurnSuspension.results`
/// in completion order. Injected into the parent's context on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubResult {
    /// The sub-agent's task id (`<ns>/t/<uuidv7>`).
    pub task_id: String,
    pub status: SubStatus,
    /// Terminal message content (summary / error / timeout notice).
    pub content: String,
    /// Sub → parent `Message` events delivered while running. When > 0 the
    /// completion note is degraded to pure metadata (no duplicate summary).
    pub sent_message_count: u64,
    /// `Message{kind: Progress}` payloads dropped during suspension — never
    /// injected into the parent context; surfaced inside this entry instead.
    pub progress: Vec<String>,
}

/// 方案 C: a parent turn suspended on pending async delegations.
///
/// Attached to `SessionContext.turn_suspension`; `pending` is registered by
/// `DelegationCoordinator::spawn_delegate_async`, terminal events (P0-2) move
/// entries into `results`, and the suspension clears when `pending` is empty
/// (P0-3). Serialized to `sessions/<sid>/suspension.json` for restart
/// recovery (P1-1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSuspension {
    /// Turn sequence that triggered the suspension (P1-1 refines this; 0
    /// placeholder until turn sequencing lands).
    pub origin_turn_seq: u64,
    /// Suspension start as unix seconds (recovery reports the gap).
    pub suspended_at: u64,
    /// Task ids of sub-agents still running.
    pub pending: Vec<String>,
    /// Collected outcomes, in completion order.
    pub results: Vec<SubResult>,
}

impl TurnSuspension {
    pub fn new(task_id: String) -> Self {
        Self {
            origin_turn_seq: 0,
            suspended_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            pending: vec![task_id],
            results: Vec::new(),
        }
    }

    /// Append a pending task (idempotent per task_id).
    pub fn add_pending(&mut self, task_id: String) {
        if !self.pending.contains(&task_id) {
            self.pending.push(task_id);
        }
    }
}

/// All decisions resolved by `SessionContext.process_turn()` for the current turn.
///
/// Everything in here is "what this turn will use"—no further resolution needed
/// inside `Agent.run()`. Channels, sessions, runtime infrastructure are accessed
/// through other parameters (`&mut Session`, `&AgentRuntime`).
pub struct TurnContext<'a> {
    /// Fully assembled system prompt: builtin sections + AGENT.md body
    /// + user profile + runtime info + skill instructions.
    pub system_prompt: &'a str,

    /// LLM model to call. `None` → Agent.run falls back to
    /// `ProviderRegistry.get_chat_provider(Capability::Chat)` (the first
    /// chat-capable provider per registration order).
    pub model_id: Option<&'a str>,

    /// Thinking / reasoning config override for this turn.
    pub thinking: Option<&'a ThinkingConfig>,

    /// Permission mode resolved as `SessionOverride > [agent].permission_mode > default`.
    pub permission_mode: PermissionMode,

    /// Run mode resolved as `SessionOverride > Interactive`.
    pub run_mode: RunMode,
}

/// Result of a single turn.
///
/// `text` is the assistant's final response. Streaming channels have already
/// received this text via `channel.push_event(TextChunk)`; non-streaming
/// channels need it sent via `channel.send`. `SessionContext.process_turn`
/// handles the channel.send—the result is also returned for logging / tests.
pub struct TurnResult {
    pub text: String,
    pub stop_reason: StopReason,
    /// When the LLM returns empty output and the turn ends abnormally, the
    /// user message is saved here so the user can retry without retyping.
    /// `SessionContext` stores this into `pending_retry` for the next turn.
    pub pending_retry: Option<String>,
    /// 方案 C: set by `SessionContext.process_turn` after `Agent::run`
    /// returns when `turn_suspension.pending` is non-empty — the turn must
    /// suspend (no user-visible reply) instead of ending. Consumed by the
    /// dispatcher (P0-3: silent resume until `pending` drains).
    pub has_pending: bool,
}
