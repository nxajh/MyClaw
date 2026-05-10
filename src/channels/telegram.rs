//! Telegram Bot API channel adapter.
//!
//! Implements the [`Channel`] trait for the Telegram Bot API.
//!
//! # Features
//!
//! - Long-poll `getUpdates` for incoming messages
//! - Send text messages via `sendMessage` with HTML formatting
//! - Message chunking (Telegram 4096 char limit) + 429 rate-limit retry
//! - Typing indicators with circuit breaker (2-failure + 60s TTL)
//! - Allowed-user filtering + @mention / reply_to_bot detection in groups
//! - Message dedup + Thread/Topic support
//! - Ack reactions (👀 on receive) + Status reactions (🤔 thinking, ❌ error)
//! - CallbackQuery handling (button → text + answerCallbackQuery)
//! - Inbound debounce (merge rapid consecutive messages)
//! - Stall watchdog (notify when thinking for too long)
//! - Photo/image download + forward attribution

#![allow(dead_code)]

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{Channel, ChannelMessage, DedupState, SendMessage, ProcessingStatus};
use crate::config::channel::TelegramConfig;

// ── Constants ─────────────────────────────────────────────────────────────────

const BOT_BIND_COMMAND: &str = "/bind";
const MAX_MESSAGE_LENGTH: usize = 4096;
const CONTINUATION_OVERHEAD: usize = 30;

// ── Markdown → Telegram HTML conversion ──────────────────────────────────────

/// Escape HTML special characters for Telegram's HTML parse mode.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Convert LLM Markdown output to Telegram-supported HTML.
///
/// Supports: bold, italic, strikethrough, inline code, code blocks (with optional language),
/// headings, links, blockquotes, and horizontal rules.
///
/// Formatting inside code blocks and inline code is preserved as-is (no nested parsing).
pub fn markdown_to_telegram_html(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() * 2);

    // Tracks which inline formatting tags are currently open.
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;

    let chars: Vec<char> = markdown.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // ── Fenced code block (```) ─────────────────────────────────────
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            // Collect the optional language identifier (e.g. "rust", "python").
            let mut lang = String::new();
            let mut j = i + 3;
            while j < len && chars[j] != '\n' {
                lang.push(chars[j]);
                j += 1;
            }
            // Skip the newline after the opening fence.
            if j < len && chars[j] == '\n' {
                j += 1;
            }
            // Find the closing fence.
            let start = j;
            while j < len {
                if j + 2 < len && chars[j] == '`' && chars[j + 1] == '`' && chars[j + 2] == '`' {
                    break;
                }
                j += 1;
            }
            let mut code: String = chars[start..j].iter().collect();
            // Trim exactly one trailing newline (common in fenced blocks).
            if code.ends_with('\n') {
                code.pop();
            }
            let escaped = escape_html(&code);
            let trimmed_lang = lang.trim();
            // Treat empty or "text" as no language.
            let has_lang = !trimmed_lang.is_empty() && trimmed_lang != "text";
            if !has_lang {
                out.push_str(&format!("<pre>{}</pre>", escaped));
            } else {
                out.push_str(&format!(
                    "<pre><code class=\"language-{}\">{}</code></pre>",
                    trimmed_lang, escaped
                ));
            }
            // Advance past the closing fence.
            i = if j + 3 <= len { j + 3 } else { len };
            continue;
        }

        // ── Inline code (`) ─────────────────────────────────────────────
        if chars[i] == '`' {
            let end = chars[i + 1..]
                .iter()
                .position(|&c| c == '`')
                .map(|p| i + 1 + p);
            if let Some(e) = end {
                let code: String = chars[i + 1..e].iter().collect();
                out.push_str(&format!("<code>{}</code>", escape_html(&code)));
                i = e + 1;
                continue;
            }
            // No closing backtick — treat as literal.
            out.push('`');
            i += 1;
            continue;
        }

        // ── Headings (# …) → bold ───────────────────────────────────────
        if chars[i] == '#' {
            let mut j = i;
            while j < len && chars[j] == '#' {
                j += 1;
            }
            // Must have a space after the hashes, and be at line start.
            if j < len && chars[j] == ' ' && (i == 0 || chars[i - 1] == '\n') {
                // Skip leading space.
                j += 1;
                let line_start = j;
                while j < len && chars[j] != '\n' {
                    j += 1;
                }
                let heading_text: String = chars[line_start..j].iter().collect();
                out.push_str(&format!("<b>{}</b>", escape_html(heading_text.trim())));
                if j < len {
                    out.push('\n');
                    j += 1;
                }
                i = j;
                continue;
            }
        }

        // ── Blockquote (> …) ────────────────────────────────────────────
        if chars[i] == '>' && (i == 0 || chars[i - 1] == '\n') {
            let mut j = i + 1;
            if j < len && chars[j] == ' ' {
                j += 1;
            }
            let line_start = j;
            while j < len && chars[j] != '\n' {
                j += 1;
            }
            let quote_text: String = chars[line_start..j].iter().collect();
            out.push_str(&format!("❝ {}", escape_html(&quote_text)));
            if j < len {
                out.push('\n');
                j += 1;
            }
            i = j;
            continue;
        }

        // ── Horizontal rule (---, ***, ___) → ───────────────────────────
        if (chars[i] == '-' || chars[i] == '*' || chars[i] == '_')
            && (i == 0 || chars[i - 1] == '\n')
        {
            let c = chars[i];
            let mut j = i;
            while j < len && chars[j] == c {
                j += 1;
            }
            // Must be at least 3 repeats, followed by newline or EOF, with only whitespace.
            if j - i >= 3 {
                let rest: String = chars[i..j].iter().collect();
                if rest.chars().all(|ch| ch == c || ch == ' ' || ch == '\t')
                    && (j >= len || chars[j] == '\n')
                {
                    out.push_str("───");
                    if j < len {
                        out.push('\n');
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        // ── Links [text](url) ───────────────────────────────────────────
        if chars[i] == '[' {
            if let Some(bracket_end) = chars[i + 1..].iter().position(|&c| c == ']') {
                let real_bracket = i + 1 + bracket_end;
                if real_bracket + 1 < len && chars[real_bracket + 1] == '(' {
                    if let Some(paren_end) = chars[real_bracket + 2..]
                        .iter()
                        .position(|&c| c == ')')
                    {
                        let real_paren = real_bracket + 2 + paren_end;
                        let link_text: String = chars[i + 1..real_bracket].iter().collect();
                        let link_url: String =
                            chars[real_bracket + 2..real_paren].iter().collect();
                        out.push_str(&format!(
                            "<a href=\"{}\">{}</a>",
                            escape_html(&link_url),
                            escape_html(&link_text)
                        ));
                        i = real_paren + 1;
                        continue;
                    }
                }
            }
        }

        // ── Strikethrough (~~) ──────────────────────────────────────────
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if strike {
                out.push_str("</s>");
                strike = false;
            } else {
                out.push_str("<s>");
                strike = true;
            }
            i += 2;
            continue;
        }

        // ── Bold (**) ───────────────────────────────────────────────────
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if bold {
                out.push_str("</b>");
                bold = false;
            } else {
                out.push_str("<b>");
                bold = true;
            }
            i += 2;
            continue;
        }

        // ── Italic (* or _) ─────────────────────────────────────────────
        // Must be preceded by whitespace/start and followed by non-whitespace,
        // or preceded by non-whitespace and followed by whitespace/end.
        if (chars[i] == '*' || chars[i] == '_') && !bold {
            let prev_ok = i == 0
                || chars[i - 1].is_whitespace()
                || chars[i - 1].is_ascii_punctuation();
            let next_ok = i + 1 < len && !chars[i + 1].is_whitespace();

            if italic {
                // Closing: must be preceded by non-whitespace.
                let prev_non_ws = i > 0 && !chars[i - 1].is_whitespace();
                if prev_non_ws {
                    out.push_str("</i>");
                    italic = false;
                    i += 1;
                    continue;
                }
            } else if prev_ok && next_ok {
                out.push_str("<i>");
                italic = true;
                i += 1;
                continue;
            }
        }

        // ── Plain text (escape HTML) ────────────────────────────────────
        match chars[i] {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
        i += 1;
    }

    // Close any tags still open at the end.
    if strike {
        out.push_str("</s>");
    }
    if italic {
        out.push_str("</i>");
    }
    if bold {
        out.push_str("</b>");
    }

    out
}

// ── Telegram types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct Update {
    #[serde(default)]
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    edited_message: Option<Message>,
    #[serde(default)]
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
struct PhotoSize {
    #[serde(default)]
    file_id: String,
    #[serde(default)]
    file_unique_id: String,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Message {
    #[serde(default)]
    message_id: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    forward_from: Option<User>,
    #[serde(default)]
    forward_from_chat: Option<Chat>,
    #[serde(default)]
    forward_sender_name: Option<String>,
    #[serde(default)]
    forward_date: Option<i64>,
    #[serde(default)]
    reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Clone, Deserialize)]
struct User {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Chat {
    #[serde(default)]
    id: i64,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    id: String,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SendMessageRequest {
    #[serde(rename = "chat_id")]
    chat_id: String,
    #[serde(rename = "message_thread_id", skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(rename = "parse_mode", skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SendChatActionRequest {
    #[serde(rename = "chat_id")]
    chat_id: String,
    #[serde(rename = "message_thread_id", skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<String>,
    #[serde(rename = "action")]
    action: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GetUpdatesResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}

// ── TelegramChannel ────────────────────────────────────────────────────────────

/// Entry in the debounce buffer for merging rapid consecutive messages from the same sender.
struct DebounceEntry {
    sender: String,
    reply_target: String,
    contents: Vec<String>,
    images: Option<Vec<String>>,
    first_ts: u64,
    timer: tokio::task::JoinHandle<()>,
}

/// Reaction tracker: reply_target → Vec<(chat_id, message_id)>.
type ReactionTracker = Arc<Mutex<std::collections::HashMap<String, Vec<(i64, i64)>>>>;

#[derive(Clone)]
pub struct TelegramChannel {
    bot_token: String,
    allowed_users: Arc<RwLock<Vec<String>>>,
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
    /// Shared HTTP client with connection pool.
    http: reqwest::Client,
    /// Whether we've logged the empty allowed_users warning.
    empty_allowed_warned: Arc<std::sync::atomic::AtomicBool>,
}

impl TelegramChannel {
    pub fn new(config: TelegramConfig) -> Self {
        let allowed = Self::normalize_allowed_users(config.allowed_users.clone());

        Self {
            bot_token: config.bot_token.clone(),
            allowed_users: Arc::new(RwLock::new(allowed)),
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
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            empty_allowed_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.bot_token, method)
    }

    fn http_client(&self) -> &reqwest::Client {
        &self.http
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

    fn is_user_allowed(&self, username: Option<&str>, user_id: Option<i64>) -> bool {
        let users = self.allowed_users.read().unwrap();
        if users.is_empty() {
            if !self.empty_allowed_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!("allowed_users is empty — all users will be rejected. Add users or '*' to allow all.");
            }
            return false;
        }
        if users.iter().any(|u| u == "*") {
            return true;
        }
        if let Some(un) = username {
            if users.iter().any(|u| u == &Self::normalize_identity(un)) {
                return true;
            }
        }
        if let Some(uid) = user_id {
            if users.iter().any(|u| u == &uid.to_string()) {
                return true;
            }
        }
        false
    }

    async fn fetch_bot_username(&self) -> Option<String> {
        let client = self.http_client();
        let resp = client
            .get(self.api_url("getMe"))
            .send()
            .await
            .ok()?;
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
            let name = fwd.username
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
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();
        let html_text = markdown_to_telegram_html(text);

        // Try sending with HTML parse_mode first.
        let req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            text: html_text.clone(),
            parse_mode: Some("HTML".to_string()),
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
            let msg_id = resp_json.get("result")
                .and_then(|r| r.get("message_id"))
                .and_then(|m| m.as_i64());
            return Ok(msg_id);
        }

        if resp.status().is_success() {
            let resp_json: serde_json::Value = resp.json().await?;
            let msg_id = resp_json.get("result")
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
        let plain_text = if text.chars().count() > MAX_MESSAGE_LENGTH {
            warn!(
                original_chars = text.chars().count(),
                limit = MAX_MESSAGE_LENGTH,
                "plain text exceeds Telegram limit, truncating"
            );
            let end_byte = text
                .char_indices()
                .nth(MAX_MESSAGE_LENGTH)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            let mut truncated = text[..end_byte].to_string();
            truncated.push_str("\n\n[... message truncated ...]");
            truncated
        } else {
            text.to_string()
        };

        let fallback_req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            text: plain_text,
            parse_mode: None,
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
        let msg_id = resp_json.get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64());
        Ok(msg_id)
    }

    /// Delete a message by chat_id and message_id.
    async fn delete_message(&self, chat_id: i64, message_id: i64) -> anyhow::Result<()> {
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
    async fn download_file_base64(&self, file_id: &str) -> anyhow::Result<String> {
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
        let download_url = format!(
            "{}/file/bot{}/{}",
            self.api_base, self.bot_token, file_path
        );
        let file_resp = client.get(&download_url).send().await?;
        if !file_resp.status().is_success() {
            anyhow::bail!(
                "file download failed: status={}",
                file_resp.status()
            );
        }
        let bytes = file_resp.bytes().await?;

        // Step 3: Encode to base64.
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
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
            warn!("Failed to send ack reaction to message {} in chat {}: {e}", message_id, chat_id);
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
            warn!("Failed to remove ack reaction from message {} in chat {}: {e}", message_id, chat_id);
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
    async fn remove_reaction(&self, chat_id: i64, message_id: i64, _emoji: &str) -> anyhow::Result<()> {
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
        self.typing_started_at.lock().insert(recipient.to_string(), std::time::Instant::now());

        let bot_token = self.bot_token.clone();
        let api_base = self.api_base.clone();
        let recipient_key = recipient.to_string();
        let recipient_key_clone = recipient_key.clone();

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
                    warn!("Telegram typing TTL exceeded ({}s) for {}", max_duration.as_secs(), recipient_key);
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
                            warn!("Telegram typing circuit breaker tripped after {consecutive_failures} consecutive failures for {}: {e}", recipient_key);
                            break;
                        }
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
        });
        tasks.insert(recipient_key_clone, handle);
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
    async fn debounce_send(&self, msg: ChannelMessage, tx: mpsc::Sender<ChannelMessage>) {
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
                    channel: "telegram".to_string(),
                    timestamp: entry.first_ts,
                    thread_ts: None,
                    interruption_scope_id: None,
                    attachments: vec![],
                    image_urls: None,
                    image_base64: entry.images,
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
                if let Some(imgs) = msg.image_base64 {
                    if let Some(ref mut existing) = entry.images {
                        existing.extend(imgs);
                    } else {
                        entry.images = Some(imgs);
                    }
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
                        images: msg.image_base64,
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

        let check_interval = std::time::Duration::from_secs(5);
        let mut interval = tokio::time::interval(check_interval);
        let stall_timeout = std::time::Duration::from_secs(self.stall_timeout_secs);
        // Track which recipients we've already sent stall notice for (to avoid spamming).
        let notified: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));

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
                {
                    let mut n = notified.lock();
                    if n.contains(&target) {
                        continue; // Already notified
                    }
                    n.insert(target.clone());
                }

                let secs = elapsed.as_secs();
                warn!("Stall detected for {target}: typing for {secs}s without response");

                let (chat_id, thread_id) = Self::parse_reply_target(&target);
                let chat_id_i64: i64 = chat_id.parse().unwrap_or(0);
                match self
                    .send_raw(
                        &chat_id,
                        &format!("🤔 还在思考中... (已等待 {secs}s)"),
                        thread_id.as_deref(),
                    )
                    .await
                {
                    Ok(Some(stall_msg_id)) => {
                        self.stall_messages.lock()
                            .entry(target.clone())
                            .or_default()
                            .push((chat_id_i64, stall_msg_id));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Failed to send stall notice: {e}");
                    }
                }
            }

            // Clean up notified set for recipients that are no longer typing.
            let typing_keys: std::collections::HashSet<String> =
                self.typing_started_at.lock().keys().cloned().collect();
            notified.lock().retain(|k| typing_keys.contains(k));
        }
    }

    /// The actual long-poll loop. Runs until channel is closed.
    async fn poll_loop(&self, tx: mpsc::Sender<ChannelMessage>) {
        let mut offset: i64 = 0;

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

            for update in updates.into_iter() {
                offset = update.update_id + 1;

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

                    // User filtering
                    let sender_username = from_user.username.as_deref();
                    let sender_id = Some(from_user.id);
                    if !self.is_user_allowed(sender_username, sender_id) {
                        continue;
                    }

                    // Dedup using callback query ID
                    let update_id = format!("cq_{}", cq.id);
                    if self.dedup.check_and_record(&update_id) {
                        continue;
                    }

                    let reply_target = if let Some(tid) = cq.message.as_ref().and_then(|m| m.message_thread_id) {
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
                        channel: "telegram".to_string(),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        thread_ts: cq.message.as_ref().and_then(|m| m.message_thread_id).map(|id| id.to_string()),
                        interruption_scope_id: None,
                        attachments: vec![],
                        image_urls: None,
                        image_base64: None,
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

                if !self.is_user_allowed(sender_username, sender_id) {
                    continue;
                }

                if Self::is_group_message(&chat) && self.mention_only {
                    let text = msg.text.as_deref().unwrap_or("");
                    if !self.contains_bot_mention(text) && !self.is_reply_to_bot(&msg) {
                        continue;
                    }
                }

                let update_id = update.update_id.to_string();
                if self.dedup.check_and_record(&update_id) {
                    // Already seen this update — skip
                    continue;
                }

                let mut content = self.parse_message_content(&msg);
                let mut image_base64: Option<Vec<String>> = None;

                // Handle photo messages: download the largest photo as base64
                if let Some(photos) = &msg.photo {
                    if let Some(largest) = photos.last() {
                        match self.download_file_base64(&largest.file_id).await {
                            Ok(b64) => {
                                image_base64 = Some(vec![b64]);
                            }
                            Err(e) => {
                                warn!("Telegram download failed for photo {}: {e}", largest.file_id);
                            }
                        }
                    }
                    // Use caption if available, otherwise default to "[图片]"
                    if content.is_empty() {
                        content = msg
                            .caption
                            .clone()
                            .unwrap_or_else(|| "[图片]".to_string());
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
                    channel: "telegram".to_string(),
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    thread_ts: msg.message_thread_id.map(|id| id.to_string()),
                    interruption_scope_id: None,
                    attachments: vec![],
                    image_urls: None,
                    image_base64,
                };

                if self.debounce_ms > 0 {
                    // Clean up stale error reactions from previous interactions
                    let stale_status = self.status_reactions.lock().remove(&channel_msg.reply_target);
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

                    let debounce_key = format!("{}|{}", channel_msg.sender, channel_msg.reply_target);
                    let is_new = !self.debounce_buffer.lock().contains_key(&debounce_key);
                    if is_new {
                        self.start_internal_typing(&channel_msg.reply_target);
                    }
                    self.debounce_send(channel_msg, tx.clone()).await;
                } else {
                    // Clean up stale error reactions from previous interactions
                    let stale_status = self.status_reactions.lock().remove(&channel_msg.reply_target);
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
        let html_overhead_per_chunk = 200; // conservative estimate for HTML expansion
        let raw_limit = MAX_MESSAGE_LENGTH
            .saturating_sub(CONTINUATION_OVERHEAD)
            .saturating_sub(html_overhead_per_chunk);

        let raw_chunks = crate::channels::message::split_message_chunk(content, raw_limit);

        let mut final_chunks = Vec::new();
        for chunk in raw_chunks {
            let html = markdown_to_telegram_html(&chunk);
            if html.chars().count() <= MAX_MESSAGE_LENGTH {
                // HTML fits — send_raw will try HTML first, then plain fallback
                final_chunks.push(chunk);
            } else {
                // HTML exceeds limit — re-split this chunk more aggressively.
                // Use plain text limit that accounts for continuation suffix.
                let plain_limit = MAX_MESSAGE_LENGTH
                    .saturating_sub(CONTINUATION_OVERHEAD)
                    .saturating_sub(CONTINUATION_OVERHEAD); // double-subtract for safety
                let sub_chunks = crate::channels::message::split_message_chunk(&chunk, plain_limit);
                final_chunks.extend(sub_chunks);
            }
        }

        final_chunks
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let (chat_id, thread_id) = Self::parse_reply_target(&message.recipient);

        // Delete any stall watchdog messages before sending the real reply.
        let stall_msgs = self.stall_messages.lock().remove(&message.recipient);
        if let Some(msgs) = stall_msgs {
            for (chat_id, msg_id) in msgs {
                if let Err(e) = self.delete_message(chat_id, msg_id).await {
                    debug!("Failed to delete stall message {}: {e}", msg_id);
                }
            }
        }

        // Split into chunks that fit Telegram's 4096-char limit.
        // We use a conservative limit because markdown_to_telegram_html() can
        // expand the text (HTML escaping + tags). If a chunk's HTML exceeds
        // 4096 after conversion, we re-split it using plain text.
        let chunks = Self::chunk_for_telegram(&message.content);

        let count = chunks.len();
        let mut last_error = None;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let text = if count > 1 && i < count - 1 {
                format!("{}\n\n(continues...)", chunk)
            } else {
                chunk
            };
            if let Err(e) = self.send_raw(&chat_id, &text, thread_id.as_deref()).await {
                warn!("Failed to send chunk {}/{}: {}", i + 1, count, e);
                last_error = Some(e);
                // Continue trying subsequent chunks
            }
            // Throttle between chunks to avoid 429 rate limiting
            if i + 1 < count {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }

        // Stop typing indicator for this recipient now that the response is sent.
        self.stop_internal_typing(&message.recipient);

        // Remove ack reactions (👀) for all tracked messages.
        let ack_info = self.pending_acks.lock().remove(&message.recipient);
        if let Some(msg_ids) = ack_info {
            for (chat_id, msg_id) in msg_ids {
                self.remove_ack(chat_id, msg_id).await;
            }
        }

        // Clean up status reactions (🤔) for all tracked messages.
        let status_info = self.status_reactions.lock().remove(&message.recipient);
        if let Some(msg_ids) = status_info {
            for (chat_id, msg_id) in msg_ids {
                let _ = self.remove_reaction(chat_id, msg_id, "🤔").await;
            }
        }

        if let Some(e) = last_error {
            return Err(e);
        }
        Ok(())
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
                self.status_reactions.lock().insert(recipient.to_string(), status_ids);
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
    use super::*;

    fn make_config() -> TelegramConfig {
        TelegramConfig {
            bot_token: "test_token_123".into(),
            allowed_users: vec!["alice".into(), "123456".into()],
            mention_only: false,
            api_base: Some("https://api.telegram.org".into()),
            proxy_url: None,
            enabled: true,
            approval_timeout_secs: 120,
            ack_reactions: true,
            workspace_dir: None,
            debounce_ms: 0, // disabled in tests
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
        assert!(dedup.check_and_record("msg1"));  // duplicate → true (already seen)
        assert!(!dedup.check_and_record("msg2")); // new → false (not seen before)
    }

    #[test]
    fn test_message_chunking() {
        let chunks = crate::channels::message::split_message_chunk("short", 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "short");

        let long = "a".repeat(5000);
        let chunks = crate::channels::message::split_message_chunk(&long, 100);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 100));
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
