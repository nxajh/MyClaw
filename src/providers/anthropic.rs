//! Anthropic provider — implements ChatProvider only.
//!
//! Chat is delegated to `AnthropicMessagesClient` from the protocols layer.
//! This file also hosts `parse_anthropic_sse` which is used by the protocol client.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, ChatUsage, StreamEvent, StopReason,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    user_agent: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self { base_url: DEFAULT_BASE_URL.to_string(), api_key, user_agent: None }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { base_url, api_key, user_agent: None }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        use crate::providers::protocols::anthropic::messages::AnthropicMessagesClient;
        let client = AnthropicMessagesClient::new(self.api_key.clone(), self.base_url.clone());
        let client = if let Some(ref ua) = self.user_agent {
            client.with_user_agent(ua.clone())
        } else {
            client
        };
        client.chat(req)
    }
}

/// Parse Anthropic-style SSE events.
/// Used by `protocols::anthropic::messages::AnthropicMessagesClient`.
pub(crate) fn parse_anthropic_sse(
    line: &str,
    tool_index_map: &mut HashMap<u64, (String, String)>,
) -> Vec<StreamEvent> {
    use crate::providers::StreamEvent as SE;

    let line = line.trim();
    if line.is_empty() || line.starts_with(':') { return vec![]; }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return vec![],
    };
    if data == "[DONE]" { return vec![]; }

    let evt = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let ty = match evt.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return vec![],
    };

    match ty {
        "content_block_start" => {
            let index = match evt.get("index").and_then(|v| v.as_u64()) {
                Some(i) => i,
                None => return vec![],
            };
            if let Some(cb) = evt.get("content_block") {
                if cb.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !id.is_empty() && !name.is_empty() {
                        tool_index_map.insert(index, (id.clone(), name.clone()));
                        return vec![SE::ToolCallStart { id, name, initial_arguments: String::new() }];
                    }
                }
            }
            vec![]
        }
        "content_block_delta" => {
            let delta = match evt.get("delta") {
                Some(d) => d,
                None => return vec![],
            };
            match delta.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text_delta" => {
                    let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() { vec![SE::Delta { text: text.to_string() }] } else { vec![] }
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() { vec![SE::Thinking { text: text.to_string() }] } else { vec![] }
                }
                "signature_delta" => {
                    let sig = delta.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                    if sig.is_empty() { vec![] } else { vec![SE::ThinkingSignature { signature: sig.to_string() }] }
                }
                "input_json_delta" => {
                    let index = match evt.get("index").and_then(|v| v.as_u64()) {
                        Some(i) => i,
                        None => return vec![],
                    };
                    let (id, _name) = match tool_index_map.get(&index) {
                        Some(entry) => entry.clone(),
                        None => return vec![],
                    };
                    let args = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                    vec![SE::ToolCallDelta { id, delta: args.to_string() }]
                }
                _ => vec![],
            }
        }
        "message_start" | "message_delta" => {
            let mut events = Vec::new();
            let usage = evt
                .get("message").and_then(|m| m.get("usage"))
                .or_else(|| evt.get("usage"));
            if let Some(usage) = usage {
                events.push(SE::Usage(ChatUsage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    cached_input_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                    reasoning_tokens: None,
                    cache_write_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()),
                }));
            }
            if ty == "message_delta" {
                if let Some(delta) = evt.get("delta") {
                    if let Some(reason_str) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                        let reason = match reason_str {
                            "end_turn" => StopReason::EndTurn,
                            "max_tokens" => StopReason::MaxTokens,
                            "tool_use" => StopReason::ToolUse,
                            "content_filter" => StopReason::ContentFilter,
                            _ => StopReason::EndTurn,
                        };
                        events.push(SE::Done { reason });
                    }
                }
            }
            events
        }
        "message_stop" => vec![SE::Done { reason: StopReason::EndTurn }],
        _ => vec![],
    }
}