use super::super::test_support::{test_ctx, MockChannel};
use super::*;
use crate::agents::session_context::TerminalRecord;
use crate::agents::{AgentMessage, DelegationNotice, SessionContext};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// #144 `notice_receiver`: three input states — no last message, a
/// last message polluted with an EMPTY receiver id (pre-#144 synthetic
/// turns wrote these), and a healthy last message.
#[test]
fn notice_receiver_falls_back_to_sender_when_last_message_missing() {
    let r = notice_receiver(None, "u1");
    assert_eq!(r.id, "u1");
}

#[test]
fn notice_receiver_self_heals_empty_receiver_pollution() {
    let polluted = crate::api::message::PersistedChannelMessage {
        id: "m1".to_string(),
        sender_id: "u1".to_string(),
        receiver: crate::api::message::MessageReceiver::new(String::new()),
        text: "x".to_string(),
        timestamp: 0,
        interruption_scope_id: None,
    };
    // Some("") must NOT win over the sender fallback — a plain
    // `unwrap_or_else` would keep the empty id and reproduce the bug.
    let r = notice_receiver(Some(&polluted), "u1");
    assert_eq!(r.id, "u1");
}

#[test]
fn notice_receiver_preserves_healthy_last_message_receiver() {
    let healthy = crate::api::message::PersistedChannelMessage {
        id: "m1".to_string(),
        sender_id: "u1".to_string(),
        receiver: crate::api::message::MessageReceiver::new("chat-42".to_string()),
        text: "x".to_string(),
        timestamp: 0,
        interruption_scope_id: None,
    };
    let r = notice_receiver(Some(&healthy), "u1");
    assert_eq!(r.id, "chat-42");
}

/// #144 `notice_fallback_sender`: derives the sender from a routing key;
/// unparsable keys degrade to "system" instead of an empty string.
#[test]
fn notice_fallback_sender_parses_routing_key() {
    assert_eq!(notice_fallback_sender("telegram:default:alice"), "alice");
}

#[test]
fn notice_fallback_sender_degrades_on_invalid_key() {
    assert_eq!(notice_fallback_sender("no-colons-here"), "system");
}

/// A session registered in `ctx` with one pending task "t1".
fn suspended_session(ctx: &OrchestratorCtx) -> Arc<SessionContext> {
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());
    sctx
}

/// P2 (2026-08-13, RFC delegation-notice-queue §5.2): a wake for an
/// ACTIVE session (auto-activated by `get_or_create_context`) with a
/// channel present persists the notice BEFORE it enters the queue. The
/// test runtime's NullRegistry makes the notice turn fail, so the entry
/// stays Pending — exactly the at-least-once state a crash would leave
/// (recovery re-enqueues it on the next start).
#[tokio::test]
async fn active_wake_persists_notice_before_enqueue() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        crate::agents::orchestrator::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
    );
    let channel: Arc<dyn crate::api::message::Channel> = MockChannel::new();
    let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
    ctx.completion_queue = Some(Arc::clone(&store));
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid.clone(),
            summary: "persisted summary".to_string(),
            duration_secs: 5,
            sent_message_count: 0,
        },
    )
    .await;
    let pending = store.pending();
    assert_eq!(pending.len(), 1, "notice must be persisted before enqueue");
    let e = &pending[0];
    assert_eq!(e.id, "delegation:t1");
    assert_eq!(e.sub_session_id, "t1");
    assert_eq!(e.parent_session_id, sid);
    assert_eq!(e.status.as_deref(), Some("completed"));
    assert_eq!(e.sent_message_count, 0);
    // The just-collected terminal cleared `pending` — wake-time silence
    // intent is false (see route_notice race-fix comment).
    assert_eq!(e.silenced_override, Some(false));
    assert!(e.content.contains("persisted summary"));
    assert_eq!(e.delivery_state, crate::agents::orchestrator::DeliveryState::Pending);
}

/// issue #129/#140: `route_shell_completion` reuses `route_notice`
/// wholesale for a shell background completion — this session never
/// called `add_pending_task` for this process_id, so `record_terminal`
/// resolves `NoSuspension` and the notice still routes/persists as an
/// ordinary notice (falls through, does not drop it — see the function's
/// doc comment). Persists to `completion_queue` the same way a
/// delegation notice does, with the shell command's `process_id` riding
/// in the `sub_session_id` slot as an opaque id (not a real sub-session).
#[tokio::test]
async fn route_shell_completion_persists_before_enqueue() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        crate::agents::orchestrator::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
    );
    let channel: Arc<dyn crate::api::message::Channel> = MockChannel::new();
    let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
    ctx.completion_queue = Some(Arc::clone(&store));
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    let sid = sctx.session_id.clone();

    route_shell_completion(
        &ctx,
        crate::tools::shell::ShellCompletion {
            session_id: sid.clone(),
            process_id: "sh_abc123".to_string(),
            content: "[系统通知] 后台命令已完成 (process_id: sh_abc123, exit_code: 0)。"
                .to_string(),
            exit_code: Some(0),
        },
    )
    .await;

    let pending = store.pending();
    assert_eq!(pending.len(), 1, "shell completion must persist before enqueue");
    let e = &pending[0];
    assert_eq!(e.id, "shell:sh_abc123");
    assert_eq!(e.sub_session_id, "sh_abc123");
    assert_eq!(e.parent_session_id, sid);
    assert_eq!(e.status, Some("completed".to_string()));
    assert!(e.content.contains("sh_abc123"));
}

/// issue #142 review (xiaoer-bot): an adopted orphan's completion
/// carries `exit_code: None` (we aren't its real parent, so the real
/// exit code is unobservable) — this must map to Completed, not Failed,
/// or the persisted status contradicts the notice's own "已完成" content.
#[tokio::test]
async fn shell_completion_with_unknown_exit_code_maps_to_completed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        crate::agents::orchestrator::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
    );
    let channel: Arc<dyn crate::api::message::Channel> = MockChannel::new();
    let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
    ctx.completion_queue = Some(Arc::clone(&store));
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    let sid = sctx.session_id.clone();

    route_shell_completion(
        &ctx,
        crate::tools::shell::ShellCompletion {
            session_id: sid,
            process_id: "sh_orphan".to_string(),
            content: "[系统通知] 后台命令已完成 (process_id: sh_orphan, exit_code: null)。"
                .to_string(),
            exit_code: None,
        },
    )
    .await;

    let pending = store.pending();
    let e = pending.iter().find(|e| e.id == "shell:sh_orphan").unwrap();
    assert_eq!(
        e.status,
        Some("completed".to_string()),
        "unknown exit code must not contradict the notice's own 已完成 content"
    );
}

/// issue #140: the core value of pending-unification — two shell
/// background processes registered as pending on the SAME session
/// (mirrors `ShellTool::register_pending`'s `add_pending_task` calls).
/// The first completion, with the second still pending, must route as a
/// SILENCED intermediate notice (wake-time intent captured synchronously
/// right after `record_terminal`, exactly like a delegation terminal
/// event); the second (last pending item) must be loud/final.
#[tokio::test]
async fn concurrent_shell_completions_silence_intermediate_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        crate::agents::orchestrator::CompletionNoticeStore::open(tmp.path().join("queue")).unwrap(),
    );
    let channel: Arc<dyn crate::api::message::Channel> = MockChannel::new();
    let mut ctx = test_ctx(vec![(("mock".to_string(), "default".to_string()), channel)]);
    ctx.completion_queue = Some(Arc::clone(&store));
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    let sid = sctx.session_id.clone();
    sctx.add_pending_task("sh_a".to_string());
    sctx.add_pending_task("sh_b".to_string());

    route_shell_completion(
        &ctx,
        crate::tools::shell::ShellCompletion {
            session_id: sid.clone(),
            process_id: "sh_a".to_string(),
            content: "a done".to_string(),
            exit_code: Some(0),
        },
    )
    .await;

    let after_a = store.pending();
    let a_entry = after_a
        .iter()
        .find(|e| e.id == "shell:sh_a")
        .expect("sh_a notice persisted");
    assert_eq!(
        a_entry.silenced_override,
        Some(true),
        "sh_b still pending — sh_a's notice must be silenced (intermediate)"
    );
    assert_eq!(sctx.suspension_snapshot().unwrap().pending, vec!["sh_b".to_string()]);

    route_shell_completion(
        &ctx,
        crate::tools::shell::ShellCompletion {
            session_id: sid,
            process_id: "sh_b".to_string(),
            content: "b done".to_string(),
            exit_code: Some(1),
        },
    )
    .await;

    let after_b = store.pending();
    let b_entry = after_b
        .iter()
        .find(|e| e.id == "shell:sh_b")
        .expect("sh_b notice persisted");
    assert_eq!(
        b_entry.silenced_override,
        Some(false),
        "sh_b is the last pending item — its notice must be loud (final)"
    );
    assert_eq!(b_entry.status, Some("failed".to_string()));
    assert!(sctx.suspension_snapshot().unwrap().pending.is_empty());
}

#[tokio::test]
async fn completed_collects_result_with_summary() {
    let ctx = test_ctx(vec![]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            summary: "the final summary".to_string(),
            duration_secs: 7,
            sent_message_count: 0,
        },
    )
    .await;
    let snap = sctx.suspension_snapshot().unwrap();
    assert_eq!(snap.results.len(), 1);
    let r = &snap.results[0];
    assert_eq!(r.status, SubStatus::Completed);
    assert!(r.content.contains("the final summary"));
    assert_eq!(r.sent_message_count, 0);
    assert!(snap.pending.is_empty());
}

#[tokio::test]
async fn completed_with_messages_degrades_summary() {
    let ctx = test_ctx(vec![]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            summary: "duplicate summary".to_string(),
            duration_secs: 3,
            sent_message_count: 3,
        },
    )
    .await;
    let snap = sctx.suspension_snapshot().unwrap();
    let r = &snap.results[0];
    assert!(!r.content.contains("duplicate summary"));
    assert!(r.content.contains("实时同步"));
    assert_eq!(r.sent_message_count, 3);
}

#[tokio::test]
async fn failed_collects_error() {
    let ctx = test_ctx(vec![]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Failed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            error: "provider exploded".to_string(),
        },
    )
    .await;
    let snap = sctx.suspension_snapshot().unwrap();
    let r = &snap.results[0];
    assert_eq!(r.status, SubStatus::Failed);
    assert!(r.content.contains("provider exploded"));
}

#[tokio::test]
async fn timed_out_collects_timeout_note() {
    let ctx = test_ctx(vec![]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::TimedOut {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            timeout_secs: 600,
            duration_secs: 600,
        },
    )
    .await;
    let snap = sctx.suspension_snapshot().unwrap();
    let r = &snap.results[0];
    assert_eq!(r.status, SubStatus::TimedOut);
    assert!(r.content.contains("超时"));
}

#[tokio::test]
async fn progress_message_is_suppressed_not_waking() {
    let ctx = test_ctx(vec![]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Message(AgentMessage {
            msg_id: "m1".to_string(),
            sender_name: "coder".to_string(),
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            text: "working on it".to_string(),
            kind: MessageKind::Progress,
        }),
    )
    .await;
    let snap = sctx.suspension_snapshot().unwrap();
    assert!(snap.results.is_empty());
    assert_eq!(snap.pending, vec!["t1".to_string()]);
    assert_eq!(
        snap.progress_by_sub_session.get("t1").unwrap(),
        &vec!["working on it".to_string()]
    );
}

#[tokio::test]
async fn progress_folds_into_terminal_result() {
    let ctx = test_ctx(vec![]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Message(AgentMessage {
            msg_id: "m1".to_string(),
            sender_name: "coder".to_string(),
            sub_session_id: "t1".to_string(),
            parent_session_id: sid.clone(),
            text: "working on it".to_string(),
            kind: MessageKind::Progress,
        }),
    )
    .await;
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            summary: "done".to_string(),
            duration_secs: 5,
            sent_message_count: 0,
        },
    )
    .await;
    let snap = sctx.suspension_snapshot().unwrap();
    let r = &snap.results[0];
    assert_eq!(r.progress, vec!["working on it".to_string()]);
    assert!(r.content.contains("done"));
    assert!(snap.pending.is_empty());
}

#[tokio::test]
async fn unknown_session_event_is_a_noop() {
    let ctx = test_ctx(vec![]);
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "ghost".to_string(),
            parent_session_id: "no-such-session".to_string(),
            summary: "x".to_string(),
            duration_secs: 0,
            sent_message_count: 0,
        },
    )
    .await;
    wake(
        &ctx,
        DelegationEvent::Failed {
            sub_session_id: "ghost".to_string(),
            parent_session_id: "no-such-session".to_string(),
            error: "x".to_string(),
        },
    )
    .await;
    // no panic; nothing further to assert
}

#[tokio::test]
async fn silence_guidance_appended_while_pending_remains() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());
    sctx.add_pending_task("t2".to_string());
    let mut content = "[系统通知] 子代理已完成后台任务 (session_id: t1)".to_string();
    maybe_append_silence_guidance(&sctx, &mut content);
    assert!(content.contains("中间恢复轮"));
    // 异步委派通知改造 (2026-08-13): 删掉"不会终结/不得输出最终结论"
    // 硬约束(死锁根源), 改为 sessions_yield 引导 + 绝不轮询.
    assert!(content.contains("将作为进度说明展示给用户"));
    assert!(content.contains("sessions_yield"));
    assert!(content.contains("结束当前轮"));
    assert!(content.contains("完成时会自动唤醒你并把结果作为下一条消息注入"));
    assert!(content.contains("绝不轮询"));
    assert!(!content.contains("不得输出最终结论"));
    assert!(!content.contains("不会终结"));
}

/// Regression test for issue #224: `record_terminal`'s duplicate check only
/// works while `turn_suspension` still exists — the `s.results` scan lives
/// inside the `Some(s)` branch. Once cleared (routine, as soon as every
/// pending task is collected), a repeat call for the same sub_session_id
/// used to fall through to `NoSuspension` instead of `Duplicate` — and
/// `route_shell_completion` deliberately treats `NoSuspension` as "route it
/// anyway" (#129: every notify-armed process gets a notice), turning a
/// repeat call into a repeat delivery to the user.
#[tokio::test]
async fn record_terminal_dedupes_after_suspension_clears() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());

    let first = sctx.record_terminal("t1".into(), SubStatus::Completed, "done".into(), 0);
    assert!(
        matches!(first, TerminalRecord::Recorded(_)),
        "expected Recorded, got {first:?}"
    );

    sctx.clear_suspension_if_collected();
    assert!(
        sctx.suspension_snapshot().is_none(),
        "suspension must be cleared once its only pending task is collected"
    );

    // A repeat call for the same id, after suspension clears, must now be
    // recognized as a duplicate — not fall through as NoSuspension.
    let second = sctx.record_terminal("t1".into(), SubStatus::Completed, "done".into(), 0);
    assert!(
        matches!(second, TerminalRecord::Duplicate),
        "expected Duplicate, got {second:?}"
    );
}

/// Companion to the test above: the dedup window is bounded, not
/// unbounded (issue #224's stated design goal — avoid the same
/// unbounded-growth class of issue #214/#222 in a long-lived session). Once
/// an entry ages out of the window, a repeat call for that id falls through
/// to `NoSuspension` again, exactly as it did before this fix.
#[tokio::test]
async fn record_terminal_repeat_after_window_expires_is_not_deduped() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());
    let first = sctx.record_terminal("t1".into(), SubStatus::Completed, "done".into(), 0);
    assert!(matches!(first, TerminalRecord::Recorded(_)));
    sctx.clear_suspension_if_collected();

    // Backdate the recorded entry past the dedup window.
    {
        let mut recent = sctx
            .recently_recorded_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (_, recorded_at) in recent.iter_mut() {
            *recorded_at = std::time::Instant::now() - std::time::Duration::from_secs(3601);
        }
    }

    let second = sctx.record_terminal("t1".into(), SubStatus::Completed, "done again".into(), 0);
    assert!(
        matches!(second, TerminalRecord::NoSuspension),
        "expected NoSuspension once the window has passed, got {second:?}"
    );
}

#[tokio::test]
async fn silence_guidance_omitted_when_last_terminal_lands() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());
    // The terminal event already collected (pending empty) — this resume
    // turn is the loud final summary, no guidance.
    let _ = sctx.record_terminal(
        "t1".to_string(),
        SubStatus::Completed,
        "done".to_string(),
        0,
    );
    let mut content = "[系统通知] 子代理已完成后台任务 (session_id: t1)".to_string();
    maybe_append_silence_guidance(&sctx, &mut content);
    assert_eq!(content, "[系统通知] 子代理已完成后台任务 (session_id: t1)");
}

#[tokio::test]
async fn silence_guidance_omitted_when_not_suspended() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    let mut content = "[系统通知] 子代理已完成后台任务 (session_id: t1)".to_string();
    maybe_append_silence_guidance(&sctx, &mut content);
    assert_eq!(content, "[系统通知] 子代理已完成后台任务 (session_id: t1)");
}

/// Race fix (E2E 恢复轮1): the silenced intent is captured at
/// wake/route time (`record_terminal` → `route_notice`, same sync
/// section) — the value is what `route_notice` puts on the synthetic
/// `ChannelInboundMessage.silenced_override`. A queued notice may start
/// after later terminals cleared `pending`; the override keeps the
/// intermediate notice silenced even though the live snapshot at turn
/// start would be empty.
#[tokio::test]
async fn silence_intent_captured_at_wake_time_survives_late_collection() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());
    sctx.add_pending_task("t2".to_string());

    // wake-1 (t1 terminal): record_terminal runs first, then route_notice
    // derives the intent from `has_pending_async_work()` — t2 remains →
    // intermediate notice.
    let _ = sctx.record_terminal("t1".into(), SubStatus::Completed, "t1 done".into(), 0);
    let intent_t1 = Some(sctx.has_pending_async_work());
    assert_eq!(intent_t1, Some(true));

    // Race: t2's terminal lands BEFORE wake-1's turn runs — live pending
    // is now empty, but wake-1's intent (Some(true)) keeps it silenced.
    let _ = sctx.record_terminal("t2".into(), SubStatus::Completed, "t2 done".into(), 0);
    let live = sctx.suspension_snapshot();
    assert!(live.as_ref().unwrap().pending.is_empty());
    assert!(crate::agents::session_context::decide_silenced(intent_t1, live.clone()));

    // wake-2 (t2 terminal): final notice → loud summary.
    let intent_t2 = Some(sctx.has_pending_async_work());
    assert_eq!(intent_t2, Some(false));
    assert!(!crate::agents::session_context::decide_silenced(intent_t2, live));
}

// ---- P1 (2026-08-13, RFC delegation-notice-queue §4): 内存投递队列 ----

#[tokio::test]
async fn notice_queue_fifo_roundtrip() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "n1".to_string(),
        content: "one".to_string(),
        silenced_override: Some(false),
    });
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "n2".to_string(),
        content: "two".to_string(),
        silenced_override: None,
    });
    assert!(sctx.has_queued_delegation_notices());
    // P1: `has_notice_turns_in_flight` covers the undrained queue too.
    assert!(sctx.has_notice_turns_in_flight());
    let taken = sctx.take_delegation_notices();
    assert_eq!(taken.len(), 2);
    assert_eq!(taken[0].id, "n1");
    assert_eq!(taken[1].id, "n2");
    assert!(!sctx.has_queued_delegation_notices());
    assert!(!sctx.has_notice_turns_in_flight());
}

#[tokio::test]
async fn busy_turn_lock_holds_notices_for_turn_end_drain() {
    let channel = MockChannel::new();
    let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    // Simulate a running turn: hold `turn_lock` so the wake's idle check
    // fails.
    let _busy = sctx.turn_lock.lock().await;
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            summary: "done".to_string(),
            duration_secs: 3,
            sent_message_count: 0,
        },
    )
    .await;
    // Enqueued, NOT dispatched — no immediate drain while busy.
    assert!(sctx.has_queued_delegation_notices());
    assert_eq!(sctx.notice_turns_in_flight.load(Ordering::SeqCst), 0);
    assert!(channel.sent.lock().unwrap().is_empty());
    drop(_busy);
}

#[tokio::test]
async fn idle_wake_drains_immediately() {
    let channel = MockChannel::new();
    let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
    let sctx = suspended_session(&ctx);
    let sid = sctx.session_id.clone();
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            summary: "done".to_string(),
            duration_secs: 3,
            sent_message_count: 0,
        },
    )
    .await;
    // wake spawned the drain; give it a moment to take + dispatch.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!sctx.has_queued_delegation_notices());
    assert_eq!(sctx.notice_turns_in_flight.load(Ordering::SeqCst), 0);
    // The notice turn ran (NullRegistry → error notice lands on channel).
    assert!(!channel.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn drain_merges_all_notices_into_one_fifo_ordered_turn() {
    let channel = MockChannel::new();
    let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
    let sctx = suspended_session(&ctx);
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "n1".to_string(),
        content: "first".to_string(),
        silenced_override: Some(false),
    });
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "n2".to_string(),
        content: "second".to_string(),
        silenced_override: Some(false),
    });
    drain_delegation_notices(&ctx, &sctx.session_id).await;
    // `drain_delegation_notices` awaits the single batched turn's
    // dispatch `JoinHandle` before returning, so by the time this
    // `.await` resolves the turn has genuinely finished, not just been
    // scheduled.
    assert!(!sctx.has_queued_delegation_notices());
    assert_eq!(sctx.notice_turns_in_flight.load(Ordering::SeqCst), 0);
    // Issue #106 round 2: both notices are delivered as ONE turn, not
    // two — it fails on NullRegistry and sends exactly one error
    // message (not one per notice).
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
    // The merged synthetic message (persisted to history before the
    // provider call, even though the call itself then fails) preserves
    // FIFO order: "first" appears before "second".
    let session = sctx.session_snapshot().await;
    let merged = session
        .history
        .iter()
        .find(|m| m.role == "user" && m.text_content().contains("first"))
        .unwrap_or_else(|| panic!("no merged notice message found in history: {:?}", session.history));
    let text = merged.text_content();
    assert!(text.contains("first"), "got: {text}");
    assert!(text.contains("second"), "got: {text}");
    assert!(
        text.find("first") < text.find("second"),
        "expected FIFO order (first before second), got: {text}"
    );
}

#[tokio::test]
async fn drain_dedupes_duplicate_notice_ids() {
    let channel = MockChannel::new();
    let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
    let sctx = suspended_session(&ctx);
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "dup".to_string(),
        content: "first".to_string(),
        silenced_override: Some(false),
    });
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "dup".to_string(),
        content: "second".to_string(),
        silenced_override: Some(false),
    });
    drain_delegation_notices(&ctx, &sctx.session_id).await;
    // drain only awaits take+spawn; the notice turn runs in background.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!sctx.has_queued_delegation_notices());
    // Only one notice dispatched (dedup) — its failed turn sends one
    // user-facing error message (issue #113).
    assert_eq!(channel.sent.lock().unwrap().len(), 1);
}

fn notice(id: &str) -> DelegationNotice {
    DelegationNotice {
        id: id.to_string(),
        content: String::new(),
        silenced_override: None,
    }
}

/// issue #106 round 2: every id in a batch beyond the first must ride
/// along as `extra_notice_ids` — this is what feeds
/// `dispatch_turn_spawn`'s mark-delivered loop; a bug here means a
/// notice was actually delivered (it's in the merged turn content) but
/// its completion-queue entry stays Pending forever, redelivering it on
/// every daemon restart despite the user having already seen it.
#[test]
fn split_batch_ids_first_is_primary_rest_are_extra() {
    let batch = vec![notice("a"), notice("b"), notice("c")];
    let (primary, extra) = split_batch_ids(&batch);
    assert_eq!(primary, "a");
    assert_eq!(extra, vec!["b".to_string(), "c".to_string()]);
}

#[test]
fn split_batch_ids_single_notice_has_no_extras() {
    let batch = vec![notice("only")];
    let (primary, extra) = split_batch_ids(&batch);
    assert_eq!(primary, "only");
    assert!(extra.is_empty());
}

#[tokio::test]
async fn undrained_notices_keep_suspension_alive() {
    let ctx = test_ctx(vec![]);
    let sctx = ctx.sessions.get_or_create_context("mock:default:u1");
    sctx.add_pending_task("t1".to_string());
    // Wake burst collected the terminal...
    let _ = sctx.record_terminal(
        "t1".to_string(),
        SubStatus::Completed,
        "done".to_string(),
        0,
    );
    // ...but a notice is still waiting for a drain — suspension survives.
    sctx.enqueue_delegation_notice(DelegationNotice {
        id: "n1".to_string(),
        content: "notice".to_string(),
        silenced_override: Some(false),
    });
    sctx.clear_suspension_if_collected();
    assert!(sctx.suspension_snapshot().is_some());
    // Drain completes the sequence — next clear drops the suspension.
    let _ = sctx.take_delegation_notices();
    sctx.clear_suspension_if_collected();
    assert!(sctx.suspension_snapshot().is_none());
}

/// 方案4: a duplicate terminal event (same sub_session_id sent twice via
/// `wake`) must only deliver ONE notice. The first `record_terminal`
/// returns `Recorded` → route_notice fires; the second returns `Duplicate`
/// → `wake` returns early without routing. A second pending task keeps the
/// suspension alive so the second call hits the idempotent guard rather
/// than `NoSuspension`.
#[tokio::test]
async fn wake_dedupes_duplicate_terminal_event() {
    let channel = MockChannel::new();
    let ctx = test_ctx(vec![(("mock".into(), "default".into()), channel.clone())]);
    let sctx = suspended_session(&ctx);
    // Second pending task keeps the suspension alive after t1 is collected.
    sctx.add_pending_task("t2".to_string());
    let sid = sctx.session_id.clone();

    // First wake: record_terminal → Recorded → notice routed.
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid.clone(),
            summary: "done".to_string(),
            duration_secs: 1,
            sent_message_count: 0,
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let count_after_first = channel.sent.lock().unwrap().len();
    assert!(
        count_after_first > 0,
        "first wake should deliver a notice"
    );

    // Second wake (same t1): record_terminal → Duplicate → no notice.
    wake(
        &ctx,
        DelegationEvent::Completed {
            sub_session_id: "t1".to_string(),
            parent_session_id: sid,
            summary: "done again".to_string(),
            duration_secs: 1,
            sent_message_count: 0,
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let count_after_second = channel.sent.lock().unwrap().len();
    assert_eq!(
        count_after_second, count_after_first,
        "duplicate wake must not deliver another notice"
    );
}
