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
pub use myclaw_capability::chat::{
    BoxStream, ChatProvider, ChatRequest, ChatResponse, ChatMessage, ContentPart,
    StreamEvent, StopReason, ToolCall, ToolSpec,
};
pub use myclaw_capability::embedding::{EmbedRequest, EmbedResponse, EmbeddingProvider, EmbedInput};
pub use myclaw_capability::image::{ImageGenerationProvider, ImageRequest, ImageResponse};
pub use myclaw_capability::search::{SearchProvider, SearchRequest, SearchResults};
pub use myclaw_capability::stt::{SttProvider, SttRequest, TranscriptionResponse};
pub use myclaw_capability::tts::{TtsProvider, TtsRequest};
pub use myclaw_capability::video::{VideoGenerationProvider, VideoRequest, VideoResponse};
pub use myclaw_capability::capability::Capability;
