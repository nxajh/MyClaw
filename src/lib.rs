//! MyClaw — AI Agent system.

use std::sync::atomic::{AtomicBool, Ordering};

/// Global shutdown flag, set by SIGUSR1. Agent loop exits at the nearest
/// checkpoint (before next LLM call or before tool execution).
pub static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

pub static TERMINATING_FLAG: AtomicBool = AtomicBool::new(false);

/// Convenience helper: check whether the shutdown flag is set (either hot switch or termination).
pub fn is_shutting_down() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst) || TERMINATING_FLAG.load(Ordering::SeqCst)
}

pub mod api;
pub mod agents;
pub mod channels;
pub mod config;
pub mod daemon;
pub mod hot_switch;
pub mod ids;
pub mod mcp;
pub mod memory;
pub mod migration;
pub mod providers;
pub mod registry;
pub mod signal;
pub mod storage;
pub mod str_utils;
pub mod sys_info;
pub mod tools;
pub mod update_state;

#[cfg(feature = "tui")]
pub mod tui;

// Re-exports
pub use agents::{
    Agent, AgentDelegator, AgentRuntime, DelegationCoordinator, DelegationEvent, InMemoryBackend,
    McpManager, Orchestrator, OrchestratorParts, PermissionMode, RunMode, Session, SessionContext,
    SessionManager, SkillManager, SystemPromptBuilder, SystemPromptConfig, ToolRegistry,
    TurnContext, TurnResult, UserProfile, UserResolver,
};
pub use channels::{
    Channel, ChannelOutboundMessage, DedupState, OutboundSendResult, ProcessingStatus,
    ToolEvent,
};
pub use config::{AppConfig, ConfigLoader};
pub use providers::ProviderRegistry;
pub use providers::capability_chat::ToolSpec;
pub use providers::{
    ChatProvider,
    FallbackChatProvider,
    ToolResult,
    XiaomiProvider, // Xiaomi MiMo provider
};
pub use registry::Registry;
