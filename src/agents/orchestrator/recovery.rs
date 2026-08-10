//! Startup recovery: resume turns interrupted by a previous crash / SIGKILL.
//!
//! User sessions with incomplete turns are resumed through the normal turn
//! path (`dispatch_turn` → `process_turn`) so `session.channel` is reinstalled
//! and channel-scoped tools (e.g. `send_message`) stay available. Only the
//! routing key's active session is recovered; inactive sessions resume when
//! the user switches back.
//!
//! Unfinished sub-agents are recovered via `Agent::run_recovery` and the
//! terminal event is delivered through [`CompletionSink`].

use std::sync::Arc;

use super::ctx::OrchestratorCtx;
use super::delegation::route_notice;
use super::key::SessionKey;
use super::turn::ResolvedTurn;
use crate::agents::turn::{SubStatus, TurnSuspension};
use crate::agents::{
    AgentRuntime, DelegationCoordinator, DelegationEvent, SessionContext, UnfinishedSubAgent,
};

/// Where a recovered sub-agent turn's output goes.
enum CompletionSink {
    /// Sub-agent session: emit `DelegationEvent::Completed` to wake the parent.
    Delegate {
        task_id: String,
        parent_session_id: String,
        #[allow(dead_code)]
        reply_target: String,
        delegator: Option<Arc<DelegationCoordinator>>,
    },
}

impl CompletionSink {
    async fn deliver(self, text: String) {
        let CompletionSink::Delegate {
            task_id,
            parent_session_id,
            reply_target: _,
            delegator,
        } = self;
        if let Some(dm) = delegator {
            if let Some(tx) = dm.event_sender() {
                let _ = tx
                    .send(DelegationEvent::Completed {
                        task_id,
                        session_id: parent_session_id,
                        summary: text,
                        duration_secs: 0,
                        sent_message_count: 0,
                    })
                    .await;
            }
        }
    }

    /// Emit a `Failed` terminal event for a delegate sink. Called when the
    /// recovery turn itself errors.
    async fn fail(self, error: String) {
        let CompletionSink::Delegate {
            task_id,
            parent_session_id,
            delegator,
            ..
        } = self;
        if let Some(dm) = delegator {
            if let Some(tx) = dm.event_sender() {
                let _ = tx
                    .send(DelegationEvent::Failed {
                        task_id,
                        session_id: parent_session_id,
                        error,
                    })
                    .await;
            }
        }
    }
}

/// Spawn the recovery turn for one session. The LLM work runs in the
/// background so the event loop starts without blocking.
fn spawn_recovery(
    turn_tracker: super::ctx::SharedTurnTracker,
    session_ctx: Arc<SessionContext>,
    runtime: AgentRuntime,
    label: &'static str,
    id: String,
    sink: CompletionSink,
) {
    tokio::spawn(async move {
        let _guard = turn_tracker.track();
        let _turn_guard = session_ctx.turn_lock.lock().await;
        let mut session = session_ctx.session.lock().await;

        let resolved = ResolvedTurn::resolve(&session, &runtime);
        let turn_ctx = resolved.turn_context();

        match session_ctx
            .agent
            .run_recovery(&mut session, turn_ctx, &runtime)
            .await
        {
            Ok(Some(tr)) if !tr.text.is_empty() => {
                tracing::info!(id = %id, "{label}: turn completed");
                sink.deliver(tr.text).await;
            }
            Ok(_) => {
                tracing::debug!(id = %id, "{label}: no recovery needed");
            }
            Err(e) => {
                tracing::warn!(id = %id, err = %e, "{label} failed");
                // P1-3: a failed sub-agent recovery must still emit a
                // terminal event — `recover_suspension` skips pending tasks
                // "covered" by this loop, so without this the parent
                // suspension would never resolve (hang).
                sink.fail(e.to_string()).await;
            }
        }
    });
}

/// Return whether startup should dispatch recovery for this session.
fn should_recover_active_session(
    active_id: Option<&str>,
    session_id: &str,
    history: &[crate::providers::capability_chat::ChatMessage],
    suspended: bool,
) -> bool {
    active_id == Some(session_id)
        && !history.is_empty()
        && super::history_has_incomplete_turn(history)
        && !suspended
}

/// Resume all incomplete user sessions and sub-agents found on disk.
///
/// User sessions: dispatch active sessions through the normal turn path
/// (`dispatch_turn` → `process_turn`) so `session.channel` is reinstalled and
/// channel-scoped tools (e.g. `send_message`) remain available during the
/// resumed turn. Inactive sessions are left for the normal message path.
///
/// Sub-agents: resume via `run_recovery` and emit the terminal event.
pub(super) fn run_startup(ctx: &Arc<OrchestratorCtx>, unfinished: &[UnfinishedSubAgent]) {
    let sessions = Arc::clone(&ctx.sessions);
    let runtime = ctx.runtime.clone();
    let channels = ctx.channels.clone();
    let delegator = ctx.delegator.clone();
    let turn_tracker = ctx.turn_tracker.clone();
    let backend = Arc::clone(sessions.backend());

    // P1-1: sessions with a persisted non-empty suspension (daemon died while
    // a turn was suspended). These are excluded from the incomplete-turn
    // recovery below — a suspended turn waits on delegation events, not an
    // incomplete history — and recovered via `recover_suspension` instead.
    // Corrupt/unparseable suspension files are ignored here; the session's
    // own restore path warns about them.
    let suspended_ids: std::collections::HashSet<String> = sessions
        .list_all_sessions()
        .into_iter()
        .filter(|info| {
            backend
                .load_suspension(&info.id)
                .and_then(|j| serde_json::from_str::<TurnSuspension>(&j).ok())
                .is_some_and(|s| !s.pending.is_empty())
        })
        .map(|info| info.id)
        .collect();

    // Task ids (FQID) the sub-agent recovery loop below will complete: their
    // terminal events arrive through the normal wake path, so
    // `recover_suspension` must not fail them.
    let covered: std::collections::HashSet<String> =
        unfinished.iter().map(|sa| sa.task_id.clone()).collect();

    // Recover only the active session for each routing key. Inactive sessions
    // resume when the user switches back and sends a normal message.
    let mut seen_owners = std::collections::HashSet::new();
    for info in sessions.list_all_sessions() {
        let key = info.owner;
        if !seen_owners.insert(key.clone()) {
            continue;
        }
        let active_id = sessions.active_session_id(&key);
        let snap = sessions.get_or_create(&key);
        if !should_recover_active_session(
            active_id.as_deref(),
            &snap.id,
            &snap.history,
            suspended_ids.contains(&snap.id),
        ) {
            continue;
        }
        let Some(parsed) = SessionKey::parse(&key) else {
            tracing::warn!(routing_key = %key, "startup recovery: invalid routing key");
            continue;
        };
        if channels.get(&parsed.account_key()).is_none() {
            tracing::warn!(routing_key = %key, "startup recovery: channel not found");
            continue;
        }
        let synthetic = crate::channels::ChannelInboundMessage {
            id: format!("recovery:{}", snap.id),
            sender: crate::channels::MessageSender::new(parsed.sender.clone()),
            receiver: snap
                .last_message
                .as_ref()
                .map(|m| m.receiver.clone())
                .unwrap_or_else(|| crate::channels::MessageReceiver::new(parsed.sender.clone())),
            content: crate::channels::ChannelMessageContent::text(
                "[系统通知] daemon 重启，继续处理此前未完成的请求。",
            ),
            timestamp: chrono::Utc::now().timestamp() as u64,
            interruption_scope_id: None,
            silenced_override: None,
            progress_text: None,
        };
        tracing::info!(session = %key, "startup recovery: found incomplete turn, dispatching through normal turn path");
        let ctx = Arc::clone(ctx);
        tokio::spawn(async move {
            super::inbound::dispatch_turn(&ctx, &parsed, synthetic).await;
        });
    }

    for sa in unfinished {
        if sa.sub_session_id.is_empty() || sa.parent_session_id.is_empty() {
            tracing::debug!(task_id = %sa.task_id, "sub-agent recovery: skipping (no sub_session_id or parent_session_id)");
            continue;
        }

        // Build the completion sink up front so both branches (recover /
        // fail) can use it.
        let sink = CompletionSink::Delegate {
            task_id: sa.task_id.clone(),
            // The event's `session_id` must be the parent's SESSION ID
            // (wake / route_notice index by session id, not routing
            // key). E29 switched the other senders to session ids but
            // missed this one — it still passed the routing key
            // (`sa.session_key`), so the recovered task's Completed
            // event was unroutable and the covered pending entry in a
            // persisted suspension would never resolve.
            parent_session_id: sa.parent_session_id.clone(),
            reply_target: sa.reply_target.clone(),
            delegator: delegator.clone(),
        };

        // Load the sub-session by its ID — NOT via a SubAgentKey routing
        // key. The session's `owner` field is the parent's routing key
        // (e.g. "telegram:myclaw:…"), not the SubAgentKey format
        // ("main:myclaw/s/…"), so get_or_create would create a brand-new
        // empty session instead of loading the existing one. That caused
        // the history-empty check below to skip the task WITHOUT emitting
        // a terminal event, leaving the parent's suspension pending list
        // permanently stuck (turn_lock deadlock).
        let snap = sessions.get_by_id(&sa.sub_session_id);
        let needs_recovery = snap.as_ref().is_some_and(|s| {
            !s.history.is_empty() && super::history_has_incomplete_turn(&s.history)
        });

        if !needs_recovery {
            // The sub-agent session can't be recovered (not found, empty,
            // or already complete). Emit a Failed terminal event so the
            // parent's suspension clears — otherwise the pending entry
            // stays forever and deadlocks the turn_lock.
            let reason = match &snap {
                None => "daemon 重启，子代理会话未找到".to_string(),
                Some(s) if s.history.is_empty() => "daemon 重启，子代理会话历史为空".to_string(),
                Some(_) => "daemon 重启，子代理任务已完成".to_string(),
            };
            tracing::info!(
                task_id = %sa.task_id,
                agent = %sa.agent_name,
                reason = %reason,
                "sub-agent startup recovery: no incomplete turn, emitting Failed terminal event"
            );
            tokio::spawn(async move {
                sink.fail(reason).await;
            });
            continue;
        }

        tracing::info!(task_id = %sa.task_id, agent = %sa.agent_name, "sub-agent startup recovery: found incomplete turn, spawning background task");
        let session_ctx = sessions
            .load_context_by_session_id(&sa.sub_session_id)
            .unwrap_or_else(|| {
                panic!(
                    "sub-agent session {} existed in get_by_id but not load_context_by_session_id",
                    sa.sub_session_id
                )
            });
        spawn_recovery(
            turn_tracker.clone(),
            session_ctx,
            runtime.clone(),
            "sub-agent startup recovery",
            sa.task_id.clone(),
            sink,
        );
    }

    // P1-1 (RFC §5): resume persisted suspensions. Prefer the registered
    // context when the session is active (its in-memory suspension is
    // authoritative); otherwise load a temporary one (restores the state from
    // disk at construction). Each uncovered pending task is failed with a
    // "daemon restarted" note; the merged notice then routes one resume turn.
    for info in sessions.list_all_sessions() {
        if !suspended_ids.contains(&info.id) {
            continue;
        }
        let Some(session_ctx) = sessions
            .registered_context_by_session_id(&info.id)
            .or_else(|| sessions.load_context_by_session_id(&info.id))
        else {
            tracing::warn!(session = %info.id, "startup recovery: cannot load context for suspended session");
            continue;
        };
        tracing::info!(session = %info.id, "startup recovery: found persisted suspension, spawning resume");
        let ctx = Arc::clone(ctx);
        let covered = covered.clone();
        tokio::spawn(async move {
            recover_suspension(&ctx, session_ctx, &covered).await;
        });
    }
}

/// P1-1 (RFC §5): recover one persisted suspension after a daemon restart.
///
/// Pending tasks NOT covered by the sub-agent recovery loop (whose terminal
/// events arrive through the normal `wake` path) are failed with a
/// "daemon 重启，子代理中断" note — mirroring wake's terminal-event handling
/// (progress folding). The merged notice then routes one resume turn; once the
/// covered tasks' terminal events land, `pending` is empty and the final
/// resume turn is loud (RFC §3.4).
async fn recover_suspension(
    ctx: &Arc<OrchestratorCtx>,
    session_ctx: Arc<SessionContext>,
    covered: &std::collections::HashSet<String>,
) {
    let snapshot = match session_ctx.suspension_snapshot() {
        Some(s) => s,
        None => return,
    };

    // Fail every pending task the sub-agent recovery loop won't cover.
    let mut failed: Vec<String> = Vec::new();
    for task_id in snapshot.pending.iter() {
        if covered.contains(task_id) {
            continue;
        }
        let progress = snapshot
            .progress_by_task
            .get(task_id)
            .cloned()
            .unwrap_or_default();
        let mut content = format!(
            "[系统通知] 子代理后台任务中断 (task_id: {}): daemon 重启，子代理进程已终止。请重新委托该任务。",
            task_id
        );
        if !progress.is_empty() {
            content.push_str("\n\n任务过程记录：\n");
            for line in &progress {
                content.push_str(&format!("- {}\n", line));
            }
        }
        session_ctx.record_terminal(task_id.clone(), SubStatus::Failed, content.clone(), 0);
        failed.push(content);
    }

    // All pending tasks are covered by the sub-agent recovery loop — their
    // terminal events arrive naturally; nothing to synthesize here.
    if failed.is_empty() {
        return;
    }

    // Merge the failure notices (with the suspension gap) into one resume
    // turn. `pending` may still hold covered tasks, making this first resume
    // turn silent; the final loud summary comes when the last covered
    // terminal event lands.
    let mut merged = String::new();
    for content in &failed {
        merged.push_str(&format!("- {}\n", content));
    }
    let suspended_secs = chrono::Utc::now()
        .timestamp()
        .saturating_sub(snapshot.suspended_at as i64);
    let notice = format!(
        "[系统通知] daemon 重启，以下后台任务已中断（挂起时长约 {}s）：\n{}",
        suspended_secs, merged
    );
    route_notice(
        ctx,
        &session_ctx.session_id,
        notice,
        format!("recovery:{}", session_ctx.session_id),
        // 方案 C (fix v2): recovery notices never carry a system progress
        // body — process_turn falls back to the model output.
        None,
    )
    .await;
}

/// P1-4: recovery unit tests — CompletionSink terminal-event delivery
/// (session_id must be the parent SESSION id, E29) and `recover_suspension`
/// semantics (fail uncovered tasks, leave covered ones for the natural wake
/// path, all-covered no-op).
#[cfg(test)]
mod tests {
    use super::super::test_support::test_ctx;
    use super::*;
    use crate::agents::session::SessionManager;
    use crate::agents::{AgentRegistry, DelegationCoordinator};
    use crate::config::sub_agent::{AgentIsolation, SubAgentConfig};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn delegator() -> Arc<DelegationCoordinator> {
        let registry = Arc::new(AgentRegistry::from_vec(vec![SubAgentConfig {
            name: "coder".to_string(),
            system_prompt: "coding specialist".to_string(),
            tools: crate::config::filters::ToolFilter::all(),
            skills: crate::config::filters::SkillFilter::all(),
            mcp: crate::config::filters::McpFilter::all(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::Shared,
            timeout: None,
        }]));
        let manager = Arc::new(SessionManager::in_memory());
        Arc::new(DelegationCoordinator::new(
            registry,
            manager,
            PathBuf::new(),
            "test",
            3,
        ))
    }

    #[tokio::test]
    async fn delegate_sink_delivers_completed_to_parent_session() {
        let dm = delegator();
        let (tx, mut rx) = mpsc::channel(8);
        dm.set_event_sender(tx);
        let sink = CompletionSink::Delegate {
            task_id: "task-1".to_string(),
            // E29: the event's session_id must be the parent SESSION id so
            // wake / route_notice can index it.
            parent_session_id: "parent-session-1".to_string(),
            reply_target: String::new(),
            delegator: Some(dm),
        };
        sink.deliver("recovered text".to_string()).await;
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Completed {
                task_id,
                session_id,
                summary,
                duration_secs,
                sent_message_count,
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(session_id, "parent-session-1");
                assert_eq!(summary, "recovered text");
                assert_eq!(duration_secs, 0);
                assert_eq!(sent_message_count, 0);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_sink_fail_emits_failed_to_parent_session() {
        let dm = delegator();
        let (tx, mut rx) = mpsc::channel(8);
        dm.set_event_sender(tx);
        let sink = CompletionSink::Delegate {
            task_id: "task-1".to_string(),
            parent_session_id: "parent-session-1".to_string(),
            reply_target: String::new(),
            delegator: Some(dm),
        };
        sink.fail("recovery turn failed".to_string()).await;
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Failed {
                task_id,
                session_id,
                error,
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(session_id, "parent-session-1");
                assert_eq!(error, "recovery turn failed");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recover_suspension_fails_uncovered_tasks() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        sctx.add_pending_task("t2".to_string());
        sctx.add_progress("t1", "halfway report");
        let covered: HashSet<String> = ["t2".to_string()].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;

        let snap = sctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.task_id, "t1");
        assert_eq!(r.status, SubStatus::Failed);
        assert!(r.content.contains("daemon 重启"), "content: {}", r.content);
        assert!(r.content.contains("halfway report"));
        // the covered task is untouched — its terminal event arrives via wake
        assert_eq!(snap.pending, vec!["t2".to_string()]);
    }

    #[tokio::test]
    async fn recover_suspension_all_covered_is_noop() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("t1".to_string());
        sctx.add_pending_task("t2".to_string());
        let covered: HashSet<String> = ["t1".to_string(), "t2".to_string()].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;
        let snap = sctx.suspension_snapshot().unwrap();
        assert!(snap.results.is_empty());
        assert_eq!(snap.pending.len(), 2);
    }

    #[test]
    fn should_recover_active_session_true_for_active_incomplete() {
        // Session is active, history is incomplete (trailing user, no assistant
        // response yet), not suspended.
        let history = vec![
            crate::providers::capability_chat::ChatMessage::user_text("hi"),
            crate::providers::capability_chat::ChatMessage::assistant_text("hello"),
            crate::providers::capability_chat::ChatMessage::user_text("again"),
        ];
        assert!(should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &history,
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_no_active_session() {
        let history = vec![crate::providers::capability_chat::ChatMessage::user_text(
            "hi",
        )];
        assert!(!should_recover_active_session(
            None, "sess-1", &history, false
        ));
    }

    #[test]
    fn should_recover_false_when_session_not_active() {
        let history = vec![crate::providers::capability_chat::ChatMessage::user_text(
            "hi",
        )];
        // active_id differs from this session's id.
        assert!(!should_recover_active_session(
            Some("other-session"),
            "sess-1",
            &history,
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_history_empty() {
        assert!(!should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &[],
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_history_complete() {
        let history = vec![
            crate::providers::capability_chat::ChatMessage::user_text("hi"),
            crate::providers::capability_chat::ChatMessage::assistant_text("hello"),
        ];
        assert!(!should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &history,
            false,
        ));
    }

    #[test]
    fn should_recover_false_when_suspended() {
        let history = vec![crate::providers::capability_chat::ChatMessage::user_text(
            "hi",
        )];
        assert!(!should_recover_active_session(
            Some("sess-1"),
            "sess-1",
            &history,
            true,
        ));
    }
}
