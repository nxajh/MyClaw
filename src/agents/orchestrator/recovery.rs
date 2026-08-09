//! Startup recovery: resume turns interrupted by a previous crash / SIGKILL.
//!
//! Both persisted user sessions and unfinished sub-agents are recovered the
//! same way — lock the turn, replay via `Agent::run_recovery`, then deliver
//! the result. The two used to be near-identical copy-pasted functions; the
//! only real difference is *where the recovered output goes*, captured here by
//! [`CompletionSink`].

use std::sync::Arc;

use super::key::{SessionKey, SubAgentKey};
use super::turn::ResolvedTurn;
use crate::agents::session::SessionManager;
use crate::agents::{
    AgentRuntime, DelegationCoordinator, DelegationEvent, SessionContext, UnfinishedSubAgent,
};
use crate::channels::Channel;
use crate::storage::SessionBackend;

/// Where a recovered turn's output goes.
enum CompletionSink {
    /// Persisted user session: deliver the recovered text back to its channel.
    Channel {
        key: String,
        backend: Arc<dyn SessionBackend>,
        channels: super::ctx::ChannelRegistry,
    },
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
        match self {
            CompletionSink::Channel {
                key,
                backend,
                channels,
            } => {
                let recipient = backend
                    .load_last_message(&key)
                    .map(|m| m.receiver.id)
                    .unwrap_or_else(|| {
                        SessionKey::parse(&key)
                            .map(|k| k.sender)
                            .unwrap_or_default()
                    });
                let Some(parsed) = SessionKey::parse(&key) else {
                    return;
                };
                let Some(channel) = channels.get(&parsed.account_key()) else {
                    return;
                };
                let message = crate::channels::ChannelOutboundMessage {
                    receiver: crate::channels::MessageReceiver::new(&recipient),
                    content: crate::channels::ChannelMessageContent::text(&text),
                    options: Default::default(),
                };
                if let Err(e) = channel.send_message(&message).await {
                    tracing::warn!(session = %key, err = %e, "startup recovery: failed to send response");
                }
            }
            CompletionSink::Delegate {
                task_id,
                parent_session_id,
                reply_target: _,
                delegator,
            } => {
                if let Some(dm) = delegator {
                    if let Some(tx) = dm.event_sender() {
                        let _ = tx
                            .send(DelegationEvent::Completed {
                                task_id,
                                session_id: parent_session_id,
                                summary: text,
                                duration_secs: 0,
                            })
                            .await;
                    }
                }
            }
        }
    }
}

/// Spawn the recovery turn for one session. The LLM work runs in the
/// background so the event loop starts without blocking.
///
/// `channel` (when present) is re-attached to the session before the resumed
/// turn runs: startup recovery bypasses `process_turn` (which is the only
/// place that normally installs `session.channel`), so without this the
/// recovered turn would lose channel-scoped tools such as `send_message`.
fn spawn_recovery(
    turn_tracker: super::ctx::SharedTurnTracker,
    session_ctx: Arc<SessionContext>,
    runtime: AgentRuntime,
    label: &'static str,
    id: String,
    sink: CompletionSink,
    channel: Option<Arc<dyn Channel>>,
) {
    tokio::spawn(async move {
        let _guard = turn_tracker.track();
        let _turn_guard = session_ctx.turn_lock.lock().await;
        let mut session = session_ctx.session.lock().await;
        if let Some(ch) = channel {
            session.channel = Some(ch);
        }

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
            }
        }
    });
}

/// Scan persisted user sessions and unfinished sub-agents for incomplete turns
/// and resume them. SessionContexts are registered synchronously (so new
/// messages can queue immediately); the LLM recovery work spawns in the
/// background.
pub(super) fn run_startup(
    sessions: &Arc<SessionManager>,
    runtime: &AgentRuntime,
    channels: &super::ctx::ChannelRegistry,
    unfinished: &[UnfinishedSubAgent],
    delegator: &Option<Arc<DelegationCoordinator>>,
    turn_tracker: &super::ctx::SharedTurnTracker,
) {
    // `list_all_sessions` returns one entry per persisted session *record*, but
    // recovery is keyed by `owner` (the routing key) and always resumes that
    // owner's *active* session. An owner with N historical sessions would
    // otherwise spawn N identical recovery tasks for the same active session
    // (they serialize on the shared turn_lock, so N-1 are wasted no-ops). Dedup
    // on the owner key so each active session is recovered exactly once.
    let mut seen_owners = std::collections::HashSet::new();
    for info in sessions.list_all_sessions() {
        let key = info.owner;
        if !seen_owners.insert(key.clone()) {
            continue;
        }
        let snap = sessions.get_or_create(&key);
        if snap.history.is_empty() || !super::history_has_incomplete_turn(&snap.history) {
            continue;
        }
        // `get_or_create(&key)` resolves the owner's *active* session, so only
        // that session is ever resumed here. Inactive sessions are left for the
        // normal message path: when the user switches back and sends a message,
        // `dispatch_turn` → `process_turn` re-installs the channel and
        // `Agent::run` continues the interrupted turn naturally.
        tracing::info!(session = %key, "startup recovery: found incomplete turn, spawning background task");
        let session_ctx = sessions.get_or_create_context(&key);
        // Startup recovery bypasses `process_turn` (the only place that
        // normally installs `session.channel`); re-attach the channel here so
        // channel-scoped tools such as `send_message` stay available during the
        // resumed turn.
        let channel = SessionKey::parse(&key).and_then(|k| channels.get(&k.account_key()));
        spawn_recovery(
            turn_tracker.clone(),
            session_ctx,
            runtime.clone(),
            "startup recovery",
            key.clone(),
            CompletionSink::Channel {
                key,
                backend: Arc::clone(sessions.backend()),
                channels: channels.clone(),
            },
            channel,
        );
    }

    for sa in unfinished {
        if sa.sub_session_id.is_empty() || sa.session_key.is_empty() {
            tracing::debug!(task_id = %sa.task_id, "sub-agent recovery: skipping (no session_id or session_key)");
            continue;
        }
        let sub_sk = SubAgentKey::new(&sa.agent_name, &sa.sub_session_id).to_string();
        let snap = sessions.get_or_create(&sub_sk);
        if snap.history.is_empty() || !super::history_has_incomplete_turn(&snap.history) {
            continue;
        }
        tracing::info!(task_id = %sa.task_id, agent = %sa.agent_name, "sub-agent startup recovery: found incomplete turn, spawning background task");
        let session_ctx = sessions.get_or_create_context(&sub_sk);
        spawn_recovery(
            turn_tracker.clone(),
            session_ctx,
            runtime.clone(),
            "sub-agent startup recovery",
            sa.task_id.clone(),
            CompletionSink::Delegate {
                task_id: sa.task_id.clone(),
                parent_session_id: sa.session_key.clone(),
                reply_target: sa.reply_target.clone(),
                delegator: delegator.clone(),
            },
            None,
        );
    }
}
