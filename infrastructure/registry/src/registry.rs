//! ServiceRegistry — capability routing center.

use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Context;
use capability::capability::Capability;
use capability::chat::ChatProvider;
use capability::embedding::EmbeddingProvider;
use capability::image::ImageGenerationProvider;
use capability::search::SearchProvider;
use capability::service_registry::ServiceRegistry;
use capability::stt::SttProvider;
use capability::tts::TtsProvider;
use capability::video::VideoGenerationProvider;

use crate::routing::{RoutingConfig, RouteEntry, RoutingStrategy};

// ── Config types ────────────────────────────────────────────────────────────────

/// Registry-level provider config (converted from config::ProviderConfig).
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Provider API kind: "minimax", "openai", "glm", etc. (HashMap key).
    pub api: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub models: Vec<ModelConfig>,
}

/// Registry-level model config (converted from config::ModelConfig).
#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub model_id: String,
    pub capabilities: Vec<Capability>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u32>,
    pub reasoning: bool,
}

impl ModelConfig {
    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

// ── Registry ───────────────────────────────────────────────────────────────────

pub struct Registry {
    providers: HashMap<String, ProviderConfig>,
    routing: RoutingConfig,
    // Per-capability provider stores: model_id → Arc'd provider
    chat_providers: HashMap<String, Arc<dyn ChatProvider>>,
    embedding_providers: HashMap<String, Arc<dyn EmbeddingProvider>>,
    image_providers: HashMap<String, Arc<dyn ImageGenerationProvider>>,
    tts_providers: HashMap<String, Arc<dyn TtsProvider>>,
    video_providers: HashMap<String, Arc<dyn VideoGenerationProvider>>,
    search_providers: HashMap<String, Arc<dyn SearchProvider>>,
    stt_providers: HashMap<String, Arc<dyn SttProvider>>,
}

impl Registry {
    pub fn new(providers: HashMap<String, ProviderConfig>, routing: RoutingConfig) -> Self {
        Self {
            providers,
            routing,
            chat_providers: HashMap::new(),
            embedding_providers: HashMap::new(),
            image_providers: HashMap::new(),
            tts_providers: HashMap::new(),
            video_providers: HashMap::new(),
            search_providers: HashMap::new(),
            stt_providers: HashMap::new(),
        }
    }

    // ── Legacy register_chat (used by Orchestrator) ──────────────────────────

    pub fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String) {
        self.chat_providers.insert(model_id, provider.into());
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn find_provider_by_model(&self, model_id: &str) -> anyhow::Result<(&str, &ModelConfig)> {
        tracing::debug!(model_id, "find_provider_by_model");
        for (key, provider) in &self.providers {
            for model in &provider.models {
                tracing::debug!(provider = %key, model = %model.model_id, "checking");
                if model.model_id == model_id {
                    return Ok((key.as_str(), model));
                }
            }
        }
        tracing::warn!(model_id, available_providers = ?self.providers.keys().collect::<Vec<_>>(), "No provider found for model");
        anyhow::bail!("No provider found for model: {}", model_id)
    }

    fn select_model(&self, entry: &RouteEntry, capability: Capability) -> anyhow::Result<&ModelConfig> {
        match entry.strategy {
            RoutingStrategy::Fixed => {
                let model_id = entry.models.first()
                    .with_context(|| "No models in route entry")?;
                let (_, model) = self.find_provider_by_model(model_id)?;
                if !model.supports(capability) {
                    anyhow::bail!("Model {} does not support {:?}", model_id, capability);
                }
                Ok(model)
            }
            RoutingStrategy::Fallback => {
                for model_id in &entry.models {
                    if let Ok((_, model)) = self.find_provider_by_model(model_id) {
                        if model.supports(capability) {
                            return Ok(model);
                        }
                    }
                }
                anyhow::bail!("All fallback models failed for {:?}", capability)
            }
            RoutingStrategy::Cheapest | RoutingStrategy::Fastest => {
                let entry = RouteEntry {
                    strategy: RoutingStrategy::Fixed,
                    models: entry.models.clone(),
                    provider: entry.provider.clone(),
                };
                self.select_model(&entry, capability)
            }
        }
    }

    pub fn get_chat_routing(&self) -> anyhow::Result<&RouteEntry> {
        self.routing.get(Capability::Chat)
            .with_context(|| "No chat routing configured")
    }

    pub fn resolve_model(&self, model_id: &str) -> anyhow::Result<(&str, &ModelConfig)> {
        let (provider_key, model) = self.find_provider_by_model(model_id)?;
        Ok((provider_key, model))
    }

    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(|s| s.as_str())
    }

    pub fn get_provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    /// Build a Registry from config types.
    ///
    /// This only converts the config data structures — it does NOT instantiate
    /// live ChatProviders.  Use `register_chat()` to add providers afterwards.
    pub fn from_config(
        providers: HashMap<String, config::provider::ProviderConfig>,
        routing: &config::routing::RoutingConfig,
    ) -> anyhow::Result<Self> {
        let registry_providers: HashMap<String, ProviderConfig> = providers
            .into_iter()
            .map(|(api, cfg)| {
                let models = cfg.models
                    .into_iter()
                    .map(|(id, mc)| ModelConfig::from((id.as_str(), mc)))
                    .collect();
                let pc = ProviderConfig {
                    api: api.clone(),
                    api_key: cfg.api_key,
                    base_url: Some(cfg.base_url),
                    models,
                };
                (api, pc)
            })
            .collect();

        let registry_routing = RoutingConfig::from_other(routing);

        Ok(Self::new(registry_providers, registry_routing))
    }
}

// ── Type conversions (config → registry) ─────────────────────────────────────

impl From<(&str, config::provider::ModelConfig)> for ModelConfig {
    fn from((model_id, cfg): (&str, config::provider::ModelConfig)) -> Self {
        Self {
            model_id: model_id.to_string(),
            capabilities: cfg.capabilities.into_iter().map(convert_capability).collect(),
            context_window: cfg.context_window,
            max_tokens: cfg.max_output_tokens,
            reasoning: cfg.reasoning,
        }
    }
}

impl From<config::provider::ProviderConfig> for ProviderConfig {
    fn from(cfg: config::provider::ProviderConfig) -> Self {
        Self {
            // api is set by the caller via ProviderConfig::with_api() or inline
            api: String::new(),
            api_key: cfg.api_key,
            base_url: Some(cfg.base_url),
            models: vec![], // models are set separately in from_config
        }
    }
}

impl ProviderConfig {
    /// Set the API kind (provider name, e.g. "minimax", "openai") — the HashMap key.
    pub fn with_api(mut self, api: String) -> Self {
        self.api = api;
        self
    }
}

fn convert_capability(c: config::provider::Capability) -> Capability {
    use config::provider::Capability as Cc;
    use Capability as Cr;
    match c {
        Cc::Chat => Cr::Chat,
        Cc::Vision => Cr::Vision,
        Cc::NativeTools => Cr::NativeTools,
        Cc::Search => Cr::Search,
        Cc::Embedding => Cr::Embedding,
        Cc::ImageGeneration => Cr::ImageGeneration,
        Cc::TextToSpeech => Cr::TextToSpeech,
        Cc::SpeechToText => Cr::SpeechToText,
        Cc::VideoGeneration => Cr::VideoGeneration,
    }
}

impl RoutingConfig {
    /// Convert from config's RoutingConfig (HashMap<String, RouteEntry>)
    /// to registry's RoutingConfig (flat struct with typed fields).
    pub fn from_other(other: &config::routing::RoutingConfig) -> Self {
        fn convert_entry(e: &config::routing::RouteEntry) -> RouteEntry {
            RouteEntry {
                strategy: match e.strategy {
                    config::routing::RoutingStrategy::Fixed => RoutingStrategy::Fixed,
                    config::routing::RoutingStrategy::Fallback => RoutingStrategy::Fallback,
                    config::routing::RoutingStrategy::Cheapest => RoutingStrategy::Cheapest,
                    config::routing::RoutingStrategy::Fastest => RoutingStrategy::Fastest,
                },
                models: e.models.clone(),
                provider: e.providers.first().cloned(),
            }
        }

        let mut cfg = RoutingConfig::default();
        for (key, entry) in other.iter() {
            let e = convert_entry(entry);
            match key {
                "chat" => cfg.chat = Some(e),
                "search" => cfg.search = Some(e),
                "embedding" => cfg.embedding = Some(e),
                "image_generation" => cfg.image_generation = Some(e),
                "text_to_speech" => cfg.text_to_speech = Some(e),
                "speech_to_text" => cfg.speech_to_text = Some(e),
                "video_generation" => cfg.video_generation = Some(e),
                "vision" | "native_tools"
                    // Map to chat routing.
                    if cfg.chat.is_none() => {
                        cfg.chat = Some(e);
                    }
                _ => {}
            }
        }
        cfg
    }
}

// ── ServiceRegistry trait impl ────────────────────────────────────────────────

impl ServiceRegistry for Registry {
    // ── Register methods ─────────────────────────────────────────────────────

    fn register_chat(&mut self, provider: Box<dyn ChatProvider>, model_id: String) {
        self.chat_providers.insert(model_id, provider.into());
    }

    fn register_embedding(&mut self, provider: Box<dyn EmbeddingProvider>, model_id: String) {
        self.embedding_providers.insert(model_id, provider.into());
    }

    fn register_image(&mut self, provider: Box<dyn ImageGenerationProvider>, model_id: String) {
        self.image_providers.insert(model_id, provider.into());
    }

    fn register_tts(&mut self, provider: Box<dyn TtsProvider>, model_id: String) {
        self.tts_providers.insert(model_id, provider.into());
    }

    fn register_video(&mut self, provider: Box<dyn VideoGenerationProvider>, model_id: String) {
        self.video_providers.insert(model_id, provider.into());
    }

    fn register_search(&mut self, provider: Box<dyn SearchProvider>, model_id: String) {
        self.search_providers.insert(model_id, provider.into());
    }

    fn register_stt(&mut self, provider: Box<dyn SttProvider>, model_id: String) {
        self.stt_providers.insert(model_id, provider.into());
    }

    // ── Get methods ──────────────────────────────────────────────────────────

    fn get_chat_provider(&self, capability: Capability) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> {
        let entry = self.routing.get(capability)
            .with_context(|| format!("No routing for {:?}", capability))?;
        let model = self.select_model(entry, capability)?;
        let provider = self.chat_providers.get(&model.model_id)
            .with_context(|| format!("No live provider for model: {}", model.model_id))?;
        Ok((Arc::clone(provider), model.model_id.clone()))
    }

    fn get_chat_provider_with_hint(
        &self,
        capability: Capability,
        provider_hint: Option<&str>,
    ) -> anyhow::Result<(Arc<dyn ChatProvider>, String)> {
        if let Some(hint) = provider_hint {
            let provider_cfg = self.providers.get(hint)
                .with_context(|| format!("Unknown provider: {}", hint))?;
            let model = provider_cfg.models.first()
                .with_context(|| format!("Provider {} has no models", hint))?;
            let provider = self.chat_providers.get(&model.model_id)
                .with_context(|| format!("No live provider for model: {}", model.model_id))?;
            Ok((Arc::clone(provider), model.model_id.clone()))
        } else {
            self.get_chat_provider(capability)
        }
    }

    fn get_embedding_provider(&self) -> anyhow::Result<(Arc<dyn EmbeddingProvider>, String)> {
        self.route_capability(Capability::Embedding, &self.embedding_providers)
    }

    fn get_image_provider(&self) -> anyhow::Result<(Arc<dyn ImageGenerationProvider>, String)> {
        self.route_capability(Capability::ImageGeneration, &self.image_providers)
    }

    fn get_tts_provider(&self) -> anyhow::Result<(Arc<dyn TtsProvider>, String)> {
        self.route_capability(Capability::TextToSpeech, &self.tts_providers)
    }

    fn get_video_provider(&self) -> anyhow::Result<(Arc<dyn VideoGenerationProvider>, String)> {
        self.route_capability(Capability::VideoGeneration, &self.video_providers)
    }

    fn get_search_provider(&self) -> anyhow::Result<(Arc<dyn SearchProvider>, String)> {
        self.route_capability(Capability::Search, &self.search_providers)
    }

    fn get_stt_provider(&self) -> anyhow::Result<(Arc<dyn SttProvider>, String)> {
        self.route_capability(Capability::SpeechToText, &self.stt_providers)
    }
}

// ── Generic routing helper ────────────────────────────────────────────────────

impl Registry {
    /// Generic capability routing: look up routing config, select model, return provider.
    fn route_capability<T: ?Sized + Send + Sync>(
        &self,
        capability: Capability,
        store: &HashMap<String, Arc<T>>,
    ) -> anyhow::Result<(Arc<T>, String)> {
        let entry = self.routing.get(capability)
            .with_context(|| format!("No routing configured for {:?}", capability))?;
        let model = self.select_model(entry, capability)?;

        // Try exact model_id match first
        if let Some(provider) = store.get(&model.model_id) {
            return Ok((Arc::clone(provider), model.model_id.clone()));
        }

        // Fallback: find any model in the store that supports this capability
        // (in case routing points to a model that wasn't registered for this capability)
        for model_id in store.keys() {
            if let Ok((_, m)) = self.find_provider_by_model(model_id) {
                if m.supports(capability) {
                    if let Some(provider) = store.get(model_id) {
                        return Ok((Arc::clone(provider), model_id.clone()));
                    }
                }
            }
        }

        anyhow::bail!(
            "No {:?} provider registered (routing model: {}, available: [{}])",
            capability,
            model.model_id,
            store.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}
