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

use tokio::sync::{Mutex, mpsc};

use crate::agents::tokens::estimate_tokens;

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

/// A running async sub-agent's parent → sub mailbox.
///
/// `tx` is held by the coordinator's `mailboxes` map — the parent's
/// `send_message(recipient=task_id)` routes through it. `rx` lives on the
/// sub-agent's `Session` so `Agent::run` can drain it before every LLM
/// request. `rx` is mutex-wrapped to keep `Session` cheaply cloneable
/// (snapshots share the same mailbox handle).
pub struct SubAgentMailbox {
    /// Sender half — the parent's `send_message` sends `AgentMail` here.
    pub tx: mpsc::Sender<AgentMail>,
    /// Receiver half — drained by `Agent::run` per LLM request.
    pub rx: Mutex<mpsc::Receiver<AgentMail>>,
}

/// Per-round injection budget for a sub-agent's inbox batch, in tokens.
///
/// RFC agent-messaging §3.7: a flood of parent messages must not blow up
/// the sub-agent's context. Only the newest messages that fit the budget
/// are injected; older ones are re-queued for a later round — never
/// dropped (§3.4 消息不丢) and never truncated.
pub const INJECTION_BUDGET_TOKENS: u64 = 2048;

/// Split a drained inbox batch into the newest messages that fit the
/// per-round injection budget and the older remainder.
///
/// `mails` is in arrival order (oldest first). Returns `(kept, deferred)`
/// where `kept` are the newest complete messages that fit the budget
/// (arrival order preserved) and `deferred` are the older ones, to be
/// re-queued for a later round. At least the newest message is always
/// kept, even if it alone exceeds the budget — individual messages are
/// never truncated.
pub fn select_within_injection_budget(
    mut mails: Vec<AgentMail>,
) -> (Vec<AgentMail>, Vec<AgentMail>) {
    let mut kept_start = mails.len();
    let mut total: u64 = 0;
    for i in (0..mails.len()).rev() {
        // Cost = rendered line + inter-message separator, via the same
        // CJK-aware estimator the context engine uses.
        let cost = estimate_tokens(&render_mail_line(&mails[i])) + 1;
        if total + cost > INJECTION_BUDGET_TOKENS && kept_start < mails.len() {
            break;
        }
        total += cost;
        kept_start = i;
    }
    let kept = mails.split_off(kept_start);
    (kept, mails)
}

/// Render a single inbox message as one reminder bullet.
fn render_mail_line(mail: &AgentMail) -> String {
    format!(
        "- [来自 {}] {}\n{}",
        mail.sender_name,
        format_timestamp(mail.timestamp),
        mail.text,
    )
}

/// Render a batch of inbox messages as a `<system-reminder>` user message.
///
/// Style mirrors `AttachmentManager` reminders so the model treats it as
/// injected context, not as a new user turn. Callers are expected to pass
/// the budget-selected subset from
/// [`select_within_injection_budget`].
pub fn render_agent_mail_reminder(mails: &[AgentMail]) -> String {
    let mut lines = Vec::new();
    for mail in mails {
        lines.push(render_mail_line(mail));
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

    fn long_mail(i: u64, tag: &str) -> AgentMail {
        AgentMail {
            msg_id: format!("m{i}"),
            sender_name: "主 agent".into(),
            text: format!("{tag}{}", "长".repeat(1500)),
            timestamp: i,
        }
    }

    #[test]
    fn select_within_budget_keeps_all_when_under_budget() {
        let mails = vec![
            AgentMail {
                msg_id: "m1".into(),
                sender_name: "主 agent".into(),
                text: "短消息一".into(),
                timestamp: 0,
            },
            AgentMail {
                msg_id: "m2".into(),
                sender_name: "主 agent".into(),
                text: "短消息二".into(),
                timestamp: 1,
            },
        ];
        let (kept, deferred) = select_within_injection_budget(mails);
        assert_eq!(kept.len(), 2);
        assert!(deferred.is_empty());
    }

    #[test]
    fn select_within_budget_defers_oldest_when_over_budget() {
        // Each message ≈ 1500 CJK chars ≈ 1500 tokens; three blow the
        // 2048-token budget, so only the newest survives this round and
        // the older two are deferred (never dropped).
        let mails = vec![
            long_mail(1, "第一"),
            long_mail(2, "第二"),
            long_mail(3, "第三"),
        ];
        let (kept, deferred) = select_within_injection_budget(mails);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].text.starts_with("第三"));
        assert_eq!(deferred.len(), 2);
        assert!(deferred[0].text.starts_with("第一"));
        assert!(deferred[1].text.starts_with("第二"));
    }

    #[test]
    fn select_within_budget_never_truncates_single_message() {
        // A single message larger than the whole budget is still kept
        // intact — individual messages are never truncated (§3.7).
        let mails = vec![AgentMail {
            msg_id: "m1".into(),
            sender_name: "主 agent".into(),
            text: "超".repeat(5000),
            timestamp: 0,
        }];
        let (kept, deferred) = select_within_injection_budget(mails);
        assert_eq!(kept.len(), 1);
        assert!(deferred.is_empty());
        assert_eq!(kept[0].text.chars().count(), 5000);
    }
}
