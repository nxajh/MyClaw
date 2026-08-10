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

use crate::agents::session::{PersistHook, Session};
use crate::agents::turn::{SubResult, SubStatus, TurnResult, TurnSuspension};
use crate::agents::{Agent, AgentRuntime, TurnContext, UserProfile};
use crate::channels::{Channel, ChannelInboundMessage};

/// 方案 C (RFC §3.3, 2026-08-10): transform a silenced resume turn's
/// accumulated output into a *progress* message — prefixed so the user reads
/// it as interim status (not the final reply), truncated defensively since
/// the model may not fully honor the silence guidance. The turn itself stays
/// suspended; only the final loud summary ends it.
fn format_progress_message(text: &str) -> String {
    const PROGRESS_PREFIX: &str = "[进度] ";
    const MAX_CHARS: usize = 500;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        format!("{}{}", PROGRESS_PREFIX, trimmed)
    } else {
        let cut: String = trimmed.chars().take(MAX_CHARS).collect();
        format!("{} {}…（完整内容已记入会话历史）", PROGRESS_PREFIX, cut)
    }
}

/// 方案 C (RFC §3.3, race fix 2026-08-10): decide whether a turn is silenced.
///
/// Delegation notices carry a **wake-time intent** (`intent`), captured when
/// the terminal event was collected — a queued notice may start long after
/// later terminals cleared `pending`, so the live snapshot at turn start is
/// racy (the E2E 恢复轮1 bug: an intermediate notice streamed as a normal
/// message because `pending` was already empty when its turn ran). User
/// messages carry `None` → fall back to the live snapshot at turn start.
/// `pub(crate)` so orchestrator tests can pin the wake-time semantics.
pub(crate) fn decide_silenced(intent: Option<bool>, live: Option<TurnSuspension>) -> bool {
    intent.unwrap_or_else(|| live.is_some_and(|s| !s.pending.is_empty()))
}

/// Per-session bundle held by the SessionManager's session-context table.
///
/// Fields:
/// - `session`: owns the conversation state (Session — history, override, …)
/// - `agent`: Agent bound to this session (resolved from `Session.agent_name`
///   at construction time; reused across every turn so per-session
///   dispatch doesn't re-look-up the SubAgentConfig)
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
    /// Hex session id, copied at construction so the coordinator can locate
    /// this context by session id without locking `session` (which is held
    /// for the whole turn). Used by 方案 C pending registration.
    pub session_id: String,
    /// Agent bound to this session at creation time. Built from
    /// `Session.agent_name` via `SessionManager.build_agent_for_session`.
    pub agent: Arc<Agent>,
    /// User message saved when the previous turn ended with an empty LLM
    /// response or interrupted streaming. Cleared once retried.
    pub pending_retry: Arc<Mutex<Option<String>>>,
    /// Serializes `process_turn` per session. Distinct from `session`'s
    /// Mutex because some readers want to peek at session state without
    /// blocking on an in-flight turn.
    pub turn_lock: Arc<Mutex<()>>,
    /// 方案 C (docs/turn-suspension-rfc.md): non-None while the parent turn
    /// is suspended on async delegations. `std::sync::Mutex` (no await in
    /// the critical section): registration happens on the sync
    /// `spawn_delegate_async` path inside `Agent::run`'s tool execution,
    /// while `process_turn` holds `session`'s tokio Mutex.
    pub turn_suspension: std::sync::Mutex<Option<TurnSuspension>>,
    /// 方案 C (RFC §5): persist hook for `sessions/<sid>/suspension.json`,
    /// cloned from `Session.persist` at construction. Suspension writes
    /// (`add_pending_task` / `add_progress` / `record_terminal` /
    /// `clear_suspension_if_collected`) happen while `session`'s tokio
    /// Mutex is held, so they cannot reach `session.persist` — this copy
    /// keeps the durable state writable on those paths.
    pub suspension_persist: Option<Arc<dyn PersistHook>>,
    /// Loaded UserProfile snapshot taken at SessionContext creation.
    /// Immutable for the lifetime of the context — per RFC §三.A reload
    /// semantics drop the SessionContext and let `SessionManager`
    /// rematerialize it from a fresh profile read.
    pub user_profile: Arc<UserProfile>,
}

impl SessionContext {
    pub fn new(session: Session, agent: Arc<Agent>) -> Self {
        // Clone the persist hook BEFORE moving `session` into the Mutex —
        // suspension writes happen while that tokio Mutex is held and cannot
        // reach `session.persist` directly (RFC §5).
        let suspension_persist = session.persist.clone();
        let ctx = Self {
            session_id: session.id.clone(),
            session: Arc::new(Mutex::new(session)),
            agent,
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            turn_suspension: std::sync::Mutex::new(None),
            suspension_persist,
            user_profile: Arc::new(UserProfile::default()),
        };
        ctx.restore_suspension();
        ctx
    }

    /// Build with a pre-loaded user profile (the path
    /// `SessionManager::get_or_create_context_with` takes).
    pub fn with_profile(session: Session, agent: Arc<Agent>, profile: Arc<UserProfile>) -> Self {
        let suspension_persist = session.persist.clone();
        let ctx = Self {
            session_id: session.id.clone(),
            session: Arc::new(Mutex::new(session)),
            agent,
            pending_retry: Arc::new(Mutex::new(None)),
            turn_lock: Arc::new(Mutex::new(())),
            turn_suspension: std::sync::Mutex::new(None),
            suspension_persist,
            user_profile: profile,
        };
        ctx.restore_suspension();
        ctx
    }

    /// 方案 C (RFC §5): hydrate `turn_suspension` from
    /// `sessions/<sid>/suspension.json` at construction (daemon-restart
    /// recovery). Corrupt JSON is warned about and ignored — the session
    /// just starts loud like an unsuspended one.
    fn restore_suspension(&self) {
        let Some(hook) = &self.suspension_persist else {
            return;
        };
        let Some(json) = hook.load_suspension(&self.session_id) else {
            return;
        };
        if json.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<TurnSuspension>(&json) {
            Ok(s) => {
                let pending = s.pending.len();
                *self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                tracing::info!(
                    session = %self.session_id,
                    pending = pending,
                    "restored suspended turn from disk"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session = %self.session_id,
                    err = %e,
                    "suspension.json corrupt; ignoring"
                );
            }
        }
    }

    /// 方案 C (RFC §5): write the current `turn_suspension` to
    /// `sessions/<sid>/suspension.json`; `None` → empty string (file
    /// deleted). No-op without a persist hook. Called after every mutation
    /// point so a crash/restart never loses collected progress.
    fn persist_suspension(&self) {
        let Some(hook) = &self.suspension_persist else {
            return;
        };
        let json = match self
            .turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(s) => match serde_json::to_string(s) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(session = %self.session_id, err = %e, "serialize suspension failed");
                    return;
                }
            },
            None => String::new(),
        };
        hook.save_suspension(&self.session_id, &json);
    }

    /// Snapshot the session for read-only consumers (e.g., /status commands).
    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }

    /// Stash rendered per-turn injections (RFC §3.5/§4.3: user-level mailbox
    /// messages + pending friend requests). Called by `dispatch_turn` right
    /// before `process_turn`; `Agent::run` injects them into the first LLM
    /// request and clears the field.
    pub async fn stash_turn_injections(&self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        let mut session = self.session.lock().await;
        session.turn_injections.extend(texts);
    }

    /// 方案 C: register an async delegation against this session's suspension
    /// (called from the sync `spawn_delegate_async` path; std Mutex, no await).
    pub fn add_pending_task(&self, task_id: String) {
        {
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(s) => s.add_pending(task_id),
                None => *guard = Some(TurnSuspension::new(task_id)),
            }
        }
        self.persist_suspension();
    }

    /// 方案 C: true while the turn is suspended on uncollected delegations.
    pub fn has_pending_delegations(&self) -> bool {
        self.turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| !s.pending.is_empty())
            .unwrap_or(false)
    }

    /// 方案 C: snapshot of the suspension state (P0-2 consumes terminal
    /// events against it; P1-1 persists it).
    pub fn suspension_snapshot(&self) -> Option<TurnSuspension> {
        self.turn_suspension
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 方案 C: accumulate a suppressed progress report (RFC §2.3 — never
    /// injected into the parent context, suspended or not). No-op when the
    /// session is not suspended.
    pub fn add_progress(&self, task_id: &str, text: &str) {
        {
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = guard.as_mut() {
                s.progress_by_task
                    .entry(task_id.to_string())
                    .or_default()
                    .push(text.to_string());
            }
        }
        self.persist_suspension();
    }

    /// 方案 C: collect a terminal event into the suspension — move the task
    /// out of `pending`, fold its suppressed progress reports into the new
    /// `SubResult`, append to `results` in completion order. Returns the
    /// updated snapshot when the session is suspended (`None` otherwise);
    /// callers detect full collection via `snapshot.pending.is_empty()`.
    pub fn record_terminal(
        &self,
        task_id: String,
        status: SubStatus,
        content: String,
        sent_message_count: u64,
    ) -> Option<TurnSuspension> {
        let snapshot = {
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
            let s = guard.as_mut()?;
            s.pending.retain(|t| t != &task_id);
            let progress = s.progress_by_task.remove(&task_id).unwrap_or_default();
            s.results.push(SubResult {
                task_id,
                status,
                content,
                sent_message_count,
                progress,
            });
            guard.clone()
        };
        // Persist after the guard drops — persist_suspension re-locks the
        // same std Mutex (not reentrant).
        self.persist_suspension();
        snapshot
    }

    /// 方案 C (RFC §3.4): clear the suspension once every pending task has
    /// been collected (no-op when there is no suspension or tasks remain).
    /// Called after each turn's `has_pending` computation — a turn whose
    /// run re-delegated keeps its (repopulated) suspension; the final
    /// resume turn ends with `pending` empty and drops the state so the
    /// next turn is loud again.
    pub fn clear_suspension_if_collected(&self) {
        {
            let mut guard = self.turn_suspension.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = guard.as_ref() {
                if s.pending.is_empty() {
                    *guard = None;
                }
            }
        }
        self.persist_suspension();
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
        // 方案 C (§3.3, race fix): delegation notices carry the wake-time
        // silence intent; capture before `inbound_msg` is moved. User-message
        // turns leave it None → live snapshot decides below.
        let silenced_intent = inbound_msg.silenced_override;

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
        // 方案 C (RFC §3.3): silent iff the session is suspended with pending
        // tasks — for delegation notices the intent is captured at wake time
        // (`silenced_intent`, see `decide_silenced`; a queued notice may run
        // after later terminals cleared `pending`), for user messages from
        // the live snapshot at turn start. Origin turns (suspension created
        // mid-run) and the final resume turn (last terminal already recorded
        // → pending empty) are loud; intermediate resume turns are silent
        // context updates — their output becomes a `[进度]` message, the user
        // sees the final summary. Also disables `ask_user` for this turn
        // (session flag).
        let silenced = decide_silenced(silenced_intent, self.suspension_snapshot());
        session.turn_silenced = silenced;
        // RFC §7.6: install per-turn streaming handle BEFORE Agent::run.
        // Channels that don't support streaming return None; the
        // fallback send block below covers them. Silenced turns get no
        // stream at all — `push_or_drop` no-ops and `finish()` reports
        // Pending, so nothing reaches the user.
        session.turn_stream = if silenced {
            None
        } else {
            channel
                .as_ref()
                .and_then(|ch| ch.create_stream(&reply_target))
        };
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
            let skills_snap = runtime.skills.read();
            // Clone history to avoid borrow conflict with attachments.
            let history_clone = session.history.clone();
            session.attachments.diff_skills(&skills_snap, &history_clone);
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
            session.attachments.diff_agents(&agent_list, &history_clone);
            // Date injection respects the configured [prompt] timezone_offset
            // (sourced from the shared ResourceProvider via ContextEngine).
            session.attachments.diff_date(runtime.context_engine.timezone_offset(), &history_clone);
            session.attachments.diff_autonomy(&prompt_config.permission_mode, &history_clone);
            // Inject user/feedback memory index as system-reminder.
            let knowledge_dir = &runtime.defaults.prompt.knowledge_dir;
            let memory_entries: Vec<crate::memory::IndexEntry> = if !knowledge_dir.is_empty() {
                let memory_dir = std::path::Path::new(knowledge_dir);
                let files = crate::memory::scan_memory_files(memory_dir);
                files.iter().map(crate::memory::IndexEntry::from).collect()
            } else {
                Vec::new()
            };
            session.attachments.diff_memory(&memory_entries, &history_clone);
            let text = session.attachments.build_text(&skills_snap);
            session.attachments.clear_pending();

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
                    has_pending: false,
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
        // — suppressed on silenced resume turns (RFC §3.3).
        if !silenced {
            if let Some(ref ch) = channel_for_send {
                ch.on_status(&reply_target, crate::ProcessingStatus::Thinking)
                    .await;
            }
        }

        let result = self.agent.run(&mut session, turn_ctx, &runtime).await;
        // Per-turn channel + turn_stream are transient. Consume the
        // stream first (RFC §7.6): finish on success delivers FinalDelivered;
        // abort on error cancels the WS transport.
        let turn_stream = session.turn_stream.take();
        session.channel = None;

        match (result, turn_stream) {
            (Ok(mut turn_result), stream) => {
                // 方案 C: turn ended with async delegations still pending →
                // mark the result so the dispatcher knows to suspend (no
                // user-visible reply; terminal events resume the turn).
                turn_result.has_pending = self.has_pending_delegations();
                // 方案 C (RFC §3.4): pending 归零 → 最终轮,挂起状态消费完毕,
                // 清除之(下一轮恢复响亮)。级联轮(本 turn 再次派发)pending
                // 非空 → 保留,继续挂起。
                self.clear_suspension_if_collected();
                // Notify channel that processing completed successfully
                // — suppressed on silenced resume turns (RFC §3.3).
                if !silenced {
                    if let Some(ref ch) = channel_for_send {
                        ch.on_status(&reply_target, crate::ProcessingStatus::Done)
                            .await;
                    }
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
                if !silenced
                    && delivery != crate::channels::StreamDelivery::FinalDelivered
                {
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
                // 方案 C (RFC §3.3, 2026-08-10): a silenced resume turn's
                // output is NOT dropped — it is transformed and delivered as
                // a *progress* message (interim status, not a turn end).
                // turn_stream is None for silenced turns, so no final-reply
                // semantics ran above (`finish()` reported Pending and the
                // fallback block skipped it); the accumulated text reaches
                // the user here as feedback while the turn stays suspended.
                if silenced && !turn_result.text.trim().is_empty() {
                    if let Some(ref ch) = channel_for_send {
                        let receiver = {
                            let mut r = crate::channels::MessageReceiver::new(reply_target.clone());
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
                                format_progress_message(&turn_result.text),
                            ),
                            options: Default::default(),
                        };
                        if let Err(e) = ch.send_message(&message).await {
                            tracing::warn!(
                                session = %session.id,
                                err = %e,
                                "process_turn: silenced progress send failed"
                            );
                        }
                    }
                }

                // ── Auto TTS ──────────────────────────────────────────────
                // If auto_tts is enabled and a TTS provider is available,
                // synthesize the reply text to audio and send as a voice message.
                if runtime.defaults.auto_tts
                    && !silenced
                    && !turn_result.text.trim().is_empty()
                {
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

/// P1-4: turn-suspension (方案 C, RFC §3/§5) behavior tests. All state is
/// exercised through the public `SessionContext` API; persistence round-trips
/// go through the manager path so the in-memory backend + `BackendPersistHook`
/// are the same wiring production uses.
#[cfg(test)]
mod suspension_tests {
    use super::*;
    use crate::agents::SessionManager;

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
        assert!(ctx.has_pending_delegations());
        assert_eq!(ctx.suspension_snapshot().unwrap().pending, vec!["t1"]);
        ctx.record_terminal("t1".into(), SubStatus::Completed, "ok".into(), 0);
        assert!(!ctx.has_pending_delegations());
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
        let snap = ctx
            .record_terminal("t1".into(), SubStatus::Completed, "summary text".into(), 0)
            .unwrap();
        assert_eq!(snap.results.len(), 1);
        let r = &snap.results[0];
        assert_eq!(r.task_id, "t1");
        assert_eq!(r.status, SubStatus::Completed);
        assert_eq!(r.content, "summary text");
        assert_eq!(r.sent_message_count, 0);
        assert_eq!(r.progress, vec!["working on it", "still going"]);
        assert!(snap.pending.is_empty());
        assert!(snap.progress_by_task.is_empty());
    }

    #[test]
    fn out_of_order_completion_collects_in_completion_order() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string());
        ctx.add_pending_task("t3".to_string());
        ctx.record_terminal("t3".into(), SubStatus::Failed, "e3".into(), 0);
        ctx.record_terminal("t1".into(), SubStatus::Completed, "c1".into(), 0);
        ctx.record_terminal("t2".into(), SubStatus::TimedOut, "t2".into(), 0);
        let snap = ctx.suspension_snapshot().unwrap();
        let order: Vec<&str> = snap.results.iter().map(|r| r.task_id.as_str()).collect();
        assert_eq!(order, vec!["t3", "t1", "t2"]);
        assert_eq!(snap.results[1].status, SubStatus::Completed);
        assert_eq!(snap.results[2].status, SubStatus::TimedOut);
        assert!(snap.pending.is_empty());
    }

    #[test]
    fn record_terminal_without_suspension_returns_none() {
        let (ctx, _m) = make_ctx();
        assert!(ctx.suspension_snapshot().is_none());
        assert!(!ctx.has_pending_delegations());
        let snap = ctx.record_terminal("t9".into(), SubStatus::Completed, "x".into(), 0);
        assert!(snap.is_none());
        assert!(ctx.suspension_snapshot().is_none());
    }

    #[test]
    fn clear_semantics_pending_kept_empty_removed_idempotent() {
        let (ctx, _m) = make_ctx();
        ctx.add_pending_task("t1".to_string());
        ctx.add_pending_task("t2".to_string());
        // pending non-empty → suspension retained
        ctx.record_terminal("t1".into(), SubStatus::Completed, "c1".into(), 0);
        ctx.clear_suspension_if_collected();
        let snap = ctx.suspension_snapshot().unwrap();
        assert_eq!(snap.pending, vec!["t2"]);
        // pending empty → suspension cleared
        ctx.record_terminal("t2".into(), SubStatus::Completed, "c2".into(), 0);
        ctx.clear_suspension_if_collected();
        assert!(ctx.suspension_snapshot().is_none());
        // idempotent
        ctx.clear_suspension_if_collected();
        assert!(ctx.suspension_snapshot().is_none());
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
                );
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let snap = ctx.suspension_snapshot().unwrap();
        assert_eq!(snap.results.len(), 8);
        assert!(snap.pending.is_empty());
        let mut by_task: Vec<&SubResult> = snap.results.iter().collect();
        by_task.sort_by_key(|r| r.task_id.clone());
        for (i, r) in by_task.iter().enumerate() {
            assert_eq!(r.task_id, format!("t{}", i));
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
        ctx.record_terminal("t1".into(), SubStatus::Completed, "final summary".into(), 2);
        let sid = ctx.session_id.clone();
        assert_eq!(ctx.suspension_snapshot().unwrap().results.len(), 1);

        // Drop the context — a fresh one must restore from the backend.
        manager.drop_context("mock:default:u1");
        let ctx2 = manager.get_or_create_context("mock:default:u1");
        assert_eq!(ctx2.session_id, sid);
        let snap2 = ctx2.suspension_snapshot().unwrap();
        assert_eq!(snap2.results.len(), 1);
        let r = &snap2.results[0];
        assert_eq!(r.task_id, "t1");
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

    #[test]
    fn progress_message_prefixed_and_passthrough() {
        let msg = format_progress_message("已接收子代理 t1 的结果，继续等待 t2。");
        assert_eq!(msg, "[进度] 已接收子代理 t1 的结果，继续等待 t2。");
    }

    #[test]
    fn progress_message_truncates_long_output() {
        let long = "字".repeat(600);
        let msg = format_progress_message(&long);
        assert!(msg.starts_with("[进度] "));
        assert!(msg.contains("完整内容已记入会话历史"));
        assert!(msg.chars().count() < 600);
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
        let _ = ctx.record_terminal("t1".into(), SubStatus::Completed, "t1 done".into(), 0);
        let live_with_t2 = ctx.suspension_snapshot();
        assert!(decide_silenced(Some(true), live_with_t2.clone()));

        // Race: t2's terminal lands BEFORE wake-1's turn starts — the live
        // snapshot is now empty (would mark the turn loud), but the
        // wake-time intent keeps the intermediate notice silenced.
        let _ = ctx.record_terminal("t2".into(), SubStatus::Completed, "t2 done".into(), 0);
        let live_empty = ctx.suspension_snapshot();
        assert!(live_empty.as_ref().unwrap().pending.is_empty());
        assert!(decide_silenced(Some(true), live_empty.clone()));

        // wake-2 (t2 terminal, final notice): intent false → loud summary
        // regardless of the live snapshot.
        assert!(!decide_silenced(Some(false), live_empty.clone()));
        assert!(!decide_silenced(Some(false), live_with_t2));
    }

    #[test]
    fn decide_silenced_defaults_to_live_snapshot_for_user_messages() {
        let (ctx, _m) = make_ctx();
        // Not suspended → loud.
        assert!(!decide_silenced(None, ctx.suspension_snapshot()));
        // Suspended with pending → silenced (user-message resume turn).
        ctx.add_pending_task("t1".to_string());
        assert!(decide_silenced(None, ctx.suspension_snapshot()));
        // Suspended but fully collected → loud (final resume turn).
        ctx.record_terminal("t1".into(), SubStatus::Completed, "ok".into(), 0);
        assert!(!decide_silenced(None, ctx.suspension_snapshot()));
    }
}
