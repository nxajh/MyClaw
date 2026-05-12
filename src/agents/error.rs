//! Structured error types for the agent domain boundary.
//!
//! These cover the failure modes that callers may want to distinguish.
//! All variants implement `Into<anyhow::Error>` so existing `?` sites
//! continue to work without change.

/// Errors produced by `AgentLoop::run` / `AgentLoop::run_streamed`.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The loop-breaker aborted the turn due to a repetitive tool pattern.
    #[error("loop breaker triggered: {reason}")]
    LoopBreak { reason: String },

    /// The per-turn tool call hard limit was exceeded.
    #[error("tool call limit reached ({limit})")]
    ToolLimitReached { limit: usize },

    /// The LLM stream produced no data within the configured timeout.
    #[error("stream chunk timeout after {secs}s")]
    StreamTimeout { secs: u64 },

    /// The streaming client disconnected before the turn completed.
    #[error("client disconnected during stream")]
    ClientDisconnected,

    /// A tool execution failure that the model cannot recover from.
    #[error("tool '{name}' failed: {source}")]
    ToolFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    /// An LLM provider error (network, auth, rate-limit, etc.).
    #[error("provider error: {0}")]
    Provider(#[from] anyhow::Error),

    /// All internal retries for a retryable provider error were exhausted.
    /// The orchestrator should offer retry/abort buttons to the user.
    #[error("retries exhausted after {attempts} attempts: {source}")]
    RetryExhausted {
        attempts: usize,
        source: anyhow::Error,
    },

    /// The LLM returned an empty response after all retries.
    /// The turn's user message has been rolled back from history.
    #[error("empty response after retries")]
    EmptyResponse {
        /// The original user message that was rolled back.
        user_message: String,
    },
}
