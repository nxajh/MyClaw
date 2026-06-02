//! User-facing message strings emitted by the orchestrator.
//!
//! Centralized here so presentation text is not interleaved with
//! orchestration logic. These are the bot's own status/control messages
//! (retry/abort prompts, timeout notices) — distinct from LLM output.
//!
//! The bot's primary audience is Chinese-speaking, so the strings are
//! authored in Chinese rather than behind a full i18n framework (which
//! would be over-engineering for this handful of constants). If
//! per-language output is needed later, this module is the single seam to
//! introduce a `fn for_lang(lang: &str) -> &'static str` lookup without
//! touching callers.

/// Shown when a retry callback fires but no message is pending retry.
pub const MSG_NO_PENDING_RETRY: &str = "没有待重试的消息，请重新发送。";

/// Acknowledgement after the user aborts a pending retry.
pub const MSG_ABORT_ACK: &str = "已取消";

/// Shown when a turn produced no model reply within the timeout.
pub const MSG_TURN_FAILED: &str = "⚠️ 处理超时，未收到模型回复。";

/// Prompt shown when a previous turn was left incomplete (e.g. restart).
pub const MSG_INCOMPLETE_TURN: &str =
    "⚠️ 检测到上次请求未处理完成（可能是服务重启）。\n\n请选择重试或放弃。";

/// Inline-button label: retry the incomplete/empty turn.
pub const BTN_RETRY: &str = "🔄 重试";

/// Inline-button label: abandon the incomplete/empty turn.
pub const BTN_ABORT: &str = "✖ 放弃";
