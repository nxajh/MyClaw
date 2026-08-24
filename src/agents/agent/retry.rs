//! Same-model LLM retry policy (extracted verbatim from the former `agent.rs`).
//!
//! Batch 3 also moved the LLM-request retry loop out of `run_inner` as
//! [`chat_with_retry`]: send request → collect stream → on transient error,
//! sleep and retry (bounded), everything else propagates.

use std::sync::Arc;

use crate::agents::turn::TurnContext;
use crate::providers::capability_chat::{ChatMessage, ChatProvider, ChatRequest, ToolSpec};

use super::stream_collect::{collect_stream, CollectedResponse};

/// Whether an LLM error is worth sleeping and retrying on the *same* model.
///
/// Aligns with [`ClassifiedError::should_same_model_retry`]: short RateLimit,
/// Overloaded, ServerError, Timeout. Billing / long RateLimit / auth / not-found
/// are never same-model-retried.
///
/// Session `/model` override uses this path only — it never degrades to the
/// routing Fallback chain. Prefer `/model off` or `/model <other>` when the
/// pinned model is unavailable.
pub(super) fn is_transient_llm_error(err: &anyhow::Error) -> bool {
    use crate::providers::{ClassifiedError, ProviderHttpError};
    // HTTP errors: classify via the existing pipeline
    if let Some(http_err) = err.downcast_ref::<ProviderHttpError>() {
        let classified = ClassifiedError::classify("", http_err.status, &http_err.message);
        return classified.should_same_model_retry();
    }
    // Stream errors: SSE interruption, connection drop, etc.
    let msg = err.to_string();
    if msg.starts_with("stream error:") {
        return true;
    }
    if msg.contains("stream chunk timeout") {
        return true;
    }
    if msg.contains("stream stalled") {
        return true;
    }
    if msg.contains("stream total timeout") {
        return true;
    }
    if msg.contains("stream ended without a completion marker") {
        return true;
    }
    false
}

/// Short exponential backoff: 1s, 2s.
/// Gateway blips usually clear in well under a second; total added latency
/// is bounded to ~3s (two retries) for an interactive turn.
pub(super) fn backoff_duration(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_secs(1u64 << attempt.min(1))
}

/// Send the streaming LLM request and collect the response, retrying on
/// transient errors (bounded by `MAX_LLM_RETRIES`) with a short backoff.
///
/// Extracted verbatim from `run_inner`'s `let response = { ... }` block
/// (former `agent/mod.rs` lines 400–449, batch 3). Same-model retry only —
/// no cross-model fallback; the ContextOverflow / empty-response backstops
/// stay in `run_inner` (they need loop-level `continue`, batch 4).
#[allow(clippy::too_many_arguments)]
pub(super) async fn chat_with_retry(
    session_id: &str,
    history_len: usize,
    provider: &Arc<dyn ChatProvider>,
    messages: &[ChatMessage],
    tool_specs: &[ToolSpec],
    model_id: &str,
    turn_ctx: &TurnContext<'_>,
    turn_stream: &mut Option<Box<dyn crate::channels::TurnStream>>,
    user_registry: Option<&Arc<crate::agents::user_registry::UserRegistry>>,
) -> anyhow::Result<CollectedResponse> {
    tracing::info!(
        session = %session_id,
        msg_count = messages.len(),
        model = %model_id,
        history_len = history_len,
        "agent: sending LLM request"
    );
    const MAX_LLM_RETRIES: usize = 2;
    let mut attempt: usize = 0;
    loop {
        let thinking = turn_ctx.thinking.cloned();
        let req = ChatRequest {
            model: model_id,
            messages,
            temperature: None,
            max_tokens: None,
            thinking,
            stop: None,
            seed: None,
            tools: if tool_specs.is_empty() {
                None
            } else {
                Some(tool_specs)
            },
            stream: true,
        };
        let stream = provider.chat(req)?;
        match collect_stream(stream, turn_stream, user_registry).await {
            Ok(resp) => break Ok(resp),
            Err(e) if attempt < MAX_LLM_RETRIES && is_transient_llm_error(&e) => {
                attempt += 1;
                tracing::warn!(
                    model = %model_id, attempt,
                    err = %e,
                    "LLM call failed with transient error, retrying"
                );
                tokio::time::sleep(backoff_duration(attempt)).await;
                continue;
            }
            Err(e) => break Err(e),
        }
    }
}
