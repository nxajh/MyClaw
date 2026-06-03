//! agents — Agent loop, session management, and prompt construction.

pub mod resource_provider;
pub mod tool_executor;
pub mod context_engine;
pub mod agent;
pub mod tokens;
pub mod error;
pub mod attachment;
pub mod recovery;
mod delegation;
pub mod loop_breaker;
mod orchestrator;
mod user_messages;
mod prompt;
pub mod session;
mod tool_registry;
mod mcp_manager;
mod delegation_coordinator;
pub mod commands;
pub mod turn_event;
pub mod turn;
pub mod delegator;
pub mod llm_stream;
pub mod session_context;
pub mod agent_registry;
pub mod ask_router;
pub mod runtime;
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
pub use workspace::watcher;
pub use workspace::skills;

pub use recovery::UnfinishedSubAgent;
pub use turn_event::TurnEvent;
pub use turn::{TurnContext, TurnResult};
pub use agent::Agent;
pub use delegator::AgentDelegator;
pub use session_context::SessionContext;
pub use agent_registry::AgentRegistry;
pub use orchestrator::OrchestratorEvent;
pub use ask_router::AskRouter;
pub use runtime::AgentRuntime;
pub use user_profile::{UserProfile, UserResolver};
pub use attachment::AttachmentManager;
pub use workspace::watcher::{WorkspaceWatcher, ChangeSet};
pub use delegation::DelegationEvent;
pub use loop_breaker::{LoopBreak, LoopBreakReason, LoopBreaker, LoopBreakerConfig};
pub use session::{InMemoryBackend, PersistHook, BackendPersistHook, Session, BreakpointItem};
pub use session::{identify_breakpoint, detect_incomplete_turn};
pub use orchestrator::{Orchestrator, OrchestratorCtx, OrchestratorParts, SchedulerEvent};
pub use prompt::{PermissionMode, RunMode, SystemPromptBuilder, SystemPromptConfig};
pub use session::SessionManager;
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
pub use delegation_coordinator::DelegationCoordinator;
pub use error::AgentError;
