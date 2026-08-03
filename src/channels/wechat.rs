//! WeChat iLink Bot channel adapter.
//!
//! Implements the [`Channel`] trait for the WeChat iLink Bot API.
//!
//! # Features
//!
//! - Long-poll `getupdates` for incoming messages
//! - Send text messages via `sendmessage`
//! - QR login flow (fetches QR code + polls for confirmation)
//! - Typing indicators (per-user typing_ticket)
//! - Allowed-user filtering
//! - Dedup of recently-seen messages
//! - Media upload/download (CDN with AES-128-ECB encryption)
//! - Image, video, and file attachment send/receive
//! - Context token persistence across restarts
//! - Markdown filtering for WeChat-incompatible syntax

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "wechat")]
use aes::Aes128;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[cfg(feature = "wechat")]
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
#[cfg(feature = "wechat")]
use ecb::{Decryptor, Encryptor};
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};
use crate::config::channel::WechatAccountConfig;
use crate::{Channel, DedupState, ProcessingStatus};

// ── Constants ─────────────────────────────────────────────────────────────────

const CHANNEL_VERSION: &str = "2.4.6";
const ILINK_APP_ID: &str = "bot";
const QR_POLL_INTERVAL_SECS: u64 = 1;
const QR_MAX_ATTEMPTS: u64 = 60;
const RATE_LIMIT_PAUSE_SECS: u64 = 3600;
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

const MESSAGE_TYPE_BOT: i64 = 2;
const MESSAGE_STATE_FINISH: i64 = 2;
const MESSAGE_STATE_GENERATING: i64 = 1;
const ITEM_TYPE_TEXT: i64 = 1;
const ITEM_TYPE_IMAGE: i64 = 2;
const ITEM_TYPE_VOICE: i64 = 3;
const ITEM_TYPE_FILE: i64 = 4;
const ITEM_TYPE_VIDEO: i64 = 5;
const ITEM_TYPE_TOOL_CALL_START: i64 = 11;
const ITEM_TYPE_TOOL_CALL_RESULT: i64 = 12;
const TYPING_STATUS_TYPING: i64 = 1;
const TYPING_STATUS_CANCEL: i64 = 2;

/// CDN base URL for media download/upload (separate from the API base).
const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

// Upload media type constants (proto: UploadMediaType).
const UPLOAD_MEDIA_IMAGE: i64 = 1;
const UPLOAD_MEDIA_VIDEO: i64 = 2;
const UPLOAD_MEDIA_FILE: i64 = 3;

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
    if data[data.len() - pad_len..]
        .iter()
        .any(|&b| b != pad_len as u8)
    {
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
    BaseInfo {
        channel_version: CHANNEL_VERSION.to_string(),
        bot_agent: Some("MyClaw".to_string()),
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_agent: Option<String>,
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
    #[serde(rename = "item_list", default)]
    item_list: Vec<MessageItem>,
    #[serde(default)]
    context_token: String,
}

impl IlinkMessage {
    fn chat_id(&self) -> &str {
        if self.group_id.is_empty() {
            &self.from_user_id
        } else {
            &self.group_id
        }
    }
    fn is_group(&self) -> bool {
        !self.group_id.is_empty()
    }
    /// Return the item list from the API response.
    /// The iLink API uses `item_list`, but older versions used `list`.
    fn items(&self) -> &[MessageItem] {
        if !self.item_list.is_empty() {
            &self.item_list
        } else {
            &self.list
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MessageItem {
    #[serde(rename = "type", default)]
    item_type: i64,
    #[serde(default)]
    text_item: Option<TextItem>,
    #[serde(default)]
    voice_item: Option<VoiceItem>,
    #[serde(default)]
    image_item: Option<InboundImageItem>,
    #[serde(default)]
    video_item: Option<InboundVideoItem>,
    #[serde(default)]
    file_item: Option<InboundFileItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct TextItem {
    #[serde(default)]
    text: String,
}

/// Inbound voice item (`type == 3`). WeChat ships a native ASR transcription in
/// `text`; we use it directly as the message body (the SILK media in `media` is
/// AES-encrypted and not downloaded). Other fields (encoding/duration) are
/// ignored.
#[derive(Debug, Clone, Deserialize)]
struct VoiceItem {
    #[serde(default)]
    text: String,
}

/// CDN media reference (`CDNMedia`). `aes_key` is base64-encoded bytes in JSON.
#[derive(Debug, Clone, Default, Deserialize)]
struct CDNMedia {
    #[serde(default)]
    encrypt_query_param: String,
    #[serde(default)]
    aes_key: String,
    #[serde(default, rename = "type")]
    encrypt_type: i64,
    #[serde(default)]
    full_url: String,
}

/// Inbound image item (`type == 2`). `aeskey` is a hex-encoded raw 16-byte AES
/// key, preferred over `media.aes_key` for inbound decryption.
#[derive(Debug, Clone, Deserialize)]
struct InboundImageItem {
    #[serde(default)]
    media: CDNMedia,
    /// Raw AES-128 key as hex string (16 bytes); preferred over media.aes_key.
    #[serde(default)]
    aeskey: String,
}

/// Inbound video item (`type == 5`).
#[derive(Debug, Clone, Deserialize)]
struct InboundVideoItem {
    #[serde(default)]
    media: CDNMedia,
}

/// Inbound file item (`type == 4`).
#[derive(Debug, Clone, Deserialize)]
struct InboundFileItem {
    #[serde(default)]
    media: CDNMedia,
    #[serde(default)]
    file_name: String,
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct GetUpdatesResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    errcode: i64,
    #[serde(default)]
    errmsg: String,
    #[serde(rename = "get_updates_buf", default)]
    get_updates_buf: String,
    #[serde(default)]
    longpolling_timeout_ms: u64,
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
    #[serde(rename = "msg")]
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
    message_type: i64,
    message_state: i64,
    item_list: Vec<SendMessageItem>,
    #[serde(rename = "context_token", skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SendMessageItem {
    #[serde(rename = "type")]
    item_type: i64,
    #[serde(skip_serializing_if = "Option::is_none", rename = "text_item")]
    text_item: Option<SendTextItem>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "image_item")]
    image_item: Option<SendImageItem>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "video_item")]
    video_item: Option<SendVideoItem>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "file_item")]
    file_item: Option<SendFileItem>,
}

#[derive(Debug, Clone, Serialize)]
struct SendTextItem {
    text: String,
}

/// CDN media reference for outbound messages.
#[derive(Debug, Clone, Serialize)]
struct SendCDNMedia {
    #[serde(rename = "encrypt_query_param")]
    encrypt_query_param: String,
    #[serde(rename = "aes_key")]
    aes_key: String,
    #[serde(rename = "encrypt_type")]
    encrypt_type: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SendImageItem {
    media: SendCDNMedia,
    mid_size: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SendVideoItem {
    media: SendCDNMedia,
    video_size: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SendFileItem {
    media: SendCDNMedia,
    file_name: String,
    len: String,
}

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

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
struct GetBotQrCodeRequest {
    #[serde(rename = "base_info")]
    base_info: BaseInfo,
}

#[allow(dead_code)]
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
    typing_tickets: HashMap<String, String>,
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

#[derive(Debug, Clone)]
struct UploadedMediaInfo {
    filekey: String,
    download_encrypted_query_param: String,
    aeskey_hex: String,
    file_size: i64,
    file_size_ciphertext: i64,
}

impl ApiClient {
    fn new(config: &WechatAccountConfig) -> Self {
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
        // Restore persisted context tokens
        let token_path = std::env::var("HOME")
            .map(|h| format!("{h}/.myclaw/state/wechat_context_tokens.json"))
            .unwrap_or_else(|_| "/tmp/wechat_context_tokens.json".to_string());
        if let Ok(json) = std::fs::read_to_string(&token_path) {
            if let Ok(tokens) = serde_json::from_str::<HashMap<String, String>>(&json) {
                state.context_tokens = tokens;
            }
        }
        Self {
            api_base: config.api_base.trim_end_matches('/').to_string(),
            http,
            state: Arc::new(RwLock::new(state)),
            client_version: build_client_version().to_string(),
        }
    }

    fn url(&self, endpoint: &str) -> String {
        let base = self
            .state
            .read()
            .api_base
            .clone()
            .unwrap_or_else(|| self.api_base.clone());
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        )
    }

    fn random_uin_header() -> String {
        let uin: u32 = rand::random();
        BASE64.encode(uin.to_string())
    }

    async fn api_post(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        let mut req = self.http.post(self.url(endpoint));
        req = req.header("AuthorizationType", "ilink_bot_token");
        if let Some(token) = self
            .state
            .read()
            .bot_token
            .clone()
            .filter(|t| !t.is_empty())
        {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req = req
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);

        let resp = req
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        resp.json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))
    }

    async fn api_get(&self, endpoint: &str) -> Result<serde_json::Value, ApiError> {
        let req = self
            .http
            .get(self.url(endpoint))
            .header("X-WECHAT-UIN", Self::random_uin_header())
            .header("iLink-App-Id", ILINK_APP_ID)
            .header("iLink-App-ClientVersion", &self.client_version);

        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        resp.json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))
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
        let req_body = GetUpdatesRequest {
            get_updates_buf: buf,
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/getupdates",
                &serde_json::to_value(&req_body).unwrap(),
            )
            .await?;

        let parsed: GetUpdatesResponse =
            serde_json::from_value(resp.clone()).map_err(|e| ApiError::Parse(format!("get_updates: {e}")))?;

        if parsed.ret != 0 || parsed.errcode != 0 {
            return Err(ApiError::Api(
                if parsed.errcode != 0 { parsed.errcode } else { parsed.ret },
                if parsed.errmsg.is_empty() { "get_updates error".into() } else { parsed.errmsg },
            ));
        }

        let new_buf = parsed.get_updates_buf.as_str();
        if !new_buf.is_empty() {
            self.state.write().get_updates_buf = new_buf.to_string();
        }
        Ok(parsed)
    }

    async fn send_text(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<(), ApiError> {
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
                    text_item: Some(SendTextItem {
                        text: filter_markdown(text),
                    }),
                    image_item: None,
                    video_item: None,
                    file_item: None,
                }],
                context_token: context_token.map(String::from),
            },
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/sendmessage",
                &serde_json::to_value(&req).unwrap(),
            )
            .await?;
        self.check_ret(&resp)
    }

    async fn send_typing(&self, to_user_id: &str, typing: bool) -> Result<(), ApiError> {
        let ticket = self.state.read()
            .typing_tickets
            .get(to_user_id)
            .cloned()
            .unwrap_or_default();
        let req = SendTypingRequest {
            ilink_user_id: to_user_id.to_string(),
            typing_ticket: ticket,
            status: if typing {
                TYPING_STATUS_TYPING
            } else {
                TYPING_STATUS_CANCEL
            },
            base_info: build_base_info(),
        };
        let resp = self
            .api_post("ilink/bot/sendtyping", &serde_json::to_value(&req).unwrap())
            .await?;
        self.check_ret(&resp)
    }

    /// Send a tool call progress item (TOOL_CALL_START or TOOL_CALL_RESULT).
    async fn send_tool_progress(
        &self,
        to_user_id: &str,
        item_type: i64,
        tool_name: &str,
        tool_call_id: &str,
        status: Option<&str>,
        context_token: Option<&str>,
    ) -> Result<(), ApiError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut item_json = serde_json::json!({
            "type": item_type,
            "create_time_ms": now_ms,
        });
        match item_type {
            ITEM_TYPE_TOOL_CALL_START => {
                item_json["is_completed"] = serde_json::json!(false);
                item_json["tool_call_start_item"] = serde_json::json!({
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                });
            }
            ITEM_TYPE_TOOL_CALL_RESULT => {
                item_json["is_completed"] = serde_json::json!(true);
                item_json["tool_call_result_item"] = serde_json::json!({
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "status": status.unwrap_or("completed"),
                });
            }
            _ => return Err(ApiError::Parse("invalid tool item type".into())),
        }
        let msg = serde_json::json!({
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": format!("myclaw_{}", uuid::Uuid::new_v4()),
            "message_type": MESSAGE_TYPE_BOT,
            "message_state": MESSAGE_STATE_GENERATING,
            "item_list": [item_json],
            "context_token": context_token,
        });
        let req = serde_json::json!({
            "msg": msg,
            "base_info": build_base_info(),
        });
        let resp = self
            .api_post("ilink/bot/sendmessage", &req)
            .await?;
        self.check_ret(&resp)
    }

    async fn get_config(&self, ilink_user_id: &str) -> Result<GetConfigResponse, ApiError> {
        let req = GetConfigRequest {
            ilink_user_id: ilink_user_id.to_string(),
            base_info: build_base_info(),
        };
        let resp = self
            .api_post("ilink/bot/getconfig", &serde_json::to_value(&req).unwrap())
            .await?;
        self.check_ret(&resp)?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_config: {e}")))
    }

    // ── CDN media methods ──────────────────────────────────────────────

    /// Parse CDN aes_key (base64) into raw 16-byte key.
    fn parse_cdn_aes_key(aes_key_base64: &str) -> Result<Vec<u8>, ApiError> {
        if aes_key_base64.is_empty() {
            return Err(ApiError::Parse("empty aes_key".into()));
        }
        let decoded = BASE64
            .decode(aes_key_base64.as_bytes())
            .map_err(|e| ApiError::Parse(format!("aes_key base64: {e}")))?;
        if decoded.len() == 16 {
            Ok(decoded)
        } else if decoded.len() == 32 {
            let hex_str = String::from_utf8_lossy(&decoded);
            hex::decode(hex_str.as_ref())
                .map_err(|e| ApiError::Parse(format!("aes_key hex: {e}")))
        } else {
            Err(ApiError::Parse(format!(
                "aes_key decoded to {} bytes, expected 16 or 32",
                decoded.len()
            )))
        }
    }

    /// Download and AES-128-ECB decrypt media from CDN.
    async fn download_cdn_media(
        &self,
        media: &CDNMedia,
        aeskey_hex: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        let key_bytes = if let Some(hex) = aeskey_hex.filter(|h| !h.is_empty()) {
            hex::decode(hex).map_err(|e| ApiError::Parse(format!("aeskey hex: {e}")))?
        } else {
            Self::parse_cdn_aes_key(&media.aes_key)?
        };
        if key_bytes.len() < 16 {
            return Err(ApiError::Parse("aes key too short".into()));
        }
        let key: [u8; 16] = key_bytes[..16].try_into().unwrap();

        let url = if !media.full_url.is_empty() {
            media.full_url.clone()
        } else {
            format!(
                "{}/download?encrypted_query_param={}",
                CDN_BASE_URL,
                urlencoding::encode(&media.encrypt_query_param)
            )
        };

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Http(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }
        let ciphertext = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        decrypt_ecb(&ciphertext, &key).map_err(|e| ApiError::Parse(format!("decrypt: {e}")))
    }

    /// Compute AES-128-ECB ciphertext size (PKCS7 padding to 16-byte boundary).
    fn aes_ecb_padded_size(plaintext_size: usize) -> usize {
        ((plaintext_size / 16) + 1) * 16
    }

    #[allow(clippy::too_many_arguments)]
    async fn get_upload_url(
        &self,
        filekey: &str,
        media_type: i64,
        to_user_id: &str,
        rawsize: i64,
        rawfilemd5: &str,
        filesize: i64,
        aeskey_hex: &str,
    ) -> Result<serde_json::Value, ApiError> {
        let req = serde_json::json!({
            "filekey": filekey,
            "media_type": media_type,
            "to_user_id": to_user_id,
            "rawsize": rawsize,
            "rawfilemd5": rawfilemd5,
            "filesize": filesize,
            "no_need_thumb": true,
            "aeskey": aeskey_hex,
            "base_info": build_base_info(),
        });
        let resp = self.api_post("ilink/bot/getuploadurl", &req).await?;
        self.check_ret(&resp)?;
        Ok(resp)
    }

    /// Upload encrypted buffer to CDN, return download param.
    async fn upload_to_cdn(
        &self,
        plaintext: &[u8],
        upload_full_url: Option<&str>,
        upload_param: &str,
        filekey: &str,
        aes_key: &[u8; 16],
    ) -> Result<String, ApiError> {
        let ciphertext = encrypt_ecb(plaintext, aes_key);
        let url = if let Some(full) = upload_full_url.filter(|s| !s.trim().is_empty()) {
            full.to_string()
        } else {
            format!(
                "{}/upload?encrypted_query_param={}&filekey={}",
                CDN_BASE_URL,
                urlencoding::encode(upload_param),
                urlencoding::encode(filekey)
            )
        };
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let err_msg = resp
                .headers()
                .get("x-error-message")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown error");
            return Err(ApiError::Http(resp.status().as_u16(), err_msg.to_string()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Parse(format!("cdn upload response: {e}")))?;
        let download_param = body
            .get("encrypt_query_param")
            .or_else(|| body.get("encrypted_query_param"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if download_param.is_empty() {
            return Err(ApiError::Parse("CDN upload: no download param".into()));
        }
        Ok(download_param)
    }

    /// Full media upload pipeline.
    async fn upload_media(
        &self,
        data: &[u8],
        to_user_id: &str,
        media_type: i64,
    ) -> Result<UploadedMediaInfo, ApiError> {
        use md5::{Digest, Md5};
        let rawsize = data.len() as i64;
        let mut hasher = Md5::new();
        hasher.update(data);
        let rawfilemd5: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        let filesize = Self::aes_ecb_padded_size(data.len()) as i64;
        let filekey: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
        let aes_key_bytes: [u8; 16] = {
            let mut arr = [0u8; 16];
            for byte in arr.iter_mut() {
                *byte = rand::random();
            }
            arr
        };
        let aeskey_hex: String = aes_key_bytes.iter().map(|b| format!("{:02x}", b)).collect();

        let resp = self
            .get_upload_url(
                &filekey,
                media_type,
                to_user_id,
                rawsize,
                &rawfilemd5,
                filesize,
                &aeskey_hex,
            )
            .await?;
        let upload_full_url = resp
            .get("upload_full_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let upload_param = resp
            .get("upload_param")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let download_param = self
            .upload_to_cdn(
                data,
                upload_full_url.as_deref(),
                upload_param,
                &filekey,
                &aes_key_bytes,
            )
            .await?;

        Ok(UploadedMediaInfo {
            filekey,
            download_encrypted_query_param: download_param,
            aeskey_hex,
            file_size: rawsize,
            file_size_ciphertext: filesize,
        })
    }

    async fn get_bot_qrcode(&self) -> Result<QrCodeResponse, ApiError> {
        let resp = self.api_get("ilink/bot/get_bot_qrcode?bot_type=3").await?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_bot_qrcode: {e}")))
    }

    async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrStatus, ApiError> {
        let endpoint = format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            urlencoding::encode(qrcode)
        );
        let resp = self.api_get(&endpoint).await?;
        serde_json::from_value(resp).map_err(|e| ApiError::Parse(format!("get_qrcode_status: {e}")))
    }

    async fn notify_start(&self) -> Result<(), ApiError> {
        let req = serde_json::json!({
            "base_info": build_base_info(),
        });
        let resp = self
            .api_post("ilink/bot/msg/notifystart", &req)
            .await?;
        self.check_ret(&resp)
    }

    async fn notify_stop(&self) -> Result<(), ApiError> {
        let req = serde_json::json!({
            "base_info": build_base_info(),
        });
        let resp = self
            .api_post("ilink/bot/msg/notifystop", &req)
            .await?;
        self.check_ret(&resp)
    }
}

// ── Inbound event ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum InboundContent {
    Text(String),
    MediaRequest {
        item_type: i64,
        media: CDNMedia,
        aeskey_hex: Option<String>,
        filename: String,
    },
    Unknown,
}

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
    let content = match msg.items().first() {
        Some(first) if first.item_type == ITEM_TYPE_TEXT => InboundContent::Text(
            first
                .text_item
                .as_ref()
                .map(|t| t.text.clone())
                .unwrap_or_default(),
        ),
        // Voice: use WeChat's native ASR transcription as the text body.
        Some(first) if first.item_type == ITEM_TYPE_VOICE => {
            match first
                .voice_item
                .as_ref()
                .map(|v| v.text.clone())
                .unwrap_or_default()
            {
                t if t.trim().is_empty() => InboundContent::Unknown,
                t => InboundContent::Text(t),
            }
        }
        Some(first) if first.item_type == ITEM_TYPE_IMAGE => {
            if let Some(ref img) = first.image_item {
                if !img.media.encrypt_query_param.is_empty() || !img.media.full_url.is_empty() {
                    InboundContent::MediaRequest {
                        item_type: ITEM_TYPE_IMAGE,
                        media: img.media.clone(),
                        aeskey_hex: Some(img.aeskey.clone()),
                        filename: format!("image_{}.jpg", msg.create_time_ms),
                    }
                } else {
                    InboundContent::Unknown
                }
            } else {
                InboundContent::Unknown
            }
        }
        Some(first) if first.item_type == ITEM_TYPE_VIDEO => {
            if let Some(ref vid) = first.video_item {
                if !vid.media.encrypt_query_param.is_empty() || !vid.media.full_url.is_empty() {
                    InboundContent::MediaRequest {
                        item_type: ITEM_TYPE_VIDEO,
                        media: vid.media.clone(),
                        aeskey_hex: None,
                        filename: format!("video_{}.mp4", msg.create_time_ms),
                    }
                } else {
                    InboundContent::Unknown
                }
            } else {
                InboundContent::Unknown
            }
        }
        Some(first) if first.item_type == ITEM_TYPE_FILE => {
            if let Some(ref f) = first.file_item {
                if !f.media.encrypt_query_param.is_empty() || !f.media.full_url.is_empty() {
                    InboundContent::MediaRequest {
                        item_type: ITEM_TYPE_FILE,
                        media: f.media.clone(),
                        aeskey_hex: None,
                        filename: if f.file_name.is_empty() {
                            format!("file_{}", msg.create_time_ms)
                        } else {
                            f.file_name.clone()
                        },
                    }
                } else {
                    InboundContent::Unknown
                }
            } else {
                InboundContent::Unknown
            }
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
enum ErrorClass {
    Auth,
    Network,
    Server,
    Parse,
}

fn error_class(err: &ApiError) -> ErrorClass {
    match err {
        ApiError::Http(code, _) if *code == 401 || *code == 403 => ErrorClass::Auth,
        ApiError::Api(code, msg) => {
            let lower = msg.to_lowercase();
            if *code == -1
                || lower.contains("token")
                || lower.contains("expired")
                || lower.contains("unauthorized")
                || lower.contains("not login")
                || lower.contains("请先登录")
                || lower.contains("未登录")
            {
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
    config: WechatAccountConfig,
    dedup: DedupState,
}

impl WechatChannel {
    pub fn new(config: WechatAccountConfig) -> Self {
        let ch = Self {
            api: ApiClient::new(&config),
            config,
            dedup: DedupState::new(),
        };
        crate::channels::warn_if_locked_down(&ch);
        ch
    }

    /// Build the unified security policy from Wechat config (RFC §14.5).
    /// Wechat keeps `allowed_users: Vec<String>` (not Option) so the
    /// historical "missing field = empty = reject all" semantic stays in
    /// place — flipping the field type to Option would be a security
    /// downgrade for users who omit `allowed_users`. We wrap with
    /// `Some(...)` to reuse the unified `AllowList::from_config` path.
    fn build_security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        use crate::channels::{AllowList, ChannelSecurityPolicy, GroupAuthMode};
        ChannelSecurityPolicy {
            allowed_users: AllowList::from_config(Some(self.config.allowed_users.clone())),
            group_mode: GroupAuthMode::Reject, // Wechat has no group concept
            group_allowlist: AllowList::All,
        }
    }

    async fn login(&self) -> anyhow::Result<()> {
        if self.api.state.read().bot_token.is_some() {
            info!("WeChat: using saved bot_token");
            if let Err(e) = self.api.notify_start().await {
                warn!("WeChat: notifyStart failed (ignored): {e}");
            } else {
                info!("WeChat: notifyStart sent");
            }
            return Ok(());
        }

        info!("WeChat: starting QR login flow");
        let qr_resp = self.api.get_bot_qrcode().await?;

        if !qr_resp.qrcode_img_content.is_empty() {
            info!(
                "WeChat QR login URL: {} (qrcode={})",
                qr_resp.qrcode_img_content, qr_resp.qrcode
            );
        }

        let mut qrcode = qr_resp.qrcode;
        for _ in 0..QR_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_secs(QR_POLL_INTERVAL_SECS)).await;
            let status = self.api.get_qrcode_status(&qrcode).await?;

            match status.status.as_str() {
                "confirmed" => {
                    info!(
                        "WeChat QR login confirmed: {} ({})",
                        status.nickname, status.ilink_bot_id
                    );
                    {
                        let mut st = self.api.state.write();
                        st.bot_token = Some(status.bot_token);
                        st.bot_wxid = Some(status.ilink_bot_id.clone());
                        st.bot_nickname = Some(status.nickname.clone());
                        if !status.baseurl.is_empty() {
                            info!("WeChat: API base updated to {}", status.baseurl);
                            st.api_base = Some(status.baseurl);
                        }
                    }
                    if let Err(e) = self.api.notify_start().await {
                        warn!("WeChat: notifyStart failed (ignored): {e}");
                    } else {
                        info!("WeChat: notifyStart sent");
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

static WECHAT_CAPS: crate::channels::message::ChannelCapabilities =
    crate::channels::message::ChannelCapabilities::wechat();

#[async_trait]
impl Channel for WechatChannel {
    fn name(&self) -> &str {
        "wechat"
    }

    fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities {
        &WECHAT_CAPS
    }

    async fn on_status(&self, recipient: &str, status: ProcessingStatus) {
        match status {
            ProcessingStatus::Thinking => {
                if let Err(e) = self.api.send_typing(recipient, true).await {
                    debug!("WeChat: typing indicator failed: {e}");
                }
            }
            ProcessingStatus::Done | ProcessingStatus::Error => {
                if let Err(e) = self.api.send_typing(recipient, false).await {
                    debug!("WeChat: typing cancel failed: {e}");
                }
            }
        }
    }

    async fn on_tool_event(
        &self,
        recipient: &str,
        event: crate::channels::ToolEvent,
    ) {
        let ctx_token = self
            .api
            .state
            .read()
            .context_tokens
            .get(recipient)
            .cloned();
        let result = match &event {
            crate::channels::ToolEvent::Start {
                tool_name,
                tool_call_id,
            } => {
                self.api
                    .send_tool_progress(
                        recipient,
                        ITEM_TYPE_TOOL_CALL_START,
                        tool_name,
                        tool_call_id,
                        None,
                        ctx_token.as_deref(),
                    )
                    .await
            }
            crate::channels::ToolEvent::End {
                tool_name,
                tool_call_id,
                success,
            } => {
                self.api
                    .send_tool_progress(
                        recipient,
                        ITEM_TYPE_TOOL_CALL_RESULT,
                        tool_name,
                        tool_call_id,
                        Some(if *success { "completed" } else { "failed" }),
                        ctx_token.as_deref(),
                    )
                    .await
            }
        };
        if let Err(e) = result {
            debug!("WeChat: tool progress send failed: {e}");
        }
    }

    fn security_policy(&self) -> crate::channels::ChannelSecurityPolicy {
        self.build_security_policy()
    }

    async fn send_message(
        &self,
        message: &crate::channels::ChannelOutboundMessage,
    ) -> anyhow::Result<crate::channels::OutboundSendResult> {
        let ctx_token = self
            .api
            .state
            .read()
            .context_tokens
            .get(&message.receiver.id)
            .cloned();

        // Send text first if any
        if !message.content.text.trim().is_empty() {
            let chunks = crate::channels::message::split_message_chunk(
                &message.content.text,
                self.capabilities().message_chunk_limit,
                self.capabilities().message_len_unit,
            );
            for chunk in chunks {
                self.api
                    .send_text(&message.receiver.id, &chunk, ctx_token.as_deref())
                    .await?;
            }
        }

        // Send files via CDN upload
        for file in &message.content.files {
            use tokio::io::AsyncReadExt;
            let mut reader = file.body.open().await?;
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).await?;

            let mime = file.meta.mime_type.as_deref().unwrap_or("application/octet-stream");
            let (media_type, item_type) = if mime.starts_with("image/") {
                (UPLOAD_MEDIA_IMAGE, ITEM_TYPE_IMAGE)
            } else if mime.starts_with("video/") {
                (UPLOAD_MEDIA_VIDEO, ITEM_TYPE_VIDEO)
            } else {
                (UPLOAD_MEDIA_FILE, ITEM_TYPE_FILE)
            };

            let uploaded = self
                .api
                .upload_media(&buf, &message.receiver.id, media_type)
                .await?;

            let aes_key_b64 = BASE64.encode(
                hex::decode(&uploaded.aeskey_hex).unwrap_or_default(),
            );

            let req = SendMessageRequest {
                msg: SendMessageMsg {
                    from_user_id: String::new(),
                    to_user_id: message.receiver.id.clone(),
                    client_id: format!("myclaw_{}", uuid::Uuid::new_v4()),
                    message_type: MESSAGE_TYPE_BOT,
                    message_state: MESSAGE_STATE_FINISH,
                    item_list: vec![match item_type {
                        ITEM_TYPE_IMAGE => SendMessageItem {
                            item_type: ITEM_TYPE_IMAGE,
                            text_item: None,
                            image_item: Some(SendImageItem {
                                media: SendCDNMedia {
                                    encrypt_query_param: uploaded.download_encrypted_query_param,
                                    aes_key: aes_key_b64,
                                    encrypt_type: 1,
                                },
                                mid_size: uploaded.file_size_ciphertext,
                            }),
                            video_item: None,
                            file_item: None,
                        },
                        ITEM_TYPE_VIDEO => SendMessageItem {
                            item_type: ITEM_TYPE_VIDEO,
                            text_item: None,
                            image_item: None,
                            video_item: Some(SendVideoItem {
                                media: SendCDNMedia {
                                    encrypt_query_param: uploaded.download_encrypted_query_param,
                                    aes_key: aes_key_b64,
                                    encrypt_type: 1,
                                },
                                video_size: uploaded.file_size_ciphertext,
                            }),
                            file_item: None,
                        },
                        _ => SendMessageItem {
                            item_type: ITEM_TYPE_FILE,
                            text_item: None,
                            image_item: None,
                            video_item: None,
                            file_item: Some(SendFileItem {
                                media: SendCDNMedia {
                                    encrypt_query_param: uploaded.download_encrypted_query_param,
                                    aes_key: aes_key_b64,
                                    encrypt_type: 1,
                                },
                                file_name: file.meta.file_name.clone(),
                                len: uploaded.file_size.to_string(),
                            }),
                        },
                    }],
                    context_token: ctx_token.clone(),
                },
                base_info: build_base_info(),
            };
            let resp = self
                .api
                .api_post("ilink/bot/sendmessage", &serde_json::to_value(&req).unwrap())
                .await?;
            self.api.check_ret(&resp)?;
        }

        Ok(crate::channels::OutboundSendResult::empty())
    }

    async fn listen(&self) -> anyhow::Result<mpsc::Receiver<ChannelInboundMessage>> {
        self.login().await?;
        let (tx, rx) = mpsc::channel::<ChannelInboundMessage>(100);

        // Clone what the background task needs.
        let this = self.clone();

        tokio::spawn(async move {
            let mut consecutive_errors = 0u32;

            loop {
                match this.api.get_updates().await {
                    Ok(resp) => {
                        consecutive_errors = 0;
                        debug!("WeChat: get_updates returned {} msgs", resp.msgs.len());
                        for msg in resp.msgs {
                            let event = parse_inbound(&msg);
                            if this.dedup.check_and_record(&event.msg_id) {
                                continue;
                            }
                            match this.check_authorization(
                                &event.sender_wxid,
                                crate::channels::MessageScope::Direct,
                            ) {
                                crate::channels::AuthDecision::Allow => {}
                                crate::channels::AuthDecision::Ignore => continue,
                                crate::channels::AuthDecision::Reject { reason } => {
                                    warn!(sender = %event.sender_wxid, reason, "wechat: rejected by policy");
                                    continue;
                                }
                            }

                            let content = match event.content {
                                InboundContent::Text(t) => {
                                    if t.trim().is_empty() {
                                        continue;
                                    }
                                    ChannelMessageContent::text(t)
                                }
                                InboundContent::MediaRequest {
                                    item_type,
                                    media,
                                    aeskey_hex,
                                    filename,
                                } => {
                                    match this.api.download_cdn_media(&media, aeskey_hex.as_deref()).await {
                                        Ok(data) => {
                                            let mime = match item_type {
                                                ITEM_TYPE_IMAGE => "image/jpeg",
                                                ITEM_TYPE_VIDEO => "video/mp4",
                                                ITEM_TYPE_FILE => "application/octet-stream",
                                                _ => "application/octet-stream",
                                            };
                                            let tmp_dir = std::env::temp_dir();
                                            let tmp_path = tmp_dir.join(format!("wechat_media_{}", uuid::Uuid::new_v4()));
                                            if let Err(e) = std::fs::write(&tmp_path, &data) {
                                                warn!("WeChat: failed to write media temp file: {e}");
                                                continue;
                                            }
                                            let mut content = ChannelMessageContent::text(String::new());
                                            content.files.push(ChannelFile {
                                                meta: ChannelFileMeta {
                                                    file_name: filename,
                                                    mime_type: Some(mime.to_string()),
                                                    size_bytes: Some(data.len() as u64),
                                                },
                                                body: std::sync::Arc::new(LocalFileBody::new(&tmp_path)),
                                            });
                                            content
                                        }
                                        Err(e) => {
                                            warn!("WeChat: media download failed: {e}");
                                            continue;
                                        }
                                    }
                                }
                                InboundContent::Unknown => continue,
                            };

                            if !event.context_token.is_empty() {
                                this.api
                                    .state
                                    .write()
                                    .context_tokens
                                    .insert(event.chat_id.clone(), event.context_token.clone());
                                // Persist context tokens to disk
                                {
                                    let state = this.api.state.read();
                                    let path = std::env::var("HOME")
                                        .map(|h| format!("{h}/.myclaw/state/wechat_context_tokens.json"))
                                        .unwrap_or_else(|_| "/tmp/wechat_context_tokens.json".to_string());
                                    let _ = std::fs::write(&path, serde_json::to_string(&state.context_tokens).unwrap_or_default());
                                }
                            }

                            // Fetch typing_ticket for this user (best-effort)
                            if let Ok(config) = this.api.get_config(&event.sender_wxid).await {
                                if !config.typing_ticket.is_empty() {
                                    this.api
                                        .state
                                        .write()
                                        .typing_tickets
                                        .insert(event.sender_wxid.clone(), config.typing_ticket);
                                }
                            }

                            let channel_msg = ChannelInboundMessage {
                                id: event.msg_id,
                                sender: MessageSender::new(event.sender_wxid),
                                receiver: MessageReceiver::new(event.chat_id),
                                content,
                                timestamp: event.raw_timestamp as u64,
                                interruption_scope_id: None,
                            };
                            // tx is moved into the async block; we send on it directly.
                            if let Err(e) = tx.send(channel_msg).await {
                                warn!("WeChat dispatch error (receiver dropped): {e}");
                                break;
                            }
                        }
                    }
                    Err(ApiError::Api(-14, _)) => {
                        warn!("WeChat: stale token / session invalid (-14), clearing token and re-login");
                        this.api.state.write().bot_token = None;
                        if let Err(login_err) = this.login().await {
                            warn!("WeChat: re-login failed: {login_err}, pausing {}s", RATE_LIMIT_PAUSE_SECS);
                            tokio::time::sleep(Duration::from_secs(RATE_LIMIT_PAUSE_SECS)).await;
                        } else {
                            info!("WeChat: re-login successful after stale token");
                            consecutive_errors = 0;
                        }
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
                                warn!(
                                    "WeChat: server error ({consecutive_errors}): {e}, retrying in {backoff}s"
                                );
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
}

/// Filter markdown for WeChat: strip image syntax, simplify H5/H6.
fn filter_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for line in text.lines() {
        // Remove image markdown ![alt](url)
        let mut filtered = line.to_string();
        while let Some(start) = filtered.find("![") {
            if let Some(end) = filtered[start..].find("](") {
                let url_start = start + end + 2;
                if let Some(url_end) = filtered[url_start..].find(')') {
                    filtered = format!("{}{}", &filtered[..start], &filtered[url_start + url_end + 1..]);
                    continue;
                }
            }
            break;
        }
        // Convert H5/H6 to bold
        let trimmed = filtered.trim_start();
        let leading_hashes = filtered.len() - trimmed.len();
        if trimmed.starts_with("#####") {
            let content = trimmed.trim_start_matches('#').trim();
            filtered = format!("{}**{}**", &filtered[..leading_hashes], content);
        }
        result.push_str(&filtered);
        result.push('\n');
    }
    result.trim_end().to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_version_encoding() {
        assert_eq!(build_client_version(), 132102); // 2.4.6
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
            list: vec![MessageItem {
                item_type: ITEM_TYPE_TEXT,
                text_item: Some(TextItem {
                    text: "hello".into(),
                }),
                voice_item: None,
                image_item: None,
                video_item: None,
                file_item: None,
            }],
            item_list: vec![],
            context_token: "ctx_tok".into(),
        };
        let event = parse_inbound(&msg);
        assert_eq!(event.sender_wxid, "user1");
        assert_eq!(event.chat_id, "user1");
        assert!(!event.is_group);
        match event.content {
            InboundContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn test_parse_voice_uses_native_asr_text() {
        let msg = IlinkMessage {
            from_user_id: "user1".into(),
            to_user_id: "bot1".into(),
            client_id: "cid_voice".into(),
            create_time_ms: 2000,
            group_id: String::new(),
            message_type: 1,
            message_state: 2,
            list: vec![MessageItem {
                item_type: ITEM_TYPE_VOICE,
                text_item: None,
                voice_item: Some(VoiceItem {
                    text: "转写出来的内容".into(),
                }),
                image_item: None,
                video_item: None,
                file_item: None,
            }],
            item_list: vec![],
            context_token: String::new(),
        };
        let event = parse_inbound(&msg);
        match event.content {
            InboundContent::Text(t) => assert_eq!(t, "转写出来的内容"),
            _ => panic!("expected voice ASR text"),
        }
    }

    #[test]
    fn test_parse_voice_without_asr_is_unknown() {
        let msg = IlinkMessage {
            from_user_id: "user1".into(),
            to_user_id: "bot1".into(),
            client_id: "cid_voice2".into(),
            create_time_ms: 2001,
            group_id: String::new(),
            message_type: 1,
            message_state: 2,
            list: vec![MessageItem {
                item_type: ITEM_TYPE_VOICE,
                text_item: None,
                voice_item: Some(VoiceItem { text: "   ".into() }),
                image_item: None,
                video_item: None,
                file_item: None,
            }],
            item_list: vec![],
            context_token: String::new(),
        };
        assert!(matches!(
            parse_inbound(&msg).content,
            InboundContent::Unknown
        ));
    }

    #[test]
    fn test_dedup() {
        let dedup = DedupState::new();
        // check_and_record returns true if already seen (should skip), false if new.
        assert!(!dedup.check_and_record("msg1")); // new → false (don't skip)
        assert!(dedup.check_and_record("msg1")); // duplicate → true (skip)
        assert!(!dedup.check_and_record("msg2")); // new → false (don't skip)
    }
}
