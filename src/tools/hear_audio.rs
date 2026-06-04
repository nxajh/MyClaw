//! `hear_audio` tool — the audio twin of `view_image`. Lets a model that can't
//! take audio natively "listen" to a voice clip by delegating to an audio-capable
//! model in the chain.
//!
//! When the serving model lacks audio input, the provider layer
//! ([`crate::providers::media`]) replaces the clip with a `[语音 #N]` marker. The
//! model then calls this tool with the clip's index and its OWN question; the
//! tool fetches the real audio from session history and asks the audio model that
//! question — context-aware transcription, on demand.

use async_trait::async_trait;
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, ChatResponse, ContentPart};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

/// Audio delegation tool. Holds the provider registry so it can locate an
/// audio-capable model at call time (`Tool::execute` only receives `&Session`).
pub struct HearAudioTool {
    providers: Arc<dyn ProviderRegistry>,
}

impl HearAudioTool {
    pub fn new(providers: Arc<dyn ProviderRegistry>) -> Self {
        Self { providers }
    }
}

/// Return the `n`-th (1-based) audio part across the session history, in order.
/// Numbering matches `providers::media::lower_media_for`, which assigns the
/// `[语音 #N]` markers by the same scan.
fn nth_audio_part(history: &[ChatMessage], n: usize) -> Option<ContentPart> {
    if n == 0 {
        return None;
    }
    let mut count = 0;
    for msg in history {
        for part in &msg.parts {
            if matches!(part, ContentPart::AudioB64 { .. }) {
                count += 1;
                if count == n {
                    return Some(part.clone());
                }
            }
        }
    }
    None
}

#[async_trait]
impl Tool for HearAudioTool {
    fn name(&self) -> &str {
        "hear_audio"
    }

    fn description(&self) -> &str {
        "听取用户发送的语音内容。当对话中出现 `[语音 #N]` 标记时，说明那里有一段你听不到的\
         语音；调用本工具并附上你想了解的具体问题（默认是转写全文），即可获得该语音的内容。\
         audio_id 对应标记里的编号 N。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "audio_id": {
                    "type": "integer",
                    "description": "语音编号，对应标记 [语音 #N] 里的 N（从 1 开始）。"
                },
                "question": {
                    "type": "string",
                    "description": "你想从这段语音了解的问题，例如『用户说了什么？』『把这段话翻译成英文』。留空则转写全文。"
                }
            },
            "required": ["audio_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        2_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let audio_id = args["audio_id"].as_u64().unwrap_or(1).max(1) as usize;
        let question = args["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("请逐字转写这段语音的内容。")
            .to_string();

        let Some(audio_part) = nth_audio_part(&session.history, audio_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("未找到第 {audio_id} 段语音（请核对标记里的编号）。")),
            });
        };

        let Some((provider, model_id)) =
            self.providers.find_chat_model_with_modality(Modality::Audio)
        else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("当前没有可用的语音模型，无法听取语音。".to_string()),
            });
        };

        let user_msg = ChatMessage {
            role: "user".into(),
            parts: vec![audio_part, ContentPart::Text { text: question }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        };
        let messages = [user_msg];
        let req = ChatRequest {
            model: &model_id,
            messages: &messages,
            temperature: Some(0.2),
            max_tokens: Some(1536),
            thinking: None,
            stop: None,
            seed: None,
            tools: None,
            stream: true,
        };

        let stream = match provider.chat(req) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("语音模型调用失败：{e}")),
                });
            }
        };
        let text = match ChatResponse::from_stream(stream).await {
            Ok(resp) => resp.text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("语音模型响应失败：{e}")),
                });
            }
        };
        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("语音模型返回了空结果。".to_string()),
            });
        }
        Ok(ToolResult { success: true, output: text, error: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aud(b: &str) -> ContentPart {
        ContentPart::AudioB64 { b64_json: b.into(), media_type: None }
    }
    fn msg(parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            parts,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }
    }

    #[test]
    fn nth_audio_part_picks_in_scan_order() {
        let history = vec![
            msg(vec![ContentPart::Text { text: "hi".into() }, aud("AAA")]),
            msg(vec![aud("BBB")]),
        ];
        assert!(matches!(nth_audio_part(&history, 1), Some(ContentPart::AudioB64 { b64_json, .. }) if b64_json == "AAA"));
        assert!(matches!(nth_audio_part(&history, 2), Some(ContentPart::AudioB64 { b64_json, .. }) if b64_json == "BBB"));
        assert!(nth_audio_part(&history, 3).is_none());
        assert!(nth_audio_part(&history, 0).is_none());
    }
}
