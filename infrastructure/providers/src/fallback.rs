//! FallbackChatProvider — decorator that wraps multiple ChatProviders
//! and retries on retryable errors (HTTP 429 rate-limit, 5xx server errors).
//!
//! This keeps fallback logic entirely within the Infrastructure layer.
//! The Application layer (Agent) only sees a single `ChatProvider`.

use async_trait::async_trait;
use capability::chat::{
    BoxStream, ChatProvider, ChatRequest, ChatMessage, StreamEvent, ToolSpec,
    ThinkingConfig,
};
use futures_util::StreamExt;
use std::sync::Arc;

/// An entry in the fallback chain: a provider + its model ID.
#[derive(Clone)]
pub struct FallbackEntry {
    pub provider: Arc<dyn ChatProvider>,
    pub model_id: String,
}

/// Decorator that tries providers in order, falling back on retryable errors.
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

/// Whether a stream error is retryable (429 rate-limit, 5xx server error).
fn is_retryable(status: u16) -> bool {
    status == 429 || status >= 500
}

#[async_trait]
impl ChatProvider for FallbackChatProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

        // Clone the borrowed data so the spawned task can retry independently.
        let messages: Vec<ChatMessage> = req.messages.to_vec();
        let tools: Option<Vec<ToolSpec>> = req.tools.map(|t| t.to_vec());
        let temperature = req.temperature;
        let max_tokens = req.max_tokens;
        let thinking: Option<ThinkingConfig> = req.thinking.map(|t| ThinkingConfig {
            effort: t.effort.clone(),
            budget_tokens: t.budget_tokens,
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
                        tracing::warn!(model = %entry.model_id, err = %e, "chat() failed, trying next");
                        continue;
                    }
                };

                // Drain the stream. If we hit a retryable HttpError, break out
                // and the outer loop will try the next provider.
                let mut retryable = false;
                let mut inner_stream = stream;

                while let Some(event) = inner_stream.next().await {
                    match &event {
                        StreamEvent::HttpError { status, message } if is_retryable(*status) => {
                            tracing::warn!(
                                model = %entry.model_id,
                                status,
                                "retryable error, falling back to next provider"
                            );
                            retryable = true;
                            break;
                        }
                        _ => {
                            let _ = tx.send(event).await;
                        }
                    }
                }

                if !retryable {
                    // Stream ended normally (Done/Error/other) — we're done.
                    return;
                }
                // else: continue to next provider in chain
            }

            // Exhausted all providers.
            let _ = tx.send(StreamEvent::Error(
                "All providers in fallback chain failed with retryable errors".to_string()
            )).await;
        });

        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}
