use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::providers::{ChatUsage, StopReason, ToolCall};
use super::super::TurnEvent;

// Re-export token tracking types from session module.
pub(crate) use super::super::session::{TokenTracker, estimate_tokens, estimate_message_tokens};

// ── StreamMode ──────────────────────────────────────────────────────────────

/// Determines how the LLM stream is consumed inside `chat_loop`.
///
/// - `Collect`: silently collect into a `CollectedResponse` (existing `run()` behavior).
/// - `Streamed`: forward events via mpsc + support cancellation (for `run_streamed()`).
#[derive(Clone)]
pub(crate) enum StreamMode {
    Collect,
    Streamed {
        event_tx: mpsc::Sender<TurnEvent>,
        cancel: CancellationToken,
    },
}

/// Response collected from a chat stream.
pub(crate) struct CollectedResponse {
    pub(crate) text: String,
    pub(crate) reasoning_content: Option<String>,
    /// Anthropic-issued opaque signature for the thinking block, required when
    /// echoing the block back in subsequent turns.
    pub(crate) thinking_signature: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) stop_reason: StopReason,
    pub(crate) usage: Option<ChatUsage>,
}

/// Returns true for tools that can mutate system state and are therefore
/// blocked in `AutonomyLevel::ReadOnly` mode.
pub(crate) fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "shell"
            | "file_write"
            | "file_edit"
            | "file_delete"
            | "agent_delegate"
            | "agent_kill"
            | "http"
    )
}

// ── Extension trait for ChatMessage ──────────────────────────────────────────

