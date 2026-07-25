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
use crate::channels::{Channel, ChannelInboundMessage};
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
        inbound_msg: ChannelInboundMessage,
        channel: Option<Arc<dyn Channel>>,
        runtime: AgentRuntime,
    ) -> anyhow::Result<TurnResult> {
        let _turn_guard = self.turn_lock.lock().await;
        let mut session = self.session.lock().await;

        let content = inbound_msg.content.text.clone();
        let reply_target = inbound_msg.receiver.id.clone();

        // Persist inbound files to session-local storage so their lifetime
        // matches the session.  Read the body stream (via
        // ChannelFileBody::open) and write it under <session_dir>/files/.
        // Falls back to the adapter's temp-file path_hint when the backend
        // cannot persist (e.g. MemoryBackend in tests).
        let session_id = session.id.clone();
        let media_parts: Vec<crate::providers::ContentPart> = {
            use crate::providers::ContentPart;
            let mut parts = Vec::new();
            for file in &inbound_msg.content.files {
                let path = match &session.persist {
                    Some(hook) => {
                        let mut reader = file.body.open().await?;
                        let mut data = Vec::new();
                        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut data).await?;
                        match hook.save_file(
                            &session_id,
                            Some(&file.meta.file_name),
                            &data,
                            file.meta.mime_type.as_deref(),
                        ) {
                            Some(saved) => saved.path,
                            None => match file.body.path_hint() {
                                Some(hint) => hint.to_string(),
                                None => {
                                    tracing::warn!(
                                        file = %file.meta.file_name,
                                        "inbound file persist failed and no path_hint; skipping"
                                    );
                                    continue;
                                }
                            },
                        }
                    }
                    None => match file.body.path_hint() {
                        Some(hint) => hint.to_string(),
                        None => {
                            tracing::warn!(
                                file = %file.meta.file_name,
                                "no persist hook and no path_hint; skipping"
                            );
                            continue;
                        }
                    },
                };
                parts.push(ContentPart::File {
                    path,
                    mime_type: file.meta.mime_type.clone(),
                    name: Some(file.meta.file_name.clone()),
                    size_bytes: file.meta.size_bytes,
                });
            }
            parts
        };

        // Session.persist was wired at SessionContext creation by
        // SessionManager; capture a clone so the post-turn `add_user`
        // persistence call sees the same hook.
        let persist_hook = session.persist.clone();
        // Snapshot before clearing — bare-"继续" soft-block uses this.
        let was_incomplete_turn = session.incomplete_turn;
        // A new turn owns the incomplete-turn state from here on.
        session.incomplete_turn = false;
        // Record inbound and persist last_message safely under turn_lock.
        session.record_inbound(inbound_msg.clone());
        if let Some(hook) = &persist_hook {
            if let Some(ref msg) = session.last_message {
                hook.save_last_message(&session.id, msg);
            }
        }
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
                .map(|a| {
                    (
                        a.config.name.clone(),
                        a.config.description.clone().unwrap_or_default(),
                    )
                })
                .collect();
            attachments.diff_agents(&agent_list, &session.history);
            // Date injection respects the configured [prompt] timezone_offset
            // (sourced from the shared ResourceProvider via ContextEngine).
            attachments.diff_date(runtime.context_engine.timezone_offset(), &session.history);
            attachments.diff_autonomy(&prompt_config.permission_mode, &session.history);
            // Inject user/feedback memory index as system-reminder.
            let knowledge_dir = &runtime.defaults.prompt.knowledge_dir;
            if !knowledge_dir.is_empty() {
                let memory_dir = std::path::Path::new(knowledge_dir);
                let files = crate::memory::scan_memory_files(memory_dir);
                let entries: Vec<crate::memory::IndexEntry> =
                    files.iter().map(crate::memory::IndexEntry::from).collect();
                attachments.diff_memory(&entries, &session.history);
            }
            let text = attachments.build_text(&skills_snap);
            attachments.clear_pending();
            text
        };

        // P3: bare "继续"/continue with no media and no clear open work →
        // soft guidance only (do not call the model). Incomplete-turn /
        // pending_retry / trailing incomplete history still fall through.
        if media_parts.is_empty() && is_bare_continue(&content) {
            let has_open = was_incomplete_turn
                || self.pending_retry.lock().await.is_some()
                || history_looks_incomplete(&session.history);
            if !has_open {
                let msg = crate::agents::user_messages::MSG_BARE_CONTINUE.to_string();
                // Tear down stream/channel without running the agent.
                let turn_stream = session.turn_stream.take();
                session.channel = None;
                if let Some(s) = turn_stream {
                    s.abort().await;
                }
                if let Some(ch) = channel_for_send {
                    let receiver = {
                        let mut r =
                            crate::channels::MessageReceiver::new(reply_target.clone());
                        if let Some(ref last_msg) = session.last_message {
                            r.reply_to_message_id = Some(
                                last_msg
                                    .receiver
                                    .reply_to_message_id
                                    .clone()
                                    .unwrap_or_else(|| last_msg.id.clone()),
                            );
                            r.thread_id = last_msg.receiver.thread_id.clone();
                        }
                        r
                    };
                    let message = crate::channels::ChannelOutboundMessage {
                        receiver,
                        content: crate::channels::ChannelMessageContent::text(msg.clone()),
                        options: Default::default(),
                    };
                    let _ = ch.send_message(&message).await;
                }
                return Ok(TurnResult {
                    text: msg,
                    stop_reason: crate::providers::StopReason::EndTurn,
                    pending_retry: None,
                });
            }
        }

        // P3: pure-image (or media-only) turns get an explicit model-side hint
        // so the agent does not invent tasks from compaction noise.
        let content_for_model = if !media_parts.is_empty() && content.trim().is_empty() {
            crate::agents::user_messages::MSG_IMAGE_ONLY_HINT.to_string()
        } else {
            content
        };

        let user_content = match reminder {
            Some(rem) => format!("{}\n\n{}", rem, content_for_model),
            None => content_for_model,
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
                // ── Media aging ────────────────────────────────────────────
                // After the model has seen the media in this turn, replace
                // inline File parts in history with compact text markers so
                // subsequent turns don't re-upload large payloads (the root
                // cause of 300s API timeouts with big videos).
                //
                // Skip when the turn ended with ContextOverflow — the model
                // never successfully processed the request, so we want the
                // media to remain inline for the retry / compaction path.
                if turn_result.stop_reason != crate::providers::StopReason::ContextOverflow {
                    age_session_media(&mut session, persist_hook.as_deref());
                }

                if let Some(ref retry_msg) = turn_result.pending_retry {
                    *self.pending_retry.lock().await = Some(retry_msg.clone());
                }
                // RFC §7.6: consume the stream first; its `finish()` reports
                // how far the streaming path actually got. Fall back to
                // `send_message` whenever delivery did NOT reach
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
                            let receiver = {
                                let mut r =
                                    crate::channels::MessageReceiver::new(reply_target.clone());
                                if let Some(ref last_msg) = session.last_message {
                                    r.reply_to_message_id = Some(
                                        last_msg
                                            .receiver
                                            .reply_to_message_id
                                            .clone()
                                            .unwrap_or_else(|| last_msg.id.clone()),
                                    );
                                    r.thread_id = last_msg.receiver.thread_id.clone();
                                }
                                r
                            };
                            let message = crate::channels::ChannelOutboundMessage {
                                receiver,
                                content: crate::channels::ChannelMessageContent::text(
                                    turn_result.text.clone(),
                                ),
                                options: Default::default(),
                            };
                            if let Err(e) = ch.send_message(&message).await {
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
                let channel_name = channel_for_send
                    .as_ref()
                    .map(|ch| ch.name())
                    .unwrap_or("none");
                tracing::error!(
                    session = %session.id,
                    channel = %channel_name,
                    err = %e,
                    "Agent turn failed"
                );
                Err(e)
            }
        }
    }
}

/// Replace inline `File` parts in session history with text markers and
/// persist the aged versions to the backend.
///
/// Iterates all user messages; after the first successful aging pass on a
/// session, subsequent turns find only markers (idempotent). Each changed
/// message is persisted via `PersistHook::update_message` so the aging
/// survives session reloads.
fn age_session_media(session: &mut Session, hook: Option<&dyn crate::agents::PersistHook>) {
    use crate::providers::media::age_media_in_message;

    let session_id = session.id.clone();
    for i in 0..session.history.len() {
        // Only age user messages — assistant messages don't carry File parts.
        if session.history[i].role != "user" {
            continue;
        }
        if !age_media_in_message(&mut session.history[i]) {
            continue;
        }
        let msg_id = session.message_ids.get(i).copied().unwrap_or(0);
        if msg_id > 0 {
            if let Some(hook) = hook {
                let aged = &session.history[i];
                hook.update_message(&session_id, msg_id, aged);
            }
        }
    }
}

/// True when the user text is only a bare continue cue (no real question).
fn is_bare_continue(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    matches!(
        t,
        "继续"
            | "繼續"
            | "接着"
            | "接着做"
            | "接着来"
            | "接着说"
            | "继续吧"
            | "继续啊"
            | "继续。"
            | "继续！"
            | "continue"
            | "Continue"
            | "CONTINUE"
            | "go on"
            | "Go on"
            | "keep going"
            | "Keep going"
    )
}

/// Cheap local check: history ends with an open tool round or orphan user
/// (mirrors orchestrator incomplete-turn shape without importing it).
fn history_looks_incomplete(history: &[crate::providers::ChatMessage]) -> bool {
    let Some(last) = history.last() else {
        return false;
    };
    match last.role.as_str() {
        "user" => true,
        "assistant" => last
            .tool_calls
            .as_ref()
            .is_some_and(|t| !t.is_empty()),
        "tool" => true,
        _ => false,
    }
}

#[cfg(test)]
mod p3_helpers_tests {
    use super::*;
    use crate::providers::ChatMessage;

    #[test]
    fn bare_continue_detects_common_cues() {
        assert!(is_bare_continue("继续"));
        assert!(is_bare_continue("  继续  "));
        assert!(is_bare_continue("continue"));
        assert!(is_bare_continue("Continue"));
        assert!(!is_bare_continue("继续做那个 SEO 修复"));
        assert!(!is_bare_continue("请继续分析日志"));
        assert!(!is_bare_continue(""));
        assert!(!is_bare_continue("hello"));
    }

    #[test]
    fn history_incomplete_on_trailing_user_or_tool() {
        assert!(!history_looks_incomplete(&[]));
        assert!(history_looks_incomplete(&[ChatMessage::user_text("hi")]));
        assert!(!history_looks_incomplete(&[
            ChatMessage::user_text("hi"),
            ChatMessage::assistant_text("ok"),
        ]));
        let mut asst = ChatMessage::assistant_text("");
        asst.tool_calls = Some(vec![crate::providers::ToolCall {
            id: "1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }]);
        assert!(history_looks_incomplete(&[asst]));
    }
}
