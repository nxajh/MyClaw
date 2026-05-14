//! agents — Agent loop, session management, and prompt construction.

mod agent_impl;
pub mod context_engine;
pub mod error;
pub mod attachment;
pub mod recovery;
mod delegation;
mod loop_breaker;
mod orchestrator;
mod prompt;
mod session_manager;
mod tool_registry;
mod mcp_manager;
mod sub_agent;
pub mod slash_command;
pub mod turn_event;

/// Scheduling: cron jobs, webhooks, heartbeat scheduler.
pub mod scheduling;
pub use scheduling::cron_loader;
pub use scheduling::heartbeat_tasks;
pub use scheduling::webhook_loader;
pub use scheduling::work_unit;

/// Workspace: agent/skill loading, skill execution, file watching.
pub mod workspace;
pub use workspace::agent_loader;
pub use workspace::skill_loader;
pub use workspace::watcher;
pub use workspace::skills;

pub use agent_impl::{Agent, AgentConfig, AgentLoop, AskUserHandler, DelegateHandler};
pub use recovery::UnfinishedSubAgent;
pub use turn_event::TurnEvent;
pub use attachment::AttachmentManager;
pub use workspace::watcher::{WorkspaceWatcher, ChangeSet};
pub use delegation::{DelegationEvent, DelegationManager};
pub use loop_breaker::{LoopBreak, LoopBreakReason, LoopBreaker, LoopBreakerConfig};
pub use session_manager::{InMemoryBackend, PersistHook, BackendPersistHook, Session, BreakpointItem};
pub use session_manager::{identify_breakpoint, detect_incomplete_turn, process_all_queues};
pub use orchestrator::{Orchestrator, OrchestratorParts, SharedSessions, SchedulerEvent};
pub use prompt::{AutonomyLevel, SkillsPromptInjectionMode, SystemPromptBuilder, SystemPromptConfig};
pub use session_manager::SessionManager;
pub use crate::storage::SessionBackend;
pub use workspace::skill_loader::SkillDefinition;
pub use scheduling::cron_types::{DeliveryConfig, RunRecord, RunStatus, ScheduleKind};
pub use scheduling::scheduler::{
    Scheduler, SharedScheduler, JobEntry, JobUpdate,
    WebhookContext, run_webhook_server, send_to_target,
    is_active_hours, resolve_tz, scan_prompt_injection,
};
pub use scheduling::webhook_loader::{WebhookJobDef, load_webhook_jobs};
pub use workspace::skills::{Skill, SkillManager};
pub use tool_registry::ToolRegistry;
pub use mcp_manager::McpManager;
pub use sub_agent::SubAgentDelegator;
pub use error::AgentError;
pub use context_engine::{ContextEngine, TokenStats, CompactionResult, TokenTracker};
