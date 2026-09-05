use super::*;
use crate::agents::session::SessionManager;
use crate::agents::AgentMessenger;
use crate::agents::AgentRegistry;
use crate::config::sub_agent::SubAgentConfig;
use crate::agents::delegation::{AgentMessage, DelegationTimeout, MessageKind};
use crate::config::sub_agent::AgentIsolation;
use super::lifecycle::{resolve_timeout, should_gc_sub_session};
use super::worktree::worktree_branch_name;

fn coder_config() -> SubAgentConfig {
    SubAgentConfig {
        name: "coder".to_string(),
        system_prompt: "You are a coding specialist.".to_string(),
        tools: crate::config::filters::ToolFilter::all(),
        skills: crate::config::filters::SkillFilter::all(),
        mcp: crate::config::filters::McpFilter::all(),
        max_tool_calls: None,
        description: None,
        model: None,
        isolation: AgentIsolation::Shared,
        timeout: None,
    }
}

fn coordinator(max_depth: u32) -> (DelegationCoordinator, Arc<SessionManager>) {
    let registry = Arc::new(AgentRegistry::from_vec(vec![coder_config()]));
    let manager = Arc::new(SessionManager::in_memory());
    let dc = DelegationCoordinator::new(
        registry,
        Arc::clone(&manager),
        PathBuf::new(),
        "test",
        max_depth,
        Default::default(),
    );
    (dc, manager)
}

#[tokio::test]
async fn sync_send_to_parent_records_text_without_broadcast() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let (tx, mut rx) = mpsc::channel(8);
    dc.set_event_sender(tx);

    // No running-table entry ⇒ sync delegation: final messages are
    // recorded for the tool-result merge (B, 2026-08-14), NOT broadcast
    // as DelegationEvent::Message — the parent is blocked in the tool
    // call and receives them via the returned result instead.
    assert!(dc
        .send_to_parent(AgentMessage {
            msg_id: "m1".to_string(),
            sender_name: "coder".to_string(),
            sub_session_id: "test/s/sync-sub".to_string(),
            parent_session_id: parent.session_id.clone(),
            text: "detailed final report".to_string(),
            kind: MessageKind::Final,
        })
        .await);
    assert!(
        rx.try_recv().is_err(),
        "sync delegation message must not be broadcast as DelegationEvent::Message"
    );
    let stored = dc
        .sent_messages
        .get("test/s/sync-sub")
        .map(|v| v.read().unwrap().clone())
        .unwrap_or_default();
    assert_eq!(stored, vec!["detailed final report"]);

    // Progress messages are never recorded (RFC §3.4: progress is
    // dropped for the parent context).
    assert!(dc
        .send_to_parent(AgentMessage {
            msg_id: "m2".to_string(),
            sender_name: "coder".to_string(),
            sub_session_id: "test/s/sync-sub".to_string(),
            parent_session_id: parent.session_id.clone(),
            text: "progress 50%".to_string(),
            kind: MessageKind::Progress,
        })
        .await);
    let stored = dc
        .sent_messages
        .get("test/s/sync-sub")
        .map(|v| v.read().unwrap().clone())
        .unwrap_or_default();
    assert_eq!(stored, vec!["detailed final report"], "progress must be skipped");
}

/// issue #260 (review finding on #262): reconcile must remove the in-flight
/// entry, drop the mailbox, and emit `Completed` to the parent with the
/// notice summary; a second reconcile is a no-op (at-most-once).
#[tokio::test]
async fn reconcile_notice_completed_removes_entry_and_emits_completed() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let (tx, mut rx) = mpsc::channel(8);
    dc.set_event_sender(tx);

    let sub_session_id = "test/s/reconcile-sub".to_string();
    dc.running.insert(
        sub_session_id.clone(),
        RunningEntry {
            handle: tokio::spawn(async {}),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: parent.session_id.clone(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(60),
            started_at: chrono::Utc::now(),
            allowed_tools: None,
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
        },
    );
    // A mailbox must exist so the reconcile is observable as "removed".
    let (mail_tx, _mail_rx) = mpsc::channel(4);
    dc.mailboxes.insert(sub_session_id.clone(), mail_tx);

    dc.reconcile_notice_completed(&sub_session_id, "reconciled summary");

    assert!(!dc.running.contains_key(&sub_session_id), "in-flight entry must be removed");
    assert!(!dc.mailboxes.contains_key(&sub_session_id), "mailbox must be removed");

    let event = rx.recv().await.expect("Completed event must be emitted");
    match event {
        DelegationEvent::Completed {
            sub_session_id: sid,
            parent_session_id: pid,
            summary,
            ..
        } => {
            assert_eq!(sid, sub_session_id);
            assert_eq!(pid, parent.session_id, "Completed must be routed to the parent");
            assert_eq!(summary, "reconciled summary");
        }
        _ => panic!("expected DelegationEvent::Completed"),
    }

    // At-most-once: reconciling an already-removed entry emits nothing.
    dc.reconcile_notice_completed(&sub_session_id, "again");
    assert!(rx.try_recv().is_err(), "second reconcile must be a no-op");
}

#[tokio::test]
async fn async_send_to_parent_broadcasts_and_counts() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let (tx, mut rx) = mpsc::channel(8);
    dc.set_event_sender(tx);

    // Fake running entry ⇒ async path: broadcast + count, and nothing
    // recorded in the sync buffer.
    let sub_session_id = "test/s/async-sub".to_string();
    dc.running.insert(
        sub_session_id.clone(),
        RunningEntry {
            handle: tokio::spawn(async {}),
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: parent.session_id.clone(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(60),
            started_at: chrono::Utc::now(),
            allowed_tools: None,
        },
    );
    assert!(dc
        .send_to_parent(AgentMessage {
            msg_id: "m1".to_string(),
            sender_name: "coder".to_string(),
            sub_session_id: sub_session_id.clone(),
            parent_session_id: parent.session_id.clone(),
            text: "working".to_string(),
            kind: MessageKind::Final,
        })
        .await);
    match rx.try_recv().expect("async message must be broadcast") {
        DelegationEvent::Message(m) => assert_eq!(m.text, "working"),
        other => panic!("expected Message event, got {:?}", other),
    }
    let count = dc
        .running
        .get(&sub_session_id)
        .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);
    assert_eq!(count, 1);
    assert!(
        dc.sent_messages.get(&sub_session_id).is_none(),
        "async messages must not touch the sync buffer"
    );
}

#[test]
fn resolve_timeout_priority_fallback_and_clamp() {
    // default: system fallback 1200s
    assert_eq!(resolve_timeout(None, None), 1200);
    // tool value wins over config regardless of ordering
    assert_eq!(resolve_timeout(Some(100), Some(50)), 100);
    assert_eq!(resolve_timeout(Some(100), Some(200)), 100);
    // config used when tool doesn't specify
    assert_eq!(resolve_timeout(None, Some(300)), 300);
    // global hard ceiling 1800s — no per-agent override anymore, the
    // tool-call value is authoritative up to this ceiling
    assert_eq!(resolve_timeout(Some(5000), None), 1800);
    assert_eq!(resolve_timeout(Some(5000), Some(9000)), 1800);
    // 0 passes through (no lower clamp)
    assert_eq!(resolve_timeout(Some(0), None), 0);
}

/// issue #106: only a *successful sync* delegation is GC'd. In
/// particular, a successful *async* delegation must NOT be GC'd — that
/// was the actual bug (the pre-fix condition was `result.is_ok()`
/// alone, with no `is_async_delegation` check at all).
#[test]
fn should_gc_sub_session_only_for_successful_sync() {
    assert!(should_gc_sub_session(false, true), "sync + success -> GC");
    assert!(
        !should_gc_sub_session(true, true),
        "async + success must NOT be GC'd (issue #106)"
    );
    assert!(
        !should_gc_sub_session(false, false),
        "sync + failure/timeout -> kept as a resumable tombstone"
    );
    assert!(
        !should_gc_sub_session(true, false),
        "async + failure/timeout -> kept"
    );
}

#[test]
fn unknown_session_depth_falls_back_to_one() {
    let (dc, _m) = coordinator(3);
    assert_eq!(dc.session_depth("no-such-session"), 1);
    assert!(dc.check_depth("no-such-session").is_ok());
}

// ── resume_timed_out (timeout layer 3) ─────────────────────────────────

/// Coordinator over a real JsonFileBackend so delegation checkpoints
/// round-trip (the in-memory backend no-ops them). Runtime is NOT
/// installed: `recover_async` logs an error and returns without spawning
/// — everything resume validates/mutates before that point is observable.
fn coordinator_with_backend(
) -> (DelegationCoordinator, Arc<SessionManager>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Arc<dyn crate::storage::SessionBackend> = Arc::new(
        crate::storage::JsonFileBackend::open(dir.path()).unwrap(),
    );
    let manager = Arc::new(SessionManager::new(backend));
    let registry = Arc::new(AgentRegistry::from_vec(vec![coder_config()]));
    let dc = DelegationCoordinator::new(
        registry,
        Arc::clone(&manager),
        PathBuf::new(),
        "test",
        3,
        Default::default(),
    );
    (dc, manager, dir)
}

fn checkpoint_for(
    sub_session_id: &str,
    parent_session_id: &str,
    status: &str,
    timeout_secs: u64,
) -> crate::storage::DelegationCheckpoint {
    crate::storage::DelegationCheckpoint {
        parent_session_id: parent_session_id.to_string(),
        sub_session_id: sub_session_id.to_string(),
        agent_name: "coder".to_string(),
        status: status.to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs,
        allowed_tools: Some(vec!["shell".to_string()]),
        last_checkpoint: None,
    }
}

#[tokio::test]
async fn resume_timed_out_rejects_missing_and_non_timeout_checkpoints() {
    let (dc, manager, _dir) = coordinator_with_backend();

    // No checkpoint at all → error.
    let err = dc.resume_timed_out("test/s/ghost", None).unwrap_err();
    assert!(format!("{:#}", err).contains("no delegation checkpoint"));

    // Real parent + sub sessions, but the tombstone says "failed" →
    // only timed_out is resumable.
    let parent = manager.get_or_create_context("mock:default:u1");
    let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    manager
        .backend()
        .save_delegation_checkpoint(&checkpoint_for(
            &sub.id,
            &parent.session_id,
            "failed",
            600,
        ))
        .unwrap();
    let err = dc.resume_timed_out(&sub.id, None).unwrap_err();
    assert!(format!("{:#}", err).contains("not resumable"));
    assert!(format!("{:#}", err).contains("failed"));
}

/// issue #134 (P2): `timed_out_checkpoints` backs agent_resume's
/// not-found listing — only `timed_out` status checkpoints qualify, not
/// `failed`/`cancelled`/`running` ones.
#[tokio::test]
async fn timed_out_checkpoints_filters_by_status() {
    let (dc, manager, _dir) = coordinator_with_backend();
    let parent = manager.get_or_create_context("mock:default:u1");
    let backend = manager.backend();

    let timed_out_sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    backend
        .save_delegation_checkpoint(&checkpoint_for(
            &timed_out_sub.id,
            &parent.session_id,
            "timed_out",
            600,
        ))
        .unwrap();

    let failed_sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    backend
        .save_delegation_checkpoint(&checkpoint_for(
            &failed_sub.id,
            &parent.session_id,
            "failed",
            600,
        ))
        .unwrap();

    let timed_out = dc.timed_out_checkpoints();
    assert_eq!(timed_out.len(), 1);
    assert_eq!(timed_out[0].sub_session_id, timed_out_sub.id);
}

#[tokio::test]
async fn resume_timed_out_rewrites_checkpoint_and_rearms_parent() {
    let (dc, manager, _dir) = coordinator_with_backend();
    let parent = manager.get_or_create_context("mock:default:u1");
    let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    let backend = manager.backend();
    backend
        .save_delegation_checkpoint(&checkpoint_for(
            &sub.id,
            &parent.session_id,
            "timed_out",
            600,
        ))
        .unwrap();

    // Parent turn already ended (no suspension) — resume must re-arm it.
    assert!(!parent.has_pending_async_work());

    let resumed = dc.resume_timed_out(&sub.id, Some(9999)).unwrap();
    assert_eq!(resumed, sub.id);

    // Fresh budget clamped to the global 1800s ceiling and checkpoint
    // flipped back to running.
    let cp = backend.load_delegation_checkpoint(&sub.id).unwrap();
    assert_eq!(cp.status, "running");
    assert_eq!(cp.timeout_secs, 1800);
    assert_eq!(cp.allowed_tools.as_deref(), Some(&["shell".to_string()][..]));

    // Parent suspension re-armed so the eventual terminal event records
    // (instead of hitting the Duplicate drop path).
    assert!(parent.has_pending_async_work());

    // Second resume while the first holds the slot → already running.
    // (recover_async returned early without inserting a RunningEntry
    // because no runtime is installed in tests, so simulate the entry.)
    dc.running.insert(
        sub.id.clone(),
        RunningEntry {
            handle: tokio::spawn(async {}),
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: parent.session_id.clone(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(1800),
            started_at: chrono::Utc::now(),
            allowed_tools: None,
        },
    );
    let err = dc.resume_timed_out(&sub.id, None).unwrap_err();
    assert!(format!("{:#}", err).contains("already running"));
}

#[tokio::test]
async fn resume_timed_out_default_budget_doubles_original_with_floor_and_ceiling() {
    let (dc, manager, _dir) = coordinator_with_backend();
    let backend = manager.backend();

    // Issue #111 repro: a small original budget (15s) must not default to
    // itself again — the delegation only reaches `resume` after that
    // budget already ran out once. 15 * 2 = 30, below the 600s floor.
    let parent = manager.get_or_create_context("mock:default:u1");
    let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    backend
        .save_delegation_checkpoint(&checkpoint_for(&sub.id, &parent.session_id, "timed_out", 15))
        .unwrap();
    dc.resume_timed_out(&sub.id, None).unwrap();
    assert_eq!(backend.load_delegation_checkpoint(&sub.id).unwrap().timeout_secs, 600);

    // Original budget large enough that doubling it clears the floor:
    // 500 * 2 = 1000.
    let sub2 = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    backend
        .save_delegation_checkpoint(&checkpoint_for(&sub2.id, &parent.session_id, "timed_out", 500))
        .unwrap();
    dc.resume_timed_out(&sub2.id, None).unwrap();
    assert_eq!(backend.load_delegation_checkpoint(&sub2.id).unwrap().timeout_secs, 1000);

    // Doubling past the global ceiling still clamps to 1800.
    let sub3 = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    backend
        .save_delegation_checkpoint(&checkpoint_for(&sub3.id, &parent.session_id, "timed_out", 1000))
        .unwrap();
    dc.resume_timed_out(&sub3.id, None).unwrap();
    assert_eq!(backend.load_delegation_checkpoint(&sub3.id).unwrap().timeout_secs, 1800);
}

/// issue #251/#252 (end to end): the sub-agent completed naturally between
/// the original timeout and `agent_resume` — its own history already ends
/// with a clean final answer. `resume_timed_out` must deliver THAT existing
/// result as a genuine `Completed` event (not a spurious `Failed` from
/// misreading `run_recovery`'s "nothing to do" as a failure), and the
/// parent's suspension must actually clear afterward instead of leaving a
/// permanent ghost `pending` entry (the #252 stale-repeat-call dedup would
/// otherwise swallow this second terminal for the same sub_session_id,
/// since the FIRST — the original TimedOut — already occupies the window).
#[tokio::test]
async fn resume_timed_out_already_complete_delivers_existing_result_not_a_spurious_failure() {
    use crate::agents::session_context::TerminalRecord;
    use crate::agents::turn::SubStatus;

    let (dc, manager, _dir) = coordinator_with_backend();
    dc.set_runtime(crate::agents::agent::tests::bailing_runtime());
    let (tx, mut rx) = mpsc::channel(8);
    dc.set_event_sender(tx);

    let parent = manager.get_or_create_context("mock:default:u1");
    let sub = manager.create_sub_session(&parent.session_id, "coder").unwrap();
    manager
        .backend()
        .save_delegation_checkpoint(&checkpoint_for(&sub.id, &parent.session_id, "timed_out", 15))
        .unwrap();

    // The original timeout already delivered and recorded — pending is
    // empty again, `results` holds the TimedOut entry, and the
    // stale-repeat-call window now contains this sub_session_id.
    parent.add_pending_task(sub.id.clone());
    let first = parent.record_terminal(sub.id.clone(), SubStatus::TimedOut, "timed out".into(), 0, "test");
    assert!(matches!(first, TerminalRecord::Recorded(_)));
    assert!(!parent.has_pending_async_work());

    // Simulate the sub-agent's own natural completion landing on disk
    // (its notification was lost to the SAME dedup window — that half of
    // #251 is a separate, harder race; this test covers what `agent_resume`
    // does once called).
    {
        let sub_ctx = manager.load_context_by_session_id(&sub.id).unwrap();
        let mut session = sub_ctx.session.lock().await;
        session.add_user("run the task".to_string());
        session.persist_last();
        session.add_assistant_with_tools(
            "TIMEOUT-TEST-abc123 all done".to_string(),
            vec![],
            None,
            None,
            None,
            None,
        );
        session.persist_last();
    }

    let resumed = dc.resume_timed_out(&sub.id, Some(30)).unwrap();
    assert_eq!(resumed, sub.id);

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("recover_async's spawned task must produce an event")
        .expect("event channel must not be closed");
    match event {
        DelegationEvent::Completed { sub_session_id, summary, .. } => {
            assert_eq!(sub_session_id, sub.id);
            assert!(summary.contains("TIMEOUT-TEST-abc123"), "got: {summary}");
        }
        other => panic!("expected Completed with the existing result, got {other:?}"),
    }

    // Route it through record_terminal exactly as `wake()` would — pending
    // must actually clear, not stay stuck as a ghost entry.
    let second = parent.record_terminal(sub.id.clone(), SubStatus::Completed, "...".into(), 0, "test");
    assert!(
        matches!(second, TerminalRecord::Recorded(_)),
        "the resume's own terminal must not be swallowed by the #224 stale-repeat-call window: {second:?}"
    );
    assert!(
        !parent.has_pending_async_work(),
        "pending must clear — no ghost entry left behind"
    );
}

#[test]
fn check_depth_three_level_chain_boundary() {
    let (dc, manager) = coordinator(3);
    let main = manager.get_or_create_context("mock:default:u1");
    let main_id = main.session_id.clone();
    assert!(dc.check_depth(&main_id).is_ok());
    let sub1 = manager.create_sub_session(&main_id, "coder").unwrap();
    assert!(dc.check_depth(&sub1.id).is_ok());
    let sub2 = manager.create_sub_session(&sub1.id, "coder").unwrap();
    let err = dc.check_depth(&sub2.id).unwrap_err();
    assert!(
        err.to_string().contains("maximum delegation depth exceeded"),
        "unexpected error: {err}"
    );
}

#[test]
fn max_depth_one_rejects_all() {
    let (dc, manager) = coordinator(1);
    let main = manager.get_or_create_context("mock:default:u1");
    let err = dc.check_depth(&main.session_id).unwrap_err();
    assert!(err.to_string().contains("maximum delegation depth exceeded"));
}

#[tokio::test]
async fn spawn_async_registers_pending_and_running() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let parent_id = parent.session_id.clone();
    let sub_session_id = dc
        .spawn_delegate_async("coder", "do the thing", &parent_id, 60, None, None)
        .unwrap();
    assert!(
        sub_session_id.contains("/s/"),
        "sub_session_id should be a session FQID: {sub_session_id}"
    );
    // current_thread runtime: the spawned body has not been polled yet, so
    // both tables are deterministically populated. The test body never
    // awaits, so the background task is cancelled at runtime drop.
    assert_eq!(dc.running_snapshot(), vec![sub_session_id.clone()]);
    assert_eq!(dc.running_count(), 1);
    let snap = parent.suspension_snapshot().unwrap();
    assert_eq!(snap.pending, vec![sub_session_id]);
}

#[tokio::test]
async fn spawn_async_depth_rejected_without_pending_or_running() {
    let (dc, manager) = coordinator(1);
    let parent = manager.get_or_create_context("mock:default:u1");
    let err = dc
        .spawn_delegate_async("coder", "task", &parent.session_id, 60, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("maximum delegation depth exceeded"));
    assert!(parent.suspension_snapshot().is_none());
    assert!(dc.running_snapshot().is_empty());
}

#[tokio::test]
async fn spawn_async_unknown_agent_is_rejected() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let err = dc
        .spawn_delegate_async("nope", "task", &parent.session_id, 60, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("Unknown sub-agent"));
    assert!(dc.running_snapshot().is_empty());
}

#[tokio::test]
async fn cancel_broadcasts_failed_cancelled_to_parent_session() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let (tx, mut rx) = mpsc::channel(8);
    dc.set_event_sender(tx);

    // Hand-instrument a running entry so the test doesn't depend on the
    // background spawn path.
    let sub_session_id = "test/s/sub".to_string();
    // Production path creates a durable checkpoint at spawn; cancel's
    // tombstone only updates an existing entry.
    let checkpoint = crate::storage::DelegationCheckpoint {
        parent_session_id: parent.session_id.clone(),
        sub_session_id: sub_session_id.clone(),
        agent_name: "coder".to_string(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs: 60,
        allowed_tools: None,
        last_checkpoint: None,
    };
    dc.session_manager
        .backend()
        .save_delegation_checkpoint(&checkpoint)
        .unwrap();
    dc.running.insert(
        sub_session_id.clone(),
        RunningEntry {
            handle: tokio::spawn(async {}),
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: parent.session_id.clone(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(60),
            started_at: chrono::Utc::now(),
            allowed_tools: None,
        },
    );

    assert!(dc.cancel(&sub_session_id).await);
    let ev = rx.recv().await.unwrap();
    match ev {
        DelegationEvent::Failed {
            sub_session_id: t,
            parent_session_id: s,
            error,
        } => {
            assert_eq!(t, sub_session_id);
            assert_eq!(s, parent.session_id);
            assert_eq!(error, "cancelled");
        }
        other => panic!("expected Failed(cancelled), got {other:?}"),
    }
    assert!(dc.running_snapshot().is_empty());

    // Tombstone is terminal Cancelled → restart skips resume (no
    // duplicate execution), unlike the hot-switch "checkpointed" path.
    let cp = dc
        .session_manager
        .backend()
        .load_delegation_checkpoint(&sub_session_id)
        .unwrap();
    assert_eq!(cp.status, "cancelled");

    // Exactly one event: the two-layer spawn's cancelled branch must NOT
    // emit a second Failed (caller owns the terminal event). Give the
    // aborted task's cleanup tail a chance to run on the current_thread
    // runtime, then confirm the channel is quiet.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(
        rx.try_recv().is_err(),
        "cancel must emit exactly one Failed(cancelled) event"
    );
}

#[tokio::test]
async fn cancel_unknown_sub_session_returns_false() {
    let (dc, _m) = coordinator(3);
    assert!(!dc.cancel("no-such-session").await);
}

#[test]
fn checkpoint_roundtrip_via_backend() {
    let (dc, manager) = coordinator(3);
    let _ = dc; // not needed — we test the backend directly
    let backend = manager.backend();
    let cp = crate::storage::DelegationCheckpoint {
        parent_session_id: "test/s/parent".to_string(),
        sub_session_id: "test/s/sub".to_string(),
        agent_name: "coder".to_string(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs: 300,
        allowed_tools: Some(vec!["shell".to_string()]),
        last_checkpoint: None,
    };
    backend.save_delegation_checkpoint(&cp).unwrap();

    let loaded = backend.load_delegation_checkpoints();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].sub_session_id, "test/s/sub");
    assert_eq!(loaded[0].agent_name, "coder");
    assert_eq!(loaded[0].timeout_secs, 300);

    backend.delete_delegation_checkpoint("test/s/sub").unwrap();
    assert!(backend.load_delegation_checkpoints().is_empty());
}

/// 方案 A: terminal cleanup rewrites the checkpoint status (tombstone)
/// instead of deleting it — single-load roundtrip + status update +
/// idempotent update of a missing checkpoint.
#[test]
fn checkpoint_tombstone_update_via_backend() {
    let (dc, manager) = coordinator(3);
    let _ = dc; // not needed — we test the backend directly
    let backend = manager.backend();
    let cp = crate::storage::DelegationCheckpoint {
        parent_session_id: "test/s/parent".to_string(),
        sub_session_id: "test/s/sub".to_string(),
        agent_name: "coder".to_string(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs: 300,
        allowed_tools: Some(vec!["shell".to_string()]),
        last_checkpoint: None,
    };
    backend.save_delegation_checkpoint(&cp).unwrap();

    // Single-load roundtrip.
    let loaded = backend.load_delegation_checkpoint("test/s/sub").unwrap();
    assert_eq!(loaded.status, "running");
    assert!(backend.load_delegation_checkpoint("no-such-session").is_none());

    // Tombstone: the terminal status is persisted, not deleted.
    backend
        .update_delegation_checkpoint_status("test/s/sub", "timed_out")
        .unwrap();
    let loaded = backend.load_delegation_checkpoint("test/s/sub").unwrap();
    assert_eq!(loaded.status, "timed_out");
    assert_eq!(loaded.agent_name, "coder"); // other fields preserved
    assert_eq!(backend.load_delegation_checkpoints().len(), 1); // still on disk

    // Idempotent: updating a missing checkpoint is a no-op.
    backend
        .update_delegation_checkpoint_status("no-such-session", "failed")
        .unwrap();

    backend.delete_delegation_checkpoint("test/s/sub").unwrap();
    assert!(backend.load_delegation_checkpoints().is_empty());
}

#[tokio::test]
async fn checkpoint_and_cancel_all_empties_running_and_writes_checkpoints() {
    let (dc, manager) = coordinator(3);
    let backend = manager.backend();

    // Insert a hand-crafted running entry.
    let sub_session_id = "test/s/sub".to_string();
    dc.running.insert(
        sub_session_id.clone(),
        RunningEntry {
            handle: tokio::spawn(async {}),
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: "parent".to_string(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(60),
            started_at: chrono::Utc::now(),
            allowed_tools: None,
        },
    );
    assert_eq!(dc.running_count(), 1);

    dc.checkpoint_and_cancel_all();

    // Running table should be empty.
    assert_eq!(dc.running_count(), 0);
    // Checkpoint should be persisted with status "checkpointed".
    let loaded = backend.load_delegation_checkpoints();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].status, "checkpointed");
    assert_eq!(loaded[0].sub_session_id, sub_session_id);
}

#[test]
fn load_checkpoints_returns_persisted_checkpoints() {
    let (dc, manager) = coordinator(3);
    let backend = manager.backend();
    let cp = crate::storage::DelegationCheckpoint {
        parent_session_id: "parent".to_string(),
        sub_session_id: "sub".to_string(),
        agent_name: "coder".to_string(),
        status: "checkpointed".to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs: 120,
        allowed_tools: None,
        last_checkpoint: Some(chrono::Utc::now()),
    };
    backend.save_delegation_checkpoint(&cp).unwrap();

    let loaded = dc.load_checkpoints();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].sub_session_id, "sub");
    assert_eq!(loaded[0].status, "checkpointed");
}

#[test]
fn worktree_branch_name_uses_subagent_prefix() {
    assert_eq!(
        worktree_branch_name("coder", "deadbeef"),
        "subagent/coder_deadbeef"
    );
    assert_eq!(
        worktree_branch_name("main", "01234567"),
        "subagent/main_01234567"
    );
}

// ── Two-layer spawn panic safety net ───────────────────────────────

/// Verifies that when the inner sub-agent task panics, the outer spawn
/// catches it and converts the JoinError into a `DelegationEvent::Failed`
/// whose error message contains "panicked". The running table and mailbox
/// must also be cleaned so the parent agent does not hang.
///
/// Because `spawn_delegate_async` calls `delegate_with_parent` (which
/// returns `Err`, not a panic, when no runtime is installed), this test
/// exercises the two-layer pattern directly: it spawns an outer task
/// whose inner task panics, using the coordinator's real `running`,
/// `mailboxes`, and event-channel infrastructure — the same code path
/// `spawn_delegate_async` uses after the fix.
#[tokio::test]
async fn panic_in_inner_task_emits_failed_and_cleans_running() {
    let (dc, manager) = coordinator(3);
    let parent = manager.get_or_create_context("mock:default:u1");
    let (tx, mut rx) = mpsc::channel(8);
    dc.set_event_sender(tx);

    let sub_session_id = "test/s/panic-sub".to_string();
    let parent_session_id = parent.session_id.clone();

    // Production path creates a durable checkpoint before spawning;
    // persist_terminal_checkpoint only updates an existing entry.
    let checkpoint = crate::storage::DelegationCheckpoint {
        parent_session_id: parent.session_id.clone(),
        sub_session_id: sub_session_id.clone(),
        agent_name: "coder".to_string(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs: 60,
        allowed_tools: None,
        last_checkpoint: None,
    };
    dc.session_manager
        .backend()
        .save_delegation_checkpoint(&checkpoint)
        .unwrap();

    // Set up the coordinator's internal tables just like
    // `spawn_delegate_async` does before spawning.
    let running = Arc::clone(&dc.running);
    let mailboxes = Arc::clone(&dc.mailboxes);
    let event_tx = dc.event_sender();
    let running_sub_session_id = sub_session_id.clone();
    let sub_session_id_clone = sub_session_id.clone();
    let sub_delegator = dc.clone();

    let handle = tokio::spawn(async move {
        let start_time = std::time::Instant::now();

        // Inner task that panics — simulates a sub-agent whose body
        // panics during execution.
        let inner_handle: tokio::task::JoinHandle<anyhow::Result<String>> =
            tokio::spawn(async {
                panic!("inner task exploded");
            });

        let result = match inner_handle.await {
            Ok(r) => r,
            Err(je) if je.is_panic() => {
                let payload = je.into_panic();
                let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic payload".to_string()
                };
                tracing::error!(panic = %msg, "sub-agent panicked");
                Err(anyhow::anyhow!("sub-agent panicked: {}", msg))
            }
            Err(je) if je.is_cancelled() => {
                running.remove(&running_sub_session_id);
                mailboxes.remove(&running_sub_session_id);
                return;
            }
            Err(je) => Err(anyhow::anyhow!("join error: {}", je)),
        };

        // ── Collection logic (mirrors spawn_delegate_async) ───────
        let duration_secs = start_time.elapsed().as_secs();
        let timed_out_secs = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<DelegationTimeout>())
            .map(|t| t.secs);
        let sent_message_count = running
            .get(&running_sub_session_id)
            .map(|e| e.messages_sent.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);

        if let Some(tx) = &event_tx {
            match (&result, timed_out_secs) {
                (Ok(summary), _) => {
                    let _ = tx
                        .send(DelegationEvent::Completed {
                            sub_session_id: sub_session_id_clone.clone(),
                            parent_session_id: parent_session_id.clone(),
                            summary: summary.clone(),
                            duration_secs,
                            sent_message_count,
                        })
                        .await;
                }
                (Err(_), Some(secs)) => {
                    let _ = tx
                        .send(DelegationEvent::TimedOut {
                            sub_session_id: sub_session_id_clone.clone(),
                            parent_session_id: parent_session_id.clone(),
                            timeout_secs: secs,
                            duration_secs,
                        })
                        .await;
                }
                (Err(e), None) => {
                    let _ = tx
                        .send(DelegationEvent::Failed {
                            sub_session_id: sub_session_id_clone.clone(),
                            parent_session_id: parent_session_id.clone(),
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        }

        let terminal = if timed_out_secs.is_some() {
            DelegationStatus::TimedOut
        } else if result.is_ok() {
            DelegationStatus::Completed
        } else {
            DelegationStatus::Failed
        };
        if let Some(entry) = running.get(&running_sub_session_id) {
            if let Ok(mut status) = entry.status.write() {
                *status = terminal;
            }
        }
        running.remove(&running_sub_session_id);
        mailboxes.remove(&running_sub_session_id);
        if terminal == DelegationStatus::Completed {
            let _ = sub_delegator
                .session_manager
                .backend()
                .delete_delegation_checkpoint(&running_sub_session_id);
        } else {
            sub_delegator.persist_terminal_checkpoint(&running_sub_session_id, terminal);
        }
    });

    dc.running.insert(
        sub_session_id.clone(),
        RunningEntry {
            handle,
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: parent.session_id.clone(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(60),
            started_at: chrono::Utc::now(),
            allowed_tools: None,
        },
    );

    // Wait for the Failed event (panic path → no DelegationTimeout →
    // falls into the generic Err branch).
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed");
    match event {
        DelegationEvent::Failed { error, .. } => {
            assert!(
                error.contains("panicked"),
                "error should mention panic: {error}"
            );
        }
        other => panic!("expected Failed event, got {other:?}"),
    }

    // Give the cleanup tail a chance to run on the current_thread
    // runtime, then verify the running table and checkpoint tombstone.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(
        dc.running.get(&sub_session_id).is_none(),
        "running table should be cleaned up after panic"
    );
    let cp = dc
        .session_manager
        .backend()
        .load_delegation_checkpoint(&sub_session_id);
    assert!(
        cp.is_some_and(|c| c.status == "failed"),
        "checkpoint should carry a 'failed' tombstone after panic"
    );
}

// ── RunningEntry timeout_secs / started_at checkpoint fallback ──────

/// `checkpoint_and_cancel_all`'s fallback (no existing durable
/// checkpoint) must use the `RunningEntry`'s actual `timeout_secs` and
/// `started_at` instead of hardcoded defaults (600 s / now).
#[tokio::test]
async fn checkpoint_fallback_uses_running_entry_timeout_and_started_at() {
    let (dc, manager) = coordinator(3);
    let backend = manager.backend();

    let started = chrono::Utc::now() - chrono::Duration::seconds(100);
    let sub_session_id = "test/s/fallback-sub".to_string();
    dc.running.insert(
        sub_session_id.clone(),
        RunningEntry {
            handle: tokio::spawn(async {}),
            sub_ctx: manager.get_or_create_context("mock:default:u1"),
            status: std::sync::RwLock::new(DelegationStatus::Running),
            agent_name: "coder".to_string(),
            parent_session_id: "parent".to_string(),
            spawned_at: std::time::Instant::now(),
            messages_sent: std::sync::atomic::AtomicU64::new(0),
            timeout_secs: Some(42),
            started_at: started,
            allowed_tools: None,
        },
    );

    // No pre-existing checkpoint → triggers the fallback branch.
    assert!(backend
        .load_delegation_checkpoint(&sub_session_id)
        .is_none());

    dc.checkpoint_and_cancel_all();

    let cp = backend
        .load_delegation_checkpoint(&sub_session_id)
        .expect("checkpoint should be written");
    assert_eq!(
        cp.timeout_secs, 42,
        "fallback should use entry's timeout_secs"
    );
    assert_eq!(
        cp.started_at, started,
        "fallback should use entry's started_at, not Utc::now()"
    );
    assert_eq!(cp.status, "checkpointed");
}
