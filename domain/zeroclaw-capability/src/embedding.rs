//! Embedding capability.

use async_trait::async_trait;

pub struct EmbedRequest {
    pub input: EmbedInput,
    pub model: String,
    /// Embedding dimensions (supported by a subset of providers).
    pub dimensions: Option<u32>,
}

pub enum EmbedInput {
    Text(String),
    Texts(Vec<String>),
}

pub struct EmbedResponse {
    pub embeddings: Vec<f32>,
    pub usage: Option<EmbeddingUsage>,
    pub model: String,
}

pub struct EmbeddingUsage {
    pub prompt_tokens: u64,
}

/// Provider handles batching internally: single Text → Vec::from([text]), multiple Texts → forwarded.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, req: EmbedRequest) -> anyhow::Result<EmbedResponse>;
}