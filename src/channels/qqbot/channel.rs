//! QQBotChannel struct + all impl blocks + Channel trait + WebSocket loop.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use super::keyboard::*;
use super::markdown_sanitize::sanitize_qq_markdown_dollars;
use super::message::split_message_chunk;
use super::token::TokenManager;
use super::types::*;
use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};
use crate::config::channel::QQBotAccountConfig;
use crate::{Channel, DedupState};

// ── Constants ─────────────────────────────────────────────────────────────────

/// WebSocket gateway URL endpoint.
pub const GATEWAY_URL: &str = "https://api.sgroup.qq.com/gateway/bot";

/// Token endpoint.
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// REST API base for v2 messages.
pub const API_BASE: &str = "https://api.sgroup.qq.com";

/// WebSocket intents:
///   PUBLIC_GUILD_MESSAGES   = 1 << 30 = 1073741824
///   GROUP_AT_MESSAGE_CREATE = 1 << 25 = 33554432
///   C2C_MESSAGE_CREATE      = 1 << 25 = 33554432
///   DIRECT_MESSAGE          = 1 << 12 = 4096
///   INTERACTION             = 1 << 26 = 67108864
pub const INTENTS: u32 = (1 << 30) | (1 << 25) | (1 << 12) | (1 << 26);

/// WebSocket opcodes.
pub const OP_RESUME: u32 = 6;
pub const OP_HELLO: u32 = 10;
pub const OP_IDENTIFY: u32 = 2;
pub const OP_HEARTBEAT: u32 = 1;
pub const OP_HEARTBEAT_ACK: u32 = 11;
pub const OP_DISPATCH: u32 = 0;
pub const OP_RECONNECT: u32 = 7;
pub const OP_INVALID_SESSION: u32 = 9;

/// Reconnect delay schedule (seconds).
pub const RECONNECT_DELAYS: &[u64] = &[1, 2, 5, 10, 30, 60];
/// Maximum rapid reconnects before backing off.
pub const RAPID_RECONNECT_LIMIT: usize = 3;
pub const RAPID_RECONNECT_WINDOW_SECS: u64 = 5;

/// Derive a file extension from a QQ attachment content_type string.
/// QQ sends content_type as either a category ("image", "video") or a MIME
/// ("image/jpeg", "video/mp4"). We need the extension for temp file paths so
/// that downstream modality detection (which falls back to extension) works.
fn mime_ext_from_content_type(ct: &str) -> &'static str {
    match ct {
        "image" | "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "video" | "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        _ => "bin",
    }
}

/// Build User-Agent string for QQ Bot HTTP requests.
pub fn user_agent() -> String {
    let os = std::env::consts::OS;
    format!("MyClaw/{} (Rust; {})", env!("MYCLAW_VERSION"), os)
}

// ── QQ Bot Channel ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct QQBotChannel {
    pub(super) config: QQBotAccountConfig,
    pub(super) account_id: String,
    pub(super) token_manager: Arc<TokenManager>,
    pub(super) dedup: DedupState,
    /// Last sequence number for heartbeat.
    pub(super) last_seq: Arc<Mutex<Option<u64>>>,
    pub(super) http_client: reqwest::Client,
    /// Active typing keep-alive tasks, keyed by recipient (e.g. "c2c:xxx").
    pub(super) typing_tasks:
        Arc<Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// WebSocket session for Resume support.
    pub(super) session: Arc<Mutex<Option<SessionState>>>,
    /// Monotonic counter for proactive message msg_seq to avoid collisions.
    pub(super) msg_seq_counter: Arc<AtomicU32>,
}

impl QQBotChannel {
    pub fn new(account_id: String, config: QQBotAccountConfig) -> Self {
        let app_id = config.app_id.clone();
        let client_secret = config.client_secret.clone();

        let ch = Self {
            config,
            account_id: account_id.clone(),
            token_manager: Arc::new(TokenManager::new(account_id, app_id, client_secret)),
            dedup: DedupState::new(),
            last_seq: Arc::new(Mutex::new(None)),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            typing_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            session: Arc::new(Mutex::new(None)),
            msg_seq_counter: Arc::new(AtomicU32::new(1)),
        };
        crate::channels::warn_if_locked_down(&ch);
        ch
    }

    /// Return the next proactive msg_seq value (monotonically increasing).
    fn next_msg_seq(&self) -> u32 {
        self.msg_seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Build the unified security policy from QQBot config (RFC §14.5).
    fn build_security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
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
    async fn fetch_gateway_url(&self) -> anyhow::Result<String> {
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
    fn build_identify(&self, token: &str) -> String {
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
    fn handle_dispatch(
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
                let msg = match self.parse_group_message(data) {
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
                let group_id = msg.receiver.id.strip_prefix("group:").unwrap_or("");
                // GROUP_AT_MESSAGE_CREATE is by definition an @-mention, so
                // has_mention=true. Policy decides whether the group itself is allowed.
                if !apply_auth(
                    self,
                    &msg.sender.id,
                    crate::channels::MessageScope::Group {
                        id: group_id,
                        has_mention: true,
                    },
                ) {
                    return None;
                }
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

                // Determine sender and reply_target based on C2C vs group
                let author = data.get("author");
                let interaction_type = data.get("type").and_then(|v| v.as_u64()).unwrap_or(0);

                let (sender, reply_target) = if interaction_type == 2 {
                    // Group interaction
                    let member_openid = author
                        .and_then(|a| a.get("member_openid"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let group_openid = data
                        .get("group_openid")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    (member_openid.to_string(), format!("group:{}", group_openid))
                } else {
                    // C2C interaction (type 1)
                    let user_openid = author
                        .and_then(|a| a.get("user_openid"))
                        .and_then(|v| v.as_str())
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
        })
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
    /// 3. Fallback → download `url` (SILK; requires SILK-aware audio model).
    async fn ingest_voice_attachments(
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
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    warn!("qqbot: reading audio bytes failed for {full_url}: {e}");
                    continue;
                }
            };
            // If we downloaded from voice_wav_url the format is WAV; otherwise SILK.
            let mime = if wav_url.is_some() {
                "audio/wav".to_string()
            } else if ctype.contains('/') {
                ctype.to_string()
            } else {
                "audio/silk".to_string()
            };
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
                },
                body: std::sync::Arc::new(LocalFileBody::new(temp_path)),
            });
        }
    }

    /// Download image attachments from the raw inbound `data` payload,
    /// save to temp files, and store in `msg.files`.
    async fn ingest_image_attachments(
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
    async fn ingest_video_file_attachments(
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
            body["msg_seq"] = serde_json::Value::Number(msg_seq.into());
        } else {
            let seq = self.next_msg_seq();
            body["msg_seq"] = serde_json::Value::Number(seq.into());
        }
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
            body["msg_seq"] = serde_json::Value::Number(msg_seq.into());
        } else {
            let seq = self.next_msg_seq();
            body["msg_seq"] = serde_json::Value::Number(seq.into());
        }
        body
    }

    /// Send a REST message to QQ Bot with retry logic (token refresh + 429 backoff).
    /// `url` is the fully constructed API endpoint URL.
    /// `body` is the pre-built JSON body.
    async fn send_rest_with_retry(
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
    async fn send_c2c_message(
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
    async fn send_group_message(
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
    /// This method spawns a background task that refreshes it every 50 seconds
    /// until the task is aborted (typically when the response is sent).
    fn start_internal_typing(&self, recipient: &str) {
        let openid = match recipient.strip_prefix("c2c:") {
            Some(id) => id.to_string(),
            None => return, // 群聊 no-op
        };

        // Abort existing task for this recipient
        let mut tasks = self.typing_tasks.lock();
        if let Some(handle) = tasks.remove(recipient) {
            handle.abort();
        }

        let http = self.http_client.clone();
        let token_mgr = self.token_manager.clone();
        let recipient_key = recipient.to_string();

        let handle = tokio::spawn(async move {
            loop {
                // 发 typing indicator
                if let Ok(token) = token_mgr.get_token().await {
                    let url = format!("{}/v2/users/{}/messages", API_BASE, openid);
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
                tokio::time::sleep(std::time::Duration::from_secs(50)).await;
            }
        });
        tasks.insert(recipient_key, handle);
    }

    /// Stop (abort) the typing keep-alive task for a recipient.
    fn stop_internal_typing(&self, recipient: &str) {
        let mut tasks = self.typing_tasks.lock();
        if let Some(handle) = tasks.remove(recipient) {
            handle.abort();
        }
    }

    /// Send a C2C message with an inline keyboard.
    async fn send_c2c_keyboard(
        &self,
        openid: &str,
        content: &str,
        keyboard: &Keyboard,
        msg_id: &str,
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
            body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());
        } else {
            body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());
        }

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
    async fn send_group_keyboard(
        &self,
        group_openid: &str,
        content: &str,
        keyboard: &Keyboard,
        msg_id: &str,
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
            body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());
        } else {
            body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());
        }

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

    /// Try to handle a bot- prefixed slash command.
    /// Returns true if the command was handled (message consumed), false to continue dispatch.
    async fn try_bot_command(&self, content: &str, reply_target: &str, msg_id: &str) -> bool {
        let trimmed = content.trim();

        let reply = match trimmed {
            "/bot-ping" => "pong 🏓".to_string(),
            "/bot-version" => format!("MyClaw {}", env!("MYCLAW_VERSION")),
            "/bot-help" => {
                // C2C: send with keyboard buttons; group: fall through to cmd-input tags.
                if let Some(openid) = reply_target.strip_prefix("c2c:") {
                    let help_text = "**🤖 MyClaw Bot Commands**\n\n*Channel-level commands (handled locally)*\n• `/bot-ping` — Check bot latency\n• `/bot-version` — Show bot version\n• `/bot-help` — Show this help\n\n*Orchestrator commands (handled by AI)*\n• `/help` — Show AI commands\n• `/new` — New conversation\n• `/status` — Show status\n\nType any command or just chat!";
                    let kb = Keyboard::from_pairs(&[
                        ("/bot-ping", "/bot-ping"),
                        ("/bot-version", "/bot-version"),
                        ("/help", "/help"),
                        ("/new", "/new"),
                        ("/status", "/status"),
                    ]);
                    if self
                        .send_c2c_keyboard(openid, help_text, &kb, msg_id)
                        .await
                        .is_ok()
                    {
                        return true;
                    }
                    // Keyboard failed, fall through to text reply below.
                }
                // Group or keyboard fallback: use cmd-input tags.
                let help_text = r#"**🤖 MyClaw Bot Commands**

<qqbot-cmd-input text="/bot-ping" /> <qqbot-cmd-input text="/bot-version" /> <qqbot-cmd-input text="/bot-help" />

*Channel-level commands (handled locally)*
• `/bot-ping` — Check bot latency
• `/bot-version` — Show bot version
• `/bot-help` — Show this help

*Orchestrator commands (handled by AI)*
• `/help` — Show AI commands
• `/new` — New conversation
• `/status` — Show status

Type any command or just chat!"#;
                help_text.to_string()
            }
            _ => return false,
        };

        // Send reply directly via REST API (bypass orchestrator), with chunking.
        // Sanitize `$` before split (same as send_message path).
        let reply = sanitize_qq_markdown_dollars(&reply);
        let chunks = split_message_chunk(
            &reply,
            self.capabilities().message_chunk_limit,
            self.capabilities().message_len_unit,
        );
        if let Some(openid) = reply_target.strip_prefix("c2c:") {
            for (i, chunk) in chunks.iter().enumerate() {
                let seq = self.next_msg_seq() + i as u32;
                if let Err(e) = self.send_c2c_message(openid, chunk, msg_id, seq).await {
                    warn!(chunk = i, err = %e, "failed to send bot command reply chunk");
                    return true;
                }
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        } else if let Some(group_openid) = reply_target.strip_prefix("group:") {
            for (i, chunk) in chunks.iter().enumerate() {
                let seq = self.next_msg_seq() + i as u32;
                if let Err(e) = self
                    .send_group_message(group_openid, chunk, msg_id, seq)
                    .await
                {
                    warn!(chunk = i, err = %e, "failed to send bot command reply chunk");
                    return true;
                }
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }

        true
    }
}

// ── Channel trait implementation ──────────────────────────────────────────────

static QQBOT_CAPS: crate::channels::message::ChannelCapabilities =
    crate::channels::message::ChannelCapabilities::qqbot();

#[async_trait]
impl Channel for QQBotChannel {
    fn name(&self) -> &str {
        "qqbot"
    }

    fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities {
        &QQBOT_CAPS
    }

    fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        self.build_security_policy()
    }

    async fn send_message(
        &self,
        msg: &crate::channels::ChannelOutboundMessage,
    ) -> anyhow::Result<crate::channels::OutboundSendResult> {
        // When files are present, text is used as caption on the first file,
        // not as a separate text message (RFC §14.5).
        // QQ msg_type=2 treats bare `$` as formula; escape currency before split.
        let chunks = if msg.content.files.is_empty() {
            let sanitized = sanitize_qq_markdown_dollars(&msg.content.text);
            split_message_chunk(
                &sanitized,
                self.capabilities().message_chunk_limit,
                self.capabilities().message_len_unit,
            )
        } else {
            Vec::new()
        };
        // reply_to_message_id carries the original message event ID for passive replies.
        let msg_id = msg.receiver.reply_to_message_id.as_deref().unwrap_or("");

        // Normalize recipient: bare openids (from startup recovery fallback)
        // are treated as c2c: prefixed.
        let raw_recipient = msg.receiver.id.clone();
        if raw_recipient.is_empty() {
            anyhow::bail!("QQBot send failed: no recipient");
        }
        let recipient = if raw_recipient.starts_with("c2c:") || raw_recipient.starts_with("group:")
        {
            raw_recipient
        } else {
            format!("c2c:{}", raw_recipient)
        };

        // Build keyboard from inline_buttons (attached to last chunk only).
        let keyboard: Option<Keyboard> = if msg.content.buttons.is_empty() {
            None
        } else {
            let pairs: Vec<(String, String)> = msg
                .content
                .buttons
                .iter()
                .map(|b| (b.label.clone(), b.callback_data.clone()))
                .collect();
            Some(Keyboard::from_pairs(&pairs))
        };

        let count = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            // msg_seq must be unique per chunk for the same msg_id (1-based).
            let msg_seq = (i as u32) + 1;
            let is_last = i == count - 1;

            let result = if is_last {
                if let Some(kb) = &keyboard {
                    // Keyboard endpoint requires a passive msg_id. When
                    // absent (active message), fall back to plain markdown.
                    if !msg_id.is_empty() {
                        if let Some(openid) = recipient.strip_prefix("c2c:") {
                            self.send_c2c_keyboard(openid, chunk, kb, msg_id).await
                        } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                            self.send_group_keyboard(group_openid, chunk, kb, msg_id)
                                .await
                        } else {
                            Err(anyhow::anyhow!(
                                "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                                recipient
                            ))
                        }
                    } else {
                        // Active message + keyboard: degrade to markdown (msg_type=2).
                        if let Some(openid) = recipient.strip_prefix("c2c:") {
                            self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
                        } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                            self.send_group_message(group_openid, chunk, msg_id, msg_seq)
                                .await
                        } else {
                            Err(anyhow::anyhow!(
                                "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                                recipient
                            ))
                        }
                    }
                } else {
                    // No keyboard — markdown send (msg_type=2).
                    if let Some(openid) = recipient.strip_prefix("c2c:") {
                        self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
                    } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                        self.send_group_message(group_openid, chunk, msg_id, msg_seq)
                            .await
                    } else {
                        Err(anyhow::anyhow!(
                            "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                            recipient
                        ))
                    }
                }
            } else {
                // Non-last chunk — markdown send (msg_type=2).
                if let Some(openid) = recipient.strip_prefix("c2c:") {
                    self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
                } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                    self.send_group_message(group_openid, chunk, msg_id, msg_seq)
                        .await
                } else {
                    Err(anyhow::anyhow!(
                        "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                        recipient
                    ))
                }
            };

            if let Err(e) = result {
                error!(chunk = i, err = %e, "failed to send chunk");
                return Err(e);
            }

            // Throttle between chunks to avoid rate limiting.
            if i < count - 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        // Stop typing indicator for this recipient now that the response is sent.
        self.stop_internal_typing(&recipient);

        for (idx, file) in msg.content.files.iter().enumerate() {
            let mut reader = file.body.open().await?;
            let mut data = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut data).await?;

            // Determine file_type: 1=image, 2=video, 3=voice, 4=file
            let file_type = match crate::providers::media::modality_from_mime(
                file.meta.mime_type.as_deref(),
                &file.meta.file_name,
            ) {
                crate::providers::media::FileModality::Image => 1,
                crate::providers::media::FileModality::Video => 2,
                crate::providers::media::FileModality::Audio => 3,
                crate::providers::media::FileModality::Other => 4,
            };

            use base64::Engine;
            let file_data = base64::engine::general_purpose::STANDARD.encode(&data);

            let (openid, is_group) = {
                let raw = &msg.receiver.id;
                if let Some(oid) = raw.strip_prefix("c2c:") {
                    (oid.to_string(), false)
                } else if let Some(oid) = raw.strip_prefix("group:") {
                    (oid.to_string(), true)
                } else {
                    (raw.clone(), false)
                }
            };

            let files_url = if is_group {
                format!("{}/v2/groups/{}/files", API_BASE, openid)
            } else {
                format!("{}/v2/users/{}/files", API_BASE, openid)
            };

            let upload_body = serde_json::json!({
                "file_type": file_type,
                "srv_send_msg": false,
                "file_data": file_data,
                "file_name": file.meta.file_name,
            });

            let token = self.token_manager.get_token().await?;
            let ua = user_agent();

            // Upload with token-expired retry (same pattern as send_rest_with_retry).
            let upload_resp = self
                .http_client
                .post(&files_url)
                .header("Authorization", format!("QQBot {}", token))
                .header("Content-Type", "application/json")
                .header("User-Agent", &ua)
                .json(&upload_body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("upload failed: {e}"))?;

            let upload_resp = if !upload_resp.status().is_success() {
                let status = upload_resp.status();
                let text = upload_resp.text().await.unwrap_or_default();

                // Token expired? Force-refresh and retry once.
                if status.as_u16() == 401 || text.contains("11244") {
                    warn!(account = %self.account_id, status = %status, "file upload got token-expired error, refreshing and retrying");
                    let new_token = self.token_manager.refresh().await?;
                    let retry_resp = self
                        .http_client
                        .post(&files_url)
                        .header("Authorization", format!("QQBot {}", new_token))
                        .header("Content-Type", "application/json")
                        .header("User-Agent", &ua)
                        .json(&upload_body)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("upload retry failed: {e}"))?;
                    if !retry_resp.status().is_success() {
                        let s = retry_resp.status();
                        let t = retry_resp.text().await.unwrap_or_default();
                        return Err(anyhow::anyhow!("upload returned {s}: {t}"));
                    }
                    retry_resp
                } else {
                    return Err(anyhow::anyhow!("upload returned {status}: {text}"));
                }
            } else {
                upload_resp
            };

            let upload_result: serde_json::Value = upload_resp
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("upload parse failed: {e}"))?;
            let file_info = upload_result
                .get("file_info")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("upload response missing file_info: {upload_result}")
                })?;

            let msg_url = if is_group {
                format!("{}/v2/groups/{}/messages", API_BASE, openid)
            } else {
                format!("{}/v2/users/{}/messages", API_BASE, openid)
            };

            let caption_text = if idx == 0 && !msg.content.text.trim().is_empty() {
                msg.content.text.as_str()
            } else {
                " "
            };
            let mut msg_body = serde_json::json!({
                "content": caption_text,
                "msg_type": 7,
                "media": { "file_info": file_info },
            });
            if let Some(ref msg_id) = msg.receiver.reply_to_message_id {
                msg_body["msg_id"] = serde_json::json!(msg_id);
            } else if let Some(ref thread_id) = msg.receiver.thread_id {
                msg_body["msg_id"] = serde_json::json!(thread_id);
            }
            self.send_rest_with_retry(&msg_url, &msg_body).await?;
        }

        Ok(crate::channels::OutboundSendResult {
            message_ids: Vec::new(),
        })
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>> {
        // Start proactive background token refresh (OpenClaw-style).
        self.token_manager.start_background_refresh().await;

        let (tx, rx) = mpsc::channel::<ChannelInboundMessage>(256);

        let channel = self.clone();
        tokio::spawn(async move {
            channel.ws_loop(tx).await;
        });

        Ok(rx)
    }

    async fn health_check(&self) -> bool {
        // Try to fetch a token to verify credentials.
        self.token_manager.get_token().await.is_ok()
    }
}

// ── WebSocket loop ────────────────────────────────────────────────────────────

impl QQBotChannel {
    /// Main WebSocket loop with auto-reconnect and incremental delay.
    async fn ws_loop(&self, tx: mpsc::Sender<ChannelInboundMessage>) {
        let mut attempt = 0usize;
        let mut last_disconnect = std::time::Instant::now();

        loop {
            let result = self.ws_connect(&tx).await;
            let now = std::time::Instant::now();
            let rapid = now.duration_since(last_disconnect).as_secs() < RAPID_RECONNECT_WINDOW_SECS;
            last_disconnect = now;

            match result {
                Ok(WsDisconnect::TryResume) => {
                    // Resume-capable disconnect — try immediately with short delay
                    info!(account = %self.account_id, "QQ Bot WebSocket disconnected (resumable), reconnecting");
                    attempt = 0;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok(WsDisconnect::Clean) => {
                    warn!(account = %self.account_id, "QQ Bot WebSocket disconnected, reconnecting");
                    if rapid {
                        attempt += 1;
                    } else {
                        attempt = 0;
                    }
                    let delay = if attempt >= RAPID_RECONNECT_LIMIT {
                        Duration::from_secs(60)
                    } else {
                        Duration::from_secs(
                            RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)],
                        )
                    };
                    // Clean disconnect clears session
                    *self.session.lock() = None;
                    info!(account = %self.account_id, delay_secs = delay.as_secs(), attempt, "reconnecting");
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::TokenExpired) => {
                    warn!(account = %self.account_id, "QQ Bot token expired, forcing refresh before reconnect");
                    if let Err(e) = self.token_manager.refresh().await {
                        error!(account = %self.account_id, err = %e, "token refresh failed");
                    }
                    *self.session.lock() = None;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Ok(WsDisconnect::Fatal) => {
                    error!(account = %self.account_id, "QQ Bot WebSocket fatal disconnect, stopping reconnect");
                    return;
                }
                Err(e) => {
                    error!(account = %self.account_id, err = %e, "QQ Bot WebSocket error, reconnecting");
                    if rapid {
                        attempt += 1;
                    } else {
                        attempt = 0;
                    }
                    let delay = if attempt >= RAPID_RECONNECT_LIMIT {
                        Duration::from_secs(60)
                    } else {
                        Duration::from_secs(
                            RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)],
                        )
                    };
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Connect to the WebSocket gateway and handle the session.
    ///
    /// Uses `tokio::select!` to multiplex heartbeat sending and message reading
    /// in a single task, avoiding the need to clone `SplitSink`.
    async fn ws_connect(
        &self,
        tx: &mpsc::Sender<ChannelInboundMessage>,
    ) -> anyhow::Result<WsDisconnect> {
        // 1. Get gateway URL.
        let ws_url = self.fetch_gateway_url().await?;
        info!(account = %self.account_id, url = %ws_url, "connecting to QQ Bot WebSocket gateway");

        // 2. Connect.
        let (ws_stream, _response) = connect_async(&ws_url)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {}", e))?;

        info!(account = %self.account_id, "QQ Bot WebSocket connected");
        let (mut write, mut read) = ws_stream.split();

        // 3. Wait for Hello (OpCode 10).
        let hello_msg = read
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("WebSocket closed before Hello"))?
            .map_err(|e| anyhow::anyhow!("WebSocket read error on Hello: {}", e))?;

        let hello_text = match hello_msg {
            Message::Text(t) => t,
            _ => return Err(anyhow::anyhow!("expected text Hello message")),
        };

        let hello: GatewayPayload = serde_json::from_str(&hello_text)
            .map_err(|e| anyhow::anyhow!("Hello parse error: {}", e))?;

        if hello.op != OP_HELLO {
            return Err(anyhow::anyhow!(
                "expected OpCode 10 (Hello), got {}",
                hello.op
            ));
        }

        let heartbeat_interval: u64 = hello.d["heartbeat_interval"].as_u64().unwrap_or(41250);

        info!(account = %self.account_id, heartbeat_interval_ms = heartbeat_interval, "received Hello");

        // 4. Send Identify or Resume.
        let token = self.token_manager.get_token().await?;
        let session = self.session.lock().clone();
        let init_payload = match session {
            Some(ref s) => {
                info!(account = %self.account_id, session_id = %s.session_id, seq = s.last_seq, "sending Resume");
                serde_json::json!({
                    "op": OP_RESUME,
                    "d": {
                        "token": format!("QQBot {}", token),
                        "session_id": s.session_id,
                        "seq": s.last_seq,
                    }
                })
                .to_string()
            }
            None => self.build_identify(&token),
        };
        write
            .send(Message::Text(init_payload.into()))
            .await
            .map_err(|e| anyhow::anyhow!("Identify/Resume send failed: {}", e))?;

        info!(account = %self.account_id, "QQ Bot Identify/Resume sent");

        // 5. Main loop: select between heartbeat tick and incoming messages.
        let mut heartbeat_ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval));
        // Consume the first immediate tick.
        heartbeat_ticker.tick().await;

        loop {
            tokio::select! {
                // Heartbeat tick.
                _ = heartbeat_ticker.tick() => {
                    let seq = *self.last_seq.lock();
                    let payload = serde_json::json!({
                        "op": OP_HEARTBEAT,
                        "d": seq,
                    });
                    let text = serde_json::to_string(&payload).unwrap_or_default();
                    if let Err(e) = write.send(Message::Text(text.into())).await {
                        warn!(account = %self.account_id, err = %e, "heartbeat send failed, connection likely closed");
                        return Ok(WsDisconnect::TryResume);
                    }
                    debug!(account = %self.account_id, "heartbeat sent");
                }
                // Incoming WebSocket message.
                msg = read.next() => {
                    match msg {
                        Some(Ok(ws_msg)) => {
                            if let Some(disconnect) = self.handle_ws_message(ws_msg, tx).await {
                                return Ok(disconnect);
                            }
                        }
                        Some(Err(e)) => {
                            warn!(account = %self.account_id, err = %e, "WebSocket read error");
                            return Ok(WsDisconnect::Clean);
                        }
                        None => {
                            info!("WebSocket stream ended");
                            return Ok(WsDisconnect::Clean);
                        }
                    }
                }
            }
        }
    }

    /// Handle a single WebSocket message. Returns `Some(WsDisconnect)` if we should
    /// disconnect, `None` to continue processing.
    async fn handle_ws_message(
        &self,
        ws_msg: Message,
        tx: &mpsc::Sender<ChannelInboundMessage>,
    ) -> Option<WsDisconnect> {
        let text = match ws_msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(0u16);
                info!(account = %self.account_id, close_code = code, "WebSocket closed by server");
                return Some(match code {
                    // Token expired — refresh and reconnect
                    4004 => {
                        warn!(account = %self.account_id, "close 4004: token expired");
                        WsDisconnect::TokenExpired
                    }
                    // Session invalid — clear session, reconnect with Identify
                    4006 | 4007 | 4009 => {
                        warn!(account = %self.account_id, code, "close: session invalidated, clearing session");
                        *self.session.lock() = None;
                        *self.last_seq.lock() = None;
                        WsDisconnect::Clean
                    }
                    // Rate limited — reconnect normally (ws_loop handles delay via attempt counter)
                    4008 => {
                        warn!(account = %self.account_id, "close 4008: rate limited");
                        WsDisconnect::Clean
                    }
                    // Fatal — stop reconnecting
                    4914 | 4915 => {
                        error!(account = %self.account_id, code, "fatal close code");
                        WsDisconnect::Fatal
                    }
                    _ => WsDisconnect::Clean,
                });
            }
            Message::Ping(_) | Message::Pong(_) => return None,
            _ => return None,
        };

        let payload: GatewayPayload = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!(account = %self.account_id, err = %e, "failed to parse WebSocket payload");
                return None;
            }
        };

        // Update sequence number.
        if let Some(s) = payload.s {
            *self.last_seq.lock() = Some(s);
        }

        match payload.op {
            OP_DISPATCH => {
                if let Some(ref event_type) = payload.t {
                    // Internal events first
                    match event_type.as_str() {
                        "READY" => {
                            if let Some(session_id) =
                                payload.d.get("session_id").and_then(|v| v.as_str())
                            {
                                info!(
                                    account = %self.account_id,
                                    session_id = session_id,
                                    "READY received, session established"
                                );
                                *self.session.lock() = Some(SessionState {
                                    session_id: session_id.to_string(),
                                    last_seq: payload.s.unwrap_or(0),
                                });
                            }
                        }
                        "RESUMED" => {
                            info!(account = %self.account_id, "RESUMED received, session restored");
                        }
                        _ => {}
                    }
                    // User messages
                    if let Some(mut channel_msg) = self.handle_dispatch(event_type, &payload.d) {
                        // Bot- prefixed slash commands — intercept before orchestrator
                        let msg_id = payload.d.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if self
                            .try_bot_command(
                                &channel_msg.content.text,
                                &channel_msg.receiver.id,
                                msg_id,
                            )
                            .await
                        {
                            debug!(msg_id = %channel_msg.id, "bot command handled, skipping orchestrator");
                            return None;
                        }

                        // Voice/audio: use QQ's native ASR text when present, else
                        // attach downloaded bytes for the auxiliary STT model.
                        self.ingest_voice_attachments(&payload.d, &mut channel_msg)
                            .await;

                        // Image: download and save to temp file so vision models
                        // can see the image without relying on proxy URL fetching.
                        self.ingest_image_attachments(&payload.d, &mut channel_msg)
                            .await;

                        // Video / file: download and save to temp file.
                        self.ingest_video_file_attachments(&payload.d, &mut channel_msg)
                            .await;

                        if tx.send(channel_msg.clone()).await.is_err() {
                            warn!(account = %self.account_id, "channel receiver dropped, stopping listen");
                            return Some(WsDisconnect::Clean);
                        }
                        // Start typing keep-alive for C2C messages.
                        self.start_internal_typing(&channel_msg.receiver.id);
                    }
                }
            }
            OP_HEARTBEAT_ACK => {
                debug!(account = %self.account_id, "heartbeat ACK received");
            }
            OP_RECONNECT => {
                warn!(account = %self.account_id, "server requested reconnect");
                return Some(WsDisconnect::TryResume);
            }
            OP_INVALID_SESSION => {
                warn!(account = %self.account_id, "invalid session (OpCode 9), clearing session for fresh identify");
                *self.last_seq.lock() = None;
                *self.session.lock() = None;
                return Some(WsDisconnect::Clean);
            }
            _ => {
                debug!(op = payload.op, "unknown opcode");
            }
        }

        None
    }
}
