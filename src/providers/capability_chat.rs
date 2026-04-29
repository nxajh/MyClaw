//! Chat capability: streaming chat interface.

use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

// ── Message types ─────────────────────────────────────────────────────────────

/// Message content segment (multimodal).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { url: String, detail: ImageDetail },
    ImageB64 { b64_json: String, detail: ImageDetail },
    /// Extended thinking block — stored in message history so it can be
    /// re-sent to the model on subsequent turns (Anthropic protocol requires
    /// the model to see its own reasoning).
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

/// Image detail level.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum ImageDetail {
    #[default]
    Auto,
    Low,
    High,
}

/// Chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub parts: Vec<ContentPart>,
    /// Tool call ID for "tool" role messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool call ID (OpenAI: tool_call_id for "tool" role).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls from assistant (OpenAI: tool_calls in assistant message).
    /// Always stored in the canonical ToolCall format regardless of
    /// which provider generated them. Each provider's build_body() is
    /// responsible for translating this into its own wire format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Whether this tool result message indicates an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self { role: role.into(), parts: vec![ContentPart::Text { text: text.into() }], name: None, tool_call_id: None, tool_calls: None, is_error: None }
    }
    pub fn user_text(text: impl Into<String>) -> Self { Self::text("user", text) }
    pub fn assistant_text(text: impl Into<String>) -> Self { Self::text("assistant", text) }
    pub fn system_text(text: impl Into<String>) -> Self { Self::text("system", text) }
    pub fn with_image_url(mut self, url: impl Into<String>) -> Self {
        self.parts.push(ContentPart::ImageUrl { url: url.into(), detail: ImageDetail::Auto });
        self
    }
    /// Collect all text from Text parts.
    pub fn text_content(&self) -> String {
        self.parts.iter().filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("")
    }
}

// ── Streaming types ───────────────────────────────────────────────────────────

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// Stream event from ChatProvider::chat().
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta { text: String },
    Thinking { text: String },
    ToolCallStart { id: String, name: String, initial_arguments: String },
    ToolCallDelta { id: String, delta: String },
    ToolCallEnd { id: String, name: String, arguments: String },
    Usage(ChatUsage),
    Done { reason: StopReason },
    Error(String),
    /// HTTP-level error with status code; used for retry/fallback decisions.
    HttpError { status: u16, message: String },
}

impl StreamEvent {
    /// Whether this error is retryable (429 rate-limit, 503 service unavailable, etc.).
    pub fn is_retryable_error(&self) -> bool {
        match self {
            StreamEvent::HttpError { status, .. } => *status == 429 || *status >= 500,
            StreamEvent::Error(msg) => {
                msg.contains("429") || msg.contains("503") || msg.contains("rate_limit")
            }
            _ => false,
        }
    }
}

/// Why the stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    StopSequence,
    ContentFilter,
    ToolUse,
    Timeout,
}

/// Token usage for a chat response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

// ── Tool calling ──────────────────────────────────────────────────────────────

/// Tool call returned in the response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON string of tool arguments.
    pub arguments: String,
}

impl ToolCall {
    /// Convert to the OpenAI / OpenAI-compatible wire format used in
    /// assistant message tool_calls arrays.
    pub fn to_openai(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": self.arguments,
            }
        })
    }
}

/// Tool specification for providers that support native tool calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

// ── Request / Response ───────────────────────────────────────────────────────

/// Chat request sent to ChatProvider::chat().
pub struct ChatRequest<'a> {
    /// Model identifier (filled by ServiceRegistry from routing config).
    pub model: &'a str,
    /// Message list.
    pub messages: &'a [ChatMessage],
    /// Temperature 0.0–2.0.
    pub temperature: Option<f64>,
    /// Maximum output tokens.
    pub max_tokens: Option<u32>,
    /// Reasoning/thinking configuration (set by config, not by user).
    pub thinking: Option<ThinkingConfig>,
    /// Stop sequences.
    pub stop: Option<Vec<String>>,
    /// Random seed.
    pub seed: Option<u64>,
    /// Tool definitions for providers with native tool calling support.
    pub tools: Option<&'a [ToolSpec]>,
    /// Stream flag (always true; caller must not set false).
    pub stream: bool,
}

#[derive(Debug, Clone)]
pub struct ThinkingConfig {
    /// Reasoning effort: "high" | "medium" | "low"
    pub effort: Option<String>,
    /// Reasoning token budget (upper bound).
    pub budget_tokens: Option<u32>,
}

/// Non-streaming chat response (assembled from StreamEvent by caller).
#[derive(Default)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<ChatUsage>,
    pub reasoning_content: Option<String>,
    pub stop_reason: StopReason,
}

impl ChatResponse {
    /// Collect a streaming response from a `BoxStream<StreamEvent>`.
    pub async fn from_stream(stream: BoxStream<StreamEvent>) -> anyhow::Result<Self> {
        use futures_util::StreamExt;
        let mut text = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage: Option<ChatUsage> = None;

        let mut stream = stream;
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Delta { text: delta } => text.push_str(&delta),
                StreamEvent::Thinking { text: delta } => reasoning_content.push_str(&delta),
                StreamEvent::ToolCallStart { id, name, initial_arguments } => {
                    tool_calls.push(ToolCall { id, name, arguments: initial_arguments });
                }
                StreamEvent::ToolCallDelta { id, delta } => {
                    if !id.is_empty() {
                        if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                            call.arguments.push_str(&delta);
                        } else {
                            tool_calls.push(ToolCall { id, name: String::new(), arguments: delta });
                        }
                    } else if let Some(last) = tool_calls.last_mut() {
                        last.arguments.push_str(&delta);
                    }
                }
                StreamEvent::ToolCallEnd { id, name, arguments } => {
                    if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                        call.name = name;
                        call.arguments = arguments;
                    }
                }
                StreamEvent::Usage(u) => usage = Some(u),
                StreamEvent::Done { reason } => {
                    stop_reason = reason;
                    break;
                }
                StreamEvent::HttpError { message, .. } => {
                    anyhow::bail!("Stream error: HTTP {}", message);
                }
                StreamEvent::Error(e) => anyhow::bail!("Stream error: {}", e),
            }
        }

        Ok(Self {
            text,
            tool_calls,
            usage,
            reasoning_content: if reasoning_content.is_empty() {
                None
            } else {
                Some(reasoning_content)
            },
            stop_reason,
        })
    }
}

// ── ChatProvider trait ───────────────────────────────────────────────────────

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Start a streaming chat. Non-streaming callers collect via ChatResponse::from_stream().
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>;
}
