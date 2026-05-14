//! QQ Bot channel adapter.
//!
//! Implements the [`Channel`] trait for the QQ Bot API (WebSocket gateway + REST).
//!
//! # Features
//!
//! - WebSocket connection with auto-reconnect (Resume + incremental backoff)
//! - Proactive background token refresh (single-writer, SystemTime-based expiry)
//! - C2C private chat + Group @bot message receive/send
//! - Markdown message format (msg_type=2)
//! - Message chunking (~2000 char limit) + 429 rate-limit retry
//! - Typing indicator (C2C only, msg_type=6, 60s validity, refreshed by typing task)
//! - Interaction API: Keyboard buttons + CallbackQuery → text + ACK
//! - Bot- prefixed slash commands (/bot-ping, /bot-version, /bot-help, etc.)
//! - Message dedup via DedupState
//! - WebSocket session Resume (session_id + last_seq preservation)

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::{Channel, ChannelMessage, DedupState, SendMessage};
use crate::config::channel::QQBotAccountConfig;
use super::message::split_message_chunk;

// ── Constants ─────────────────────────────────────────────────────────────────

/// QQ Bot message max length (conservative, real limit may vary).
const QQ_MAX_MESSAGE_LENGTH: usize = 2000;

/// WebSocket gateway URL endpoint.
const GATEWAY_URL: &str = "https://api.sgroup.qq.com/gateway/bot";

/// Token endpoint.
const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// REST API base for v2 messages.
const API_BASE: &str = "https://api.sgroup.qq.com";

/// WebSocket intents:
///   PUBLIC_GUILD_MESSAGES   = 1 << 30 = 1073741824
///   GROUP_AT_MESSAGE_CREATE = 1 << 25 = 33554432
///   C2C_MESSAGE_CREATE      = 1 << 25 = 33554432
///   DIRECT_MESSAGE          = 1 << 12 = 4096
///   INTERACTION             = 1 << 26 = 67108864
const INTENTS: u32 = (1 << 30) | (1 << 25) | (1 << 12) | (1 << 26);

/// WebSocket opcodes.
const OP_RESUME: u32 = 6;
const OP_HELLO: u32 = 10;
const OP_IDENTIFY: u32 = 2;
const OP_HEARTBEAT: u32 = 1;
const OP_HEARTBEAT_ACK: u32 = 11;
const OP_DISPATCH: u32 = 0;
const OP_RECONNECT: u32 = 7;
const OP_INVALID_SESSION: u32 = 9;

/// Build User-Agent string for QQ Bot HTTP requests.
fn user_agent() -> String {
    let os = std::env::consts::OS;
    format!("MyClaw/{} (Rust; {})", env!("MYCLAW_VERSION"), os)
}

/// Reconnect delay schedule (seconds).
const RECONNECT_DELAYS: &[u64] = &[1, 2, 5, 10, 30, 60];
/// Maximum rapid reconnects before backing off.
const RAPID_RECONNECT_LIMIT: usize = 3;
const RAPID_RECONNECT_WINDOW_SECS: u64 = 5;

/// Session state for WebSocket Resume.
#[derive(Clone)]
struct SessionState {
    session_id: String,
    last_seq: u64,
}

/// Result of a WebSocket disconnection, used by ws_loop to decide reconnect strategy.
enum WsDisconnect {
    /// Normal disconnect or unknown close code — reconnect with fresh Identify.
    Clean,
    /// Should try Resume (e.g. server-initiated Reconnect opcode).
    TryResume,
    /// Fatal — do not reconnect (e.g. close codes 4914/4915).
    Fatal,
    /// Token-related — refresh token before reconnecting.
    TokenExpired,
}

// ── Interaction / Keyboard types ──────────────────────────────────────────────

/// Permission metadata for a keyboard button. type=2 means all users can click.
#[derive(Clone, serde::Serialize)]
struct ButtonPermission {
    r#type: u32,
}

/// What happens when a button is clicked.
#[derive(Clone, serde::Serialize)]
struct ButtonAction {
    /// 1 = Callback (INTERACTION_CREATE), 2 = Link (opens URL).
    r#type: u32,
    /// Payload delivered in data.resolved.button_data when type=1.
    data: String,
    permission: ButtonPermission,
    #[serde(skip_serializing_if = "Option::is_none")]
    click_limit: Option<u32>,
}

/// Visual rendering of a button.
#[derive(Clone, serde::Serialize)]
struct ButtonRenderData {
    label: String,
    visited_label: String,
    style: u32,
}

/// A single button in a keyboard row.
#[derive(Clone, serde::Serialize)]
struct Button {
    id: String,
    render_data: ButtonRenderData,
    action: ButtonAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<String>,
}

/// A row of buttons.
#[derive(Clone, serde::Serialize)]
struct ButtonRow {
    buttons: Vec<Button>,
}

/// Keyboard content wrapper.
#[derive(Clone, serde::Serialize)]
struct KeyboardContent {
    rows: Vec<ButtonRow>,
}

/// A keyboard (grid of button rows). Top-level payload for message body.
#[derive(Clone, serde::Serialize)]
struct Keyboard {
    content: KeyboardContent,
}

impl Keyboard {
    /// Create a keyboard from a slice of label/value pairs (one row per pair).
    /// Max 5 buttons per row, max 5 rows.
    fn from_pairs(pairs: &[(impl AsRef<str>, impl AsRef<str>)]) -> Self {
        let mut rows = Vec::new();
        let mut current_row = Vec::new();
        for (i, (label, value)) in pairs.iter().enumerate() {
            current_row.push(Button {
                id: format!("btn_{}", i),
                render_data: ButtonRenderData {
                    label: label.as_ref().to_string(),
                    visited_label: format!("✓ {}", label.as_ref()),
                    style: 1,
                },
                action: ButtonAction {
                    r#type: 1, // Callback
                    data: value.as_ref().to_string(),
                    permission: ButtonPermission { r#type: 2 }, // All users
                    click_limit: None,
                },
                group_id: None,
            });
            if current_row.len() >= 5 {
                rows.push(ButtonRow { buttons: std::mem::take(&mut current_row) });
            }
        }
        if !current_row.is_empty() {
            rows.push(ButtonRow { buttons: current_row });
        }
        Self { content: KeyboardContent { rows } }
    }
}

// ── Token state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TokenState {
    access_token: String,
    /// Wall-clock expiry time. Uses `SystemTime` instead of `Instant` so that
    /// token expiry is correctly detected after system suspend (e.g. laptop
    /// sleep). NTP adjustments of a few seconds are negligible compared to the
    /// typical ~2-hour token lifetime.
    expires_at: std::time::SystemTime,
}

struct TokenManager {
    state: tokio::sync::RwLock<Option<TokenState>>,
    app_id: String,
    client_secret: String,
    http_client: reqwest::Client,
    /// Background refresh task handle.
    bg_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TokenManager {
    fn new(app_id: String, client_secret: String) -> Self {
        Self {
            state: tokio::sync::RwLock::new(None),
            app_id,
            client_secret,
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            bg_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Start the background token refresh loop.
    /// Refreshes the token before it expires, so callers always get a valid token.
    async fn start_background_refresh(self: &Arc<Self>) {
        let mut handle = self.bg_handle.lock().await;
        if handle.is_some() {
            return; // Already running.
        }
        // Synchronously fetch the first token before spawning the background loop,
        // so that listen() returns with a populated cache and get_token() won't
        // race with a fallback do_refresh().
        if let Err(e) = self.do_refresh().await {
            error!(error = %e, "QQ Bot initial token fetch failed");
        }
        let this = Arc::clone(self);
        *handle = Some(tokio::spawn(async move {
            this.background_refresh_loop().await;
        }));
    }

    /// Background loop: refresh token before expiry.
    /// The initial fetch is already done by `start_background_refresh`; this loop
    /// handles ongoing periodic refresh.  If the initial fetch failed, the token
    /// cache is still empty, so the first sleep_duration will be 5 s and the loop
    /// will retry naturally.
    async fn background_refresh_loop(&self) {
        loop {
            // Calculate sleep duration until next refresh.
            let sleep_duration = {
                let state = self.state.read().await;
                match *state {
                    Some(ref s) => {
                        let remaining = s.expires_at
                            .duration_since(std::time::SystemTime::now())
                            .unwrap_or(Duration::ZERO);
                        // Refresh early — but never more than 1/3 of the remaining lifetime,
                        // so short-lived tokens still get a reasonable sleep window.
                        let refresh_ahead = Duration::min(
                            Duration::from_secs(300),
                            remaining / 3,
                        );
                        let jitter = Duration::from_millis(
                            rand::random::<u64>() % 30_000
                        );
                        remaining.saturating_sub(refresh_ahead).saturating_sub(jitter)
                    }
                    None => Duration::from_secs(5), // No token, retry soon.
                }
            };

            if !sleep_duration.is_zero() {
                tokio::time::sleep(sleep_duration).await;
            }

            if let Err(e) = self.do_refresh().await {
                error!(error = %e, "QQ Bot background token refresh failed, retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    /// Get a valid access token from cache.
    /// The background task ensures the token is always fresh; this is a read-only fast path.
    async fn get_token(&self) -> anyhow::Result<String> {
        let state = self.state.read().await;
        if let Some(ref s) = *state {
            if s.expires_at > std::time::SystemTime::now() {
                return Ok(s.access_token.clone());
            }
        }
        drop(state);
        Err(anyhow::anyhow!("QQ Bot token not available (expired or not initialized)"))
    }

    /// Force refresh the access token.
    async fn refresh(&self) -> anyhow::Result<String> {
        self.do_refresh().await
    }

    /// Internal: fetch a new token and update the cache.
    async fn do_refresh(&self) -> anyhow::Result<String> {
        let token_state = self.fetch_new_token().await?;
        let token = token_state.access_token.clone();
        *self.state.write().await = Some(token_state);
        Ok(token)
    }

    /// Actually fetch a new token from the API.
    async fn fetch_new_token(&self) -> anyhow::Result<TokenState> {
        let body = serde_json::json!({
            "appId": self.app_id,
            "clientSecret": self.client_secret,
        });

        let ua = user_agent();
        let resp = self
            .http_client
            .post(TOKEN_URL)
            .json(&body)
            .header("User-Agent", &ua)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("token request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "token request returned {}: {}",
                status,
                text
            ));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("token parse error: {}", e))?;

        let access_token = data["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing access_token in response"))?
            .to_string();

        let expires_in: u64 = data["expires_in"]
            .as_u64()
            .unwrap_or(7000);

        let token_state = TokenState {
            access_token: access_token.clone(),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(expires_in),
        };

        info!(expires_in_secs = expires_in, "QQ Bot access token refreshed");
        Ok(token_state)
    }
}

// ── Gateway payload types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GatewayPayload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    op: u32,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    d: serde_json::Value,
}

// ── QQ Bot Channel ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct QQBotChannel {
    config: QQBotAccountConfig,
    token_manager: Arc<TokenManager>,
    dedup: DedupState,
    /// Last sequence number for heartbeat.
    last_seq: Arc<Mutex<Option<u64>>>,
    http_client: reqwest::Client,
    /// Active typing keep-alive tasks, keyed by recipient (e.g. "c2c:xxx").
    typing_tasks: Arc<Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// WebSocket session for Resume support.
    session: Arc<Mutex<Option<SessionState>>>,
    /// Monotonic counter for proactive message msg_seq to avoid collisions.
    msg_seq_counter: Arc<AtomicU32>,
}

impl QQBotChannel {
    pub fn new(config: QQBotAccountConfig) -> Self {
        let app_id = config.app_id.clone();
        let client_secret = config.client_secret.clone();

        Self {
            config,
            token_manager: Arc::new(TokenManager::new(app_id, client_secret)),
            dedup: DedupState::new(),
            last_seq: Arc::new(Mutex::new(None)),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            typing_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            session: Arc::new(Mutex::new(None)),
            msg_seq_counter: Arc::new(AtomicU32::new(1)),
        }
    }

    /// Return the next proactive msg_seq value (monotonically increasing).
    fn next_msg_seq(&self) -> u32 {
        self.msg_seq_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if a C2C user is allowed.
    fn is_user_allowed(&self, openid: &str) -> bool {
        match &self.config.allow_from {
            None => true,
            Some(list) => list.iter().any(|u| u == openid),
        }
    }

    /// Check if a group is allowed.
    fn is_group_allowed(&self, group_openid: &str) -> bool {
        match &self.config.group_allow_from {
            None => true,
            Some(list) => list.iter().any(|g| g == group_openid),
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
            warn!(status = %status, "gateway got token-expired error, refreshing and retrying");
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
    ) -> Option<ChannelMessage> {
        match event_type {
            "C2C_MESSAGE_CREATE" => {
                let msg = self.parse_c2c_message(data)?;
                // Dedup check.
                if self.dedup.check_and_record(&msg.id) {
                    debug!(msg_id = %msg.id, "duplicate C2C message, skipping");
                    return None;
                }
                // Access check.
                if !self.is_user_allowed(&msg.sender) {
                    debug!(sender = %msg.sender, "C2C message from disallowed user");
                    return None;
                }
                Some(msg)
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                let msg = self.parse_group_message(data)?;
                // Dedup check.
                if self.dedup.check_and_record(&msg.id) {
                    debug!(msg_id = %msg.id, "duplicate group message, skipping");
                    return None;
                }
                // Access check: check group allow list.
                if let Some(group_id) = msg.reply_target.strip_prefix("group:") {
                    if !self.is_group_allowed(group_id) {
                        debug!(group = group_id, "group message from disallowed group");
                        return None;
                    }
                }
                Some(msg)
            }
            "INTERACTION_CREATE" => {
                // QQ Bot interaction: button click -> convert to text message.
                let resolved = data.get("data")
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
                    let group_openid = data.get("group_openid")
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

                // Access check for interaction
                if let Some(openid) = reply_target.strip_prefix("c2c:") {
                    if !self.is_user_allowed(openid) {
                        debug!(sender = %sender, "interaction from disallowed user");
                        return None;
                    }
                } else if let Some(group_id) = reply_target.strip_prefix("group:") {
                    if !self.is_group_allowed(group_id) {
                        debug!(group = group_id, "interaction from disallowed group");
                        return None;
                    }
                }

                // Acknowledge the interaction within 3 seconds (QQ Bot requirement).
                self.ack_interaction(event_id);

                Some(ChannelMessage {
                    id: event_id.to_string(),
                    sender,
                    reply_target,
                    content: button_data.to_string(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    thread_ts: None,
                    interruption_scope_id: None,
                    attachments: vec![],
                    image_urls: None,
                    image_base64: None,
                })
            }
            _ => {
                debug!(event = event_type, "ignoring dispatch event");
                None
            }
        }
    }

    /// Parse a C2C_MESSAGE_CREATE event into a ChannelMessage.
    fn parse_c2c_message(&self, data: &serde_json::Value) -> Option<ChannelMessage> {
        let author = data.get("author")?;
        let user_openid = author.get("user_openid")?.as_str()?;
        let content = data.get("content")?.as_str()?;
        let msg_id = data.get("id")?.as_str()?;

        let cleaned_content = content.trim().to_string();

        // Parse image URLs from attachments.
        let image_urls = if let Some(attachments) = data.get("attachments").and_then(|a| a.as_array()) {
            let urls: Vec<String> = attachments.iter()
                .filter_map(|a| {
                    if a.get("content_type").and_then(|v| v.as_str()) == Some("image") {
                        a.get("url").and_then(|v| v.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect();
            if urls.is_empty() { None } else { Some(urls) }
        } else {
            None
        };

        Some(ChannelMessage {
            id: msg_id.to_string(),
            sender: user_openid.to_string(),
            reply_target: format!("c2c:{}", user_openid),
            content: cleaned_content,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: None,
            interruption_scope_id: None,
            attachments: vec![],
            image_urls,
            image_base64: None,
        })
    }

    /// Parse a GROUP_AT_MESSAGE_CREATE event into a ChannelMessage.
    fn parse_group_message(&self, data: &serde_json::Value) -> Option<ChannelMessage> {
        let author = data.get("author")?;
        let member_openid = author.get("member_openid")?.as_str()?;
        let group_openid = data.get("group_openid")?.as_str()?;
        let content = data.get("content")?.as_str()?;
        let msg_id = data.get("id")?.as_str()?;

        let cleaned_content = content.trim().to_string();

        // Parse image URLs from attachments.
        let image_urls = if let Some(attachments) = data.get("attachments").and_then(|a| a.as_array()) {
            let urls: Vec<String> = attachments.iter()
                .filter_map(|a| {
                    if a.get("content_type").and_then(|v| v.as_str()) == Some("image") {
                        a.get("url").and_then(|v| v.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect();
            if urls.is_empty() { None } else { Some(urls) }
        } else {
            None
        };

        Some(ChannelMessage {
            id: msg_id.to_string(),
            sender: member_openid.to_string(),
            reply_target: format!("group:{}", group_openid),
            content: cleaned_content,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: None,
            interruption_scope_id: None,
            attachments: vec![],
            image_urls,
            image_base64: None,
        })
    }

    /// Build a markdown message body for QQ Bot API.
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
    async fn send_rest_with_retry(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<()> {
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
            warn!(status = %status, "QQ Bot REST got token-expired error, refreshing and retrying");
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
            warn!(retry_after_secs = retry_after, "QQ Bot REST rate limited, retrying after delay");
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
                    let _ = http.post(&url)
                        .header("Authorization", format!("QQBot {}", token))
                        .header("Content-Type", "application/json")
                        .header("User-Agent", user_agent())
                        .json(&body)
                        .send().await;
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
        let resp = self.http_client
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
                warn!(status = %status, "C2C keyboard got token-expired error, refreshing and retrying");
                let new_token = self.token_manager.refresh().await?;
                let resp = self.http_client
                    .post(&url)
                    .header("Authorization", format!("QQBot {}", new_token))
                    .header("Content-Type", "application/json")
                    .header("User-Agent", &ua)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("C2C keyboard retry failed: {}", e))?;

                if resp.status().is_success() {
                    debug!(openid = openid, "C2C keyboard message sent (after token refresh)");
                    return Ok(());
                }
                let retry_status = resp.status();
                let retry_text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("C2C keyboard retry returned {}: {}", retry_status, retry_text));
            }

            return Err(anyhow::anyhow!("C2C keyboard send returned {}: {}", status, text));
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
        let resp = self.http_client
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
                warn!(status = %status, "Group keyboard got token-expired error, refreshing and retrying");
                let new_token = self.token_manager.refresh().await?;
                let resp = self.http_client
                    .post(&url)
                    .header("Authorization", format!("QQBot {}", new_token))
                    .header("Content-Type", "application/json")
                    .header("User-Agent", &ua)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Group keyboard retry failed: {}", e))?;

                if resp.status().is_success() {
                    debug!(group_openid = group_openid, "group keyboard message sent (after token refresh)");
                    return Ok(());
                }
                let retry_status = resp.status();
                let retry_text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("Group keyboard retry returned {}: {}", retry_status, retry_text));
            }

            return Err(anyhow::anyhow!("Group keyboard send returned {}: {}", status, text));
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
                    warn!(error = %e, "failed to get token for interaction ACK");
                    return;
                }
            };

            let url = format!("{}/v2/interactions/{}/ack", API_BASE, event_id);
            let ua = user_agent();
            let body = serde_json::json!({ "code": 0 });

            let _ = http.put(&url)
                .header("Authorization", format!("QQBot {}", token))
                .header("Content-Type", "application/json")
                .header("User-Agent", &ua)
                .json(&body)
                .send()
                .await;

            debug!(event_id = %event_id, "interaction acknowledged");
        });
    }

    /// Try to handle a bot- prefixed slash command.
    /// Returns true if the command was handled (message consumed), false to continue dispatch.
    async fn try_bot_command(
        &self,
        content: &str,
        reply_target: &str,
        msg_id: &str,
    ) -> bool {
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
                    if self.send_c2c_keyboard(openid, help_text, &kb, msg_id).await.is_ok() {
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
        let chunks = split_message_chunk(&reply, QQ_MAX_MESSAGE_LENGTH);
        if let Some(openid) = reply_target.strip_prefix("c2c:") {
            for (i, chunk) in chunks.iter().enumerate() {
                let seq = self.next_msg_seq() + i as u32;
                if let Err(e) = self.send_c2c_message(openid, chunk, msg_id, seq).await {
                    warn!(chunk = i, error = %e, "failed to send bot command reply chunk");
                    return true;
                }
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        } else if let Some(group_openid) = reply_target.strip_prefix("group:") {
            for (i, chunk) in chunks.iter().enumerate() {
                let seq = self.next_msg_seq() + i as u32;
                if let Err(e) = self.send_group_message(group_openid, chunk, msg_id, seq).await {
                    warn!(chunk = i, error = %e, "failed to send bot command reply chunk");
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

#[async_trait]
impl Channel for QQBotChannel {
    fn name(&self) -> &str {
        "qqbot"
    }

    async fn send(&self, msg: &SendMessage) -> anyhow::Result<()> {
        let chunks = split_message_chunk(&msg.content, QQ_MAX_MESSAGE_LENGTH);
        // thread_ts carries the original message event ID for passive replies.
        let msg_id = msg.thread_ts.as_deref().unwrap_or("");

        // Normalize recipient: bare openids (from startup recovery fallback)
        // are treated as c2c: prefixed.
        let raw_recipient = msg.recipient.clone();
        if raw_recipient.is_empty() {
            anyhow::bail!("QQBot send failed: no recipient");
        }
        let recipient = if raw_recipient.starts_with("c2c:") || raw_recipient.starts_with("group:") {
            raw_recipient
        } else {
            format!("c2c:{}", raw_recipient)
        };

        // Build keyboard from inline_buttons (attached to last chunk only).
        let keyboard: Option<Keyboard> = msg.inline_buttons.as_ref().map(|buttons| {
            let pairs: Vec<(String, String)> = buttons.iter()
                .map(|b| (b.label.clone(), b.callback_data.clone()))
                .collect();
            Keyboard::from_pairs(&pairs)
        });

        let count = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            // msg_seq must be unique per chunk for the same msg_id (1-based).
            let msg_seq = (i as u32) + 1;
            let is_last = i == count - 1;

            let result = if is_last {
                if let Some(kb) = &keyboard {
                    // Last chunk with buttons — use keyboard endpoint.
                    if let Some(openid) = recipient.strip_prefix("c2c:") {
                        self.send_c2c_keyboard(openid, chunk, kb, msg_id).await
                    } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                        self.send_group_keyboard(group_openid, chunk, kb, msg_id).await
                    } else {
                        Err(anyhow::anyhow!(
                            "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                            recipient
                        ))
                    }
                } else {
                    // Last chunk without buttons — normal send.
                    if let Some(openid) = recipient.strip_prefix("c2c:") {
                        self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
                    } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                        self.send_group_message(group_openid, chunk, msg_id, msg_seq).await
                    } else {
                        Err(anyhow::anyhow!(
                            "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                            recipient
                        ))
                    }
                }
            } else {
                // Non-last chunk — always normal send.
                if let Some(openid) = recipient.strip_prefix("c2c:") {
                    self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
                } else if let Some(group_openid) = recipient.strip_prefix("group:") {
                    self.send_group_message(group_openid, chunk, msg_id, msg_seq).await
                } else {
                    Err(anyhow::anyhow!(
                        "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                        recipient
                    ))
                }
            };

            if let Err(e) = result {
                error!(chunk = i, error = %e, "failed to send chunk");
                return Err(e);
            }

            // Throttle between chunks to avoid rate limiting.
            if i < count - 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        // Stop typing indicator for this recipient now that the response is sent.
        self.stop_internal_typing(&recipient);

        Ok(())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>> {
        // Start proactive background token refresh (OpenClaw-style).
        self.token_manager.start_background_refresh().await;

        let (tx, rx) = mpsc::channel::<ChannelMessage>(256);

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
    async fn ws_loop(&self, tx: mpsc::Sender<ChannelMessage>) {
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
                    info!("QQ Bot WebSocket disconnected (resumable), reconnecting...");
                    attempt = 0;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok(WsDisconnect::Clean) => {
                    warn!("QQ Bot WebSocket disconnected, reconnecting...");
                    if rapid { attempt += 1; } else { attempt = 0; }
                    let delay = if attempt >= RAPID_RECONNECT_LIMIT {
                        Duration::from_secs(60)
                    } else {
                        Duration::from_secs(RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)])
                    };
                    // Clean disconnect clears session
                    *self.session.lock() = None;
                    info!(delay_secs = delay.as_secs(), attempt, "reconnecting");
                    tokio::time::sleep(delay).await;
                }
                Ok(WsDisconnect::TokenExpired) => {
                    warn!("QQ Bot token expired, forcing refresh before reconnect");
                    if let Err(e) = self.token_manager.refresh().await {
                        error!(error = %e, "token refresh failed");
                    }
                    *self.session.lock() = None;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Ok(WsDisconnect::Fatal) => {
                    error!("QQ Bot WebSocket fatal disconnect, stopping reconnect");
                    return;
                }
                Err(e) => {
                    error!(error = %e, "QQ Bot WebSocket error, reconnecting...");
                    if rapid { attempt += 1; } else { attempt = 0; }
                    let delay = if attempt >= RAPID_RECONNECT_LIMIT {
                        Duration::from_secs(60)
                    } else {
                        Duration::from_secs(RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)])
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
    async fn ws_connect(&self, tx: &mpsc::Sender<ChannelMessage>) -> anyhow::Result<WsDisconnect> {
        // 1. Get gateway URL.
        let ws_url = self.fetch_gateway_url().await?;
        info!(url = %ws_url, "connecting to QQ Bot WebSocket gateway");

        // 2. Connect.
        let (ws_stream, _response) = connect_async(&ws_url)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {}", e))?;

        info!("QQ Bot WebSocket connected");
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
            return Err(anyhow::anyhow!("expected OpCode 10 (Hello), got {}", hello.op));
        }

        let heartbeat_interval: u64 = hello.d["heartbeat_interval"]
            .as_u64()
            .unwrap_or(41250);

        info!(heartbeat_interval_ms = heartbeat_interval, "received Hello");

        // 4. Send Identify or Resume.
        let token = self.token_manager.get_token().await?;
        let session = self.session.lock().clone();
        let init_payload = match session {
            Some(ref s) => {
                info!(session_id = %s.session_id, seq = s.last_seq, "sending Resume");
                serde_json::json!({
                    "op": OP_RESUME,
                    "d": {
                        "token": format!("QQBot {}", token),
                        "session_id": s.session_id,
                        "seq": s.last_seq,
                    }
                }).to_string()
            }
            None => self.build_identify(&token),
        };
        write
            .send(Message::Text(init_payload.into()))
            .await
            .map_err(|e| anyhow::anyhow!("Identify/Resume send failed: {}", e))?;

        info!("QQ Bot Identify/Resume sent");

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
                        warn!(error = %e, "heartbeat send failed, connection likely closed");
                        return Ok(WsDisconnect::TryResume);
                    }
                    debug!("heartbeat sent");
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
                            warn!(error = %e, "WebSocket read error");
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
        tx: &mpsc::Sender<ChannelMessage>,
    ) -> Option<WsDisconnect> {
        let text = match ws_msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(0u16);
                info!(close_code = code, "WebSocket closed by server");
                return Some(match code {
                    // Token expired — refresh and reconnect
                    4004 => {
                        warn!("close 4004: token expired");
                        WsDisconnect::TokenExpired
                    }
                    // Session invalid — clear session, reconnect with Identify
                    4006 | 4007 | 4009 => {
                        warn!(code, "close: session invalidated, clearing session");
                        *self.session.lock() = None;
                        *self.last_seq.lock() = None;
                        WsDisconnect::Clean
                    }
                    // Rate limited — reconnect normally (ws_loop handles delay via attempt counter)
                    4008 => {
                        warn!("close 4008: rate limited");
                        WsDisconnect::Clean
                    }
                    // Fatal — stop reconnecting
                    4914 | 4915 => {
                        error!(code, "fatal close code");
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
                warn!(error = %e, "failed to parse WebSocket payload");
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
                            if let Some(session_id) = payload.d.get("session_id").and_then(|v| v.as_str()) {
                                info!(session_id = session_id, "READY received, session established");
                                *self.session.lock() = Some(SessionState {
                                    session_id: session_id.to_string(),
                                    last_seq: payload.s.unwrap_or(0),
                                });
                            }
                        }
                        "RESUMED" => {
                            info!("RESUMED received, session restored");
                        }
                        _ => {}
                    }
                    // User messages
                    if let Some(channel_msg) = self.handle_dispatch(event_type, &payload.d) {
                        // Bot- prefixed slash commands — intercept before orchestrator
                        let msg_id = payload.d.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if self.try_bot_command(&channel_msg.content, &channel_msg.reply_target, msg_id).await {
                            debug!(msg_id = %channel_msg.id, "bot command handled, skipping orchestrator");
                            return None;
                        }

                        if tx.send(channel_msg.clone()).await.is_err() {
                            warn!("channel receiver dropped, stopping listen");
                            return Some(WsDisconnect::Clean);
                        }
                        // Start typing keep-alive for C2C messages.
                        self.start_internal_typing(&channel_msg.reply_target);
                    }
                }
            }
            OP_HEARTBEAT_ACK => {
                debug!("heartbeat ACK received");
            }
            OP_RECONNECT => {
                warn!("server requested reconnect");
                return Some(WsDisconnect::TryResume);
            }
            OP_INVALID_SESSION => {
                warn!("invalid session (OpCode 9), clearing session for fresh identify");
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
