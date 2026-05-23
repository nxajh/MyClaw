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

use crate::config::agent::{PermissionMode, RunMode};
use crate::providers::{StopReason, ThinkingConfig};

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
}
