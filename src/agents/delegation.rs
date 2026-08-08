//! Async-delegation event type used by the orchestrator event loop.
//!
//! When `agent_delegate` is called with mode="async", the sub-agent runs in
//! a background tokio task. Completion (or failure) is reported via a
//! `DelegationEvent` sent through an mpsc channel owned by
//! `DelegationCoordinator`; the orchestrator's main event loop picks the
//! event up and routes it back into the parent session's `process_turn`.
//!
//! ## Agent-to-agent messaging (RFC agent-messaging §3)
//!
//! - Parent → sub: `AgentMail` delivered through the sub-agent's inbox
//!   (an mpsc receiver installed on `Session.sub_agent_inbox` while the
//!   sub-agent runs). `Agent::run` drains it before each LLM request and
//!   injects the messages as a `<system-reminder>` — visible on the next
//!   tool round, consumed on injection.
//! - Sub → parent: `DelegationEvent::Message` over the same mpsc channel
//!   used by `Completed`/`Failed`. The orchestrator's `wake` routes it
//!   into the parent session (queued behind the turn lock — never
//!   preempting an in-flight turn).

/// Events sent from background sub-agents to the Orchestrator.
///
/// RFC v2 §三.C: `session_id` identifies the parent session that spawned
/// the sub-agent — orchestrator routes the completion message back into
/// this session's `process_turn` so the LLM can react to the sub-agent's
/// result.
#[derive(Debug, Clone)]
pub enum DelegationEvent {
    /// Sub-agent completed successfully.
    Completed {
        task_id: String,
        /// Hex session ID of the parent session (NOT a routing key).
        session_id: String,
        summary: String,
        /// How long the sub-agent ran (in seconds).
        duration_secs: u64,
    },
    /// Sub-agent failed.
    Failed {
        task_id: String,
        /// Hex session ID of the parent session (NOT a routing key).
        session_id: String,
        error: String,
    },
    /// Sub-agent sent a message to the parent while running in background.
    ///
    /// RFC agent-messaging §3.4/§3.6: the payload's `task_id` is the
    /// **sender's own** task id (identity, so the parent can reply via
    /// `recipient`); its `session_id` is the parent session the message
    /// must wake. Wrapped in a payload struct (not inline named fields) so
    /// the message type is addressable as a type in the `AgentMessenger`
    /// trait.
    Message(AgentMessage),
}

/// A sub-agent → parent message (RFC agent-messaging §3.4/§3.6).
#[derive(Debug, Clone)]
pub struct AgentMessage {
    /// Unique message id (observability / dedup).
    pub msg_id: String,
    /// Display name of the sending sub-agent (its agent name).
    pub sender_name: String,
    /// The sub-agent's own task_id (identity — NOT the recipient).
    pub task_id: String,
    /// Hex session ID of the parent session (NOT a routing key).
    pub session_id: String,
    pub text: String,
}

/// A parent → sub message sitting in a sub-agent's inbox.
///
/// Delivered via an mpsc channel so the parent's `send_message` tool never
/// needs to touch the sub-agent's locked `Session`. `Agent::run` drains the
/// inbox before every LLM request and renders the batch as a
/// `<system-reminder>`; anything still queued when the sub-agent finishes
/// is drained and attached to its result so it is never silently lost.
#[derive(Debug, Clone)]
pub struct AgentMail {
    /// Unique message id (observability).
    pub msg_id: String,
    /// Display name of the sender (the parent agent's name).
    pub sender_name: String,
    pub text: String,
    /// Unix timestamp (seconds).
    pub timestamp: u64,
}

/// Render a batch of inbox messages as a `<system-reminder>` user message.
///
/// Style mirrors `AttachmentManager` reminders so the model treats it as
/// injected context, not as a new user turn.
pub fn render_agent_mail_reminder(mails: &[AgentMail]) -> String {
    let mut lines = Vec::new();
    for mail in mails {
        lines.push(format!(
            "- [来自 {}] {}\n{}",
            mail.sender_name,
            format_timestamp(mail.timestamp),
            mail.text,
        ));
    }
    format!(
        "<system-reminder>\n[来自主 agent 的消息，共 {} 条。请处理这些消息并继续你的任务。]\n{}\n</system-reminder>",
        mails.len(),
        lines.join("\n\n"),
    )
}

fn format_timestamp(ts: u64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts as i64, 0);
    match dt {
        Some(dt) => dt.format("%H:%M:%S").to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_agent_mail_reminder_lists_each_message() {
        let mails = vec![
            AgentMail {
                msg_id: "m1".into(),
                sender_name: "主 agent".into(),
                text: "继续调研，注意 clippy".into(),
                timestamp: 0,
            },
            AgentMail {
                msg_id: "m2".into(),
                sender_name: "主 agent".into(),
                text: "完成后汇报".into(),
                timestamp: 1,
            },
        ];
        let rendered = render_agent_mail_reminder(&mails);
        assert!(rendered.starts_with("<system-reminder>"));
        assert!(rendered.ends_with("</system-reminder>"));
        assert!(rendered.contains("[来自主 agent 的消息，共 2 条"));
        assert!(rendered.contains("[来自 主 agent]"));
        assert!(rendered.contains("继续调研，注意 clippy"));
        assert!(rendered.contains("完成后汇报"));
    }

    #[test]
    fn render_agent_mail_reminder_empty_batch() {
        let rendered = render_agent_mail_reminder(&[]);
        assert!(rendered.contains("<system-reminder>"));
        assert!(rendered.contains("共 0 条"));
    }
}
