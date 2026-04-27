//! Agent — shared factory for AgentLoop instances.

#![allow(clippy::module_inception)]

mod agent;
mod session_manager;
mod skills;

pub use agent::{Agent, AgentConfig, AgentLoop};
pub use session_manager::{InMemoryBackend, Session, SessionManager};
pub use skills::{Skill, SkillsManager};
