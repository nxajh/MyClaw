//! FallbackChatProvider — decorator that wraps multiple ChatProviders
//! and retries on retryable errors with structured error classification.
//!
//! This keeps fallback logic entirely within the Infrastructure layer.
//! The Application layer (Agent) only sees a single `ChatProvider`.

/// Tag embedded in the error message when every provider in the chain has
/// been tried and all failed.  The outer retry loop in run.rs checks for this
/// to avoid restarting the whole chain from scratch.
pub const CHAIN_EXHAUSTED_TAG: &str = "fallback_chain_exhausted";

/// Tag embedded in the error message when every provider in the chain is
/// currently on cooldown and none was attempted.
pub const CHAIN_ALL_COOLING_TAG: &str = "fallback_chain_all_cooling";

use async_trait::async_trait;
use crate::providers::{
    BoxStream, ChatProvider, ChatRequest, ChatMessage, StreamEvent, ChatToolSpec,
    ThinkingConfig, ClassifiedError, ErrorCategory,
};
use crate::providers::credential_pool::SharedCredentialPool;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// Per-model cooldown deadlines, shared across clones so all requests see
    /// the same state.  Keyed by model_id; value is the earliest Instant at
    /// which the model should be tried again.
    model_cooldowns: Arc<Mutex<HashMap<String, Instant>>>,
}

impl FallbackChatProvider {
    pub fn new(chain: Vec<FallbackEntry>) -> Self {
        assert!(!chain.is_empty(), "fallback chain must not be empty");
        Self {
            chain,
            model_cooldowns: Arc::new(Mutex::new(HashMap::new())),
        }
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

/// Record a cooldown deadline for `model_id` if the classified error carries one.
fn record_cooldown(
    cooldowns: &Mutex<HashMap<String, Instant>>,
    model_id: &str,
    classified: &ClassifiedError,
) {
    if let Some(d) = classified.cooldown_duration() {
        cooldowns.lock().unwrap().insert(model_id.to_string(), Instant::now() + d);
    }
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
        let cooldowns = Arc::clone(&self.model_cooldowns);

        tokio::spawn(async move {
            let mut soonest_cooling: Option<Instant> = None;
            let mut any_attempted = false;

            for entry in &chain {
                // ── Cooldown gate ──────────────────────────────────────────────
                {
                    let mut cg = cooldowns.lock().unwrap();
                    if let Some(&available_at) = cg.get(&entry.model_id) {
                        if Instant::now() < available_at {
                            soonest_cooling = Some(
                                soonest_cooling.map_or(available_at, |s: Instant| s.min(available_at))
                            );
                            tracing::info!(
                                model = %entry.model_id,
                                secs_remaining = available_at.saturating_duration_since(Instant::now()).as_secs(),
                                "skipping model: on cooldown"
                            );
                            continue;
                        }
                        // Cooldown expired — remove stale entry.
                        cg.remove(&entry.model_id);
                    }
                }
                any_attempted = true;

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
                        if is_provider_error(&classified.category)
                            || classified.recovery_hints().retry
                        {
                            record_cooldown(&cooldowns, &entry.model_id, &classified);
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
                                body = %classified.message,
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
                                record_cooldown(&cooldowns, &entry.model_id, &classified);
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
                                record_cooldown(&cooldowns, &entry.model_id, &classified);
                                tracing::warn!(
                                    model = %entry.model_id,
                                    category = %classified.category,
                                    reason = ?classified.reason,
                                    message = %classified.message,
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

            // ── All entries processed ──────────────────────────────────────────
            if !any_attempted {
                // Every entry was skipped due to active cooldown.
                let wait_secs = soonest_cooling
                    .map(|at| at.saturating_duration_since(Instant::now()).as_secs())
                    .unwrap_or(0);
                let _ = tx.send(StreamEvent::Error(
                    format!("{CHAIN_ALL_COOLING_TAG}: all providers on cooldown, retry in {wait_secs}s")
                )).await;
            } else {
                // Tried at least one provider; all failed with retryable errors.
                let _ = tx.send(StreamEvent::Error(
                    format!("{CHAIN_EXHAUSTED_TAG}: All providers in fallback chain failed with retryable errors")
                )).await;
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}
