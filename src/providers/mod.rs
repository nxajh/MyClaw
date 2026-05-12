//! providers — LLM provider implementations and capability traits.

// ── Capability traits (defs live here, impls below) ───────────────────────────

pub mod capability;           // Capability, Modality, model configs
pub mod capability_chat;     // ChatProvider, ChatMessage, StreamEvent, etc.
pub mod capability_embedding; // EmbeddingProvider, EmbedRequest, etc.
pub mod capability_tool;      // Tool, ToolResult
pub mod error_class;          // FailoverReason, ClassifiedError
pub mod image;               // ImageGenerationProvider
pub mod search;              // SearchProvider
pub mod tts;                 // TtsProvider
pub mod stt;                 // SttProvider
pub mod video;               // VideoGenerationProvider
pub mod service_registry;    // ServiceRegistry trait

// Re-export traits at crate level for external consumers
pub use capability::{
    Capability, Modality,
    ChatModelConfig, EmbeddingModelConfig, BasicModelConfig,
    ChatPricing, EmbeddingPricing, BasicPricing,
};
pub use capability_chat::{
    BoxStream, ChatProvider, ChatRequest, ChatResponse, ChatMessage, ContentPart,
    StopReason, StreamEvent, ToolCall, ToolSpec as ChatToolSpec, ThinkingConfig, ImageDetail, ChatUsage,
};
pub use error_class::{ClassifiedError, ErrorCategory, FailoverReason, RecoveryHints};
pub use capability_embedding::{
    EmbeddingProvider, EmbedRequest, EmbedResponse, EmbeddingUsage, EmbedInput,
};
pub use capability_tool::{Tool, ToolResult, ToolSpec};
pub use image::{ImageGenerationProvider, ImageRequest, ImageResponse, ImageFormat, ImageOutput};
pub use search::{SearchProvider, SearchRequest, SearchResult, SearchResults};
pub use tts::{TtsProvider, TtsRequest, TtsFormat, TtsVoice};
pub use stt::{SttProvider, SttRequest, TranscriptionResponse, SttSegment};
pub use video::{VideoGenerationProvider, VideoRequest, VideoResponse};
pub use service_registry::{ServiceRegistry, ProviderSummary};

// ── Implementations ────────────────────────────────────────────────────────────

pub mod anthropic;
pub mod credential_pool;
pub mod fallback;
pub mod glm;
pub mod google;
pub mod http;
pub mod kimi;
pub mod minimax;
pub mod openai;
pub mod shared;
pub mod xiaomi;

pub use fallback::FallbackChatProvider;
pub use credential_pool::{CredentialPool, CredentialEntry, CredentialStatus, RotationStrategy, SharedCredentialPool};
pub use anthropic::AnthropicProvider;
pub use glm::GlmProvider;
pub use google::GoogleProvider;
pub use kimi::KimiProvider;
pub use minimax::MiniMaxProvider;
pub use openai::OpenAiProvider;
pub use xiaomi::XiaomiProvider;

pub use shared::{AuthStyle, ProviderHandle, ProviderInstance,
    create_provider, create_provider_by_url, create_provider_by_url_with_user_agent,
    create_full_openai_provider, create_full_openai_provider_with_user_agent,
    parse_openai_sse, build_openai_chat_body};

pub use reqwest::Client;