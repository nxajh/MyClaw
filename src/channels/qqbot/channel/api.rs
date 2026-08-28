use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use parking_lot::Mutex;
use tracing::{debug, error, warn};

use super::super::flow::{DeliverDebouncer, RateLimiter, ReplyLimiter};
use super::super::keyboard::*;
use super::super::markdown_sanitize::sanitize_qq_markdown;
use super::super::message::split_message_chunk;
use super::super::token::TokenManager;
use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};
use crate::channels::shared::{TypingKeepAlive, TypingParams};
use crate::config::channel::QQBotAccountConfig;
use crate::{Channel, DedupState};

use super::QQBotChannel;
use super::is_ssrf_blocked;
use super::protocol::{
    API_BASE, GATEWAY_URL, INTENTS, OP_IDENTIFY, mime_ext_from_content_type, user_agent,
};
use super::text::{QQ_MAX_VISUAL_LINES_PER_BUBBLE, split_by_visual_lines, strip_internal_tags};

/// Maximum file size for inline base64 upload (10 MB).
///
/// QQ Bot inline file upload (msg_type=7) requires base64-encoding the entire
/// payload, so large files risk memory exhaustion. Files with a `source_url`
/// bypass this limit via URL direct upload; only files without a valid URL
/// that exceed this limit will bail.
pub(super) const MAX_INLINE_UPLOAD_BYTES: usize = 10_485_760;

impl QQBotChannel {
    pub fn new(account_id: String, config: QQBotAccountConfig) -> Self {
        let app_id = config.app_id.clone();
        let client_secret = config.client_secret.clone();

        // Extract debounce config before `config` is moved into the struct.
        let debounce_window_ms = config.debounce_window_ms;
        let debounce_separator = config.debounce_separator.clone();

        let ch = Self {
            config,
            account_id: account_id.clone(),
            token_manager: Arc::new(TokenManager::new(account_id, app_id, client_secret)),
            dedup: DedupState::new(),
            last_seq: Arc::new(Mutex::new(None)),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            typing: TypingKeepAlive::new(),
            session: Arc::new(Mutex::new(None)),
            msg_seq_counter: Arc::new(AtomicU32::new(1)),
            started_at: std::time::Instant::now(),
            group_history: Arc::new(Mutex::new(std::collections::HashMap::new())),
            reply_limiter: Arc::new(Mutex::new(ReplyLimiter::new())),
            rate_limiter: Arc::new(Mutex::new(RateLimiter::new())),
            debouncer: Arc::new(DeliverDebouncer::new(
                debounce_window_ms,
                debounce_separator,
            )),
        };
        crate::channels::warn_if_locked_down(&ch);
        ch
    }

    /// Return the next proactive msg_seq value (monotonically increasing).
    pub(super) fn next_msg_seq(&self) -> u32 {
        self.msg_seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Send plain text (no keyboard, no files) to a recipient, chunked.
    /// Used by the debounce flush path. Returns an empty-id result on success.
    pub(super) async fn send_plain_text_chunked(
        &self,
        recipient: &str,
        text: &str,
        msg_id: &str,
    ) -> anyhow::Result<crate::channels::OutboundSendResult> {
        let sanitized = sanitize_qq_markdown(&strip_internal_tags(text));
        let pre_chunks = split_by_visual_lines(&sanitized, QQ_MAX_VISUAL_LINES_PER_BUBBLE);
        let mut chunks = Vec::new();
        for pre in pre_chunks {
            chunks.append(&mut split_message_chunk(
                &pre,
                self.capabilities().message_chunk_limit,
                self.capabilities().message_len_unit,
            ));
        }
        if chunks.is_empty() {
            return Ok(crate::channels::OutboundSendResult::empty());
        }
        let count = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            let msg_seq = self.next_msg_seq();
            let result = if let Some(openid) = recipient.strip_prefix("c2c:") {
                self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
            } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                self.send_group_message(group_openid, chunk, msg_id, msg_seq)
                    .await
            } else {
                anyhow::bail!(
                    "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                    recipient
                )
            };
            if let Err(e) = result {
                error!(chunk = i, err = %e, "failed to send debounced text chunk");
                return Err(e);
            }
            if i < count - 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        Ok(crate::channels::OutboundSendResult::empty())
    }

    /// Build the unified security policy from QQBot config (RFC §14.5).
    pub(super) fn build_security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        use crate::channels::{AllowList, ChannelSecurityPolicy, GroupAuthMode};
        let group_allowlist = AllowList::from_config(self.config.allowed_groups.clone());
        let group_mode = match &self.config.allowed_groups {
            None => GroupAuthMode::Reject,  // Phase 4 "统一关"
            Some(_) => GroupAuthMode::Open, // QQBot has no @mention concept
        };
        ChannelSecurityPolicy {
            allowed_users: AllowList::from_config(self.config.allowed_users.clone()),
            group_mode,
            group_allowlist,
        }
    }

    /// Fetch WebSocket gateway URL from the API.
    pub(super) async fn fetch_gateway_url(&self) -> anyhow::Result<String> {
        let token = self.token_manager.get_token().await?;
        let ua = user_agent();
        let resp = self
            .http_client
            .get(GATEWAY_URL)
            .header("Authorization", format!("QQBot {}", token))
            .header("User-Agent", &ua)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("gateway request failed: {}", e))?;

        if resp.status().is_success() {
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("gateway parse error: {}", e))?;
            return data["url"]
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("missing url in gateway response"));
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        // Token expired? Force-refresh and retry once.
        if status.as_u16() == 401 || text.contains("11244") {
            warn!(account = %self.account_id, status = %status, "gateway got token-expired error, refreshing and retrying");
            let new_token = self.token_manager.refresh().await?;
            let ua = user_agent();
            let resp = self
                .http_client
                .get(GATEWAY_URL)
                .header("Authorization", format!("QQBot {}", new_token))
                .header("User-Agent", &ua)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("gateway retry request failed: {}", e))?;

            if resp.status().is_success() {
                let data: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("gateway parse error: {}", e))?;
                return data["url"]
                    .as_str()
                    .map(String::from)
                    .ok_or_else(|| anyhow::anyhow!("missing url in gateway response"));
            }

            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("gateway returned {}: {}", status, text));
        }

        Err(anyhow::anyhow!("gateway returned {}: {}", status, text))
    }

    /// Build an Identify payload.
    pub(super) fn build_identify(&self, token: &str) -> String {
        let payload = serde_json::json!({
            "op": OP_IDENTIFY,
            "d": {
                "token": format!("QQBot {}", token),
                "intents": INTENTS,
                "shard": [0, 1],
            }
        });
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Handle a dispatch event (OpCode 0).
    pub(super) fn handle_dispatch(
        &self,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Option<ChannelInboundMessage> {
        fn apply_auth(
            ch: &QQBotChannel,
            sender: &str,
            scope: crate::channels::MessageScope<'_>,
        ) -> bool {
            use crate::channels::Channel;
            match ch.check_authorization(sender, scope) {
                crate::channels::AuthDecision::Allow => true,
                crate::channels::AuthDecision::Ignore => {
                    debug!(sender = %sender, "qqbot: inbound ignored by policy");
                    false
                }
                crate::channels::AuthDecision::Reject { reason } => {
                    warn!(sender = %sender, reason, "qqbot: inbound rejected by policy");
                    false
                }
            }
        }
        match event_type {
            "C2C_MESSAGE_CREATE" => {
                debug!(data = %data, "C2C_MESSAGE_CREATE raw payload");
                let msg = match self.parse_c2c_message(data) {
                    Some(m) => m,
                    None => {
                        tracing::warn!(
                            data = %data,
                            "C2C_MESSAGE_CREATE: parse returned None, message dropped"
                        );
                        return None;
                    }
                };
                if self.dedup.check_and_record(&msg.id) {
                    debug!(msg_id = %msg.id, "duplicate C2C message, skipping");
                    return None;
                }
                if !self.rate_limiter.lock().check(&msg.sender.id) {
                    warn!(sender = %msg.sender.id, "qqbot: rate limited");
                    return None;
                }
                if !apply_auth(self, &msg.sender.id, crate::channels::MessageScope::Direct) {
                    return None;
                }
                if !msg.content.files.is_empty() {
                    tracing::info!(
                        msg_id = %msg.id,
                        files = msg.content.files.len(),
                        content_len = msg.content.text.len(),
                        "C2C message with file attachments received"
                    );
                }
                Some(msg)
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                let mut msg = match self.parse_group_message(data) {
                    Some(m) => m,
                    None => {
                        tracing::warn!(
                            data = %data,
                            "GROUP_AT_MESSAGE_CREATE: parse returned None, message dropped"
                        );
                        return None;
                    }
                };
                if self.dedup.check_and_record(&msg.id) {
                    debug!(msg_id = %msg.id, "duplicate group message, skipping");
                    return None;
                }
                if !self.rate_limiter.lock().check(&msg.sender.id) {
                    warn!(sender = %msg.sender.id, "qqbot: rate limited");
                    return None;
                }
                let group_id = msg
                    .receiver
                    .id
                    .strip_prefix("group:")
                    .unwrap_or("")
                    .to_string();
                // GROUP_AT_MESSAGE_CREATE is by definition an @-mention, so
                // has_mention=true. Policy decides whether the group itself is allowed.
                if !apply_auth(
                    self,
                    &msg.sender.id,
                    crate::channels::MessageScope::Group {
                        id: &group_id,
                        has_mention: true,
                    },
                ) {
                    return None;
                }
                // Inject recent group chat history as context.
                self.inject_group_history(&mut msg, &group_id);
                Some(msg)
            }
            "INTERACTION_CREATE" => {
                // QQ Bot interaction: button click -> convert to text message.
                debug!(data = %data, "INTERACTION_CREATE raw event data");
                let resolved = data
                    .get("data")
                    .and_then(|d| d.get("resolved"))
                    .or_else(|| data.get("resolved"));

                // Try to get button callback data
                let button_data = resolved
                    .and_then(|r| r.get("button_data"))
                    .or_else(|| resolved.and_then(|r| r.get("data")))
                    .or_else(|| data.get("data").and_then(|d| d.get("button_data")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if button_data.is_empty() {
                    debug!("INTERACTION_CREATE with empty button data, ignoring");
                    return None;
                }

                let event_id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");

                // Determine sender and reply_target based on chat_type
                // (0=guild, 1=group, 2=C2C). Note: `type` is the interaction
                // kind (11=button callback), NOT the chat scene.
                let chat_type = data.get("chat_type").and_then(|v| v.as_u64()).unwrap_or(2);

                let (sender, reply_target) = if chat_type == 1 {
                    // Group interaction — member_openid at top level
                    let member_openid = data
                        .get("member_openid")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            data.get("author")
                                .and_then(|a| a.get("member_openid"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("unknown");
                    let group_openid = data
                        .get("group_openid")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    (member_openid.to_string(), format!("group:{}", group_openid))
                } else {
                    // C2C interaction — user_openid at top level
                    let user_openid = data
                        .get("user_openid")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            data.get("author")
                                .and_then(|a| a.get("user_openid"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("unknown");
                    (user_openid.to_string(), format!("c2c:{}", user_openid))
                };

                // Access check for interaction.
                let scope = if let Some(group_id) = reply_target.strip_prefix("group:") {
                    crate::channels::MessageScope::Group {
                        id: group_id,
                        has_mention: true,
                    }
                } else {
                    crate::channels::MessageScope::Direct
                };
                if !apply_auth(self, &sender, scope) {
                    return None;
                }

                // Acknowledge the interaction within 3 seconds (QQ Bot requirement).
                self.ack_interaction(event_id);

                // Try to extract the original message ID from the interaction
                // event for passive-reply routing (avoids active-message restrictions).
                let original_msg_id = data
                    .get("message_id")
                    .or_else(|| data.get("data").and_then(|d| d.get("message_id")))
                    .or_else(|| resolved.and_then(|r| r.get("message_id")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(ref mid) = original_msg_id {
                    debug!(msg_id = %mid, "INTERACTION_CREATE: extracted original message_id for passive reply");
                }

                Some(ChannelInboundMessage {
                    id: event_id.to_string(),
                    sender: MessageSender::new(sender),
                    receiver: MessageReceiver::new(reply_target)
                        .with_reply_to(original_msg_id.unwrap_or_default()),
                    content: ChannelMessageContent::text(button_data.to_string()),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    interruption_scope_id: None,
                    silenced_override: None,
                    run_mode: Default::default(),
                })
            }
            _ => {
                debug!(event = event_type, "ignoring dispatch event");
                None
            }
        }
    }

    /// Parse a C2C_MESSAGE_CREATE event into a ChannelInboundMessage.
    fn parse_c2c_message(&self, data: &serde_json::Value) -> Option<ChannelInboundMessage> {
        let author = data.get("author")?;
        let user_openid = author.get("user_openid")?.as_str()?;
        // content may be absent or empty for image-only messages — don't bail on it
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let msg_id = data.get("id")?.as_str()?;

        Some(ChannelInboundMessage {
            id: msg_id.to_string(),
            sender: MessageSender::new(user_openid.to_string()),
            receiver: MessageReceiver::new(format!("c2c:{}", user_openid)),
            content: ChannelMessageContent::text(content),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: Default::default(),
        })
    }

    /// Parse a GROUP_AT_MESSAGE_CREATE event into a ChannelInboundMessage.
    fn parse_group_message(&self, data: &serde_json::Value) -> Option<ChannelInboundMessage> {
        let author = data.get("author")?;
        let member_openid = author.get("member_openid")?.as_str()?;
        let group_openid = data.get("group_openid")?.as_str()?;
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let msg_id = data.get("id")?.as_str()?;

        Some(ChannelInboundMessage {
            id: msg_id.to_string(),
            sender: MessageSender::new(member_openid.to_string()),
            receiver: MessageReceiver::new(format!("group:{}", group_openid)),
            content: ChannelMessageContent::text(content),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            interruption_scope_id: None,
            silenced_override: None,
            run_mode: Default::default(),
        })
    }

    /// Resolve the per-group history limit from config.
    ///
    /// Checks for an explicit entry for `group_openid`, then falls back to the
    /// wildcard `"*"` entry, and finally to the default of 20.
    pub(super) fn resolve_group_history_limit(&self, group_openid: &str) -> usize {
        self.config
            .group_config
            .get(group_openid)
            .and_then(|c| c.history_limit)
            .or_else(|| {
                self.config
                    .group_config
                    .get("*")
                    .and_then(|c| c.history_limit)
            })
            .unwrap_or(20)
    }

    /// Record a group message in the per-group history buffer.
    ///
    /// Called for **all** group events (even rejected ones) so the bot has
    /// recent context when it is eventually @-mentioned.
    pub(super) fn record_group_history(&self, group_openid: &str, sender: &str, content: &str) {
        let limit = self.resolve_group_history_limit(group_openid);
        let mut history = self.group_history.lock();
        let entries = history.entry(group_openid.to_string()).or_default();
        entries.push_back((sender.to_string(), content.to_string()));
        while entries.len() > limit {
            entries.pop_front();
        }
    }

    /// Prepend recent group chat history to an inbound message's text.
    ///
    /// The last entry in the buffer (the current message) is excluded so only
    /// *prior* messages appear in the history block. If there is no prior
    /// history the message is left unchanged.
    pub(super) fn inject_group_history(&self, msg: &mut ChannelInboundMessage, group_openid: &str) {
        let history = self.group_history.lock();
        if let Some(entries) = history.get(group_openid) {
            // Exclude the last entry — it is the current message being processed.
            if entries.len() <= 1 {
                return;
            }
            let prior_count = entries.len() - 1;
            let history_text = entries
                .iter()
                .take(prior_count)
                .map(|(sender, content)| format!("[{}] {}", sender, content))
                .collect::<Vec<_>>()
                .join("\n");
            if !history_text.is_empty() {
                msg.content.text = format!(
                    "[Chat history begins]\n{}\n[Chat history ends]\n[Current message]\n{}",
                    history_text, msg.content.text
                );
            }
        }
    }

    /// Ingest voice/audio attachments from a raw inbound `data` payload.
    ///
    /// Per the official QQ v2 schema, an inbound voice attachment is
    /// `{ content_type: "voice", url, filename, size }` and the file is SILK.
    /// Some gateways also provide `voice_wav_url` (a pre-converted WAV) and
    /// `asr_refer_text` (Tencent's own transcription).
    ///
    /// Priority:
    /// 1. `asr_refer_text` present → inject text directly, skip download.
    /// 2. `voice_wav_url` present → download WAV (no SILK decoding needed).
    /// 3. Fallback → download `url` (SILK) and convert SILK→WAV locally via
    ///    `silk2wav` so audio models can process it. On conversion failure the
    ///    raw SILK bytes are kept.
    pub(super) async fn ingest_voice_attachments(
        &self,
        data: &serde_json::Value,
        msg: &mut ChannelInboundMessage,
    ) {
        let Some(attachments) = data.get("attachments").and_then(|a| a.as_array()) else {
            return;
        };
        for att in attachments {
            let ctype = att
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Official: content_type == "voice". Accept "audio"/"audio/*" too.
            if !(ctype == "voice" || ctype == "audio" || ctype.starts_with("audio")) {
                continue;
            }

            // Best-effort, non-official: a proxy-injected transcription.
            let asr = att
                .get("asr_refer_text")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(text) = asr {
                if msg.content.text.trim().is_empty() {
                    msg.content.text = text.to_string();
                } else {
                    msg.content.text = format!("{}\n[语音] {}", msg.content.text, text);
                }
                continue;
            }

            // Choose audio source: prefer platform-preconverted WAV over raw SILK.
            let wav_url = att
                .get("voice_wav_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let raw_url = att.get("url").and_then(|v| v.as_str());
            let Some(audio_url) = wav_url.or(raw_url) else {
                continue;
            };

            let full_url = if audio_url.starts_with("http") {
                audio_url.to_string()
            } else {
                format!("https://{audio_url}")
            };
            if is_ssrf_blocked(&full_url) {
                warn!(url = %full_url, "qqbot: blocked SSRF attempt to private address");
                continue;
            }
            let resp = match self
                .http_client
                .get(&full_url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => r,
                Err(e) => {
                    // If the WAV URL failed and a raw SILK URL exists, try it.
                    if wav_url.is_some() {
                        if let Some(fallback) = raw_url {
                            let fb_full = if fallback.starts_with("http") {
                                fallback.to_string()
                            } else {
                                format!("https://{fallback}")
                            };
                            if is_ssrf_blocked(&fb_full) {
                                warn!(url = %fb_full, "qqbot: blocked SSRF attempt (SILK fallback) to private address");
                                continue;
                            }
                            warn!(
                                "qqbot: voice_wav_url download failed ({e}), falling back to raw SILK"
                            );
                            match self
                                .http_client
                                .get(&fb_full)
                                .send()
                                .await
                                .and_then(|r| r.error_for_status())
                            {
                                Ok(r) => r,
                                Err(e2) => {
                                    warn!("qqbot: SILK fallback also failed: {e2}");
                                    continue;
                                }
                            }
                        } else {
                            warn!("qqbot: audio download failed for {full_url}: {e}");
                            continue;
                        }
                    } else {
                        warn!("qqbot: audio download failed for {full_url}: {e}");
                        continue;
                    }
                }
            };
            let mut bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    warn!("qqbot: reading audio bytes failed for {full_url}: {e}");
                    continue;
                }
            };
            // If we downloaded from voice_wav_url the format is WAV; otherwise SILK.
            // For raw SILK, attempt local SILK→WAV conversion so downstream audio
            // models can understand it without needing a SILK-aware decoder.
            let is_raw_silk = wav_url.is_none();
            let mut mime = if wav_url.is_some() {
                "audio/wav".to_string()
            } else if ctype.contains('/') {
                ctype.to_string()
            } else {
                "audio/silk".to_string()
            };
            if is_raw_silk {
                // SILK → WAV (24 kHz, QQ's default voice sample rate).
                match silk2wav::silk_to_wav(&bytes, 24000) {
                    Ok(wav_bytes) => {
                        debug!(
                            silk_bytes = bytes.len(),
                            wav_bytes = wav_bytes.len(),
                            "qqbot: SILK→WAV conversion succeeded"
                        );
                        bytes = wav_bytes.into();
                        mime = "audio/wav".to_string();
                    }
                    Err(e) => {
                        warn!("qqbot: SILK→WAV conversion failed ({e}), keeping raw SILK");
                    }
                }
            }
            let file_name = att
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("voice")
                .to_string();
            let voice_ext = if mime == "audio/wav" {
                "wav"
            } else if mime == "audio/ogg" {
                "ogg"
            } else {
                "silk"
            };
            let temp_path = std::env::temp_dir().join(format!(
                "myclaw-qq-voice-{}.{}",
                uuid::Uuid::new_v4(),
                voice_ext
            ));
            if tokio::fs::write(&temp_path, &bytes).await.is_err() {
                warn!("qqbot: failed to save voice to temp file");
                continue;
            }
            msg.content.files.push(ChannelFile {
                meta: ChannelFileMeta {
                    file_name,
                    mime_type: Some(mime),
                    size_bytes: Some(bytes.len() as u64),
                    source_url: None,
                },
                body: std::sync::Arc::new(LocalFileBody::new(temp_path)),
            });
        }
    }

    /// Download image attachments from the raw inbound `data` payload,
    /// save to temp files, and store in `msg.files`.
    pub(super) async fn ingest_image_attachments(
        &self,
        data: &serde_json::Value,
        msg: &mut ChannelInboundMessage,
    ) {
        let Some(attachments) = data.get("attachments").and_then(|a| a.as_array()) else {
            return;
        };
        for att in attachments {
            let ct = att
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !(ct == "image" || ct.starts_with("image/")) {
                continue;
            }
            let Some(url) = att.get("url").and_then(|v| v.as_str()) else {
                continue;
            };
            let full_url = if url.starts_with("http") {
                url.to_string()
            } else {
                format!("https://{url}")
            };
            if is_ssrf_blocked(&full_url) {
                warn!(url = %full_url, "qqbot: blocked SSRF attempt to private address");
                continue;
            }
            match self
                .http_client
                .get(&full_url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(resp) => match resp.bytes().await {
                    Ok(bytes) => {
                        let ext = mime_ext_from_content_type(ct);
                        let temp_path = std::env::temp_dir().join(format!(
                            "myclaw-qq-img-{}.{}",
                            uuid::Uuid::new_v4(),
                            ext
                        ));
                        if tokio::fs::write(&temp_path, &bytes).await.is_ok() {
                            tracing::debug!(url = %full_url, size = bytes.len(), "qqbot: image downloaded and saved to temp file");
                            msg.content.files.push(ChannelFile {
                                meta: ChannelFileMeta {
                                    file_name: format!("image-{}.{}", uuid::Uuid::new_v4(), ext),
                                    mime_type: Some(ct.to_string()),
                                    size_bytes: Some(bytes.len() as u64),
                                    source_url: Some(full_url.clone()),
                                },
                                body: std::sync::Arc::new(LocalFileBody::new(temp_path)),
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("qqbot: reading image bytes failed for {full_url}: {e}");
                    }
                },
                Err(e) => {
                    tracing::warn!("qqbot: image download failed for {full_url}: {e}");
                }
            }
        }
    }

    /// Download video and generic file attachments from the raw inbound `data`
    /// payload, save to temp files, and store in `msg.files`.
    pub(super) async fn ingest_video_file_attachments(
        &self,
        data: &serde_json::Value,
        msg: &mut ChannelInboundMessage,
    ) {
        let Some(attachments) = data.get("attachments").and_then(|a| a.as_array()) else {
            return;
        };
        for att in attachments {
            let ct = att
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Accept "video", "video/*", "file" (generic).
            if !(ct == "video"
                || ct.starts_with("video/")
                || ct == "file"
                || ct.starts_with("application/"))
            {
                continue;
            }
            let Some(url) = att.get("url").and_then(|v| v.as_str()) else {
                continue;
            };
            let full_url = if url.starts_with("http") {
                url.to_string()
            } else {
                format!("https://{url}")
            };
            if is_ssrf_blocked(&full_url) {
                warn!(url = %full_url, "qqbot: blocked SSRF attempt to private address");
                continue;
            }
            match self
                .http_client
                .get(&full_url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(resp) => match resp.bytes().await {
                    Ok(bytes) => {
                        let mime = if ct.contains('/') {
                            ct.to_string()
                        } else if ct == "video" {
                            "video/mp4".to_string()
                        } else {
                            "application/octet-stream".to_string()
                        };
                        let ext = att
                            .get("filename")
                            .and_then(|v| v.as_str())
                            .and_then(|n| n.rsplit_once('.').map(|(_, e)| e.to_string()))
                            .unwrap_or_else(|| {
                                if ct == "video" || ct.starts_with("video/") {
                                    "mp4".to_string()
                                } else {
                                    "bin".to_string()
                                }
                            });
                        let file_name = att
                            .get("filename")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                format!("attachment-{}.{}", uuid::Uuid::new_v4(), ext)
                            });
                        let temp_path = std::env::temp_dir().join(format!(
                            "myclaw-qq-file-{}.{}",
                            uuid::Uuid::new_v4(),
                            ext
                        ));
                        if tokio::fs::write(&temp_path, &bytes).await.is_ok() {
                            tracing::debug!(url = %full_url, size = bytes.len(), %mime, "qqbot: attachment downloaded and saved to temp file");
                            msg.content.files.push(ChannelFile {
                                meta: ChannelFileMeta {
                                    file_name,
                                    mime_type: Some(mime),
                                    size_bytes: Some(bytes.len() as u64),
                                    source_url: Some(full_url.clone()),
                                },
                                body: std::sync::Arc::new(LocalFileBody::new(temp_path)),
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "qqbot: reading attachment bytes failed for {full_url}: {e}"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!("qqbot: attachment download failed for {full_url}: {e}");
                }
            }
        }
    }

    /// Build a markdown message body for QQ Bot API.
    /// Build a plain-text message body (msg_type=0).
    /// Required for active messages where markdown (msg_type=2) is not supported.
    fn build_text_body(&self, content: &str, msg_id: &str, msg_seq: u32) -> serde_json::Value {
        let mut body = serde_json::json!({
            "content": content,
            "msg_type": 0,
        });
        if !msg_id.is_empty() {
            body["msg_id"] = serde_json::Value::String(msg_id.to_string());
        }
        body["msg_seq"] = serde_json::Value::Number(msg_seq.into());
        body
    }

    fn build_markdown_body(&self, content: &str, msg_id: &str, msg_seq: u32) -> serde_json::Value {
        let mut body = serde_json::json!({
            "content": "",
            "msg_type": 2,
            "markdown": {
                "content": content,
            },
        });
        if !msg_id.is_empty() {
            body["msg_id"] = serde_json::Value::String(msg_id.to_string());
        }
        body["msg_seq"] = serde_json::Value::Number(msg_seq.into());
        body
    }

    /// Send a REST message to QQ Bot with retry logic (token refresh + 429 backoff).
    /// `url` is the fully constructed API endpoint URL.
    /// `body` is the pre-built JSON body.
    pub(super) async fn send_rest_with_retry(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let token = self.token_manager.get_token().await?;
        let ua = user_agent();
        let resp = self
            .http_client
            .post(url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", &ua)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("QQ Bot REST send failed: {}", e))?;

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();

        // Token-expired? Force-refresh and retry once.
        if status.as_u16() == 401 || text.contains("11244") {
            warn!(account = %self.account_id, status = %status, "QQ Bot REST got token-expired error, refreshing and retrying");
            let new_token = self.token_manager.refresh().await?;
            let ua = user_agent();
            let resp = self
                .http_client
                .post(url)
                .header("Authorization", format!("QQBot {}", new_token))
                .header("Content-Type", "application/json")
                .header("User-Agent", &ua)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("QQ Bot REST retry failed: {}", e))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("QQ Bot REST returned {}: {}", status, text));
            }
            return Ok(());
        }

        // Rate limited? Wait and retry once.
        if status.as_u16() == 429 {
            let retry_after = resp_headers
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5);
            warn!(
                account = %self.account_id,
                retry_after_secs = retry_after,
                "QQ Bot REST rate limited, retrying after delay"
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
            let token = self.token_manager.get_token().await?;
            let ua = user_agent();
            let resp = self
                .http_client
                .post(url)
                .header("Authorization", format!("QQBot {}", token))
                .header("Content-Type", "application/json")
                .header("User-Agent", &ua)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("QQ Bot REST retry after 429 failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("QQ Bot REST returned {}: {}", status, text));
            }
            return Ok(());
        }

        Err(anyhow::anyhow!("QQ Bot REST returned {}: {}", status, text))
    }

    /// Upload a file to the QQ Bot `/files` endpoint and return the `file_info`
    /// string needed for the subsequent message send.
    ///
    /// Handles token-expired (401 / code 11244) by refreshing and retrying once.
    /// `files_url` is the fully constructed `/v2/groups/{id}/files` or
    /// `/v2/users/{id}/files` endpoint. `upload_body` is the pre-built JSON
    /// payload (either `file_data` base64 or `url` direct upload).
    pub(super) async fn upload_file_to_qq(
        &self,
        files_url: &str,
        upload_body: &serde_json::Value,
    ) -> anyhow::Result<String> {
        let token = self.token_manager.get_token().await?;
        let ua = user_agent();

        let upload_resp = self
            .http_client
            .post(files_url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", &ua)
            .json(upload_body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("upload failed: {e}"))?;

        let upload_resp = if !upload_resp.status().is_success() {
            let status = upload_resp.status();
            let text = upload_resp.text().await.unwrap_or_default();

            // Token expired? Force-refresh and retry once.
            if status.as_u16() == 401 || text.contains("11244") {
                warn!(
                    account = %self.account_id,
                    status = %status,
                    "file upload got token-expired error, refreshing and retrying"
                );
                let new_token = self.token_manager.refresh().await?;
                let retry_resp = self
                    .http_client
                    .post(files_url)
                    .header("Authorization", format!("QQBot {}", new_token))
                    .header("Content-Type", "application/json")
                    .header("User-Agent", &ua)
                    .json(upload_body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("upload retry failed: {e}"))?;
                if !retry_resp.status().is_success() {
                    let s = retry_resp.status();
                    let t = retry_resp.text().await.unwrap_or_default();
                    anyhow::bail!("upload returned {s}: {t}");
                }
                retry_resp
            } else {
                anyhow::bail!("upload returned {status}: {text}");
            }
        } else {
            upload_resp
        };

        let upload_result: serde_json::Value = upload_resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("upload parse failed: {e}"))?;

        upload_result
            .get("file_info")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("upload response missing file_info: {upload_result}")
            })
    }

    /// Send a C2C message via REST API (plain text, msg_type=0).
    /// Used for active messages (no msg_id) where markdown is not allowed.
    async fn send_c2c_text(
        &self,
        openid: &str,
        content: &str,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v2/users/{}/messages", API_BASE, openid);
        let body = self.build_text_body(content, msg_id, msg_seq);
        self.send_rest_with_retry(&url, &body).await
    }

    /// Send a group message via REST API (plain text, msg_type=0).
    /// Used for active messages (no msg_id) where markdown is not allowed.
    async fn send_group_text(
        &self,
        group_openid: &str,
        content: &str,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v2/groups/{}/messages", API_BASE, group_openid);
        let body = self.build_text_body(content, msg_id, msg_seq);
        self.send_rest_with_retry(&url, &body).await
    }

    /// Send a C2C message via REST API (markdown format).
    pub(super) async fn send_c2c_message(
        &self,
        openid: &str,
        content: &str,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v2/users/{}/messages", API_BASE, openid);
        let body = self.build_markdown_body(content, msg_id, msg_seq);
        self.send_rest_with_retry(&url, &body).await
    }

    /// Send a group message via REST API (markdown format).
    pub(super) async fn send_group_message(
        &self,
        group_openid: &str,
        content: &str,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v2/groups/{}/messages", API_BASE, group_openid);
        let body = self.build_markdown_body(content, msg_id, msg_seq);
        self.send_rest_with_retry(&url, &body).await
    }

    /// Start a typing keep-alive task for a C2C recipient.
    ///
    /// QQ Bot typing indicator (msg_type=6) expires after 60 seconds.
    /// The shared keep-alive loop refreshes it every 50 seconds until the
    /// task is aborted (typically when the response is sent). QQ Bot has no
    /// TTL cap or send-failure circuit breaker: the loop simply refreshes
    /// until aborted.
    pub(super) fn start_internal_typing(&self, recipient: &str) {
        let openid = match recipient.strip_prefix("c2c:") {
            Some(id) => id.to_string(),
            None => return, // 群聊 no-op
        };

        let http = self.http_client.clone();
        let token_mgr = self.token_manager.clone();

        self.typing.start(
            recipient,
            TypingParams::interval_only(Duration::from_secs(50)),
            move || {
                move || {
                    let http = http.clone();
                    let token_mgr = token_mgr.clone();
                    let url = format!("{}/v2/users/{}/messages", API_BASE, openid);
                    async move {
                        // 发 typing indicator
                        if let Ok(token) = token_mgr.get_token().await {
                            let body = serde_json::json!({
                                "msg_type": 6,
                                "input_notify": { "input_type": 1, "input_second": 60 },
                            });
                            let _ = http
                                .post(&url)
                                .header("Authorization", format!("QQBot {}", token))
                                .header("Content-Type", "application/json")
                                .header("User-Agent", user_agent())
                                .json(&body)
                                .send()
                                .await;
                        }
                        Ok(())
                    }
                }
            },
        );
    }

    /// Stop (abort) the typing keep-alive task for a recipient.
    pub(super) fn stop_internal_typing(&self, recipient: &str) {
        self.typing.stop(recipient);
    }

    /// Send a C2C message with an inline keyboard.
    pub(super) async fn send_c2c_keyboard(
        &self,
        openid: &str,
        content: &str,
        keyboard: &Keyboard,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let token = self.token_manager.get_token().await?;
        let url = format!("{}/v2/users/{}/messages", API_BASE, openid);

        let mut body = serde_json::json!({
            "content": "",
            "msg_type": 2,
            "markdown": {
                "content": content,
            },
            "keyboard": keyboard,
        });

        if !msg_id.is_empty() {
            body["msg_id"] = serde_json::Value::String(msg_id.to_string());
        }
        body["msg_seq"] = serde_json::Value::Number(msg_seq.into());

        let ua = user_agent();
        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", &ua)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("C2C keyboard send failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();

            // Token expired? Force-refresh and retry once.
            if status.as_u16() == 401 || text.contains("11244") {
                warn!(account = %self.account_id, status = %status, "C2C keyboard got token-expired error, refreshing and retrying");
                let new_token = self.token_manager.refresh().await?;
                let resp = self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("QQBot {}", new_token))
                    .header("Content-Type", "application/json")
                    .header("User-Agent", &ua)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("C2C keyboard retry failed: {}", e))?;

                if resp.status().is_success() {
                    debug!(
                        openid = openid,
                        "C2C keyboard message sent (after token refresh)"
                    );
                    return Ok(());
                }
                let retry_status = resp.status();
                let retry_text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "C2C keyboard retry returned {}: {}",
                    retry_status,
                    retry_text
                ));
            }

            return Err(anyhow::anyhow!(
                "C2C keyboard send returned {}: {}",
                status,
                text
            ));
        }

        debug!(openid = openid, "C2C keyboard message sent");
        Ok(())
    }

    /// Send a group message with an inline keyboard.
    pub(super) async fn send_group_keyboard(
        &self,
        group_openid: &str,
        content: &str,
        keyboard: &Keyboard,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let token = self.token_manager.get_token().await?;
        let url = format!("{}/v2/groups/{}/messages", API_BASE, group_openid);

        let mut body = serde_json::json!({
            "content": "",
            "msg_type": 2,
            "markdown": {
                "content": content,
            },
            "keyboard": keyboard,
        });

        if !msg_id.is_empty() {
            body["msg_id"] = serde_json::Value::String(msg_id.to_string());
        }
        body["msg_seq"] = serde_json::Value::Number(msg_seq.into());

        let ua = user_agent();
        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", &ua)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Group keyboard send failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();

            // Token expired? Force-refresh and retry once.
            if status.as_u16() == 401 || text.contains("11244") {
                warn!(account = %self.account_id, status = %status, "group keyboard got token-expired error, refreshing and retrying");
                let new_token = self.token_manager.refresh().await?;
                let resp = self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("QQBot {}", new_token))
                    .header("Content-Type", "application/json")
                    .header("User-Agent", &ua)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Group keyboard retry failed: {}", e))?;

                if resp.status().is_success() {
                    debug!(
                        group_openid = group_openid,
                        "group keyboard message sent (after token refresh)"
                    );
                    return Ok(());
                }
                let retry_status = resp.status();
                let retry_text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "Group keyboard retry returned {}: {}",
                    retry_status,
                    retry_text
                ));
            }

            return Err(anyhow::anyhow!(
                "Group keyboard send returned {}: {}",
                status,
                text
            ));
        }

        debug!(group_openid = group_openid, "group keyboard message sent");
        Ok(())
    }

    /// Acknowledge an interaction event (fire-and-forget).
    /// QQ Bot requires acknowledging within 3 seconds.
    fn ack_interaction(&self, event_id: &str) {
        let http = self.http_client.clone();
        let token_mgr = self.token_manager.clone();
        let event_id = event_id.to_string();

        tokio::spawn(async move {
            let token = match token_mgr.get_token().await {
                Ok(t) => t,
                Err(e) => {
                    warn!(err = %e, "failed to get token for interaction ACK");
                    return;
                }
            };

            let url = format!("{}/interactions/{}", API_BASE, event_id);
            let ua = user_agent();
            let body = serde_json::json!({ "code": 0 });

            match http
                .put(&url)
                .header("Authorization", format!("QQBot {}", token))
                .header("Content-Type", "application/json")
                .header("User-Agent", &ua)
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().as_u16() >= 400 => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    warn!(
                        event_id = %event_id,
                        status = %status,
                        body = %text,
                        "interaction ACK failed"
                    );
                }
                Ok(_) => {
                    debug!(event_id = %event_id, "interaction acknowledged");
                }
                Err(e) => {
                    warn!(event_id = %event_id, err = %e, "interaction ACK request failed");
                }
            }
        });
    }
}
