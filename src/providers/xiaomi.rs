//! Xiaomi MiMo provider — dual-protocol: Anthropic (default) + OpenAI (media).
//!
//! Text and image requests use the Anthropic Messages API (with MiMo-specific
//! thinking patches). When messages contain video or audio `ContentPart::File`
//! entries, the provider transparently switches to the OpenAI Chat Completions
//! endpoint which supports `video_url` and `input_audio` content types.
//!
//! Supported models (per official docs https://mimo.mi.com/docs/zh-CN/api/chat/openai-api):
//! - mimo-v2.5-pro  — text + image only (Anthropic)
//! - mimo-v2.5      — text + image + video + audio (OpenAI)
//! - mimo-v2-omni   — text + image + video + audio (OpenAI)

use async_trait::async_trait;

use crate::providers::capability_chat::ContentPart;
use crate::providers::media::FileModality;
use crate::providers::protocols::anthropic::message_rendering::build_anthropic_body;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/anthropic";

/// Model name used for video/audio requests (OpenAI endpoint).
/// mimo-v2.5 supports video + audio per official documentation.
const MEDIA_MODEL: &str = "mimo-v2.5";

#[derive(Clone)]
pub struct XiaomiProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl XiaomiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            user_agent: None,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            base_url,
            api_key,
            user_agent: None,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    /// Derive the OpenAI-compatible base URL from the Anthropic base URL.
    /// e.g. `https://api.xiaomimimo.com/anthropic` → `https://api.xiaomimimo.com`
    fn openai_base_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if let Some(pos) = base.rfind("/anthropic") {
            base[..pos].to_string()
        } else {
            base.to_string()
        }
    }
}

/// Returns true if any message contains a `ContentPart::File` with video or
/// audio modality. These require the OpenAI endpoint.
fn has_media_content(messages: &[crate::providers::ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let ContentPart::File {
                path, mime_type, ..
            } = p
            {
                matches!(
                    crate::providers::media::modality_from_mime(mime_type.as_deref(), path),
                    FileModality::Video | FileModality::Audio
                )
            } else {
                false
            }
        })
    })
}

/// MiMo requires that every assistant message with tool_calls also contains a
/// `reasoning_content` (thinking) block when thinking mode is active. If the
/// model didn't produce one (or it was lost during compaction), we insert an
/// empty thinking block to avoid a 400 error from the MiMo API.
fn patch_mimo_thinking(body: &mut serde_json::Value) {
    let messages = match body.get_mut("messages") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => return,
    };

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let content = match msg.get_mut("content") {
            Some(serde_json::Value::Array(arr)) => arr,
            _ => continue,
        };

        let has_thinking = content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"));
        let has_tool_use = content
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"));

        if !has_thinking && has_tool_use {
            content.insert(0, serde_json::json!({"type": "thinking", "thinking": ""}));
        }
    }
}

#[async_trait]
impl ChatProvider for XiaomiProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        if has_media_content(req.messages) {
            // ── OpenAI path for video / audio ────────────────────────────────
            tracing::info!(
                model = %req.model,
                media_model = MEDIA_MODEL,
                "XiaomiProvider: detected video/audio content, routing via OpenAI protocol"
            );

            use crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient;

            let openai_base = self.openai_base_url();

            // Override model to the media-capable one.
            let media_req = ChatRequest {
                model: MEDIA_MODEL,
                ..req
            };

            let client = OpenAiChatCompletionsClient::new(
                self.api_key.clone(),
                openai_base,
            );
            let client = if let Some(ref ua) = self.user_agent {
                client.with_user_agent(ua.clone())
            } else {
                client
            };
            // OpenAiChatCompletionsClient.chat() builds the body internally
            // via render_openai_chat_body which already handles video_url,
            // input_audio, and image_url content parts.
            client.chat(media_req)
        } else {
            // ── Anthropic path (default) ─────────────────────────────────────
            use crate::providers::protocols::anthropic::messages::AnthropicMessagesClient;

            let thinking_enabled = req.thinking.as_ref().is_some_and(|t| t.enabled);
            let mut body = build_anthropic_body(&req);

            // MiMo-specific: MiMo ALWAYS requires a thinking block in every assistant
            // message that contains tool_use, regardless of whether the thinking
            // parameter is enabled.
            patch_mimo_thinking(&mut body);

            let client = AnthropicMessagesClient::new(self.api_key.clone(), self.base_url.clone());
            let client = if let Some(ref ua) = self.user_agent {
                client.with_user_agent(ua.clone())
            } else {
                client
            };
            client.chat_with_body(body, thinking_enabled)
        }
    }
}
