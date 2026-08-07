//! TurnEvent — 流式事件，Agent turn 过程中实时推送给 Client。
//!
//! 参考 OpenClaw 的 TurnEvent 设计，通过 mpsc channel 传递，
//! WebSocket handler 用 tokio::join! 并发转发给 Client。
//!
//! 所有事件都包裹在 `VersionedEvent` 中，带有 schema 版本号、单调递增的
//! 序列号和时间戳，方便 CI/自动化工具消费。

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

/// Schema identifier for the versioned event protocol.
pub const TURN_EVENT_SCHEMA: &str = "myclaw.turn-event";
/// Current protocol version.
pub const TURN_EVENT_VERSION: u16 = 1;

static GLOBAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Versioned wrapper around a TurnEvent. Every event sent to the client
/// carries this envelope so consumers can parse, version, and order events.
#[derive(Debug, Clone, Serialize)]
pub struct VersionedEvent {
    /// Schema identifier ("myclaw.turn-event").
    pub schema: &'static str,
    /// Protocol version.
    pub v: u16,
    /// Monotonically increasing sequence number (process-global).
    pub seq: u64,
    /// ISO 8601 timestamp.
    pub ts: String,
    /// The actual event payload.
    #[serde(flatten)]
    pub event: TurnEvent,
}

impl VersionedEvent {
    /// Wrap a TurnEvent in a versioned envelope with a fresh seq and timestamp.
    pub fn new(event: TurnEvent) -> Self {
        Self {
            schema: TURN_EVENT_SCHEMA,
            v: TURN_EVENT_VERSION,
            seq: GLOBAL_SEQ.fetch_add(1, Ordering::Relaxed),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            event,
        }
    }
}

/// Run summary emitted at the end of a turn. Contains outcome, timing,
/// tool-call counts, token usage, and optional TTS metadata.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub schema: &'static str,
    pub v: u16,
    pub outcome: String,
    pub rounds: u32,
    pub tool_calls: std::collections::HashMap<String, u32>,
    pub tokens: Option<TokenUsage>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<TtsSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TtsSummary {
    pub triggered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

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
        id: String,
        name: String,
        args: serde_json::Value,
    },

    /// 工具返回结果
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        output: String,
    },

    /// Turn 被用户取消
    #[serde(rename = "cancelled")]
    Cancelled { partial: String },

    /// Turn 完成（最终事件，包含完整文本）
    #[serde(rename = "done")]
    Done { text: String },

    /// Turn 失败 — 模型返回空回复（流式路径专用）
    #[serde(rename = "empty_response")]
    EmptyResponse { user_message: String },

    /// Turn 发生错误
    #[serde(rename = "error")]
    Error { message: String },
}

impl TurnEvent {
    /// Wrap this event in a versioned envelope.
    pub fn versioned(self) -> VersionedEvent {
        VersionedEvent::new(self)
    }
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
            id: "tc-1".into(),
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

    #[test]
    fn versioned_event_envelope() {
        let event = TurnEvent::Chunk {
            delta: "test".into(),
        };
        let v = event.versioned();
        assert_eq!(v.schema, "myclaw.turn-event");
        assert_eq!(v.v, 1);
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""schema":"myclaw.turn-event""#));
        assert!(json.contains(r#""v":1"#));
        assert!(json.contains(r#""seq""#));
        assert!(json.contains(r#""ts""#));
        assert!(json.contains(r#""type":"chunk""#));
    }

    #[test]
    fn seq_monotonic() {
        let e1 = TurnEvent::Done { text: "a".into() }.versioned();
        let e2 = TurnEvent::Done { text: "b".into() }.versioned();
        assert!(e2.seq > e1.seq);
    }

    #[test]
    fn run_summary_serialize() {
        let s = RunSummary {
            schema: "myclaw.summary",
            v: 1,
            outcome: "success".into(),
            rounds: 3,
            tool_calls: {
                let mut m = std::collections::HashMap::new();
                m.insert("shell".into(), 2u32);
                m
            },
            tokens: Some(TokenUsage {
                prompt: 1000,
                completion: 500,
                total: 1500,
            }),
            elapsed_ms: 3200,
            tts: Some(TtsSummary {
                triggered: true,
                chars: Some(120),
                duration_ms: Some(2100),
                provider: Some("edge_tts".into()),
            }),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""outcome":"success""#));
        assert!(json.contains(r#""triggered":true"#));
        assert!(json.contains(r#""provider":"edge_tts""#));
    }

    #[test]
    fn run_summary_skip_none_tts() {
        let s = RunSummary {
            schema: "myclaw.summary",
            v: 1,
            outcome: "success".into(),
            rounds: 1,
            tool_calls: std::collections::HashMap::new(),
            tokens: None,
            elapsed_ms: 100,
            tts: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("tts"));
    }
}
