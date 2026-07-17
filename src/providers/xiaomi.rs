//! Xiaomi MiMo provider — dual-protocol: Anthropic or OpenAI.
//!
//! The protocol is determined by the user's config (`protocol = "messages"` or
//! `protocol = "chat_completions"`). When Messages, uses mimo-v2.5-pro with MiMo-specific
//! thinking patches. When OpenAI, uses the standard OpenAI Chat Completions
//! client which supports text, image, video, and audio inputs.
//!
//! When the request contains video or audio ContentPart::File entries and the
//! current model doesn't support those modalities, the provider automatically
//! switches to `mimo-v2.5` (which supports video_url / input_audio).

use async_trait::async_trait;

use crate::providers::capability_chat::ContentPart;
use crate::providers::media::FileModality;
use crate::providers::{BoxStream, ChatProvider, ChatRequest, SharedApiKey, StreamEvent};

const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/anthropic";

/// Model name used when the request contains video or audio content.
/// mimo-v2.5 supports video_url and input_audio per official documentation.
const MEDIA_MODEL: &str = "mimo-v2.5";

#[derive(Clone)]
pub struct XiaomiProvider {
    base_url: String,
    api_key: SharedApiKey,
    user_agent: Option<String>,
    openai: bool,
}

impl XiaomiProvider {
    pub fn new(api_key: impl Into<SharedApiKey>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            user_agent: None,
            openai: false,
        }
    }

    pub fn with_base_url(api_key: impl Into<SharedApiKey>, base_url: String) -> Self {
        Self {
            base_url,
            api_key: api_key.into(),
            user_agent: None,
            openai: false,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    /// When set, requests are sent via OpenAI Chat Completions protocol.
    pub fn with_openai(mut self) -> Self {
        self.openai = true;
        self
    }

    /// Derive the OpenAI-compatible base URL from the Anthropic base URL.
    /// e.g. `https://api.xiaomimimo.com/anthropic` → `https://api.xiaomimimo.com/v1`
    fn openai_base_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let stripped = if let Some(pos) = base.rfind("/anthropic") {
            base[..pos].to_string()
        } else {
            base.to_string()
        };
        format!("{}/v1", stripped)
    }
}

/// Returns true if any message contains a `ContentPart::File` with video or
/// audio modality.
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
        // Detect video/audio in the request and switch to media-capable model.
        let needs_media_model = has_media_content(req.messages);
        let effective_model = if needs_media_model {
            tracing::info!(
                original_model = %req.model,
                media_model = MEDIA_MODEL,
                "XiaomiProvider: request contains video/audio, switching to media model"
            );
            MEDIA_MODEL
        } else {
            req.model
        };

        let media_req = ChatRequest {
            model: effective_model,
            ..req
        };

        if self.openai {
            // ── OpenAI path ──────────────────────────────────────────────
            use crate::providers::protocols::openai::chat_completions::OpenAiChatCompletionsClient;

            let openai_base = self.openai_base_url();
            tracing::debug!(
                base_url = %openai_base,
                model = %media_req.model,
                "XiaomiProvider: using OpenAI protocol"
            );

            let client = OpenAiChatCompletionsClient::new(self.api_key.get(), openai_base);
            let client = if let Some(ref ua) = self.user_agent {
                client.with_user_agent(ua.clone())
            } else {
                client
            };
            client.chat(media_req)
        } else {
            // ── Anthropic path (default) ─────────────────────────────────
            use crate::providers::protocols::anthropic::messages::AnthropicMessagesClient;

            let thinking_enabled = media_req.thinking.as_ref().is_some_and(|t| t.enabled);
            let mut body =
                crate::providers::protocols::anthropic::message_rendering::build_anthropic_body(
                    &media_req,
                );

            // MiMo-specific: MiMo ALWAYS requires a thinking block in every
            // assistant message that contains tool_use.
            patch_mimo_thinking(&mut body);

            let client = AnthropicMessagesClient::new(self.api_key.get(), self.base_url.clone());
            let client = if let Some(ref ua) = self.user_agent {
                client.with_user_agent(ua.clone())
            } else {
                client
            };
            client.chat_with_body(body, thinking_enabled)
        }
    }
}
