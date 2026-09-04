use crate::agents::turn::TurnSuspension;

///
/// Delegation notices carry a **wake-time intent** (`intent`), captured when
/// the terminal event was collected — a queued notice may start long after
/// later terminals cleared `pending`, so the live snapshot at turn start is
/// racy (the E2E 恢复轮1 bug: an intermediate notice was misjudged as the
/// final turn because `pending` was already empty when its turn ran, so the
/// EndTurn→Continue mapping / ask_user disable / TTS+on_status suppression
/// and the injected SILENCE_GUIDANCE disagreed with each other).
///
/// issue #131 decision 3: real user messages carry `None` and are simply
/// never silenced (`intent.unwrap_or(false)`), regardless of live pending
/// state. Before #131 this defaulted to the live-snapshot check instead —
/// safe only because the (now-removed) silent user-message queue guaranteed
/// `pending` was empty by the time a queued message actually dispatched.
/// Once a user message can dispatch immediately while background work is
/// still pending (the whole point of removing the queue — an interrupt gets
/// answered right away), falling back to the live snapshot would silence a
/// genuine user turn: its reply would stream as commentary instead of a
/// real answer, the turn would map EndTurn→Continue (never actually end),
/// and `ask_user` would be disabled for it. "Silenced" is now exclusively an
/// opt-in synthetic marker for delegation-notice-routed turns, which always
/// pass `Some(bool)`; `live` is kept as a parameter only because callers
/// still have a snapshot in hand, not because this function still uses it
/// for `None`. `pub(crate)` so orchestrator tests can pin the wake-time
/// semantics.
pub(crate) fn decide_silenced(intent: Option<bool>, _live: Option<TurnSuspension>) -> bool {
    intent.unwrap_or(false)
}


/// 方案 C (fix v2): semantic stop-reason for observability — a silenced
/// resume turn with pending delegations whose model output ended with
/// `EndTurn` is reported as `Continue` (the turn does NOT end; later
/// terminal events keep resuming it until the final loud summary). Pure
/// so unit tests can pin the mapping without a full turn pipeline.
pub(crate) fn semantic_stop_reason(
    silenced: bool,
    has_pending: bool,
    stop_reason: crate::providers::StopReason,
) -> crate::providers::StopReason {
    if silenced && has_pending && stop_reason == crate::providers::StopReason::EndTurn {
        crate::providers::StopReason::Continue
    } else {
        stop_reason
    }
}

/// 方案4 (terminal-event idempotency): three-state result of
/// `record_terminal` — distinguishes a first recording from an idempotent
/// duplicate hit so callers know whether to send a notification.
#[derive(Debug, Clone)]
pub enum TerminalRecord {
    /// 首次记录：结果已写入 suspension，调用方应发送通知
    Recorded(Box<TurnSuspension>),
    /// 该 sub_session_id 已记录过（幂等命中）：调用方应跳过通知
    Duplicate,
    /// 会话没有活跃 suspension（父代理未被挂起等待）
    NoSuspension,
}

/// P1-4: turn-suspension (方案 C, RFC §3/§5) behavior tests. All state is
/// exercised through the public `SessionContext` API; persistence round-trips
/// go through the manager path so the in-memory backend + `BackendPersistHook`
/// are the same wiring production uses.
#[cfg(test)]
mod suspension_tests {
    use super::*;
    use std::sync::Arc;
    use crate::agents::SessionManager;
    use crate::agents::turn::{SubResult, SubStatus};
    use crate::agents::session_context::SessionContext;
    use crate::agents::session_context::NoticeTurnGuard;

    fn make_ctx() -> (Arc<SessionContext>, Arc<SessionManager>) {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        (ctx, manager)
    }

    /// A suspended context with one pending task "t1".
    fn suspended() -> Arc<SessionContext> {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx
    }

    #[test]
    fn pending_flips_with_registration_and_collection() {
        let ctx = suspended();
        assert!(ctx.has_pending_async_work());
        assert_eq!(ctx.suspension_snapshot().unwrap().pending, vec!["t1"]);
        ctx.record_terminal("t1".into(), SubStatus::Completed, "ok".into(), 0, "test");
        assert!(!ctx.has_pending_async_work());
        assert!(ctx.suspension_snapshot().is_some(), "collected result keeps the suspension until clear");
    }

    #[test]
    fn add_pending_is_idempotent() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string());
        assert_eq!(
            ctx.suspension_snapshot().unwrap().pending,
            vec!["t1", "t2"]
        );
    }

    #[test]
    fn progress_folds_into_terminal_result() {
        let ctx = suspended();
        ctx.add_progress("t1", "working on it");
        ctx.add_progress("t1", "still going");
        let snap = match ctx
            .record_terminal("t1".into(), SubStatus::Completed, "summary text".into(), 0, "test")
        {
            TerminalRecord::Recorded(s) => s,
            other => panic!("expected Recorded, got {other:?}"),
        };
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.sub_session_id, "t1");
        assert_eq!(r.status, SubStatus::Completed);
        assert_eq!(r.content, "summary text");
        assert_eq!(r.sent_message_count, 0);
        assert_eq!(r.progress, vec!["working on it", "still going"]);
        assert!(snap.pending.is_empty());
        assert!(snap.progress_by_sub_session.is_empty());
    }

    #[test]
    fn out_of_order_completion_collects_in_completion_order() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string());
        ctx.add_pending_task("t3".to_string());
        ctx.record_terminal("t3".into(), SubStatus::Failed, "e3".into(), 0, "test");
        ctx.record_terminal("t1".into(), SubStatus::Completed, "c1".into(), 0, "test");
        ctx.record_terminal("t2".into(), SubStatus::TimedOut, "t2".into(), 0, "test");
        let snap = ctx.suspension_snapshot().unwrap();
        let order: Vec<&str> = snap.results.iter().map(|r| r.sub_session_id.as_str()).collect();
        assert_eq!(order, vec!["t3", "t1", "t2"]);
        assert_eq!(snap.results[1].status, SubStatus::Completed);
        assert_eq!(snap.results[2].status, SubStatus::TimedOut);
        assert!(snap.pending.is_empty());
    }

    #[test]
    fn record_terminal_without_suspension_returns_no_suspension() {
        let (ctx, _m) = make_ctx();
        assert!(ctx.suspension_snapshot().is_none());
        assert!(!ctx.has_pending_async_work());
        let rec = ctx.record_terminal("t9".into(), SubStatus::Completed, "x".into(), 0, "test");
        assert!(matches!(rec, TerminalRecord::NoSuspension));
        assert!(ctx.suspension_snapshot().is_none());
    }

    /// 方案4: a second `record_terminal` for the same sub_session_id returns
    /// `Duplicate` (idempotent hit) — callers must skip the notification.
    #[test]
    fn record_terminal_second_call_is_duplicate() {
        let ctx = suspended();
        ctx.add_pending_task("t2".to_string()); // keep suspension alive after t1
        let first = ctx.record_terminal("t1".into(), SubStatus::Completed, "done".into(), 0, "test");
        assert!(matches!(first, TerminalRecord::Recorded(_)));
        // Second call: t1 is no longer pending and already in results → Duplicate
        let second = ctx.record_terminal("t1".into(), SubStatus::Completed, "again".into(), 0, "test");
        assert!(matches!(second, TerminalRecord::Duplicate));
        // Results unchanged — no duplicate entry added
        let snap = ctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        assert_eq!(snap.results[0].content, "done");
    }

    /// 方案4: idempotency survives a daemon restart — a fresh context that
    /// restores the persisted suspension must still return `Duplicate` for a
    /// sub_session_id already collected before the crash.
    #[test]
    fn record_terminal_duplicate_survives_restart() {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string()); // keep suspension alive
        let first = ctx.record_terminal("t1".into(), SubStatus::Completed, "pre-crash".into(), 0, "test");
        assert!(matches!(first, TerminalRecord::Recorded(_)));

        // Simulate restart: drop the context and re-create from the backend.
        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");
        // t1 is already in results (persisted) → second call is Duplicate
        let replayed = ctx2.record_terminal("t1".into(), SubStatus::Completed, "post-crash".into(), 0, "test");
        assert!(matches!(replayed, TerminalRecord::Duplicate));
        let snap = ctx2.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 1);
        assert_eq!(snap.results[0].content, "pre-crash");
    }

    #[test]
    fn clear_semantics_pending_kept_empty_removed_idempotent() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string());
        // pending non-empty → suspension retained
        ctx.record_terminal("t1".into(), SubStatus::Completed, "c1".into(), 0, "test");
        ctx.clear_suspension_if_collected();
        let snap = ctx.suspension_snapshot().unwrap();
        assert_eq!(snap.pending, vec!["t2"]);
        // pending empty → suspension cleared
        ctx.record_terminal("t2".into(), SubStatus::Completed, "c2".into(), 0, "test");
        ctx.clear_suspension_if_collected();
        assert!(ctx.suspension_snapshot().is_none());
        // idempotent
        ctx.clear_suspension_if_collected();
        assert!(ctx.suspension_snapshot().is_none());
    }

    /// 单 preview (2026-08-12): `pending` empty alone is NOT the end of the
    /// suspension sequence — delegation notices still queued behind
    /// `turn_lock` (counted in `notice_turns_in_flight`) must keep the
    /// suspension (and its cross-turn preview) alive; only the LAST notice
    /// turn's exit (counter → 0) may clear it.
    #[test]
    fn clear_respects_notice_turns_in_flight() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.bump_notice_turn(); // wake burst dispatched a notice (counter=1)
        // Wake burst collects the only terminal → pending empty, but the
        // notice turn has not run yet → suspension must survive.
        let _ = ctx.record_terminal("t1".into(), SubStatus::Completed, "done".into(), 0, "test");
        ctx.clear_suspension_if_collected();
        let snap = ctx.suspension_snapshot().expect("suspension kept while notice in flight");
        assert!(snap.pending.is_empty());
        // The notice turn finishes → counter=0 → next clear drops the state.
        ctx.finish_notice_turn();
        ctx.clear_suspension_if_collected();
        assert!(ctx.suspension_snapshot().is_none());
        // Idempotent with counter at 0.
        ctx.clear_suspension_if_collected();
        assert!(ctx.suspension_snapshot().is_none());
    }

    /// 单 preview (2026-08-12): counter roundtrip — bump increments,
    /// finish decrements (saturating so a direct `process_turn` call without
    /// a dispatch-time bump never underflows), and the RAII guard decrements
    /// exactly once even when `finish()` was already called explicitly.
    #[test]
    fn notice_turn_counter_roundtrip() {
        let (ctx, _m) = make_ctx();
        assert!(!ctx.has_notice_turns_in_flight());
        ctx.bump_notice_turn();
        ctx.bump_notice_turn();
        assert!(ctx.has_notice_turns_in_flight());
        ctx.finish_notice_turn();
        assert!(ctx.has_notice_turns_in_flight());
        ctx.finish_notice_turn();
        assert!(!ctx.has_notice_turns_in_flight());
        // Saturating: extra finishes below zero are no-ops, no underflow.
        ctx.finish_notice_turn();
        ctx.finish_notice_turn();
        assert!(!ctx.has_notice_turns_in_flight());

        // RAII guard: active on a notice turn, decrements on drop.
        {
            let mut g = NoticeTurnGuard::new(&ctx, true);
            ctx.bump_notice_turn();
            assert!(ctx.has_notice_turns_in_flight());
            g.finish(); // explicit finish → Drop must not double-decrement
        }
        assert!(!ctx.has_notice_turns_in_flight());

        // Inactive guard (user turn) never touches the counter.
        {
            let _g = NoticeTurnGuard::new(&ctx, false);
        }
        assert!(!ctx.has_notice_turns_in_flight());
    }

    #[test]
    fn eight_threads_concurrent_collection_loses_nothing() {
        let (ctx, _m) = make_ctx();
        for i in 0..8 {
            ctx.add_pending_task(format!("t{}", i));
        }
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let ctx = Arc::clone(&ctx);
            handles.push(std::thread::spawn(move || {
                ctx.record_terminal(
                    format!("t{}", i),
                    SubStatus::Completed,
                    format!("r{}", i),
                    i as u64,
                "test",
                );
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let snap = ctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 8);
        assert!(snap.pending.is_empty());
        let mut by_sub_session: Vec<&SubResult> = snap.results.iter().collect();
        by_sub_session.sort_by_key(|r| r.sub_session_id.clone());
        for (i, r) in by_sub_session.iter().enumerate() {
            assert_eq!(r.sub_session_id, format!("t{}", i));
            assert_eq!(r.content, format!("r{}", i));
            assert_eq!(r.sent_message_count, i as u64);
        }
    }

    #[test]
    fn persist_restore_roundtrip_preserves_results() {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        ctx.add_pending_task("t1".to_string());
        ctx.add_progress("t1", "halfway");
        ctx.record_terminal("t1".into(), SubStatus::Completed, "final summary".into(), 2, "test");
        let sid = ctx.session_id.clone();
        assert_eq!(ctx.suspension_snapshot().unwrap().results.len(), 1);

        // Drop the context — a fresh one must restore from the backend.
        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");
        assert_eq!(ctx2.session_id, sid);
        let snap2 = ctx2.suspension_snapshot().unwrap();
        assert_eq!(snap2.results.len(), 1);
        let r = &snap2.results[0];
        assert_eq!(r.sub_session_id, "t1");
        assert_eq!(r.status, SubStatus::Completed);
        assert_eq!(r.content, "final summary");
        assert_eq!(r.sent_message_count, 2);
        assert_eq!(r.progress, vec!["halfway"]);

        // Clearing persists a None → next rebuild is unsuspended.
        ctx2.clear_suspension_if_collected();
        manager.drop_context("mock:default:u1");
        let ctx3 = manager.get_or_create_context("mock:default:u1");
        assert!(ctx3.suspension_snapshot().is_none());
    }

    #[test]
    fn corrupt_and_empty_json_are_ignored_on_restore() {
        let manager = Arc::new(SessionManager::in_memory());
        let ctx = manager.get_or_create_context("mock:default:u1");
        let sid = ctx.session_id.clone();
        // Corrupt JSON → restore warns and ignores.
        manager.backend().save_suspension(&sid, "{ not json").unwrap();
        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");
        assert!(ctx2.suspension_snapshot().is_none());
        // Empty JSON → treated as no suspension.
        manager.backend().save_suspension(&sid, "").unwrap();
        manager.drop_context("mock:default:u1");
        let ctx3 = manager.get_or_create_context("mock:default:u1");
        assert!(ctx3.suspension_snapshot().is_none());
    }

    /// 旧版 `suspension.json`(2026-08-10 方案 B 时期,含 `progress_preview`
    /// 字段)必须仍可反序列化——结构无 `deny_unknown_fields`,未知字段被 serde
    /// 忽略;往返序列化不携带该字段。
    #[test]
    fn suspension_serde_ignores_legacy_progress_preview_field() {
        let legacy = r#"{"origin_turn_seq":0,"suspended_at":1700000000,"pending":["t1"],"results":[],"progress_by_task":{},"progress_preview":{"reply_target":"12345:678","msg_id":"42","lines":["子代理任务 t1 已完成"],"origin_text":null}}"#;
        let s: TurnSuspension = serde_json::from_str(legacy).unwrap();
        assert_eq!(s.pending, vec!["t1"]);
        let json = serde_json::to_string(&s).unwrap();
        let s2: TurnSuspension = serde_json::from_str(&json).unwrap();
        assert_eq!(s2.pending, vec!["t1"]);
        assert!(!json.contains("progress_preview"));
    }

    /// Race fix (E2E 恢复轮1): a delegation notice's silenced flag must come
    /// from the WAKE-time intent, not the live snapshot at turn start — a
    /// queued notice may run after later terminal events cleared `pending`.
    #[test]
    fn decide_silenced_uses_wake_time_intent_over_live_snapshot() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string());

        // wake-1 (t1 terminal): collected while t2 still pending → the
        // route-time intent says "intermediate notice".
        let _ = ctx.record_terminal("t1".into(), SubStatus::Completed, "t1 done".into(), 0, "test");
        let live_with_t2 = ctx.suspension_snapshot();
        assert!(decide_silenced(Some(true), live_with_t2.clone()));

        // Race: t2's terminal lands BEFORE wake-1's turn starts — the live
        // snapshot is now empty (would mark the turn loud), but the
        // wake-time intent keeps the intermediate notice silenced.
        let _ = ctx.record_terminal("t2".into(), SubStatus::Completed, "t2 done".into(), 0, "test");
        let live_empty = ctx.suspension_snapshot();
        assert!(live_empty.as_ref().unwrap().pending.is_empty());
        assert!(decide_silenced(Some(true), live_empty.clone()));

        // wake-2 (t2 terminal, final notice): intent false → loud summary
        // regardless of the live snapshot.
        assert!(!decide_silenced(Some(false), live_empty.clone()));
        assert!(!decide_silenced(Some(false), live_with_t2));
    }

    /// issue #131 decision 3: a genuine user message (`silenced_override =
    /// None`) is never silenced, regardless of live pending state — the
    /// silent user-message queue that used to guarantee `pending` was empty
    /// by dispatch time is gone, so a user interrupt must always get a real,
    /// turn-ending answer rather than being folded into intermediate
    /// commentary just because background work happens to still be pending.
    #[test]
    fn decide_silenced_never_silences_user_messages_even_with_live_pending() {
        let (ctx, _m) = make_ctx();
        // Not suspended → loud.
        assert!(!decide_silenced(None, ctx.suspension_snapshot()));
        // Suspended with pending (background work still running) → still
        // loud: this is the exact case the old live-snapshot fallback got
        // wrong once queuing was removed.
        ctx.add_pending_task("t1".to_string());
        assert!(!decide_silenced(None, ctx.suspension_snapshot()));
        // Suspended but fully collected → loud either way.
        ctx.record_terminal("t1".into(), SubStatus::Completed, "ok".into(), 0, "test");
        assert!(!decide_silenced(None, ctx.suspension_snapshot()));
    }

    /// 方案 C (fix v2): a silenced resume turn with pending delegations whose
    /// model output ended with `EndTurn` is semantically `Continue` — the
    /// turn does NOT end; the final loud summary is the only EndTurn.
    #[test]
    fn semantic_stop_reason_maps_silenced_endturn_to_continue() {
        use crate::providers::StopReason;
        assert_eq!(
            semantic_stop_reason(true, true, StopReason::EndTurn),
            StopReason::Continue
        );
        // Loud final resume / non-pending turns keep EndTurn.
        assert_eq!(
            semantic_stop_reason(false, true, StopReason::EndTurn),
            StopReason::EndTurn
        );
        assert_eq!(
            semantic_stop_reason(true, false, StopReason::EndTurn),
            StopReason::EndTurn
        );
        // Non-EndTurn reasons pass through untouched.
        assert_eq!(
            semantic_stop_reason(true, true, StopReason::MaxTokens),
            StopReason::MaxTokens
        );
        assert_eq!(
            semantic_stop_reason(true, true, StopReason::ToolUse),
            StopReason::ToolUse
        );
        // A provider never produces Continue; the function is idempotent.
        assert_eq!(
            semantic_stop_reason(true, true, StopReason::Continue),
            StopReason::Continue
        );
    }

}
