//! ServiceRegistry trait: the single routing point for all provider capabilities.

use crate::capability::Capability;
use crate::chat::ChatProvider;
use crate::embedding::EmbeddingProvider;
use crate::image::ImageGenerationProvider;
use crate::search::SearchProvider;
use crate::stt::SttProvider;
use crate::tts::TtsProvider;
use crate::video::VideoGenerationProvider;
use std::sync::Arc;

/// ServiceRegistry trait — implemented by Infrastructure, consumed by Application.
pub trait ServiceRegistry: Send + Sync {
    // ── Register ──────────────────────────────────────────────────────────────

    /// Register a Chat provider with its model ID.
    fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String);

    /// Register an Embedding provider with its model ID.
    fn register_embedding(&mut self, provider: Box<dyn EmbeddingProvider>, model_id: String);

    /// Register an ImageGeneration provider with its model ID.
    fn register_image(&mut self, provider: Box<dyn ImageGenerationProvider>, model_id: String);

    /// Register a TTS provider with its model ID.
    fn register_tts(&mut self, provider: Box<dyn TtsProvider>, model_id: String);

    /// Register a VideoGeneration provider with its model ID.
    fn register_video(&mut self, provider: Box<dyn VideoGenerationProvider>, model_id: String);

    /// Register a Search provider with its model ID.
    fn register_search(&mut self, provider: Box<dyn SearchProvider>, model_id: String);

    /// Register a STT provider with its model ID.
    fn register_stt(&mut self, provider: Box<dyn SttProvider>, model_id: String);

    // ── Get providers ─────────────────────────────────────────────────────────

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

    /// Get a Search provider.
    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)>;

    /// Get a STT provider.
    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)>;
}