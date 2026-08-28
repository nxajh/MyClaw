//! LLM stream collection (extracted verbatim from the former `agent.rs`).

use std::sync::Arc;

use futures_util::StreamExt;

use crate::api::turn_event::TurnEvent;
use crate::identity::user_registry::UserRegistry;
use crate::providers::capability_chat::{StopReason, ToolCall};
use crate::providers::{BoxStream, StreamEvent};

/// Bundle of fields extracted from one streaming LLM response. Mirrors
/// the shape of `agent_impl::types::CollectedResponse` but is defined
/// here so `agent.rs` doesn't reach into `agent_impl/` internals.
pub(super) struct CollectedResponse {
    pub(super) text: String,
    pub(super) reasoning_content: Option<String>,
    pub(super) thinking_signature: Option<String>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) tool_call_events: usize,
    pub(super) stop_reason: StopReason,
    pub(super) usage: Option<crate::providers::ChatUsage>,
    /// Model that actually produced the stream (from the fallback chain's
    /// `ModelUsed` announcement). `None` when the caller's `model_id` is
    /// authoritative (direct provider, override, or no failover).
    pub(super) actual_model: Option<String>,
}

/// Push a `TurnEvent` to `session.turn_stream`, dropping the stream on
/// permanent transport failure (RFC §7.6.5(a)).
///
/// Without this short-circuit, `Agent::run` would keep generating chunks
/// for a disconnected client and waste LLM output budget. After drop,
/// subsequent `push_or_drop` calls become no-ops; the end-of-turn fallback
/// `send_message` in `SessionContext::process_turn` then ensures the user
/// still receives the final text via the non-streaming path.
pub(super) async fn push_or_drop(
    turn_stream: &mut Option<Box<dyn crate::api::turn_stream::TurnStream>>,
    event: TurnEvent,
) {
    let Some(stream) = turn_stream.as_mut() else {
        return;
    };
    if let Err(e) = stream.push(event).await {
        tracing::warn!(
            err = %e,
            "turn_stream push failed; dropping stream for remainder of turn"
        );
        *turn_stream = None;
    }
}

/// Read a full stream into a [`CollectedResponse`]. When `channel`
/// per-chunk `TurnEvent::Chunk` / `Thinking` events are pushed via
/// `session.turn_stream.push(…)` as text streams in (RFC §7.6).
/// Simplified compared to `AgentLoop::collect_stream_inner` — no
/// max_output_bytes guard, no cancellation token.
///
/// `user_registry` (P4 第二波): when present, chunks are rendered through
/// [`RefRenderer`] and the final text through [`render_refs`] so `<ref id="…"/>`
pub(super) async fn collect_stream(
    stream: BoxStream<StreamEvent>,
    turn_stream: &mut Option<Box<dyn crate::api::turn_stream::TurnStream>>,
    user_registry: Option<&Arc<UserRegistry>>,
) -> anyhow::Result<CollectedResponse> {
    let mut stream = stream;
    let mut text = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut thinking_signature: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_call_events: usize = 0;
    let mut usage: Option<crate::providers::ChatUsage> = None;
    let mut actual_model: Option<String> = None;
    let mut received_first_chunk = false;
    let mut ref_renderer = crate::agents::mention::RefRenderer::new();

    // Overall budget for the whole stream read (RFC v2 §六.A): bounds the
    // worst case of many small-but-alive chunks trickling forever. Each
    // individual wait is already bounded by the first-chunk/interval
    // timeouts; this caps the total wall-clock spent in this function.
    let stream_started_at = std::time::Instant::now();

    let stop_reason = loop {
        if stream_started_at.elapsed() > crate::agents::llm_stream::STREAM_TOTAL_TIMEOUT {
            anyhow::bail!(
                "stream total timeout after {}s",
                crate::agents::llm_stream::STREAM_TOTAL_TIMEOUT.as_secs()
            );
        }
        let event_opt = if !received_first_chunk {
            match tokio::time::timeout(
                crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT,
                stream.next(),
            )
            .await
            {
                Ok(ev) => ev,
                Err(_) => anyhow::bail!(
                    "stream chunk timeout after {}s, no data received",
                    crate::agents::llm_stream::STREAM_FIRST_CHUNK_TIMEOUT.as_secs()
                ),
            }
        } else {
            // Bound mid-stream stalls: once data has started flowing, a long
            // silence means a broken/hung connection (distinct from a slow
            // cold start, which the first-chunk timeout covers).
            match tokio::time::timeout(
                crate::agents::llm_stream::STREAM_CHUNK_INTERVAL_TIMEOUT,
                stream.next(),
            )
            .await
            {
                Ok(ev) => ev,
                Err(_) => anyhow::bail!(
                    "stream stalled: no chunk for {}s mid-response",
                    crate::agents::llm_stream::STREAM_CHUNK_INTERVAL_TIMEOUT.as_secs()
                ),
            }
        };

        let event = match event_opt {
            Some(e) => {
                received_first_chunk = true;
                e
            }
            // The stream ended without a terminal `Done`/`Error`/`HttpError`.
            // A well-behaved provider always sends one; reaching here means the
            // connection was closed mid-response. Fail the turn instead of
            // persisting a truncated reply as if it were complete.
            None => anyhow::bail!(
                "stream ended without a completion marker (provider truncated the response)"
            ),
        };

        match event {
            StreamEvent::Delta { text: delta } => {
                text.push_str(&delta);
                // P4 第二波：显示层渲染（跨 chunk 缓冲 `<ref …` 前缀）。
                let out_delta = match user_registry {
                    Some(r) => ref_renderer.push(&delta, r.as_ref()),
                    None => delta.clone(),
                };
                push_or_drop(
                    turn_stream,
                    TurnEvent::Chunk {
                        delta: out_delta,
                    },
                )
                .await;
            }
            StreamEvent::Thinking { text: delta } => {
                if !delta.is_empty() {
                    push_or_drop(
                        turn_stream,
                        TurnEvent::Thinking {
                            delta: delta.clone(),
                        },
                    )
                    .await;
                    if let Some(rc) = &mut reasoning_content {
                        rc.push_str(&delta);
                    } else {
                        reasoning_content = Some(delta);
                    }
                }
            }
            StreamEvent::ThinkingSignature { signature } => {
                thinking_signature = Some(signature);
            }
            StreamEvent::ToolCallStart {
                id,
                name,
                initial_arguments,
            } => {
                tool_call_events += 1;
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: initial_arguments,
                });
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                delta,
                name,
            } => {
                tool_call_events += 1;
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
                tool_call_events += 1;
                if let Some(call) = tool_calls.iter_mut().find(|c| c.id == id) {
                    call.name = name;
                    call.arguments = arguments;
                }
            }
            StreamEvent::Usage(u) => {
                if let Some(ref mut existing) = usage {
                    if u.input_tokens.is_some() {
                        existing.input_tokens = u.input_tokens;
                    }
                    if u.output_tokens.is_some() {
                        existing.output_tokens = u.output_tokens;
                    }
                    if u.cached_input_tokens.is_some() {
                        existing.cached_input_tokens = u.cached_input_tokens;
                    }
                } else {
                    usage = Some(u);
                }
            }
            StreamEvent::ModelUsed { model } => {
                // Last announcement wins: a mid-stream failover re-announces
                // with the entry that ultimately completed the request.
                actual_model = Some(model);
            }
            StreamEvent::Done { reason } => break reason,
            StreamEvent::HttpError { status, message } => {
                return Err(crate::providers::ProviderHttpError { status, message }.into());
            }
            StreamEvent::Error(e) => anyhow::bail!("stream error: {}", e),
        }
    };

    // Log complete tool calls for debugging (not just SSE deltas).
    if !tool_calls.is_empty() {
        for tc in &tool_calls {
            tracing::info!(
                tool_call_id = %tc.id,
                name = %tc.name,
                arguments = %tc.arguments,
                "tool call complete"
            );
        }
    }

    Ok(CollectedResponse {
        text: match user_registry {
            Some(r) => {
                // P4 第二波：整段渲染（含 flush 未闭合前缀）——Done/fallback 同源。
                let mut full = text;
                full.push_str(&ref_renderer.flush());
                crate::agents::mention::render_refs(&full, r.as_ref())
            }
            None => text,
        },
        reasoning_content,
        thinking_signature,
        tool_calls,
        tool_call_events,
        stop_reason,
        usage,
        actual_model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events_to_stream(
        events: Vec<crate::providers::StreamEvent>,
    ) -> BoxStream<crate::providers::StreamEvent> {
        use futures_util::stream;
        Box::pin(stream::iter(events))
    }

    #[tokio::test]
    async fn collect_stream_accepts_thinking_without_signature() {
        use crate::providers::StreamEvent;

        // Non-Anthropic providers (Xiaomi MiMo, …) speak Anthropic-compatible
        // protocol but never emit signature_delta. Collect must succeed; the
        // anthropic renderer's filter_map drops the unreplayable block at
        // send time so the next turn doesn't 400.
        let s = events_to_stream(vec![
            StreamEvent::Thinking {
                text: "let me think...".into(),
            },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed without signature: {e}"),
        };
        assert_eq!(resp.reasoning_content.as_deref(), Some("let me think..."));
        assert!(resp.thinking_signature.is_none());
    }

    #[tokio::test]
    async fn collect_stream_accepts_thinking_with_signature() {
        use crate::providers::StreamEvent;

        let s = events_to_stream(vec![
            StreamEvent::Thinking {
                text: "thinking...".into(),
            },
            StreamEvent::ThinkingSignature {
                signature: "sig123".into(),
            },
            StreamEvent::Delta {
                text: "hello".into(),
            },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert_eq!(resp.reasoning_content.as_deref(), Some("thinking..."));
        assert_eq!(resp.thinking_signature.as_deref(), Some("sig123"));
        assert_eq!(resp.text, "hello");
    }

    #[tokio::test]
    async fn collect_stream_accepts_no_thinking() {
        use crate::providers::StreamEvent;

        // No thinking at all → no signature needed.
        let s = events_to_stream(vec![
            StreamEvent::Delta { text: "hi".into() },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert!(resp.reasoning_content.is_none());
        assert!(resp.thinking_signature.is_none());
    }

    #[tokio::test]
    async fn collect_stream_captures_model_used() {
        use crate::providers::StreamEvent;

        // The fallback chain announces the actual model before content flows;
        // a mid-stream failover re-announces, and the last one wins.
        let s = events_to_stream(vec![
            StreamEvent::ModelUsed {
                model: "glm-5.2".into(),
            },
            StreamEvent::Delta { text: "hi".into() },
            StreamEvent::ModelUsed {
                model: "qwen3.7-plus".into(),
            },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert_eq!(resp.actual_model.as_deref(), Some("qwen3.7-plus"));
        assert_eq!(resp.text, "hi");
    }

    #[tokio::test]
    async fn collect_stream_model_used_absent_when_direct() {
        use crate::providers::StreamEvent;

        // Direct/provider path (no fallback wrapper) emits no ModelUsed;
        // actual_model must stay None so the caller keeps its model_id.
        let s = events_to_stream(vec![
            StreamEvent::Delta { text: "hi".into() },
            StreamEvent::Done {
                reason: crate::providers::StopReason::EndTurn,
            },
        ]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        let resp = match collect_stream(s, &mut turn_stream, None).await {
            Ok(r) => r,
            Err(e) => panic!("should succeed: {e}"),
        };
        assert!(resp.actual_model.is_none());
    }

    #[tokio::test]
    async fn collect_stream_rejects_truncated_stream() {
        use crate::providers::StreamEvent;

        // Stream ends with content but no terminal Done/Error — i.e. the
        // provider connection was closed mid-response. collect_stream must
        // fail the turn rather than persist the partial text as complete.
        let s = events_to_stream(vec![StreamEvent::Delta {
            text: "中方".into(),
        }]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream, None).await.is_err(),
            "truncated stream (no completion marker) must error"
        );
    }

    #[tokio::test]
    async fn collect_stream_propagates_provider_error() {
        use crate::providers::StreamEvent;

        // A provider that detects truncation emits StreamEvent::Error after
        // the partial deltas; that must surface as a turn failure.
        let s = events_to_stream(vec![
            StreamEvent::Delta {
                text: "partial".into(),
            },
            StreamEvent::Error("stream closed before completion".into()),
        ]);
        let mut turn_stream: Option<Box<dyn crate::api::turn_stream::TurnStream>> = None;
        assert!(
            collect_stream(s, &mut turn_stream, None).await.is_err(),
            "provider Error must fail the turn"
        );
    }
}
