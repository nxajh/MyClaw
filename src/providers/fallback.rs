//! FallbackChatProvider — decorator that wraps multiple ChatProviders
//! and retries on retryable errors with structured error classification.
//!
//! This keeps fallback logic entirely within the Infrastructure layer.
//! The Application layer (Agent) only sees a single `ChatProvider`.

/// Tag embedded in the error message when every provider in the chain has
/// been tried and all failed.  The outer retry loop in run.rs checks for this
/// to avoid restarting the whole chain from scratch.
pub const CHAIN_EXHAUSTED_TAG: &str = "fallback_chain_exhausted";

use async_trait::async_trait;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, ChatMessage, StreamEvent, ChatToolSpec,
    ThinkingConfig, ClassifiedError, ErrorCategory,
};
use crate::providers::credential_pool::SharedCredentialPool;
use futures_util::StreamExt;
use std::sync::Arc;

/// An entry in the fallback chain: a provider + its model ID + optional credential pool.
#[derive(Clone)]
pub struct FallbackEntry {
    pub provider: Arc<dyn ChatProvider>,
    pub model_id: String,
    /// Optional credential pool for same-provider key rotation.
    pub credential_pool: Option<SharedCredentialPool>,
}

/// Decorator that tries providers in order, falling back based on error classification.
#[derive(Clone)]
pub struct FallbackChatProvider {
    chain: Vec<FallbackEntry>,
}

impl FallbackChatProvider {
    pub fn new(chain: Vec<FallbackEntry>) -> Self {
        assert!(!chain.is_empty(), "fallback chain must not be empty");
        Self { chain }
    }
}

/// Returns `true` if the error is provider-specific and warrants failover to the
/// next provider in the chain (rather than a simple retry).
fn is_provider_error(cat: &ErrorCategory) -> bool {
    matches!(
        cat,
        ErrorCategory::Auth
            | ErrorCategory::AuthPermanent
            | ErrorCategory::Billing
            | ErrorCategory::ModelNotFound
    )
}

#[async_trait]
impl ChatProvider for FallbackChatProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        // Clone the borrowed data so the spawned task can retry independently.
        let messages: Vec<ChatMessage> = req.messages.to_vec();
        let tools: Option<Vec<ChatToolSpec>> = req.tools.map(|t| t.to_vec());
        let temperature = req.temperature;
        let max_tokens = req.max_tokens;
        let thinking: Option<ThinkingConfig> = req.thinking.map(|t| ThinkingConfig {
            enabled: t.enabled,
            effort: t.effort.clone(),
        });
        let stop = req.stop.clone();
        let seed = req.seed;
        let stream_flag = req.stream;

        let chain = self.chain.clone();

        tokio::spawn(async move {
            for entry in &chain {
                let req = ChatRequest {
                    model: &entry.model_id,
                    messages: &messages,
                    temperature,
                    max_tokens,
                    thinking: thinking.clone(),
                    stop: stop.clone(),
                    seed,
                    tools: tools.as_deref(),
                    stream: stream_flag,
                };

                let stream = match entry.provider.chat(req) {
                    Ok(s) => s,
                    Err(e) => {
                        let classified = ClassifiedError::classify("fallback", 0, &e.to_string())
                            .with_provider("fallback", &entry.model_id);
                        tracing::warn!(
                            model = %entry.model_id,
                            category = %classified.category,
                            reason = ?classified.reason,
                            retryable = classified.recovery_hints().retry,
                            "chat() failed: {}", classified.message
                        );
                        // Only continue to next provider if classified as a provider
                        // error or if retryable.
                        if is_provider_error(&classified.category)
                            || classified.recovery_hints().retry
                        {
                            continue;
                        }
                        // Non-retryable setup error — propagate immediately.
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        return;
                    }
                };

                // Drain the stream. Classify errors to decide whether to failover.
                let mut should_failover = false;
                let mut inner_stream = stream;

                while let Some(event) = inner_stream.next().await {
                    match &event {
                        StreamEvent::HttpError { status, message } => {
                            let classified = ClassifiedError::classify(
                                "fallback",
                                *status,
                                message,
                            )
                            .with_provider("fallback", &entry.model_id);
                            tracing::warn!(
                                model = %entry.model_id,
                                status = *status,
                                category = %classified.category,
                                reason = ?classified.reason,
                                cooldown = ?classified.cooldown_duration(),
                                "classified HTTP error"
                            );
                            if classified.should_rotate_credential {
                                if let Some(ref pool) = entry.credential_pool {
                                    if let Some(next_key) = pool.next_credential() {
                                        tracing::info!(
                                            model = %entry.model_id,
                                            key_prefix = %next_key.chars().take(4).collect::<String>(),
                                            "rotating to next credential"
                                        );
                                    }
                                }
                            }
                            if is_provider_error(&classified.category)
                                || classified.recovery_hints().retry
                            {
                                should_failover = true;
                                break;
                            }
                            // Non-retryable HTTP error — propagate and stop.
                            let _ = tx.send(event).await;
                            return;
                        }
                        StreamEvent::Error(msg) => {
                            let classified = ClassifiedError::classify("fallback", 0, msg)
                                .with_provider("fallback", &entry.model_id);
                            if classified.should_rotate_credential {
                                if let Some(ref pool) = entry.credential_pool {
                                    if let Some(next_key) = pool.next_credential() {
                                        tracing::info!(
                                            model = %entry.model_id,
                                            key_prefix = %next_key.chars().take(4).collect::<String>(),
                                            "rotating to next credential"
                                        );
                                    }
                                }
                            }
                            if is_provider_error(&classified.category)
                                || classified.recovery_hints().retry
                            {
                                tracing::warn!(
                                    model = %entry.model_id,
                                    category = %classified.category,
                                    reason = ?classified.reason,
                                    "classified stream error, failing over"
                                );
                                should_failover = true;
                                break;
                            }
                            // Non-retryable stream error — propagate.
                            let _ = tx.send(event).await;
                            return;
                        }
                        _ => {
                            let _ = tx.send(event).await;
                        }
                    }
                }

                if !should_failover {
                    // Stream ended normally — we're done.
                    return;
                }
                // else: continue to next provider in chain
            }

            // Exhausted all providers.
            let _ = tx.send(StreamEvent::Error(
                format!("{CHAIN_EXHAUSTED_TAG}: All providers in fallback chain failed with retryable errors")
            )).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}
