//! `SessionContext` — bundles a `Session` with everything the per-turn
//! pipeline needs.
//!
//! RFC v2 §三.A: SessionContext is the boundary that owns per-session
//! mutable state (attachments, pending retry, turn lock) and drives
//! `process_turn`. `Agent::run` takes `&mut Session` plus a `TurnContext`
//! (already-resolved decisions); SessionContext is what the Orchestrator
//! holds in its session table and what `process_turn` operates on per turn.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agents::attachment::AttachmentManager;
use crate::agents::session::Session;
use crate::agents::turn::TurnResult;
use crate::agents::{Agent, AgentRuntime, TurnContext, UserProfile};
use crate::channels::{Channel, ChannelMessage};

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
    /// Attachments awaiting injection on the next user turn. Mutex
    /// because `AttachmentManager.diff_*` mutate pending state; the
    /// outer Arc lives on the SessionContext itself, so a plain
    /// `Mutex<AttachmentManager>` here is sufficient.
    pub attachments: Mutex<AttachmentManager>,
    /// User message saved when the previous turn ended with an empty LLM
    /// response or interrupted streaming. Cleared once retried.
    pub pending_retry: Arc<Mutex<Option<String>>>,
    /// Serializes `process_turn` per session. Distinct from `session`'s
    /// Mutex because some readers want to peek at session state without
    /// blocking on an in-flight turn.
    pub turn_lock: Arc<Mutex<()>>,
    /// Loaded UserProfile snapshot taken at SessionContext creation.
    /// Immutable for the lifetime of the context — per RFC §三.A reload
    /// semantics drop the SessionContext and let `SessionManager`
    /// rematerialize it from a fresh profile read.
    pub user_profile: Arc<UserProfile>,
}

impl SessionContext {
    pub fn new(session: Session, agent: Arc<Agent>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            agent,
            attachments: Mutex::new(AttachmentManager::new()),
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            user_profile: Arc::new(UserProfile::default()),
        }
    }

    /// Build with a pre-loaded user profile (the path
    /// `SessionManager::get_or_create_context_with` takes).
    pub fn with_profile(session: Session, agent: Arc<Agent>, profile: Arc<UserProfile>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            agent,
            attachments: Mutex::new(AttachmentManager::new()),
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            user_profile: profile,
        }
    }

    /// Snapshot the session for read-only consumers (e.g., /status commands).
    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }

    /// Run one turn end-to-end: acquire the turn lock, replay the
    /// inbound message, resolve `TurnContext` from the session override,
    /// invoke `Agent.run`, and return the result. The caller is
    /// responsible for dispatching the response — user-message paths
    /// send via `channel.send`; scheduled paths route through the
    /// scheduler's `send_to_target_internal`.
    ///
    /// `channel` is `Some` when an originating channel exists (user
    /// turn or scheduler turn with a "last channel" handle). It's
    /// stored on `session.channel` so per-turn tools like `ask_user`
    /// can reach it; `None` means tools requiring a channel will error.
    ///
    /// On `Ok(TurnResult)`, the caller may inspect `text` /
    /// `pending_retry`. The latter is also stashed on
    /// `SessionContext.pending_retry` here so the next inbound for the
    /// same session can offer a retry prompt.
    pub async fn process_turn(
        self: &Arc<Self>,
        inbound_msg: ChannelMessage,
        channel: Option<Arc<dyn Channel>>,
        runtime: AgentRuntime,
    ) -> anyhow::Result<TurnResult> {
        let _turn_guard = self.turn_lock.lock().await;
        let mut session = self.session.lock().await;

        let content = inbound_msg.content.clone();
        let reply_target = inbound_msg.reply_target.clone();
        session.record_inbound(inbound_msg);

        // Session.persist was wired at SessionContext creation by
        // SessionManager; capture a clone so the post-turn `add_user`
        // persistence call sees the same hook.
        let persist_hook = session.persist.clone();
        let channel_for_send = channel.clone();
        // RFC §7.6: install per-turn streaming handle BEFORE Agent::run.
        // Channels that don't support streaming return None; the
        // fallback send block below covers them.
        session.turn_stream = channel
            .as_ref()
            .and_then(|ch| ch.create_stream(&reply_target));
        session.channel = channel;

        let session_override = session.session_override.clone();
        let mut prompt_config = runtime.defaults.prompt.clone();
        if let Some(pm) = session_override.permission_mode {
            prompt_config.permission_mode = pm;
        }
        if let Some(rm) = session_override.run_mode {
            prompt_config.run_mode = rm;
        }

        // Sub-agent delegations set `system_prompt_override` to bypass
        // the full SystemPromptBuilder (sub-agents want a minimal
        // AGENT.md-derived identity prompt, not the full behavioral
        // ruleset).
        let system_prompt = match &session_override.system_prompt_override {
            Some(custom) => custom.clone(),
            None => runtime.build_system_prompt(&prompt_config),
        };

        // RFC §三.A line 312-323: process_turn computes the attachment
        // delta (skills/agents/MCP/memory/date/autonomy) against the
        // history's announced state and prepends a <system-reminder> to
        // the user content before recording the turn.
        let reminder = {
            let mut attachments = self.attachments.lock().await;
            let skills_snap = runtime.skills.read();
            attachments.diff_skills(&skills_snap, &session.history);
            let agent_list: Vec<(String, String)> = runtime
                .agents
                .values_cloned()
                .into_iter()
                .map(|a| (a.config.name.clone(), a.config.description.clone().unwrap_or_default()))
                .collect();
            attachments.diff_agents(&agent_list, &session.history);
            // Date injection respects the configured [prompt] timezone_offset
            // (sourced from the shared ResourceProvider via ContextEngine).
            attachments.diff_date(runtime.context_engine.timezone_offset(), &session.history);
            attachments.diff_autonomy(&prompt_config.permission_mode);
            let text = attachments.build_text(&skills_snap);
            attachments.clear_pending();
            text
        };

        let user_content = match reminder {
            Some(rem) => format!("{}\n\n{}", rem, content),
            None => content,
        };
        // Build media (image) content parts from the inbound message so the
        // image-bearing user message is recorded in history (and externalized
        // by the persist hook below). `image_urls` and `image_base64` come from
        // the channel's `ChannelMessage`.
        let media_parts: Vec<crate::providers::ContentPart> = {
            use crate::providers::{ContentPart, ImageDetail};
            let mut parts = Vec::new();
            if let Some(msg) = session.last_message.as_ref() {
                if let Some(urls) = msg.image_urls.as_ref() {
                    for url in urls {
                        parts.push(ContentPart::ImageUrl {
                            url: url.clone(),
                            detail: ImageDetail::Auto,
                        });
                    }
                }
                if let Some(b64s) = msg.image_base64.as_ref() {
                    for b64 in b64s {
                        parts.push(ContentPart::ImageB64 {
                            b64_json: b64.clone(),
                            media_type: None,
                            detail: ImageDetail::Auto,
                        });
                    }
                }
            }
            parts
        };
        if media_parts.is_empty() {
            session.add_user(user_content);
        } else {
            session.add_user_with_media(user_content, media_parts);
        }
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
        // Per-turn channel + turn_stream are transient. Consume the
        // stream first (RFC §7.6): finish on success delivers FinalDelivered;
        // abort on error cancels the WS transport.
        let turn_stream = session.turn_stream.take();
        session.channel = None;

        match (result, turn_stream) {
            (Ok(turn_result), stream) => {
                if let Some(ref retry_msg) = turn_result.pending_retry {
                    *self.pending_retry.lock().await = Some(retry_msg.clone());
                }
                // RFC §7.6: consume the stream first; its `finish()` reports
                // how far the streaming path actually got. Fall back to
                // `send_payload`/`send` whenever delivery did NOT reach
                // `FinalDelivered` — covers three cases uniformly:
                //   1. No stream at all (non-streaming channel) → Pending
                //   2. Stream existed but transport failed mid-turn → Visible
                //      or Pending (push_or_drop in Agent::run set turn_stream
                //      to None on Err; finish() returns Pending here)
                //   3. Stream existed and acked everything → FinalDelivered
                //      → skip fallback (avoids the double-display bug)
                let delivery = match stream {
                    Some(s) => s.finish().await,
                    None => crate::channels::StreamDelivery::Pending,
                };
                if delivery != crate::channels::StreamDelivery::FinalDelivered {
                    if let Some(ch) = channel_for_send {
                        if !turn_result.text.trim().is_empty() {
                            let send_msg = crate::channels::SendMessage::new(
                                turn_result.text.clone(),
                                reply_target.clone(),
                            );
                            if let Err(e) = ch.send(&send_msg).await {
                                tracing::error!(
                                    session = %session.id,
                                    err = %e,
                                    "process_turn: fallback send failed"
                                );
                            }
                        }
                    }
                }
                Ok(turn_result)
            }
            (Err(e), stream) => {
                if let Some(s) = stream {
                    s.abort().await;
                }
                tracing::error!(session = %session.id, err = %e, "Agent turn failed");
                Err(e)
            }
        }
    }
}
