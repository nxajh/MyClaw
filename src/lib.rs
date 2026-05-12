//! MyClaw — AI Agent system.

use std::sync::atomic::{AtomicBool, Ordering};

/// Global shutdown flag, set by SIGUSR1. Agent loop exits at the nearest
/// checkpoint (before next LLM call or before tool execution).
pub static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// Convenience helper: check whether the shutdown flag is set.
pub fn is_shutting_down() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

pub mod agents;
pub mod channels;
pub mod config;
pub mod daemon;
pub mod mcp;
pub mod memory;
pub mod providers;
pub mod registry;
pub mod storage;
pub mod str_utils;
pub mod tools;

#[cfg(feature = "tui")]
pub mod tui;

// Re-exports
pub use agents::{
    Agent, AgentConfig, AgentLoop, InMemoryBackend, Session, SessionManager,
    ToolRegistry, SkillManager, Orchestrator, OrchestratorParts,
    SystemPromptBuilder, SystemPromptConfig,
    AutonomyLevel, SkillsPromptInjectionMode,
    McpManager, AskUserHandler, DelegateHandler, SubAgentDelegator,
    DelegationEvent, DelegationManager,
};
pub use channels::{Channel, ChannelMessage, SendMessage, DedupState, ProcessingStatus};
pub use config::{AppConfig, ConfigLoader};
pub use registry::{Registry};
pub use providers::ServiceRegistry;
pub use providers::{
    ChatProvider, FallbackChatProvider, ToolResult,
    XiaomiProvider, // Xiaomi MiMo provider
};
pub use providers::capability_chat::ToolSpec;
