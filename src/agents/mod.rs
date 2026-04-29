//! agents — Agent loop, session management, and prompt construction.

mod agent_impl;
mod loop_breaker;
mod orchestrator;
mod prompt;
mod session_manager;
mod skills;
mod mcp_manager;
mod sub_agent;

pub use agent_impl::{Agent, AgentConfig, AgentLoop, AskUserHandler};
pub use loop_breaker::{LoopBreak, LoopBreakReason, LoopBreaker, LoopBreakerConfig};
pub use session_manager::{InMemoryBackend, Session};
pub use orchestrator::{Orchestrator, OrchestratorParts};
pub use prompt::{AutonomyLevel, SkillsPromptInjectionMode, SystemPromptBuilder, SystemPromptConfig};
pub use session_manager::SessionManager;
pub use crate::storage::SessionBackend;
pub use skills::{Skill, SkillsManager};
pub use mcp_manager::McpManager;
pub use sub_agent::SubAgentDelegator;
