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

/// Check if a line is a markdown table separator row (e.g. `|---|---|`).
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    // Must start and end with '|'.
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return false;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    // Split by '|' and check each segment is a valid separator cell.
    for part in inner.split('|') {
        let t = part.trim();
        if t.is_empty() {
            // Empty segment — allow (some parsers do |---||---|).
            continue;
        }
        // Must consist only of '-' and optionally leading/trailing ':'.
        let t = t.trim_start_matches(':').trim_end_matches(':');
        if t.is_empty() || !t.chars().all(|c| c == '-') {
            return false;
        }
    }
    true
}

/// Parse a markdown table row `| a | b |` into trimmed cell values.
fn parse_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    // Strip leading and trailing '|'.
    let inner = if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Pre-process markdown text: detect tables outside of code fences and convert
/// them into `<pre>` blocks with column-aligned monospace text.
///
/// Tables inside triple-backtick code fences are left untouched.
fn convert_markdown_tables(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.split('\n').collect();
    let mut result = String::with_capacity(markdown.len());
    let mut i = 0;
    let mut in_fence = false;

    while i < lines.len() {
        let line = lines[i];

        // Track code-fence state.
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            result.push_str(line);
            if i + 1 < lines.len() {
                result.push('\n');
            }
            i += 1;
            continue;
        }

        // Only look for tables outside code fences.
        if !in_fence
            && line.trim().starts_with('|')
            && i + 1 < lines.len()
            && is_table_separator(lines[i + 1])
        {
            // ── Found a table starting at line `i`. ──

            // 1. Parse the header row.
            let header = parse_table_row(line);
            let num_cols = header.len();

            // 2. Parse alignment from the separator row.
            let sep_trimmed = lines[i + 1].trim();
            let sep_inner = &sep_trimmed[1..sep_trimmed.len() - 1];
            let aligns: Vec<char> = sep_inner
                .split('|')
                .map(|part| {
                    let t = part.trim();
                    let starts = t.starts_with(':');
                    let ends = t.ends_with(':');
                    match (starts, ends) {
                        (true, true) => 'C',
                        (true, false) => 'L',
                        (false, true) => 'R',
                        _ => 'L',
                    }
                })
                .collect();

            // 3. Collect data rows.
            let mut data_rows: Vec<Vec<String>> = Vec::new();
            let mut j = i + 2; // Past header + separator.
            while j < lines.len() {
                let dl = lines[j].trim();
                if dl.starts_with('|') && dl.ends_with('|') && dl.len() > 1 {
                    let cells = parse_table_row(lines[j]);
                    if cells.len() >= num_cols {
                        data_rows.push(cells);
                        j += 1;
                        continue;
                    }
                }
                break;
            }

            // 4. If no data rows, not a valid table — pass through as-is.
            if data_rows.is_empty() {
                result.push_str(line);
                result.push('\n');
                result.push_str(lines[i + 1]);
                i += 2;
                if i < lines.len() {
                    result.push('\n');
                }
                continue;
            }

            // 5. Compute per-column widths (max over header + data rows).
            let mut col_widths: Vec<usize> = header.iter().map(|c| c.len()).collect();
            for row in &data_rows {
                for (k, cell) in row.iter().enumerate() {
                    if k < col_widths.len() {
                        col_widths[k] = col_widths[k].max(cell.len());
                    }
                }
            }

            // 6. Format a row with alignment, trimming trailing whitespace.
            let format_row = |cells: &[String]| -> String {
                let mut parts = Vec::with_capacity(cells.len());
                for (k, cell) in cells.iter().enumerate() {
                    let w = col_widths.get(k).copied().unwrap_or(0);
                    let a = aligns.get(k).copied().unwrap_or('L');
                    match a {
                        'C' => {
                            let total = w.saturating_sub(cell.len());
                            let left = total.div_ceil(2);
                            let right = total - left;
                            parts.push(format!(
                                "{}{}{}",
                                " ".repeat(left),
                                cell,
                                " ".repeat(right)
                            ));
                        }
                        'R' => {
                            parts.push(format!(
                                "{}{}",
                                " ".repeat(w.saturating_sub(cell.len())),
                                cell
                            ));
                        }
                        _ => {
                            parts.push(format!("{}{}", cell, " ".repeat(w - cell.len())));
                        }
                    }
                }
                let row = parts.join("  ");
                row.trim_end().to_string()
            };

            // 7. Build sentinel-wrapped block (actual <pre> tags are emitted
            //    by the main markdown_to_telegram_html loop so that the
            //    content is not re-processed for inline formatting or HTML
            //    escaping.
            result.push('\x00');
            result.push_str(&format_row(&header));
            for row in &data_rows {
                result.push('\n');
                result.push_str(&format_row(row));
            }
            result.push('\x00');

            // 8. Advance past all consumed table lines.
            i = j;
            if i < lines.len() {
                result.push('\n');
            }
        } else {
            // Non-table line — pass through as-is.
            result.push_str(line);
            if i + 1 < lines.len() {
                result.push('\n');
            }
            i += 1;
        }
    }

    result
}

/// Convert LLM Markdown output to Telegram-supported HTML.
///
/// Supports: bold, italic, strikethrough, inline code, code blocks (with optional language),
/// headings, links, blockquotes, and horizontal rules.
///
/// Formatting inside code blocks and inline code is preserved as-is (no nested parsing).
pub fn markdown_to_telegram_html(markdown: &str) -> String {
    // Pre-process: convert markdown tables (outside code fences) to <pre> blocks.
    let markdown = convert_markdown_tables(markdown);

    let mut out = String::with_capacity(markdown.len() * 2);

    // Tracks which inline formatting tags are currently open.
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;

    let chars: Vec<char> = markdown.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // ── Table pre block (\x00...\x00) from convert_markdown_tables ──
        if chars[i] == '\x00' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '\x00') {
                let content: String = chars[i + 1..i + 1 + end].iter().collect();
                out.push_str("<pre>");
                out.push_str(&escape_html(&content));
                out.push_str("</pre>");
                i = i + 1 + end + 1;
                continue;
            }
        }

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

        if resp.status().is_success() {
            return Ok(());
        }

        // HTML parse failed (likely malformed tags) — fall back to plain text.
        let html_status = resp.status();
        let html_body = resp.text().await.unwrap_or_default();
        warn!(
            "sendMessage with HTML parse_mode failed (status={html_status}, body={html_body}), \
             falling back to plain text"
        );

        let fallback_req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            message_thread_id: thread_id.map(String::from),
            text: text.to_string(),
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

    /// Call Telegram `getFile` API and return the direct file download URL.
    async fn get_file_url(&self, file_id: &str) -> anyhow::Result<String> {
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
        Ok(format!(
            "{}/file/bot{}/{}",
            self.api_base, self.bot_token, file_path
        ))
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
                    if !self.contains_bot_mention(text) {
                        continue;
                    }
                }

                let update_id = update.update_id.to_string();
                if self.dedup.check_and_record(&update_id) {
                    // Already seen this update — skip
                    continue;
                }

                let mut content = self.parse_message_content(&msg);
                let mut image_urls: Option<Vec<String>> = None;

                // Handle photo messages: get the largest photo's URL
                if let Some(photos) = &msg.photo {
                    if let Some(largest) = photos.last() {
                        match self.get_file_url(&largest.file_id).await {
                            Ok(url) => {
                                image_urls = Some(vec![url]);
                            }
                            Err(e) => {
                                warn!("Telegram getFile failed for photo {}: {e}", largest.file_id);
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
                    reply_target: chat.id.to_string(),
                    content,
                    channel: "telegram".to_string(),
                    timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    thread_ts: None,
                    interruption_scope_id: None,
                    attachments: vec![],
                    image_urls,
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
            caption: None,
            photo: None,
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
            caption: None,
            photo: None,
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

    // ── Markdown table tests ──────────────────────────────────────────────

    #[test]
    fn test_md_basic_table() {
        let input = "| Name  | Age |\n|-------|-----|\n| Alice | 30  |\n| Bob   | 25  |";
        let expected = "<pre>Name   Age\nAlice  30\nBob    25</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_column_width_alignment() {
        let input = "| ID | Name    | Score |\n|----|---------|-------|\n| 1  | Alice   | 95    |\n| 2  | Bob     | 87    |\n| 3  | Charlie | 92    |";
        let expected = "<pre>ID  Name     Score\n1   Alice    95\n2   Bob      87\n3   Charlie  92</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_single_column_table() {
        let input = "| Item |\n|------|\n| Foo  |\n| Bar  |";
        let expected = "<pre>Item\nFoo\nBar</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_with_surrounding_text() {
        let input = "Header text\n\n| Name | Age |\n|------|-----|\n| Alice | 30 |\n\nFooter text";
        let expected = "Header text\n\n<pre>Name   Age\nAlice  30</pre>\n\nFooter text";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_multiple_tables() {
        let input = "| A | B |\n|---|---|\n| 1 | 2 |\n\nsome text\n\n| X | Y |\n|---|---|\n| 3 | 4 |";
        let expected = "<pre>A  B\n1  2</pre>\n\nsome text\n\n<pre>X  Y\n3  4</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_alignment_marks() {
        let input = "| Left | Center | Right |\n|:-----|:------:|------:|\n| a    |   b    |     c |";
        let expected = "<pre>Left  Center  Right\na        b        c</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_inside_code_block_ignored() {
        let input = "```\n| a | b |\n|---|---|\n| 1 | 2 |\n```";
        let expected = "<pre>| a | b |\n|---|---|\n| 1 | 2 |</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_with_bold_in_cells() {
        let input = "| Name | Value |\n|------|-------|\n| Foo  | **x** |";
        let expected = "<pre>Name  Value\nFoo   **x**</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_uneven_column_widths() {
        let input = "| A   | B |\n|-----|---|\n| foo | 1 |\n| bar | 2 |";
        let expected = "<pre>A    B\nfoo  1\nbar  2</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_no_data_rows() {
        // Only header + separator: not a valid table, pass through as-is.
        let input = "| A | B |\n|---|---|";
        let expected = "| A | B |\n|---|---|";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_code_block_then_table() {
        let input = "```rust\nfn main() {}\n```\n\n| X | Y |\n|---|---|\n| 1 | 2 |";
        let expected = "<pre><code class=\"language-rust\">fn main() {}</code></pre>\n\n<pre>X  Y\n1  2</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_left_alignment() {
        let input = "| Name  |\n|:------|\n| Alice |";
        let expected = "<pre>Name\nAlice</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_center_alignment() {
        let input = "| Name  |\n|:-----:|\n| Alice |";
        let expected = "<pre> Name\nAlice</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_right_alignment() {
        let input = "| Value |\n|------:|\n|   42  |";
        let expected = "<pre>Value\n   42</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn test_md_table_only_separator_line() {
        // A standalone separator row without header+data is not a table.
        let input = "|---|---|";
        let expected = "|---|---|";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }
}
