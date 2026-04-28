//! ServiceRegistry trait: the single routing point for all provider capabilities.

use std::sync::Arc;
use super::capability::Capability;
use super::capability_chat::ChatProvider;
use super::capability_embedding::EmbeddingProvider;
use super::image::ImageGenerationProvider;
use super::search::SearchProvider;
use super::stt::SttProvider;
use super::tts::TtsProvider;
use super::video::VideoGenerationProvider;

/// ServiceRegistry — read-only view consumed by Application / Domain layers.
pub trait ServiceRegistry: Send + Sync {
    fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;
    fn get_chat_provider_with_hint(&self, capability: Capability, provider_hint: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;
    fn get_chat_fallback_chain(&self, capability: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>>;
    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)>;
    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)>;
    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)>;
    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)>;
    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)>;
    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)>;
}