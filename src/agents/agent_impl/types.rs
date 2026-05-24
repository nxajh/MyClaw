use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::providers::{ChatMessage, ChatUsage, ContentPart, StopReason, ToolCall};
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
    #[allow(dead_code)]
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

/// Extension methods for ChatMessage.
#[allow(dead_code)]
pub(super) trait ChatMessageExt {
    fn with_name(self, name: String) -> ChatMessage;
}

impl ChatMessageExt for ChatMessage {
    fn with_name(self, name: String) -> ChatMessage {
        ChatMessage {
            role: self.role,
            parts: self.parts,
            name: Some(name),
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }
    }
}
