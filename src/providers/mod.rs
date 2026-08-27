//! providers — LLM provider implementations and capability traits.

// ── Capability traits (defs live here, impls below) ───────────────────────────

pub mod tokens; // Token estimation over ChatMessage (moved from agents, #151 Phase 3d)
pub mod registry; // Registry facade over providers (moved from top level, #151 Phase 4)
pub mod capability; // Capability, Modality, model configs
pub mod capability_chat; // ChatProvider, ChatMessage, StreamEvent, etc.
pub mod capability_media; // Embedding/STT/TTS/Video/Image capabilities (merged, #151 Phase 10)
pub mod capability_tool; // Tool, ToolResult
pub mod error_class; // FailoverReason, ClassifiedError
pub mod protocols; // Protocol-specific message rendering & clients
pub mod provider_registry;
pub mod search; // SearchProvider
pub mod edge_tts; // EdgeTtsProvider (free Microsoft Edge TTS via subprocess)

// Re-export traits at crate level for external consumers
pub use capability::{
    BasicModelConfig, BasicPricing, Capability, ChatModelConfig, ChatPricing, EmbeddingModelConfig,
    EmbeddingPricing, Modality,
};
pub use capability_chat::{
    BoxStream, ChatMessage, ChatMessageUsage, ChatProvider, ChatRequest, ChatResponse, ChatUsage,
    ContentPart, StopReason, StreamEvent, ThinkingConfig, ToolCall, ToolSpec as ChatToolSpec,
};
pub use capability_media::{
    EmbedInput, EmbedRequest, EmbedResponse, EmbeddingProvider, EmbeddingUsage,
};
pub use capability_tool::{Tool, ToolResult, ToolSource, ToolSpec};
pub use error_class::{
    ClassifiedError, ErrorCategory, FailoverReason, LONG_COOLDOWN_THRESHOLD, ProviderHttpError,
    RecoveryHints, format_cooldown_zh,
};
pub use capability_media::{ImageFormat, ImageGenerationProvider, ImageOutput, ImageRequest, ImageResponse};
pub use provider_registry::{ProviderRegistry, ProviderSummary, SearchFallbackEntry};
pub use registry::Registry;
pub use search::{SearchProvider, SearchRequest, SearchResult, SearchResults};
pub use capability_media::{SttProvider, SttRequest, SttSegment, TranscriptionResponse};
pub use capability_media::{TtsFormat, TtsProvider, TtsRequest, TtsVoice};
pub use capability_media::{VideoGenerationProvider, VideoRequest, VideoResponse};

// ── Implementations ────────────────────────────────────────────────────────────

pub mod vendor_overrides; // Anthropic/Kimi shells + DeepSeek/Qwen body overrides (merged, #151 Phase 10)
pub mod credential_pool;
pub mod fallback;
pub mod glm;
pub mod glm_mcp;
pub mod google;
pub mod http;
pub mod media;
pub mod provider_factory;
pub mod provider_id;
pub mod minimax;
pub mod openai;
pub mod shared;
pub mod xiaomi;

pub use vendor_overrides::{AnthropicProvider, KimiProvider};
pub use credential_pool::{
    CredentialEntry, CredentialPool, CredentialStatus, RotationStrategy, SharedApiKey,
    SharedCredentialPool,
};
pub use fallback::FallbackChatProvider;
pub use glm::GlmProvider;
pub use google::GoogleProvider;
pub use media::{
    FileModality, MediaCaps, MediaInlineDecision, MediaInputPolicy, MediaLoweringProvider,
    MediaMarkerReason, MediaPolicy, MediaTransport, age_media_in_message, audio_marker,
    image_marker, lower_media_for, modality_from_mime,
};
pub use minimax::MiniMaxProvider;
pub use openai::OpenAiProvider;
pub use xiaomi::XiaomiProvider;

pub use provider_factory::{
    BuildChatProviderRequest, BuildEmbeddingProviderRequest, BuildImageProviderRequest,
    BuildSearchProviderRequest, BuildSttProviderRequest, BuildTtsProviderRequest,
    BuildVideoProviderRequest, ProviderFactory,
};
pub use provider_id::{ProviderId, detect_from_url, well_known};
pub use shared::AuthStyle;

pub use reqwest::Client;
