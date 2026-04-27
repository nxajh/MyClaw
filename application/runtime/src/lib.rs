//! runtime — Application Layer
//!
//! Contains: Agent, AgentLoop, SkillsManager, SessionManager, SystemPromptBuilder, Scheduler, Doctor

pub mod agent;
pub mod cron;
pub mod doctor;
pub mod mcp;
pub mod prompt;

// Re-export top-level types for convenient access.
pub use agent::{Agent, AgentConfig, AgentLoop, Session, SessionManager, SkillsManager};
