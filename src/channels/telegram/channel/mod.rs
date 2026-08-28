//! TelegramChannel — the main bot adapter for the Telegram Bot API.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::channels::shared::{InboundDebouncer, TypingKeepAlive};
use crate::DedupState;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use crate::channels::message::ChannelInboundMessage;
use crate::channels::{Channel, FoldCandidate, TurnStream};
use crate::ProcessingStatus;
use super::super::turn_stream::TelegramTurnStream;

mod api;
mod session;

#[cfg(test)]
pub(crate) mod tests;

// ── TelegramChannel ────────────────────────────────────────────────────────────

/// Reaction tracker: reply_target → Vec<(chat_id, message_id)>.
type ReactionTracker = Arc<Mutex<std::collections::HashMap<String, Vec<(i64, i64)>>>>;

#[derive(Clone)]
pub struct TelegramChannel {
    bot_token: String,
    /// Normalized DM whitelist. Plain `Vec<String>` (not Arc<RwLock>)
    /// because MyClaw applies config changes via `myclaw reload` → SIGUSR1
    /// → hot_switch full-process restart, not in-process mutation. New
    /// process = fresh struct = no writer ever exists in this process.
    allowed_users: Vec<String>,
    /// Phase 4: allowed group chat IDs (RFC §14.5). `None` = reject all
    /// groups (Phase 4 default); `Some(vec ["*"])` = allow all groups.
    allowed_groups: Option<Vec<String>>,
    mention_only: bool,
    api_base: String,
    dedup: DedupState,
    /// Username of this bot (fetched lazily). Wrapped in Arc for Clone.
    bot_username: Arc<Mutex<Option<String>>>,
    /// Workspace directory for saving attachments.
    workspace_dir: Option<std::path::PathBuf>,
    /// Active typing keep-alive tasks, keyed by recipient (chat_id).
    typing: TypingKeepAlive,
    /// Whether to send acknowledgement reactions on received messages.
    ack_reactions: bool,
    /// Track ack reactions: reply_target → (chat_id, message_id) for removal after reply.
    pending_acks: ReactionTracker,
    /// Status reactions: reply_target → Vec<(chat_id, msg_id)>.
    status_reactions: ReactionTracker,
    /// Debounce window in milliseconds (0 = disabled) + merge buffer
    /// ("sender|reply_target" key).
    debouncer: InboundDebouncer,
    /// Stall watchdog timeout in seconds (0 = disabled).
    stall_timeout_secs: u64,
    /// Track when typing started for each recipient: reply_target → Instant.
    typing_started_at: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    /// Stall watchdog messages to delete when real reply arrives: reply_target → [(chat_id, msg_id)].
    stall_messages: ReactionTracker,
    /// Streaming preview mode for this channel.
    streaming_mode: crate::config::channel::StreamingMode,
    /// Per-account auto-TTS switch (default off).
    tts: bool,
    /// Targets with active streams; stall watchdog skips these to avoid
    /// redundant "still thinking" messages alongside the live preview.
    pub(crate) streaming_targets: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Directory for persisting state (e.g. Telegram update offset).
    base_dir: std::path::PathBuf,
    /// Shared HTTP client with connection pool.
    http: reqwest::Client,
    /// Lightweight ring buffer of recent sent messages (max 100 entries) for
    /// debugging and potential reply-chain context.
    message_cache: Arc<Mutex<VecDeque<(i64, String)>>>,
}

static TELEGRAM_CAPS: crate::channels::message::ChannelCapabilities =
    crate::channels::message::ChannelCapabilities::telegram();

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities {
        &TELEGRAM_CAPS
    }

    fn tts_enabled(&self) -> bool {
        self.tts
    }

    fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        use crate::channels::{AllowList, ChannelSecurityPolicy, GroupAuthMode};
        let allowed_users = if self.allowed_users.iter().any(|s| s == "*") {
            AllowList::All
        } else {
            AllowList::Whitelist(self.allowed_users.clone())
        };
        let group_allowlist = AllowList::from_config(self.allowed_groups.clone());
        let group_mode = match (&self.allowed_groups, self.mention_only) {
            (None, _) => GroupAuthMode::Reject,
            (Some(_), true) => GroupAuthMode::MentionOnly,
            (Some(_), false) => GroupAuthMode::Open,
        };
        ChannelSecurityPolicy {
            allowed_users,
            group_mode,
            group_allowlist,
        }
    }

    async fn send_message(
        &self,
        msg: &crate::channels::ChannelOutboundMessage,
    ) -> anyhow::Result<crate::channels::OutboundSendResult> {
        let (chat_id, thread_id) = Self::parse_reply_target(&msg.receiver.id);

        // Delete any stall watchdog messages before sending the real reply.
        let stall_msgs = self.stall_messages.lock().remove(&msg.receiver.id);
        if let Some(msgs) = stall_msgs {
            for (chat_id, msg_id) in msgs {
                if let Err(e) = self.delete_message_raw(chat_id, msg_id).await {
                    debug!("Failed to delete stall message {}: {e}", msg_id);
                }
            }
        }

        // Split into chunks that fit Telegram's rich message limit (32 768 chars).
        // Each chunk is sent via sendRichMessage with Markdown formatting.
        //
        // When files are present, text is used as caption on the first file,
        // not as a separate text message (RFC §14.5).
        let chunks = if msg.content.files.is_empty() {
            Self::chunk_for_telegram(&msg.content.text)
        } else {
            Vec::new()
        };

        let count = chunks.len();
        let mut last_error = None;
        let mut ids = Vec::new();

        // Build reply_markup from inline buttons (attached to last chunk only).
        let reply_markup: Option<serde_json::Value> = if msg.content.buttons.is_empty() {
            None
        } else {
            let keyboard: Vec<Vec<serde_json::Value>> = vec![
                msg.content
                    .buttons
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "text": b.label,
                            "callback_data": b.callback_data,
                        })
                    })
                    .collect(),
            ];
            Some(serde_json::json!({ "inline_keyboard": keyboard }))
        };

        for (i, chunk) in chunks.into_iter().enumerate() {
            let text = if count > 1 && i < count - 1 {
                format!("{}\n\n(continues...)", chunk)
            } else {
                chunk
            };
            // Attach buttons only to the last chunk.
            let markup = if i == count - 1 {
                reply_markup.clone()
            } else {
                None
            };
            match self
                .send_text(&chat_id, &text, thread_id.as_deref(), markup)
                .await
            {
                Ok(Some(id)) => {
                    self.cache_message(id, text.clone());
                    ids.push(crate::channels::MessageId::new(id.to_string()));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("Failed to send chunk {}/{}: {}", i + 1, count, e);
                    last_error = Some(e);
                    // Continue trying subsequent chunks
                }
            }
            // Throttle between chunks to avoid 429 rate limiting
            if i + 1 < count {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }

        // Stop typing indicator for this recipient now that the response is sent.
        self.stop_internal_typing(&msg.receiver.id);

        // Remove ack reactions (👀) for all tracked messages.
        let ack_info = self.pending_acks.lock().remove(&msg.receiver.id);
        if let Some(msg_ids) = ack_info {
            for (chat_id, msg_id) in msg_ids {
                self.remove_ack(chat_id, msg_id).await;
            }
        }

        // Clean up status reactions (🤔) for all tracked messages.
        let status_info = self.status_reactions.lock().remove(&msg.receiver.id);
        if let Some(msg_ids) = status_info {
            for (chat_id, msg_id) in msg_ids {
                let _ = self.remove_reaction(chat_id, msg_id, "🤔").await;
            }
        }

        if let Some(e) = last_error {
            return Err(e);
        }

        for (idx, file) in msg.content.files.iter().enumerate() {
            let caption = if idx == 0 && !msg.content.text.trim().is_empty() {
                Some(msg.content.text.as_str())
            } else {
                None
            };
            use tokio_util::io::ReaderStream;

            let modality = crate::providers::media::modality_from_mime(
                file.meta.mime_type.as_deref(),
                &file.meta.file_name,
            );

            // For audio files, convert to Ogg/Opus and send as a voice message
            // (Telegram voice bubble) instead of an audio player attachment.
            // Falls back to sendAudio if ffmpeg is unavailable or conversion fails.
            if modality == crate::providers::media::FileModality::Audio {
                use tokio::io::AsyncReadExt;
                let reader = file.body.open().await?;
                let mut reader = reader;
                let mut audio_bytes = Vec::new();
                reader.read_to_end(&mut audio_bytes).await?;

                match self.convert_to_opus_ogg(&audio_bytes).await {
                    Ok(ogg_bytes) => {
                        let part = reqwest::multipart::Part::stream(ogg_bytes)
                            .file_name("voice.ogg");
                        let mut form = reqwest::multipart::Form::new()
                            .text("chat_id", chat_id.clone())
                            .part("voice", part);
                        if let Some(thread_id) = thread_id.clone() {
                            form = form.text("message_thread_id", thread_id);
                        }
                        let resp = self
                            .http
                            .post(self.api_url("sendVoice"))
                            .multipart(form)
                            .send()
                            .await
                            .map_err(|e| anyhow::anyhow!("Telegram sendVoice failed: {e}"))?;
                        if !resp.status().is_success() {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            anyhow::bail!("Telegram sendVoice returned {status}: {text}");
                        }
                        let resp_json: serde_json::Value = resp.json().await?;
                        if let Some(id) = resp_json
                            .get("result")
                            .and_then(|r| r.get("message_id"))
                            .and_then(|m| m.as_i64())
                        {
                            ids.push(crate::channels::MessageId::new(id.to_string()));
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Telegram: Opus conversion failed ({e}), falling back to sendAudio"
                        );
                        // Fall through to the generic path below with the original bytes
                        let part = reqwest::multipart::Part::stream(audio_bytes)
                            .file_name(file.meta.file_name.clone());
                        let mut form = reqwest::multipart::Form::new()
                            .text("chat_id", chat_id.clone())
                            .part("audio", part);
                        if let Some(thread_id) = thread_id.clone() {
                            form = form.text("message_thread_id", thread_id);
                        }
                        if let Some(caption) = caption.filter(|c| !c.is_empty()) {
                            form = form.text("caption", caption.to_string());
                        }
                        let resp = self
                            .http
                            .post(self.api_url("sendAudio"))
                            .multipart(form)
                            .send()
                            .await
                            .map_err(|e| anyhow::anyhow!("Telegram sendAudio failed: {e}"))?;
                        if !resp.status().is_success() {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            anyhow::bail!("Telegram sendAudio returned {status}: {text}");
                        }
                        let resp_json: serde_json::Value = resp.json().await?;
                        if let Some(id) = resp_json
                            .get("result")
                            .and_then(|r| r.get("message_id"))
                            .and_then(|m| m.as_i64())
                        {
                            ids.push(crate::channels::MessageId::new(id.to_string()));
                        }
                        continue;
                    }
                }
            }

            let (method, part_name) = match modality {
                crate::providers::media::FileModality::Image => ("sendPhoto", "photo"),
                crate::providers::media::FileModality::Audio => ("sendAudio", "audio"),
                crate::providers::media::FileModality::Video => ("sendVideo", "video"),
                crate::providers::media::FileModality::Other => ("sendDocument", "document"),
            };

            let reader = file.body.open().await?;
            let stream = ReaderStream::new(reader);
            let body = reqwest::Body::wrap_stream(stream);
            let part =
                reqwest::multipart::Part::stream(body).file_name(file.meta.file_name.clone());
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.clone())
                .part(part_name.to_string(), part);
            if let Some(thread_id) = thread_id.clone() {
                form = form.text("message_thread_id", thread_id);
            }
            if let Some(caption) = caption.filter(|c| !c.is_empty()) {
                form = form.text("caption", caption.to_string());
            }

            let resp = self
                .http
                .post(self.api_url(method))
                .multipart(form)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Telegram {method} failed: {e}"))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram {method} returned {status}: {text}");
            }

            let resp_json: serde_json::Value = resp.json().await?;
            if let Some(id) = resp_json
                .get("result")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_i64())
            {
                ids.push(crate::channels::MessageId::new(id.to_string()));
            }
        }

        Ok(crate::channels::OutboundSendResult { message_ids: ids })
    }

    async fn edit_message(
        &self,
        receiver: &crate::channels::MessageReceiver,
        message_id: &crate::channels::MessageId,
        content: crate::channels::ChannelMessageContent,
    ) -> anyhow::Result<()> {
        let (chat_id_str, _) = Self::parse_reply_target(&receiver.id);
        let chat_id: i64 = chat_id_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id: {chat_id_str}"))?;
        let mid: i64 = message_id
            .as_str()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid message_id: {}", message_id.as_str()))?;
        self.edit_message_text_raw(chat_id, mid, &content.text)
            .await?;
        Ok(())
    }

    async fn delete_message(
        &self,
        receiver: &crate::channels::MessageReceiver,
        message_id: &crate::channels::MessageId,
    ) -> anyhow::Result<()> {
        let (chat_id_str, _) = Self::parse_reply_target(&receiver.id);
        let chat_id: i64 = chat_id_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id: {chat_id_str}"))?;
        let mid: i64 = message_id
            .as_str()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid message_id: {}", message_id.as_str()))?;
        self.delete_message_raw(chat_id, mid).await?;
        Ok(())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>> {
        // Lazily fetch bot username for mention detection.
        if let Some(username) = self.fetch_bot_username().await {
            info!("Telegram bot username: @{}", username);
            self.set_bot_username(username);
        }

        let (tx, rx) = mpsc::channel::<ChannelInboundMessage>(100);
        let ch = self.clone();

        tokio::spawn(async move {
            ch.poll_loop(tx).await;
        });

        // Spawn stall watchdog.
        let watchdog_ch = self.clone();
        tokio::spawn(async move {
            watchdog_ch.stall_watchdog().await;
        });

        Ok(rx)
    }

    async fn health_check(&self) -> bool {
        let client = self.http_client();
        client
            .get(self.api_url("getMe"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn on_status(&self, recipient: &str, status: ProcessingStatus) {
        match status {
            ProcessingStatus::Thinking => {
                // Remove 👀 from all tracked messages, replace with 🤔.
                // Scope the lock to avoid holding parking_lot::MutexGuard across await.
                let ack_info = self.pending_acks.lock().get(recipient).cloned();
                let msg_ids = match ack_info {
                    Some(ids) if !ids.is_empty() => ids,
                    _ => return,
                };
                let mut status_ids = Vec::with_capacity(msg_ids.len());
                for (chat_id, msg_id) in &msg_ids {
                    self.remove_ack(*chat_id, *msg_id).await;
                    let _ = self.set_reaction(*chat_id, *msg_id, "🤔").await;
                    status_ids.push((*chat_id, *msg_id));
                }
                self.status_reactions
                    .lock()
                    .insert(recipient.to_string(), status_ids);
            }
            ProcessingStatus::Done => {
                // Stop typing keep-alive (critical for streaming path where
                // send_message is skipped — typing would run forever otherwise).
                self.stop_internal_typing(recipient);

                // Remove ack reactions (👀) — normally done in send_message,
                // but streaming path skips it.
                let ack_info = self.pending_acks.lock().remove(recipient);
                if let Some(msg_ids) = ack_info {
                    for (chat_id, msg_id) in msg_ids {
                        self.remove_ack(chat_id, msg_id).await;
                    }
                }

                // Remove 🤔 reaction from all tracked messages.
                let status_info = self.status_reactions.lock().remove(recipient);
                if let Some(msg_ids) = status_info {
                    for (chat_id, msg_id) in msg_ids {
                        let _ = self.remove_reaction(chat_id, msg_id, "🤔").await;
                    }
                }
            }
            ProcessingStatus::Error => {
                // Stop typing on error too.
                self.stop_internal_typing(recipient);
                // Replace 🤔 with ❌ on all tracked messages.
                // Keep entries in status_reactions for cleanup on next message.
                let info = self.status_reactions.lock().get(recipient).cloned();
                if let Some(msg_ids) = info {
                    for (chat_id, msg_id) in &msg_ids {
                        let _ = self.remove_reaction(*chat_id, *msg_id, "🤔").await;
                        let _ = self.set_reaction(*chat_id, *msg_id, "❌").await;
                    }
                }
            }
        }
    }

    fn create_stream(&self, reply_target: &str) -> Option<Box<dyn TurnStream>> {
        self.create_stream_folding(reply_target, None)
    }

    /// 单 preview (2026-08-12): like `create_stream`, but when `fold` names
    /// an existing preview message the stream takes it over — `msg_id` is
    /// seeded so the first flush EDITS that message (append lines) instead
    /// of sending a second one; `text` seeds the inherited history so the
    /// user keeps seeing prior progress (保留历史行追加). If the fold target
    /// was deleted server-side, `flush_preview` falls back to sending a new
    /// message.
    fn create_stream_folding(
        &self,
        reply_target: &str,
        fold: Option<FoldCandidate>,
    ) -> Option<Box<dyn TurnStream>> {
        let (chat_id_str, thread_id) = Self::parse_reply_target(reply_target);
        let chat_id: i64 = match chat_id_str.parse() {
            Ok(id) => id,
            Err(_) => return None,
        };

        // In "off" mode, don't create a stream at all.
        if self.streaming_mode == crate::config::channel::StreamingMode::Off {
            return None;
        }

        // Mark this target as actively streaming so the stall watchdog
        // skips it (the live preview replaces the "still thinking" message).
        self.streaming_targets
            .lock()
            .insert(reply_target.to_string());

        Some(Box::new(TelegramTurnStream::new_stream(
            self.clone(),
            chat_id,
            thread_id,
            reply_target,
            self.streaming_mode,
            fold,
        )))
    }
}

