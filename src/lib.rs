//! MyClaw — AI Agent system.

pub mod agents;
pub mod channels;
pub mod channels_message;  // shared channel message types
pub mod config;
pub mod daemon;
pub mod mcp;
pub mod providers;
pub mod registry;
pub mod storage;
pub mod tools;

// Re-exports
pub use agents::{
    Agent, AgentConfig, AgentLoop, InMemoryBackend, Session, SessionManager,
    SkillsManager, Orchestrator, OrchestratorParts,
    SystemPromptBuilder, SystemPromptConfig,
    AutonomyLevel, SkillsPromptInjectionMode,
};
pub use channels::{Channel, ChannelMessage, SendMessage};
pub use channels_message::DedupState;
pub use config::{AppConfig, ConfigLoader};
pub use registry::{Registry};
pub use providers::ServiceRegistry;
pub use providers::{ChatProvider, FallbackChatProvider, ToolResult};
pub use providers::capability_chat::ToolSpec;