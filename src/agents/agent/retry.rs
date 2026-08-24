//! Same-model LLM retry policy (extracted verbatim from the former `agent.rs`).

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
