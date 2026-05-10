//! TurnEvent — 流式事件，Agent turn 过程中实时推送给 Client。
//!
//! 参考 OpenClaw 的 TurnEvent 设计，通过 mpsc channel 传递，
//! WebSocket handler 用 tokio::join! 并发转发给 Client。

use serde::Serialize;

/// Agent turn 过程中的流式事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum TurnEvent {
    /// LLM 文本片段
    #[serde(rename = "chunk")]
    Chunk { delta: String },

    /// 思考过程片段（thinking model）
    #[serde(rename = "thinking")]
    Thinking { delta: String },

    /// Agent 正在调用工具
    #[serde(rename = "tool_call")]
    ToolCall {
        name: String,
        args: serde_json::Value,
    },

    /// 工具返回结果
    #[serde(rename = "tool_result")]
    ToolResult { name: String, output: String },

    /// Turn 被用户取消
    #[serde(rename = "cancelled")]
    Cancelled { partial: String },

    /// Turn 完成（最终事件，包含完整文本）
    #[serde(rename = "done")]
    Done { text: String },

    /// Turn 发生错误
    #[serde(rename = "error")]
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_chunk() {
        let event = TurnEvent::Chunk {
            delta: "hello".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"chunk""#));
        assert!(json.contains(r#""delta":"hello""#));
    }

    #[test]
    fn serialize_tool_call() {
        let event = TurnEvent::ToolCall {
            name: "shell".into(),
            args: serde_json::json!({"cmd": "ls"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"tool_call""#));
        assert!(json.contains(r#""name":"shell""#));
    }

    #[test]
    fn serialize_cancelled() {
        let event = TurnEvent::Cancelled {
            partial: "partial text".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"cancelled""#));
    }
}
