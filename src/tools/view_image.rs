//! `view_image` tool — lets a text-only primary model "look at" an image by
//! delegating to a vision-capable model in the chat routing chain.
//!
//! When the serving model lacks native image input, the turn's images are
//! replaced with `[图片 #N — 调用 view_image …]` placeholders (see
//! `agent::placeholder_images`). The model then calls this tool with the image
//! id and ITS OWN question; the tool fetches the real image from session
//! history and asks the vision model that question — so the answer is
//! context-aware, unlike a blind upfront caption that never saw the question.

use async_trait::async_trait;
use std::sync::Arc;

use crate::agents::session::Session;
use crate::providers::capability::Modality;
use crate::providers::capability_chat::{ChatMessage, ChatRequest, ChatResponse, ContentPart};
use crate::providers::provider_registry::ProviderRegistry;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

/// Vision delegation tool. Holds the provider registry so it can locate a
/// vision-capable model at call time (`Tool::execute` only receives `&Session`).
pub struct ViewImageTool {
    providers: Arc<dyn ProviderRegistry>,
}

impl ViewImageTool {
    pub fn new(providers: Arc<dyn ProviderRegistry>) -> Self {
        Self { providers }
    }
}

/// Return the `n`-th (1-based) image part across the session history, in order.
/// Numbering matches `placeholder_images`, which assigns ids by the same scan.
fn nth_image_part(history: &[ChatMessage], n: usize) -> Option<ContentPart> {
    if n == 0 {
        return None;
    }
    let mut count = 0;
    for msg in history {
        for part in &msg.parts {
            if matches!(part, ContentPart::ImageB64 { .. } | ContentPart::ImageUrl { .. }) {
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
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "查看用户发送的图片内容。当对话中出现 `[图片 #N — 调用 view_image]` 占位符时，\
         调用本工具并附上你想了解的具体问题，即可获得该图片的相关信息。\
         image_id 对应占位符里的编号 N。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "image_id": {
                    "type": "integer",
                    "description": "图片编号，对应占位符 [图片 #N] 里的 N（从 1 开始）。"
                },
                "question": {
                    "type": "string",
                    "description": "你想从这张图片了解的具体问题，例如『图里有几个人？』『识别图中的文字』。问题越具体，回答越准确。"
                }
            },
            "required": ["image_id", "question"]
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
        let image_id = args["image_id"].as_u64().unwrap_or(1).max(1) as usize;
        let question = args["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("请详细描述这张图片的内容，包括其中的文字、物体和场景。")
            .to_string();

        let Some(image_part) = nth_image_part(&session.history, image_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("未找到第 {image_id} 张图片（请核对占位符里的编号）。")),
            });
        };

        let Some((provider, model_id)) =
            self.providers.find_chat_model_with_modality(Modality::Image)
        else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("当前没有可用的视觉模型，无法查看图片。".to_string()),
            });
        };

        let user_msg = ChatMessage {
            role: "user".into(),
            parts: vec![image_part, ContentPart::Text { text: question }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        };
        let messages = [user_msg];
        let req = ChatRequest {
            model: &model_id,
            messages: &messages,
            temperature: Some(0.3),
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
                    error: Some(format!("视觉模型调用失败：{e}")),
                });
            }
        };
        let text = match ChatResponse::from_stream(stream).await {
            Ok(resp) => resp.text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("视觉模型响应失败：{e}")),
                });
            }
        };
        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("视觉模型返回了空结果。".to_string()),
            });
        }
        Ok(ToolResult { success: true, output: text, error: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::capability_chat::ImageDetail;

    fn img(b: &str) -> ContentPart {
        ContentPart::ImageB64 { b64_json: b.into(), media_type: None, detail: ImageDetail::Auto }
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
    fn nth_image_part_picks_in_scan_order() {
        let history = vec![
            msg(vec![ContentPart::Text { text: "hi".into() }, img("AAA")]),
            msg(vec![ContentPart::Text { text: "bye".into() }]),
            msg(vec![img("BBB")]),
        ];
        assert!(matches!(nth_image_part(&history, 1), Some(ContentPart::ImageB64 { b64_json, .. }) if b64_json == "AAA"));
        assert!(matches!(nth_image_part(&history, 2), Some(ContentPart::ImageB64 { b64_json, .. }) if b64_json == "BBB"));
    }

    #[test]
    fn nth_image_part_rejects_out_of_range_and_zero() {
        let history = vec![msg(vec![img("AAA")])];
        assert!(nth_image_part(&history, 0).is_none());
        assert!(nth_image_part(&history, 2).is_none());
        assert!(nth_image_part(&[], 1).is_none());
    }
}
