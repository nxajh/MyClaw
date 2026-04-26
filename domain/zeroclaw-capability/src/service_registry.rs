//! ServiceRegistry trait: the single routing point for all provider capabilities.
//!
//! Infrastructure layer implements this trait. Application and Domain layers
//! depend only on the trait, not the implementation.

use crate::capability::Capability;
use crate::chat::ChatProvider;
use crate::embedding::EmbeddingProvider;
use crate::image::ImageGenerationProvider;
use crate::tts::TtsProvider;
use crate::video::VideoGenerationProvider;

/// ServiceRegistry trait — implemented by Infrastructure, consumed by Application.
///
/// Routing is capability-based: callers ask for a specific capability and get back
/// a provider instance that implements it, along with the model string to use.
///
/// ```rust
/// // Application code
/// let (chat, model_id) = registry.get_chat_provider(Capability::Chat)?;
/// let response = chat.chat(ChatRequest { model: &model_id, ... })?;
/// ```
pub trait ServiceRegistry: Send + Sync {
    /// Register a Chat provider with its model ID.
    fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String);

    /// Get a Chat provider for the given capability (or hint).
    fn get_chat_provider(
        &self,
        capability: Capability,
    ) -> anyhow::Result<(Box<dyn ChatProvider>, String)>;

    /// Get a Chat provider with a provider hint override.
    fn get_chat_provider_with_hint(
        &self,
        capability: Capability,
        provider_hint: Option<&str>,
    ) -> anyhow::Result<(Box<dyn ChatProvider>, String)>;

    /// Get an Embedding provider.
    fn get_embedding_provider(&self) -> anyhow::Result<(Box<dyn EmbeddingProvider>, String)>;

    /// Get an ImageGeneration provider.
    fn get_image_provider(&self) -> anyhow::Result<(Box<dyn ImageGenerationProvider>, String)>;

    /// Get a TTS provider.
    fn get_tts_provider(&self) -> anyhow::Result<(Box<dyn TtsProvider>, String)>;

    /// Get a VideoGeneration provider.
    fn get_video_provider(&self) -> anyhow::Result<(Box<dyn VideoGenerationProvider>, String)>;
}