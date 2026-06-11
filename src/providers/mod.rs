//! providers — LLM provider implementations and capability traits.

// ── Capability traits (defs live here, impls below) ───────────────────────────

pub mod capability; // Capability, Modality, model configs
pub mod capability_chat; // ChatProvider, ChatMessage, StreamEvent, etc.
pub mod capability_embedding; // EmbeddingProvider, EmbedRequest, etc.
pub mod capability_tool; // Tool, ToolResult
pub mod error_class; // FailoverReason, ClassifiedError
pub mod image; // ImageGenerationProvider
pub mod protocols; // Protocol-specific message rendering & clients
pub mod provider_registry;
pub mod search; // SearchProvider
pub mod stt; // SttProvider
pub mod tts; // TtsProvider
pub mod video; // VideoGenerationProvider // ProviderRegistry trait

// Re-export traits at crate level for external consumers
pub use capability::{
    BasicModelConfig, BasicPricing, Capability, ChatModelConfig, ChatPricing, EmbeddingModelConfig,
    EmbeddingPricing, Modality,
};
pub use capability_chat::{
    BoxStream, ChatMessage, ChatProvider, ChatRequest, ChatResponse, ChatUsage, ContentPart,
    StopReason, StreamEvent, ThinkingConfig, ToolCall, ToolSpec as ChatToolSpec,
};
pub use capability_embedding::{
    EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider, EmbeddingUsage,
};
pub use capability_tool::{Tool, ToolResult, ToolSource, ToolSpec};
pub use error_class::{
    ClassifiedError, ErrorCategory, FailoverReason, ProviderHttpError, RecoveryHints,
};
pub use image::{ImageFormat, ImageGenerationProvider, ImageOutput, ImageRequest, ImageResponse};
pub use provider_registry::{ProviderRegistry, ProviderSummary};
pub use search::{SearchProvider, SearchRequest, SearchResult, SearchResults};
pub use stt::{SttProvider, SttRequest, SttSegment, TranscriptionResponse};
pub use tts::{TtsFormat, TtsProvider, TtsRequest, TtsVoice};
pub use video::{VideoGenerationProvider, VideoRequest, VideoResponse};

// ── Implementations ────────────────────────────────────────────────────────────

pub mod anthropic;
pub mod credential_pool;
pub mod fallback;
pub mod glm;
pub mod google;
pub mod http;
pub mod kimi;
pub mod media;
pub mod minimax;
pub mod openai;
pub mod provider_factory;
pub mod provider_id;
pub mod shared;
pub mod xiaomi;

pub use anthropic::AnthropicProvider;
pub use credential_pool::{
    CredentialEntry, CredentialPool, CredentialStatus, RotationStrategy, SharedCredentialPool,
};
pub use fallback::FallbackChatProvider;
pub use glm::GlmProvider;
pub use google::GoogleProvider;
pub use kimi::KimiProvider;
pub use media::{
    MediaCaps, MediaInputPolicy, MediaLoweringProvider, MediaPolicy, MediaTransport, audio_marker,
    image_marker, lower_media_for,
};
pub use minimax::MiniMaxProvider;
pub use openai::OpenAiProvider;
pub use xiaomi::XiaomiProvider;

pub use provider_factory::{
    BuildChatProviderRequest, BuildEmbeddingProviderRequest, BuildImageProviderRequest,
    BuildSearchProviderRequest, BuildSttProviderRequest, BuildTtsProviderRequest,
    BuildVideoProviderRequest, ProviderFactory,
};
pub use provider_id::{ProviderId, detect_from_url};
pub use shared::AuthStyle;

pub use reqwest::Client;
