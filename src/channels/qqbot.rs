//! QQ Bot channel adapter.
//!
//! Implements the [`Channel`] trait for the QQ Bot API (WebSocket gateway + REST).
//!
//! # Features (v1)
//!
//! - WebSocket connection with auto-reconnect
//! - Heartbeat maintenance
//! - C2C private chat message receive/send
//! - Group @bot message receive/send
//! - Text message chunking (QQ ~2000 char limit)
//! - Markdown message format (no template needed, all bots can use)
//! - Message dedup via DedupState
//! - Typing indicator (C2C only, msg_type=6, 60s validity, refreshed by orchestrator)

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::{Channel, ChannelMessage, DedupState, SendMessage};
use crate::config::channel::QQBotConfig;
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
///   PUBLIC_GUILD_MESSAGES  = 1 << 30 = 1073741824
///   GROUP_AT_MESSAGE_CREATE = 1 << 25 = 33554432
///   C2C_MESSAGE_CREATE      = 1 << 25 = 33554432
///   Combined: 1073741824 | 33554432 = 1107296256
const INTENTS: u32 = (1 << 30) | (1 << 25);

/// WebSocket opcodes.
const OP_HELLO: u32 = 10;
const OP_IDENTIFY: u32 = 2;
const OP_HEARTBEAT: u32 = 1;
const OP_HEARTBEAT_ACK: u32 = 11;
const OP_DISPATCH: u32 = 0;
const OP_RECONNECT: u32 = 7;
const OP_INVALID_SESSION: u32 = 9;

/// Reconnect delay base (seconds).
const RECONNECT_DELAY_SECS: u64 = 5;

// ── Token state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TokenState {
    access_token: String,
    expires_at: std::time::Instant,
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
    fn start_background_refresh(self: &Arc<Self>) {
        let mut handle = self.bg_handle.blocking_lock();
        if handle.is_some() {
            return; // Already running.
        }
        let this = Arc::clone(self);
        *handle = Some(tokio::spawn(async move {
            this.background_refresh_loop().await;
        }));
    }

    /// Background loop: fetch initial token, then refresh before expiry.
    async fn background_refresh_loop(&self) {
        // Initial fetch.
        if let Err(e) = self.do_refresh().await {
            error!(error = %e, "QQ Bot initial token fetch failed, retrying in 5s");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        loop {
            // Calculate sleep duration until next refresh.
            let sleep_duration = {
                let state = self.state.read().await;
                match *state {
                    Some(ref s) => {
                        let remaining = s.expires_at.saturating_duration_since(std::time::Instant::now());
                        // Refresh 5 minutes before expiry, with up to 30s random jitter.
                        let refresh_ahead = Duration::from_secs(300);
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
    /// Falls back to a synchronous refresh only if the cache is empty (e.g. background
    /// task hasn't completed its first fetch yet).
    async fn get_token(&self) -> anyhow::Result<String> {
        let state = self.state.read().await;
        if let Some(ref s) = *state {
            if s.expires_at > std::time::Instant::now() {
                return Ok(s.access_token.clone());
            }
        }
        drop(state);

        // Cache miss or expired — background task may not have finished yet.
        // Do a one-shot refresh as fallback.
        self.do_refresh().await
    }

    /// Force refresh the access token (e.g. after receiving 11244 from the API).
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

        let resp = self
            .http_client
            .post(TOKEN_URL)
            .json(&body)
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
            expires_at: std::time::Instant::now() + Duration::from_secs(expires_in),
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
    config: QQBotConfig,
    token_manager: Arc<TokenManager>,
    dedup: DedupState,
    /// Last sequence number for heartbeat.
    last_seq: Arc<Mutex<Option<u64>>>,
    http_client: reqwest::Client,
}

impl QQBotChannel {
    pub fn new(config: QQBotConfig) -> Self {
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
        }
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
    /// Retries once with a fresh token if the server rejects it (code 11244).
    async fn fetch_gateway_url(&self) -> anyhow::Result<String> {
        let mut attempt = 0u8;
        loop {
            let token = if attempt == 0 {
                self.token_manager.get_token().await?
            } else {
                // Force refresh on retry — server told us the token is expired.
                self.token_manager.refresh().await?
            };
            let resp = self
                .http_client
                .get(GATEWAY_URL)
                .header("Authorization", format!("QQBot {}", token))
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

            // If the server says token expired, force-refresh and retry once.
            if attempt == 0 && text.contains("11244") {
                warn!("gateway rejected token (expired), force-refreshing and retrying");
                attempt += 1;
                continue;
            }

            return Err(anyhow::anyhow!("gateway returned {}: {}", status, text));
        }
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

        Some(ChannelMessage {
            id: msg_id.to_string(),
            sender: user_openid.to_string(),
            reply_target: format!("c2c:{}", user_openid),
            content: cleaned_content,
            channel: "qqbot".to_string(),
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

    /// Parse a GROUP_AT_MESSAGE_CREATE event into a ChannelMessage.
    fn parse_group_message(&self, data: &serde_json::Value) -> Option<ChannelMessage> {
        let author = data.get("author")?;
        let member_openid = author.get("member_openid")?.as_str()?;
        let group_openid = data.get("group_openid")?.as_str()?;
        let content = data.get("content")?.as_str()?;
        let msg_id = data.get("id")?.as_str()?;

        let cleaned_content = content.trim().to_string();

        Some(ChannelMessage {
            id: msg_id.to_string(),
            sender: member_openid.to_string(),
            reply_target: format!("group:{}", group_openid),
            content: cleaned_content,
            channel: "qqbot".to_string(),
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

    /// Send a C2C message via REST API (markdown format).
    async fn send_c2c_message(
        &self,
        openid: &str,
        content: &str,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let token = self.token_manager.get_token().await?;
        let url = format!("{}/v2/users/{}/messages", API_BASE, openid);

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
        }

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("C2C send failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // Token may have expired — QQ Bot returns 500 with code 11244 (not 401).
            // Background refresh should prevent this, but retry as safety net.
            if status.as_u16() == 401 || text.contains("11244") {
                warn!(status = %status, "C2C send got auth error, refreshing token and retrying");
                let token = self.token_manager.refresh().await?;
                let resp = self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("QQBot {}", token))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("C2C send retry failed: {}", e))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow::anyhow!(
                        "C2C send retry returned {}: {}",
                        status,
                        text
                    ));
                }
            } else {
                return Err(anyhow::anyhow!("C2C send returned {}: {}", status, text));
            }
        }

        debug!(openid = openid, "C2C message sent");
        Ok(())
    }

    /// Send a group message via REST API (markdown format).
    async fn send_group_message(
        &self,
        group_openid: &str,
        content: &str,
        msg_id: &str,
        msg_seq: u32,
    ) -> anyhow::Result<()> {
        let token = self.token_manager.get_token().await?;
        let url = format!("{}/v2/groups/{}/messages", API_BASE, group_openid);

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
        }

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("group send failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // Token may have expired — QQ Bot returns 500 with code 11244 (not 401).
            // Background refresh should prevent this, but retry as safety net.
            if status.as_u16() == 401 || text.contains("11244") {
                warn!(status = %status, "group send got auth error, refreshing token and retrying");
                let token = self.token_manager.refresh().await?;
                let resp = self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("QQBot {}", token))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("group send retry failed: {}", e))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow::anyhow!(
                        "group send retry returned {}: {}",
                        status,
                        text
                    ));
                }
            } else {
                return Err(anyhow::anyhow!(
                    "group send returned {}: {}",
                    status,
                    text
                ));
            }
        }

        debug!(group = group_openid, "group message sent");
        Ok(())
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

        for (i, chunk) in chunks.iter().enumerate() {
            // msg_seq must be unique per chunk for the same msg_id (1-based).
            let msg_seq = (i as u32) + 1;
            let result = if let Some(openid) = msg.recipient.strip_prefix("c2c:") {
                self.send_c2c_message(openid, chunk, msg_id, msg_seq).await
            } else if let Some(group_openid) = msg.recipient.strip_prefix("group:") {
                self.send_group_message(group_openid, chunk, msg_id, msg_seq).await
            } else {
                Err(anyhow::anyhow!(
                    "invalid QQ Bot recipient format: {} (expected c2c:<openid> or group:<openid>)",
                    msg.recipient
                ))
            };

            if let Err(e) = result {
                error!(chunk = i, error = %e, "failed to send chunk");
                return Err(e);
            }

            // Small delay between chunks to avoid rate limiting.
            if i + 1 < chunks.len() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        Ok(())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>> {
        // Start proactive background token refresh (OpenClaw-style).
        self.token_manager.start_background_refresh();

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

    /// Send typing indicator (C2C only).
    ///
    /// QQ Bot API: `POST /v2/users/{openid}/messages` with `msg_type: 6`
    /// and `input_notify.input_second: 60`. Only works for C2C private chat.
    /// The orchestrator refreshes this every ~4 seconds while waiting for a response.
    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        // Only C2C supports typing indicator — group chat does not.
        let openid = match recipient.strip_prefix("c2c:") {
            Some(id) => id,
            None => return Ok(()),
        };

        let token = self.token_manager.get_token().await?;
        let url = format!("{}/v2/users/{}/messages", API_BASE, openid);

        let body = serde_json::json!({
            "msg_type": 6,
            "input_notify": {
                "input_type": 1,
                "input_second": 60,
            },
        });

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("QQBot {}", token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("typing indicator request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // Non-fatal: typing indicator failure should not break message flow.
            debug!(status = %status, body = %text, "typing indicator failed (non-fatal)");
        }

        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        // QQ Bot API has no "stop typing" — the indicator expires after input_second (60s).
        Ok(())
    }
}

// ── WebSocket loop ────────────────────────────────────────────────────────────

impl QQBotChannel {
    /// Main WebSocket loop with auto-reconnect.
    async fn ws_loop(&self, tx: mpsc::Sender<ChannelMessage>) {
        loop {
            match self.ws_connect(&tx).await {
                Ok(()) => {
                    warn!("QQ Bot WebSocket disconnected, reconnecting...");
                }
                Err(e) => {
                    error!(error = %e, "QQ Bot WebSocket error, reconnecting...");
                }
            }

            info!(delay_secs = RECONNECT_DELAY_SECS, "reconnecting");
            tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
        }
    }

    /// Connect to the WebSocket gateway and handle the session.
    ///
    /// Uses `tokio::select!` to multiplex heartbeat sending and message reading
    /// in a single task, avoiding the need to clone `SplitSink`.
    async fn ws_connect(&self, tx: &mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
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

        // 4. Send Identify.
        let token = self.token_manager.get_token().await?;
        let identify = self.build_identify(&token);
        write
            .send(Message::Text(identify.into()))
            .await
            .map_err(|e| anyhow::anyhow!("Identify send failed: {}", e))?;

        info!("QQ Bot Identify sent");

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
                        return Ok(());
                    }
                    debug!("heartbeat sent");
                }
                // Incoming WebSocket message.
                msg = read.next() => {
                    match msg {
                        Some(Ok(ws_msg)) => {
                            let should_reconnect = self.handle_ws_message(ws_msg, tx).await;
                            if should_reconnect {
                                return Ok(());
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "WebSocket read error");
                            return Err(anyhow::anyhow!("WebSocket read error: {}", e));
                        }
                        None => {
                            info!("WebSocket stream ended");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Handle a single WebSocket message. Returns `true` if we should reconnect.
    async fn handle_ws_message(
        &self,
        ws_msg: Message,
        tx: &mpsc::Sender<ChannelMessage>,
    ) -> bool {
        let text = match ws_msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                info!(frame = ?frame, "WebSocket closed by server");
                return true;
            }
            Message::Ping(_) | Message::Pong(_) => return false,
            _ => return false,
        };

        let payload: GatewayPayload = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "failed to parse WebSocket payload");
                return false;
            }
        };

        // Update sequence number.
        if let Some(s) = payload.s {
            *self.last_seq.lock() = Some(s);
        }

        match payload.op {
            OP_DISPATCH => {
                if let Some(ref event_type) = payload.t {
                    if let Some(channel_msg) = self.handle_dispatch(event_type, &payload.d) {
                        if tx.send(channel_msg).await.is_err() {
                            warn!("channel receiver dropped, stopping listen");
                            return true;
                        }
                    }
                }
            }
            OP_HEARTBEAT_ACK => {
                debug!("heartbeat ACK received");
            }
            OP_RECONNECT => {
                warn!("server requested reconnect");
                return true;
            }
            OP_INVALID_SESSION => {
                warn!("invalid session (OpCode 9), clearing session for fresh identify");
                *self.last_seq.lock() = None;
                return true;
            }
            _ => {
                debug!(op = payload.op, "unknown opcode");
            }
        }

        false
    }
}
