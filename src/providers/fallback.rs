//! FallbackChatProvider — decorator that wraps multiple ChatProviders
//! and retries on retryable errors with structured error classification.
//!
//! This keeps fallback logic entirely within the Infrastructure layer.
//! The Application layer (Agent) only sees a single `ChatProvider`.
//!
//! Session `/model` override never uses this wrapper — only the default
//! routing path does. Recovery decisions come from [`ClassifiedError`]:
//! - `should_same_model_retry()` → short sleep on the same entry
//! - `should_fallback` / `retryable` → record cooldown and try next model

/// Tag embedded in the error message when every provider in the chain has
/// been tried and all failed.  The outer retry loop in run.rs checks for this
/// to avoid restarting the whole chain from scratch.
pub const CHAIN_EXHAUSTED_TAG: &str = "fallback_chain_exhausted";

/// Tag embedded in the error message when every provider in the chain is
/// currently on cooldown and none was attempted.
pub const CHAIN_ALL_COOLING_TAG: &str = "fallback_chain_all_cooling";

use crate::providers::credential_pool::SharedCredentialPool;
use crate::providers::{
    BoxStream, ChatMessage, ChatProvider, ChatRequest, ChatToolSpec, ClassifiedError,
    ErrorCategory, SharedApiKey, StreamEvent, ThinkingConfig,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Maximum retries for transient server errors (Overloaded, ServerError) before
/// falling over to the next provider in the chain.
const TRANSIENT_MAX_RETRIES: u32 = 2;

/// Short rate-limit: at most one same-model sleep before failover.
const RATE_LIMIT_SAME_MODEL_RETRIES: u32 = 1;

/// Cap same-model sleep for short rate limits so interactive turns stay bounded.
const RATE_LIMIT_SLEEP_CAP: Duration = Duration::from_secs(60);

/// Base delay for exponential backoff on transient server errors.
/// Actual delays: attempt 1 = 2 s, attempt 2 = 4 s.
const TRANSIENT_BACKOFF_BASE: Duration = Duration::from_secs(2);

/// Fallback duration for `mark_exhausted` when the triggering error somehow
/// carries no classified cooldown at all. Categories that set
/// `should_rotate_credential` always have one in practice (checked against
/// `recovery_hints_for` — every rotation-worthy category returns
/// `Some(..)`), so this only guards against a future category gaining
/// rotation eligibility without a cooldown.
const DEFAULT_EXHAUSTION_COOLDOWN: Duration = Duration::from_secs(3600);

/// Whether the classified error is worth retrying on the *same* chain entry
/// (short RateLimit / Overloaded / ServerError / Timeout).
fn is_same_model_retryable(classified: &ClassifiedError) -> bool {
    classified.should_same_model_retry()
}

/// Whether to leave this entry and try the next model in the chain.
fn should_failover_to_next(classified: &ClassifiedError) -> bool {
    // Align with ClassifiedError flags: fallbackable categories + retryable chain
    // continuation (RateLimit etc. now set should_fallback=true as well).
    classified.should_fallback || classified.recovery_hints().retry
}

/// Delay before same-model retry.
fn same_model_delay(classified: &ClassifiedError, attempt: u32) -> Duration {
    match classified.category {
        ErrorCategory::RateLimit => classified
            .cooldown_duration()
            .unwrap_or(RATE_LIMIT_SLEEP_CAP)
            .min(RATE_LIMIT_SLEEP_CAP),
        _ => transient_backoff(attempt),
    }
}

/// Max same-model attempts for this category (0-based attempt counter ceiling).
fn same_model_max_retries(classified: &ClassifiedError) -> u32 {
    match classified.category {
        ErrorCategory::RateLimit => RATE_LIMIT_SAME_MODEL_RETRIES,
        _ => TRANSIENT_MAX_RETRIES,
    }
}

/// Exponential backoff delay for transient retry attempt `n` (1-based).
fn transient_backoff(attempt: u32) -> Duration {
    TRANSIENT_BACKOFF_BASE * 2u32.pow(attempt.saturating_sub(1))
}

/// An entry in the fallback chain: a provider + its model ID + optional credential pool.
#[derive(Clone)]
pub struct FallbackEntry {
    pub provider: Arc<dyn ChatProvider>,
    pub model_id: String,
    /// Provider vendor id (e.g. "glm", "openai") — used by error classifier
    /// to apply vendor-specific rules (GLM code 1305 → Overloaded, etc.).
    pub provider_id: String,
    /// Optional credential pool for same-provider key rotation.
    pub credential_pool: Option<SharedCredentialPool>,
    /// Shared API key cell — when present, credential rotation updates this
    /// cell and the provider re-reads it on the next request.
    pub shared_api_key: Option<SharedApiKey>,
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

/// Record a cooldown deadline for `model_id` if the classified error carries one.
///
/// issue #197: when `credential_pool` is present, prefer the pool's own
/// `soonest_recovery(model_id)` over the classified error's cooldown. The
/// classified error only reflects whichever single key failed *last* in a
/// rotation sequence — using its duration directly could park the whole
/// model behind a long Billing cooldown (e.g. 5h) even though another key
/// in the pool, scoped to this same model, recovers in minutes.
/// `soonest_recovery` returns `None` when nothing has been marked
/// exhausted for this model at all (a non-key-related failover reason,
/// e.g. ContextOverflow), in which case the classified duration is used
/// exactly as before.
fn record_cooldown(
    cooldowns: &Mutex<HashMap<String, Instant>>,
    model_id: &str,
    classified: &ClassifiedError,
    credential_pool: Option<&SharedCredentialPool>,
) {
    let pool_duration = credential_pool.and_then(|p| p.soonest_recovery(model_id));
    let duration = pool_duration.or_else(|| classified.cooldown_duration());
    if let Some(d) = duration {
        cooldowns
            .lock()
            .unwrap()
            .insert(model_id.to_string(), Instant::now() + d);
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
                                soonest_cooling
                                    .map_or(available_at, |s: Instant| s.min(available_at)),
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

                let max_rotations = entry.credential_pool.as_ref().map(|p| p.len()).unwrap_or(1);
                let mut broke_for_failover = false;

                'credential_retry: for _rotation in 0..max_rotations {
                    let mut transient_attempt = 0u32;
                    let mut should_failover = false;
                    let mut should_rotate = false;
                    let mut last_classified: Option<ClassifiedError> = None;

                    'transient_retry: loop {
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
                                let classified = ClassifiedError::classify(
                                    &entry.provider_id,
                                    0,
                                    &e.to_string(),
                                )
                                .with_provider(&entry.provider_id, &entry.model_id);
                                tracing::warn!(
                                    model = %entry.model_id,
                                    category = %classified.category,
                                    reason = ?classified.reason,
                                    retryable = classified.recovery_hints().retry,
                                    "chat() failed: {}", classified.message
                                );

                                if classified.should_rotate_credential
                                    && entry.credential_pool.is_some()
                                    && entry.shared_api_key.is_some()
                                {
                                    if let (Some(pool), Some(key)) =
                                        (&entry.credential_pool, &entry.shared_api_key)
                                    {
                                        let old_key = key.get();
                                        pool.mark_exhausted(
                                            &old_key,
                                            &entry.model_id,
                                            &classified.reason,
                                            classified
                                                .cooldown_duration()
                                                .unwrap_or(DEFAULT_EXHAUSTION_COOLDOWN),
                                        );
                                        match pool.next_credential(&entry.model_id) {
                                            Some(new_key) => {
                                                key.set(&new_key);
                                                tracing::info!(
                                                    model = %entry.model_id,
                                                    key_prefix = %new_key.chars().take(4).collect::<String>(),
                                                    "credential rotated (setup error), retrying same provider"
                                                );
                                                continue 'credential_retry;
                                            }
                                            None => {
                                                tracing::warn!(
                                                    model = %entry.model_id,
                                                    "all credentials exhausted (setup error), failing over"
                                                );
                                            }
                                        }
                                    }
                                }

                                // Same-model short retry (Overloaded/ServerError/Timeout/short RL).
                                if is_same_model_retryable(&classified)
                                    && transient_attempt < same_model_max_retries(&classified)
                                {
                                    transient_attempt += 1;
                                    let delay = same_model_delay(&classified, transient_attempt);
                                    tracing::info!(
                                        model = %entry.model_id,
                                        category = %classified.category,
                                        attempt = transient_attempt,
                                        delay_secs = delay.as_secs(),
                                        "transient setup error, retrying same provider"
                                    );
                                    tokio::time::sleep(delay).await;
                                    continue 'transient_retry;
                                }

                                if should_failover_to_next(&classified) {
                                    record_cooldown(&cooldowns, &entry.model_id, &classified, entry.credential_pool.as_ref());
                                    broke_for_failover = true;
                                    break 'credential_retry;
                                }
                                // Non-retryable setup error — propagate immediately.
                                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                                return;
                            }
                        };

                        // Drain the stream. Classify errors to decide whether to failover or rotate.
                        let mut saw_content = false;
                        let mut model_announced = false;
                        let mut inner_stream = stream;

                        while let Some(event) = inner_stream.next().await {
                            match &event {
                                StreamEvent::HttpError { status, message } => {
                                    let classified = ClassifiedError::classify(
                                        &entry.provider_id,
                                        *status,
                                        message,
                                    )
                                    .with_provider(&entry.provider_id, &entry.model_id);
                                    tracing::warn!(
                                        model = %entry.model_id,
                                        status = *status,
                                        category = %classified.category,
                                        reason = ?classified.reason,
                                        cooldown = ?classified.cooldown_duration(),
                                        retry_after = ?classified.retry_after,
                                        same_model_retry = classified.should_same_model_retry(),
                                        body = %classified.message,
                                        "classified HTTP error"
                                    );

                                    if classified.should_rotate_credential
                                        && entry.credential_pool.is_some()
                                        && entry.shared_api_key.is_some()
                                    {
                                        should_rotate = true;
                                        last_classified = Some(classified);
                                        break;
                                    }

                                    // Same-model short retry before failing over.
                                    if is_same_model_retryable(&classified)
                                        && !saw_content
                                        && transient_attempt < same_model_max_retries(&classified)
                                    {
                                        transient_attempt += 1;
                                        let delay =
                                            same_model_delay(&classified, transient_attempt);
                                        tracing::info!(
                                            model = %entry.model_id,
                                            category = %classified.category,
                                            attempt = transient_attempt,
                                            delay_secs = delay.as_secs(),
                                            "transient HTTP error, retrying same provider"
                                        );
                                        tokio::time::sleep(delay).await;
                                        continue 'transient_retry;
                                    }

                                    if should_failover_to_next(&classified) {
                                        record_cooldown(&cooldowns, &entry.model_id, &classified, entry.credential_pool.as_ref());
                                        should_failover = true;
                                        break;
                                    }
                                    // Non-retryable HTTP error — propagate and stop.
                                    let _ = tx.send(event).await;
                                    return;
                                }
                                StreamEvent::Error(msg) => {
                                    let classified =
                                        ClassifiedError::classify(&entry.provider_id, 0, msg)
                                            .with_provider(&entry.provider_id, &entry.model_id);

                                    if classified.should_rotate_credential
                                        && entry.credential_pool.is_some()
                                        && entry.shared_api_key.is_some()
                                    {
                                        should_rotate = true;
                                        last_classified = Some(classified);
                                        break;
                                    }

                                    if is_same_model_retryable(&classified)
                                        && !saw_content
                                        && transient_attempt < same_model_max_retries(&classified)
                                    {
                                        transient_attempt += 1;
                                        let delay =
                                            same_model_delay(&classified, transient_attempt);
                                        tracing::info!(
                                            model = %entry.model_id,
                                            category = %classified.category,
                                            attempt = transient_attempt,
                                            delay_secs = delay.as_secs(),
                                            "transient stream error, retrying same provider"
                                        );
                                        tokio::time::sleep(delay).await;
                                        continue 'transient_retry;
                                    }

                                    if should_failover_to_next(&classified) {
                                        record_cooldown(&cooldowns, &entry.model_id, &classified, entry.credential_pool.as_ref());
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
                                    // Announce the entry that actually produced
                                    // this stream before any content flows, so
                                    // the caller can attribute usage/history to
                                    // the real model (not just the chain head).
                                    if !model_announced {
                                        model_announced = true;
                                        let _ = tx
                                            .send(StreamEvent::ModelUsed {
                                                model: entry.model_id.clone(),
                                            })
                                            .await;
                                    }
                                    saw_content = true;
                                    let _ = tx.send(event).await;
                                }
                            }
                        }

                        // Stream ended (success or non-transient error after retries).
                        break 'transient_retry;
                    }

                    if !should_rotate && !should_failover {
                        // Stream ended normally — we're done.
                        tracing::info!(
                            model = %entry.model_id,
                            "chat completed via fallback chain"
                        );
                        return;
                    }

                    if should_rotate {
                        if let (Some(pool), Some(key), Some(classified)) = (
                            &entry.credential_pool,
                            &entry.shared_api_key,
                            &last_classified,
                        ) {
                            let old_key = key.get();
                            pool.mark_exhausted(
                                &old_key,
                                &entry.model_id,
                                &classified.reason,
                                classified
                                    .cooldown_duration()
                                    .unwrap_or(DEFAULT_EXHAUSTION_COOLDOWN),
                            );
                            match pool.next_credential(&entry.model_id) {
                                Some(new_key) => {
                                    key.set(&new_key);
                                    tracing::info!(
                                        model = %entry.model_id,
                                        key_prefix = %new_key.chars().take(4).collect::<String>(),
                                        "credential rotated, retrying same provider"
                                    );
                                    continue 'credential_retry;
                                }
                                None => {
                                    tracing::warn!(
                                        model = %entry.model_id,
                                        "all credentials exhausted, failing over"
                                    );
                                    record_cooldown(&cooldowns, &entry.model_id, classified, entry.credential_pool.as_ref());
                                }
                            }
                        }
                    }

                    // If we get here (should_failover, or should_rotate with all credentials exhausted),
                    // we break the inner loop to failover to the next entry in the outer loop.
                    broke_for_failover = true;
                    break 'credential_retry;
                }

                if !broke_for_failover {
                    // Safety guard: if the inner loop ended without broke_for_failover and didn't return,
                    // it means all rotations were attempted but none succeeded. Record a default cooldown.
                    let classified = ClassifiedError::classify(
                        &entry.provider_id,
                        503,
                        "all credentials exhausted",
                    );
                    record_cooldown(&cooldowns, &entry.model_id, &classified, entry.credential_pool.as_ref());
                }
            }

            // ── All entries processed ──────────────────────────────────────────
            if !any_attempted {
                // Every entry was skipped due to active cooldown.
                let wait_secs = soonest_cooling
                    .map(|at| at.saturating_duration_since(Instant::now()).as_secs())
                    .unwrap_or(0);
                let _ = tx
                    .send(StreamEvent::Error(format!(
                        "{CHAIN_ALL_COOLING_TAG}: all providers on cooldown, retry in {wait_secs}s"
                    )))
                    .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::FailoverReason;

    enum MockResult {
        FailHttp { status: u16, message: String },
        Ok(Vec<StreamEvent>),
    }

    struct MockChatProvider {
        result: MockResult,
    }

    impl ChatProvider for MockChatProvider {
        fn chat(&self, _req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
            match &self.result {
                MockResult::FailHttp { status, message } => Ok(Box::pin(
                    futures_util::stream::iter(vec![StreamEvent::HttpError {
                        status: *status,
                        message: message.clone(),
                    }]),
                )),
                MockResult::Ok(events) => Ok(Box::pin(futures_util::stream::iter(
                    events.clone(),
                ))),
            }
        }
    }

    fn entry(
        model_id: &str,
        provider_id: &str,
        result: MockResult,
    ) -> FallbackEntry {
        FallbackEntry {
            provider: Arc::new(MockChatProvider { result }),
            model_id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            credential_pool: None,
            shared_api_key: None,
        }
    }

    /// issue #197 (problem 1): reproduces the reported incident directly
    /// against `record_cooldown` — a Billing error on one key (5h cooldown)
    /// must not park the whole model behind that 5h wait when another key
    /// in the same pool, for the same model, only has ~1 minute left.
    #[test]
    fn record_cooldown_uses_pool_soonest_recovery_not_the_last_errors_duration() {
        use crate::providers::credential_pool::{CredentialPool, RotationStrategy};

        let pool = SharedCredentialPool::new(CredentialPool::new(
            "glm",
            vec!["key-8f85".to_string(), "key-e700".to_string()],
            RotationStrategy::FillFirst,
        ));
        // key-8f85: short RateLimit cooldown, ~60s left.
        pool.mark_exhausted(
            "key-8f85",
            "glm-5.2",
            &FailoverReason::RateLimit,
            Duration::from_secs(60),
        );
        // key-e700: the *last* error tried — long Billing cooldown, 5h.
        pool.mark_exhausted(
            "key-e700",
            "glm-5.2",
            &FailoverReason::Billing,
            Duration::from_secs(5 * 3600),
        );

        // The "last classified error" the old code used verbatim — Billing,
        // 5h. If record_cooldown ignored the pool, glm-5.2 would be parked
        // for 5h even though key-8f85 recovers in ~60s.
        let classified = ClassifiedError::new(FailoverReason::Billing, "quota exhausted");
        assert_eq!(classified.cooldown_duration(), Some(Duration::from_secs(5 * 3600)));

        let cooldowns: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        record_cooldown(&cooldowns, "glm-5.2", &classified, Some(&pool));

        let deadline = *cooldowns.lock().unwrap().get("glm-5.2").expect("cooldown recorded");
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_secs(60),
            "model cooldown must reflect the pool's soonest-recovering key (~60s), \
             not the last error's own 5h Billing duration, got {remaining:?}"
        );
    }

    /// A failover reason unrelated to credential exhaustion (no key was
    /// ever marked exhausted for this model) must still use the classified
    /// error's own duration — the pool has nothing to derive from.
    #[test]
    fn record_cooldown_falls_back_to_classified_duration_without_pool_exhaustion() {
        use crate::providers::credential_pool::{CredentialPool, RotationStrategy};

        let pool = SharedCredentialPool::new(CredentialPool::new(
            "glm",
            vec!["key-1".to_string()],
            RotationStrategy::FillFirst,
        ));
        // Note: no mark_exhausted call — this model's scope has no recorded
        // exhaustion in the pool at all.

        let classified = ClassifiedError::new(FailoverReason::Overloaded, "server overloaded");
        let expected = classified.cooldown_duration().unwrap();

        let cooldowns: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        record_cooldown(&cooldowns, "glm-5.2", &classified, Some(&pool));

        let deadline = *cooldowns.lock().unwrap().get("glm-5.2").expect("cooldown recorded");
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining <= expected && remaining > expected.saturating_sub(Duration::from_secs(1)),
            "expected ~{expected:?}, got {remaining:?}"
        );
    }

    fn request(model: &str) -> ChatRequest<'_> {
        ChatRequest {
            model,
            messages: &[],
            temperature: None,
            max_tokens: None,
            thinking: None,
            stop: None,
            seed: None,
            tools: None,
            stream: true,
        }
    }

    /// Failover to the second chain entry must announce the actual model
    /// (not the chain head the caller asked for) before any content flows.
    #[tokio::test]
    async fn failover_announces_actual_model() {
        let fb = FallbackChatProvider::new(vec![
            entry(
                "glm-5.2",
                "glm",
                MockResult::FailHttp {
                    status: 404,
                    message: "model not found".into(),
                },
            ),
            entry(
                "qwen3.7-plus",
                "qwen",
                MockResult::Ok(vec![
                    StreamEvent::Delta { text: "hi".into() },
                    StreamEvent::Done {
                        reason: crate::providers::StopReason::EndTurn,
                    },
                ]),
            ),
        ]);

        let stream = fb.chat(request("glm-5.2")).unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            matches!(&events[0], StreamEvent::ModelUsed { model } if model == "qwen3.7-plus"),
            "first event must announce the fallback model, got {:?}",
            events.first()
        );
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Done { .. })),
            "stream must still terminate with Done"
        );
    }

    /// Chain-head success also announces its model — the caller can then
    /// attribute usage identically whether or not a failover happened.
    #[tokio::test]
    async fn primary_success_announces_chain_head() {
        let fb = FallbackChatProvider::new(vec![entry(
            "glm-5.2",
            "glm",
            MockResult::Ok(vec![
                StreamEvent::Delta { text: "hi".into() },
                StreamEvent::Done {
                    reason: crate::providers::StopReason::EndTurn,
                },
            ]),
        )]);

        let stream = fb.chat(request("glm-5.2")).unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            matches!(&events[0], StreamEvent::ModelUsed { model } if model == "glm-5.2"),
            "chain head success must announce itself, got {:?}",
            events.first()
        );
    }

    /// All entries failing must NOT emit a ModelUsed announcement.
    #[tokio::test]
    async fn all_failed_does_not_announce_model() {
        let fb = FallbackChatProvider::new(vec![
            entry(
                "glm-5.2",
                "glm",
                MockResult::FailHttp {
                    status: 404,
                    message: "model not found".into(),
                },
            ),
            entry(
                "qwen3.7-plus",
                "qwen",
                MockResult::FailHttp {
                    status: 404,
                    message: "model not found".into(),
                },
            ),
        ]);

        let stream = fb.chat(request("glm-5.2")).unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::ModelUsed { .. })),
            "no ModelUsed must be emitted when every entry failed"
        );
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "expected an Error event"
        );
    }

    /// #91: a stream-level error tagged with `TOOL_CALL_PARSE_LOSS_TAG` (the
    /// model claimed a tool call but the SSE chunk carrying it failed to
    /// parse) must propagate immediately, never trigger failover to the next
    /// chain entry — switching models here would silently discard the
    /// original model's decision instead of recovering it.
    #[tokio::test]
    async fn tool_call_lost_error_does_not_failover() {
        use crate::providers::error_class::TOOL_CALL_PARSE_LOSS_TAG;

        let fb = FallbackChatProvider::new(vec![
            entry(
                "glm-5.3",
                "glm",
                MockResult::Ok(vec![StreamEvent::Error(format!(
                    "{TOOL_CALL_PARSE_LOSS_TAG}: provider reported finish_reason=tool_calls \
                     but no tool call could be parsed from the stream"
                ))]),
            ),
            entry(
                "glm-5.2",
                "glm",
                MockResult::Ok(vec![
                    StreamEvent::Delta { text: "hi".into() },
                    StreamEvent::Done {
                        reason: crate::providers::StopReason::EndTurn,
                    },
                ]),
            ),
        ]);

        let stream = fb.chat(request("glm-5.3")).unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;

        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::ModelUsed { .. })),
            "must not fail over to (or announce) the second chain entry, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error(msg) if msg.contains(TOOL_CALL_PARSE_LOSS_TAG))),
            "the original tool-call-lost error must propagate unchanged, got {events:?}"
        );
    }
}
