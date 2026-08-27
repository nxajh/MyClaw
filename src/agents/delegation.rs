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

/// Delegation lifecycle status for a sub-agent task.
///
/// Consumed by the coordinator's running table and surfaced through
/// `agent_list` so the parent agent can distinguish a healthy in-flight
/// task from one killed by the wall-clock timeout or `agent_kill`.
///
/// `Idle` is reserved for the future "parked waiting for parent message"
/// mode (RFC agent-messaging §3): async sub-agents currently run to
/// completion without parking, so no live entry ever transitions to
/// `Idle` today. The variant exists so the state machine is complete and
/// callers (`agent_list`) can render it once the parked mode lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    /// Spawned into the background and processing (the only live state
    /// today — entries are removed from the running table on exit).
    Running,
    /// Parked waiting for a parent message (reserved; see enum doc).
    Idle,
    /// Finished successfully (transient — recorded in the event, the
    /// entry is removed from the running table immediately after).
    Completed,
    /// Finished with an error (transient).
    Failed,
    /// Killed by the wall-clock timeout (transient).
    TimedOut,
    /// Cancelled by the parent via `agent_kill` (transient).
    Cancelled,
    /// Persisted to disk during shutdown; the task is resumable on restart.
    /// On startup, checkpointed tasks are resumed (not marked Failed).
    Checkpointed,
}

impl DelegationStatus {
    /// Whether this status means the task is no longer executing.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DelegationStatus::Completed
                | DelegationStatus::Failed
                | DelegationStatus::TimedOut
                | DelegationStatus::Cancelled
        )
    }

    /// Snake-case string form — matches the serde representation and the
    /// `DelegationCheckpoint.status` field, so persisted statuses can be
    /// compared/written without going through serde.
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationStatus::Running => "running",
            DelegationStatus::Idle => "idle",
            DelegationStatus::Completed => "completed",
            DelegationStatus::Failed => "failed",
            DelegationStatus::TimedOut => "timed_out",
            DelegationStatus::Cancelled => "cancelled",
            DelegationStatus::Checkpointed => "checkpointed",
        }
    }
}

/// Structured error for a sub-agent killed by its wall-clock timeout.
///
/// `delegate_with_parent` returns this (wrapped in `anyhow::Error`) from
/// the timeout branch; the async wrapper downcasts it to emit
/// `DelegationEvent::TimedOut` instead of a generic `Failed`.
#[derive(Debug, thiserror::Error)]
#[error("sub-agent '{agent}' timed out after {secs}s")]
pub struct DelegationTimeout {
    pub agent: String,
    pub secs: u64,
}

/// Events sent from background sub-agents to the Orchestrator.
///
/// RFC v2 §三.C: `parent_session_id` identifies the parent session that
/// spawned the sub-agent — orchestrator routes the completion message back
/// into this session's `process_turn` so the LLM can react to the sub-agent's
/// result. `sub_session_id` is the sub-agent's own session FQID — its
/// identity and the addressing key for `agent_kill` / `send_message`.
#[derive(Debug, Clone)]
pub enum DelegationEvent {
    /// Sub-agent completed successfully.
    Completed {
        /// The sub-agent's own session FQID (identity / addressing key).
        sub_session_id: String,
        /// Parent session FQID that spawned the sub-agent.
        parent_session_id: String,
        summary: String,
        /// How long the sub-agent ran (in seconds).
        duration_secs: u64,
        /// Number of `Message` events the sub-agent delivered to its parent
        /// while running. When > 0 the summary has effectively already been
        /// streamed to the parent session, so the orchestrator can degrade
        /// the completion note to pure metadata instead of duplicating it.
        sent_message_count: u64,
    },
    /// Sub-agent failed.
    Failed {
        /// The sub-agent's own session FQID (identity / addressing key).
        sub_session_id: String,
        /// Parent session FQID that spawned the sub-agent.
        parent_session_id: String,
        error: String,
    },
    /// Sub-agent was killed by its wall-clock timeout.
    ///
    /// Distinct from `Failed`: the parent should treat the task as
    /// abandoned (possibly mid-flight) rather than finished-with-error.
    TimedOut {
        /// The sub-agent's own session FQID (identity / addressing key).
        sub_session_id: String,
        /// Parent session FQID that spawned the sub-agent.
        parent_session_id: String,
        /// The effective timeout that killed the sub-agent (seconds).
        timeout_secs: u64,
        /// How long the sub-agent ran before the timeout fired (seconds).
        duration_secs: u64,
    },
    /// Sub-agent sent a message to the parent while running in background.
    ///
    /// RFC agent-messaging §3.4/§3.6: the payload's `sub_session_id` is the
    /// **sender's own** session id (identity, so the parent can reply via
    /// `recipient`); its `parent_session_id` is the parent session the
    /// message must wake. Wrapped in a payload struct (not inline named
    /// fields) so the message type is addressable as a type in the
    /// `AgentMessenger` trait.
    Message(AgentMessage),
}

pub use crate::api::agent_mail::{AgentMail, AgentMessage, MessageKind};

/// A running async sub-agent's parent → sub mailbox.
///
/// `tx` is held by the coordinator's `mailboxes` map — the parent's
/// `send_message(recipient=sub_session_id)` routes through it. `rx` lives on
/// the sub-agent's `Session` so `Agent::run` can drain it before every LLM
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
