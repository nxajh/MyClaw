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

        let content = crate::str_utils::neutralize_spoofing(&inbound_msg.content.text);
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

        // Notify channel that processing has started (typing indicator, etc.)
        if let Some(ref ch) = channel_for_send {
            ch.on_status(&reply_target, crate::ProcessingStatus::Thinking)
                .await;
        }

        let result = self.agent.run(&mut session, turn_ctx, &runtime).await;
        // Per-turn channel + turn_stream are transient. Consume the
        // stream first (RFC §7.6): finish on success delivers FinalDelivered;
        // abort on error cancels the WS transport.
        let turn_stream = session.turn_stream.take();
        session.channel = None;

        match (result, turn_stream) {
            (Ok(turn_result), stream) => {
                // Notify channel that processing completed successfully
                if let Some(ref ch) = channel_for_send {
                    ch.on_status(&reply_target, crate::ProcessingStatus::Done)
                        .await;
                }
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
                    if let Some(ref ch) = channel_for_send {
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

                // ── Auto TTS ──────────────────────────────────────────────
                // If auto_tts is enabled and a TTS provider is available,
                // synthesize the reply text to audio and send as a voice message.
                if runtime.defaults.auto_tts && !turn_result.text.trim().is_empty() {
                    if let Some(ref ch) = channel_for_send {
                        if let Ok((tts_provider, tts_model)) =
                            runtime.providers.get_tts_provider()
                        {
                            let text_for_tts = prepare_text_for_tts(turn_result.text.trim());
                            // Skip very long texts or texts that became empty after stripping.
                            if !text_for_tts.is_empty() && text_for_tts.chars().count() <= 2000 {
                                let req = crate::providers::TtsRequest {
                                    model: tts_model,
                                    input: text_for_tts.to_string(),
                                    voice: crate::providers::TtsVoice::Id(String::new()),
                                    response_format: None,
                                    speed: None,
                                };
                                match tts_provider.synthesize(req) {
                                    Ok(audio_resp) => {
                                        let temp_path = std::env::temp_dir().join(format!(
                                            "myclaw-tts-{}.mp3",
                                            uuid::Uuid::new_v4()
                                        ));
                                        if std::fs::write(&temp_path, &audio_resp.audio.bytes)
                                            .is_ok()
                                        {
                                            let receiver = {
                                                let mut r = crate::channels::MessageReceiver::new(
                                                    reply_target.clone(),
                                                );
                                                if let Some(ref last_msg) = session.last_message {
                                                    r.reply_to_message_id = Some(
                                                        last_msg
                                                            .receiver
                                                            .reply_to_message_id
                                                            .clone()
                                                            .unwrap_or_else(|| last_msg.id.clone()),
                                                    );
                                                    r.thread_id =
                                                        last_msg.receiver.thread_id.clone();
                                                }
                                                r
                                            };
                                            let voice_file = crate::channels::ChannelFile {
                                                meta: crate::channels::ChannelFileMeta {
                                                    file_name: format!(
                                                        "voice-{}.mp3",
                                                        uuid::Uuid::new_v4()
                                                    ),
                                                    mime_type: Some(
                                                        audio_resp.audio.mime_type.clone(),
                                                    ),
                                                    size_bytes: Some(
                                                        audio_resp.audio.bytes.len() as u64,
                                                    ),
                                                    source_url: None,
                                                },
                                                body: std::sync::Arc::new(
                                                    crate::channels::LocalFileBody::new(
                                                        temp_path.to_string_lossy().to_string(),
                                                    ),
                                                ),
                                            };
                                            let message =
                                                crate::channels::ChannelOutboundMessage {
                                                    receiver,
                                                    content: crate::channels::ChannelMessageContent {
                                                        text: String::new(),
                                                        files: vec![voice_file],
                                                        buttons: vec![],
                                                    },
                                                    options: Default::default(),
                                                };
                                            if let Err(e) = ch.send_message(&message).await {
                                                tracing::warn!(
                                                    session = %session.id,
                                                    err = %e,
                                                    "auto_tts: voice send failed"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            session = %session.id,
                                            err = %e,
                                            "auto_tts: synthesis failed"
                                        );
                                    }
                                }
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
                // Notify channel of error + send error notice to user
                if let Some(ref ch) = channel_for_send {
                    ch.on_status(&reply_target, crate::ProcessingStatus::Error)
                        .await;
                    // Best-effort error notice
                    let err_msg = crate::agents::user_messages::MSG_TURN_FAILED.to_string();
                    let receiver =
                        crate::channels::MessageReceiver::new(reply_target.clone());
                    let message = crate::channels::ChannelOutboundMessage {
                        receiver,
                        content: crate::channels::ChannelMessageContent::text(err_msg),
                        options: Default::default(),
                    };
                    let _ = ch.send_message(&message).await;
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

/// Prepare text for speech synthesis.
///
/// Multi-stage cleanup so the TTS engine receives a clean spoken script:
/// 1. Strip `<think>` reasoning blocks (models emit these; users want to
///    see reasoning, not hear it).
/// 2. Strip markdown formatting (headings, emphasis, code, links, lists,
///    tables, blockquotes, horizontal rules).
/// 3. Normalize symbols to spoken words (°C → "度", % → "百分之",
///    $ → "美元", → → "到", emoji removal).
fn prepare_text_for_tts(input: &str) -> String {
    use regex::Regex;

    // ── Stage 1: Remove <think> blocks ──
    let re_think = Regex::new(r"(?s)<think[\s>].*?</think>").unwrap();
    let re_think_open = Regex::new(r"(?s)<think[\s>].*\z").unwrap();
    let mut text = re_think.replace_all(input, " ").to_string();
    text = re_think_open.replace_all(&text, " ").to_string();

    // ── Stage 2: Strip markdown ──
    let mut out = String::with_capacity(text.len());
    let re_heading = Regex::new(r"^#{1,6}\s*").unwrap();
    let re_quote = Regex::new(r"^\s*>\s?").unwrap();
    let re_bullet = Regex::new(r"^\s*[-*+•]\s+").unwrap();
    let re_number = Regex::new(r"^\s*\d+\.\s+").unwrap();
    let re_hr_dash = Regex::new(r"^\s*-{3,}\s*$").unwrap();
    let re_hr_star = Regex::new(r"^\s*\*{3,}\s*$").unwrap();
    let re_hr_under = Regex::new(r"^\s*_{3,}\s*$").unwrap();
    let re_link = Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap();
    let re_image = Regex::new(r"!\[[^\]]*\]\([^)]*\)").unwrap();
    let re_bold = Regex::new(r"\*\*|__").unwrap();

    for line in text.lines() {
        let mut l = line.to_string();

        if l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~") {
            continue;
        }
        l = re_heading.replace_all(&l, "").to_string();
        l = re_quote.replace_all(&l, "").to_string();
        l = re_bullet.replace_all(&l, "").to_string();
        l = re_number.replace_all(&l, "").to_string();
        if re_hr_dash.is_match(&l) || re_hr_star.is_match(&l) || re_hr_under.is_match(&l) {
            continue;
        }
        l = l.replace('|', " ");
        l = re_link.replace_all(&l, "$1").to_string();
        l = re_image.replace_all(&l, "").to_string();
        l = l.replace('`', "");
        l = re_bold.replace_all(&l, "").to_string();
        l = l.replace(['*', '_'], "");

        out.push_str(&l);
        out.push('\n');
    }
    let re_newlines = Regex::new(r"\n{3,}").unwrap();
    let result = re_newlines.replace_all(&out, "\n\n");
    let mut result = result.trim().to_string();

    // ── Stage 3: Normalize symbols for speech ──
    result = normalize_symbols_for_tts(&result);

    result
}

/// Expand common symbols and shorthand into words a TTS engine reads well.
/// Focused on Chinese-language usage patterns.
fn normalize_symbols_for_tts(text: &str) -> String {
    use regex::Regex;
    let mut t = text.to_string();

    // Temperature: 25°C → "25度", 25°C → "25摄氏度" (keep it short for Chinese)
    let re_temp_c = Regex::new(r"(?i)([-+]?\d+(?:\.\d+)?)\s*°\s*C\b").unwrap();
    t = re_temp_c.replace_all(&t, "${1}摄氏度").to_string();
    let re_temp_f = Regex::new(r"(?i)([-+]?\d+(?:\.\d+)?)\s*°\s*F\b").unwrap();
    t = re_temp_f.replace_all(&t, "${1}华氏度").to_string();
    // Bare degree sign → 度
    let re_degree = Regex::new(r"([-+]?\d+(?:\.\d+)?)\s*°").unwrap();
    t = re_degree.replace_all(&t, "${1}度").to_string();
    t = t.replace('°', "度");

    // Percentage: 50% → "百分之50"
    let re_percent = Regex::new(r"(\d+(?:\.\d+)?)\s*%").unwrap();
    t = re_percent.replace_all(&t, "百分之${1}").to_string();
    t = t.replace('%', "百分之");

    // Currency: $50 → "50美元", ¥50 → "50元", €50 → "50欧元", £50 → "50英镑"
    let re_usd = Regex::new(r"\$\s*(\d+(?:[,.]\d+)*)").unwrap();
    t = re_usd.replace_all(&t, "${1}美元").to_string();
    t = t.replace('¥', "元");
    t = t.replace('€', "欧元");
    t = t.replace('£', "英镑");

    // Arrows and operators
    t = t.replace('→', " 到 ");
    t = t.replace('⇒', " 到 ");
    t = t.replace('≈', " 约 ");
    t = t.replace("~", " 约 ");

    // Common separators
    t = t.replace('&', " 和 ");
    t = t.replace("•", " ");

    // Emojis — broad Unicode pictograph ranges. Most TTS engines read them
    // as awkward labels ("grinning face with smile") or skip them entirely.
    let re_emoji = Regex::new(concat!(
        "[",
        "\u{1F600}-\u{1F64F}", // emoticons
        "\u{1F300}-\u{1F5FF}", // symbols & pictographs
        "\u{1F680}-\u{1F6FF}", // transport & map
        "\u{1F700}-\u{1F77F}",
        "\u{1F780}-\u{1F7FF}",
        "\u{1F800}-\u{1F8FF}",
        "\u{1F900}-\u{1F9FF}", // supplemental symbols
        "\u{1FA00}-\u{1FAFF}",
        "\u{2600}-\u{26FF}",   // misc symbols (☀ ☂ ☎ etc.)
        "\u{2700}-\u{27BF}",   // dingbats (✂ ✅ ✨ etc.)
        "\u{1F1E6}-\u{1F1FF}", // regional indicators (flags)
        "]+",
    )).unwrap();
    t = re_emoji.replace_all(&t, " ").to_string();

    // Variation selectors (invisible formatting chars after emoji)
    let re_vs = Regex::new("[\u{FE0F}\u{FE0E}]").unwrap();
    t = re_vs.replace_all(&t, "").to_string();

    // Collapse whitespace left by removals
    let re_multi_space = Regex::new(r"[ \t]{2,}").unwrap();
    t = re_multi_space.replace_all(&t, " ").to_string();
    let re_space_punct = Regex::new(r"\s+([，。！？；：、,.!?;:])").unwrap();
    t = re_space_punct.replace_all(&t, "${1}").to_string();

    t.trim().to_string()
}

#[cfg(test)]
mod test_strip_markdown {
    use super::*;

    #[test]
    fn test_basic_stripping() {
        assert_eq!(prepare_text_for_tts("**hello**"), "hello");
        assert_eq!(prepare_text_for_tts("# 标题"), "标题");
        assert_eq!(prepare_text_for_tts("- 列表项"), "列表项");
        assert_eq!(prepare_text_for_tts("`代码`"), "代码");
        assert_eq!(prepare_text_for_tts("[链接文字](https://example.com)"), "链接文字");
    }

    #[test]
    fn test_complex() {
        let input = "## 标题\n\n**重点**和*斜体*文字。\n\n- 第一项\n- 第二项\n\n```\ncode block\n```\n";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("```"));
        assert!(!result.contains("**"));
        assert!(!result.contains("##"));
        assert!(result.contains("重点"));
        assert!(result.contains("第一项"));
    }

    #[test]
    fn test_think_block_removal() {
        let input = "<think>let me analyze this</think>你好世界";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("think"));
        assert!(!result.contains("analyze"));
        assert!(result.contains("你好世界"));
    }

    #[test]
    fn test_think_block_multiline() {
        let input = "<think>\nstep 1: do something\nstep 2: more analysis\n</think>\nThe answer is 42.";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("step 1"));
        assert!(result.contains("The answer is 42"));
    }

    #[test]
    fn test_symbol_expansion() {
        assert!(prepare_text_for_tts("温度25°C").contains("摄氏度"));
        assert!(prepare_text_for_tts("优惠50%").contains("百分之50"));
        assert!(prepare_text_for_tts("$100").contains("美元"));
        assert!(prepare_text_for_tts("¥50").contains("元"));
        assert!(prepare_text_for_tts("A → B").contains("到"));
    }

    #[test]
    fn test_emoji_removal() {
        let result = prepare_text_for_tts("你好😀世界");
        assert!(!result.contains("😀"));
        assert!(result.contains("你好"));
        assert!(result.contains("世界"));
    }

    #[test]
    fn test_combined() {
        let input = "## 📊 报告\n\n<think>分析数据中</think>\n\n**关键指标**：25°C，增长50%\n\n- 收入：$1000\n- 趋势：↑📈\n";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("📊"));
        assert!(!result.contains("📈"));
        assert!(!result.contains("think"));
        assert!(!result.contains("**"));
        assert!(!result.contains("##"));
        assert!(result.contains("报告"));
        assert!(result.contains("摄氏度"));
        assert!(result.contains("百分之50"));
        assert!(result.contains("美元"));
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
