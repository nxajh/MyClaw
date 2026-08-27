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

use std::time::Duration;

#[cfg(feature = "wechat")]
use aes::Aes128;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[cfg(feature = "wechat")]
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
#[cfg(feature = "wechat")]
use ecb::{Decryptor, Encryptor};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::channels::message::{
    ChannelFile, ChannelFileMeta, ChannelInboundMessage, ChannelMessageContent, LocalFileBody,
    MessageReceiver, MessageSender,
};
use crate::channels::shared::{InboundDebouncer, TypingKeepAlive, TypingParams};
use crate::config::channel::WechatAccountConfig;
use crate::{Channel, DedupState, ProcessingStatus};

// ── Constants ─────────────────────────────────────────────────────────────────

pub(crate) const CHANNEL_VERSION: &str = "2.4.6";
pub(crate) const ILINK_APP_ID: &str = "bot";
pub(crate) const QR_POLL_INTERVAL_SECS: u64 = 1;
pub(crate) const QR_MAX_ATTEMPTS: u64 = 60;
pub(crate) const RATE_LIMIT_PAUSE_SECS: u64 = 3600;
pub(crate) const MAX_CONSECUTIVE_ERRORS: u32 = 10;
pub(crate) const TYPING_TICKET_TTL: Duration = Duration::from_secs(300);

pub(crate) const MESSAGE_TYPE_BOT: i64 = 2;
pub(crate) const MESSAGE_STATE_FINISH: i64 = 2;
pub(crate) const MESSAGE_STATE_GENERATING: i64 = 1;
pub(crate) const ITEM_TYPE_TEXT: i64 = 1;
pub(crate) const ITEM_TYPE_IMAGE: i64 = 2;
pub(crate) const ITEM_TYPE_VOICE: i64 = 3;
pub(crate) const ITEM_TYPE_FILE: i64 = 4;
pub(crate) const ITEM_TYPE_VIDEO: i64 = 5;
pub(crate) const ITEM_TYPE_TOOL_CALL_START: i64 = 11;
pub(crate) const ITEM_TYPE_TOOL_CALL_RESULT: i64 = 12;
pub(crate) const TYPING_STATUS_TYPING: i64 = 1;
pub(crate) const TYPING_STATUS_CANCEL: i64 = 2;

/// CDN base URL for media download/upload (separate from the API base).
pub(crate) const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

// Upload media type constants (proto: UploadMediaType).
pub(crate) const UPLOAD_MEDIA_IMAGE: i64 = 1;
pub(crate) const UPLOAD_MEDIA_VIDEO: i64 = 2;
pub(crate) const UPLOAD_MEDIA_FILE: i64 = 3;


pub(crate) mod crypto;
pub(crate) mod types;
pub(crate) mod state;
pub(crate) mod api_client;
pub(crate) mod inbound;
pub(crate) mod error;

pub(crate) use crypto::*;
pub(crate) use types::*;
pub(crate) use state::*;
pub(crate) use api_client::*;
pub(crate) use inbound::*;
pub(crate) use error::*;

// ── WechatChannel ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WechatChannel {
    api: ApiClient,
    config: WechatAccountConfig,
    dedup: DedupState,
    /// Debounce window in milliseconds (0 = disabled) + merge buffer.
    debouncer: InboundDebouncer,
    allowed_groups: Option<Vec<String>>,
    /// Active typing keep-alive tasks, keyed by recipient (wxid).
    typing: TypingKeepAlive,
}

impl WechatChannel {
    pub fn new(account_id: String, config: WechatAccountConfig) -> Self {
        let ch = Self {
            api: ApiClient::new(&config, account_id),
            debouncer: InboundDebouncer::new(config.debounce_ms, None),
            allowed_groups: config.allowed_groups.clone(),
            typing: TypingKeepAlive::new(),
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
        let (group_mode, group_allowlist) = match &self.allowed_groups {
            None => (GroupAuthMode::Reject, AllowList::All),
            Some(groups) if groups.iter().any(|g| g == "*") => {
                (GroupAuthMode::Open, AllowList::All)
            }
            Some(list) => (GroupAuthMode::Open, AllowList::Whitelist(list.clone())),
        };
        ChannelSecurityPolicy {
            allowed_users: AllowList::from_config(Some(self.config.allowed_users.clone())),
            group_mode,
            group_allowlist,
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

impl WechatChannel {
    /// Buffer an inbound message for debounce merging. Buffer/merge/timer
    /// mechanics live in the shared `InboundDebouncer` (silent on dispatch
    /// errors, matching the previous behavior).
    async fn debounce_send(
        &self,
        msg: ChannelInboundMessage,
        tx: mpsc::Sender<ChannelInboundMessage>,
    ) {
        self.debouncer.push(msg, tx).await;
    }

    /// Start a typing keep-alive background task for a recipient.
    ///
    /// WeChat's typing indicator expires after a few seconds. The shared
    /// keep-alive loop re-sends it every 3 seconds until
    /// `stop_typing_keepalive` is called, with a 120s TTL cap and a circuit
    /// breaker on 3 consecutive send failures.
    fn start_typing_keepalive(&self, recipient: &str) {
        let api = self.api.clone();
        let recipient_key = recipient.to_string();

        self.typing.start(
            recipient,
            TypingParams {
                interval: Duration::from_secs(3),
                max_duration: Some(Duration::from_secs(120)),
                max_consecutive_failures: 3,
                on_expired: None,
                on_breaker: Some(Box::new(|_failures, recipient_key, e| {
                    debug!("WeChat: typing keep-alive circuit breaker for {recipient_key}: {e}");
                })),
                on_exit: None,
            },
            move || {
                let api = api.clone();
                move || {
                    let api = api.clone();
                    let to = recipient_key.clone();
                    async move { api.send_typing(&to, true).await.map_err(|e| e.to_string()) }
                }
            },
        );
    }

    /// Stop (abort) the typing keep-alive task and send a cancel.
    fn stop_typing_keepalive(&self, recipient: &str) {
        self.typing.stop(recipient);
    }
}

#[async_trait]
impl Channel for WechatChannel {
    fn name(&self) -> &str {
        "wechat"
    }

    fn capabilities(&self) -> &crate::channels::message::ChannelCapabilities {
        &WECHAT_CAPS
    }

    fn tts_enabled(&self) -> bool {
        self.config.tts
    }

    async fn on_status(&self, recipient: &str, status: ProcessingStatus) {
        match status {
            ProcessingStatus::Thinking => {
                // Generate a per-turn run_id shared by all tool progress
                // messages and the final text reply for this recipient.
                self.api
                    .state
                    .write()
                    .run_ids
                    .insert(recipient.to_string(), uuid::Uuid::new_v4().to_string());

                self.start_typing_keepalive(recipient);
            }
            ProcessingStatus::Done | ProcessingStatus::Error => {
                // Clear the run_id for this turn.
                self.api.state.write().run_ids.remove(recipient);

                self.stop_typing_keepalive(recipient);
                if let Err(e) = self.api.send_typing(recipient, false).await {
                    debug!("WeChat: typing cancel failed: {e}");
                }
            }
        }
    }

    async fn on_tool_event(&self, recipient: &str, event: crate::channels::ToolEvent) {
        let ctx_token = self.api.state.read().context_tokens.get(recipient).cloned();
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
        // Stop typing keep-alive — the real reply is being sent.
        self.stop_typing_keepalive(&message.receiver.id);

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

            let mime = file
                .meta
                .mime_type
                .as_deref()
                .unwrap_or("application/octet-stream");
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

            let aes_key_b64 = BASE64.encode(hex::decode(&uploaded.aeskey_hex).unwrap_or_default());

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
                    run_id: self
                        .api
                        .state
                        .read()
                        .run_ids
                        .get(&message.receiver.id)
                        .cloned(),
                },
                base_info: build_base_info(),
            };
            let resp = self
                .api
                .api_post(
                    "ilink/bot/sendmessage",
                    &serde_json::to_value(&req).unwrap(),
                )
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
                            let scope = if event.is_group {
                                crate::channels::MessageScope::Group {
                                    id: &event.chat_id,
                                    has_mention: false,
                                }
                            } else {
                                crate::channels::MessageScope::Direct
                            };
                            match this.check_authorization(&event.sender_wxid, scope) {
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
                                InboundContent::Voice { text, media } => {
                                    let mut content = ChannelMessageContent::text(text);
                                    if !media.encrypt_query_param.is_empty()
                                        || !media.full_url.is_empty()
                                    {
                                        match this.api.download_cdn_media(&media, None).await {
                                            Ok(data) => {
                                                let tmp_dir = std::env::temp_dir();
                                                let tmp_path = tmp_dir.join(format!(
                                                    "wechat_voice_{}",
                                                    uuid::Uuid::new_v4()
                                                ));
                                                if std::fs::write(&tmp_path, &data).is_ok() {
                                                    content.files.push(ChannelFile {
                                                        meta: ChannelFileMeta {
                                                            file_name: format!(
                                                                "voice_{}.silk",
                                                                event.raw_timestamp
                                                            ),
                                                            mime_type: Some(
                                                                "audio/silk".to_string(),
                                                            ),
                                                            size_bytes: Some(data.len() as u64),
                                                            source_url: None,
                                                        },
                                                        body: std::sync::Arc::new(
                                                            LocalFileBody::new(&tmp_path),
                                                        ),
                                                    });
                                                }
                                            }
                                            Err(e) => {
                                                warn!("WeChat: voice download failed: {e}");
                                            }
                                        }
                                    }
                                    content
                                }
                                InboundContent::MediaRequest {
                                    item_type,
                                    media,
                                    aeskey_hex,
                                    filename,
                                } => {
                                    match this
                                        .api
                                        .download_cdn_media(&media, aeskey_hex.as_deref())
                                        .await
                                    {
                                        Ok(data) => {
                                            let mime = match item_type {
                                                ITEM_TYPE_IMAGE => "image/jpeg",
                                                ITEM_TYPE_VIDEO => "video/mp4",
                                                ITEM_TYPE_FILE => "application/octet-stream",
                                                _ => "application/octet-stream",
                                            };
                                            let tmp_dir = std::env::temp_dir();
                                            let tmp_path = tmp_dir.join(format!(
                                                "wechat_media_{}",
                                                uuid::Uuid::new_v4()
                                            ));
                                            if let Err(e) = std::fs::write(&tmp_path, &data) {
                                                warn!(
                                                    "WeChat: failed to write media temp file: {e}"
                                                );
                                                continue;
                                            }
                                            let mut content =
                                                ChannelMessageContent::text(String::new());
                                            content.files.push(ChannelFile {
                                                meta: ChannelFileMeta {
                                                    file_name: filename,
                                                    mime_type: Some(mime.to_string()),
                                                    size_bytes: Some(data.len() as u64),
                                                    source_url: None,
                                                },
                                                body: std::sync::Arc::new(LocalFileBody::new(
                                                    &tmp_path,
                                                )),
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
                                persist_context_tokens(&this.api.state.read());
                            }

                            // Fetch typing_ticket for this user (cached with TTL)
                            {
                                let need_fetch = {
                                    let state = this.api.state.read();
                                    match state.typing_tickets.get(&event.sender_wxid) {
                                        Some((_, ts)) => ts.elapsed() > TYPING_TICKET_TTL,
                                        None => true,
                                    }
                                };
                                if need_fetch {
                                    if let Ok(config) =
                                        this.api.get_config(&event.sender_wxid).await
                                    {
                                        if !config.typing_ticket.is_empty() {
                                            this.api.state.write().typing_tickets.insert(
                                                event.sender_wxid.clone(),
                                                (config.typing_ticket, std::time::Instant::now()),
                                            );
                                        }
                                    }
                                }
                            }

                            let channel_msg = ChannelInboundMessage {
                                id: event.msg_id,
                                sender: MessageSender::new(event.sender_wxid),
                                receiver: MessageReceiver::new(event.chat_id),
                                content,
                                timestamp: event.raw_timestamp as u64,
                                interruption_scope_id: None,
                                silenced_override: None,
                                run_mode: Default::default(),
                            };
                            this.debounce_send(channel_msg, tx.clone()).await;
                        }
                    }
                    Err(ApiError::Api(-14, _)) => {
                        warn!(
                            "WeChat: stale token / session invalid (-14), clearing token and re-login"
                        );
                        this.api.state.write().bot_token = None;
                        if let Err(login_err) = this.login().await {
                            warn!(
                                "WeChat: re-login failed: {login_err}, pausing {}s",
                                RATE_LIMIT_PAUSE_SECS
                            );
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
                    filtered = format!(
                        "{}{}",
                        &filtered[..start],
                        &filtered[url_start + url_end + 1..]
                    );
                    continue;
                }
            }
            break;
        }
        // Replace bare '<' (not a valid HTML tag start) with full-width '＜'
        // to prevent WeChat markdown renderer from treating "<3次" as an HTML tag,
        // which would swallow the rest of the line and break **bold** rendering.
        // WeChat does not decode HTML entities, so &lt; would show literally.
        {
            let chars: Vec<char> = filtered.chars().collect();
            let mut escaped = String::with_capacity(filtered.len());
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '<' {
                    let next = chars.get(i + 1).copied().unwrap_or(' ');
                    if next.is_ascii_alphabetic() || next == '/' {
                        escaped.push('<');
                    } else {
                        escaped.push('＜');
                    }
                } else {
                    escaped.push(chars[i]);
                }
                i += 1;
            }
            filtered = escaped;
        }
        // Convert H5/H6 to bold. CommonMark requires whitespace (or EOL)
        // after the '#' run for it to be a heading at all — issue #114:
        // without that check, "#####108" (a literal hash-prefixed number,
        // not a heading) had its hashes silently swallowed into "**108**".
        let trimmed = filtered.trim_start();
        let leading_hashes = filtered.len() - trimmed.len();
        if trimmed.starts_with("#####") {
            let after_hashes = trimmed.trim_start_matches('#');
            let is_heading = after_hashes.is_empty() || after_hashes.starts_with(char::is_whitespace);
            if is_heading {
                let content = after_hashes.trim();
                filtered = format!("{}**{}**", &filtered[..leading_hashes], content);
            }
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

    /// issue #114 (顺带修): a real H5 heading ("##### Title") still
    /// collapses to bold.
    #[test]
    fn test_filter_markdown_h5_heading_still_converts() {
        assert_eq!(filter_markdown("##### Title"), "**Title**");
    }

    /// issue #114 (顺带修): "#####108" is a literal hash-prefixed number,
    /// not a heading (no whitespace after the '#' run) — the hashes must
    /// not be silently swallowed into "**108**".
    #[test]
    fn test_filter_markdown_hash_digit_not_treated_as_heading() {
        assert_eq!(filter_markdown("#####108"), "#####108");
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
                    media: CDNMedia::default(),
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
            InboundContent::Voice { text, .. } => assert_eq!(text, "转写出来的内容"),
            _ => panic!("expected voice content with ASR text"),
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
                voice_item: Some(VoiceItem {
                    text: "   ".into(),
                    media: CDNMedia::default(),
                }),
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
