//! TelegramChannel — the main bot adapter for the Telegram Bot API.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::channel::TelegramAccountConfig;
use crate::{Channel, ChannelMessage, DedupState, ProcessingStatus};

use super::markdown::markdown_to_telegram_html;
use super::types::{Chat, GetUpdatesResponse, Message, SendChatActionRequest, SendMessageRequest};

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_MESSAGE_LENGTH: usize = 4096;
const CONTINUATION_OVERHEAD: usize = 30;

// ── TelegramChannel ────────────────────────────────────────────────────────────

/// Entry in the debounce buffer for merging rapid consecutive messages from the same sender.
struct DebounceEntry {
    sender: String,
    reply_target: String,
    contents: Vec<String>,
    files: Vec<crate::channels::FileAttachment>,
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
    /// Directory for persisting state (e.g. Telegram update offset).
    data_dir: std::path::PathBuf,
    /// Shared HTTP client with connection pool.
    http: reqwest::Client,
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
            data_dir: directories::ProjectDirs::from("", "", "myclaw")
                .map(|d| d.data_dir().to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from(".myclaw")),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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

    async fn send_raw(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
        reply_markup: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();
        let html_text = markdown_to_telegram_html(text);

        // Try sending with HTML parse_mode first.
        let req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            text: html_text.clone(),
            parse_mode: Some("HTML".to_string()),
            reply_markup,
        };
        let resp = client
            .post(self.api_url("sendMessage"))
            .json(&req)
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
            // Retry once
            let resp2 = client
                .post(self.api_url("sendMessage"))
                .json(&req)
                .send()
                .await?;
            if !resp2.status().is_success() {
                let status = resp2.status();
                let body_text = resp2.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "Telegram API error after 429 retry: {} {}",
                    status,
                    body_text
                ));
            }
            let resp_json: serde_json::Value = resp2.json().await?;
            let msg_id = resp_json
                .get("result")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_i64());
            return Ok(msg_id);
        }

        if resp.status().is_success() {
            let resp_json: serde_json::Value = resp.json().await?;
            let msg_id = resp_json
                .get("result")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_i64());
            return Ok(msg_id);
        }

        // HTML parse failed — fall back to plain text.
        let html_status = resp.status();
        let html_body = resp.text().await.unwrap_or_default();
        warn!(
            "sendMessage with HTML parse_mode failed (status={html_status}, body={html_body}), \
             falling back to plain text"
        );

        // Ensure plain text fits Telegram's limit (truncate if necessary).
        // Telegram measures in UTF-16 code units — emoji counts as 2.
        let plain_units = text.encode_utf16().count();
        let plain_text = if plain_units > MAX_MESSAGE_LENGTH {
            warn!(
                original_units = plain_units,
                limit = MAX_MESSAGE_LENGTH,
                "plain text exceeds Telegram limit, truncating"
            );
            // Reserve room for the suffix so that truncated body + suffix
            // still fits within MAX_MESSAGE_LENGTH (Telegram counts UTF-16
            // units; the suffix is ASCII so its char count == UTF-16 units).
            const TRUNCATION_SUFFIX: &str = "\n\n[... message truncated ...]";
            let suffix_units = TRUNCATION_SUFFIX.encode_utf16().count();
            let body_limit = MAX_MESSAGE_LENGTH.saturating_sub(suffix_units);
            let mut acc = 0usize;
            let mut end_byte = text.len();
            for (i, ch) in text.char_indices() {
                let cost = ch.len_utf16();
                if acc + cost > body_limit {
                    end_byte = i;
                    break;
                }
                acc += cost;
            }
            let mut truncated = text[..end_byte].to_string();
            truncated.push_str(TRUNCATION_SUFFIX);
            truncated
        } else {
            text.to_string()
        };

        let fallback_req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            text: plain_text,
            parse_mode: None,
            reply_markup: None,
        };
        let fallback_resp = client
            .post(self.api_url("sendMessage"))
            .json(&fallback_req)
            .send()
            .await?;

        if !fallback_resp.status().is_success() {
            let status = fallback_resp.status();
            let body = fallback_resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessage failed: status={status}, body={body}");
        }
        let resp_json: serde_json::Value = fallback_resp.json().await?;
        let msg_id = resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64());
        Ok(msg_id)
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
        let html_text = markdown_to_telegram_html(text);

        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": html_text,
            "parse_mode": "HTML",
        });

        let resp = client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            return Ok(true);
        }

        // HTML parse failed — fallback to plain text.
        if resp.status().as_u16() == 400 {
            let body_plain = serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": text,
            });
            let resp2 = client
                .post(self.api_url("editMessageText"))
                .json(&body_plain)
                .send()
                .await?;
            return Ok(resp2.status().is_success());
        }

        Ok(false)
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
    /// and dispatched as a single `ChannelMessage` after the debounce window
    /// expires. If debounce is disabled (`debounce_ms == 0`), the message is
    /// sent immediately via `tx`.
    async fn debounce_send(&self, mut msg: ChannelMessage, tx: mpsc::Sender<ChannelMessage>) {
        if self.debounce_ms == 0 {
            if let Err(e) = tx.send(msg).await {
                warn!("Telegram dispatch error: {e}");
            }
            return;
        }

        let key = format!("{}|{}", msg.sender, msg.reply_target);
        let debounce_ms = self.debounce_ms;
        let buffer = self.debounce_buffer.clone();
        let sender_key = key.clone();

        // Create timer task (starts sleeping immediately).
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
            let entry = buffer.lock().remove(&sender_key);
            if let Some(entry) = entry {
                let merged = entry.contents.join("\n");
                let channel_msg = ChannelMessage {
                    id: format!("debounced_{}", entry.first_ts),
                    sender: entry.sender,
                    reply_target: entry.reply_target,
                    content: merged,
                    timestamp: entry.first_ts,
                    thread_ts: None,
                    interruption_scope_id: None,
                    files: entry.files,
                };
                let _ = tx.send(channel_msg).await;
            }
        });

        // Lock the buffer and update/create entry.
        {
            let mut buf = self.debounce_buffer.lock();
            if let Some(entry) = buf.get_mut(&key) {
                // Merge into existing entry.
                if !msg.content.is_empty() {
                    entry.contents.push(msg.content);
                }
                if !msg.files.is_empty() {
                    entry.files.extend(msg.files.drain(..));
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
                        reply_target: msg.reply_target,
                        contents: if msg.content.is_empty() {
                            vec![]
                        } else {
                            vec![msg.content]
                        },
                        files: msg.files,
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
                typing
                    .iter()
                    .filter_map(|(target, started)| {
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
                    .send_raw(
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
    async fn poll_loop(&self, tx: mpsc::Sender<ChannelMessage>) {
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

                    let channel_msg = ChannelMessage {
                        id: update_id,
                        sender: sender_username
                            .map(|u| u.to_string())
                            .or_else(|| sender_id.map(|id| id.to_string()))
                            .unwrap_or_default(),
                        reply_target,
                        content: data,
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        thread_ts: cq
                            .message
                            .as_ref()
                            .and_then(|m| m.message_thread_id)
                            .map(|id| id.to_string()),
                        interruption_scope_id: None,
                        files: vec![],
                    };

                    // Send ack reaction if enabled.
                    if self.ack_reactions {
                        let chat_id = chat.id;
                        let msg_id = cq.message.as_ref().map(|m| m.message_id).unwrap_or(0);
                        self.ack_message(chat_id, msg_id).await;
                        self.pending_acks
                            .lock()
                            .entry(channel_msg.reply_target.clone())
                            .or_default()
                            .push((chat_id, msg_id));
                    }

                    if let Err(e) = tx.send(channel_msg.clone()).await {
                        warn!("Telegram dispatch callback error: {e}");
                    }
                    self.start_internal_typing(&channel_msg.reply_target);

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
                let has_forward = msg.forward_from.is_some()
                    || msg.forward_from_chat.is_some()
                    || msg.forward_sender_name.is_some();

                if !has_text && !has_photo && !has_forward {
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
                let mut files: Vec<crate::channels::FileAttachment> = Vec::new();

                // Handle photo messages: download the largest photo and save to temp file
                if let Some(photos) = &msg.photo {
                    if let Some(largest) = photos.last() {
                        match self.download_file_bytes(&largest.file_id).await {
                            Ok(data) => {
                                let temp_path = std::env::temp_dir()
                                    .join(format!("myclaw-tg-img-{}.png", uuid::Uuid::new_v4()));
                                if tokio::fs::write(&temp_path, &data).await.is_ok() {
                                    files.push(crate::channels::FileAttachment {
                                        path: temp_path.to_string_lossy().to_string(),
                                        file_name: Some(format!("photo-{}.png", largest.file_id)),
                                        mime_type: Some("image/png".to_string()),
                                        size_bytes: Some(data.len() as u64),
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
                                files.push(crate::channels::FileAttachment {
                                    path: temp_path.to_string_lossy().to_string(),
                                    file_name: Some(fname),
                                    mime_type: Some(mime),
                                    size_bytes: Some(data.len() as u64),
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
                                files.push(crate::channels::FileAttachment {
                                    path: temp_path.to_string_lossy().to_string(),
                                    file_name: Some(fname),
                                    mime_type: Some(mime),
                                    size_bytes: Some(data.len() as u64),
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

                let channel_msg = ChannelMessage {
                    id: update_id,
                    sender: sender_username
                        .map(|u| u.to_string())
                        .or_else(|| sender_id.map(|id| id.to_string()))
                        .unwrap_or_default(),
                    reply_target: if let Some(tid) = msg.message_thread_id {
                        format!("{}:{}", chat.id, tid)
                    } else {
                        chat.id.to_string()
                    },
                    content,
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    thread_ts: msg.message_thread_id.map(|id| id.to_string()),
                    interruption_scope_id: None,
                    files,
                };

                if self.debounce_ms > 0 {
                    // Clean up stale error reactions from previous interactions
                    let stale_status = self
                        .status_reactions
                        .lock()
                        .remove(&channel_msg.reply_target);
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
                            .entry(channel_msg.reply_target.clone())
                            .or_default()
                            .push((chat.id, msg.message_id));
                    }

                    let debounce_key =
                        format!("{}|{}", channel_msg.sender, channel_msg.reply_target);
                    let is_new = !self.debounce_buffer.lock().contains_key(&debounce_key);
                    if is_new {
                        self.start_internal_typing(&channel_msg.reply_target);
                    }
                    self.debounce_send(channel_msg, tx.clone()).await;
                } else {
                    // Clean up stale error reactions from previous interactions
                    let stale_status = self
                        .status_reactions
                        .lock()
                        .remove(&channel_msg.reply_target);
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
                            .entry(channel_msg.reply_target.clone())
                            .or_default()
                            .push((chat.id, msg.message_id));
                    }
                    if let Err(e) = tx.send(channel_msg.clone()).await {
                        warn!("Telegram dispatch error: {e}");
                    }
                    self.start_internal_typing(&channel_msg.reply_target);
                }
            }
        }
    }

    /// Split message content into chunks that fit Telegram's 4096-char limit.
    ///
    /// `markdown_to_telegram_html()` can significantly expand text (HTML escaping
    /// of `<>&"` chars, plus `<b>`, `<code>`, `<pre>` tags). A 4000-char Markdown
    /// chunk can easily exceed 4096 chars as HTML.
    ///
    /// Strategy:
    /// 1. Split by raw Markdown chars (conservative limit)
    /// 2. For each chunk, check if its HTML conversion exceeds 4096
    /// 3. If it does, re-split that chunk more aggressively using plain text limit
    fn chunk_for_telegram(content: &str) -> Vec<String> {
        use crate::channels::message::LenUnit;
        let html_overhead_per_chunk = 200; // conservative estimate for HTML expansion
        let raw_limit = MAX_MESSAGE_LENGTH
            .saturating_sub(CONTINUATION_OVERHEAD)
            .saturating_sub(html_overhead_per_chunk);

        // Telegram measures in UTF-16 code units; splitting by codepoints
        // under-counts emoji-heavy text and trips the 4096 limit.
        let raw_chunks =
            crate::channels::message::split_message_chunk(content, raw_limit, LenUnit::Utf16Units);

        let mut final_chunks = Vec::new();
        for chunk in raw_chunks {
            let html = markdown_to_telegram_html(&chunk);
            if html.encode_utf16().count() <= MAX_MESSAGE_LENGTH {
                final_chunks.push(chunk);
            } else {
                let plain_limit = MAX_MESSAGE_LENGTH
                    .saturating_sub(CONTINUATION_OVERHEAD)
                    .saturating_sub(CONTINUATION_OVERHEAD);
                let sub_chunks = crate::channels::message::split_message_chunk(
                    &chunk,
                    plain_limit,
                    LenUnit::Utf16Units,
                );
                final_chunks.extend(sub_chunks);
            }
        }

        final_chunks
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

        // Split into chunks that fit Telegram's 4096-char limit.
        // We use a conservative limit because markdown_to_telegram_html() can
        // expand the text (HTML escaping + tags). If a chunk's HTML exceeds
        // 4096 after conversion, we re-split it using plain text.
        let chunks = Self::chunk_for_telegram(&msg.content.text);

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
                .send_raw(&chat_id, &text, thread_id.as_deref(), markup)
                .await
            {
                Ok(Some(id)) => ids.push(crate::channels::MessageId::new(id.to_string())),
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

            let mime = file.meta.mime_type.as_deref().unwrap_or_default();
            let (method, part_name) = if mime.starts_with("image/") {
                ("sendPhoto", "photo")
            } else if mime.starts_with("audio/") {
                ("sendAudio", "audio")
            } else if mime.starts_with("video/") {
                ("sendVideo", "video")
            } else {
                ("sendDocument", "document")
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

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>> {
        // Lazily fetch bot username for mention detection.
        if let Some(username) = self.fetch_bot_username().await {
            info!("Telegram bot username: @{}", username);
            self.set_bot_username(username);
        }

        let (tx, rx) = mpsc::channel::<ChannelMessage>(100);
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
                // Remove 🤔 reaction from all tracked messages (send() already handles cleanup, but this is a safety net).
                let status_info = self.status_reactions.lock().remove(recipient);
                if let Some(msg_ids) = status_info {
                    for (chat_id, msg_id) in msg_ids {
                        let _ = self.remove_reaction(chat_id, msg_id, "🤔").await;
                    }
                }
            }
            ProcessingStatus::Error => {
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
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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

    // ── Markdown → Telegram HTML tests ──────────────────────────────────────

    #[test]
    fn test_md_bold() {
        assert_eq!(
            markdown_to_telegram_html("this is **bold** text"),
            "this is <b>bold</b> text"
        );
    }

    #[test]
    fn test_md_italic_asterisk() {
        assert_eq!(
            markdown_to_telegram_html("this is *italic* text"),
            "this is <i>italic</i> text"
        );
    }

    #[test]
    fn test_md_italic_underscore() {
        assert_eq!(
            markdown_to_telegram_html("this is _italic_ text"),
            "this is <i>italic</i> text"
        );
    }

    #[test]
    fn test_md_strikethrough() {
        assert_eq!(
            markdown_to_telegram_html("this is ~~deleted~~ text"),
            "this is <s>deleted</s> text"
        );
    }

    #[test]
    fn test_md_inline_code() {
        assert_eq!(
            markdown_to_telegram_html("use `println!()` for output"),
            "use <code>println!()</code> for output"
        );
    }

    #[test]
    fn test_md_code_block_plain() {
        let input = "```\nfn main() {\n    println!(\"hi\");\n}\n```";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<pre>fn main() {\n    println!(&quot;hi&quot;);\n}</pre>"
        );
    }

    #[test]
    fn test_md_code_block_with_lang() {
        let input = "```rust\nfn main() {}\n```";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<pre><code class=\"language-rust\">fn main() {}</code></pre>"
        );
    }

    #[test]
    fn test_md_link() {
        assert_eq!(
            markdown_to_telegram_html("[Rust](https://rust-lang.org)"),
            "<a href=\"https://rust-lang.org\">Rust</a>"
        );
    }

    #[test]
    fn test_md_heading() {
        assert_eq!(
            markdown_to_telegram_html("# Hello World\nSome text"),
            "<b>Hello World</b>\nSome text"
        );
    }

    #[test]
    fn test_md_blockquote() {
        assert_eq!(
            markdown_to_telegram_html("> important note"),
            "❝ important note"
        );
    }

    #[test]
    fn test_md_horizontal_rule() {
        assert_eq!(markdown_to_telegram_html("---"), "───");
        assert_eq!(markdown_to_telegram_html("***"), "───");
    }

    #[test]
    fn test_md_html_escape_in_plain_text() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn test_md_no_formatting() {
        let input = "just plain text, no markup";
        assert_eq!(markdown_to_telegram_html(input), input);
    }

    #[test]
    fn test_md_mixed_formatting() {
        let input = "**bold** and *italic* and `code`";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<b>bold</b> and <i>italic</i> and <code>code</code>"
        );
    }

    #[test]
    fn test_md_formatting_not_inside_code_block() {
        let input = "```text\n**not bold** and *not italic*\n```";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<pre>**not bold** and *not italic*</pre>"
        );
    }

    #[test]
    fn test_md_formatting_not_inside_inline_code() {
        assert_eq!(
            markdown_to_telegram_html("`**not bold**`"),
            "<code>**not bold**</code>"
        );
    }

    #[test]
    fn test_md_unclosed_bold_closed_at_end() {
        assert_eq!(
            markdown_to_telegram_html("start **never closed"),
            "start <b>never closed</b>"
        );
    }

    #[test]
    fn test_md_multiline_heading() {
        let input = "# First\n## Second\n### Third";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<b>First</b>\n<b>Second</b>\n<b>Third</b>"
        );
    }

    #[test]
    fn test_md_complex_message() {
        let input = "\
**Summary**

Here is some `inline code` and a [link](https://example.com).

```python
print('hello')
```

> A blockquote";

        let expected = "\
<b>Summary</b>

Here is some <code>inline code</code> and a <a href=\"https://example.com\">link</a>.

<pre><code class=\"language-python\">print('hello')</code></pre>

❝ A blockquote";

        assert_eq!(markdown_to_telegram_html(input), expected);
    }
}
