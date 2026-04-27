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
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self { role: role.into(), parts: vec![ContentPart::Text { text: text.into() }], name: None }
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
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, delta: String },
    ToolCallEnd { id: String, name: String, arguments: String },
    Usage(ChatUsage),
    Done { reason: StopReason },
    Error(String),
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

// ── ChatProvider trait ───────────────────────────────────────────────────────

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Start a streaming chat. Non-streaming callers collect via ChatResponse::from_stream().
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>>;
}