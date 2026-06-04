//! RetryChatProvider — decorator that retries the SAME provider/model on
//! transient errors (502/503/504/timeout/connection drop) with short backoff.
//!
//! Distinct from [`FallbackChatProvider`](crate::providers::FallbackChatProvider),
//! which fails over to a *different* model. This wrapper is used for the
//! `session_override.model` path (the user picked a specific model via `/model`):
//! a gateway blip should be retried on the chosen model, NOT silently swapped for
//! another one. The Agent only ever sees a single `ChatProvider`.

use async_trait::async_trait;
use crate::providers::{
    BoxStream, ChatMessage, ChatProvider, ChatRequest, ChatToolSpec, ClassifiedError,
    ErrorCategory, StreamEvent, ThinkingConfig,
};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

/// Retry decorator over a single chat provider/model.
#[derive(Clone)]
pub struct RetryChatProvider {
    inner: Arc<dyn ChatProvider>,
    model_id: String,
    max_retries: usize,
}

impl RetryChatProvider {
    pub fn new(inner: Arc<dyn ChatProvider>, model_id: String, max_retries: usize) -> Self {
        Self { inner, model_id, max_retries }
    }
}

/// Transient errors worth retrying on the *same* model quickly. Excludes
/// RateLimit (needs a long cooldown / credential rotation, not a fast retry),
/// FormatError/ContextOverflow (retrying the identical request won't help), and
/// Auth/Billing (permanent until the operator intervenes). A status-less stream
/// error (connection drop) classifies as `Timeout`, so it is covered here.
fn is_transient(category: &ErrorCategory) -> bool {
    matches!(
        category,
        ErrorCategory::ServerError | ErrorCategory::Overloaded | ErrorCategory::Timeout
    )
}

/// Short exponential backoff: 500ms, 1s, 2s, … (gateway blips usually clear in
/// well under a second; we keep total added latency bounded for an interactive
/// turn rather than honoring the classifier's multi-minute cooldown).
fn backoff(attempt: usize) -> Duration {
    Duration::from_millis(500u64 << attempt.min(4) as u32)
}

#[async_trait]
impl ChatProvider for RetryChatProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        // Own the borrowed request data so the spawned task can retry independently.
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

        let inner = Arc::clone(&self.inner);
        let model_id = self.model_id.clone();
        let max_retries = self.max_retries;

        tokio::spawn(async move {
            let mut attempt: usize = 0;
            loop {
                let req = ChatRequest {
                    model: &model_id,
                    messages: &messages,
                    temperature,
                    max_tokens,
                    thinking: thinking.clone(),
                    stop: stop.clone(),
                    seed,
                    tools: tools.as_deref(),
                    stream: stream_flag,
                };

                // Decide retry vs. forward for a classified error. We only retry
                // when nothing has been streamed yet — re-running after partial
                // output would duplicate the user-visible response.
                let mut emitted = false;

                match inner.chat(req) {
                    Ok(mut stream) => {
                        let mut retry = false;
                        while let Some(event) = stream.next().await {
                            match &event {
                                StreamEvent::HttpError { status, message } => {
                                    let classified = ClassifiedError::classify(
                                        "retry", *status, message,
                                    );
                                    if !emitted
                                        && attempt < max_retries
                                        && is_transient(&classified.category)
                                    {
                                        tracing::warn!(
                                            model = %model_id, status = *status,
                                            attempt = attempt + 1,
                                            "transient HTTP error; retrying same model"
                                        );
                                        retry = true;
                                        break;
                                    }
                                    let _ = tx.send(event).await;
                                    return;
                                }
                                StreamEvent::Error(msg) => {
                                    let classified = ClassifiedError::classify("retry", 0, msg);
                                    if !emitted
                                        && attempt < max_retries
                                        && is_transient(&classified.category)
                                    {
                                        tracing::warn!(
                                            model = %model_id, attempt = attempt + 1,
                                            "transient stream error; retrying same model"
                                        );
                                        retry = true;
                                        break;
                                    }
                                    let _ = tx.send(event).await;
                                    return;
                                }
                                StreamEvent::Delta { .. }
                                | StreamEvent::Thinking { .. }
                                | StreamEvent::ToolCallStart { .. }
                                | StreamEvent::ToolCallDelta { .. }
                                | StreamEvent::ToolCallEnd { .. } => {
                                    emitted = true;
                                    let _ = tx.send(event).await;
                                }
                                _ => {
                                    let _ = tx.send(event).await;
                                }
                            }
                        }
                        if !retry {
                            // Stream ended (Done forwarded) — success, or a
                            // mid-stream error already forwarded above.
                            return;
                        }
                    }
                    Err(e) => {
                        let classified = ClassifiedError::classify("retry", 0, &e.to_string());
                        if !(attempt < max_retries && is_transient(&classified.category)) {
                            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                            return;
                        }
                        tracing::warn!(
                            model = %model_id, attempt = attempt + 1,
                            "transient chat() setup error; retrying same model"
                        );
                    }
                }

                tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::StopReason;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Mock provider that returns one scripted event sequence per `chat()` call.
    struct Scripted {
        scripts: Arc<Mutex<VecDeque<Vec<StreamEvent>>>>,
    }
    #[async_trait]
    impl ChatProvider for Scripted {
        fn chat(&self, _req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
            let evs = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::pin(futures_util::stream::iter(evs)))
        }
    }

    fn scripted(scripts: Vec<Vec<StreamEvent>>) -> Arc<Scripted> {
        Arc::new(Scripted { scripts: Arc::new(Mutex::new(scripts.into())) })
    }

    fn req() -> ChatRequest<'static> {
        ChatRequest {
            model: "m",
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

    async fn drain(p: &RetryChatProvider) -> (String, bool) {
        let mut s = p.chat(req()).unwrap();
        let (mut text, mut err) = (String::new(), false);
        while let Some(ev) = s.next().await {
            match ev {
                StreamEvent::Delta { text: t } => text.push_str(&t),
                StreamEvent::HttpError { .. } | StreamEvent::Error(_) => err = true,
                _ => {}
            }
        }
        (text, err)
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let provider = scripted(vec![
            vec![StreamEvent::HttpError { status: 502, message: "bad gateway".into() }],
            vec![StreamEvent::HttpError { status: 502, message: "bad gateway".into() }],
            vec![
                StreamEvent::Delta { text: "hello".into() },
                StreamEvent::Done { reason: StopReason::EndTurn },
            ],
        ]);
        let (text, err) = drain(&RetryChatProvider::new(provider, "m".into(), 3)).await;
        assert_eq!(text, "hello");
        assert!(!err, "transient 502s should be retried away");
    }

    #[tokio::test]
    async fn does_not_retry_format_error() {
        let provider = scripted(vec![vec![StreamEvent::HttpError {
            status: 400,
            message: "bad request".into(),
        }]]);
        let (_t, err) = drain(&RetryChatProvider::new(provider, "m".into(), 3)).await;
        assert!(err, "400 is not transient — forwarded, not retried");
    }

    #[tokio::test]
    async fn does_not_retry_after_partial_output() {
        // A Delta already streamed, then a 502 — retrying would duplicate output,
        // so the error is forwarded instead.
        let provider = scripted(vec![vec![
            StreamEvent::Delta { text: "partial".into() },
            StreamEvent::HttpError { status: 502, message: "boom".into() },
        ]]);
        let (text, err) = drain(&RetryChatProvider::new(provider, "m".into(), 3)).await;
        assert_eq!(text, "partial");
        assert!(err, "post-output error must not be retried");
    }
}
