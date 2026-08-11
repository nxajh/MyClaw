//! TelegramChannel — the main bot adapter for the Telegram Bot API.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::agents::TurnEvent;
use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};
use crate::channels::{FoldCandidate, StreamDelivery, TurnStream};
use crate::config::channel::TelegramAccountConfig;
use crate::{Channel, DedupState, ProcessingStatus};

use super::types::{Chat, GetUpdatesResponse, Message, SendChatActionRequest};

// ── Constants ─────────────────────────────────────────────────────────────────

const RICH_MESSAGE_LENGTH: usize = 32768;
const CONTINUATION_OVERHEAD: usize = 30;

/// Throttle interval for streaming preview edits (edit-on-stream).
const STREAM_THROTTLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Maximum preview text length (codepoints) to stay under Telegram's 4096-char
/// limit for `sendMessage` / `editMessageText` during streaming.
const STREAM_PREVIEW_LIMIT: usize = 4000;

/// A GFM table delimiter row, e.g. `| --- | :--: |`.
fn is_table_delimiter_local(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let mut has_dash = false;
    for c in t.chars() {
        match c {
            '-' => has_dash = true,
            '|' | ':' | ' ' | '\t' => {}
            _ => return false,
        }
    }
    has_dash
}

// ── TelegramChannel ────────────────────────────────────────────────────────────

/// Entry in the debounce buffer for merging rapid consecutive messages from the same sender.
struct DebounceEntry {
    sender: MessageSender,
    receiver: MessageReceiver,
    texts: Vec<String>,
    files: Vec<ChannelFile>,
    first_ts: u64,
    timer: tokio::task::JoinHandle<()>,
}

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
    typing_tasks: Arc<Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Whether to send acknowledgement reactions on received messages.
    ack_reactions: bool,
    /// Track ack reactions: reply_target → (chat_id, message_id) for removal after reply.
    pending_acks: ReactionTracker,
    /// Status reactions: reply_target → Vec<(chat_id, msg_id)>.
    status_reactions: ReactionTracker,
    /// Debounce window in milliseconds (0 = disabled).
    debounce_ms: u64,
    /// Debounce buffer: "sender|reply_target" → pending entry.
    debounce_buffer: Arc<Mutex<std::collections::HashMap<String, DebounceEntry>>>,
    /// Stall watchdog timeout in seconds (0 = disabled).
    stall_timeout_secs: u64,
    /// Track when typing started for each recipient: reply_target → Instant.
    typing_started_at: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    /// Stall watchdog messages to delete when real reply arrives: reply_target → [(chat_id, msg_id)].
    stall_messages: ReactionTracker,
    /// Streaming preview mode for this channel.
    streaming_mode: crate::config::channel::StreamingMode,
    /// Targets with active streams; stall watchdog skips these to avoid
    /// redundant "still thinking" messages alongside the live preview.
    streaming_targets: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Directory for persisting state (e.g. Telegram update offset).
    data_dir: std::path::PathBuf,
    /// Shared HTTP client with connection pool.
    http: reqwest::Client,
    /// Lightweight ring buffer of recent sent messages (max 100 entries) for
    /// debugging and potential reply-chain context.
    message_cache: Arc<Mutex<VecDeque<(i64, String)>>>,
}

impl TelegramChannel {
    pub fn new(config: TelegramAccountConfig) -> Self {
        let allowed = Self::normalize_allowed_users(config.allowed_users.clone());

        let ch = Self {
            bot_token: config.bot_token.clone(),
            allowed_users: allowed,
            allowed_groups: config.allowed_groups.clone(),
            mention_only: config.mention_only,
            api_base: config
                .api_base
                .unwrap_or_else(|| "https://api.telegram.org".to_string()),
            dedup: DedupState::new(),
            bot_username: Arc::new(Mutex::new(None)),
            workspace_dir: config.workspace_dir.map(std::path::PathBuf::from),
            typing_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            ack_reactions: config.ack_reactions,
            pending_acks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            status_reactions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            debounce_ms: config.debounce_ms,
            debounce_buffer: Arc::new(Mutex::new(std::collections::HashMap::new())),
            stall_timeout_secs: config.stall_timeout_secs,
            typing_started_at: Arc::new(Mutex::new(std::collections::HashMap::new())),
            stall_messages: Arc::new(Mutex::new(std::collections::HashMap::new())),
            streaming_mode: config.streaming_mode,
            streaming_targets: Arc::new(Mutex::new(std::collections::HashSet::new())),
            data_dir: directories::ProjectDirs::from("", "", "myclaw")
                .map(|d| d.data_dir().to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from(".myclaw")),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            message_cache: Arc::new(Mutex::new(VecDeque::new())),
        };
        crate::channels::warn_if_locked_down(&ch);
        ch
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.bot_token, method)
    }

    fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Return cached recent sent messages (message_id, text) for debugging
    /// and potential reply-chain context. Returns up to 100 most recent entries.
    fn get_cached_messages(&self, _chat_id: i64) -> Vec<(i64, String)> {
        self.message_cache.lock().iter().cloned().collect()
    }

    /// Push a message to the ring buffer cache (max 100 entries).
    fn cache_message(&self, message_id: i64, text: String) {
        let mut cache = self.message_cache.lock();
        if cache.len() >= 100 {
            cache.pop_front();
        }
        cache.push_back((message_id, text));
    }

    /// Path to the file that persists the Telegram update offset.
    fn offset_path(&self) -> std::path::PathBuf {
        self.data_dir.join("telegram_offset")
    }

    /// Load the persisted update offset from disk.
    /// Returns 0 when the file does not exist or is unreadable.
    fn load_offset(&self) -> i64 {
        let path = self.offset_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => content.trim().parse().unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Persist the given offset to disk *before* processing the batch.
    /// This ensures that even if the process is killed mid-processing,
    /// a subsequent restart will not re-fetch already-seen updates.
    fn persist_offset(&self, offset: i64) {
        let path = self.offset_path();
        // Best-effort: create parent directory if it doesn't exist yet.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, offset.to_string()) {
            warn!(
                "Failed to persist Telegram offset to {}: {e}",
                path.display()
            );
        }
    }

    fn normalize_identity(value: &str) -> String {
        value.trim().trim_start_matches('@').to_string()
    }

    fn normalize_allowed_users(users: Vec<String>) -> Vec<String> {
        users
            .into_iter()
            .map(|u| Self::normalize_identity(&u))
            .filter(|u| !u.is_empty())
            .collect()
    }

    /// Telegram-specific authorization helper that accommodates both
    /// `username` and `user_id` candidates against the trait-level
    /// `check_authorization(sender, scope)` primitive: a config can list
    /// users by either form, so each candidate gets a shot.
    ///
    /// Returns `Allow` if any candidate matches; otherwise the decision
    /// made for the username candidate (or a synthetic Reject).
    fn try_authorize(
        &self,
        username: Option<&str>,
        user_id: Option<i64>,
        scope: crate::channels::MessageScope<'_>,
    ) -> crate::channels::AuthDecision {
        use crate::channels::{AuthDecision, Channel};
        let identities: Vec<String> = [
            username.map(Self::normalize_identity),
            user_id.map(|i| i.to_string()),
        ]
        .into_iter()
        .flatten()
        .collect();

        if identities.is_empty() {
            return AuthDecision::Reject {
                reason: "no sender identity",
            };
        }

        let mut last_non_allow = AuthDecision::Reject {
            reason: "sender not in allowed_users",
        };
        for ident in &identities {
            match self.check_authorization(ident, scope) {
                AuthDecision::Allow => return AuthDecision::Allow,
                AuthDecision::Ignore => return AuthDecision::Ignore,
                r @ AuthDecision::Reject { .. } => last_non_allow = r,
            }
        }
        last_non_allow
    }

    async fn fetch_bot_username(&self) -> Option<String> {
        let client = self.http_client();
        let resp = client.get(self.api_url("getMe")).send().await.ok()?;
        let data: serde_json::Value = resp.json().await.ok()?;
        data.get("result")?
            .get("username")?
            .as_str()
            .map(String::from)
    }

    fn get_bot_username(&self) -> Option<String> {
        self.bot_username.lock().clone()
    }

    fn set_bot_username(&self, username: String) {
        *self.bot_username.lock() = Some(username);
    }

    /// Find all @mention spans for the bot in text.
    fn find_bot_mention_spans(&self, text: &str) -> Vec<(usize, usize)> {
        let bot_username = match self.get_bot_username() {
            Some(u) => u.trim_start_matches('@').to_string(),
            None => return vec![],
        };
        if bot_username.is_empty() {
            return vec![];
        }

        let mut spans = Vec::new();
        for (at_idx, ch) in text.char_indices() {
            if ch != '@' {
                continue;
            }
            let prev_ok = at_idx == 0
                || !text[..at_idx]
                    .chars()
                    .next_back()
                    .map(|c| c.is_ascii_alphanumeric() || c == '_')
                    .unwrap_or(false);
            if !prev_ok {
                continue;
            }

            let search_start = at_idx + 1;
            let username_end = text[search_start..]
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                .last()
                .map(|(i, _)| i + 1)
                .unwrap_or(0);

            if username_end == 0 {
                continue;
            }

            let men = &text[search_start..search_start + username_end];
            if men.eq_ignore_ascii_case(&bot_username) {
                spans.push((search_start, search_start + username_end));
            }
        }
        spans
    }

    /// Strip @mentions of the bot from text, returning the cleaned text.
    fn strip_bot_mentions(&self, text: &str) -> String {
        let spans = self.find_bot_mention_spans(text);
        if spans.is_empty() {
            return text.to_string();
        }

        let mut result = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end) in spans {
            result.push_str(&text[cursor..start]);
            cursor = end;
        }
        result.push_str(&text[cursor..]);
        result.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Check if text contains a @mention of the bot.
    fn contains_bot_mention(&self, text: &str) -> bool {
        !self.find_bot_mention_spans(text).is_empty()
    }

    /// Check if the message is a reply to a message sent by this bot.
    fn is_reply_to_bot(&self, msg: &Message) -> bool {
        if let Some(ref replied) = msg.reply_to_message {
            if let Some(ref from) = replied.from {
                if let Some(bot_un) = self.get_bot_username() {
                    let from_un = from.username.as_deref().unwrap_or("");
                    return from_un.eq_ignore_ascii_case(bot_un.trim_start_matches('@'));
                }
            }
        }
        false
    }

    fn is_group_message(chat: &Chat) -> bool {
        chat.kind == "group" || chat.kind == "supergroup"
    }

    fn format_forward_attribution(msg: &Message) -> Option<String> {
        if let Some(fwd) = &msg.forward_from {
            let name = fwd
                .username
                .as_ref()
                .map(|u| format!("@{}", u))
                .or_else(|| fwd.first_name.clone())
                .unwrap_or_default();
            return Some(format!("[Forwarded from {}] ", name));
        }
        if let Some(fwd_chat) = &msg.forward_from_chat {
            let title = fwd_chat
                .title
                .clone()
                .or_else(|| fwd_chat.username.clone().map(|u| format!("@{}", u)))
                .unwrap_or_default();
            return Some(format!("[Forwarded from channel: {}] ", title));
        }
        if let Some(name) = &msg.forward_sender_name {
            return Some(format!("[Forwarded from {}] ", name));
        }
        None
    }

    fn parse_reply_target(reply_target: &str) -> (String, Option<String>) {
        if let Some((chat_id, thread_id)) = reply_target.split_once(':') {
            (chat_id.to_string(), Some(thread_id.to_string()))
        } else {
            (reply_target.to_string(), None)
        }
    }

    /// Ensure a blank line before GFM table blocks so Telegram's markdown
    /// parser recognises them. Without a preceding blank line (or message
    /// start), the parser treats `| col | col |` as literal text.
    fn normalize_markdown_tables(text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut result: Vec<String> = Vec::with_capacity(lines.len() + 4);

        for (i, &line) in lines.iter().enumerate() {
            // Detect a table delimiter row preceded by a header row.
            if is_table_delimiter_local(line) && i > 0 {
                let header = lines[i - 1].trim_end();
                if header.contains('|') && !header.is_empty() {
                    // Check if the line before the header is already blank
                    // or the header is the first line. We look at what's
                    // currently last in `result` (which includes lines[0..=i-1]).
                    // The header was pushed as `result`'s last entry. We need
                    // a blank line before it if the entry before it is non-blank.
                    let header_idx = result.len() - 1; // header is last pushed
                    let need_blank = if header_idx == 0 {
                        false // header is first line — fine
                    } else {
                        !result[header_idx - 1].trim().is_empty()
                    };
                    if need_blank {
                        // Insert blank line before the header.
                        let hdr = result.pop().unwrap();
                        result.push(String::new()); // blank line
                        result.push(hdr);
                    }
                }
            }
            result.push(line.to_string());
        }

        result.join("\n")
    }

    async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
        reply_markup: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<i64>> {
        // Try sendRichMessage first; fall back to sendMessage on failure.
        match self
            .send_rich_message(chat_id, text, thread_id, reply_markup.clone())
            .await
        {
            Ok(id) => Ok(id),
            Err(e) => {
                warn!("sendRichMessage failed ({e}), falling back to sendMessage");
                self.send_plain_message(chat_id, text, thread_id, reply_markup)
                    .await
            }
        }
    }

    /// Send via `sendRichMessage` (Telegram Markdown rendering).
    async fn send_rich_message(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
        reply_markup: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();

        let normalized = Self::normalize_markdown_tables(text);
        let mut rich_body = serde_json::json!({
            "chat_id": chat_id,
            "rich_message": {
                "markdown": normalized,
            },
        });
        if let Some(tid) = thread_id {
            rich_body["message_thread_id"] = serde_json::Value::from(tid);
        }
        if let Some(ref markup) = reply_markup {
            rich_body["reply_markup"] = markup.clone();
        }

        let resp = client
            .post(self.api_url("sendRichMessage"))
            .json(&rich_body)
            .send()
            .await?;

        // Handle 429 Too Many Requests with retry
        if resp.status().as_u16() == 429 {
            let retry_after: u64 = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            warn!("Telegram 429 rate limited, retrying after {}s", retry_after);
            tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
            let resp = client
                .post(self.api_url("sendRichMessage"))
                .json(&rich_body)
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("sendRichMessage failed after 429 retry: {status} {body}");
            }
            let resp_json: serde_json::Value = resp.json().await?;
            return Ok(resp_json
                .get("result")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_i64()));
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendRichMessage failed: {status} {body}");
        }

        let resp_json: serde_json::Value = resp.json().await?;
        Ok(resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64()))
    }

    /// Plain-text fallback using standard `sendMessage` (no rich formatting).
    async fn send_plain_message(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
        reply_markup: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown",
        });
        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::from(tid);
        }
        if let Some(ref markup) = reply_markup {
            body["reply_markup"] = markup.clone();
        }

        let resp = client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await?;

        if resp.status().as_u16() == 429 {
            let retry_after: u64 = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
            let resp = client.post(self.api_url("sendMessage")).json(&body).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("sendMessage fallback failed after 429 retry: {status} {body}");
            }
            let resp_json: serde_json::Value = resp.json().await?;
            return Ok(resp_json
                .get("result")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_i64()));
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessage fallback failed: {status} {body}");
        }

        let resp_json: serde_json::Value = resp.json().await?;
        Ok(resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64()))
    }

    /// Low-level `deleteMessage` API wrapper using primitive i64 ids.
    async fn delete_message_raw(&self, chat_id: i64, message_id: i64) -> anyhow::Result<()> {
        let client = self.http_client();
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        });
        let resp = client
            .post(self.api_url("deleteMessage"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("deleteMessage failed: {}", text);
        }
        Ok(())
    }

    /// Low-level `editMessageText` API wrapper using primitive i64 ids.
    async fn edit_message_text_raw(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<bool> {
        let client = self.http_client();

        let normalized = Self::normalize_markdown_tables(text);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "rich_message": {
                "markdown": normalized,
            },
        });

        let resp = client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!("editMessageText failed: {status} {body}");
            return Ok(false);
        }

        Ok(true)
    }

    /// Send a message using rich_message format (markdown rendering), no reply_markup.
    async fn send_rich_message_simple(
        &self,
        chat_id: &str,
        markdown: &str,
        thread_id: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "rich_message": {
                "markdown": markdown,
            },
        });
        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::from(tid);
        }
        let resp = client
            .post(self.api_url("sendRichMessage"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendRichMessage(preview) failed: {status} {body}");
        }
        let resp_json: serde_json::Value = resp.json().await?;
        Ok(resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64()))
    }

    /// Send a text message with `parse_mode: HTML` (standard Telegram API).
    async fn send_text_html(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
        });
        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::from(tid);
        }
        let resp = client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessage(HTML) failed: {status} {body}");
        }
        let resp_json: serde_json::Value = resp.json().await?;
        Ok(resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64()))
    }

    /// Edit a message using rich_message format (markdown rendering).
    async fn edit_message_rich(
        &self,
        chat_id: i64,
        message_id: i64,
        markdown: &str,
    ) -> anyhow::Result<bool> {
        let client = self.http_client();
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "rich_message": {
                "markdown": markdown,
            },
        });
        let resp = client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!("editMessageText(rich) failed: {status} {body}");
            return Ok(false);
        }
        Ok(true)
    }

    /// Edit a message with `parse_mode: HTML` (standard Telegram API).
    async fn edit_message_text_html(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<bool> {
        let client = self.http_client();
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "parse_mode": "HTML",
        });
        let resp = client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!("editMessageText(HTML) failed: {status} {body}");
            return Ok(false);
        }
        Ok(true)
    }

    async fn send_chat_action(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        action: &str,
    ) -> anyhow::Result<()> {
        let client = self.http_client();
        let req = SendChatActionRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            action: action.to_string(),
        };
        let resp = client
            .post(self.api_url("sendChatAction"))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            warn!("sendChatAction failed: {}", resp.status());
        }
        Ok(())
    }

    fn parse_message_content(&self, msg: &Message) -> String {
        let mut content = msg.text.clone().unwrap_or_default();

        if let Some(attr) = Self::format_forward_attribution(msg) {
            content = format!("{}{}", attr, content);
        }

        content
    }

    /// Download a Telegram file by file_id and return its base64-encoded content.
    async fn download_file_bytes(&self, file_id: &str) -> anyhow::Result<Vec<u8>> {
        // Step 1: Get the file_path from Telegram.
        let client = self.http_client();
        let url = format!("{}?file_id={}", self.api_url("getFile"), file_id);
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("getFile failed: status={}", resp.status());
        }
        let data: serde_json::Value = resp.json().await?;
        let file_path = data
            .get("result")
            .and_then(|r| r.get("file_path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("getFile response missing file_path"))?;

        // Step 2: Download the file content.
        let download_url = format!("{}/file/bot{}/{}", self.api_base, self.bot_token, file_path);
        let file_resp = client.get(&download_url).send().await?;
        if !file_resp.status().is_success() {
            anyhow::bail!("file download failed: status={}", file_resp.status());
        }
        Ok(file_resp.bytes().await?.to_vec())
    }

    /// Convert audio bytes (MP3/WAV/any) to Ogg/Opus for Telegram voice bubbles.
    /// Uses ffmpeg as a subprocess. Returns the Ogg/Opus bytes.
    async fn convert_to_opus_ogg(&self, audio_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        use tokio::io::AsyncWriteExt;

        let temp_dir = tempfile::tempdir()?;
        let input_path = temp_dir.path().join("input_audio");
        let output_path = temp_dir.path().join("voice.ogg");

        // Write input bytes to temp file
        let mut input_file = tokio::fs::File::create(&input_path).await?;
        input_file.write_all(audio_bytes).await?;
        input_file.flush().await?;
        drop(input_file);

        // Run ffmpeg to convert to Ogg/Opus
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-i")
            .arg(&input_path)
            .args([
                "-acodec", "libopus",
                "-ac", "1",
                "-b:a", "48k",
                "-vbr", "on",
                "-application", "voip",
                "-compression_level", "10",
                "-f", "ogg",
            ])
            .arg(&output_path)
            .arg("-y")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn ffmpeg: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg conversion failed: {}", &stderr[..stderr.len().min(200)]);
        }

        let result = tokio::fs::read(&output_path).await?;
        if result.is_empty() {
            anyhow::bail!("ffmpeg produced empty output");
        }
        Ok(result)
    }

    /// Send an acknowledgement reaction (👀) to a message.
    async fn ack_message(&self, chat_id: i64, message_id: i64) {
        if !self.ack_reactions {
            return;
        }
        let client = self.http_client();
        let url = self.api_url("setMessageReaction");
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{"type": "emoji", "emoji": "👀"}]
        });
        if let Err(e) = client.post(&url).json(&body).send().await {
            warn!(
                "Failed to send ack reaction to message {} in chat {}: {e}",
                message_id, chat_id
            );
        }
    }

    /// Remove acknowledgement reaction from a message after reply.
    async fn remove_ack(&self, chat_id: i64, message_id: i64) {
        if !self.ack_reactions {
            return;
        }
        let client = self.http_client();
        let url = self.api_url("setMessageReaction");
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": []
        });
        if let Err(e) = client.post(&url).json(&body).send().await {
            warn!(
                "Failed to remove ack reaction from message {} in chat {}: {e}",
                message_id, chat_id
            );
        }
    }

    /// Set an emoji reaction on a message.
    async fn set_reaction(&self, chat_id: i64, message_id: i64, emoji: &str) -> anyhow::Result<()> {
        let client = self.http_client();
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{ "type": "emoji", "emoji": emoji }]
        });
        let resp = client
            .post(self.api_url("setMessageReaction"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("setMessageReaction failed: {}", text);
        }
        Ok(())
    }

    /// Remove a specific emoji reaction from a message.
    async fn remove_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        _emoji: &str,
    ) -> anyhow::Result<()> {
        // Setting an empty reaction array removes all reactions.
        let client = self.http_client();
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": []
        });
        let resp = client
            .post(self.api_url("setMessageReaction"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("setMessageReaction(remove) failed: {}", text);
        }
        Ok(())
    }

    /// Acknowledge a callback query (stops the loading spinner on the button).
    async fn answer_callback_query(&self, callback_query_id: &str) {
        let client = self.http_client();
        let url = self.api_url("answerCallbackQuery");
        let body = serde_json::json!({
            "callback_query_id": callback_query_id
        });
        if let Err(e) = client.post(&url).json(&body).send().await {
            warn!("Failed to answer callback query {}: {e}", callback_query_id);
        }
    }

    /// Start a typing keep-alive task for a recipient.
    ///
    /// Telegram's sendChatAction lasts ~5 seconds. This method spawns a
    /// background task that refreshes it every 4 seconds until aborted.
    fn start_internal_typing(&self, recipient: &str) {
        let (chat_id, thread_id) = Self::parse_reply_target(recipient);

        // Abort existing task for this recipient
        let mut tasks = self.typing_tasks.lock();
        if let Some(handle) = tasks.remove(recipient) {
            handle.abort();
        }

        // Record typing start time for stall watchdog.
        self.typing_started_at
            .lock()
            .insert(recipient.to_string(), std::time::Instant::now());

        let bot_token = self.bot_token.clone();
        let api_base = self.api_base.clone();
        let recipient_key = recipient.to_string();
        let recipient_key_clone = recipient_key.clone();
        let typing_tasks = self.typing_tasks.clone();
        let typing_started_at = self.typing_started_at.clone();

        let handle = tokio::spawn(async move {
            let max_consecutive_failures: u32 = 2;
            let max_duration = std::time::Duration::from_secs(60);
            let start = tokio::time::Instant::now();
            let mut consecutive_failures: u32 = 0;

            // Create a typing-specific client with shorter timeout.
            let typing_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            loop {
                // TTL check
                if start.elapsed() >= max_duration {
                    warn!(
                        "Telegram typing TTL exceeded ({}s) for {}",
                        max_duration.as_secs(),
                        recipient_key
                    );
                    break;
                }

                // Send typing action
                let url = format!("{}/bot{}/sendChatAction", api_base, bot_token);
                let req = SendChatActionRequest {
                    chat_id: chat_id.clone(),
                    message_thread_id: thread_id.clone(),
                    action: "typing".to_string(),
                };
                match typing_client.post(&url).json(&req).send().await {
                    Ok(_) => consecutive_failures = 0,
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures >= max_consecutive_failures {
                            warn!(
                                "Telegram typing circuit breaker tripped after {consecutive_failures} consecutive failures for {}: {e}",
                                recipient_key
                            );
                            break;
                        }
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }

            // Task exiting: clean up only if no new task has taken over.
            let mut tasks = typing_tasks.lock();
            if let Some(h) = tasks.get(&recipient_key_clone) {
                if h.is_finished() {
                    tasks.remove(&recipient_key_clone);
                    typing_started_at.lock().remove(&recipient_key_clone);
                }
            }
        });
        tasks.insert(recipient.to_string(), handle);
    }

    /// Stop (abort) the typing keep-alive task for a recipient.
    fn stop_internal_typing(&self, recipient: &str) {
        let mut tasks = self.typing_tasks.lock();
        if let Some(handle) = tasks.remove(recipient) {
            handle.abort();
        }
        // Remove stall watchdog tracking.
        self.typing_started_at.lock().remove(recipient);
    }
}

impl TelegramChannel {
    /// Buffer an inbound message for debounce merging.
    ///
    /// Messages from the same sender in the same conversation are merged
    /// and dispatched as a single `ChannelInboundMessage` after the debounce window
    /// expires. If debounce is disabled (`debounce_ms == 0`), the message is
    /// sent immediately via `tx`.
    async fn debounce_send(
        &self,
        mut msg: ChannelInboundMessage,
        tx: mpsc::Sender<ChannelInboundMessage>,
    ) {
        if self.debounce_ms == 0 {
            if let Err(e) = tx.send(msg).await {
                warn!("Telegram dispatch error: {e}");
            }
            return;
        }

        let key = format!("{}|{}", msg.sender.id, msg.receiver.id);
        let debounce_ms = self.debounce_ms;
        let buffer = self.debounce_buffer.clone();
        let sender_key = key.clone();

        // Create timer task (starts sleeping immediately).
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
            let entry = buffer.lock().remove(&sender_key);
            if let Some(entry) = entry {
                let channel_msg = ChannelInboundMessage {
                    id: format!("debounced_{}", entry.first_ts),
                    sender: entry.sender,
                    receiver: entry.receiver,
                    content: ChannelMessageContent {
                        text: entry.texts.join("\n"),
                        files: entry.files,
                        buttons: vec![],
                    },
                    timestamp: entry.first_ts,
                    interruption_scope_id: None,
                    silenced_override: None,
                };
                let _ = tx.send(channel_msg).await;
            }
        });

        // Lock the buffer and update/create entry.
        {
            let mut buf = self.debounce_buffer.lock();
            if let Some(entry) = buf.get_mut(&key) {
                // Merge into existing entry.
                if !msg.content.text.is_empty() {
                    entry.texts.push(msg.content.text);
                }
                if !msg.content.files.is_empty() {
                    entry.files.append(&mut msg.content.files);
                }
                // Cancel old timer, set new one.
                entry.timer.abort();
                entry.timer = handle;
            } else {
                // New entry.
                buf.insert(
                    key,
                    DebounceEntry {
                        sender: msg.sender,
                        receiver: msg.receiver,
                        texts: if msg.content.text.is_empty() {
                            vec![]
                        } else {
                            vec![msg.content.text]
                        },
                        files: msg.content.files,
                        first_ts: msg.timestamp,
                        timer: handle,
                    },
                );
            }
        }
    }

    /// Background task that monitors for stalled conversations.
    ///
    /// If typing has been active for longer than `stall_timeout_secs` for a
    /// recipient, sends a "still thinking" notice so the user knows the bot
    /// is alive. Only one notice is sent per stall event.
    async fn stall_watchdog(&self) {
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
    async fn poll_loop(&self, tx: mpsc::Sender<ChannelInboundMessage>) {
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
                };

                if self.debounce_ms > 0 {
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

                    let debounce_key =
                        format!("{}|{}", channel_msg.sender.id, channel_msg.receiver.id);
                    let is_new = !self.debounce_buffer.lock().contains_key(&debounce_key);
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
    fn chunk_for_telegram(content: &str) -> Vec<String> {
        use crate::channels::message::{LenUnit, split_message_chunk};

        let rich_limit = RICH_MESSAGE_LENGTH.saturating_sub(CONTINUATION_OVERHEAD * 2);
        split_message_chunk(content, rich_limit, LenUnit::Codepoints)
    }
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

        let (fold_msg_id, inherited) = match fold {
            Some(f) => (f.msg_id.parse::<i64>().ok(), Some(f.text)),
            None => (None, None),
        };

        Some(Box::new(TelegramTurnStream {
            channel: self.clone(),
            chat_id,
            thread_id,
            reply_target: reply_target.to_string(),
            mode: self.streaming_mode,
            msg_id: fold_msg_id,
            // Partial mode: seed accumulated with the inherited body so a
            // resumed turn keeps prior text instead of replacing it.
            accumulated: if self.streaming_mode
                == crate::config::channel::StreamingMode::Partial
            {
                inherited.clone().unwrap_or_default()
            } else {
                String::new()
            },
            tool_lines: Vec::new(),
            tool_count: 0,
            thinking_steps: 0,
            commentary_notes: 0,
            thinking_tokens: 0,
            thinking_active: false,
            pending_commentary: String::new(),
            inherited_preview: inherited,
            defer_collapse: false,
            start: std::time::Instant::now(),
            last_edit: std::time::Instant::now() - STREAM_THROTTLE,
            delivery: StreamDelivery::Pending,
            finished: false,
        }))
    }
}

// ── Telegram streaming preview (edit-on-stream) ───────────────────────────────

/// Escape HTML special characters for Telegram parse_mode=HTML.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Clip detail text to max 300 chars (matching OpenClaw's clipTelegramProgressText).
fn clip_detail(s: &str) -> String {
    const MAX: usize = 300;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= MAX {
        return s.to_string();
    }
    let clipped: String = chars.into_iter().take(MAX - 1).collect();
    format!("{}…", clipped.trim_end())
}

/// Resolve (emoji, label, detail) for a tool call, OpenClaw-style.
fn resolve_tool_display(name: &str, args: &serde_json::Value) -> (String, String, String) {
    let key = name.to_lowercase();
    let (emoji, label) = match key.as_str() {
        "shell" => ("🛠️", "Shell"),
        "file_read" | "read_file" | "read" => ("📖", "Read"),
        "file_write" | "write_file" | "write" => ("✍️", "Write"),
        "file_edit" | "edit" | "patch" => ("📝", "Edit"),
        "web_search" => ("🔍", "Search"),
        "http_request" => ("🌐", "HTTP"),
        "content_search" | "grep" => ("🔎", "Grep"),
        "glob_search" | "glob" => ("📂", "Glob"),
        "list_dir" | "ls" => ("📂", "List"),
        "memory_manage" => ("🧠", "Memory"),
        "memory_search" => ("🧠", "Memory"),
        "agent_delegate" | "delegate" => ("✨", "Delegate"),
        "calculator" => ("🔢", "Calc"),
        "view_image" => ("🖼️", "Image"),
        "view_video" => ("🎬", "Video"),
        "hear_audio" => ("🎵", "Audio"),
        "skill_view" => ("📜", "Skill"),
        "skill_manage" => ("📜", "Skill"),
        "send_message" => ("💬", "Send"),
        "ask_user" => ("❓", "Ask"),
        "task_create" => ("📋", "Task"),
        "task_update" => ("📋", "Task"),
        "task_list" => ("📋", "Task"),
        "task_delete" => ("📋", "Task"),
        "shell_poll" => ("📊", "Poll"),
        _ => ("🔧", name),
    }
    .to_owned();

    // Extract a short detail string from the most relevant arg field.
    let detail_keys: &[&str] = match key.as_str() {
        "shell" | "shell_poll" => &["command", "cmd"],
        "file_read" | "read_file" | "read" => &["path", "file_path"],
        "file_write" | "write_file" | "write" => &["path", "file_path"],
        "file_edit" | "edit" | "patch" => &["path", "file_path"],
        "web_search" => &["query", "q"],
        "http_request" => &["url", "method"],
        "content_search" | "grep" => &["pattern", "regex"],
        "glob_search" | "glob" => &["pattern"],
        "list_dir" | "ls" => &["path"],
        "memory_manage" => &["name", "action"],
        "memory_search" => &["query"],
        "agent_delegate" | "delegate" => &["agent", "task"],
        "calculator" => &["expression"],
        "view_image" => &["path"],
        "view_video" => &["path"],
        "hear_audio" => &["path"],
        "skill_view" | "skill_manage" => &["name", "action"],
        "task_create" => &["subject"],
        "task_update" => &["task_id", "status"],
        "task_delete" => &["task_id"],
        _ => &[],
    };

    let detail = detail_keys
        .iter()
        .find_map(|&k| {
            args.get(k)
                .and_then(|v| v.as_str())
                .map(|s| {
                    let s = s.trim();
                    if s.chars().count() > 50 {
                        let truncated: String = s.chars().take(47).collect();
                        format!("{truncated}…")
                    } else {
                        s.to_string()
                    }
                })
        })
        .unwrap_or_default();

    (emoji.to_string(), label.to_string(), detail)
}

/// Format a single tool-call progress line as Telegram Markdown.
///
/// Output: `**📖 Read** \`/path/to/file\``
/// With optional status: `… _failed_`
fn format_tool_line(name: &str, args: &serde_json::Value) -> String {
    let (emoji, label, detail) = resolve_tool_display(name, args);
    let label_full = format!("{emoji} {label}");
    if detail.is_empty() {
        format!("**{label_full}**")
    } else {
        let detail_clipped = clip_detail(&detail);
        format!("**{label_full}** `{detail_clipped}`")
    }
}

/// Re-format a tool line to append a status suffix (e.g. `_failed_`).
fn tool_line_with_status(line: &str, success: bool) -> String {
    if success {
        line.to_string()
    } else {
        format!("{line} _failed_")
    }
}

/// Per-turn streaming handle for Telegram.
///
/// Two modes:
/// - **Partial**: accumulates ALL text chunks and live-edits a preview
///   message. The final edit replaces it with the complete answer.
/// - **Progress**: shows only tool-call progress lines with per-tool emoji,
///   label, and arg detail (e.g. `📖 Read /path`), rendered as rich markdown.
///   When the turn completes, the preview collapses to a one-line summary
///   (e.g. `🛠️ 4 tool calls · ⏱️ 21s`) and the final answer is sent as a
///   separate message via the normal `send_message` path.
struct TelegramTurnStream {
    channel: TelegramChannel,
    chat_id: i64,
    thread_id: Option<String>,
    /// Original reply_target — used to remove from `streaming_targets`.
    reply_target: String,
    mode: crate::config::channel::StreamingMode,
    /// Message being live-edited; `None` until first flush.
    msg_id: Option<i64>,
    /// Accumulated text (partial mode only).
    accumulated: String,
    /// Tool-call progress lines (progress mode): `["🔧 file_read", …]`.
    tool_lines: Vec<String>,
    /// Tool-call count (progress mode, for collapse summary).
    tool_count: usize,
    /// Thinking step count (progress mode, for collapse summary).
    thinking_steps: usize,
    /// Commentary notes count (progress mode, for collapse summary).
    commentary_notes: usize,
    /// Estimated thinking token count for the current round (progress mode).
    thinking_tokens: usize,
    /// Whether thinking is currently active (progress mode).
    thinking_active: bool,
    /// Pending commentary text accumulated from Chunk events; flushed to a
    /// 💬 line when a ToolCall arrives (text before tools = commentary;
    /// text after last tool = final answer, discarded on Done).
    pending_commentary: String,
    /// 单 preview (2026-08-12): body of the preview message taken over from
    /// a previous (origin) turn — rendered as the leading block so prior
    /// progress lines stay visible (保留历史行追加). `None` on fresh streams.
    inherited_preview: Option<String>,
    /// 单 preview (2026-08-12): intermediate (silenced) resume turn — `Done`
    /// keeps the preview lines (no collapse); the final resume turn
    /// collapses. Set via `TurnStream::defer_collapse`.
    defer_collapse: bool,
    /// Turn start time (progress mode, for collapse summary).
    start: std::time::Instant,
    last_edit: std::time::Instant,
    delivery: StreamDelivery,
    finished: bool,
}

impl TelegramTurnStream {
    fn is_progress(&self) -> bool {
        self.mode == crate::config::channel::StreamingMode::Progress
    }

    /// Flush the current thinking round into the step list as a retained line.
    fn flush_completed_thinking(&mut self) {
        if self.thinking_active {
            self.thinking_active = false;
            if self.thinking_tokens > 0 {
                self.tool_lines.push(format!(
                    "🧠 Thinking… (~{} tokens)",
                    self.thinking_tokens
                ));
            }
            self.thinking_tokens = 0;
        }
    }

    /// Build the preview text for the current mode.
    fn preview_text(&self) -> String {
        if self.is_progress() {
            let mut lines = Vec::new();

            // 单 preview: inherited body (from the origin turn) stays as the
            // leading block — prior progress lines are never wiped, new lines
            // append below (保留历史行追加).
            if let Some(inh) = &self.inherited_preview {
                let text = clip_detail(inh.trim());
                if !text.is_empty() {
                    lines.push(text);
                }
            }

            // Headline: pending commentary shown as bold when no steps yet.
            if !self.pending_commentary.trim().is_empty()
                && self.tool_lines.is_empty()
                && !self.thinking_active
            {
                let text = clip_detail(self.pending_commentary.trim());
                lines.push(format!("**{}**", text));
            }

            // Tail: pending commentary (💬) and live thinking (🧠).
            let mut tail = Vec::new();
            if !self.pending_commentary.trim().is_empty() && !self.tool_lines.is_empty() {
                let text = clip_detail(self.pending_commentary.trim());
                tail.push(format!("💬 {}", text));
            }
            // Live thinking line at end (most recent activity).
            if self.thinking_active && self.thinking_tokens > 0 {
                tail.push(format!(
                    "🧠 Thinking… (~{} tokens)",
                    self.thinking_tokens
                ));
            }

            // Truncate oldest tool lines so total preview stays under
            // STREAM_PREVIEW_LIMIT (Telegram editMessageText 4096-char cap).
            let total = self.tool_lines.len();
            let mut skip = total;
            for s in 0..total {
                let mut test = lines.clone();
                test.extend(self.tool_lines[s..].iter().cloned());
                test.extend(tail.clone());
                if test.join("\n\n").chars().count() <= STREAM_PREVIEW_LIMIT {
                    skip = s;
                    break;
                }
            }

            if skip > 0 && skip < total {
                lines.push(format!("… {} earlier", skip));
            }
            lines.extend(self.tool_lines[skip..].iter().cloned());
            lines.extend(tail);

            lines.join("\n\n")
        } else {
            self.accumulated.chars().take(STREAM_PREVIEW_LIMIT).collect()
        }
    }

    /// Send or edit the preview message.
    async fn flush_preview(&mut self) {
        let preview = self.preview_text();
        if preview.is_empty() {
            return;
        }
        // Loop so an edit failure (message deleted server-side, e.g. the
        // taken-over preview was removed) falls back to sending a new message
        // in the same flush instead of wedging the stream on a dead msg_id.
        loop {
            match self.msg_id {
                Some(mid) => {
                    if self
                        .channel
                        .edit_message_rich(self.chat_id, mid, &preview)
                        .await
                        .is_ok()
                    {
                        self.delivery = StreamDelivery::Visible;
                        break;
                    }
                    // Edit failed — drop the stale id and retry as a send.
                    self.msg_id = None;
                }
                None => {
                    if let Ok(Some(id)) = self
                        .channel
                        .send_rich_message_simple(
                            &self.chat_id.to_string(),
                            &preview,
                            self.thread_id.as_deref(),
                        )
                        .await
                    {
                        self.msg_id = Some(id);
                        self.delivery = StreamDelivery::Visible;
                    }
                    break;
                }
            }
        }
        self.last_edit = std::time::Instant::now();
    }

    /// Delete the preview message (transition to `send_message` fallback).
    async fn delete_preview(&mut self) {
        if let Some(mid) = self.msg_id.take() {
            let _ = self.channel.delete_message_raw(self.chat_id, mid).await;
        }
    }

    /// Build the collapse summary line, OpenClaw-style.
    ///
    /// `🧠 2 thoughts · 🛠️ 4 tool calls · ⏱️ 21s`
    fn collapse_summary(&self) -> String {
        let elapsed = self.start.elapsed().as_secs().max(1);
        let mut parts = Vec::new();
        if self.thinking_steps > 0 {
            let plural = if self.thinking_steps == 1 { "thought" } else { "thoughts" };
            parts.push(format!("🧠 {} {plural}", self.thinking_steps));
        }
        if self.commentary_notes > 0 {
            let plural = if self.commentary_notes == 1 { "note" } else { "notes" };
            parts.push(format!("💬 {} {plural}", self.commentary_notes));
        }
        if self.tool_count > 0 {
            let plural = if self.tool_count == 1 { "tool call" } else { "tool calls" };
            parts.push(format!("🛠️ {} {plural}", self.tool_count));
        }
        parts.push(format!("⏱️ {elapsed}s"));
        parts.join(" · ")
    }

    /// Edit the preview message into a collapse summary.
    async fn collapse_to_summary(&mut self) {
        let summary = self.collapse_summary();
        if let Some(mid) = self.msg_id {
            if self
                .channel
                .edit_message_rich(self.chat_id, mid, &summary)
                .await
                .is_ok()
            {
                return;
            }
        }
        // Fallback: delete if edit failed or no msg_id.
        self.delete_preview().await;
    }

    /// Remove this target from the streaming tracker.
    fn untrack(&self) {
        self.channel
            .streaming_targets
            .lock()
            .remove(&self.reply_target);
    }
}

#[async_trait]
impl TurnStream for TelegramTurnStream {
    async fn push(&mut self, event: TurnEvent) -> anyhow::Result<StreamDelivery> {
        if self.finished {
            return Ok(self.delivery);
        }

        if self.is_progress() {
            // ── Progress mode ───────────────────────────────────────────────
            match event {
                TurnEvent::Chunk { delta } => {
                    // Thinking ends when text starts; retain completed round.
                    self.flush_completed_thinking();
                    // Accumulate text chunks. If a tool call follows, this
                    // text was commentary (intermediate explanation); if Done
                    // follows, it was the final answer streaming (discarded).
                    self.pending_commentary.push_str(&delta);
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::ToolCall { name, args, .. } => {
                    // Thinking ends when a tool call starts; retain completed round.
                    self.flush_completed_thinking();
                    // Flush pending commentary as a 💬 line before the tool call.
                    if !self.pending_commentary.trim().is_empty() {
                        self.commentary_notes += 1;
                        let text = clip_detail(self.pending_commentary.trim());
                        self.tool_lines
                            .push(format!("💬 {}", text));
                        self.pending_commentary.clear();
                    }
                    self.tool_count += 1;
                    self.tool_lines.push(format_tool_line(&name, &args));
                    // Throttle: avoid edit-storm on rapid tool calls.
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::Thinking { delta } => {
                    // Count bursts (thinking rounds), not individual deltas.
                    // Each transition from non-thinking to thinking is one round.
                    if !self.thinking_active {
                        // Flush pending commentary before new thinking round
                        // (preserves chronological ordering in step list).
                        if !self.pending_commentary.trim().is_empty() {
                            self.commentary_notes += 1;
                            let text = clip_detail(self.pending_commentary.trim());
                            self.tool_lines.push(format!("💬 {}", text));
                            self.pending_commentary.clear();
                        }
                        self.thinking_steps += 1;
                        self.thinking_tokens = 0;
                    }
                    self.thinking_active = true;
                    // Rough token estimate: ~1 token per 4 chars, minimum 1 per event.
                    let est = (delta.len() / 4).max(1);
                    self.thinking_tokens += est;
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::ToolResult { name, output, .. } => {
                    // Detect failure from output and annotate the matching
                    // tool line with `_failed_` (OpenClaw-style).
                    let failed = output.starts_with("error")
                        || output.starts_with("Error")
                        || output.contains("failed:")
                        || output.contains("panicked");
                    if failed {
                        // Find the last line for this tool name.
                        let label = resolve_tool_display(&name, &serde_json::Value::Null).1;
                        if let Some(line) = self
                            .tool_lines
                            .iter_mut()
                            .rev()
                            .find(|l| l.contains(&label) && !l.contains("_failed_"))
                        {
                            *line = tool_line_with_status(line, false);
                        }
                    }
                }
                TurnEvent::Done { text } => {
                    if self.defer_collapse {
                        // 单 preview (silenced resume turn): the turn's model
                        // output is intermediate progress — append it as a 💬
                        // line and KEEP the preview (no collapse). The final
                        // resume turn collapses. `pending_commentary` carries
                        // the streamed text; `text` is the fallback for
                        // non-streaming providers.
                        let note = if !self.pending_commentary.trim().is_empty() {
                            std::mem::take(&mut self.pending_commentary)
                        } else {
                            text
                        };
                        if !note.trim().is_empty() {
                            self.commentary_notes += 1;
                            self.tool_lines
                                .push(format!("💬 {}", clip_detail(note.trim())));
                        }
                        self.flush_preview().await;
                        self.finished = true;
                    } else {
                        // Collapse preview into a one-line summary; the final
                        // answer is sent by `send_message` (delivery != FinalDelivered).
                        self.collapse_to_summary().await;
                        self.finished = true;
                    }
                }
                TurnEvent::Cancelled { .. }
                | TurnEvent::Error { .. }
                | TurnEvent::EmptyResponse { .. } => {
                    self.delete_preview().await;
                    self.finished = true;
                }
            }
        } else {
            // ── Partial mode (legacy) ──────────────────────────────────────
            match event {
                TurnEvent::Chunk { delta } => {
                    self.accumulated.push_str(&delta);
                    if self.last_edit.elapsed() >= STREAM_THROTTLE {
                        self.flush_preview().await;
                    }
                }
                TurnEvent::Done { text } => {
                    if self.defer_collapse {
                        // 单 preview (silenced resume turn): append the turn's
                        // intermediate text to the inherited body; keep the
                        // message (no delete, no FinalDelivered — the
                        // suspended-turn gate skips the fallback send).
                        self.accumulated.push_str(&text);
                        self.finished = true;
                        self.flush_preview().await;
                    } else {
                        self.accumulated = text;
                        self.finished = true;
                        if self.accumulated.chars().count() > STREAM_PREVIEW_LIMIT {
                            self.delete_preview().await;
                            // Leave delivery as Visible/Pending → triggers fallback.
                        } else {
                            self.flush_preview().await;
                            self.delivery = StreamDelivery::FinalDelivered;
                        }
                    }
                }
                TurnEvent::Error { .. } | TurnEvent::EmptyResponse { .. } => {
                    self.finished = true;
                }
                TurnEvent::Cancelled { partial } => {
                    self.accumulated = partial;
                    self.finished = true;
                    self.flush_preview().await;
                }
                _ => {}
            }
        }
        Ok(self.delivery)
    }

    fn status(&self) -> StreamDelivery {
        self.delivery
    }

    fn fold_candidate(&self) -> Option<FoldCandidate> {
        // Only a flushed preview message can be repurposed; if it was
        // deleted (partial mode past the 4096 cap) there is nothing to fold.
        let msg_id = self.msg_id?;
        // Report what the user currently sees: progress mode collapses to
        // the one-line summary on Done — EXCEPT for defer_collapse resume
        // turns, which keep the full preview lines (单 preview takeover
        // needs the real body to seed the next turn's inherited history).
        let text = if self.finished && self.is_progress() && !self.defer_collapse {
            self.collapse_summary()
        } else {
            self.preview_text()
        };
        Some(FoldCandidate {
            msg_id: msg_id.to_string(),
            text,
        })
    }

    fn defer_collapse(&mut self) {
        self.defer_collapse = true;
    }

    async fn finish(self: Box<Self>) -> StreamDelivery {
        let mut s = *self;
        s.untrack();
        if !s.finished && !s.accumulated.is_empty() {
            s.flush_preview().await;
        }
        s.delivery
    }

    async fn abort(self: Box<Self>) {
        self.untrack();
        // Best-effort: delete the preview message if it was never finalized.
        if let (Some(mid), false) = (self.msg_id, self.finished) {
            let _ = self.channel.delete_message_raw(self.chat_id, mid).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::User;
    use super::*;

    fn make_config() -> TelegramAccountConfig {
        TelegramAccountConfig {
            bot_token: "test_token_123".into(),
            allowed_users: vec!["alice".into(), "123456".into()],
            allowed_groups: None,
            mention_only: false,
            api_base: Some("https://api.telegram.org".into()),
            proxy_url: None,
            enabled: true,
            approval_timeout_secs: 120,
            ack_reactions: true,
            workspace_dir: None,
            debounce_ms: 0,        // disabled in tests
            stall_timeout_secs: 0, // disabled in tests
            streaming_mode: crate::config::channel::StreamingMode::Partial,
        }
    }

    #[test]
    fn test_normalize_identity() {
        assert_eq!(TelegramChannel::normalize_identity("@Alice"), "Alice");
        assert_eq!(TelegramChannel::normalize_identity("  Bob  "), "Bob");
        assert_eq!(TelegramChannel::normalize_identity("charlie"), "charlie");
    }

    #[test]
    fn test_normalize_allowed_users() {
        let users = vec!["@Alice".into(), "  Bob  ".into(), "charlie".into()];
        let normalized = TelegramChannel::normalize_allowed_users(users);
        assert_eq!(normalized, vec!["Alice", "Bob", "charlie"]);
    }

    #[test]
    fn phase4_security_policy_default_rejects_groups() {
        use crate::channels::{Channel, GroupAuthMode};
        let ch = TelegramChannel::new(make_config());
        // make_config has allowed_groups = None, mention_only = false
        let policy = ch.security_policy();
        assert!(
            matches!(policy.group_mode, GroupAuthMode::Reject),
            "Phase 4 default: missing allowed_groups must reject groups"
        );
    }

    #[test]
    fn phase4_check_authorization_dm_allows_listed_user() {
        use crate::channels::{AuthDecision, MessageScope};
        let ch = TelegramChannel::new(make_config());
        // make_config lists "alice" — should allow DM from username alice
        let decision = ch.try_authorize(Some("alice"), Some(999), MessageScope::Direct);
        assert_eq!(decision, AuthDecision::Allow);

        // user_id 123456 also listed — should allow DM by id even without username
        let decision = ch.try_authorize(None, Some(123456), MessageScope::Direct);
        assert_eq!(decision, AuthDecision::Allow);

        // Unlisted user → Reject
        let decision = ch.try_authorize(Some("bob"), Some(777), MessageScope::Direct);
        assert!(matches!(decision, AuthDecision::Reject { .. }));
    }

    #[test]
    fn phase4_group_mention_only_with_allowlist() {
        use crate::channels::{AuthDecision, MessageScope};
        let mut cfg = make_config();
        cfg.allowed_groups = Some(vec!["*".into()]);
        cfg.mention_only = true;
        let ch = TelegramChannel::new(cfg);

        // Allowed user, group, no mention → Ignore (silent drop, not warn)
        let decision = ch.try_authorize(
            Some("alice"),
            None,
            MessageScope::Group {
                id: "-100123",
                has_mention: false,
            },
        );
        assert_eq!(decision, AuthDecision::Ignore);

        // Allowed user, group, with mention → Allow
        let decision = ch.try_authorize(
            Some("alice"),
            None,
            MessageScope::Group {
                id: "-100123",
                has_mention: true,
            },
        );
        assert_eq!(decision, AuthDecision::Allow);
    }

    /// 挂起轮折叠 (2026-08-11): a flushed preview message is reported as a
    /// fold candidate (id + current body) so the suspension machinery can
    /// edit it in place; no flushed message → None (nothing to fold).
    #[test]
    fn fold_candidate_reports_flushed_message() {
        let ch = TelegramChannel::new(make_config());
        let mut s = TelegramTurnStream {
            channel: ch,
            chat_id: 42,
            thread_id: None,
            reply_target: "t".to_string(),
            mode: crate::config::channel::StreamingMode::Partial,
            msg_id: Some(7),
            accumulated: "hello".to_string(),
            tool_lines: vec![],
            tool_count: 0,
            thinking_steps: 0,
            commentary_notes: 0,
            thinking_tokens: 0,
            thinking_active: false,
            pending_commentary: String::new(),
            inherited_preview: None,
            defer_collapse: false,
            start: std::time::Instant::now(),
            last_edit: std::time::Instant::now(),
            delivery: StreamDelivery::FinalDelivered,
            finished: true,
        };
        let f = s.fold_candidate().unwrap();
        assert_eq!(f.msg_id, "7");
        assert_eq!(f.text, "hello");
        // No flushed message (deleted / never sent) → None.
        s.msg_id = None;
        assert!(s.fold_candidate().is_none());
    }

    #[test]
    fn test_parse_reply_target() {
        assert_eq!(
            TelegramChannel::parse_reply_target("12345"),
            ("12345".to_string(), None)
        );
        assert_eq!(
            TelegramChannel::parse_reply_target("12345:67890"),
            ("12345".to_string(), Some("67890".to_string()))
        );
    }

    #[test]
    fn test_message_thread_id_in_reply_target() {
        // Simulates: chat.id = -100123456, message_thread_id = Some(42)
        let reply_target = if let Some(tid) = Some(42_i64) {
            format!("{}:{}", -100123456_i64, tid)
        } else {
            (-100123456_i64).to_string()
        };
        assert_eq!(reply_target, "-100123456:42");
        let (chat_id, thread_id) = TelegramChannel::parse_reply_target(&reply_target);
        assert_eq!(chat_id, "-100123456");
        assert_eq!(thread_id, Some("42".to_string()));
    }

    #[test]
    fn test_forward_attribution_user() {
        let msg = Message {
            message_id: 1,
            message_thread_id: None,
            from: None,
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("hello".into()),
            caption: None,
            photo: None,
            voice: None,
            audio: None,
            video: None,
            video_note: None,
            document: None,
            forward_from: Some(User {
                id: 42,
                username: Some("bob".into()),
                first_name: None,
            }),
            forward_from_chat: None,
            forward_sender_name: None,
            forward_date: Some(1_700_000_000),
            reply_to_message: None,
        };
        assert_eq!(
            TelegramChannel::format_forward_attribution(&msg),
            Some("[Forwarded from @bob] ".to_string())
        );
    }

    #[test]
    fn test_forward_attribution_channel() {
        let msg = Message {
            message_id: 1,
            message_thread_id: None,
            from: None,
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("news".into()),
            caption: None,
            photo: None,
            voice: None,
            audio: None,
            video: None,
            video_note: None,
            document: None,
            forward_from: None,
            forward_from_chat: Some(Chat {
                id: -1_001_234_567_890_i64,
                kind: "channel".into(),
                username: Some("dailynews".into()),
                title: Some("Daily News".into()),
            }),
            forward_sender_name: None,
            forward_date: Some(1_700_000_000),
            reply_to_message: None,
        };
        assert_eq!(
            TelegramChannel::format_forward_attribution(&msg),
            Some("[Forwarded from channel: Daily News] ".to_string())
        );
    }

    #[test]
    fn test_forward_attribution_hidden_sender() {
        let msg = Message {
            message_id: 1,
            message_thread_id: None,
            from: None,
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("secret".into()),
            caption: None,
            photo: None,
            voice: None,
            audio: None,
            video: None,
            video_note: None,
            document: None,
            forward_from: None,
            forward_from_chat: None,
            forward_sender_name: Some("Hidden User".into()),
            forward_date: Some(1_700_000_000),
            reply_to_message: None,
        };
        assert_eq!(
            TelegramChannel::format_forward_attribution(&msg),
            Some("[Forwarded from Hidden User] ".to_string())
        );
    }

    #[test]
    fn test_forward_attribution_none() {
        let msg = Message {
            message_id: 1,
            message_thread_id: None,
            from: Some(User {
                id: 1,
                username: Some("alice".into()),
                first_name: None,
            }),
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("hello".into()),
            caption: None,
            photo: None,
            voice: None,
            audio: None,
            video: None,
            video_note: None,
            document: None,
            forward_from: None,
            forward_from_chat: None,
            forward_sender_name: None,
            forward_date: None,
            reply_to_message: None,
        };
        assert_eq!(TelegramChannel::format_forward_attribution(&msg), None);
    }

    #[test]
    fn test_bot_mention_spans() {
        let ch = TelegramChannel::new(make_config());
        // Set bot username directly in the Arc<Mutex<>>.
        *ch.bot_username.lock() = Some("mybot".to_string());

        // Direct mention: "@mybot" at indices [7, 12) in "Hello @mybot how are you?"
        let text = "Hello @mybot how are you?";
        let spans = ch.find_bot_mention_spans(text);
        assert_eq!(spans, vec![(7, 12)]); // [7, 12) = "mybot"

        // Not a mention (alphanumeric before @).
        let text2 = "email@mybot.com";
        let spans2 = ch.find_bot_mention_spans(text2);
        assert!(spans2.is_empty());

        // Strip mentions.
        let text3 = "Hey @mybot what's up?";
        let stripped = ch.strip_bot_mentions(text3);
        assert!(!stripped.contains("@mybot"));
        assert!(stripped.contains("Hey"));
    }

    #[test]
    fn test_dedup() {
        let dedup = DedupState::new();
        assert!(!dedup.check_and_record("msg1")); // new → false (not seen before)
        assert!(dedup.check_and_record("msg1")); // duplicate → true (already seen)
        assert!(!dedup.check_and_record("msg2")); // new → false (not seen before)
    }

    #[test]
    fn test_message_chunking() {
        use crate::channels::message::LenUnit;
        let chunks =
            crate::channels::message::split_message_chunk("short", 10, LenUnit::Codepoints);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "short");

        let long = "a".repeat(5000);
        let chunks = crate::channels::message::split_message_chunk(&long, 100, LenUnit::Codepoints);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 100));
    }

    #[test]
    fn test_utf16_chunking_emoji() {
        use crate::channels::message::{LenUnit, split_message_chunk};
        // Build 2100 codepoints of emoji (each is 2 UTF-16 units = 4200 units total)
        let emoji_text = "😀".repeat(2100);
        assert_eq!(emoji_text.chars().count(), 2100);
        assert_eq!(emoji_text.encode_utf16().count(), 4200);

        let chunks = split_message_chunk(&emoji_text, 4096, LenUnit::Utf16Units);
        assert!(chunks.len() > 1, "must split when UTF-16 exceeds limit");
        for c in &chunks {
            assert!(
                c.encode_utf16().count() <= 4096,
                "each chunk must fit Telegram UTF-16 limit"
            );
        }
    }

    #[test]
    fn test_normalize_markdown_tables() {
        // No blank line before table → should insert one.
        let input = "Here are the results:\n| A | B |\n| --- | --- |\n| 1 | 2 |";
        let out = TelegramChannel::normalize_markdown_tables(input);
        assert!(
            out.contains("Here are the results:\n\n| A | B |"),
            "expected blank line before table, got:\n{out}"
        );

        // Already has a blank line → unchanged.
        let input2 = "Intro:\n\n| A | B |\n| --- | --- |\n| 1 | 2 |";
        let out2 = TelegramChannel::normalize_markdown_tables(input2);
        assert_eq!(out2, input2);

        // Table at start of message → no blank needed.
        let input3 = "| A | B |\n| --- | --- |\n| 1 | 2 |";
        let out3 = TelegramChannel::normalize_markdown_tables(input3);
        assert_eq!(out3, input3);

        // Multiple tables in one message.
        let input4 = "Text1\n| A |\n| - |\n| x |\n\nText2\n| B |\n| - |\n| y |";
        let out4 = TelegramChannel::normalize_markdown_tables(input4);
        assert!(out4.contains("Text1\n\n| A |"), "first table missing blank");
        assert!(
            out4.contains("Text2\n\n| B |"),
            "second table missing blank"
        );
    }
}
