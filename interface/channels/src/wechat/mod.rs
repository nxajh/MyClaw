//! WeChat iLink Bot channel adapter.
//!
//! Implements the [`Channel`] trait for the WeChat iLink Bot API.
//!
//! # Features (v1)
//!
//! - Long-poll `getupdates` for incoming messages
//! - Send text messages via `sendmessage`
//! - QR login flow (fetches QR code + polls for confirmation)
//! - Typing indicators
//! - Allowed-user filtering
//! - Dedup of recently-seen messages
//!
//! # Not in v1
//!
//! - Media upload/download (CDN)
//! - Video/voice/file attachments
//! - Group @mention detection

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aes::Aes128;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
use ecb::{Decryptor, Encryptor};
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{Channel, ChannelMessage, DedupState, SendMessage};
use config::channel::WechatConfig;

// ── Constants ─────────────────────────────────────────────────────────────────

const CHANNEL_VERSION: &str = "2.1.7";
const ILINK_APP_ID: &str = "bot";
const QR_POLL_INTERVAL_SECS: u64 = 3;
const QR_MAX_ATTEMPTS: u64 = 60;
const RATE_LIMIT_PAUSE_SECS: u64 = 3600;
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

const MESSAGE_TYPE_BOT: i64 = 2;
const MESSAGE_STATE_FINISH: i64 = 2;
const ITEM_TYPE_TEXT: i64 = 1;
const TYPING_STATUS_TYPING: i64 = 1;
const TYPING_STATUS_CANCEL: i64 = 2;

// ── Crypto helpers ─────────────────────────────────────────────────────────────

fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut padded = data.to_vec();
    padded.extend(vec![padding as u8; padding]);
    padded
}

fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("Empty data".into());
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > data.len() {
        return Err("Invalid padding".into());
    }
    if data[data.len() - pad_len..].iter().any(|&b| b != pad_len as u8) {
        return Err("Invalid PKCS7 padding".into());
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

fn encrypt_ecb(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let padded = pkcs7_pad(plaintext, 16);
    let mut enc = Encryptor::<Aes128>::new(key.into());
    padded
        .chunks(16)
        .flat_map(|chunk| {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            enc.encrypt_block_mut(&mut block.into());
            block.to_vec()
        })
        .collect()
}

#[allow(dead_code)]
fn decrypt_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if !ciphertext.len().is_multiple_of(16) {
        return Err("Ciphertext length is not a multiple of 16".into());
    }
    let mut dec = Decryptor::<Aes128>::new(key.into());
    let decrypted: Vec<u8> = ciphertext
        .chunks(16)
        .flat_map(|chunk| {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            dec.decrypt_block_mut(&mut block.into());
            block.to_vec()
        })
        .collect();
    pkcs7_unpad(&decrypted)
}

// ── API types ─────────────────────────────────────────────────────────────────

fn build_base_info() -> BaseInfo {
    BaseInfo { channel_version: CHANNEL_VERSION.to_string() }
}

fn build_client_version() -> u32 {
    let parts: Vec<u32> = CHANNEL_VERSION
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();
    let major = parts.first().copied().unwrap_or(0);
    let minor = parts.get(1).copied().unwrap_or(0);
    let patch = parts.get(2).copied().unwrap_or(0);
    ((major & 0xff) << 16) | ((minor & 0xff) << 8) | (patch & 0xff)
}

#[derive(Debug, Clone, Serialize)]
struct BaseInfo {
    channel_version: String,
}

// ── Inbound message types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct IlinkMessage {
    #[serde(default)]
    from_user_id: String,
    #[serde(default)]
    to_user_id: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    create_time_ms: i64,
    #[serde(default)]
    group_id: String,
    #[serde(rename = "type", default)]
    message_type: i64,
    #[serde(rename = "state", default)]
    message_state: i64,
    #[serde(default)]
    list: Vec<MessageItem>,
    #[serde(default)]
    context_token: String,
}

impl IlinkMessage {
    fn chat_id(&self) -> &str {
        if self.group_id.is_empty() { &self.from_user_id } else { &self.group_id }
    }
    fn is_group(&self) -> bool { !self.group_id.is_empty() }
}

#[derive(Debug, Clone, Deserialize)]
struct MessageItem {
    #[serde(rename = "type", default)]
    item_type: i64,
    #[serde(default)]
    text_item: Option<TextItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct TextItem {
    #[serde(default)]
    text: String,
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct GetUpdatesResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    errmsg: String,
    #[serde(rename = "get_updates_buf", default)]
    get_updates_buf: String,
    #[serde(default)]
    msgs: Vec<IlinkMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct GetConfigResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    errmsg: String,
    #[serde(default)]
    wxid: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    typing_ticket: String,
    #[serde(default)]
    aeskey: String,
}

#[derive(Debug, Clone, Deserialize)]
struct QrCodeResponse {
    #[serde(default)]
    qrcode: String,
    #[serde(default)]
    qrcode_img_content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct QrStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    bot_token: String,
    #[serde(default)]
    ilink_bot_id: String,
    #[serde(default)]
    baseurl: String,
    #[serde(default)]
    ilink_user_id: String,
    #[serde(default)]
    nickname: String,
}

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct GetUpdatesRequest {
    #[serde(rename = "get_updates_buf")]
    get_updates_buf: String,
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
struct SendMessageRequest {
    #[serde(rename = "snake_case")]
    msg: SendMessageMsg,
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct SendMessageMsg {
    #[serde(default)]
    from_user_id: String,
    to_user_id: String,
    client_id: String,
    #[serde(rename = "type")]
    message_type: i64,
    #[serde(rename = "state")]
    message_state: i64,
    #[serde(rename = "list")]
    item_list: Vec<SendMessageItem>,
    #[serde(rename = "context_token", skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SendMessageItem {
    #[serde(rename = "type")]
    item_type: i64,
    #[serde(rename = "text_item")]
    text_item: SendTextItem,
}

#[derive(Debug, Clone, Serialize)]
struct SendTextItem { text: String }

#[derive(Debug, Clone, Serialize)]
struct SendTypingRequest {
    #[serde(rename = "ilink_user_id")]
    ilink_user_id: String,
    #[serde(rename = "typing_ticket")]
    typing_ticket: String,
    status: i64,
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
struct GetConfigRequest {
    #[serde(rename = "ilink_user_id")]
    ilink_user_id: String,
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
struct GetBotQrCodeRequest { #[serde(rename = "base_info")] base_info: BaseInfo }

#[derive(Debug, Clone, Serialize)]
struct GetQrCodeStatusRequest {
    #[serde(rename = "qrcode")]
    qrcode: String,
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

// ── API error ─────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("HTTP {0}: {1}")]
    Http(u16, String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("API error {0}: {1}")]
    Api(i64, String),
    #[error("Not authenticated")]
    NotAuthenticated,
}

// ── Shared state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct SharedState {
    bot_token: Option<String>,
    bot_wxid: Option<String>,
    bot_nickname: Option<String>,
    get_updates_buf: String,
    typing_ticket: Option<String>,
    aes_key: Option<String>,
    context_tokens: HashMap<String, String>,
    api_base: Option<String>,
}

// ── API client ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ApiClient {
    api_base: String,
    http: Client,
    state: Arc<RwLock<SharedState>>,
    client_version: String,
}

impl ApiClient {
    fn new(config: &WechatConfig) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(config.poll_timeout + 15))
            .build()
            .unwrap_or_else(|_| Client::new());
        let mut state = SharedState::default();
        if let Some(ref token) = config.bot_token {
            state.bot_token = Some(token.clone());
        }
        if let Some(ref key) = config.aes_key {
            state.aes_key = Some(key.clone());
        }
        Self {
            api_base: config.api_base.trim_end_matches('/').to_string(),
            http,
            state: Arc::new(RwLock::new(state)),
            client_version: build_client_version().to_string(),
        }
    }

    fn url(&self, endpoint: &str) -> String {
        let base = self.state.read()
            .api_base.clone()
            .unwrap_or_else(|| self.api_base.clone());
        format!("{}/{}", base.trim_end_matches('/'), endpoint.trim_start_matches('/'))
    }

    fn random_uin_header() -> String {
        let uin: u32 = rand::random();
        BASE64.encode(uin.to_string())
    }

    async fn api_post(&self, endpoint: &str, body: &serde_json::Value) -> Result<serde_json::Value, ApiError> {
        let mut req = self.http.post(self.url(endpoint));
        req = req.header("AuthorizationType", "ilink_bot_token");
        if let Some(token) = self.state.read().bot_token.clone().filter(|t| !t.is_empty()) {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req = req
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);

        let resp = req.json(body).send().await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ApiError::Http(resp.status().as_u16(), resp.text().await.unwrap_or_default()));
        }

        resp.json().await.map_err(|e| ApiError::Parse(e.to_string()))
    }

    async fn api_get(&self, endpoint: &str) -> Result<serde_json::Value, ApiError> {
        let req = self.http.get(self.url(endpoint))
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);

        let resp = req.send().await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ApiError::Http(resp.status().as_u16(), resp.text().await.unwrap_or_default()));
        }

        resp.json().await.map_err(|e| ApiError::Parse(e.to_string()))
    }

    fn check_ret(&self, raw: &serde_json::Value) -> Result<(), ApiError> {
        let code = raw.get("ret").and_then(|v| v.as_i64()).unwrap_or(0);
        let errmsg = raw.get("errmsg").and_then(|v| v.as_str()).unwrap_or("");
        match code {
            0 => Ok(()),
            -14 => Err(ApiError::Api(-14, "rate limited".into())),
            _ if code != 0 => Err(ApiError::Api(code, errmsg.into())),
            _ => Ok(()),
        }
    }

    // ── High-level API methods ──────────────────────────────────────────

    async fn get_updates(&self) -> Result<GetUpdatesResponse, ApiError> {
        let buf = self.state.read().get_updates_buf.clone();
        let req_body = GetUpdatesRequest { get_updates_buf: buf, base_info: build_base_info() };
        let resp = self.api_post("ilink/bot/getupdates", &serde_json::to_value(&req_body).unwrap()).await?;

        let new_buf = resp.get("get_updates_buf").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !new_buf.is_empty() {
            self.state.write().get_updates_buf = new_buf;
        }

        serde_json::from_value(resp.clone())
            .map_err(|e| ApiError::Parse(format!("get_updates: {e}")))
    }

    async fn send_text(&self, to_user_id: &str, text: &str, context_token: Option<&str>) -> Result<(), ApiError> {
        let client_id = format!("myclaw_{}", uuid::Uuid::new_v4());
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: SendTextItem { text: text.to_string() },
                }],
                context_token: context_token.map(String::from),
            },
            base_info: build_base_info(),
        };
        let resp = self.api_post("ilink/bot/sendmessage", &serde_json::to_value(&req).unwrap()).await?;
        self.check_ret(&resp)
    }

    async fn send_typing(&self, to_user_id: &str, typing: bool) -> Result<(), ApiError> {
        let ticket = self.state.read().typing_ticket.clone().unwrap_or_default();
        let req = SendTypingRequest {
            ilink_user_id: to_user_id.to_string(),
            typing_ticket: ticket,
            status: if typing { TYPING_STATUS_TYPING } else { TYPING_STATUS_CANCEL },
            base_info: build_base_info(),
        };
        let resp = self.api_post("ilink/bot/sendtyping", &serde_json::to_value(&req).unwrap()).await?;
        self.check_ret(&resp)
    }

    async fn get_config(&self, ilink_user_id: &str) -> Result<GetConfigResponse, ApiError> {
        let req = GetConfigRequest { ilink_user_id: ilink_user_id.to_string(), base_info: build_base_info() };
        let resp = self.api_post("ilink/bot/getconfig", &serde_json::to_value(&req).unwrap()).await?;
        self.check_ret(&resp)?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_config: {e}")))
    }

    async fn get_bot_qrcode(&self) -> Result<QrCodeResponse, ApiError> {
        let _req = GetBotQrCodeRequest { base_info: build_base_info() };
        let resp = self.api_get("ilink/bot/getbotqrcode").await?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_bot_qrcode: {e}")))
    }

    async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrStatus, ApiError> {
        let req = GetQrCodeStatusRequest { qrcode: qrcode.to_string(), base_info: build_base_info() };
        let resp = self.api_post("ilink/bot/getqrcodeqrt", &serde_json::to_value(&req).unwrap()).await?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_qrcode_status: {e}")))
    }
}

// ── Inbound event ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum InboundContent { Text(String), Unknown }

#[derive(Debug, Clone)]
struct InboundEvent {
    msg_id: String,
    sender_wxid: String,
    chat_id: String,
    is_group: bool,
    content: InboundContent,
    context_token: String,
    raw_timestamp: i64,
}

fn parse_inbound(msg: &IlinkMessage) -> InboundEvent {
    let content = match msg.list.first() {
        Some(first) if first.item_type == ITEM_TYPE_TEXT => {
            InboundContent::Text(first.text_item.as_ref().map(|t| t.text.clone()).unwrap_or_default())
        }
        _ => InboundContent::Unknown,
    };

    let msg_id = if msg.client_id.is_empty() {
        format!("{}_{}", msg.from_user_id, msg.create_time_ms)
    } else {
        msg.client_id.clone()
    };

    InboundEvent {
        msg_id,
        sender_wxid: msg.from_user_id.clone(),
        chat_id: msg.chat_id().to_string(),
        is_group: msg.is_group(),
        content,
        context_token: msg.context_token.clone(),
        raw_timestamp: msg.create_time_ms,
    }
}

// ── Error classification ──────────────────────────────────────────────────────

#[derive(Debug)]
enum ErrorClass { Auth, Network, Server, Parse }

fn error_class(err: &ApiError) -> ErrorClass {
    match err {
        ApiError::Http(code, _) if *code == 401 || *code == 403 => ErrorClass::Auth,
        ApiError::Api(code, msg) => {
            let lower = msg.to_lowercase();
            if *code == -1 || lower.contains("token") || lower.contains("expired")
                || lower.contains("unauthorized") || lower.contains("not login")
                || lower.contains("请先登录") || lower.contains("未登录") {
                ErrorClass::Auth
            } else {
                ErrorClass::Server
            }
        }
        ApiError::Network(_) => ErrorClass::Network,
        ApiError::Parse(_) => ErrorClass::Parse,
        ApiError::NotAuthenticated => ErrorClass::Auth,
        ApiError::Http(_, _) => ErrorClass::Server,
    }
}

fn classify_backoff(err: &ApiError, count: u32) -> u64 {
    match err {
        ApiError::Network(_) => std::cmp::min(5 + 2 * count as u64, 30),
        ApiError::Parse(_) => 3,
        ApiError::Http(401, _) | ApiError::Http(403, _) => 5,
        _ => std::cmp::min(2u64.pow(std::cmp::min(count, 6)), 60),
    }
}

// ── WechatChannel ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WechatChannel {
    api: ApiClient,
    config: WechatConfig,
    dedup: DedupState,
}

impl WechatChannel {
    pub fn new(config: WechatConfig) -> Self {
        Self { api: ApiClient::new(&config), config, dedup: DedupState::new() }
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        let allowed = &self.config.allowed_users;
        !allowed.is_empty() && (allowed.iter().any(|u| u == "*" || u == user_id))
    }

    async fn login(&self) -> anyhow::Result<()> {
        if self.api.state.read().bot_token.is_some() {
            info!("WeChat: using saved bot_token");
            return Ok(());
        }

        info!("WeChat: starting QR login flow");
        let qr_resp = self.api.get_bot_qrcode().await?;

        if !qr_resp.qrcode_img_content.is_empty() {
            info!("WeChat QR code image available (base64, {} bytes)", qr_resp.qrcode_img_content.len());
        }

        let mut qrcode = qr_resp.qrcode;
        for _ in 0..QR_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_secs(QR_POLL_INTERVAL_SECS)).await;
            let status = self.api.get_qrcode_status(&qrcode).await?;

            match status.status.as_str() {
                "confirmed" => {
                    info!("WeChat QR login confirmed: {} ({})", status.nickname, status.ilink_bot_id);
                    let mut st = self.api.state.write();
                    st.bot_token = Some(status.bot_token);
                    st.bot_wxid = Some(status.ilink_bot_id.clone());
                    st.bot_nickname = Some(status.nickname.clone());
                    if !status.baseurl.is_empty() {
                        info!("WeChat: API base updated to {}", status.baseurl);
                        st.api_base = Some(status.baseurl);
                    }
                    return Ok(());
                }
                "expired" => {
                    warn!("WeChat: QR code expired, refreshing");
                    qrcode = self.api.get_bot_qrcode().await?.qrcode;
                }
                "scaned_but_redirect" if !status.baseurl.is_empty() => {
                    info!("WeChat: IDC redirect to {}", status.baseurl);
                    self.api.state.write().api_base = Some(status.baseurl.clone());
                }
                _ => { /* "wait" or "scaned" — keep polling */ }
            }
        }

        anyhow::bail!("QR login timed out after {} attempts", QR_MAX_ATTEMPTS)
    }
}

#[async_trait]
impl Channel for WechatChannel {
    fn name(&self) -> &str { "wechat" }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let ctx_token = self.api.state.read().context_tokens.get(&message.recipient).cloned();
        let chunks = crate::split_message_chunk(&message.content, 2048);
        for chunk in chunks {
            self.api.send_text(&message.recipient, &chunk, ctx_token.as_deref()).await?;
        }
        Ok(())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelMessage>> {
        self.login().await?;
        let (tx, rx) = mpsc::channel::<ChannelMessage>(100);

        // Clone what the background task needs.
        let this = self.clone();

        tokio::spawn(async move {
            let mut consecutive_errors = 0u32;

            loop {
                match this.api.get_updates().await {
                    Ok(resp) => {
                        consecutive_errors = 0;
                        for msg in resp.msgs {
                            let event = parse_inbound(&msg);
                            if !this.dedup.check_and_record(&event.msg_id) { continue; }
                            if !this.is_user_allowed(&event.sender_wxid) { continue; }

                            let content = match event.content {
                                InboundContent::Text(t) => t,
                                InboundContent::Unknown => continue,
                            };
                            if content.trim().is_empty() { continue; }

                            if !event.context_token.is_empty() {
                                this.api.state.write()
                                    .context_tokens.insert(event.chat_id.clone(), event.context_token.clone());
                            }

                            let channel_msg = ChannelMessage {
                                id: event.msg_id,
                                sender: event.sender_wxid,
                                reply_target: event.chat_id,
                                content,
                                channel: "wechat".to_string(),
                                timestamp: event.raw_timestamp as u64,
                                thread_ts: None,
                                interruption_scope_id: None,
                                attachments: vec![],
                            };
                            // tx is moved into the async block; we send on it directly.
                            if let Err(e) = tx.send(channel_msg).await {
                                warn!("WeChat dispatch error (receiver dropped): {e}");
                                break;
                            }
                        }
                    }
                    Err(ApiError::Api(-14, _)) => {
                        warn!("WeChat: rate limited (-14), pausing {}s", RATE_LIMIT_PAUSE_SECS);
                        tokio::time::sleep(Duration::from_secs(RATE_LIMIT_PAUSE_SECS)).await;
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        let backoff = classify_backoff(&e, consecutive_errors);
                        match error_class(&e) {
                            ErrorClass::Auth => {
                                warn!("WeChat: auth error ({consecutive_errors}): {e}");
                                this.api.state.write().bot_token = None;
                                if let Err(login_err) = this.login().await {
                                    warn!("WeChat: re-login failed: {login_err}");
                                } else {
                                    info!("WeChat: re-login successful");
                                    consecutive_errors = 0;
                                }
                            }
                            ErrorClass::Network => {
                                warn!("WeChat: network error, retrying in {backoff}s: {e}");
                            }
                            ErrorClass::Server => {
                                warn!("WeChat: server error ({consecutive_errors}): {e}, retrying in {backoff}s");
                                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                    warn!("WeChat: max consecutive errors, attempting re-login");
                                    this.api.state.write().bot_token = None;
                                    if let Err(login_err) = this.login().await {
                                        warn!("WeChat: re-login failed: {login_err}");
                                    } else {
                                        info!("WeChat: re-login successful");
                                        consecutive_errors = 0;
                                    }
                                }
                            }
                            ErrorClass::Parse => {
                                warn!("WeChat: parse error, retrying in {backoff}s: {e}");
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn health_check(&self) -> bool {
        self.api.state.read().bot_token.is_some()
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.api.send_typing(recipient, true).await?;
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.api.send_typing(recipient, false).await?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_version_encoding() {
        assert_eq!(build_client_version(), 131335); // 2.1.7
    }

    #[test]
    fn test_pkcs7_roundtrip() {
        let data = b"hello world";
        let padded = pkcs7_pad(data, 16);
        assert_eq!(padded.len() % 16, 0);
        assert_eq!(pkcs7_unpad(&padded).unwrap(), data.to_vec());
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = b"0123456789abcdef";
        let plaintext = b"hello world test";
        let encrypted = encrypt_ecb(plaintext, key);
        assert_eq!(decrypt_ecb(&encrypted, key).unwrap(), plaintext.to_vec());
    }

    #[test]
    fn test_parse_text_message() {
        let msg = IlinkMessage {
            from_user_id: "user1".into(),
            to_user_id: "bot1".into(),
            client_id: "cid_123".into(),
            create_time_ms: 1000,
            group_id: String::new(),
            message_type: 1,
            message_state: 2,
            list: vec![MessageItem { item_type: ITEM_TYPE_TEXT, text_item: Some(TextItem { text: "hello".into() }) }],
            context_token: "ctx_tok".into(),
        };
        let event = parse_inbound(&msg);
        assert_eq!(event.sender_wxid, "user1");
        assert_eq!(event.chat_id, "user1");
        assert!(!event.is_group);
        match event.content { InboundContent::Text(t) => assert_eq!(t, "hello"), _ => panic!("expected text") }
    }

    #[test]
    fn test_dedup() {
        let dedup = DedupState::new();
        // HashSet::insert returns true when value was inserted (not a duplicate).
        assert!(dedup.check_and_record("msg1"));   // new → inserted → true
        assert!(!dedup.check_and_record("msg1")); // duplicate → not inserted → false
        assert!(dedup.check_and_record("msg2"));  // new → inserted → true
    }
}
