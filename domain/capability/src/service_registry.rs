//! ServiceRegistry trait: the single routing point for all provider capabilities.

use crate::capability::Capability;
use crate::chat::ChatProvider;
use crate::embedding::EmbeddingProvider;
use crate::image::ImageGenerationProvider;
use crate::tts::TtsProvider;
use crate::video::VideoGenerationProvider;
use std::sync::Arc;

/// ServiceRegistry trait — implemented by Infrastructure, consumed by Application.
pub trait ServiceRegistry: Send + Sync {
    /// Register a Chat provider with its model ID.
    fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String);

    /// Get a Chat provider for the given capability.
    fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;

    /// Get a Chat provider with a provider hint override.
    fn get_chat_provider_with_hint(
        &self,
        capability: Capability,
        provider_hint: Option<&str>,
    ) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;

    /// Get an Embedding provider.
    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)>;

    /// Get an ImageGeneration provider.
    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)>;

    /// Get a TTS provider.
    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)>;

    /// Get a VideoGeneration provider.
    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)>;
}