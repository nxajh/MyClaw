//! agents — Agent loop, session management, and prompt construction.

mod agent_impl;
mod delegation;
mod loop_breaker;
mod orchestrator;
mod prompt;
mod session_manager;
pub mod skill_loader;
mod skills;
mod tool_registry;
mod mcp_manager;
mod sub_agent;
pub mod work_unit;

pub use agent_impl::{Agent, AgentConfig, AgentLoop, AskUserHandler, DelegateHandler};
pub use delegation::{DelegationEvent, DelegationManager};
pub use loop_breaker::{LoopBreak, LoopBreakReason, LoopBreaker, LoopBreakerConfig};
pub use session_manager::{InMemoryBackend, PersistHook, BackendPersistHook, Session};
pub use orchestrator::{Orchestrator, OrchestratorParts};
pub use prompt::{AutonomyLevel, SkillsPromptInjectionMode, SystemPromptBuilder, SystemPromptConfig};
pub use session_manager::SessionManager;
pub use crate::storage::SessionBackend;
pub use skill_loader::SkillDefinition;
pub use skills::{Skill, SkillManager};
pub use tool_registry::ToolRegistry;
pub use mcp_manager::McpManager;
pub use sub_agent::SubAgentDelegator;
