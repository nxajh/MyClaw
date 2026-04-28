//! providers — Provider implementations for OpenAI, Anthropic, GLM, Kimi, MiniMax.

pub mod anthropic;
pub mod fallback;
pub mod glm;
pub mod kimi;
pub mod minimax;
pub mod openai;
pub mod shared;

// Re-export from shared module at crate root
pub use shared::{AuthStyle, ProviderHandle, ProviderInstance, create_provider, create_provider_by_url,
    create_full_openai_provider, parse_openai_sse, build_openai_chat_body};

// Re-export fallback decorator
pub use fallback::{FallbackChatProvider, FallbackEntry};

// Re-export Client directly (from reqwest)
pub use reqwest::Client;

// Public types from each provider
pub use anthropic::AnthropicProvider;
pub use glm::GlmProvider;
pub use kimi::KimiProvider;
pub use minimax::MiniMaxProvider;
pub use openai::OpenAiProvider;

// Re-export capability types
pub use capability::chat::{
    BoxStream, ChatProvider, ChatRequest, ChatResponse, ChatMessage, ContentPart,
    StreamEvent, StopReason, ToolCall, ToolSpec,
};
pub use capability::embedding::{EmbedRequest, EmbedResponse, EmbeddingProvider, EmbedInput};
pub use capability::image::{ImageGenerationProvider, ImageRequest, ImageResponse};
pub use capability::search::{SearchProvider, SearchRequest, SearchResults};
pub use capability::stt::{SttProvider, SttRequest, TranscriptionResponse};
pub use capability::tts::{TtsProvider, TtsRequest};
pub use capability::video::{VideoGenerationProvider, VideoRequest, VideoResponse};
pub use capability::capability::Capability;
