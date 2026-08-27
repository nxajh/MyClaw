use super::*;
// ── Inbound event ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) enum InboundContent {
    Text(String),
    Voice {
        text: String,
        media: CDNMedia,
    },
    MediaRequest {
        item_type: i64,
        media: CDNMedia,
        aeskey_hex: Option<String>,
        filename: String,
    },
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct InboundEvent {
    pub(crate) msg_id: String,
    pub(crate) sender_wxid: String,
    pub(crate) chat_id: String,
    pub(crate) is_group: bool,
    pub(crate) content: InboundContent,
    pub(crate) context_token: String,
    pub(crate) raw_timestamp: i64,
}

pub(crate) fn parse_inbound(msg: &IlinkMessage) -> InboundEvent {
    let content = match msg.items().first() {
        Some(first) if first.item_type == ITEM_TYPE_TEXT => InboundContent::Text(
            first
                .text_item
                .as_ref()
                .map(|t| t.text.clone())
                .unwrap_or_default(),
        ),
        // Voice: use WeChat's native ASR transcription as text, and download
        // the SILK audio media alongside.
        Some(first) if first.item_type == ITEM_TYPE_VOICE => {
            let voice = first.voice_item.as_ref();
            let text = voice.map(|v| v.text.clone()).unwrap_or_default();
            let media = voice.map(|v| v.media.clone()).unwrap_or_default();
            if text.trim().is_empty()
                && media.encrypt_query_param.is_empty()
                && media.full_url.is_empty()
            {
                InboundContent::Unknown
            } else {
                InboundContent::Voice { text, media }
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
