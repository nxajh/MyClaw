use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{debug, warn};

use crate::channels::shared::{InboundDebouncer, TypingKeepAlive, TypingParams};
use crate::config::channel::TelegramAccountConfig;
use crate::DedupState;

use super::TelegramChannel;
use super::super::types::{Chat, Message, SendChatActionRequest};

// ── Constants ─────────────────────────────────────────────────────────────────

pub(super) const RICH_MESSAGE_LENGTH: usize = 32768;
pub(super) const CONTINUATION_OVERHEAD: usize = 30;

/// Whether a failed `editMessageText` call is Telegram's benign "content
/// unchanged" rejection (issue #113) — the throttled streaming preview can
/// legitimately try to re-edit a message with identical content when the
/// underlying text hasn't changed since the last flush. This is not an
/// error worth a WARN; the edit is simply a no-op.
/// Body for the plain-text `sendMessage` fallback (#144): no `parse_mode`.
/// Any parse mode can 400 on unclosed Markdown entities (e.g. an odd number
/// of underscores) and turn the "guaranteed delivery" tier into a silent
/// drop, so the fallback sends unformatted text by design.
pub(super) fn plain_send_message_body(
    chat_id: &str,
    text: &str,
    thread_id: Option<&str>,
    reply_markup: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
    });
    if let Some(tid) = thread_id {
        body["message_thread_id"] = serde_json::Value::from(tid);
    }
    if let Some(markup) = reply_markup {
        body["reply_markup"] = markup.clone();
    }
    body
}

pub(super) fn is_edit_not_modified(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST && body.contains("message is not modified")
}

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
            typing: TypingKeepAlive::new(),
            ack_reactions: config.ack_reactions,
            pending_acks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            status_reactions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            debouncer: InboundDebouncer::new(config.debounce_ms, Some("Telegram")),
            stall_timeout_secs: config.stall_timeout_secs,
            typing_started_at: Arc::new(Mutex::new(std::collections::HashMap::new())),
            stall_messages: Arc::new(Mutex::new(std::collections::HashMap::new())),
            streaming_mode: config.streaming_mode,
            tts: config.tts,
            streaming_targets: Arc::new(Mutex::new(std::collections::HashSet::new())),
            base_dir: crate::config::default_base_dir(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            message_cache: Arc::new(Mutex::new(VecDeque::new())),
        };
        crate::channels::warn_if_locked_down(&ch);
        ch
    }

    /// Install the real configured base_dir (`telegram_offset` lives at
    /// `{base_dir}/telegram_offset`). `new()` defaults to
    /// `config::default_base_dir()` so this is only needed when the
    /// composition root's `AppConfig.base_dir` differs from that default.
    pub fn with_base_dir(mut self, base_dir: std::path::PathBuf) -> Self {
        self.base_dir = base_dir;
        self
    }

    pub(super) fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.bot_token, method)
    }

    pub(super) fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Return cached recent sent messages (message_id, text) for debugging
    /// and potential reply-chain context. Returns up to 100 most recent entries.
    fn get_cached_messages(&self, _chat_id: i64) -> Vec<(i64, String)> {
        self.message_cache.lock().iter().cloned().collect()
    }

    /// Push a message to the ring buffer cache (max 100 entries).
    pub(super) fn cache_message(&self, message_id: i64, text: String) {
        let mut cache = self.message_cache.lock();
        if cache.len() >= 100 {
            cache.pop_front();
        }
        cache.push_back((message_id, text));
    }

    /// Path to the file that persists the Telegram update offset.
    fn offset_path(&self) -> std::path::PathBuf {
        crate::config::telegram_offset_path(&self.base_dir)
    }

    /// Load the persisted update offset from disk.
    /// Returns 0 when the file does not exist or is unreadable.
    pub(super) fn load_offset(&self) -> i64 {
        let path = self.offset_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => content.trim().parse().unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Persist the given offset to disk *before* processing the batch.
    /// This ensures that even if the process is killed mid-processing,
    /// a subsequent restart will not re-fetch already-seen updates.
    pub(super) fn persist_offset(&self, offset: i64) {
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

    pub(super) fn normalize_identity(value: &str) -> String {
        value.trim().trim_start_matches('@').to_string()
    }

    pub(super) fn normalize_allowed_users(users: Vec<String>) -> Vec<String> {
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
    pub(super) fn try_authorize(
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

    pub(super) async fn fetch_bot_username(&self) -> Option<String> {
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

    pub(super) fn set_bot_username(&self, username: String) {
        *self.bot_username.lock() = Some(username);
    }

    /// Find all @mention spans for the bot in text.
    pub(super) fn find_bot_mention_spans(&self, text: &str) -> Vec<(usize, usize)> {
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
    pub(super) fn strip_bot_mentions(&self, text: &str) -> String {
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
    pub(super) fn contains_bot_mention(&self, text: &str) -> bool {
        !self.find_bot_mention_spans(text).is_empty()
    }

    /// Check if the message is a reply to a message sent by this bot.
    pub(super) fn is_reply_to_bot(&self, msg: &Message) -> bool {
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

    pub(super) fn is_group_message(chat: &Chat) -> bool {
        chat.kind == "group" || chat.kind == "supergroup"
    }

    pub(super) fn format_forward_attribution(msg: &Message) -> Option<String> {
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

    pub(super) fn parse_reply_target(reply_target: &str) -> (String, Option<String>) {
        if let Some((chat_id, thread_id)) = reply_target.split_once(':') {
            (chat_id.to_string(), Some(thread_id.to_string()))
        } else {
            (reply_target.to_string(), None)
        }
    }

    /// Ensure a blank line before GFM table blocks so Telegram's markdown
    /// parser recognises them. Without a preceding blank line (or message
    /// start), the parser treats `| col | col |` as literal text.
    /// Escape a leading `#{1,6}` run when immediately followed by a digit
    /// (issue #114). CommonMark requires whitespace (or end of line) after
    /// the `#` run for a valid ATX heading, but Telegram's rich-message
    /// renderer parses more loosely and treats e.g. `#108` (no space) as a
    /// heading — a false positive on issue/PR references and other
    /// hash-prefixed numbers at the start of a line. A genuine heading is
    /// always `#` + space, so this narrow match never touches real
    /// headings (`# Title`, `## Notes`, ...).
    pub(super) fn escape_digit_heading_lookalikes(text: &str) -> String {
        let re = regex::Regex::new(r"(?m)^(#{1,6})(\d)").unwrap();
        re.replace_all(text, r"\$1$2").to_string()
    }

    pub(super) fn normalize_markdown_tables(text: &str) -> String {
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

    pub(super) async fn send_text(
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

        let normalized = Self::escape_digit_heading_lookalikes(&Self::normalize_markdown_tables(text));
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
    /// #144: no `parse_mode` — this is the last-resort tier whose contract is
    /// "guarantee delivery", and any parse mode (Markdown included) can 400 on
    /// unclosed entities and drop the message entirely. Rich rendering is
    /// `send_rich_message`'s job; if it failed, retrying with the same parsing
    /// risk defeats the fallback.
    async fn send_plain_message(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<&str>,
        reply_markup: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<i64>> {
        let client = self.http_client();
        let body = plain_send_message_body(chat_id, text, thread_id, reply_markup.as_ref());

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
    pub(crate) async fn delete_message_raw(&self, chat_id: i64, message_id: i64) -> anyhow::Result<()> {
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
    pub(super) async fn edit_message_text_raw(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<bool> {
        let client = self.http_client();

        let normalized = Self::escape_digit_heading_lookalikes(&Self::normalize_markdown_tables(text));
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
            if is_edit_not_modified(status, &body) {
                debug!("editMessageText: content unchanged, skipping (not an error)");
            } else {
                warn!("editMessageText failed: {status} {body}");
            }
            return Ok(false);
        }

        Ok(true)
    }

    /// Send a message using rich_message format (markdown rendering), no reply_markup.
    pub(crate) async fn send_rich_message_simple(
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
    pub(crate) async fn edit_message_rich(
        &self,
        chat_id: i64,
        message_id: i64,
        markdown: &str,
    ) -> anyhow::Result<bool> {
        let client = self.http_client();
        // issue #114: this feeds the same rich_message.markdown server-side
        // renderer as send_rich_message/edit_message_text_raw (which already
        // normalize) — the streaming preview (this fn's only caller) can
        // just as easily contain a leading "#108"-shaped line.
        let normalized =
            Self::escape_digit_heading_lookalikes(&Self::normalize_markdown_tables(markdown));
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
            if is_edit_not_modified(status, &body) {
                debug!("editMessageText(rich): content unchanged, skipping (not an error)");
            } else {
                warn!("editMessageText(rich) failed: {status} {body}");
            }
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
            if is_edit_not_modified(status, &body) {
                debug!("editMessageText(HTML): content unchanged, skipping (not an error)");
            } else {
                warn!("editMessageText(HTML) failed: {status} {body}");
            }
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

    pub(super) fn parse_message_content(&self, msg: &Message) -> String {
        let mut content = msg.text.clone().unwrap_or_default();

        if let Some(attr) = Self::format_forward_attribution(msg) {
            content = format!("{}{}", attr, content);
        }

        content
    }

    /// Download a Telegram file by file_id and return its base64-encoded content.
    pub(super) async fn download_file_bytes(&self, file_id: &str) -> anyhow::Result<Vec<u8>> {
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
    pub(super) async fn convert_to_opus_ogg(&self, audio_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
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
    pub(super) async fn ack_message(&self, chat_id: i64, message_id: i64) {
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
    pub(super) async fn remove_ack(&self, chat_id: i64, message_id: i64) {
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
    pub(super) async fn set_reaction(&self, chat_id: i64, message_id: i64, emoji: &str) -> anyhow::Result<()> {
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
    pub(super) async fn remove_reaction(
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
    pub(super) async fn answer_callback_query(&self, callback_query_id: &str) {
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
    /// Telegram's sendChatAction lasts ~5 seconds. The shared keep-alive
    /// loop refreshes it every 4 seconds until aborted, with a 60s TTL cap
    /// and a circuit breaker on 2 consecutive send failures.
    pub(super) fn start_internal_typing(&self, recipient: &str) {
        let (chat_id, thread_id) = Self::parse_reply_target(recipient);

        // Record typing start time for stall watchdog.
        self.typing_started_at
            .lock()
            .insert(recipient.to_string(), std::time::Instant::now());

        let bot_token = self.bot_token.clone();
        let api_base = self.api_base.clone();
        let typing_started_at = self.typing_started_at.clone();

        self.typing.start(
            recipient,
            TypingParams {
                interval: std::time::Duration::from_secs(4),
                max_duration: Some(std::time::Duration::from_secs(60)),
                max_consecutive_failures: 2,
                on_expired: Some(Box::new(|max_secs, recipient_key| {
                    // issue #113: this fires whenever a turn simply runs
                    // long (normal for e.g. multi-step tool use) — it is
                    // not itself evidence of a stall. Cross-reference with
                    // the LLM/provider logs for that turn before treating
                    // this as the root cause of anything.
                    warn!(
                        "Telegram typing TTL exceeded ({}s) for {} — long turn, not necessarily a stall; \
                         cross-reference with LLM/provider logs for this turn before escalating",
                        max_secs, recipient_key
                    );
                })),
                on_breaker: Some(Box::new(
                    |consecutive_failures, recipient_key, e| {
                        warn!(
                            "Telegram typing circuit breaker tripped after {consecutive_failures} consecutive failures for {recipient_key}: {e}"
                        );
                    },
                )),
                on_exit: Some(Box::new(move |recipient_key| {
                    typing_started_at.lock().remove(recipient_key);
                })),
            },
            move || {
                // Create a typing-specific client with shorter timeout.
                let typing_client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new());
                move || {
                    let client = typing_client.clone();
                    let url = format!("{}/bot{}/sendChatAction", api_base, bot_token);
                    let req = SendChatActionRequest {
                        chat_id: chat_id.clone(),
                        message_thread_id: thread_id.clone(),
                        action: "typing".to_string(),
                    };
                    async move {
                        client
                            .post(&url)
                            .json(&req)
                            .send()
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }
                }
            },
        );
    }

    /// Stop (abort) the typing keep-alive task for a recipient.
    pub(super) fn stop_internal_typing(&self, recipient: &str) {
        self.typing.stop(recipient);
        // Remove stall watchdog tracking.
        self.typing_started_at.lock().remove(recipient);
    }
}

