//! ServiceRegistry trait: the single routing point for all provider capabilities.
//!
//! This file defines only the **read** side of the registry — the methods that
//! Application and Domain layers call to obtain providers.  Registration
//! (the write side) is an Infrastructure concern and lives in
//! `infrastructure/registry`.

use crate::capability::Capability;
use crate::chat::ChatProvider;
use crate::embedding::EmbeddingProvider;
use crate::image::ImageGenerationProvider;
use crate::search::SearchProvider;
use crate::stt::SttProvider;
use crate::tts::TtsProvider;
use crate::video::VideoGenerationProvider;
use std::sync::Arc;

/// ServiceRegistry — read-only view consumed by Application / Domain layers.
///
/// Infrastructure (`infrastructure/registry`) implements this trait and also
/// exposes a separate builder API for registration.
pub trait ServiceRegistry: Send + Sync {
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
