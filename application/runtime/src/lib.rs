//! runtime — Application Layer
//!
//! Contains: Agent, AgentLoop, SkillsManager, SessionManager,
//! SystemPromptBuilder, Scheduler, Doctor, McpManager
//!
//! DDD: This crate depends only on Domain crates (`capability`, `session`).
//! It does NOT depend on Infrastructure crates directly.

pub mod agent;
pub mod cron;
pub mod doctor;
pub mod mcp;
pub mod prompt;

// Re-export top-level types for convenient access.
pub use agent::{Agent, AgentConfig, AgentLoop, InMemoryBackend, Session, SessionManager, SkillsManager};
pub use prompt::{AutonomyLevel, SkillsPromptInjectionMode, SystemPromptBuilder, SystemPromptConfig};
