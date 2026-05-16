//! Shared utilities for providers: auth and legacy factory/routing.

// ── Auth ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AuthStyle {
    Bearer,
    XApiKey,
}

pub fn build_auth(auth: &AuthStyle, credential: &str) -> String {
    match auth {
        AuthStyle::Bearer => format!("Bearer {}", credential),
        AuthStyle::XApiKey => credential.to_string(),
    }
}

// ── Legacy factory ─────────────────────────────────────────────────────────────

pub fn create_provider(name: &str, api_key: String) -> Option<Box<dyn ProviderInstance>> {
    match name {
        "openai" => Some(Box::new(crate::providers::openai::OpenAiProvider::new(api_key)) as _),
        "anthropic" => Some(Box::new(crate::providers::anthropic::AnthropicProvider::new(api_key)) as _),
        "glm" => Some(Box::new(crate::providers::glm::GlmProvider::new(api_key)) as _),
        "kimi" => Some(Box::new(crate::providers::kimi::KimiProvider::new(api_key)) as _),
        "minimax" => Some(Box::new(crate::providers::minimax::MiniMaxProvider::new(api_key)) as _),
        "xiaomi" | "mimo" => Some(Box::new(crate::providers::xiaomi::XiaomiProvider::new(api_key)) as _),
        _ => None,
    }
}

pub trait ProviderInstance: Send + Sync {}

impl ProviderInstance for crate::providers::openai::OpenAiProvider {}
impl ProviderInstance for crate::providers::anthropic::AnthropicProvider {}
impl ProviderInstance for crate::providers::glm::GlmProvider {}
impl ProviderInstance for crate::providers::kimi::KimiProvider {}
impl ProviderInstance for crate::providers::minimax::MiniMaxProvider {}
impl ProviderInstance for crate::providers::xiaomi::XiaomiProvider {}

/// Create a provider by inspecting the base_url hostname.
/// Falls back to OpenAI-compatible if no specific match is found.
pub fn create_provider_by_url(
    api_key: String,
    base_url: &str,
) -> Option<Box<dyn crate::providers::ChatProvider>> {
    create_provider_by_url_with_user_agent(api_key, base_url, None)
}

/// Create a provider with optional user_agent by inspecting the base_url hostname.
pub fn create_provider_by_url_with_user_agent(
    api_key: String,
    base_url: &str,
    user_agent: Option<&str>,
) -> Option<Box<dyn crate::providers::ChatProvider>> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/').next()
        .unwrap_or("");

    tracing::info!(base_url, host, "auto-detecting provider type from base_url");

    if host.contains("bigmodel.cn") || host.contains("zhipuai") {
        let mut p = crate::providers::glm::GlmProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("xiaomimimo") {
        let mut p = crate::providers::xiaomi::XiaomiProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("anthropic.com") || host.contains("claude.ai") {
        let mut p = crate::providers::anthropic::AnthropicProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("minimax") {
        let mut p = crate::providers::minimax::MiniMaxProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else if host.contains("moonshot") || host.contains("kimi") {
        let mut p = crate::providers::kimi::KimiProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    } else {
        tracing::info!(host, "no specific match, using OpenAI-compatible provider");
        let mut p = crate::providers::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string());
        if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
        Some(Box::new(p))
    }
}

/// Create a full OpenAI provider (Chat + Embedding + Image + TTS) by URL.
pub fn create_full_openai_provider(
    api_key: String,
    base_url: &str,
) -> Option<crate::providers::openai::OpenAiProvider> {
    create_full_openai_provider_with_user_agent(api_key, base_url, None)
}

/// Create a full OpenAI provider with optional user_agent.
pub fn create_full_openai_provider_with_user_agent(
    api_key: String,
    base_url: &str,
    user_agent: Option<&str>,
) -> Option<crate::providers::openai::OpenAiProvider> {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/').next()
        .unwrap_or("");

    if host.contains("anthropic.com") || host.contains("claude.ai")
        || host.contains("bigmodel.cn") || host.contains("zhipuai")
        || host.contains("minimax") || host.contains("moonshot") || host.contains("kimi")
        || host.contains("xiaomimimo")
    {
        return None;
    }

    let mut p = crate::providers::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string());
    if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
    Some(p)
}

/// Capability-aware provider creation result.
pub enum ProviderHandle {
    OpenAi(crate::providers::openai::OpenAiProvider),
    Glm(crate::providers::glm::GlmProvider),
    Google(crate::providers::google::GoogleProvider),
    Kimi(crate::providers::kimi::KimiProvider),
    MiniMax(crate::providers::minimax::MiniMaxProvider),
    Anthropic(crate::providers::anthropic::AnthropicProvider),
    Xiaomi(crate::providers::xiaomi::XiaomiProvider),
}

impl ProviderHandle {
    pub fn from_url(api_key: String, base_url: &str) -> Option<Self> {
        Self::from_url_with_user_agent(api_key, base_url, None)
    }

    pub fn from_url_with_user_agent(api_key: String, base_url: &str, user_agent: Option<&str>) -> Option<Self> {
        let host = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/').next()
            .unwrap_or("");

        if host.contains("bigmodel.cn") || host.contains("zhipuai") {
            let mut p = crate::providers::glm::GlmProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Glm(p))
        } else if host.contains("googleapis.com") || host.contains("google.com") {
            let p = crate::providers::google::GoogleProvider::with_base_url(api_key, base_url.to_string());
            Some(ProviderHandle::Google(p))
        } else if host.contains("xiaomimimo") {
            let mut p = crate::providers::xiaomi::XiaomiProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Xiaomi(p))
        } else if host.contains("anthropic.com") || host.contains("claude.ai") {
            let mut p = crate::providers::anthropic::AnthropicProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Anthropic(p))
        } else if host.contains("minimax") {
            let mut p = crate::providers::minimax::MiniMaxProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::MiniMax(p))
        } else if host.contains("moonshot") || host.contains("kimi") {
            let mut p = crate::providers::kimi::KimiProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::Kimi(p))
        } else {
            let mut p = crate::providers::openai::OpenAiProvider::with_base_url(api_key, base_url.to_string());
            if let Some(ua) = user_agent { p = p.with_user_agent(ua.to_string()); }
            Some(ProviderHandle::OpenAi(p))
        }
    }

    pub fn into_chat_provider(self) -> Box<dyn crate::providers::ChatProvider> {
        match self {
            ProviderHandle::OpenAi(p) => Box::new(p),
            ProviderHandle::Glm(p) => Box::new(p),
            ProviderHandle::Google(_) => panic!("Google provider does not implement ChatProvider"),
            ProviderHandle::Kimi(p) => Box::new(p),
            ProviderHandle::MiniMax(p) => Box::new(p),
            ProviderHandle::Anthropic(p) => Box::new(p),
            ProviderHandle::Xiaomi(p) => Box::new(p),
        }
    }

    pub fn into_embedding_provider(self) -> Option<Box<dyn crate::providers::EmbeddingProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            ProviderHandle::Glm(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    pub fn into_image_provider(self) -> Option<Box<dyn crate::providers::ImageGenerationProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    pub fn into_tts_provider(self) -> Option<Box<dyn crate::providers::TtsProvider>> {
        match self {
            ProviderHandle::OpenAi(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    pub fn into_video_provider(self) -> Option<Box<dyn crate::providers::VideoGenerationProvider>> {
        None
    }

    pub fn into_search_provider(self) -> Option<Box<dyn crate::providers::SearchProvider>> {
        match self {
            ProviderHandle::Glm(p) => Some(Box::new(p)),
            ProviderHandle::Google(p) => Some(Box::new(p)),
            ProviderHandle::MiniMax(p) => Some(Box::new(p)),
            _ => None,
        }
    }

    pub fn into_stt_provider(self) -> Option<Box<dyn crate::providers::SttProvider>> {
        None
    }
}
