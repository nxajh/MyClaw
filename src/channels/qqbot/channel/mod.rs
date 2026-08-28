//! QQBotChannel struct + all impl blocks + Channel trait + WebSocket loop.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use super::flow::{DeliverDebouncer, RateLimiter, ReplyLimiter};
use super::keyboard::*;
use super::markdown_sanitize::sanitize_qq_markdown;
use super::message::split_message_chunk;
use super::token::TokenManager;
use super::types::*;
use crate::channels::message::{ChannelInboundMessage, ProcessingStatus};
use crate::channels::shared::TypingKeepAlive;
use crate::config::channel::QQBotAccountConfig;
use crate::{Channel, DedupState};

mod api;
mod protocol;
mod text;
mod ws;

#[cfg(test)]
mod tests;

pub use protocol::{TOKEN_URL, user_agent};

use self::api::MAX_INLINE_UPLOAD_BYTES;
use self::protocol::API_BASE;
use self::text::{QQ_MAX_VISUAL_LINES_PER_BUBBLE, split_by_visual_lines, strip_internal_tags};

/// Extract quoted message content from a QQ reply event payload.
///
/// When a user replies to a message (QQ `message_type` = 103), the inbound
/// event contains a `msg_elements` array where index `[0]` holds the quoted
/// original message. This function returns the quoted text, if present.
pub(super) fn extract_quote_content(data: &serde_json::Value) -> Option<String> {
    let has_reply = data.get("message_type").and_then(|v| v.as_u64()) == Some(103)
        || data.get("msg_elements").is_some();
    if !has_reply {
        return None;
    }
    let elements = data.get("msg_elements")?.as_array()?;
    let first = elements.first()?;
    let content = first.get("content").and_then(|v| v.as_str())?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Check if a URL points to a private/loopback address (SSRF protection).
pub(super) fn is_ssrf_blocked(url: &str) -> bool {
    // Parse host from URL
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    // Block obvious private ranges
    if host == "localhost" || host == "0.0.0.0" || host.is_empty() {
        return true;
    }

    // Check IP-based hosts
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            return true;
        }
        match ip {
            std::net::IpAddr::V4(v4) => {
                return v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast();
            }
            std::net::IpAddr::V6(v6) => {
                return v6.is_loopback() || v6.is_unspecified();
            }
        }
    }

    // Block .local, .internal, .localhost TLDs
    if host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".localhost")
    {
        return true;
    }

    false
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
    pub(super) typing: TypingKeepAlive,
    /// WebSocket session for Resume support.
    pub(super) session: Arc<Mutex<Option<SessionState>>>,
    /// Monotonic counter for proactive message msg_seq to avoid collisions.
    pub(super) msg_seq_counter: Arc<AtomicU32>,
    /// Startup instant for uptime reporting.
    pub(super) started_at: std::time::Instant,
    /// Per-group recent message history (group_openid → deque of (sender, content)).
    pub(super) group_history: Arc<Mutex<GroupHistory>>,
    /// Passive reply limiter (QQ allows ~4 replies per msg_id per hour).
    pub(super) reply_limiter: Arc<Mutex<ReplyLimiter>>,
    /// Per-sender + global rate limiter.
    pub(super) rate_limiter: Arc<Mutex<RateLimiter>>,
    /// Outbound debounce merge (disabled when window_ms == 0).
    pub(super) debouncer: Arc<DeliverDebouncer>,
}

/// Per-group message history: group_openid → VecDeque of (sender, content).
type GroupHistory = std::collections::HashMap<String, VecDeque<(String, String)>>;

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

    fn tts_enabled(&self) -> bool {
        self.config.tts
    }

    fn group_stats(&self) -> Vec<crate::channels::GroupStat> {
        let history = self.group_history.lock();
        history
            .iter()
            .map(|(gid, deque)| crate::channels::GroupStat {
                group_id: gid.chars().take(12).collect(),
                name: self
                    .config
                    .group_config
                    .get(gid)
                    .and_then(|c| c.name.clone())
                    .or_else(|| {
                        self.config
                            .group_config
                            .get("*")
                            .and_then(|c| c.name.clone())
                    }),
                buffered_messages: deque.len(),
                history_limit: self.resolve_group_history_limit(gid),
            })
            .collect()
    }

    fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        self.build_security_policy()
    }

    async fn send_message(
        &self,
        msg: &crate::channels::ChannelOutboundMessage,
    ) -> anyhow::Result<crate::channels::OutboundSendResult> {
        // ── Outbound debounce merge ──────────────────────────────────────────
        // When enabled and this is a text-only message (no files/buttons),
        // coalesce rapid sends to the same recipient within the window into a
        // single merged message (avoids message bombing).
        if self.debouncer.enabled()
            && msg.content.files.is_empty()
            && msg.content.buttons.is_empty()
            && !msg.content.text.trim().is_empty()
        {
            let raw_recipient = msg.receiver.id.clone();
            if raw_recipient.is_empty() {
                anyhow::bail!("QQBot send failed: no recipient");
            }
            let recipient = if raw_recipient.starts_with("c2c:")
                || raw_recipient.starts_with("group:")
            {
                raw_recipient
            } else {
                format!("c2c:{}", raw_recipient)
            };
            let raw_msg_id = msg
                .receiver
                .reply_to_message_id
                .as_deref()
                .unwrap_or("")
                .to_string();

            let (rx, is_first) =
                self.debouncer
                    .enqueue(&recipient, msg.content.text.clone(), &raw_msg_id);
            if is_first {
                let ch = self.clone();
                let recipient_clone = recipient.clone();
                let window = Duration::from_millis(ch.debouncer.window_ms);
                let separator = ch.debouncer.separator.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(window).await;
                    if let Some(entry) = ch.debouncer.take(&recipient_clone) {
                        let merged = entry.texts.join(&separator);
                        // Resolve passive-reply budget once for the merged message.
                        let msg_id = if !entry.msg_id.is_empty()
                            && ch.reply_limiter.lock().check_and_record(&entry.msg_id)
                        {
                            entry.msg_id.clone()
                        } else {
                            String::new()
                        };
                        let result = ch
                            .send_plain_text_chunked(
                                &recipient_clone,
                                &merged,
                                &msg_id,
                            )
                            .await;
                        ch.stop_internal_typing(&recipient_clone);
                        for waiter in entry.waiters {
                            let send_res = match &result {
                                Ok(r) => Ok(r.clone()),
                                Err(e) => Err(anyhow::anyhow!("{e:#}")),
                            };
                            let _ = waiter.send(send_res);
                        }
                    }
                });
            }
            return match rx.await {
                Ok(result) => result,
                Err(_) => anyhow::bail!("QQBot debounce flush channel closed unexpectedly"),
            };
        }

        // When files are present, text is used as caption on the first file,
        // not as a separate text message (RFC §14.5).
        // QQ msg_type=2: escape `$` and pad `**` for CJK before split.
        let chunks = if msg.content.files.is_empty() {
            let sanitized = sanitize_qq_markdown(&strip_internal_tags(&msg.content.text));
            // Pre-split by estimated visual lines to mitigate QQ client-side
            // layout bug, then apply character-limit splitting to each sub-text.
            let pre_chunks =
                split_by_visual_lines(&sanitized, QQ_MAX_VISUAL_LINES_PER_BUBBLE);
            let mut all = Vec::new();
            for pre in pre_chunks {
                all.append(&mut split_message_chunk(
                    &pre,
                    self.capabilities().message_chunk_limit,
                    self.capabilities().message_len_unit,
                ));
            }
            all
        } else {
            Vec::new()
        };
        // reply_to_message_id carries the original message event ID for passive replies.
        let raw_msg_id = msg.receiver.reply_to_message_id.as_deref().unwrap_or("");
        // Check passive reply limit (QQ allows ~4 passive replies per msg_id per hour).
        let can_reply_passive = if !raw_msg_id.is_empty() {
            self.reply_limiter.lock().check_and_record(raw_msg_id)
        } else {
            false // No msg_id = active message
        };
        let msg_id = if can_reply_passive { raw_msg_id } else { "" };

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
        let base_seq = self.next_msg_seq();
        for (i, chunk) in chunks.iter().enumerate() {
            // msg_seq must be unique per chunk. Use the monotonic counter so
            // repeated sends to the same msg_id are not deduplicated by QQ.
            let msg_seq = base_seq + i as u32;
            let is_last = i == count - 1;

            let result = if is_last {
                if let Some(kb) = &keyboard {
                    // Keyboard endpoint requires a passive msg_id. When
                    // absent (active message), fall back to plain markdown.
                    if !msg_id.is_empty() {
                        if let Some(openid) = recipient.strip_prefix("c2c:") {
                            self.send_c2c_keyboard(openid, chunk, kb, msg_id, msg_seq).await
                        } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                            self.send_group_keyboard(group_openid, chunk, kb, msg_id, msg_seq)
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

            // Try URL direct upload first: if the file has a source_url and it's
            // SSRF-safe, pass the URL directly to QQ's API. This avoids
            // downloading and base64-encoding large files locally.
            let mut file_info: Option<String> = None;
            if let Some(ref url) = file.meta.source_url {
                if !is_ssrf_blocked(url) {
                    let url_upload_body = serde_json::json!({
                        "file_type": file_type,
                        "srv_send_msg": false,
                        "url": url,
                        "file_name": file.meta.file_name,
                    });
                    match self.upload_file_to_qq(&files_url, &url_upload_body).await {
                        Ok(info) => {
                            debug!(url = %url, "QQ Bot file uploaded via URL direct upload");
                            file_info = Some(info);
                        }
                        Err(e) => {
                            warn!(
                                url = %url,
                                err = %e,
                                "QQ Bot URL direct upload failed, falling back to base64"
                            );
                        }
                    }
                }
            }

            // Fall back to base64 inline upload if URL upload was not attempted
            // or failed. Reads the file body, base64-encodes, and uploads.
            // Files exceeding the inline limit without a working URL upload bail.
            let file_info = if let Some(info) = file_info {
                info
            } else {
                let mut reader = file.body.open().await?;
                let mut data = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut data).await?;

                if data.len() > MAX_INLINE_UPLOAD_BYTES {
                    anyhow::bail!(
                        "QQ Bot file upload size {} exceeds {}B inline limit \
                         and URL direct upload unavailable or failed",
                        data.len(),
                        MAX_INLINE_UPLOAD_BYTES
                    );
                }

                use base64::Engine;
                let file_data = base64::engine::general_purpose::STANDARD.encode(&data);
                let upload_body = serde_json::json!({
                    "file_type": file_type,
                    "srv_send_msg": false,
                    "file_data": file_data,
                    "file_name": file.meta.file_name,
                });
                self.upload_file_to_qq(&files_url, &upload_body).await?
            };

            let msg_url = if is_group {
                format!("{}/v2/groups/{}/messages", API_BASE, openid)
            } else {
                format!("{}/v2/users/{}/messages", API_BASE, openid)
            };

            let caption_text = if idx == 0 && !msg.content.text.trim().is_empty() {
                strip_internal_tags(msg.content.text.as_str())
            } else {
                " ".to_string()
            };
            let mut msg_body = serde_json::json!({
                "content": caption_text,
                "msg_type": 7,
                "media": { "file_info": file_info },
            });
            if !msg_id.is_empty() {
                msg_body["msg_id"] = serde_json::json!(msg_id);
            } else if let Some(ref thread_id) = msg.receiver.thread_id {
                msg_body["msg_id"] = serde_json::json!(thread_id);
            }
            msg_body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());
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

    /// Override on_status to drive QQ typing indicators from orchestrator
    /// lifecycle events. Thinking → start typing keep-alive; Done/Error → stop.
    async fn on_status(&self, recipient: &str, status: ProcessingStatus) {
        match status {
            ProcessingStatus::Thinking => {
                self.start_internal_typing(recipient);
            }
            ProcessingStatus::Done | ProcessingStatus::Error => {
                self.stop_internal_typing(recipient);
            }
        }
    }

    /// Override on_tool_event to provide per-tool typing feedback.
    /// Tool execution starts a fresh typing pulse so the user sees activity
    /// during long-running tool calls. End is a no-op because QQ typing
    /// auto-expires; the keep-alive loop continues until `on_status(Done)`.
    async fn on_tool_event(&self, recipient: &str, event: crate::channels::ToolEvent) {
        match event {
            crate::channels::ToolEvent::Start { .. } => {
                self.start_internal_typing(recipient);
            }
            crate::channels::ToolEvent::End { .. } => {
                // Typing auto-expires; no explicit stop needed for QQ.
            }
        }
    }
}
