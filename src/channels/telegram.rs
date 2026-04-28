//! Telegram Bot API channel adapter.
//!
//! Implements the [`Channel`] trait for the Telegram Bot API.
//!
//! # Features (v1)
//!
//! - Long-poll `getUpdates` for incoming messages
//! - Send text messages via `sendMessage`
//! - Message chunking (Telegram 4096 char limit)
//! - Typing indicators (sendChatAction)
//! - Allowed-user filtering
//! - Message dedup
//! - @mention detection in groups
//!
//! # Not in v1
//!
//! - Streaming draft edits
//! - TTS voice messages
//! - Voice transcription
//! - Inline keyboard approvals
//! - File attachments

#![allow(dead_code)]

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{Channel, ChannelMessage, DedupState, SendMessage};
use crate::config::channel::TelegramConfig;

// ── Constants ─────────────────────────────────────────────────────────────────

const BOT_BIND_COMMAND: &str = "/bind";
const MAX_MESSAGE_LENGTH: usize = 4096;
const CONTINUATION_OVERHEAD: usize = 30;

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
struct Message {
    #[serde(default)]
    message_id: i64,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    forward_from: Option<User>,
    #[serde(default)]
    forward_from_chat: Option<Chat>,
    #[serde(default)]
    forward_sender_name: Option<String>,
    #[serde(default)]
    forward_date: Option<i64>,
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
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.bot_token, method)
    }

    fn http_client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
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
    ) -> anyhow::Result<()> {
        let client = self.http_client();
        let req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            text: text.to_string(),
            parse_mode: None,
        };
        let resp = client
            .post(self.api_url("sendMessage"))
            .json(&req)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessage failed: status={status}, body={body}");
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
}

impl TelegramChannel {
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

                let msg = match update.message {
                    Some(m) => m,
                    None => continue,
                };

                let chat = msg.chat.clone();
                let from = msg.from.clone();

                if msg.text.is_none()
                    && msg.forward_from.is_none()
                    && msg.forward_from_chat.is_none()
                    && msg.forward_sender_name.is_none()
                {
                    continue;
                }

                let sender_username = from.as_ref().and_then(|u| u.username.as_deref());
                let sender_id = from.as_ref().map(|u| u.id);

                if !self.is_user_allowed(sender_username, sender_id) {
                    continue;
                }

                if Self::is_group_message(&chat) && self.mention_only {
                    let text = msg.text.as_deref().unwrap_or("");
                    if !self.contains_bot_mention(text) {
                        continue;
                    }
                }

                let update_id = update.update_id.to_string();
                if self.dedup.check_and_record(&update_id) {
                    // Already seen this update — skip
                    continue;
                }

                let content = self.parse_message_content(&msg);

                let channel_msg = ChannelMessage {
                    id: update_id,
                    sender: sender_username
                        .map(|u| u.to_string())
                        .or_else(|| sender_id.map(|id| id.to_string()))
                        .unwrap_or_default(),
                    reply_target: chat.id.to_string(),
                    content,
                    channel: "telegram".to_string(),
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    thread_ts: None,
                    interruption_scope_id: None,
                    attachments: vec![],
                };

                if let Err(e) = tx.send(channel_msg).await {
                    warn!("Telegram dispatch error: {e}");
                }
            }
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let (chat_id, thread_id) = Self::parse_reply_target(&message.recipient);
        let chunks =
            crate::channels::message::split_message_chunk(&message.content, MAX_MESSAGE_LENGTH - CONTINUATION_OVERHEAD);

        let count = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let text = if count > 1 && i < count - 1 {
                format!("{}\n\n(continues...)", chunk)
            } else if count > 1 && i == 0 {
                format!("{}\n\n(continued)\n\n", chunk)
            } else {
                chunk
            };
            self.send_raw(&chat_id, &text, thread_id.as_deref()).await?;
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

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let (chat_id, thread_id) = Self::parse_reply_target(recipient);
        self.send_chat_action(&chat_id, thread_id.as_deref(), "typing")
            .await
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        // Telegram doesn't have a "stop typing" action.
        Ok(())
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
    fn test_forward_attribution_user() {
        let msg = Message {
            message_id: 1,
            from: None,
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("hello".into()),
            forward_from: Some(User {
                id: 42,
                username: Some("bob".into()),
                first_name: None,
            }),
            forward_from_chat: None,
            forward_sender_name: None,
            forward_date: Some(1_700_000_000),
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
            from: None,
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("news".into()),
            forward_from: None,
            forward_from_chat: Some(Chat {
                id: -1_001_234_567_890_i64,
                kind: "channel".into(),
                username: Some("dailynews".into()),
                title: Some("Daily News".into()),
            }),
            forward_sender_name: None,
            forward_date: Some(1_700_000_000),
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
            from: None,
            chat: Chat {
                id: 1,
                kind: "private".into(),
                username: None,
                title: None,
            },
            text: Some("secret".into()),
            forward_from: None,
            forward_from_chat: None,
            forward_sender_name: Some("Hidden User".into()),
            forward_date: Some(1_700_000_000),
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
            forward_from: None,
            forward_from_chat: None,
            forward_sender_name: None,
            forward_date: None,
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
}
