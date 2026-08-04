//! QQBotChannel struct + all impl blocks + Channel trait + WebSocket loop.

#![allow(dead_code)]

use std::collections::VecDeque;
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
use super::markdown_sanitize::sanitize_qq_markdown;
use super::message::split_message_chunk;
use super::token::TokenManager;
use super::types::*;
use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender, ProcessingStatus,
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

/// Estimated characters per visual line in QQ mobile markdown (~19-20 CJK).
const QQ_CHARS_PER_LINE: f64 = 20.0;

/// Maximum estimated visual lines per QQ message bubble.
///
/// QQ's markdown renderer corrupts spacing at ~35-40 visual lines (tested
/// empirically with short/medium/long paragraphs). 30 provides a safe margin.
const QQ_MAX_VISUAL_LINES_PER_BUBBLE: usize = 30;

/// Maximum file size for inline base64 upload (10 MB).
///
/// QQ Bot inline file upload (msg_type=7) requires base64-encoding the entire
/// payload, so large files risk memory exhaustion. Files with a `source_url`
/// bypass this limit via URL direct upload; only files without a valid URL
/// that exceed this limit will bail.
const MAX_INLINE_UPLOAD_BYTES: usize = 10_485_760;

/// Estimate display width: CJK = 1.0, ASCII = 0.5.
fn display_width(s: &str) -> f64 {
    s.chars()
        .map(|c| if c.is_ascii() { 0.5 } else { 1.0 })
        .sum()
}

/// Estimate visual lines a text block occupies on QQ mobile.
fn estimate_visual_lines(text: &str) -> usize {
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                1
            } else {
                let w = display_width(line);
                ((w / QQ_CHARS_PER_LINE).ceil() as usize).max(1)
            }
        })
        .sum()
}

/// Extract quoted message content from a QQ reply event payload.
///
/// When a user replies to a message (QQ `message_type` = 103), the inbound
/// event contains a `msg_elements` array where index `[0]` holds the quoted
/// original message. This function returns the quoted text, if present.
fn extract_quote_content(data: &serde_json::Value) -> Option<String> {
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

/// Pre-split text so each bubble stays under `max_lines` estimated visual lines.
///
/// Splits land on `\n\n` boundaries (never inside fenced code blocks). Each
/// part's visual-line cost is estimated from CJK/ASCII display widths.
/// GFM table rows are kept together: if the current buffer ends with a table
/// line and the next part starts with one, the split is deferred so the table
/// is not broken across bubbles.
fn split_by_visual_lines(text: &str, max_lines: usize) -> Vec<String> {
    let parts: Vec<&str> = text.split("\n\n").collect();

    // Pre-compute each part's visual-line cost.
    // Non-first parts include a 1-line gap (the \n\n separator).
    let costs: Vec<usize> = parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let gap = if i == 0 { 0 } else { 1 };
            gap + estimate_visual_lines(part)
        })
        .collect();

    let total: usize = costs.iter().sum();
    if total <= max_lines {
        return vec![text.to_string()];
    }

    // Accumulate parts, splitting when cumulative cost exceeds max_lines.
    // Never split inside a fenced code block, and never split between
    // adjacent GFM table rows that happen to be separated by a blank line.
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_cost = 0usize;
    let mut in_code = false;

    for (i, part) in parts.iter().enumerate() {
        let started_in_code = in_code;
        for line in part.lines() {
            if is_fence_line(line) {
                in_code = !in_code;
            }
        }

        // Detect a GFM table continuation: current ends with a table row and
        // the incoming part starts with one. A blank line inside a table
        // (produced by some generators) should not cause a mid-table split.
        let breaks_table = !current.is_empty()
            && current.lines().last().is_some_and(is_gfm_table_line)
            && part.lines().next().is_some_and(is_gfm_table_line);

        // Split before this part if it would overflow and we're outside code,
        // unless doing so would break a table continuation.
        if !current.is_empty()
            && !started_in_code
            && !breaks_table
            && current_cost + costs[i] > max_lines
        {
            chunks.push(current.trim_end().to_string());
            current.clear();
            current_cost = 0;
        }

        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(part);
        current_cost += costs[i];
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_string());
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

/// Check if a line is part of a GFM (GitHub Flavored Markdown) table.
///
/// A pipe-table row starts with `|` (the most common form). Separator rows
/// without a leading pipe (e.g. `---|:---:|---`) are also recognised.
fn is_gfm_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Pipe table row: | col1 | col2 |  (trailing pipe optional)
    if trimmed.starts_with('|') {
        return true;
    }
    // Separator-only row without leading pipe: --- | :---: | ---
    let mut has_dash = false;
    for c in trimmed.chars() {
        match c {
            '-' => has_dash = true,
            '|' | ':' | ' ' | '\t' => {}
            _ => return false,
        }
    }
    has_dash
}

/// Check if a line is a markdown fence (``` or ~~~).
fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Check if a URL points to a private/loopback address (SSRF protection).
fn is_ssrf_blocked(url: &str) -> bool {
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

/// Strip model reasoning/thinking content and framework scaffolding tags.
fn strip_internal_tags(text: &str) -> String {
    let patterns: &[(&str, &str)] = &[
        // XML-style thinking tags
        ("<thinking>", "</thinking>"),
        ("<think>", "</think>"),
        ("<system-reminder>", "</system-reminder>"),
        ("<previous_response>", "</previous_response>"),
    ];

    let mut result = text.to_string();
    for (open, close) in patterns {
        // Remove complete blocks
        while let Some(start) = result.find(open) {
            if let Some(end) = result[start..].find(close) {
                let end_abs = start + end + close.len();
                result.replace_range(start..end_abs, "");
            } else {
                // Unclosed tag — remove from start to end of string
                result.replace_range(start.., "");
                break;
            }
        }
        // Remove standalone tags
        result = result.replace(open, "").replace(close, "");
    }

    // Deepseek format: `think`...`/think`
    while let Some(start) = result.find("`think`") {
        if let Some(end) = result[start..].find("`/think`") {
            let end_abs = start + end + "`/think`".len();
            result.replace_range(start..end_abs, "");
        } else {
            break;
        }
    }

    result.trim().to_string()
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
    /// Startup instant for uptime reporting.
    pub(super) started_at: std::time::Instant,
    /// Per-group recent message history (group_openid → deque of (sender, content)).
    pub(super) group_history: Arc<Mutex<GroupHistory>>,
    /// Passive reply limiter (QQ allows ~4 replies per msg_id per hour).
    pub(super) reply_limiter: Arc<Mutex<ReplyLimiter>>,
    /// Outbound debounce merge (disabled when window_ms == 0).
    pub(super) debouncer: Arc<DeliverDebouncer>,
}

/// Per-group message history: group_openid → VecDeque of (sender, content).
type GroupHistory = std::collections::HashMap<String, VecDeque<(String, String)>>;

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
            typing_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            session: Arc::new(Mutex::new(None)),
            msg_seq_counter: Arc::new(AtomicU32::new(1)),
            started_at: std::time::Instant::now(),
            group_history: Arc::new(Mutex::new(std::collections::HashMap::new())),
            reply_limiter: Arc::new(Mutex::new(ReplyLimiter::new())),
            debouncer: Arc::new(DeliverDebouncer::new(
                debounce_window_ms,
                debounce_separator,
            )),
        };
        crate::channels::warn_if_locked_down(&ch);
        ch
    }

    /// Return the next proactive msg_seq value (monotonically increasing).
    fn next_msg_seq(&self) -> u32 {
        self.msg_seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Send plain text (no keyboard, no files) to a recipient, chunked.
    /// Used by the debounce flush path. Returns an empty-id result on success.
    async fn send_plain_text_chunked(
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

    /// Resolve the per-group history limit from config.
    ///
    /// Checks for an explicit entry for `group_openid`, then falls back to the
    /// wildcard `"*"` entry, and finally to the default of 20.
    fn resolve_group_history_limit(&self, group_openid: &str) -> usize {
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
    fn record_group_history(&self, group_openid: &str, sender: &str, content: &str) {
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
    fn inject_group_history(&self, msg: &mut ChannelInboundMessage, group_openid: &str) {
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

    /// Upload a file to the QQ Bot `/files` endpoint and return the `file_info`
    /// string needed for the subsequent message send.
    ///
    /// Handles token-expired (401 / code 11244) by refreshing and retrying once.
    /// `files_url` is the fully constructed `/v2/groups/{id}/files` or
    /// `/v2/users/{id}/files` endpoint. `upload_body` is the pre-built JSON
    /// payload (either `file_data` base64 or `url` direct upload).
    async fn upload_file_to_qq(
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
        }
        body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());

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
        }
        body["msg_seq"] = serde_json::Value::Number(self.next_msg_seq().into());

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

    fn group_stats(&self) -> Vec<crate::channels::GroupStat> {
        let history = self.group_history.lock();
        history
            .iter()
            .map(|(gid, deque)| crate::channels::GroupStat {
                group_id: gid.clone(),
                name: None,
                buffered_messages: deque.len(),
                history_limit: 20,
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
        for (i, chunk) in chunks.iter().enumerate() {
            // msg_seq must be unique and monotonic globally across the session.
            let msg_seq = self.next_msg_seq();
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

// ── Reconnect state machine ──────────────────────────────────────────────────

/// Manages WebSocket reconnection backoff state, extracted from `ws_loop`
/// for testability.
///
/// Tracks the number of consecutive rapid disconnects and computes the
/// appropriate sleep duration before the next attempt based on the disconnect
/// type.
struct ReconnectManager {
    attempt: usize,
    last_disconnect: std::time::Instant,
}

impl ReconnectManager {
    fn new() -> Self {
        Self {
            attempt: 0,
            last_disconnect: std::time::Instant::now(),
        }
    }

    /// Returns the delay before the next reconnect attempt based on disconnect
    /// type.
    ///
    /// - `TryResume`: resets attempt counter, returns 1 s (fast retry).
    /// - `Clean`: increments attempt on rapid disconnect, uses indexed backoff
    ///   schedule (caps at 60 s after `RAPID_RECONNECT_LIMIT` rapid failures).
    /// - `TokenExpired`: returns 3 s.
    /// - `Fatal`: returns 0 s (caller stops reconnecting).
    fn next_delay(&mut self, disconnect: &WsDisconnect) -> Duration {
        let now = std::time::Instant::now();
        let rapid =
            now.duration_since(self.last_disconnect).as_secs() < RAPID_RECONNECT_WINDOW_SECS;
        self.last_disconnect = now;

        match disconnect {
            WsDisconnect::TryResume => {
                self.attempt = 0;
                Duration::from_secs(1)
            }
            WsDisconnect::Clean => {
                if rapid {
                    self.attempt += 1;
                } else {
                    self.attempt = 0;
                }
                if self.attempt >= RAPID_RECONNECT_LIMIT {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(
                        RECONNECT_DELAYS[self.attempt.min(RECONNECT_DELAYS.len() - 1)],
                    )
                }
            }
            WsDisconnect::TokenExpired => Duration::from_secs(3),
            WsDisconnect::Fatal => Duration::from_secs(0),
        }
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }
}

// ── Reply limiter (passive reply cap) ─────────────────────────────────────────

/// Tracks passive reply counts per message_id to avoid hitting QQ's limit
/// (~4 replies/msg_id within 1 hour).
pub(super) struct ReplyLimiter {
    /// msg_id → (reply_count, first_seen_ms)
    entries: std::collections::HashMap<String, (u32, u128)>,
    /// Max replies per msg_id
    limit: u32,
    /// TTL in ms (1 hour)
    ttl_ms: u128,
}

impl ReplyLimiter {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            limit: 4,
            ttl_ms: 3_600_000,
        }
    }

    /// Check if we can still reply passively to this msg_id.
    /// Returns false when limit exceeded or TTL expired.
    fn check_and_record(&mut self, msg_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Periodic cleanup: remove expired entries
        self.entries
            .retain(|_, (_, first_seen)| now - *first_seen < self.ttl_ms);

        match self.entries.get_mut(msg_id) {
            Some((count, _)) => {
                if *count >= self.limit {
                    return false;
                }
                *count += 1;
                true
            }
            None => {
                // LRU eviction if too many entries
                if self.entries.len() > 10_000 {
                    if let Some(oldest_key) = self
                        .entries
                        .iter()
                        .min_by_key(|(_, (_, first))| *first)
                        .map(|(k, _)| k.clone())
                    {
                        self.entries.remove(&oldest_key);
                    }
                }
                self.entries.insert(msg_id.to_string(), (1, now));
                true
            }
        }
    }
}


// ── Deliver debouncer ─────────────────────────────────────────────────────────

/// A pending debounced delivery for one recipient.
struct PendingDeliver {
    texts: Vec<String>,
    msg_id: String,
    waiters: Vec<
        tokio::sync::oneshot::Sender<anyhow::Result<crate::channels::OutboundSendResult>>,
    >,
}

/// Outbound debounce merge: coalesces rapid text-only sends to the same
/// recipient within a window into a single message (avoids message bombing).
///
/// Modelled after the official plugin's `DeliverDebouncer`. The first send to a
/// recipient within an idle window drives a flush task; subsequent sends within
/// the window append to the buffer and await the shared flush result.
pub(super) struct DeliverDebouncer {
    window_ms: u64,
    separator: String,
    pending: parking_lot::Mutex<std::collections::HashMap<String, PendingDeliver>>,
}

impl DeliverDebouncer {
    fn new(window_ms: u64, separator: String) -> Self {
        Self {
            window_ms,
            separator,
            pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn enabled(&self) -> bool {
        self.window_ms > 0
    }

    /// Buffer a text. Returns a receiver for the eventual send result and
    /// whether this caller is the first in the window (and must drive the flush).
    fn enqueue(
        &self,
        recipient: &str,
        text: String,
        msg_id: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<anyhow::Result<crate::channels::OutboundSendResult>>,
        bool,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut pending = self.pending.lock();
        let is_first = !pending.contains_key(recipient);
        let entry = pending
            .entry(recipient.to_string())
            .or_insert_with(|| PendingDeliver {
                texts: Vec::new(),
                msg_id: String::new(),
                waiters: Vec::new(),
            });
        entry.texts.push(text);
        if !msg_id.is_empty() {
            entry.msg_id = msg_id.to_string();
        }
        entry.waiters.push(tx);
        (rx, is_first)
    }

    /// Remove and return the pending entry for a recipient (flush driver only).
    fn take(&self, recipient: &str) -> Option<PendingDeliver> {
        self.pending.lock().remove(recipient)
    }
}

// ── WebSocket loop ────────────────────────────────────────────────────────────

impl QQBotChannel {
    /// Main WebSocket loop with auto-reconnect and incremental delay.
    ///
    /// Reconnection state (attempt counter, rapid-disconnect window) is
    /// delegated to [`ReconnectManager`].
    async fn ws_loop(&self, tx: mpsc::Sender<ChannelInboundMessage>) {
        let mut mgr = ReconnectManager::new();

        loop {
            let result = self.ws_connect(&tx).await;

            match result {
                Ok(WsDisconnect::TryResume) => {
                    info!(account = %self.account_id, "QQ Bot WebSocket disconnected (resumable), reconnecting");
                    let delay = mgr.next_delay(&WsDisconnect::TryResume);
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::Clean) => {
                    warn!(account = %self.account_id, "QQ Bot WebSocket disconnected, reconnecting");
                    let delay = mgr.next_delay(&WsDisconnect::Clean);
                    // Clean disconnect clears session
                    *self.session.lock() = None;
                    info!(account = %self.account_id, delay_secs = delay.as_secs(), "reconnecting");
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::TokenExpired) => {
                    warn!(account = %self.account_id, "QQ Bot token expired, forcing refresh before reconnect");
                    if let Err(e) = self.token_manager.refresh().await {
                        error!(account = %self.account_id, err = %e, "token refresh failed");
                    }
                    *self.session.lock() = None;
                    let delay = mgr.next_delay(&WsDisconnect::TokenExpired);
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::Fatal) => {
                    error!(account = %self.account_id, "QQ Bot WebSocket fatal disconnect, stopping reconnect");
                    return;
                }
                Err(e) => {
                    error!(account = %self.account_id, err = %e, "QQ Bot WebSocket error, reconnecting");
                    let delay = mgr.next_delay(&WsDisconnect::Clean);
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
                    // Feature 2: Record group history for ALL group events
                    // (before auth/filtering) so even rejected messages are
                    // captured as context for future @-mentions.
                    if event_type.contains("GROUP") {
                        if let (Some(group_openid), Some(sender), Some(content)) = (
                            payload.d.get("group_openid").and_then(|v| v.as_str()),
                            payload
                                .d
                                .get("author")
                                .and_then(|a| a.get("member_openid"))
                                .and_then(|v| v.as_str()),
                            payload
                                .d
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(str::trim),
                        ) {
                            self.record_group_history(group_openid, sender, content);
                        }
                    }
                    // User messages
                    if let Some(mut channel_msg) = self.handle_dispatch(event_type, &payload.d) {
                        // Feature 1: Quote message resolution — when the user
                        // replied to a message (message_type=103), prepend the
                        // quoted content so the model has full context.
                        if let Some(quoted) = extract_quote_content(&payload.d) {
                            channel_msg.content.text = format!(
                                "[Quoted message begins]\n{}\n[Quoted message ends]\n[Current message]\n{}",
                                quoted, channel_msg.content.text
                            );
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

                        match tx.try_send(channel_msg.clone()) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                error!(
                                    account = %self.account_id,
                                    "QQBot inbound queue full, dropping message"
                                );
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                warn!(account = %self.account_id, "channel receiver dropped, stopping listen");
                                return Some(WsDisconnect::Clean);
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short_text_no_split() {
        let text = "para1\n\npara2\n\npara3";
        assert_eq!(split_by_visual_lines(text, 30), vec![text]);
    }

    #[test]
    fn split_long_short_paragraphs() {
        // 25 short CJK paragraphs → each ~2 visual lines + gap = ~75 total
        let paras: Vec<String> = (1..=25)
            .map(|i| format!("段落{i:02}: 这是一段测试文字。"))
            .collect();
        let text = paras.join("\n\n");
        let chunks = split_by_visual_lines(&text, 30);
        assert!(chunks.len() > 1);
        // No chunk should exceed ~30 visual lines
        for chunk in &chunks {
            let lines = estimate_visual_lines(chunk);
            assert!(lines <= 35, "chunk has {lines} visual lines (>35)");
        }
    }

    #[test]
    fn split_long_paragraphs_fewer_per_bubble() {
        // 10 long CJK paragraphs → each ~5 visual lines + gap = ~59 total
        let paras: Vec<String> = (1..=10)
            .map(|i| format!("段落{i}: 这是一段很长的测试文字，每段约一百个字符左右，用于测试渲染器对长段落的处理能力和行数估算的准确性。第{i}段结束。"))
            .collect();
        let text = paras.join("\n\n");
        let chunks = split_by_visual_lines(&text, 30);
        assert!(chunks.len() > 1, "should split: 10 long paras");
    }

    #[test]
    fn split_never_inside_code_block() {
        let mut paras = Vec::new();
        for i in 1..=10 {
            paras.push(format!("para{i}"));
        }
        paras.push(
            "```\ncode line 1\n\ncode line 2\n\n```\n\nafter code".to_string(),
        );
        for i in 11..=25 {
            paras.push(format!("para{i}"));
        }
        let text = paras.join("\n\n");
        let chunks = split_by_visual_lines(&text, 30);
        for chunk in &chunks {
            let fence_count = chunk.matches("```").count();
            assert_eq!(
                fence_count % 2,
                0,
                "code fence split across chunks: {fence_count} occurrences"
            );
        }
    }

    #[test]
    fn split_code_block_not_split_across_chunks() {
        let mut paras = Vec::new();
        paras.push("intro text".to_string());
        for i in 1..=18 {
            paras.push(format!("paragraph {i}"));
        }
        paras.push(
            "```\ntitle inside code\n\ncontent line 1\n\ncontent line 2\n```".to_string(),
        );
        for i in 19..=25 {
            paras.push(format!("trailing paragraph {i}"));
        }
        let text = paras.join("\n\n");
        let chunks = split_by_visual_lines(&text, 30);
        for chunk in &chunks {
            let opens = chunk.matches("```").count();
            assert_eq!(
                opens % 2,
                0,
                "fences must be balanced: found {opens} in chunk"
            );
        }
    }

    #[test]
    fn display_width_cjk_vs_ascii() {
        // 10 CJK chars = 10.0
        assert_eq!(display_width("这是一段中文测试文字"), 10.0);
        // 20 ASCII chars = 10.0
        assert_eq!(display_width("abcdefghijklmnopqrst"), 10.0);
        // Mixed
        assert_eq!(display_width("中文ab"), 1.0 + 1.0 + 0.5 + 0.5);
    }

    #[test]
    fn reconnect_manager_try_resume_resets_attempt() {
        let mut mgr = ReconnectManager::new();
        // Simulate a few rapid Clean disconnects to ramp up attempt.
        mgr.attempt = 2;
        let delay = mgr.next_delay(&WsDisconnect::TryResume);
        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(mgr.attempt, 0);
    }

    #[test]
    fn reconnect_manager_clean_first_rapid() {
        let mut mgr = ReconnectManager::new();
        // Immediately after construction → rapid.
        let delay = mgr.next_delay(&WsDisconnect::Clean);
        assert_eq!(delay, Duration::from_secs(RECONNECT_DELAYS[1]));
        assert_eq!(mgr.attempt, 1);
    }

    #[test]
    fn reconnect_manager_caps_at_60s() {
        let mut mgr = ReconnectManager::new();
        // Simulate RAPID_RECONNECT_LIMIT rapid disconnects.
        for _ in 0..RAPID_RECONNECT_LIMIT {
            mgr.next_delay(&WsDisconnect::Clean);
        }
        let delay = mgr.next_delay(&WsDisconnect::Clean);
        assert_eq!(delay, Duration::from_secs(60));
    }

    #[test]
    fn reconnect_manager_token_expired_fixed_delay() {
        let mut mgr = ReconnectManager::new();
        let delay = mgr.next_delay(&WsDisconnect::TokenExpired);
        assert_eq!(delay, Duration::from_secs(3));
    }

    #[test]
    fn reconnect_manager_reset() {
        let mut mgr = ReconnectManager::new();
        mgr.attempt = 5;
        mgr.reset();
        assert_eq!(mgr.attempt, 0);
    }

    // ── Feature 1: Quote message extraction ──────────────────────────────────

    #[test]
    fn quote_message_extraction() {
        // Reply event with message_type=103 and msg_elements
        let data = serde_json::json!({
            "id": "MSG123",
            "message_type": 103,
            "content": "that's interesting",
            "author": { "member_openid": "user_a" },
            "group_openid": "group_1",
            "msg_elements": [
                { "content": "original message text", "elem_type": 1 }
            ]
        });
        let quoted = extract_quote_content(&data);
        assert_eq!(quoted.as_deref(), Some("original message text"));

        // Normal message (no quote) — should return None
        let normal = serde_json::json!({
            "id": "MSG456",
            "content": "hello world",
            "author": { "member_openid": "user_b" },
            "group_openid": "group_1"
        });
        assert!(extract_quote_content(&normal).is_none());

        // msg_elements present but empty array
        let empty_elements = serde_json::json!({
            "id": "MSG789",
            "msg_elements": []
        });
        assert!(extract_quote_content(&empty_elements).is_none());

        // msg_elements present but content is empty string
        let empty_content = serde_json::json!({
            "message_type": 103,
            "msg_elements": [{ "content": "   " }]
        });
        assert!(extract_quote_content(&empty_content).is_none());
    }

    // ── Feature 2: Group history ──────────────────────────────────────────────

    /// Build a minimal test channel with the given group config.
    fn test_channel(group_config: Vec<(&str, Option<usize>)>) -> QQBotChannel {
        use crate::config::channel::{GroupConfig, QQBotAccountConfig};
        let mut gc = std::collections::HashMap::new();
        for (gid, limit) in group_config {
            gc.insert(
                gid.to_string(),
                GroupConfig {
                    history_limit: limit,
                    ..Default::default()
                },
            );
        }
        let config = QQBotAccountConfig {
            enabled: true,
            app_id: "test_app".to_string(),
            client_secret: "test_secret".to_string(),
            allowed_users: None,
            allowed_groups: Some(vec!["*".to_string()]),
            group_config: gc,
            debounce_window_ms: 0,
            debounce_separator: "\n\n---\n\n".to_string(),
        };
        QQBotChannel::new("test".to_string(), config)
    }

    #[test]
    fn group_history_capped_at_limit() {
        let ch = test_channel(vec![("*", Some(3))]);
        // Insert 5 messages; limit is 3 → only last 3 should remain.
        for i in 0..5 {
            ch.record_group_history("grp1", &format!("user{i}"), &format!("msg{i}"));
        }
        let history = ch.group_history.lock();
        let entries = history.get("grp1").unwrap();
        assert_eq!(entries.len(), 3, "history should be capped at 3");
        // First two entries (msg0, msg1) should have been evicted.
        assert_eq!(entries[0].1, "msg2");
        assert_eq!(entries[1].1, "msg3");
        assert_eq!(entries[2].1, "msg4");
    }

    #[test]
    fn group_history_inject_format() {
        let ch = test_channel(vec![("*", Some(20))]);
        // Record some prior messages.
        ch.record_group_history("grp1", "alice", "hello");
        ch.record_group_history("grp1", "bob", "how are you?");
        // Record the current message.
        ch.record_group_history("grp1", "carol", "@bot what's up");

        let mut msg = ChannelInboundMessage {
            id: "m1".to_string(),
            sender: MessageSender::new("carol".to_string()),
            receiver: MessageReceiver::new("group:grp1".to_string()),
            content: ChannelMessageContent::text("@bot what's up".to_string()),
            timestamp: 0,
            interruption_scope_id: None,
        };
        ch.inject_group_history(&mut msg, "grp1");

        // The injected text should contain the two prior messages but NOT the
        // current message (carol's @bot what's up) in the history block.
        assert!(msg.content.text.contains("[Chat history begins]"));
        assert!(msg.content.text.contains("[alice] hello"));
        assert!(msg.content.text.contains("[bob] how are you?"));
        assert!(msg.content.text.contains("[Chat history ends]"));
        assert!(msg.content.text.contains("[Current message]"));
        assert!(msg.content.text.contains("@bot what's up"));
        // The current message should NOT appear in the history block.
        let history_section = msg
            .content
            .text
            .split("[Chat history ends]")
            .next()
            .unwrap_or("");
        assert!(
            !history_section.contains("[carol]"),
            "current sender should not appear in history block"
        );
    }

    #[test]
    fn group_history_inject_empty_no_change() {
        // When there is only the current message (no prior history), injection
        // should be a no-op.
        let ch = test_channel(vec![]);
        ch.record_group_history("grp1", "alice", "hello");

        let mut msg = ChannelInboundMessage {
            id: "m1".to_string(),
            sender: MessageSender::new("alice".to_string()),
            receiver: MessageReceiver::new("group:grp1".to_string()),
            content: ChannelMessageContent::text("hello".to_string()),
            timestamp: 0,
            interruption_scope_id: None,
        };
        let original = msg.content.text.clone();
        ch.inject_group_history(&mut msg, "grp1");
        assert_eq!(msg.content.text, original, "text should be unchanged");
    }

    // ── Feature 3: Per-group config resolution ────────────────────────────────

    #[test]
    fn per_group_config_resolves() {
        let ch = test_channel(vec![("group_specific", Some(10)), ("*", Some(5))]);

        // Explicit per-group entry wins.
        assert_eq!(ch.resolve_group_history_limit("group_specific"), 10);
        // Unknown group falls back to wildcard "*".
        assert_eq!(ch.resolve_group_history_limit("unknown_group"), 5);
    }

    #[test]
    fn per_group_config_default_when_no_wildcard() {
        // No wildcard, no explicit entry → default 20.
        let ch = test_channel(vec![("group_a", Some(3))]);
        assert_eq!(ch.resolve_group_history_limit("group_a"), 3);
        assert_eq!(ch.resolve_group_history_limit("group_b"), 20);
    }

    // ── Outbound safety: reply limiter, SSRF, rate limiter, sanitize ──────────

    #[test]
    fn reply_limiter_allows_then_blocks() {
        let mut rl = ReplyLimiter::new();
        // First 4 replies should be allowed.
        for i in 1..=4 {
            assert!(
                rl.check_and_record("msg-001"),
                "reply #{i} should be allowed"
            );
        }
        // 5th reply should be blocked.
        assert!(
            !rl.check_and_record("msg-001"),
            "reply #5 should be blocked"
        );
        // A different msg_id should still work.
        assert!(
            rl.check_and_record("msg-002"),
            "different msg_id should be allowed"
        );
    }

    #[test]
    fn ssrf_blocks_localhost() {
        assert!(is_ssrf_blocked("http://127.0.0.1:8080/img.png"));
        assert!(is_ssrf_blocked("https://localhost/file"));
        assert!(is_ssrf_blocked("http://192.168.1.1/secret"));
        assert!(is_ssrf_blocked("http://10.0.0.1/internal"));
        assert!(is_ssrf_blocked("http://169.254.169.254/metadata"));
    }

    #[test]
    fn ssrf_allows_public() {
        assert!(!is_ssrf_blocked("https://example.com/image.png"));
        assert!(!is_ssrf_blocked("https://multimedia.nt.qq.com/audio.wav"));
    }

    #[test]
    fn strip_internal_tags_removes_thinking() {
        // XML-style thinking tags
        let input = "<thinking>secret reasoning</thinking>Hello world";
        assert_eq!(strip_internal_tags(input), "Hello world");

        // <think> tags
        let input2 = "Hi <think>hidden</think> there";
        assert_eq!(strip_internal_tags(input2), "Hi  there");

        // system-reminder tags
        let input3 = "<system-reminder>do not leak</system-reminder>visible text";
        assert_eq!(strip_internal_tags(input3), "visible text");

        // Deepseek format with backticks
        let input4 = "`think`reasoning here`/think`answer";
        assert_eq!(strip_internal_tags(input4), "answer");

        // No tags → unchanged
        assert_eq!(strip_internal_tags("plain text"), "plain text");
    }

    #[test]
    fn debouncer_buffers_and_merges_texts() {
        let d = DeliverDebouncer::new(100, "\n---\n".to_string());
        assert!(d.enabled());

        let (_, first1) = d.enqueue("c2c:u1", "hello".to_string(), "m1");
        assert!(first1, "first enqueue should drive the flush");
        let (_, first2) = d.enqueue("c2c:u1", "world".to_string(), "m1");
        assert!(!first2, "second enqueue within window should not drive");

        let entry = d.take("c2c:u1").expect("pending entry expected");
        assert_eq!(entry.texts.len(), 2);
        assert_eq!(entry.texts.join("\n---\n"), "hello\n---\nworld");
        assert_eq!(entry.msg_id, "m1");
        assert_eq!(entry.waiters.len(), 2);
    }

    #[test]
    fn debouncer_separate_recipients_are_independent() {
        let d = DeliverDebouncer::new(100, "\n".to_string());
        let (_, f1) = d.enqueue("c2c:a", "one".to_string(), "");
        let (_, f2) = d.enqueue("c2c:b", "two".to_string(), "");
        assert!(f1 && f2, "distinct recipients each drive their own flush");
        assert!(d.take("c2c:a").is_some());
        assert!(d.take("c2c:b").is_some());
    }

    #[test]
    fn debouncer_disabled_when_window_is_zero() {
        let d = DeliverDebouncer::new(0, "\n".to_string());
        assert!(!d.enabled());
    }
}
