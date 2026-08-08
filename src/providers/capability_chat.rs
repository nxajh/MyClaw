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
    Text {
        text: String,
    },
    /// A workspace-relative or absolute file reference. New inbound media is
    /// stored under `sessions/<session_id>/files/` and represented with this
    /// canonical form in history; provider rendering decides whether to inline it,
    /// upload it, send a URL/ref, or render a marker.
    File {
        path: String,
        #[serde(rename = "mime", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
    },
    /// Extended thinking block — stored in message history so it can be
    /// re-sent to the model on subsequent turns (Anthropic protocol requires
    /// the model to see its own reasoning, including the opaque signature).
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        /// Anthropic-issued signature that must be echoed back in subsequent
        /// turns when this block appears in the conversation history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// Lowercase hex SHA-256 of `bytes`, used for stable content-derived names.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    /// Model that generated this message (for assistant messages only).
    /// Used by renderers to decide whether thinking blocks / thought
    /// signatures originated from a different provider and must be
    /// dropped or replaced with dummy values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// LLM usage for assistant messages, persisted for observability and cost/cache analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ChatMessageUsage>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            parts: vec![ContentPart::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
        }
    }
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::text("user", text)
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::text("assistant", text)
    }
    pub fn system_text(text: impl Into<String>) -> Self {
        Self::text("system", text)
    }
    /// Collect all text from Text parts.
    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

// ── Streaming types ───────────────────────────────────────────────────────────

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// Stream event from ChatProvider::chat().
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta {
        text: String,
    },
    Thinking {
        text: String,
    },
    /// Opaque signature for the preceding thinking block (Anthropic extended
    /// thinking). Must be stored alongside the thinking text and echoed back
    /// in subsequent requests.
    ThinkingSignature {
        signature: String,
    },
    ToolCallStart {
        id: String,
        name: String,
        initial_arguments: String,
    },
    ToolCallDelta {
        index: u32,
        id: String,
        name: String,
        delta: String,
    },
    ToolCallEnd {
        id: String,
        name: String,
        arguments: String,
    },
    Usage(ChatUsage),
    /// The model that actually produced this stream. Emitted by the fallback
    /// chain when a non-primary entry completes a request (the caller's
    /// `model_id` is then stale). Direct/provider paths never emit it — the
    /// caller's model_id is authoritative in that case.
    ModelUsed {
        model: String,
    },
    Done {
        reason: StopReason,
    },
    Error(String),
    /// HTTP-level error with status code; used for retry/fallback decisions.
    HttpError {
        status: u16,
        message: String,
    },
}

impl StreamEvent {
    /// Whether this error is retryable (legacy heuristic; prefer `ClassifiedError`).
    pub fn is_retryable_error(&self) -> bool {
        match self {
            StreamEvent::HttpError { status, .. } => *status == 429 || *status >= 500,
            StreamEvent::Error(msg) => {
                msg.contains("429") || msg.contains("503") || msg.contains("rate_limit")
            }
            _ => false,
        }
    }

    /// Classify this event into a structured error (if it's an error variant).
    pub fn classify(&self) -> Option<crate::providers::ClassifiedError> {
        match self {
            StreamEvent::HttpError { status, message } => Some(
                crate::providers::ClassifiedError::from_http(*status, Some(message)),
            ),
            StreamEvent::Error(msg) => Some(crate::providers::ClassifiedError::from_message(msg)),
            _ => None,
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
    /// The request exceeded the model's context window (the provider rejected it
    /// rather than completing). Distinct from `EndTurn` so the agent can force a
    /// compaction and retry instead of treating the empty body as a normal stop.
    ContextOverflow,
}

/// True when a provider `finish_reason` string signals a context-window
/// overflow (e.g. GLM's `model_context_window_exceeded`). Kept protocol-agnostic
/// so every renderer maps these to [`StopReason::ContextOverflow`] uniformly.
/// Note: a plain `"length"` (output-token cap) is NOT an overflow.
pub fn is_context_overflow_reason(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("context")
        && (r.contains("exceed") || r.contains("window") || r.contains("overflow"))
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

/// Per-assistant-message LLM usage persisted with history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessageUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
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
        // Sanitize: some providers (e.g. xAI) reject empty or non-JSON arguments
        // with HTTP 400. Replace with "{}" so malformed historical entries don't
        // break future requests.
        let args = if self.arguments.trim().is_empty()
            || serde_json::from_str::<serde_json::Value>(&self.arguments).is_err()
        {
            "{}".to_string()
        } else {
            self.arguments.clone()
        };
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": args,
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
    /// Model identifier (filled by ProviderRegistry from routing config).
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
    /// Whether thinking/reasoning is enabled. Derived from model config's `reasoning` field.
    pub enabled: bool,
    /// Reasoning effort: "high" | "medium" | "low". Configurable at runtime.
    pub effort: Option<String>,
}

/// Non-streaming chat response (assembled from StreamEvent by caller).
#[derive(Default)]
pub struct ChatResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<ChatUsage>,
    pub reasoning_content: Option<String>,
    /// Anthropic-issued opaque signature for the thinking block.
    /// Must be echoed back when the thinking block is re-sent in subsequent turns.
    pub thinking_signature: Option<String>,
    pub stop_reason: StopReason,
}

impl ChatResponse {
    /// Collect a streaming response from a `BoxStream<StreamEvent>`.
    pub async fn from_stream(stream: BoxStream<StreamEvent>) -> anyhow::Result<Self> {
        use futures_util::StreamExt;
        let mut text = String::new();
        let mut reasoning_content = String::new();
        let mut thinking_signature: Option<String> = None;
        let mut tool_calls = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage: Option<ChatUsage> = None;

        let mut stream = stream;
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Delta { text: delta } => text.push_str(&delta),
                StreamEvent::Thinking { text: delta } => reasoning_content.push_str(&delta),
                StreamEvent::ToolCallStart {
                    id,
                    name,
                    initial_arguments,
                } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: initial_arguments,
                    });
                }
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    delta,
                } => {
                    let idx = index as usize;
                    while tool_calls.len() <= idx {
                        tool_calls.push(ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                    }
                    let call = &mut tool_calls[idx];
                    if !id.is_empty() {
                        call.id = id;
                    }
                    if !name.is_empty() {
                        call.name = name;
                    }
                    call.arguments.push_str(&delta);
                }
                StreamEvent::ToolCallEnd {
                    id,
                    name,
                    arguments,
                } => {
                    if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                        call.name = name;
                        call.arguments = arguments;
                    }
                }
                StreamEvent::Usage(u) => usage = Some(u),
                StreamEvent::ModelUsed { .. } => {
                    // Non-streaming callers attribute usage to their own
                    // model_id; the announcement is informational only.
                }
                StreamEvent::Done { reason } => {
                    stop_reason = reason;
                    break;
                }
                StreamEvent::ThinkingSignature { signature } => {
                    thinking_signature = Some(signature);
                }
                StreamEvent::HttpError { message, .. } => {
                    anyhow::bail!("Stream error: HTTP {}", message);
                }
                StreamEvent::Error(e) => anyhow::bail!("Stream error: {}", e),
            }
        }

        let reasoning_content = if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        };

        Ok(Self {
            text,
            tool_calls,
            usage,
            reasoning_content,
            thinking_signature,
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

#[cfg(test)]
mod overflow_reason_tests {
    use super::is_context_overflow_reason;

    #[test]
    fn classifies_overflow_strings() {
        assert!(is_context_overflow_reason("model_context_window_exceeded"));
        assert!(is_context_overflow_reason("context_length_exceeded"));
        assert!(is_context_overflow_reason("CONTEXT_WINDOW_OVERFLOW"));
    }

    #[test]
    fn rejects_normal_stop_reasons() {
        // `length` (output cap) and `stop` must NOT be treated as overflow.
        assert!(!is_context_overflow_reason("length"));
        assert!(!is_context_overflow_reason("stop"));
        assert!(!is_context_overflow_reason("tool_calls"));
        assert!(!is_context_overflow_reason("content_filter"));
    }
}
