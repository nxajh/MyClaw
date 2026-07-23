//! Standard OpenAI Chat Completions client.
//!
//! Implements `ChatProvider` for the OpenAI Chat Completions endpoint
//! and OpenAI-compatible providers.

use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::providers::http::build_reqwest_client;
use crate::providers::protocols::openai::chat_message_rendering::render_openai_chat_body;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, SharedApiKey, StopReason, StreamEvent,
};
use reqwest::Client;

/// Post-process the rendered OpenAI body with provider-specific fields.
/// Receives the full body and the original request, returns the modified body.
/// Used for thinking parameters that differ across providers
/// (GLM `clear_thinking`, DeepSeek `{"type":"enabled"}`, reasoning_content echo).
pub type BodyOverrideFn = fn(serde_json::Value, &ChatRequest<'_>) -> serde_json::Value;

/// OpenAI Chat Completions protocol client.
#[derive(Clone)]
pub struct OpenAiChatCompletionsClient {
    base_url: String,
    api_key: SharedApiKey,
    client: Client,
    user_agent: Option<String>,
    body_override: Option<BodyOverrideFn>,
}

impl OpenAiChatCompletionsClient {
    pub fn new(api_key: impl Into<SharedApiKey>, base_url: String) -> Self {
        Self {
            base_url,
            api_key: api_key.into(),
            client: build_reqwest_client(),
            user_agent: None,
            body_override: None,
        }
    }

    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    pub fn with_body_override(mut self, f: BodyOverrideFn) -> Self {
        self.body_override = Some(f);
        self
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key.get())
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn common_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, self.auth().parse().unwrap());
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if let Some(ref ua) = self.user_agent {
            headers.insert(reqwest::header::USER_AGENT, ua.parse().unwrap());
        }
        headers
    }
}

#[async_trait]
impl ChatProvider for OpenAiChatCompletionsClient {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let url = self.chat_url();
        let body = render_openai_chat_body(&req);
        let body = match self.body_override {
            Some(f) => f(body, &req),
            None => body,
        };
        let client = self.client.clone();
        let headers = self.common_headers();
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        tokio::spawn(async move {
            let resp = match client.post(&url).headers(headers).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "request failed");
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return;
                }
            };

            if resp.error_for_status_ref().is_err() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx
                    .send(StreamEvent::HttpError {
                        status: status.as_u16(),
                        message: format!("HTTP {}: {}", status, text),
                    })
                    .await;
                return;
            }

            let mut saw_tool_call = false;
            let mut sse_stop_reason: Option<StopReason> = None;
            // True once the server sends the `[DONE]` SSE sentinel. Together
            // with `sse_stop_reason` (set from a `finish_reason` chunk) this
            // distinguishes a clean completion from a mid-stream connection
            // close (which yields neither).
            let mut saw_done_sentinel = false;
            let mut tool_index_map: HashMap<u32, String> = HashMap::new();
            // Rolling dump of recent `data:` payloads for anomaly diagnosis
            // (e.g. finish_reason=tool_calls with no parsed tool events).
            let mut recent_sse_data: std::collections::VecDeque<String> =
                std::collections::VecDeque::with_capacity(RECENT_SSE_CAP + 1);
            let mut buffer = String::new();
            let mut utf8_buf = Vec::new();
            let mut stream = resp.bytes_stream();

            while let Some(item) = stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(url = %url, error = %e, "stream read error");
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        return;
                    }
                };
                utf8_buf.extend_from_slice(&bytes);
                let try_decode = std::str::from_utf8(&utf8_buf);
                let text = match try_decode {
                    Ok(s) => {
                        let owned = s.to_string();
                        utf8_buf.clear();
                        owned
                    }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        if valid == 0 && utf8_buf.len() < 4 {
                            continue;
                        }
                        let t = String::from_utf8_lossy(&utf8_buf[..valid]).into_owned();
                        utf8_buf.clear();
                        t
                    }
                };
                if text.is_empty() {
                    continue;
                }
                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].to_string();
                    buffer.drain(..=pos);
                    let trimmed = line.trim();
                    if let Some(data) = trimmed.strip_prefix("data:") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            saw_done_sentinel = true;
                        } else if !data.is_empty() {
                            push_recent_sse(&mut recent_sse_data, data);
                        }
                    }
                    let events =
                        parse_openai_sse(&line, &mut tool_index_map, saw_tool_call);
                    for ev in events {
                        match ev {
                            StreamEvent::ToolCallStart { .. }
                            | StreamEvent::ToolCallDelta { .. } => {
                                saw_tool_call = true;
                                let _ = tx.send(ev).await;
                            }
                            // Consume SSE-reported Done; emit a single authoritative Done at the end.
                            StreamEvent::Done { reason } => {
                                sse_stop_reason = Some(reason);
                            }
                            _ => {
                                let _ = tx.send(ev).await;
                            }
                        }
                    }
                }
            }

            // Determine final stop reason: prefer the SSE-reported reason (which carries
            // MaxTokens / ContentFilter), but override with ToolUse when tool calls were
            // made and the provider reported "stop" instead of "tool_calls".
            // A clean completion requires an authoritative end marker: a
            // `finish_reason` chunk or the `[DONE]` sentinel. Without either,
            // the connection was closed mid-response — emit an error rather
            // than a silent, truncated success.
            if sse_stop_reason.is_some() || saw_done_sentinel {
                let final_reason = match sse_stop_reason {
                    Some(StopReason::ToolUse) => StopReason::ToolUse,
                    Some(r) if saw_tool_call => {
                        tracing::debug!(
                            ?r,
                            "overriding SSE stop reason with ToolUse (saw tool call events)"
                        );
                        StopReason::ToolUse
                    }
                    Some(r) => r,
                    None => {
                        if saw_tool_call {
                            StopReason::ToolUse
                        } else {
                            StopReason::EndTurn
                        }
                    }
                };
                // finish_reason=tool_calls (or equivalent) but no ToolCallStart/Delta
                // was ever parsed — agent may treat bridge text as a final reply.
                if matches!(final_reason, StopReason::ToolUse) && !saw_tool_call {
                    tracing::warn!(
                        url = %url,
                        stream_saw_tool_call = false,
                        recent_sse_count = recent_sse_data.len(),
                        recent_sse = %format_recent_sse(&recent_sse_data),
                        "finish_reason=tool_calls but no tool call events parsed; dumping recent SSE data"
                    );
                } else {
                    tracing::debug!(
                        stream_saw_tool_call = saw_tool_call,
                        ?final_reason,
                        "SSE stream completed"
                    );
                }
                let _ = tx
                    .send(StreamEvent::Done {
                        reason: final_reason,
                    })
                    .await;
            } else {
                let _ = tx
                    .send(StreamEvent::Error(
                        "stream closed before completion (no finish_reason / [DONE]; \
                         likely upstream truncation)"
                            .to_string(),
                    ))
                    .await;
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

/// Max recent `data:` payloads retained for anomaly dumps.
const RECENT_SSE_CAP: usize = 12;
/// Truncate each dumped SSE payload to keep WARN lines readable.
const RECENT_SSE_ITEM_MAX: usize = 400;

/// Byte index at or before `idx` that is a UTF-8 char boundary.
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Byte index at or after `idx` that is a UTF-8 char boundary.
fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn push_recent_sse(buf: &mut std::collections::VecDeque<String>, data: &str) {
    let clipped = if data.len() > RECENT_SSE_ITEM_MAX {
        // Prefer head (structure) + tail (finish_reason often at end of short chunks).
        // Must slice on char boundaries — GLM tool_call arguments frequently contain
        // CJK, and a raw byte cut at 368 panic'd the SSE reader worker
        // (2026-07-23 xiaoliu view_image: "byte index 368 is not a char boundary").
        let head_end = floor_char_boundary(data, RECENT_SSE_ITEM_MAX.saturating_sub(32));
        let head = &data[..head_end];
        let tail_start = ceil_char_boundary(data, data.len().saturating_sub(24));
        let tail = &data[tail_start..];
        format!("{head}…{tail}")
    } else {
        data.to_string()
    };
    if buf.len() >= RECENT_SSE_CAP {
        buf.pop_front();
    }
    buf.push_back(clipped);
}

fn format_recent_sse(buf: &std::collections::VecDeque<String>) -> String {
    buf.iter()
        .enumerate()
        .map(|(i, s)| format!("[{i}] {s}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Parse one OpenAI Chat Completions SSE `data:` line into stream events.
///
/// `stream_saw_tool_call` is the stream-level flag (true if any prior chunk
/// already emitted ToolCallStart/Delta). Used only for accurate finish_reason
/// logging — not for parsing decisions.
fn parse_openai_sse(
    line: &str,
    tool_index_map: &mut HashMap<u32, String>,
    stream_saw_tool_call: bool,
) -> Vec<StreamEvent> {
    use crate::providers::{ChatUsage, StopReason};

    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return vec![];
    }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return vec![],
    };
    if data == "[DONE]" {
        return vec![];
    }

    #[derive(serde::Deserialize)]
    struct Chunk {
        #[serde(default)]
        choices: Vec<Choice>,
        usage: Option<ChunkUsage>,
    }
    #[derive(serde::Deserialize)]
    struct Choice {
        #[serde(default)]
        delta: Delta,
        finish_reason: Option<String>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Delta {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Option<Vec<TcDelta>>,
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(dead_code)]
    struct TcDelta {
        index: u32,
        id: Option<String>,
        function: Option<FuncDelta>,
        /// Some providers put type on the tool_call object; ignored for events.
        #[serde(default, rename = "type")]
        type_: Option<String>,
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    #[allow(dead_code)]
    struct FuncDelta {
        name: Option<String>,
        arguments: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct PromptTokensDetails {
        #[serde(default)]
        cached_tokens: Option<u64>,
    }
    #[derive(serde::Deserialize)]
    struct ChunkUsage {
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        #[serde(default)]
        prompt_tokens_details: Option<PromptTokensDetails>,
    }

    let chunk: Chunk = match serde_json::from_str(data) {
        Ok(c) => c,
        Err(e) => {
            // Only log parse failures that look like they might contain tool calls
            // or finish reasons — silent drop of garbage is still fine for noise.
            if data.contains("tool_call") || data.contains("finish_reason") {
                tracing::warn!(
                    error = %e,
                    raw_data = %truncate_for_log(data, RECENT_SSE_ITEM_MAX),
                    "SSE chunk JSON parse failed (possible tool_calls lost)"
                );
            }
            return vec![];
        }
    };

    if chunk.choices.is_empty() {
        if let Some(u) = chunk.usage {
            return vec![StreamEvent::Usage(ChatUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cached_input_tokens: u.prompt_tokens_details.and_then(|d| d.cached_tokens),
                reasoning_tokens: None,
                cache_write_tokens: None,
            })];
        }
        return vec![];
    }

    let mut events = Vec::new();

    for choice in &chunk.choices {
        let mut chunk_emitted_tool_event = false;
        if let Some(tcs) = &choice.delta.tool_calls {
            tracing::debug!(
                raw_tool_calls = %serde_json::to_string(tcs).unwrap_or_default(),
                "SSE tool_calls delta"
            );

            for tc in tcs {
                let id = tc.id.clone().unwrap_or_default();
                let func = tc.function.as_ref();

                // First chunk for this tool call carries id + name → ToolCallStart.
                // GLM sends id + name + arguments all in one chunk.
                if !id.is_empty() && func.is_some_and(|f| f.name.is_some()) {
                    let initial_args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    tool_index_map.insert(tc.index, id.clone());
                    events.push(StreamEvent::ToolCallStart {
                        id,
                        name: func.and_then(|f| f.name.clone()).unwrap_or_default(),
                        initial_arguments: initial_args,
                    });
                    chunk_emitted_tool_event = true;
                } else {
                    let args = func.and_then(|f| f.arguments.clone()).unwrap_or_default();
                    if !args.is_empty() {
                        let delta_id = if id.is_empty() {
                            tool_index_map.get(&tc.index).cloned().unwrap_or_default()
                        } else {
                            tool_index_map.insert(tc.index, id.clone());
                            id
                        };
                        let delta_name = func.and_then(|f| f.name.clone()).unwrap_or_default();
                        events.push(StreamEvent::ToolCallDelta {
                            index: tc.index,
                            id: delta_id,
                            name: delta_name,
                            delta: args,
                        });
                        chunk_emitted_tool_event = true;
                    } else if !id.is_empty() || func.is_some_and(|f| f.name.is_some()) {
                        // Partial start (id without name, or name without id) — log
                        // so we can see non-standard streaming shapes.
                        tracing::debug!(
                            index = tc.index,
                            id = %id,
                            has_name = func.is_some_and(|f| f.name.is_some()),
                            "SSE tool_calls delta ignored (no complete start / no args)"
                        );
                    }
                }
            }
        }

        // Skip content when tool_calls were present (some providers send both).
        if !chunk_emitted_tool_event {
            if let Some(text) = &choice.delta.content {
                if !text.is_empty() {
                    events.push(StreamEvent::Delta { text: text.clone() });
                }
            }
            if let Some(reasoning) = &choice.delta.reasoning_content {
                if !reasoning.is_empty() {
                    events.push(StreamEvent::Thinking {
                        text: reasoning.clone(),
                    });
                }
            }
        }

        if let Some(ref r) = choice.finish_reason {
            // stream_saw_tool_call = any prior chunk; chunk_emitted_tool_event =
            // this finish chunk only (almost always false for tool_calls finish).
            tracing::debug!(
                finish_reason = %r,
                stream_saw_tool_call = stream_saw_tool_call,
                chunk_emitted_tool_event = chunk_emitted_tool_event,
                "SSE finish_reason received"
            );
            let reason = match r.as_str() {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "content_filter" | "sensitive" => StopReason::ContentFilter,
                "tool_calls" => StopReason::ToolUse,
                s if crate::providers::capability_chat::is_context_overflow_reason(s) => {
                    StopReason::ContextOverflow
                }
                _ => StopReason::EndTurn,
            };
            events.push(StreamEvent::Done { reason });
        }
    }

    events
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::StopReason;

    fn parse_line(line: &str, stream_saw: bool) -> Vec<StreamEvent> {
        let mut map = HashMap::new();
        parse_openai_sse(line, &mut map, stream_saw)
    }

    #[test]
    fn finish_reason_tool_calls_logs_stream_level_flag() {
        // finish chunk alone: stream_saw=true should still parse Done(ToolUse)
        let events = parse_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            true,
        );
        assert!(
            matches!(
                events.as_slice(),
                [StreamEvent::Done {
                    reason: StopReason::ToolUse
                }]
            ),
            "got: {events:?}"
        );
    }

    #[test]
    fn tool_call_start_then_finish() {
        let mut map = HashMap::new();
        let start = parse_openai_sse(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"web_search","arguments":"{\"q\":\"x\"}"}}]}}]}"#,
            &mut map,
            false,
        );
        assert!(
            matches!(
                start.as_slice(),
                [StreamEvent::ToolCallStart {
                    id,
                    name,
                    ..
                }] if id == "call_1" && name == "web_search"
            ),
            "got: {start:?}"
        );
        let finish = parse_openai_sse(
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &mut map,
            true,
        );
        assert!(matches!(
            finish.as_slice(),
            [StreamEvent::Done {
                reason: StopReason::ToolUse
            }]
        ));
    }

    #[test]
    fn recent_sse_ring_buffer_caps() {
        let mut buf = std::collections::VecDeque::new();
        for i in 0..20 {
            push_recent_sse(&mut buf, &format!("chunk-{i}"));
        }
        assert_eq!(buf.len(), RECENT_SSE_CAP);
        assert_eq!(buf.front().map(String::as_str), Some("chunk-8"));
        assert_eq!(buf.back().map(String::as_str), Some("chunk-19"));
    }

    #[test]
    fn push_recent_sse_cjk_mid_char_does_not_panic() {
        // Repro of 2026-07-23 xiaoliu: long view_image SSE data with CJK in
        // function.arguments; old code sliced at byte 368 inside a 3-byte char.
        let prefix = r#"{"id":"2026072314374382605a644f604a5b","created":1784788663,"object":"chat.completion.chunk","model":"glm-5.2","choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_b920f2366cb943b49f3da0a7","index":0,"type":"function","function":{"name":"view_image","arguments":""#;
        // Pad with CJK so that RECENT_SSE_ITEM_MAX-32 (368) lands mid-character
        // for many offsets (each 请 is 3 bytes).
        let cjk = "请描述这张图片中的全部文字与关键数据".repeat(40);
        let suffix = r#""}}]}}],"finish_reason":"tool_calls"}"#;
        let data = format!("{prefix}{cjk}{suffix}");
        assert!(data.len() > RECENT_SSE_ITEM_MAX);

        let mut buf = std::collections::VecDeque::new();
        // Must not panic on char-boundary unsafe byte slices.
        push_recent_sse(&mut buf, &data);
        assert_eq!(buf.len(), 1);
        let clipped = buf.front().unwrap();
        assert!(clipped.contains('…'), "expected head…tail clip: {clipped}");
        assert!(clipped.is_char_boundary(clipped.len()));
        // Round-trip: every prefix of clipped is still valid UTF-8 (slice ok).
        for i in 0..=clipped.len() {
            if clipped.is_char_boundary(i) {
                let _ = &clipped[..i];
            }
        }
    }

    #[test]
    fn push_recent_sse_exact_mid_utf8_at_368() {
        // Construct so byte index 368 is inside a multi-byte char.
        let mut s = String::new();
        while s.len() < 367 {
            s.push('a');
        }
        // Now len=367; next 请 occupies 367..370 — index 368 is mid-char.
        s.push('请');
        while s.len() <= RECENT_SSE_ITEM_MAX {
            s.push('描');
        }
        s.push_str(r#","finish_reason":"tool_calls"}"#);
        assert!(!s.is_char_boundary(368));

        let mut buf = std::collections::VecDeque::new();
        push_recent_sse(&mut buf, &s);
        assert_eq!(buf.len(), 1);
        assert!(buf.front().unwrap().contains('…'));
    }

    #[test]
    fn empty_delta_finish_chunk_does_not_claim_chunk_tool() {
        // Documents: finish chunk almost never carries tool_calls; stream flag matters.
        let events = parse_line(
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            false,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::Done {
                reason: StopReason::ToolUse
            }
        ));
    }
}
