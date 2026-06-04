//! ProviderRegistry trait: the single routing point for all provider capabilities.

use std::sync::Arc;
use super::capability::{Capability, ChatModelConfig, Modality};
use super::capability_chat::ChatProvider;
use super::capability_embedding::EmbeddingProvider;
use super::image::ImageGenerationProvider;
use super::search::SearchProvider;
use super::stt::SttProvider;
use super::tts::TtsProvider;
use super::video::VideoGenerationProvider;

/// Summary of a provider's registered models and capabilities.
#[derive(Debug, Clone)]
pub struct ProviderSummary {
    /// Provider key (e.g. "openai", "google", "minimax").
    pub name: String,
    /// Model IDs registered for Chat capability.
    pub chat_models: Vec<String>,
    /// Model IDs registered for Search capability.
    pub search_models: Vec<String>,
}

/// ProviderRegistry — read-only view consumed by Application / Domain layers.
pub trait ProviderRegistry: Send + Sync {
    fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;
    fn get_chat_provider_with_hint(&self, capability: Capability, provider_hint: Option<&str>) -> anyhow::Result<(Arc<dyn ChatProvider>, String)>;
    fn get_chat_fallback_chain(&self, capability: Capability) -> anyhow::Result<Vec<(Arc<dyn ChatProvider>, String)>>;
    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)>;
    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)>;
    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)>;
    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)>;
    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)>;
    #[allow(clippy::type_complexity)]
    fn get_search_fallback_chain(&self) -> anyhow::Result<Vec<(Arc<dyn SearchProvider>, String, String)>>;
    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)>;
    
    /// 获取 chat model 配置（含 input/output/pricing 等）
    fn get_chat_model_config(&self, model_id: &str) -> anyhow::Result<&ChatModelConfig>;

    /// Get a chat provider by exact model_id, bypassing fallback routing.
    /// Returns None if the model_id is not directly registered.
    fn get_chat_provider_by_model(&self, model_id: &str) -> Option<(Arc<dyn ChatProvider>, String)>;

    /// Get the list of model IDs in the chat routing config (in fallback order).
    fn get_chat_routing_models(&self) -> Vec<String>;

    /// The chat model that will actually be tried first for the next request,
    /// accounting for fallback cooldown state. Lets callers shape the request
    /// (e.g. native images vs the `view_image` tool) for the model that will
    /// really serve it, instead of always assuming the primary. Default: the
    /// first routed model.
    fn next_chat_model(&self) -> Option<String> {
        self.get_chat_routing_models().into_iter().next()
    }

    /// Get summaries for all registered providers (name, chat models, search models).
    fn get_all_provider_summaries(&self) -> Vec<ProviderSummary>;

    /// Find a registered chat model that supports the given input modality.
    ///
    /// The candidate set stays within user-declared routing: the chat fallback
    /// chain (`[routing.chat]`) is searched in order, and the first model whose
    /// `ChatModelConfig.input` contains `modality` is returned. No global model
    /// auto-discovery is performed.
    ///
    /// Default implementation built on the public trait methods
    /// (`get_chat_routing_models` / `get_chat_model_config` /
    /// `get_chat_provider_by_model`).
    fn find_chat_model_with_modality(
        &self,
        modality: Modality,
    ) -> Option<(Arc<dyn ChatProvider>, String)> {
        for model_id in self.get_chat_routing_models() {
            if let Ok(cfg) = self.get_chat_model_config(&model_id) {
                if cfg.supports_input(modality.clone()) {
                    if let Some(found) = self.get_chat_provider_by_model(&model_id) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }
}