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
use super::delegation::{drain_delegation_notices, process_non_active, route_notice};
use super::key::SessionKey;
use super::turn::ResolvedTurn;
use crate::agents::turn::{SubStatus, TurnSuspension};
use crate::agents::{
    AgentRuntime, DelegationCoordinator, DelegationEvent, DelegationNotice, SessionContext,
    UnfinishedSubAgent,
};

/// Where a recovered sub-agent turn's output goes.
enum CompletionSink {
    /// Sub-agent session: emit `DelegationEvent::Completed` to wake the parent.
    Delegate {
        sub_session_id: String,
        parent_session_id: String,
        #[allow(dead_code)]
        reply_target: String,
        delegator: Option<Arc<DelegationCoordinator>>,
    },
}

impl CompletionSink {
    #[allow(dead_code)]
    async fn deliver(self, text: String) {
        let CompletionSink::Delegate {
            sub_session_id,
            parent_session_id,
            reply_target: _,
            delegator,
        } = self;
        if let Some(dm) = delegator {
            if let Some(tx) = dm.event_sender() {
                let _ = tx
                    .send(DelegationEvent::Completed {
                        sub_session_id,
                        parent_session_id,
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
            sub_session_id,
            parent_session_id,
            delegator,
            ..
        } = self;
        if let Some(dm) = delegator {
            if let Some(tx) = dm.event_sender() {
                let _ = tx
                    .send(DelegationEvent::Failed {
                        sub_session_id,
                        parent_session_id,
                        error,
                    })
                    .await;
            }
        }
    }
}

/// Spawn the recovery turn for one session. The LLM work runs in the
/// background so the event loop starts without blocking.
#[allow(dead_code)]
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
    let _runtime = ctx.runtime.clone();
    let channels = ctx.channels.clone();
    let delegator = ctx.delegator.clone();
    let _turn_tracker = ctx.turn_tracker.clone();
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

    // Sub-session ids (FQID) the sub-agent recovery loop below will complete:
    // their terminal events arrive through the normal wake path, so
    // `recover_suspension` must not fail them.
    let covered: std::collections::HashSet<String> =
        unfinished.iter().map(|sa| sa.sub_session_id.clone()).collect();

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
        };
        tracing::info!(session = %key, "startup recovery: found incomplete turn, dispatching through normal turn path");
        let ctx = Arc::clone(ctx);
        tokio::spawn(async move {
            super::inbound::dispatch_turn(&ctx, &parsed, synthetic).await;
        });
    }

    for sa in unfinished {
        if sa.sub_session_id.is_empty() || sa.parent_session_id.is_empty() {
            tracing::debug!(session_id = %sa.sub_session_id, "sub-agent recovery: skipping (no sub_session_id or parent_session_id)");
            continue;
        }

        // Build the completion sink up front so both branches (recover /
        // fail) can use it.
        let sink = CompletionSink::Delegate {
            sub_session_id: sa.sub_session_id.clone(),
            // The event's `parent_session_id` must be the parent's SESSION ID
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
                session_id = %sa.sub_session_id,
                agent = %sa.agent_name,
                reason = %reason,
                "sub-agent startup recovery: no incomplete turn, emitting Failed terminal event"
            );
            tokio::spawn(async move {
                sink.fail(reason).await;
            });
            continue;
        }

        tracing::info!(session_id = %sa.sub_session_id, agent = %sa.agent_name, "sub-agent startup recovery: found incomplete turn, spawning background task");
        let session_ctx = sessions
            .load_context_by_session_id(&sa.sub_session_id)
            .unwrap_or_else(|| {
                panic!(
                    "sub-agent session {} existed in get_by_id but not load_context_by_session_id",
                    sa.sub_session_id
                )
            });
        if let Some(ref delegator) = delegator {
            delegator.recover_async(
                sa.sub_session_id.clone(),
                sa.agent_name.clone(),
                sa.parent_session_id.clone(),
                session_ctx,
                sa.timeout_secs,
                sa.allowed_tools.clone(),
            );
        } else {
            tokio::spawn(async move {
                sink.fail("daemon restarts, sub-agents disabled".to_string()).await;
            });
        }
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

    // P2 (2026-08-13, RFC delegation-notice-queue §5): re-deliver persisted
    // completion notices (crash between wake-persist and turn-delivery). Runs
    // AFTER suspension recovery so active-session lookups see the same
    // registrations the suspension loop just materialized.
    recover_completion_queue(ctx);
}

/// P2 (2026-08-13, RFC delegation-notice-queue §5): re-deliver completion
/// notices persisted before a crash. Entries written by `route_notice` are
/// re-enqueued into their parent session's notice queue (active sessions get
/// an immediate drain; non-active ones take the `process_non_active` path,
/// which marks the entry delivered once the turn persists its content).
/// Entries whose parent session no longer exists are dead-lettered (marked
/// delivered / dropped) with a warning — there is nothing left to deliver to.
fn recover_completion_queue(ctx: &Arc<OrchestratorCtx>) {
    let Some(store) = ctx.completion_queue.clone() else {
        // Degraded (P1 in-memory delivery) or tests — nothing to recover.
        return;
    };
    let pending = store.pending();
    if pending.is_empty() {
        return;
    }
    tracing::info!(count = pending.len(), "recovering persisted completion notices");

    for entry in pending {
        let session_id = entry.parent_session_id.clone();

        // Dead-letter: the parent session vanished — drop the entry.
        let Some(session) = ctx.sessions.get_by_id(&session_id) else {
            tracing::warn!(
                session_id = %session_id,
                notice_id = %entry.id,
                "completion queue: parent session not found; dead-lettering notice"
            );
            if let Err(e) = store.mark_delivered(&entry.id) {
                tracing::warn!(notice_id = %entry.id, err = %e, "completion queue: dead-letter mark failed");
            }
            continue;
        };

        // The entry's routing key (owner) decides activity + channel.
        let Some(key) = SessionKey::parse(&session.owner) else {
            tracing::warn!(
                session_id = %session_id,
                owner = %session.owner,
                "completion queue: invalid routing key; dead-lettering notice"
            );
            if let Err(e) = store.mark_delivered(&entry.id) {
                tracing::warn!(notice_id = %entry.id, err = %e, "completion queue: dead-letter mark failed");
            }
            continue;
        };

        let is_active = ctx
            .sessions
            .active_session_id(&session.owner)
            .is_some_and(|id| id == session_id);

        if is_active && ctx.channel(&key.account_key()).is_some() {
            // Active: re-enqueue into the registered context's notice queue
            // and drain immediately (mirrors `route_notice`'s active path).
            let Some(sctx) = ctx.sessions.registered_context_by_session_id(&session_id) else {
                // Materialization race (session switched away mid-startup) —
                // fall back to the non-active path.
                let ctx = Arc::clone(ctx);
                let sid = session_id.clone();
                let id = entry.id.clone();
                let content = entry.content.clone();
                let silenced = entry.silenced_override;
                tokio::spawn(async move {
                    process_non_active(&ctx, &sid, &content, silenced, Some(id)).await;
                });
                continue;
            };
            sctx.enqueue_delegation_notice(DelegationNotice {
                id: entry.id,
                content: entry.content,
                silenced_override: entry.silenced_override,
            });
            let ctx = Arc::clone(ctx);
            let sid = session_id.clone();
            tokio::spawn(async move {
                drain_delegation_notices(&ctx, &sid).await;
            });
        } else {
            // Non-active (or channel missing): process via the non-active
            // path with the persisted id so a successful turn marks the
            // entry delivered.
            let ctx = Arc::clone(ctx);
            let sid = session_id.clone();
            let id = entry.id.clone();
            let content = entry.content.clone();
            let silenced = entry.silenced_override;
            tokio::spawn(async move {
                process_non_active(&ctx, &sid, &content, silenced, Some(id)).await;
            });
        }
    }
}

/// P1-1 (RFC §5): recover one persisted suspension after a daemon restart.
///
/// Pending sub-sessions NOT covered by the sub-agent recovery loop (whose
/// terminal events arrive through the normal `wake` path) are failed with a
/// "daemon 重启，子代理中断" note — mirroring wake's terminal-event handling
/// (progress folding). The merged notice then routes one resume turn; once the
/// covered sub-sessions' terminal events land, `pending` is empty and the final
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

    // Fail every pending sub-session the sub-agent recovery loop won't cover.
    let mut failed: Vec<String> = Vec::new();
    for sub_session_id in snapshot.pending.iter() {
        if covered.contains(sub_session_id) {
            continue;
        }
        let progress = snapshot
            .progress_by_sub_session
            .get(sub_session_id)
            .cloned()
            .unwrap_or_default();
        let mut content = format!(
            "[系统通知] 子代理后台任务中断 (session_id: {}): daemon 重启，子代理进程已终止。请重新委托该任务。",
            sub_session_id
        );
        if !progress.is_empty() {
            content.push_str("\n\n任务过程记录：\n");
            for line in &progress {
                content.push_str(&format!("- {}\n", line));
            }
        }
        session_ctx.record_terminal(sub_session_id.clone(), SubStatus::Failed, content.clone(), 0);
        failed.push(content);
    }

    // All pending sub-sessions are covered by the sub-agent recovery loop —
    // their terminal events arrive naturally; nothing to synthesize here.
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
        // P2: recovery-synthesized notices are NOT persisted (no NoticeMeta)
        // — the store only tracks wake-time delegation events, and the
        // `recovery:` id would never be marked delivered anyway.
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

    /// P2 (2026-08-13, RFC delegation-notice-queue §5): entries whose parent
    /// session vanished are dead-lettered (dropped) — no re-delivery loop, no
    /// crash, and the file is removed.
    #[test]
    fn recovery_dead_letters_vanished_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::storage::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        store
            .append(crate::storage::CompletionNoticeEntry {
                seq: 0,
                id: "delegation:t1".to_string(),
                sub_session_id: "t1".to_string(),
                parent_session_id: "ghost-session".to_string(),
                status: Some("completed".to_string()),
                content: "notice".to_string(),
                silenced_override: None,
                sent_message_count: 0,
                enqueued_at: 0,
                delivery_state: crate::storage::DeliveryState::Pending,
            })
            .unwrap();
        let mut ctx = test_ctx(vec![]);
        ctx.completion_queue = Some(Arc::clone(&store));
        recover_completion_queue(&Arc::new(ctx));
        assert!(
            store.pending().is_empty(),
            "dead-lettered entry must be dropped"
        );
        assert!(
            !tmp.path().join("queue").join("1.json").exists(),
            "dead-lettered file must be removed"
        );
    }

    /// P2: an entry whose parent session EXISTS is re-delivered through the
    /// non-active path (spawned; no channel in the ctx → not the active
    /// branch). NullRegistry makes the turn fail, so the entry stays Pending —
    /// at-least-once semantics preserved (re-tried on the next restart).
    #[tokio::test]
    async fn recovery_keeps_pending_when_turn_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::storage::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
        );
        let mut ctx = test_ctx(vec![]);
        ctx.completion_queue = Some(Arc::clone(&store));
        // Create the parent session (get_or_create auto-activates; the
        // missing channel routes recovery through the non-active path).
        let session = ctx.sessions.get_or_create("mock:default:u1");
        let sid = session.id.clone();
        store
            .append(crate::storage::CompletionNoticeEntry {
                seq: 0,
                id: "delegation:t1".to_string(),
                sub_session_id: "t1".to_string(),
                parent_session_id: sid,
                status: Some("completed".to_string()),
                content: "notice".to_string(),
                silenced_override: None,
                sent_message_count: 0,
                enqueued_at: 0,
                delivery_state: crate::storage::DeliveryState::Pending,
            })
            .unwrap();
        recover_completion_queue(&Arc::new(ctx));
        assert_eq!(
            store.pending().len(),
            1,
            "failed turn must keep the entry Pending (at-least-once)"
        );
    }

    #[tokio::test]
    async fn delegate_sink_delivers_completed_to_parent_session() {
        let dm = delegator();
        let (tx, mut rx) = mpsc::channel(8);
        dm.set_event_sender(tx);
        let sink = CompletionSink::Delegate {
            sub_session_id: "sub-session-1".to_string(),
            // E29: the event's parent_session_id must be the parent SESSION id
            // so wake / route_notice can index it.
            parent_session_id: "parent-session-1".to_string(),
            reply_target: String::new(),
            delegator: Some(dm),
        };
        sink.deliver("recovered text".to_string()).await;
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Completed {
                sub_session_id,
                parent_session_id,
                summary,
                duration_secs,
                sent_message_count,
            } => {
                assert_eq!(sub_session_id, "sub-session-1");
                assert_eq!(parent_session_id, "parent-session-1");
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
            sub_session_id: "sub-session-1".to_string(),
            parent_session_id: "parent-session-1".to_string(),
            reply_target: String::new(),
            delegator: Some(dm),
        };
        sink.fail("recovery turn failed".to_string()).await;
        let ev = rx.recv().await.unwrap();
        match ev {
            DelegationEvent::Failed {
                sub_session_id,
                parent_session_id,
                error,
            } => {
                assert_eq!(sub_session_id, "sub-session-1");
                assert_eq!(parent_session_id, "parent-session-1");
                assert_eq!(error, "recovery turn failed");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recover_suspension_fails_uncovered_tasks() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("s1".to_string());
        sctx.add_pending_task("s2".to_string());
        sctx.add_progress("s1", "halfway report");
        let covered: HashSet<String> = ["s2".to_string()].into_iter().collect();
        recover_suspension(&ctx, sctx.clone(), &covered).await;

        let snap = sctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.sub_session_id, "s1");
        assert_eq!(r.status, SubStatus::Failed);
        assert!(r.content.contains("daemon 重启"), "content: {}", r.content);
        assert!(r.content.contains("halfway report"));
        // the covered sub-session is untouched — its terminal event arrives via wake
        assert_eq!(snap.pending, vec!["s2".to_string()]);
    }

    #[tokio::test]
    async fn recover_suspension_all_covered_is_noop() {
        let ctx = Arc::new(test_ctx(vec![]));
        let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
        sctx.add_pending_task("s1".to_string());
        sctx.add_pending_task("s2".to_string());
        let covered: HashSet<String> = ["s1".to_string(), "s2".to_string()].into_iter().collect();
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
