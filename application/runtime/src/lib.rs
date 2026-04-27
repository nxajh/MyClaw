//! runtime — Application Layer
//!
//! Contains:
//! - **Agent / AgentLoop** — core chat loop with tool calling
//! - **SkillsManager** — tool registry and dispatch
//! - **SessionManager** — session lifecycle and persistence
//! - **SystemPromptBuilder** — prompt assembly
//! - **Orchestrator** — message routing Application Service
//!
//! DDD: This crate depends only on Domain crates (`capability`, `session`)
//! and the Interface trait (`channels`). It does NOT depend on Infrastructure
//! concrete types — those are injected via the Composition Root.

pub mod agent;
pub mod cron;
pub mod doctor;
pub mod mcp;
pub mod orchestrator;
pub mod prompt;
pub mod skills;

// Re-export top-level types for convenient access.
pub use agent::{Agent, AgentConfig, AgentLoop, InMemoryBackend, Session, SessionManager, SkillsManager};
pub use orchestrator::{Orchestrator, OrchestratorParts};
pub use prompt::{AutonomyLevel, SkillsPromptInjectionMode, SystemPromptBuilder, SystemPromptConfig};
