//! agents — Agent loop, session management, and prompt construction.

pub mod agent;
pub mod agent_registry;
pub mod ask_router;
pub mod attachment;
pub mod commands;
pub mod context_engine;
mod delegation;
mod delegation_coordinator;
pub mod delegator;
pub mod known_users;
pub mod error;
pub mod llm_stream;
pub mod loop_breaker;
mod mcp_manager;
#[cfg(test)]
mod media_e2e_test;
pub mod memory_fork;
mod orchestrator;
mod prompt;
pub mod recovery;
pub mod resource_provider;
pub mod runtime;
pub mod session;
pub mod session_context;
pub mod tokens;
pub mod tool_executor;
mod tool_registry;
pub mod turn;
pub mod turn_event;
mod user_messages;
pub mod user_profile;

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
pub use workspace::skills;
pub use workspace::watcher;

pub use crate::storage::SessionBackend;
pub use agent::Agent;
pub use agent_registry::AgentRegistry;
pub use ask_router::AskRouter;
pub use known_users::{KnownUser, KnownUsersRegistry};
pub use attachment::AttachmentManager;
pub use delegation::DelegationEvent;
pub use delegation_coordinator::DelegationCoordinator;
pub use delegator::AgentDelegator;
pub use error::AgentError;
pub use loop_breaker::{LoopBreak, LoopBreakReason, LoopBreaker, LoopBreakerConfig};
pub use mcp_manager::McpManager;
pub use orchestrator::OrchestratorEvent;
pub use orchestrator::{Orchestrator, OrchestratorCtx, OrchestratorParts, SchedulerEvent};
pub use prompt::{PermissionMode, RunMode, SystemPromptBuilder, SystemPromptConfig};
pub use recovery::UnfinishedSubAgent;
pub use runtime::AgentRuntime;
pub use scheduling::cron_types::{DeliveryConfig, RunRecord, RunStatus, ScheduleKind};
pub use scheduling::scheduler::{
    JobEntry, JobUpdate, Scheduler, SharedScheduler, WebhookContext, is_active_hours, resolve_tz,
    run_webhook_server, scan_prompt_injection, send_to_target,
};
pub use scheduling::webhook_loader::{WebhookJobDef, load_webhook_jobs};
pub use session::SessionManager;
pub use session::{BackendPersistHook, BreakpointItem, InMemoryBackend, PersistHook, Session};
pub use session::{detect_incomplete_turn, identify_breakpoint};
pub use session_context::SessionContext;
pub use tool_registry::ToolRegistry;
pub use turn::{TurnContext, TurnResult};
pub use turn_event::{
    RunSummary, TokenUsage, TtsSummary, TurnEvent, VersionedEvent,
};
pub use user_profile::{UserProfile, UserResolver};
pub use workspace::skill_loader::SkillDefinition;
pub use workspace::skills::{Skill, SkillManager};
pub use workspace::watcher::{ChangeSet, WorkspaceWatcher};
