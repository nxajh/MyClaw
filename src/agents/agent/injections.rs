//! Per-request transient injections (extracted verbatim from the former
//! `agent/mod.rs` run_inner, batch 3 of the module split).
//!
//! Two shapes live here, both "inject as consumption" (transient
//! `<system-reminder>` user messages not persisted to history):
//!
//! - [`Agent::reinject_after_compaction`] — post-compaction re-injection:
//!   re-run the diff-based attachment snapshots (skills / agents / date /
//!   autonomy / memory / draft-reminder) against the freshly-compacted
//!   history and insert the rebuilt reminder if anything is missing.
//! - [`Agent::inject_per_round`] — per-LLM-round injections: delegation
//!   deadline reminder, sub-agent inbox drain (with injection budget), and
//!   per-turn injections stashed on the session by `dispatch_turn`.

use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability_chat::{ChatMessage, ToolSpec};

impl super::Agent {
    /// Post-compaction re-injection guard. Compaction summarizes away old
    /// `<system-reminder>` blocks; re-run the attachment diffs and insert
    /// the rebuilt snapshot as a transient user message when non-empty.
    ///
    /// Extracted verbatim from `run_inner` (former `agent/mod.rs` lines
    /// 248–330, batch 3). Returns the message list to use for this round:
    /// the freshly-compacted history when a compaction pass fired
    /// (possibly carrying the re-injected reminder), or the input
    /// unchanged when no compaction was needed.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn reinject_after_compaction(
        &self,
        session: &mut Session,
        messages: Vec<ChatMessage>,
        runtime: &crate::agents::AgentRuntime,
        turn_ctx: &crate::agents::turn::TurnContext<'_>,
        provider: &Arc<dyn crate::providers::capability_chat::ChatProvider>,
        model_id: &str,
        tool_specs: &[ToolSpec],
    ) -> Vec<ChatMessage> {
        let mut messages = messages;
        let context = &runtime.context_engine;
        let permission_mode = turn_ctx.permission_mode;
        // Pre-send compaction guard: compact BEFORE the request when the
        // history we're about to send is over threshold. Driven by a direct
        // history estimate (not the token tracker) so a stale/under-counted
        // tracker can't let an over-window request through. This is the real
        // fix for the "974 msgs sent at 31k tracked → context overflow" bug.
        // Loops until under threshold so a history far over the window
        // converges within this turn rather than one chunk per user turn.
        if let Some(compacted) = context
            .compact_until_fit(
                session,
                turn_ctx.system_prompt,
                model_id,
                Arc::clone(provider),
                tool_specs,
                runtime.task_boards.as_ref(),
                false,
            )
            .await
        {
            messages = compacted;

            // Post-compaction re-injection: compaction summarizes away old
            // <system-reminder> blocks (skills/memory/date/autonomy/agents).
            // The agent loop bypasses process_turn, so the diff-based
            // attachment injection in session_context.rs never runs. Re-run
            // the diffs here against the freshly-compacted history; any
            // missing reminders are injected as transient messages (not
            // persisted to history, exactly like sub-agent inbox below).
            let reminder = {
                let skills_snap = runtime.skills.read();
                let history_clone = session.history.clone();
                // RFC #101 P1: pass the FQID (users/{uuid}), not the routing key.
                let owner = session.owner_fqid.clone();
                session
                    .attachments
                    .diff_skills(&skills_snap, &history_clone, Some(owner.as_str()));
                let agent_list: Vec<(String, String)> = runtime
                    .agents
                    .values_cloned()
                    .into_iter()
                    .map(|a| {
                        (
                            a.config.name.clone(),
                            a.config.description.clone().unwrap_or_default(),
                        )
                    })
                    .collect();
                session.attachments.diff_agents(&agent_list, &history_clone);
                session
                    .attachments
                    .diff_date(runtime.context_engine.timezone_offset(), &history_clone);
                session
                    .attachments
                    .diff_autonomy(&permission_mode, &history_clone);
                // Daily throttled draft-skill backlog reminder (issue #89,
                // layer ②) — best-effort, never blocks the turn.
                // #101 P2: per-layer accounting — the user-layer backlog
                // belongs to this session's owner; the agent-layer backlog
                // surfaces only in the operator's sessions
                // (runtime.defaults.operator = [system] operator
                // normalized to a bare uuid at daemon assembly).
                let is_operator = !session.owner_fqid.is_empty()
                    && runtime
                        .defaults
                        .operator
                        .as_deref()
                        .is_some_and(|op| crate::ids::bare_dir_name(&session.owner_fqid) == op);
                if let Some(backlog) = crate::agents::skill_draft_reminder::check_and_arm(
                    std::path::Path::new(&runtime.defaults.prompt.base_dir),
                    runtime.context_engine.timezone_offset(),
                    &session.owner_fqid,
                    is_operator,
                ) {
                    session
                        .attachments
                        .push_skill_draft_reminder(backlog.user_layer, backlog.agent_layer);
                }
                let memory_root = &runtime.defaults.prompt.memory_root;
                let memory_entries: Vec<crate::memory::IndexEntry> = if !memory_root.is_empty() {
                    let memory_dir = std::path::Path::new(memory_root);
                    let files = crate::memory::scan_memory_files(memory_dir);
                    files.iter().map(crate::memory::IndexEntry::from).collect()
                } else {
                    Vec::new()
                };
                session
                    .attachments
                    .diff_memory(&memory_entries, &history_clone);
                let text = session.attachments.build_text(&skills_snap);
                session.attachments.clear_pending();
                text
            };
            if let Some(reminder_text) = reminder {
                tracing::info!(
                    session = %session.id,
                    "injected system-reminder snapshot after compaction"
                );
                let snapshot_msg = ChatMessage::user_text(reminder_text);
                if messages.len() > 1 {
                    messages.insert(1, snapshot_msg);
                } else {
                    messages.push(snapshot_msg);
                }
            }
        }
        messages
    }

    /// Per-LLM-round injections: delegation-deadline wall-clock reminder,
    /// sub-agent inbox drain (bounded by the per-round injection budget),
    /// and per-turn injections rendered by `dispatch_turn`. Injected as
    /// transient `<system-reminder>` user messages (consumption — not
    /// persisted to history).
    ///
    /// Extracted verbatim from `run_inner` (former `agent/mod.rs` lines
    /// 332–398, batch 3).
    pub(super) async fn inject_per_round(
        &self,
        session: &mut Session,
        messages: &mut Vec<ChatMessage>,
    ) {
        // Time-awareness: sub-agent sessions carry a wall-clock kill
        // deadline. Inject the remaining budget as a transient
        // `<system-reminder>` before every LLM request (not persisted —
        // same consumption model as the inbox below) so the sub-agent
        // can pace itself and, at ≤20% remaining, is told to wrap up
        // instead of being killed mid-flight with nothing delivered.
        if let Some(deadline) = session.delegation_deadline {
            let remaining = deadline.remaining_secs();
            if remaining > 0 {
                messages.push(ChatMessage::user_text(deadline.render_reminder()));
            }
        }

        // RFC agent-messaging §3.4/§3.7: drain the sub-agent inbox before
        // this LLM request so parent → sub messages are visible on the
        // next tool round. Injected as a `<system-reminder>` user message
        // (not persisted to history — the tool-loop alternation stays
        // clean and injection is consumption). Placement is deliberately
        // AFTER compaction so a compaction pass cannot drop the batch.
        // §3.7: if the batch exceeds the per-round budget, only the
        // newest complete messages are injected and the older remainder
        // is re-queued for a later round (never dropped, never truncated).
        if let Some(mailbox) = &session.sub_agent_inbox {
            let mut pending = Vec::new();
            {
                let mut rx = mailbox.rx.lock().await;
                while let Ok(mail) = rx.try_recv() {
                    pending.push(mail);
                }
            }
            if !pending.is_empty() {
                let (kept, deferred) =
                    crate::agents::delegation::select_within_injection_budget(pending);
                if !deferred.is_empty() {
                    tracing::warn!(
                        session = %session.id,
                        deferred = deferred.len(),
                        "inbox batch over injection budget; deferring older messages to a later round"
                    );
                    for mail in deferred {
                        let _ = mailbox.tx.send(mail).await;
                    }
                }
                if !kept.is_empty() {
                    tracing::info!(
                        session = %session.id,
                        count = kept.len(),
                        "injecting sub-agent inbox messages before LLM request"
                    );
                    messages.push(ChatMessage::user_text(
                        crate::agents::delegation::render_agent_mail_reminder(&kept),
                    ));
                }
            }
        }

        // RFC §3.5/§4.3: per-turn injections (user-level mailbox +
        // pending friend requests), rendered by `dispatch_turn` and
        // stashed on the session. 注入即消费 — injected into this first
        // LLM request then cleared; pending requests are re-rendered
        // every turn by `dispatch_turn` while they remain.
        if !session.turn_injections.is_empty() {
            let injections = std::mem::take(&mut session.turn_injections);
            for text in injections {
                messages.push(ChatMessage::user_text(text));
            }
        }
    }
}
