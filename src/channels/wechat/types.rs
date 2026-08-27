use super::*;
// ── API types ─────────────────────────────────────────────────────────────────

pub(crate) fn build_base_info() -> BaseInfo {
    BaseInfo {
        channel_version: CHANNEL_VERSION.to_string(),
        bot_agent: Some("MyClaw".to_string()),
    }
}

pub(crate) fn build_client_version() -> u32 {
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
pub(crate) struct BaseInfo {
    pub(crate) channel_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bot_agent: Option<String>,
}

// ── Inbound message types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IlinkMessage {
    #[serde(default)]
    pub(crate) from_user_id: String,
    #[serde(default)]
    pub(crate) to_user_id: String,
    #[serde(default)]
    pub(crate) client_id: String,
    #[serde(default)]
    pub(crate) create_time_ms: i64,
    #[serde(default)]
    pub(crate) group_id: String,
    #[serde(rename = "type", default)]
    pub(crate) message_type: i64,
    #[serde(rename = "state", default)]
    pub(crate) message_state: i64,
    #[serde(default)]
    pub(crate) list: Vec<MessageItem>,
    #[serde(rename = "item_list", default)]
    pub(crate) item_list: Vec<MessageItem>,
    #[serde(default)]
    pub(crate) context_token: String,
}

impl IlinkMessage {
    pub(crate) fn chat_id(&self) -> &str {
        if self.group_id.is_empty() {
            &self.from_user_id
        } else {
            &self.group_id
        }
    }
    pub(crate) fn is_group(&self) -> bool {
        !self.group_id.is_empty()
    }
    /// Return the item list from the API response.
    /// The iLink API uses `item_list`, but older versions used `list`.
    pub(crate) fn items(&self) -> &[MessageItem] {
        if !self.item_list.is_empty() {
            &self.item_list
        } else {
            &self.list
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageItem {
    #[serde(rename = "type", default)]
    pub(crate) item_type: i64,
    #[serde(default)]
    pub(crate) text_item: Option<TextItem>,
    #[serde(default)]
    pub(crate) voice_item: Option<VoiceItem>,
    #[serde(default)]
    pub(crate) image_item: Option<InboundImageItem>,
    #[serde(default)]
    pub(crate) video_item: Option<InboundVideoItem>,
    #[serde(default)]
    pub(crate) file_item: Option<InboundFileItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TextItem {
    #[serde(default)]
    pub(crate) text: String,
}

/// Inbound voice item (`type == 3`). WeChat ships a native ASR transcription in
/// `text`; we use it directly as the message body (the SILK media in `media` is
/// AES-encrypted and not downloaded). Other fields (encoding/duration) are
/// ignored.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VoiceItem {
    #[serde(default)]
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) media: CDNMedia,
}

/// CDN media reference (`CDNMedia`). `aes_key` is base64-encoded bytes in JSON.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CDNMedia {
    #[serde(default)]
    pub(crate) encrypt_query_param: String,
    #[serde(default)]
    pub(crate) aes_key: String,
    #[serde(default, rename = "type")]
    pub(crate) encrypt_type: i64,
    #[serde(default)]
    pub(crate) full_url: String,
}

/// Inbound image item (`type == 2`). `aeskey` is a hex-encoded raw 16-byte AES
/// key, preferred over `media.aes_key` for inbound decryption.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InboundImageItem {
    #[serde(default)]
    pub(crate) media: CDNMedia,
    /// Raw AES-128 key as hex string (16 bytes); preferred over media.aes_key.
    #[serde(default)]
    pub(crate) aeskey: String,
}

/// Inbound video item (`type == 5`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InboundVideoItem {
    #[serde(default)]
    pub(crate) media: CDNMedia,
}

/// Inbound file item (`type == 4`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InboundFileItem {
    #[serde(default)]
    pub(crate) media: CDNMedia,
    #[serde(default)]
    pub(crate) file_name: String,
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GetUpdatesResponse {
    #[serde(default)]
    pub(crate) ret: i64,
    #[serde(default)]
    pub(crate) errcode: i64,
    #[serde(default)]
    pub(crate) errmsg: String,
    #[serde(rename = "get_updates_buf", default)]
    pub(crate) get_updates_buf: String,
    #[serde(default)]
    pub(crate) longpolling_timeout_ms: u64,
    #[serde(default)]
    pub(crate) msgs: Vec<IlinkMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GetConfigResponse {
    #[serde(default)]
    pub(crate) ret: i64,
    #[serde(default)]
    pub(crate) errmsg: String,
    #[serde(default)]
    pub(crate) wxid: String,
    #[serde(default)]
    pub(crate) nickname: String,
    #[serde(default)]
    pub(crate) typing_ticket: String,
    #[serde(default)]
    pub(crate) aeskey: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct QrCodeResponse {
    #[serde(default)]
    pub(crate) qrcode: String,
    #[serde(default)]
    pub(crate) qrcode_img_content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct QrStatus {
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) bot_token: String,
    #[serde(default)]
    pub(crate) ilink_bot_id: String,
    #[serde(default)]
    pub(crate) baseurl: String,
    #[serde(default)]
    pub(crate) ilink_user_id: String,
    #[serde(default)]
    pub(crate) nickname: String,
}

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GetUpdatesRequest {
    #[serde(rename = "get_updates_buf")]
    pub(crate) get_updates_buf: String,
    #[serde(rename = "base_info")]
    pub(crate) base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendMessageRequest {
    #[serde(rename = "msg")]
    pub(crate) msg: SendMessageMsg,
    #[serde(rename = "base_info")]
    pub(crate) base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct SendMessageMsg {
    #[serde(default)]
    pub(crate) from_user_id: String,
    pub(crate) to_user_id: String,
    pub(crate) client_id: String,
    pub(crate) message_type: i64,
    pub(crate) message_state: i64,
    pub(crate) item_list: Vec<SendMessageItem>,
    #[serde(rename = "context_token", skip_serializing_if = "Option::is_none")]
    pub(crate) context_token: Option<String>,
    #[serde(rename = "run_id", skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendMessageItem {
    #[serde(rename = "type")]
    pub(crate) item_type: i64,
    #[serde(skip_serializing_if = "Option::is_none", rename = "text_item")]
    pub(crate) text_item: Option<SendTextItem>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "image_item")]
    pub(crate) image_item: Option<SendImageItem>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "video_item")]
    pub(crate) video_item: Option<SendVideoItem>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "file_item")]
    pub(crate) file_item: Option<SendFileItem>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendTextItem {
    pub(crate) text: String,
}

/// CDN media reference for outbound messages.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendCDNMedia {
    #[serde(rename = "encrypt_query_param")]
    pub(crate) encrypt_query_param: String,
    #[serde(rename = "aes_key")]
    pub(crate) aes_key: String,
    #[serde(rename = "encrypt_type")]
    pub(crate) encrypt_type: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendImageItem {
    pub(crate) media: SendCDNMedia,
    pub(crate) mid_size: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendVideoItem {
    pub(crate) media: SendCDNMedia,
    pub(crate) video_size: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendFileItem {
    pub(crate) media: SendCDNMedia,
    pub(crate) file_name: String,
    pub(crate) len: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SendTypingRequest {
    #[serde(rename = "ilink_user_id")]
    pub(crate) ilink_user_id: String,
    #[serde(rename = "typing_ticket")]
    pub(crate) typing_ticket: String,
    pub(crate) status: i64,
    #[serde(rename = "base_info")]
    pub(crate) base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GetConfigRequest {
    #[serde(rename = "ilink_user_id")]
    pub(crate) ilink_user_id: String,
    #[serde(rename = "base_info")]
    pub(crate) base_info: BaseInfo,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GetBotQrCodeRequest {
    #[serde(rename = "base_info")]
    pub(crate) base_info: BaseInfo,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GetQrCodeStatusRequest {
    #[serde(rename = "qrcode")]
    pub(crate) qrcode: String,
    #[serde(rename = "base_info")]
    pub(crate) base_info: BaseInfo,
}

// ── API error ─────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
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
