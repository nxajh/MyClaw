//! `SessionContext` — bundles a `Session` with everything the per-turn
//! pipeline needs.
//!
//! RFC v2 §三.A: introduces SessionContext as the boundary that owns
//! per-session mutable state (attachments, pending retry, turn lock) and
//! triggers `process_turn`. `Agent.run` takes `&mut Session` plus a
//! `TurnContext` (already-resolved decisions); SessionContext is what
//! the Orchestrator actually holds in its session table.
//!
//! This struct is intentionally additive in this commit — it is not yet
//! wired into the Orchestrator or AgentLoop. C18 (Agent.run rewrite) and
//! E29 (Orchestrator main loop) will start consuming it. Keeping the
//! scaffold here so downstream commits don't have to introduce both the
//! type and its callers in one go.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agents::attachment::AttachmentManager;
use crate::agents::session::Session;
use crate::agents::{Agent, AgentRuntime, TurnContext, UserProfile};
use crate::channels::{Channel, ChannelMessage, SendMessage};

/// Sent when the per-turn `Agent.run` returns Err. Mirrors the same
/// short notice the old AgentLoop surfaced — anyhow::Error strings can
/// leak internal paths, tool args, and stack hints, so we never forward
/// them verbatim to the user.
const MSG_TURN_FAILED: &str = "⚠️ 处理超时，未收到模型回复。";

/// Per-session bundle held by the SessionManager's session-context table.
///
/// Fields:
/// - `session`: owns the conversation state (Session — history, override, …)
/// - `agent`: Agent bound to this session (resolved from `Session.agent_name`
///   at construction time; reused across every turn so per-session
///   dispatch doesn't re-look-up the SubAgentConfig)
/// - `attachments`: per-session AttachmentManager (file uploads pending injection)
/// - `pending_retry`: user message saved when last turn ended abnormally;
///   surfaced as a "retry?" prompt next time the user types
/// - `turn_lock`: tokio Mutex held for the duration of `process_turn`,
///   ensuring two messages on the same session do not race the LLM
/// - `user_profile`: snapshot of `workspace/users/{user_id}/profile.toml` at
///   session load time. Used by SystemPromptBuilder.build_with_profile so
///   the LLM sees a stable "## User" section. Refreshed via
///   `reload_user_profile` when the profile file changes mid-session.
pub struct SessionContext {
    /// Mutable session state. Wrapped in Mutex so the turn lock and the
    /// Session itself share the same critical section.
    pub session: Arc<Mutex<Session>>,
    /// Agent bound to this session at creation time. Built from
    /// `Session.agent_name` via `SessionManager.build_agent_for_session`.
    pub agent: Arc<Agent>,
    /// Attachments awaiting injection on the next user turn.
    pub attachments: Arc<AttachmentManager>,
    /// User message saved when the previous turn ended with an empty LLM
    /// response or interrupted streaming. Cleared once retried.
    pub pending_retry: Arc<Mutex<Option<String>>>,
    /// Serializes `process_turn` per session. Distinct from `session`'s
    /// Mutex because some readers want to peek at session state without
    /// blocking on an in-flight turn.
    pub turn_lock: Arc<Mutex<()>>,
    /// Loaded UserProfile (G41). Wrapped in Mutex so /memory-style commands
    /// can rewrite it without taking the session lock.
    pub user_profile: Arc<Mutex<UserProfile>>,
}

impl SessionContext {
    pub fn new(session: Session, agent: Arc<Agent>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            agent,
            attachments: Arc::new(AttachmentManager::new()),
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            user_profile: Arc::new(Mutex::new(UserProfile::default())),
        }
    }

    /// Re-read profile.toml from disk and update the held profile.
    /// Returns the new profile so callers can log it.
    pub async fn reload_user_profile(
        &self,
        workspace_dir: &std::path::Path,
        user_id: &str,
    ) -> UserProfile {
        let p = UserProfile::load(workspace_dir, user_id);
        *self.user_profile.lock().await = p.clone();
        p
    }

    /// Snapshot the session for read-only consumers (e.g., /status commands).
    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }

    /// Run one user turn end-to-end: acquire the turn lock, append the
    /// user message, resolve TurnContext from the session override, invoke
    /// `Agent.run`, then dispatch the response (or error notice) back to
    /// the channel. This is the canonical per-turn entry point — the
    /// Orchestrator's inbound dispatch path spawns this on a background
    /// task per message so the main event loop stays unblocked.
    ///
    /// `inbound_msg` is replayed into `Session.record_inbound` against the
    /// canonical session held by this context (the upstream `SessionManager`
    /// cache copy was updated earlier; this keeps the two in sync).
    /// `content` is the user-visible text that gets appended via
    /// `Session::add_user`.
    pub async fn process_turn(
        self: &Arc<Self>,
        inbound_msg: ChannelMessage,
        content: String,
        channel: Arc<dyn Channel>,
        reply_target: String,
        runtime: AgentRuntime,
    ) {
        let _turn_guard = self.turn_lock.lock().await;
        let mut session = self.session.lock().await;

        session.record_inbound(inbound_msg);

        let persist_hook = runtime.persist.clone();
        session.persist = persist_hook.clone();
        session.channel = Some(channel.clone());

        let session_override = session.session_override.clone();
        let mut prompt_config = runtime.defaults.prompt.clone();
        if let Some(pm) = session_override.permission_mode {
            prompt_config.permission_mode = pm;
        }
        if let Some(rm) = session_override.run_mode {
            prompt_config.run_mode = rm;
        }

        let system_prompt = if !runtime.defaults.system_prompt.is_empty() {
            runtime.defaults.system_prompt.clone()
        } else {
            let s = runtime.skills.read();
            crate::agents::SystemPromptBuilder::new(prompt_config.clone()).build(&s)
        };

        session.add_user(content);
        if let Some(ref hook) = persist_hook {
            if let Some(last) = session.history.last().cloned() {
                if let Some(id) = hook.persist_message(&session.id, &last) {
                    if let Some(slot) = session.message_ids.last_mut() {
                        *slot = id;
                    }
                }
            }
        }

        let thinking = session_override.to_thinking_config();
        let model_id = session_override.model.as_deref();
        let turn_ctx = TurnContext {
            system_prompt: &system_prompt,
            model_id,
            thinking: thinking.as_ref(),
            permission_mode: prompt_config.permission_mode,
            run_mode: prompt_config.run_mode,
        };

        let result = self.agent.run(&mut session, turn_ctx, &runtime).await;
        match result {
            Ok(turn_result) => {
                if !turn_result.text.trim().is_empty() {
                    let send_msg = SendMessage {
                        content: turn_result.text,
                        recipient: reply_target,
                        subject: None,
                        thread_ts: None,
                        cancellation_token: None,
                        attachments: vec![],
                        image_urls: None,
                        inline_buttons: None,
                    };
                    if let Err(e) = channel.send(&send_msg).await {
                        tracing::error!(session = %session.id, err = %e, "send response failed");
                    }
                }
                if let Some(retry_msg) = turn_result.pending_retry {
                    *self.pending_retry.lock().await = Some(retry_msg);
                }
            }
            Err(e) => {
                tracing::error!(session = %session.id, err = %e, "Agent turn failed");
                let send_msg = SendMessage::new(MSG_TURN_FAILED, reply_target);
                let _ = channel.send(&send_msg).await;
            }
        }

        session.persist = None;
        session.channel = None;
    }
}
