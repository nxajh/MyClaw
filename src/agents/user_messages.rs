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

/// Map a failed-turn error to a user-facing notice that reflects the *kind* of
/// failure, instead of always showing the generic timeout text.
///
/// Prefers the typed [`ProviderHttpError`](crate::providers::ProviderHttpError)
/// (carries the real HTTP status) and falls back to substring heuristics on the
/// error message for connection/timeout/chain-exhausted cases.
pub fn user_facing_error_message(err: &anyhow::Error) -> String {
    use crate::agents::error::AgentError;
    use crate::providers::ProviderHttpError;
    use crate::providers::fallback::{CHAIN_ALL_COOLING_TAG, CHAIN_EXHAUSTED_TAG};

    // Typed agent errors — matched structurally, not via string heuristics.
    if let Some(agent_err) = err.downcast_ref::<AgentError>() {
        if let AgentError::LoopBreak { .. } = agent_err {
            return "⚠️ 检测到重复操作，已自动中断。如需继续，请发送新消息。".to_string();
        }
    }

    if let Some(http) = err.downcast_ref::<ProviderHttpError>() {
        let too_long = http.message.contains("too long")
            || http.message.contains("maximum context")
            || http.message.contains("过长");
        return match http.status {
            500 | 502 | 503 | 504 => "⚠️ 模型服务暂时不可用，请稍后重试。",
            429 => "⚠️ 请求过于频繁，请稍候再试。",
            413 => "⚠️ 消息内容过长，请精简后重试。",
            400 if too_long => "⚠️ 消息内容过长，请精简后重试。",
            400 => "⚠️ 请求无法处理（格式错误）。",
            _ => "⚠️ 模型调用失败，请稍后重试。",
        }
        .to_string();
    }

    let s = err.to_string();
    let msg = if s.contains(CHAIN_EXHAUSTED_TAG) || s.contains(CHAIN_ALL_COOLING_TAG) {
        "⚠️ 多个模型均暂时不可用，请稍后重试。"
    } else if s.contains("too long") || s.contains("message is too long") {
        "⚠️ 消息内容过长，请精简后重试。"
    } else if s.contains("timeout")
        || s.contains("stalled")
        || s.contains("truncated")
        || s.contains("no completion marker")
    {
        "⚠️ 模型响应超时，请重试。"
    } else if s.contains("error sending request")
        || s.contains("connection")
        || s.contains("broken pipe")
        || s.contains("reset")
    {
        "⚠️ 网络连接中断，请重试。"
    } else {
        MSG_TURN_FAILED
    };
    msg.to_string()
}

/// Prompt shown when a previous turn was left incomplete (e.g. restart).
pub const MSG_INCOMPLETE_TURN: &str =
    "⚠️ 检测到上次请求未处理完成（可能是服务重启）。\n\n请选择重试或放弃。";

/// Inline-button label: retry the incomplete/empty turn.
pub const BTN_RETRY: &str = "🔄 重试";

/// Inline-button label: abandon the incomplete/empty turn.
pub const BTN_ABORT: &str = "✖ 放弃";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_5xx_maps_to_service_unavailable() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 502,
            message: "Bad Gateway".into(),
        }
        .into();
        assert!(user_facing_error_message(&e).contains("服务暂时不可用"));
    }

    #[test]
    fn http_400_too_long_maps_to_too_long() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 400,
            message: "Bad Request: message is too long".into(),
        }
        .into();
        assert!(user_facing_error_message(&e).contains("过长"));
    }

    #[test]
    fn chain_exhausted_maps_to_multi_model() {
        let e = anyhow::anyhow!(
            "stream error: {}: all providers failed",
            crate::providers::fallback::CHAIN_EXHAUSTED_TAG
        );
        assert!(user_facing_error_message(&e).contains("多个模型"));
    }

    #[test]
    fn connection_drop_maps_to_network() {
        let e = anyhow::anyhow!("stream error: error sending request for url");
        assert!(user_facing_error_message(&e).contains("网络"));
    }

    #[test]
    fn unknown_falls_back_to_generic() {
        let e = anyhow::anyhow!("some unexpected internal state");
        assert_eq!(user_facing_error_message(&e), MSG_TURN_FAILED);
    }

    #[test]
    fn loop_break_exact_repeat_maps_to_loop_message() {
        let e: anyhow::Error = crate::agents::AgentError::LoopBreak {
            reason: crate::agents::LoopBreakReason::ExactRepeat {
                tool: "shell".into(),
                count: 3,
                threshold: 3,
            },
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("自动中断"), "got: {msg}");
    }

    #[test]
    fn loop_break_max_calls_maps_to_loop_message() {
        let e: anyhow::Error = crate::agents::AgentError::LoopBreak {
            reason: crate::agents::LoopBreakReason::MaxCalls {
                count: 200,
                limit: 200,
            },
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("自动中断"), "got: {msg}");
    }
}
