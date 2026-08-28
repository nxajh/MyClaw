use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};

use super::api::{CONTINUATION_OVERHEAD, RICH_MESSAGE_LENGTH};
use super::super::types::GetUpdatesResponse;
use super::TelegramChannel;
use super::ReactionTracker;

impl TelegramChannel {
    /// Buffer an inbound message for debounce merging.
    ///
    /// Messages from the same sender in the same conversation are merged
    /// and dispatched as a single `ChannelInboundMessage` after the debounce window
    /// expires. If debounce is disabled (`debounce_ms == 0`), the message is
    /// sent immediately via `tx`. Buffer/merge/timer mechanics live in the
    /// shared `InboundDebouncer`.
    async fn debounce_send(
        &self,
        msg: ChannelInboundMessage,
        tx: mpsc::Sender<ChannelInboundMessage>,
    ) {
        self.debouncer.push(msg, tx).await;
    }

    /// Background task that monitors for stalled conversations.
    ///
    /// If typing has been active for longer than `stall_timeout_secs` for a
    /// recipient, sends a "still thinking" notice so the user knows the bot
    /// is alive. Only one notice is sent per stall event.
    pub(super) async fn stall_watchdog(&self) {
        if self.stall_timeout_secs == 0 {
            return; // disabled
        }

        let check_interval = std::time::Duration::from_secs(10);
        let mut interval = tokio::time::interval(check_interval);
        let stall_timeout = std::time::Duration::from_secs(self.stall_timeout_secs);
        // Track sent stall notices: reply_target → [(chat_id, msg_id)].
        let notified: ReactionTracker = Arc::new(Mutex::new(std::collections::HashMap::new()));

        loop {
            interval.tick().await;
            let now = std::time::Instant::now();

            let stalled: Vec<(String, std::time::Duration)> = {
                let typing = self.typing_started_at.lock();
                let streaming = self.streaming_targets.lock();
                typing
                    .iter()
                    .filter_map(|(target, started)| {
                        // Skip targets with an active stream — the live
                        // preview already signals "I'm working".
                        if streaming.contains(target) {
                            return None;
                        }
                        let elapsed = now.duration_since(*started);
                        if elapsed >= stall_timeout {
                            Some((target.clone(), elapsed))
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            for (target, elapsed) in stalled {
                let secs = elapsed.as_secs();
                let (chat_id, _thread_id) = Self::parse_reply_target(&target);
                let chat_id_i64: i64 = chat_id.parse().unwrap_or(0);

                // Check if we already have a stall message for this target.
                let existing_msg_ids = {
                    let n = notified.lock();
                    n.get(&target).cloned()
                };

                // Already have a stall message — edit it in place.
                if let Some(msg_ids) = existing_msg_ids {
                    let text = format!("🤔 还在思考中... (已等待 {secs}s)");
                    for (cid, mid) in msg_ids {
                        if let Err(e) = self.edit_message_text_raw(cid, mid, &text).await {
                            warn!("Failed to edit stall message {mid}: {e}");
                        }
                    }
                    continue;
                }

                // First time over threshold — send a new stall message.
                warn!("Stall detected for {target}: typing for {secs}s without response");
                match self
                    .send_text(
                        &chat_id,
                        &format!("🤔 还在思考中... (已等待 {secs}s)"),
                        None,
                        None,
                    )
                    .await
                {
                    Ok(Some(stall_msg_id)) => {
                        self.stall_messages
                            .lock()
                            .entry(target.clone())
                            .or_default()
                            .push((chat_id_i64, stall_msg_id));
                        let mut n = notified.lock();
                        n.insert(target.clone(), vec![(chat_id_i64, stall_msg_id)]);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Failed to send stall notice: {e}");
                    }
                }
            }

            // Clean up notified entries for recipients that are no longer typing.
            let typing_keys: std::collections::HashSet<String> =
                self.typing_started_at.lock().keys().cloned().collect();
            notified.lock().retain(|k, _| typing_keys.contains(k));
        }
    }

    /// The actual long-poll loop. Runs until channel is closed.
    pub(super) async fn poll_loop(&self, tx: mpsc::Sender<ChannelInboundMessage>) {
        let mut offset: i64 = self.load_offset();
        if offset > 0 {
            info!("Resuming Telegram polling from persisted offset {offset}");
        }

        loop {
            let http = self.http_client();
            let url = format!(
                "{}?offset={}&timeout=30",
                self.api_url("getUpdates"),
                offset
            );

            let resp = match http.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Telegram getUpdates network error: {e}, retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            if !resp.status().is_success() {
                warn!(
                    "Telegram getUpdates HTTP error: {}, retrying in 5s",
                    resp.status()
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }

            let data: Result<GetUpdatesResponse, _> = resp.json().await;
            let updates = match data {
                Ok(d) if d.ok => d.result,
                Ok(d) => {
                    warn!("Telegram getUpdates returned ok=false: {:?}", d);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                Err(e) => {
                    warn!("Telegram getUpdates parse error: {e}, retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Persist the offset *before* processing any updates.
            // This ensures that if the process is killed mid-processing,
            // a restart will not re-fetch these updates.
            if let Some(last) = updates.last() {
                offset = last.update_id + 1;
                self.persist_offset(offset);
                debug!("Persisted Telegram offset {offset}");
            }

            for update in updates.into_iter() {
                // Handle callback query (inline keyboard button click).
                if let Some(cq) = update.callback_query {
                    // ACK the callback query to stop loading spinner.
                    self.answer_callback_query(&cq.id).await;

                    let data = match cq.data {
                        Some(d) if !d.is_empty() => d,
                        _ => continue,
                    };
                    let from_user = match cq.from {
                        Some(u) => u,
                        None => continue,
                    };
                    let chat = match cq.message {
                        Some(ref m) => m.chat.clone(),
                        None => continue,
                    };

                    // User filtering (callback queries are user-initiated
                    // button taps; we treat group callbacks as implicit
                    // @mentions so MentionOnly mode still lets them through).
                    let sender_username = from_user.username.as_deref();
                    let sender_id = Some(from_user.id);
                    let scope = if Self::is_group_message(&chat) {
                        crate::channels::MessageScope::Group {
                            id: &chat.id.to_string(),
                            has_mention: true,
                        }
                    } else {
                        crate::channels::MessageScope::Direct
                    };
                    match self.try_authorize(sender_username, sender_id, scope) {
                        crate::channels::AuthDecision::Allow => {}
                        crate::channels::AuthDecision::Ignore => continue,
                        crate::channels::AuthDecision::Reject { reason } => {
                            warn!(reason, "telegram: callback rejected by policy");
                            continue;
                        }
                    }

                    // Dedup using callback query ID
                    let update_id = format!("cq_{}", cq.id);
                    if self.dedup.check_and_record(&update_id) {
                        continue;
                    }

                    let reply_target =
                        if let Some(tid) = cq.message.as_ref().and_then(|m| m.message_thread_id) {
                            format!("{}:{}", chat.id, tid)
                        } else {
                            chat.id.to_string()
                        };

                    let channel_msg = ChannelInboundMessage {
                        id: update_id,
                        sender: MessageSender::new(
                            sender_username
                                .map(|u| u.to_string())
                                .or_else(|| sender_id.map(|id| id.to_string()))
                                .unwrap_or_default(),
                        ),
                        receiver: MessageReceiver::new(reply_target).with_thread(
                            cq.message
                                .as_ref()
                                .and_then(|m| m.message_thread_id)
                                .map(|id| id.to_string())
                                .unwrap_or_default(),
                        ),
                        content: ChannelMessageContent::text(data),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        interruption_scope_id: None,
                        silenced_override: None,
                        run_mode: Default::default(),
                    };

                    // Send ack reaction if enabled.
                    if self.ack_reactions {
                        let chat_id = chat.id;
                        let msg_id = cq.message.as_ref().map(|m| m.message_id).unwrap_or(0);
                        self.ack_message(chat_id, msg_id).await;
                        self.pending_acks
                            .lock()
                            .entry(channel_msg.receiver.id.clone())
                            .or_default()
                            .push((chat_id, msg_id));
                    }

                    if let Err(e) = tx.send(channel_msg.clone()).await {
                        warn!("Telegram dispatch callback error: {e}");
                    }
                    self.start_internal_typing(&channel_msg.receiver.id);

                    continue;
                }

                let msg = match update.message {
                    Some(m) => m,
                    None => continue,
                };

                let chat = msg.chat.clone();
                let from = msg.from.clone();

                let has_text = msg.text.is_some();
                let has_photo = msg.photo.is_some();
                let has_voice = msg.voice.is_some();
                let has_audio = msg.audio.is_some();
                let has_video = msg.video.is_some();
                let has_video_note = msg.video_note.is_some();
                let has_document = msg.document.is_some();
                let has_forward = msg.forward_from.is_some()
                    || msg.forward_from_chat.is_some()
                    || msg.forward_sender_name.is_some();

                if !has_text
                    && !has_photo
                    && !has_voice
                    && !has_audio
                    && !has_video
                    && !has_video_note
                    && !has_document
                    && !has_forward
                {
                    continue;
                }

                let sender_username = from.as_ref().and_then(|u| u.username.as_deref());
                let sender_id = from.as_ref().map(|u| u.id);

                // Phase 4: build a MessageScope and let security_policy decide.
                // has_mention is computed unconditionally so the policy (not the
                // caller) controls whether to require it.
                let chat_id_str = chat.id.to_string();
                let scope = if Self::is_group_message(&chat) {
                    let text = msg.text.as_deref().unwrap_or("");
                    let has_mention = self.contains_bot_mention(text) || self.is_reply_to_bot(&msg);
                    crate::channels::MessageScope::Group {
                        id: &chat_id_str,
                        has_mention,
                    }
                } else {
                    crate::channels::MessageScope::Direct
                };
                match self.try_authorize(sender_username, sender_id, scope) {
                    crate::channels::AuthDecision::Allow => {}
                    crate::channels::AuthDecision::Ignore => continue,
                    crate::channels::AuthDecision::Reject { reason } => {
                        warn!(
                            chat_id = chat.id,
                            sender_id = ?sender_id,
                            reason,
                            "telegram: inbound rejected by policy"
                        );
                        continue;
                    }
                }

                let update_id = update.update_id.to_string();
                if self.dedup.check_and_record(&update_id) {
                    // Already seen this update — skip
                    continue;
                }

                let mut content = self.parse_message_content(&msg);
                let mut files: Vec<ChannelFile> = Vec::new();

                // Handle photo messages: download the largest photo and save to temp file
                if let Some(photos) = &msg.photo {
                    if let Some(largest) = photos.last() {
                        match self.download_file_bytes(&largest.file_id).await {
                            Ok(data) => {
                                let temp_path = std::env::temp_dir()
                                    .join(format!("myclaw-tg-img-{}.png", uuid::Uuid::new_v4()));
                                if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                    files.push(ChannelFile {
                                        meta: ChannelFileMeta {
                                            file_name: format!("photo-{}.png", largest.file_id),
                                            mime_type: Some("image/png".to_string()),
                                            size_bytes: Some(data.len() as u64),
                                            source_url: None,
                                        },
                                        body: Arc::new(LocalFileBody::new(temp_path)),
                                    });
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "Telegram download failed for photo {}: {e}",
                                    largest.file_id
                                );
                            }
                        }
                    }
                    // Use caption if available, otherwise default to "[图片]"
                    if content.is_empty() {
                        content = msg.caption.clone().unwrap_or_default();
                    }
                }

                // Handle voice / audio messages: download and save to temp file.
                if let Some(voice) = &msg.voice {
                    let mime = voice
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "audio/ogg".to_string());
                    let fname = format!("voice-{}.ogg", voice.file_id);
                    match self.download_file_bytes(&voice.file_id).await {
                        Ok(data) => {
                            let temp_path = std::env::temp_dir()
                                .join(format!("myclaw-tg-{}", uuid::Uuid::new_v4()));
                            if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                files.push(ChannelFile {
                                    meta: ChannelFileMeta {
                                        file_name: fname,
                                        mime_type: Some(mime),
                                        size_bytes: Some(data.len() as u64),
                                        source_url: None,
                                    },
                                    body: Arc::new(LocalFileBody::new(temp_path)),
                                });
                            }
                        }
                        Err(e) => {
                            warn!("Telegram download failed for voice {}: {e}", voice.file_id)
                        }
                    }
                    if content.is_empty() {
                        content = msg.caption.clone().unwrap_or_default();
                    }
                }
                if let Some(audio) = &msg.audio {
                    let mime = audio
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "audio/mpeg".to_string());
                    let fname = format!("audio-{}", audio.file_id);
                    match self.download_file_bytes(&audio.file_id).await {
                        Ok(data) => {
                            let temp_path = std::env::temp_dir()
                                .join(format!("myclaw-tg-{}", uuid::Uuid::new_v4()));
                            if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                files.push(ChannelFile {
                                    meta: ChannelFileMeta {
                                        file_name: fname,
                                        mime_type: Some(mime),
                                        size_bytes: Some(data.len() as u64),
                                        source_url: None,
                                    },
                                    body: Arc::new(LocalFileBody::new(temp_path)),
                                });
                            }
                        }
                        Err(e) => {
                            warn!("Telegram download failed for audio {}: {e}", audio.file_id)
                        }
                    }
                    if content.is_empty() {
                        content = msg.caption.clone().unwrap_or_default();
                    }
                }

                // Handle video / video_note / document messages.
                if let Some(video) = &msg.video {
                    let mime = video
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "video/mp4".to_string());
                    let fname = format!("video-{}", video.file_id);
                    match self.download_file_bytes(&video.file_id).await {
                        Ok(data) => {
                            let temp_path = std::env::temp_dir()
                                .join(format!("myclaw-tg-{}", uuid::Uuid::new_v4()));
                            if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                files.push(ChannelFile {
                                    meta: ChannelFileMeta {
                                        file_name: fname,
                                        mime_type: Some(mime),
                                        size_bytes: Some(data.len() as u64),
                                        source_url: None,
                                    },
                                    body: Arc::new(LocalFileBody::new(temp_path)),
                                });
                            }
                        }
                        Err(e) => {
                            warn!("Telegram download failed for video {}: {e}", video.file_id)
                        }
                    }
                    if content.is_empty() {
                        content = msg.caption.clone().unwrap_or_default();
                    }
                }
                if let Some(vn) = &msg.video_note {
                    let fname = format!("videonote-{}", vn.file_id);
                    match self.download_file_bytes(&vn.file_id).await {
                        Ok(data) => {
                            let temp_path = std::env::temp_dir()
                                .join(format!("myclaw-tg-{}", uuid::Uuid::new_v4()));
                            if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                files.push(ChannelFile {
                                    meta: ChannelFileMeta {
                                        file_name: fname,
                                        mime_type: Some("video/mp4".to_string()),
                                        size_bytes: Some(data.len() as u64),
                                        source_url: None,
                                    },
                                    body: Arc::new(LocalFileBody::new(temp_path)),
                                });
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Telegram download failed for video_note {}: {e}",
                                vn.file_id
                            )
                        }
                    }
                    if content.is_empty() {
                        content = msg.caption.clone().unwrap_or_default();
                    }
                }
                if let Some(doc) = &msg.document {
                    let mime = doc
                        .mime_type
                        .clone()
                        .or_else(|| {
                            doc.file_name
                                .as_deref()
                                .and_then(crate::providers::media::infer_mime_from_name)
                        })
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    let fname = doc
                        .file_name
                        .clone()
                        .unwrap_or_else(|| format!("file-{}", doc.file_id));
                    match self.download_file_bytes(&doc.file_id).await {
                        Ok(data) => {
                            let temp_path = std::env::temp_dir()
                                .join(format!("myclaw-tg-{}", uuid::Uuid::new_v4()));
                            if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                files.push(ChannelFile {
                                    meta: ChannelFileMeta {
                                        file_name: fname,
                                        mime_type: Some(mime),
                                        size_bytes: Some(data.len() as u64),
                                        source_url: None,
                                    },
                                    body: Arc::new(LocalFileBody::new(temp_path)),
                                });
                            }
                        }
                        Err(e) => {
                            warn!("Telegram download failed for document {}: {e}", doc.file_id)
                        }
                    }
                    if content.is_empty() {
                        content = msg.caption.clone().unwrap_or_default();
                    }
                }

                let channel_msg = ChannelInboundMessage {
                    id: update_id,
                    sender: MessageSender::new(
                        sender_username
                            .map(|u| u.to_string())
                            .or_else(|| sender_id.map(|id| id.to_string()))
                            .unwrap_or_default(),
                    ),
                    receiver: MessageReceiver::new(if let Some(tid) = msg.message_thread_id {
                        format!("{}:{}", chat.id, tid)
                    } else {
                        chat.id.to_string()
                    }),
                    content: ChannelMessageContent {
                        text: content,
                        files,
                        buttons: vec![],
                    },
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    interruption_scope_id: None,
                    silenced_override: None,
                    run_mode: Default::default(),
                };

                if self.debouncer.window_ms() > 0 {
                    // Clean up stale error reactions from previous interactions
                    let stale_status = self
                        .status_reactions
                        .lock()
                        .remove(&channel_msg.receiver.id);
                    if let Some(msg_ids) = stale_status {
                        for (cid, mid) in msg_ids {
                            let _ = self.remove_reaction(cid, mid, "❌").await;
                        }
                    }

                    // Ack every message, accumulate all message IDs.
                    if self.ack_reactions {
                        self.ack_message(chat.id, msg.message_id).await;
                        self.pending_acks
                            .lock()
                            .entry(channel_msg.receiver.id.clone())
                            .or_default()
                            .push((chat.id, msg.message_id));
                    }

                    let is_new = !self
                        .debouncer
                        .is_pending(&channel_msg.sender.id, &channel_msg.receiver.id);
                    if is_new {
                        self.start_internal_typing(&channel_msg.receiver.id);
                    }
                    self.debounce_send(channel_msg, tx.clone()).await;
                } else {
                    // Clean up stale error reactions from previous interactions
                    let stale_status = self
                        .status_reactions
                        .lock()
                        .remove(&channel_msg.receiver.id);
                    if let Some(msg_ids) = stale_status {
                        for (cid, mid) in msg_ids {
                            let _ = self.remove_reaction(cid, mid, "❌").await;
                        }
                    }

                    // No debounce — ack every message.
                    if self.ack_reactions {
                        self.ack_message(chat.id, msg.message_id).await;
                        self.pending_acks
                            .lock()
                            .entry(channel_msg.receiver.id.clone())
                            .or_default()
                            .push((chat.id, msg.message_id));
                    }
                    if let Err(e) = tx.send(channel_msg.clone()).await {
                        warn!("Telegram dispatch error: {e}");
                    }
                    self.start_internal_typing(&channel_msg.receiver.id);
                }
            }
        }
    }

    /// Split message content into chunks that fit Telegram's rich message limit.
    ///
    /// Rich messages (Bot API 10.1 `sendRichMessage`) support up to 32 768
    /// UTF-8 characters. We leave a margin for the `(continues...)` suffix
    /// and for Telegram's own overhead.
    pub(super) fn chunk_for_telegram(content: &str) -> Vec<String> {
        use crate::channels::message::{LenUnit, split_message_chunk};

        let rich_limit = RICH_MESSAGE_LENGTH.saturating_sub(CONTINUATION_OVERHEAD * 2);
        split_message_chunk(content, rich_limit, LenUnit::Codepoints)
    }
}

