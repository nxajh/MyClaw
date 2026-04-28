//! agents — Agent loop, session management, and prompt construction.

mod agent_impl;
mod orchestrator;
mod prompt;
mod session_manager;
mod skills;

pub use agent_impl::{Agent, AgentConfig, AgentLoop}; pub use session_manager::{InMemoryBackend, Session};
pub use orchestrator::{Orchestrator, OrchestratorParts};
pub use prompt::{AutonomyLevel, SkillsPromptInjectionMode, SystemPromptBuilder, SystemPromptConfig};
pub use session_manager::SessionManager;
pub use crate::storage::SessionBackend;
pub use skills::{Skill, SkillsManager};
