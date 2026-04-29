//! MyClaw — AI Agent system.

pub mod agents;
pub mod channels;
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
    McpManager, AskUserHandler, DelegateHandler, SubAgentDelegator,
    DelegationEvent, DelegationManager,
};
pub use channels::{Channel, ChannelMessage, SendMessage, DedupState};
pub use config::{AppConfig, ConfigLoader};
pub use registry::{Registry};
pub use providers::ServiceRegistry;
pub use providers::{
    ChatProvider, FallbackChatProvider, ToolResult,
    XiaomiProvider, // Xiaomi MiMo provider
};
pub use providers::capability_chat::ToolSpec;
