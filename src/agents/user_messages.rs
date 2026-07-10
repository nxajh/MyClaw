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
/// classified via [`ClassifiedError`](crate::providers::ClassifiedError) so that
/// 429 bodies such as Codex `usage_limit_reached` / `model_cooldown` produce
/// distinct copy (not a single "请求过于频繁").
pub fn user_facing_error_message(err: &anyhow::Error) -> String {
    use crate::agents::error::AgentError;
    use crate::providers::fallback::{CHAIN_ALL_COOLING_TAG, CHAIN_EXHAUSTED_TAG};
    use crate::providers::{ClassifiedError, ErrorCategory, ProviderHttpError};

    // Typed agent errors — matched structurally, not via string heuristics.
    if let Some(AgentError::LoopBreak { .. }) = err.downcast_ref::<AgentError>() {
        return "⚠️ 检测到重复操作，已自动中断。如需继续，请发送新消息。".to_string();
    }

    if let Some(http) = err.downcast_ref::<ProviderHttpError>() {
        let classified = ClassifiedError::classify("", http.status, &http.message);
        return message_for_classified(&classified);
    }

    let s = err.to_string();
    let msg = if s.contains(CHAIN_EXHAUSTED_TAG) || s.contains(CHAIN_ALL_COOLING_TAG) {
        "⚠️ 多个模型均暂时不可用，请稍后重试或使用 /model 指定可用模型。"
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
        // Last resort: try classifying the raw string (status 0 → Timeout path).
        let classified = ClassifiedError::from_message(&s);
        if !matches!(classified.category, ErrorCategory::Timeout)
            || s.contains("rate")
            || s.contains("quota")
            || s.contains("429")
        {
            // only use if it found something more specific than generic timeout
            if !matches!(classified.category, ErrorCategory::Timeout) {
                return message_for_classified(&classified);
            }
        }
        MSG_TURN_FAILED
    };
    msg.to_string()
}

fn message_for_classified(c: &crate::providers::ClassifiedError) -> String {
    use crate::providers::{ErrorCategory, format_cooldown_zh};

    match c.category {
        ErrorCategory::Billing => {
            "⚠️ 当前模型额度已用尽。请使用 /model 切换其他模型，或 /model off 恢复默认路由。"
                .to_string()
        }
        ErrorCategory::RateLimit if c.is_long_cooldown() => {
            let wait = c
                .cooldown_duration()
                .map(format_cooldown_zh)
                .unwrap_or_else(|| "一段时间".to_string());
            format!(
                "⚠️ 模型冷却中，{wait}后可用。请使用 /model 切换，或 /model off 恢复默认路由。"
            )
        }
        ErrorCategory::RateLimit => {
            let secs = c
                .cooldown_duration()
                .map(|d| d.as_secs().max(1))
                .unwrap_or(60);
            format!("⚠️ 请求过于频繁，请约 {secs} 秒后再试。")
        }
        ErrorCategory::Overloaded | ErrorCategory::ServerError => {
            "⚠️ 模型服务暂时不可用，请稍后重试。".to_string()
        }
        ErrorCategory::Timeout => "⚠️ 模型响应超时，请重试。".to_string(),
        ErrorCategory::PayloadTooLarge | ErrorCategory::ContextOverflow => {
            "⚠️ 消息内容过长，请精简后重试。".to_string()
        }
        ErrorCategory::Auth | ErrorCategory::AuthPermanent => {
            let invalid_key = c.message.contains("Invalid API Key")
                || c.message.contains("invalid_key")
                || c.message.contains("invalid_api_key")
                || c.message.contains("Unauthorized");
            if invalid_key {
                "⚠️ 模型 API 密钥已失效，请联系管理员更新。".to_string()
            } else if c.status_code == Some(403) {
                "⚠️ 模型额度不足或无权访问，请联系管理员。".to_string()
            } else {
                "⚠️ 模型认证失败，请联系管理员。".to_string()
            }
        }
        ErrorCategory::ModelNotFound => {
            "⚠️ 指定模型不可用。请使用 /model 切换，或 /model off 恢复默认路由。".to_string()
        }
        ErrorCategory::FormatError => {
            let too_long = c.message.contains("too long")
                || c.message.contains("maximum context")
                || c.message.contains("过长");
            if too_long {
                "⚠️ 消息内容过长，请精简后重试。".to_string()
            } else {
                "⚠️ 请求无法处理（格式错误）。".to_string()
            }
        }
    }
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
    fn http_401_invalid_key_maps_to_auth_failure() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 401,
            message: "Invalid API Key".into(),
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("密钥"), "got: {msg}");
    }

    #[test]
    fn http_403_maps_to_billing() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 403,
            message: "Forbidden".into(),
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("额度") || msg.contains("无权"), "got: {msg}");
    }

    #[test]
    fn http_429_usage_limit_maps_to_quota_not_rate() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 429,
            message: r#"{"error":{"type":"usage_limit_reached","plan_type":"free"}}"#.into(),
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("额度"), "got: {msg}");
        assert!(!msg.contains("过于频繁"), "got: {msg}");
        assert!(msg.contains("/model"), "got: {msg}");
    }

    #[test]
    fn http_429_model_cooldown_maps_to_cooldown_copy() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 429,
            message: r#"{"error":{"type":"model_cooldown","resets_in_seconds":2534400}}"#.into(),
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("冷却"), "got: {msg}");
        assert!(msg.contains("/model"), "got: {msg}");
        assert!(!msg.contains("过于频繁"), "got: {msg}");
    }

    #[test]
    fn http_429_short_rate_limit_maps_to_frequent() {
        let e: anyhow::Error = crate::providers::ProviderHttpError {
            status: 429,
            message: r#"{"error":{"retry_after":5}}"#.into(),
        }
        .into();
        let msg = user_facing_error_message(&e);
        assert!(msg.contains("过于频繁"), "got: {msg}");
        assert!(msg.contains("5"), "got: {msg}");
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
