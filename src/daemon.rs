//! Daemon — MyClaw server process entry point (Composition Root).
//!
//! This is the **Composition Root** in DDD terms:
//! 1. Load config from TOML file
//! 2. Assemble all Infrastructure components (Registry, Providers, Tools, Storage)
//! 3. Inject them into Application layer (Orchestrator, Agent)
//! 4. Run the daemon until shutdown signal
//!
//! DDD: The Composition Root is the *only* place that knows about concrete
//! Infrastructure types. Application layer receives everything through traits.

use anyhow::{Context, Result};
use crate::agents::{
    Agent, AgentConfig, InMemoryBackend, Orchestrator, OrchestratorParts, SessionManager,
    ToolRegistry, SkillManager, Skill, SystemPromptConfig, AutonomyLevel, SkillsPromptInjectionMode,
    McpManager, SubAgentDelegator, DelegationManager,
};
use crate::tools::TaskDelegator;
use crate::config::sub_agent::SubAgentConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use crate::channels::Channel;

/// Default config file locations.
const DEFAULT_CONFIG_PATHS: &[&str] = &[
    "myclaw.toml",
    "~/.myclaw/myclaw.toml",
    "/etc/myclaw/myclaw.toml",
];

/// Load configuration from the first found config file.
pub fn load_config() -> Result<crate::config::AppConfig> {
    for path in DEFAULT_CONFIG_PATHS {
        let expanded = shellexpand::tilde(path).to_string();
        let p = PathBuf::from(expanded);
        if p.exists() {
            tracing::info!(path = %p.display(), "loading config");
            return crate::config::ConfigLoader::from_file(&p)
                .context("failed to load config");
        }
    }
    anyhow::bail!(
        "No config file found. Looked in: {}",
        DEFAULT_CONFIG_PATHS.join(", ")
    );
}

/// Load configuration from a specific path.
pub fn load_config_from(path: &str) -> Result<crate::config::AppConfig> {
    let expanded = shellexpand::tilde(path).to_string();
    let p = PathBuf::from(expanded.clone());
    if !p.exists() {
        anyhow::bail!("Config file not found: {}", expanded);
    }
    tracing::info!(path = %p.display(), "loading config");
    crate::config::ConfigLoader::from_file(&p).context("failed to load config")
}

/// Initialize tracing subscriber based on config.
pub fn init_tracing(config: &crate::config::AppConfig) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let level = config
        .logging
        .level
        .as_deref()
        .unwrap_or("info");

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_thread_ids(true));

    tracing::subscriber::set_global_default(subscriber)
        .expect("failed to set tracing subscriber");
}

/// Calculate max output bytes from model config (max_output_tokens).
/// Approximate: 1 token ≈ 4 bytes, with 100KB minimum as safety floor.
fn calculate_max_output_bytes(
    config: &crate::config::AppConfig,
    _registry: &Arc<dyn crate::providers::ServiceRegistry>,
) -> usize {
    // Try to get max_output_tokens from the first chat model
    let default_bytes = 100 * 1024; // 100KB default
    
    if let Some(chat_route) = config.routing.get(crate::providers::Capability::Chat) {
        if let Some(model_id) = chat_route.models.first() {
            // Search through all providers for this model
            for provider_config in config.providers.values() {
                if let Some(chat_config) = &provider_config.chat {
                    if let Some(model_config) = chat_config.models.get(model_id) {
                        if let Some(max_tokens) = model_config.max_output_tokens {
                            // 1 token ≈ 4 bytes, minimum 100KB
                            let bytes = (max_tokens as usize * 4).max(default_bytes);
                            tracing::info!(
                                model = %model_id,
                                max_output_tokens = max_tokens,
                                max_output_bytes = bytes,
                                "calculated max output bytes from model config"
                            );
                            return bytes;
                        }
                    }
                }
            }
        }
    }
    
    tracing::info!(max_output_bytes = default_bytes, "using default max output bytes");
    default_bytes
}

/// Print startup banner with config summary.
fn print_banner(config: &crate::config::AppConfig, mcp_servers: usize, mcp_tools: usize, sub_agent_count: usize, sub_agent_names: &[String]) {
    println!();
    println!("🐾 MyClaw Daemon");
    println!("  📁 Workspace: {}", config.workspace_dir.display());

    let channels: Vec<&str> = config
        .channels
        .enabled_channels()
        .iter()
        .map(|s| &**s)
        .collect();
    println!("  📡 Channels: {}", channels.join(", "));

    let providers: Vec<&str> = config.providers.keys().map(|s| &**s).collect();
    println!("  🤖 Providers: {}", providers.join(", "));

    if let Some(chat_route) = config
        .routing
        .get(crate::providers::Capability::Chat)
        .map(|e| e.models.join(" → "))
    {
        println!("  🗺️  Chat route: {}", chat_route);
    }

    if mcp_servers > 0 {
        println!("  🔌 MCP servers: {} ({} tools)", mcp_servers, mcp_tools);
    }

    if sub_agent_count > 0 {
        let names: Vec<&str> = sub_agent_names.iter().map(|s| s.as_str()).collect();
        println!("  🤝 Sub-agents: {} ({})", sub_agent_count, names.join(", "));
    }

    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();
}

// ═══════════════════════════════════════════════════════════════════════════════
// Composition Root — assemble all components
// ═══════════════════════════════════════════════════════════════════════════════

/// Build the Registry and register all providers from config.
fn build_registry(config: &crate::config::AppConfig) -> anyhow::Result<crate::registry::Registry> {
    let mut registry =
        crate::registry::Registry::from_config(config.providers.clone(), &config.routing)
            .context("failed to build registry")?;

    for (provider_key, provider_cfg) in &config.providers {
        // ── Chat ──────────────────────────────────────────────────────
        if let Some(ref chat) = provider_cfg.chat {
            let api_key = provider_cfg.effective_api_key(chat.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}'", provider_key))?;
            let user_agent = chat.user_agent.as_deref();

            for (model_id, model_cfg) in &chat.models {
                tracing::info!(
                    provider = %provider_key,
                    model = %model_id,
                    capability = "chat",
                    "registering chat provider"
                );

                let handle = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(),
                    &chat.base_url,
                    user_agent,
                ).with_context(|| format!(
                    "cannot determine provider type from base_url '{}' (key='{}')",
                    chat.base_url, provider_key
                ))?;

                let chat_provider: Box<dyn crate::providers::ChatProvider> = handle.into_chat_provider();
                registry.register_chat(chat_provider, model_id.clone(), model_cfg.clone());
            }
        }

        // ── Embedding ─────────────────────────────────────────────────
        if let Some(ref emb) = provider_cfg.embedding {
            let api_key = provider_cfg.effective_api_key(emb.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' embedding", provider_key))?;
            let user_agent = emb.user_agent.as_deref();

            for model_id in emb.models.keys() {
                tracing::info!(
                    provider = %provider_key,
                    model = %model_id,
                    capability = "embedding",
                    "registering embedding provider"
                );

                if let Some(emb_provider) = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(), &emb.base_url, user_agent,
                ).and_then(|h| h.into_embedding_provider()) {
                    registry.register_embedding(emb_provider, model_id.clone());
                }
            }
        }

        // ── ImageGeneration ───────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.image_generation {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' image_generation", provider_key))?;
            let user_agent = sec.user_agent.as_deref();

            for model_id in sec.models.keys() {
                if let Some(img) = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(), &sec.base_url, user_agent,
                ).and_then(|h| h.into_image_provider()) {
                    registry.register_image(img, model_id.clone());
                }
            }
        }

        // ── TTS ───────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.tts {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' tts", provider_key))?;
            let user_agent = sec.user_agent.as_deref();

            for model_id in sec.models.keys() {
                if let Some(tts) = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(), &sec.base_url, user_agent,
                ).and_then(|h| h.into_tts_provider()) {
                    registry.register_tts(tts, model_id.clone());
                }
            }
        }

        // ── Video ─────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.video {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' video", provider_key))?;
            let user_agent = sec.user_agent.as_deref();

            for model_id in sec.models.keys() {
                if let Some(vid) = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(), &sec.base_url, user_agent,
                ).and_then(|h| h.into_video_provider()) {
                    registry.register_video(vid, model_id.clone());
                }
            }
        }

        // ── Search ────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.search {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' search", provider_key))?;
            let user_agent = sec.user_agent.as_deref();

            for model_id in sec.models.keys() {
                if let Some(srch) = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(), &sec.base_url, user_agent,
                ).and_then(|h| h.into_search_provider()) {
                    registry.register_search(srch, model_id.clone());
                }
            }
        }

        // ── STT ───────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.stt {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' stt", provider_key))?;
            let user_agent = sec.user_agent.as_deref();

            for model_id in sec.models.keys() {
                if let Some(stt) = crate::providers::ProviderHandle::from_url_with_user_agent(
                    api_key.clone(), &sec.base_url, user_agent,
                ).and_then(|h| h.into_stt_provider()) {
                    registry.register_stt(stt, model_id.clone());
                }
            }
        }
    }

    // --- Wrap with FallbackChatProvider if strategy is Fallback ---
    registry.maybe_wrap_chat_fallback(&config.routing);

    Ok(registry)
}

/// Build ToolRegistry with all built-in + MCP + SkillTool registered.
async fn build_tools(mcp_manager: &McpManager, skills: &Arc<parking_lot::RwLock<SkillManager>>) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    let builtin = crate::tools::builtin_tools();
    for tool in builtin {
        tools.register(tool);
    }

    // Register additional built-in tools.
    tools.register(Arc::new(crate::tools::ListDirTool::new()));
    tools.register(Arc::new(crate::tools::TaskManagerTool::new(
        crate::tools::TaskManagerTool::shared_state(),
    )));

    // SkillTool — loads skill body on demand.
    tools.register(Arc::new(crate::tools::SkillTool::new(Arc::clone(skills))));

    // Inject MCP tools (if any servers are configured and connected).
    if mcp_manager.is_connected().await {
        let mcp_tools = mcp_manager.tools().await;
        let count = mcp_tools.len();
        for tool in mcp_tools {
            tools.register(tool);
        }
        tracing::info!(mcp_tools = count, "MCP tools registered");
    } else {
        tracing::debug!("MCP manager not connected, skipping MCP tool injection");
    }

    tracing::info!(tool_count = tools.tool_count(), "tool registry built");
    tools
}

/// Build SkillManager from SKILL.md files in workspace.
fn build_skill_manager(workspace_dir: &std::path::Path) -> SkillManager {
    let mut manager = SkillManager::new();
    let skills_dir = workspace_dir.join("skills");
    let definitions = crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    for def in definitions {
        tracing::info!(name = %def.name, "skill registered");
        manager.register(Skill::from_definition(&def));
    }
    tracing::info!(skill_count = manager.skill_count(), "skill manager built");
    manager
}

/// Build sub-agent configs from AGENT.md files in workspace.
///
/// Sub-agents are defined in `workspace/agents/<name>/AGENT.md` — each file
/// contains YAML front matter (metadata) and Markdown body (system prompt).
fn build_sub_agents(workspace_dir: &std::path::Path) -> Vec<crate::config::sub_agent::SubAgentConfig> {
    let agents_dir = workspace_dir.join("agents");
    let agents = crate::agents::agent_loader::load_agents_from_dir(&agents_dir);
    if !agents.is_empty() {
        tracing::info!(agent_count = agents.len(), "sub-agents loaded from workspace");
    }
    agents
}

/// Build the session backend (shared with SessionManager and persist hooks).
fn build_session_backend(config: &crate::config::AppConfig) -> Arc<dyn crate::storage::SessionBackend> {
    let sessions_dir = config.workspace_dir.join("sessions");
    match crate::storage::JsonFileBackend::open(&sessions_dir) {
        Ok(backend) => {
            tracing::info!(path = %sessions_dir.display(), "session storage opened");
            Arc::new(backend)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to open session storage, falling back to in-memory");
            Arc::new(InMemoryBackend::new())
        }
    }
}

/// Build Channel adapters from config.
fn build_channels(config: &crate::config::AppConfig) -> Vec<(&'static str, Arc<dyn Channel>)> {
    let mut channels: Vec<(&'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref cfg) = config.channels.telegram {
        if cfg.enabled {
            channels.push((
                "telegram",
                Arc::new(crate::channels::telegram::TelegramChannel::new(cfg.clone())),
            ));
        }
    }

    #[cfg(feature = "wechat")]
    if let Some(ref cfg) = config.channels.wechat {
        if cfg.enabled {
            channels.push((
                "wechat",
                Arc::new(crate::channels::wechat::WechatChannel::new(cfg.clone())),
            ));
        }
    }

    #[cfg(feature = "qqbot")]
    if let Some(ref cfg) = config.channels.qqbot {
        if cfg.enabled {
            channels.push((
                "qqbot",
                Arc::new(crate::channels::qqbot::QQBotChannel::new(cfg.clone())),
            ));
        }
    }

    channels
}

/// Convert config agent settings into Application-layer prompt config.
fn build_prompt_config(
    cfg: &crate::config::agent::AgentConfig,
    workspace_dir: &std::path::Path,
    knowledge_dir: &std::path::Path,
) -> SystemPromptConfig {
    SystemPromptConfig {
        workspace_dir: workspace_dir.to_string_lossy().to_string(),
        knowledge_dir: knowledge_dir.to_string_lossy().to_string(),
        model_name: cfg.prompt.model_name.clone().unwrap_or_default(),
        autonomy: match cfg.autonomy_level {
            crate::config::agent::AutonomyLevel::Full => AutonomyLevel::Full,
            crate::config::agent::AutonomyLevel::Default => AutonomyLevel::Default,
            crate::config::agent::AutonomyLevel::ReadOnly => AutonomyLevel::ReadOnly,
        },
        skills_mode: SkillsPromptInjectionMode::Compact,
        compact: cfg.prompt.compact,
        max_chars: cfg.prompt.max_chars,
        bootstrap_max_chars: cfg.prompt.bootstrap_max_chars,
        native_tools: cfg.prompt.native_tools,
        channel_name: cfg.prompt.channel_name.clone(),
        host_info: None,
        timezone_offset: cfg.prompt.timezone_offset,
    }
}

/// Run the MyClaw daemon, blocking until shutdown.
pub async fn run(config: crate::config::AppConfig) -> Result<()> {
    // 让进程 cwd 与 workspace_dir 一致，保证 file_read 等工具的相对路径解析
    // 和 system prompt 告诉 LLM 的 "Working directory" 一致
    std::env::set_current_dir(&config.workspace_dir).with_context(|| {
        format!("failed to set cwd to workspace_dir '{}'", config.workspace_dir.display())
    })?;

    // Write PID file for hot-switch coordination (used by `myclaw update`).
    let pid_file = std::env::temp_dir().join("myclaw.pid");
    if let Err(e) = std::fs::write(&pid_file, std::process::id().to_string()) {
        tracing::warn!(error = %e, "failed to write PID file (non-fatal)");
    } else {
        tracing::debug!(pid = %std::process::id(), path = %pid_file.display(), "PID file written");
    }

    // Ensure knowledge directory exists
    if let Err(e) = crate::memory::ensure_memory_dir(
        config.knowledge_dir.to_str().unwrap_or("."),
    ) {
        tracing::warn!(error = %e, "failed to create knowledge directory (non-fatal)");
    }

    // ── Composition Root: assemble all components ──────────────────────────

    let registry = build_registry(&config)?;

    let mcp_manager = McpManager::new();
    if let Err(e) = mcp_manager.connect(&config.mcp_servers).await {
        tracing::warn!(error = %e, "MCP server connection had errors (non-fatal), continuing");
    }

    // Build skill manager (SKILL.md files).
    let skills = build_skill_manager(&config.workspace_dir);
    let skills_arc: Arc<parking_lot::RwLock<SkillManager>> = Arc::new(parking_lot::RwLock::new(skills));

    // Build tool registry (all built-in + MCP + SkillTool).
    let mut tools = build_tools(&mcp_manager, &skills_arc).await;

    // Build sub-agent configs (AGENT.md files from workspace/agents/).
    let sub_agent_configs = build_sub_agents(&config.workspace_dir);
    let sub_agent_count = sub_agent_configs.len();
    let sub_agent_names: Vec<String> = sub_agent_configs.iter().map(|a| a.name.clone()).collect();
    let sub_agent_configs_arc: Arc<parking_lot::RwLock<Vec<SubAgentConfig>>> =
        Arc::new(parking_lot::RwLock::new(sub_agent_configs));

    let registry_arc: Arc<dyn crate::providers::ServiceRegistry> = Arc::new(registry);

    // Register WebSearchTool — requires ServiceRegistry for search routing.
    let search_cooldown = Arc::new(crate::tools::search_cooldown::SearchProviderCooldown::new());
    tools.register(Arc::new(crate::tools::WebSearchTool::new(
        registry_arc.clone(),
        Arc::clone(&search_cooldown),
    )));
    tracing::debug!("web_search tool registered (connected to ServiceRegistry)");

    // WorkspaceWatcher for hot-reload.
    let watcher = crate::agents::WorkspaceWatcher::new(&config.workspace_dir, &config.knowledge_dir)?;
    let change_rx = watcher.rx.clone();

    // ── Sub-agent delegator (conditional) ──────────────────────────────────────

    let (tools_arc, sub_agent_delegator_arc) = if sub_agent_count == 0 {
        // Single-agent mode: add tool_search to base registry.
        let base_tools_arc: Arc<ToolRegistry> = Arc::new(tools);
        let tool_search = crate::tools::ToolSearchTool::new(Arc::clone(&base_tools_arc));

        let mut final_tools = ToolRegistry::new();
        for tool in base_tools_arc.all_tools() {
            final_tools.register(tool);
        }
        final_tools.register(Arc::new(tool_search));

        (Arc::new(final_tools), None)
    } else {
        tracing::info!(agents = sub_agent_count, "multi-agent mode enabled");

        let base_tools_arc: Arc<ToolRegistry> = Arc::new(tools);

        let delegator = SubAgentDelegator::new(
            Arc::clone(&sub_agent_configs_arc),
            registry_arc.clone(),
            Arc::clone(&base_tools_arc),
            Arc::clone(&skills_arc),
            config.agent.max_tool_calls,
            config.workspace_dir.join("sessions"),
            config.workspace_dir.join("worktrees"),
        );
        let delegator_arc = Arc::new(delegator);

        // Build agent_delegate tool.
        let delegate_tool = crate::tools::AgentDelegateTool::new(
            Arc::clone(&delegator_arc) as Arc<dyn TaskDelegator>,
        );

        // Build parent tool registry: same tools + agent_delegate + tool_search.
        let mut parent_tools = ToolRegistry::new();
        for tool in base_tools_arc.all_tools() {
            parent_tools.register(tool);
        }
        parent_tools.register(Arc::new(delegate_tool));
        tracing::info!("agent_delegate tool registered (multi-agent mode)");

        let tool_search = crate::tools::ToolSearchTool::new(Arc::clone(&base_tools_arc));
        parent_tools.register(Arc::new(tool_search));

        (Arc::new(parent_tools), Some(delegator_arc))
    };

    // ── Delegation channel (conditional — only when sub-agents configured) ─────
    let (delegation_manager, delegation_rx) = if sub_agent_delegator_arc.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agents::DelegationEvent>(100);
        (Some(Arc::new(DelegationManager::new(tx))), Some(rx))
    } else {
        (None, None)
    };

    let session_backend = build_session_backend(&config);
    let session_manager = Arc::new(SessionManager::new(Arc::clone(&session_backend)));
    let mut channels = build_channels(&config);

    // ── Sub-agent recovery: detect interrupted sub-agents from a previous run ──
    let sessions_root = config.workspace_dir.join("sessions");
    let unfinished_subagents = scan_unfinished_subagents(&sessions_root);
    if !unfinished_subagents.is_empty() {
        tracing::info!(
            count = unfinished_subagents.len(),
            "detected unfinished sub-agents from previous run"
        );
        for sa in &unfinished_subagents {
            tracing::info!(
                agent = %sa.agent_name,
                task_id = %sa.task_id,
                "  unfinished sub-agent"
            );
        }
    }
    // Clean up stale marker files — they belong to the old process.
    cleanup_stale_subagent_markers(&sessions_root);

    // Create ClientChannel separately (needs session_manager for management API).
    #[cfg(feature = "client")]
    let _client_channel: Option<Arc<crate::channels::ClientChannel>> =
        config.channels.client.as_ref().filter(|c| c.enabled).map(|cfg| {
            let cc = crate::channels::ClientChannel::new(cfg.clone());
            cc.set_session_manager(session_manager.clone());
            cc.set_tool_names(tools_arc.all_tools().iter().map(|t| t.spec().name.to_string()).collect());
            cc.set_workspace_dir(config.workspace_dir.clone());
            Arc::new(cc)
        });
    #[cfg(feature = "client")]
    if let Some(ref cc) = _client_channel {
        channels.push(("client", cc.clone() as Arc<dyn Channel>));
    }

    let agent_config = AgentConfig {
        max_tool_calls: config.agent.max_tool_calls,
        max_history: config.agent.max_history,
        prompt_config: build_prompt_config(&config.agent, &config.workspace_dir, &config.knowledge_dir),
        context: config.agent.context.clone(),
        stream_chunk_timeout_secs: config.agent.stream_chunk_timeout_secs,
        max_output_bytes: calculate_max_output_bytes(&config, &registry_arc),
        loop_breaker_threshold: config.agent.loop_breaker_threshold as usize,
        tool_timeout_secs: config.agent.tool_timeout_secs,
    };
    let mcp_manager_arc = Arc::new(mcp_manager);

    // Get MCP server instructions for attachment injection.
    let mcp_instructions = mcp_manager_arc.server_instructions().await;

    // Scheduler config (used for both parts and webhook launch).
    let scheduler_config = config.agent.scheduler.clone();

    let agent = Agent::new(registry_arc, tools_arc, skills_arc, agent_config)
        .with_mcp_instructions(mcp_instructions)
        .with_sub_agent_configs(sub_agent_configs_arc)
        .with_workspace_dirs(
            config.workspace_dir.join("skills"),
            config.workspace_dir.join("agents"),
        );

    // Create scheduler channel for Scheduler → Orchestrator communication.
    let (scheduler_tx, scheduler_rx) = tokio::sync::mpsc::channel::<crate::agents::SchedulerEvent>(100);

    let session_manager_for_webhook = Arc::clone(&session_manager);

    let parts = OrchestratorParts {
        agent: agent.clone(),
        session_manager,
        channels,
        sub_delegator: sub_agent_delegator_arc,
        delegation_manager,
        delegation_rx,
        persist_backend: session_backend.clone(),
        mcp_manager: Some(Arc::clone(&mcp_manager_arc)),
        change_rx: Some(change_rx.clone()),
        scheduler_rx: Some(scheduler_rx),
        search_cooldown: Some(search_cooldown),
        unfinished_subagents,
    };

    // ── Launch ─────────────────────────────────────────────────────────────

    let (mut orchestrator, _msg_tx) = Orchestrator::new(parts);
    print_banner(&config, mcp_manager_arc.server_count().await, mcp_manager_arc.tool_count().await, sub_agent_count, &sub_agent_names);

    // ── Scheduler tasks ────────────────────────────────────────────────────

    if scheduler_config.webhook.enabled {
        let wh_dir = config.workspace_dir.join("webhooks");
        let wh_jobs = crate::agents::load_webhook_jobs(&wh_dir);
        let wh_ctx = Arc::new(crate::agents::WebhookContext {
            agent: agent.clone(),
            channels: orchestrator.shared().channels,
            sessions: orchestrator.shared().sessions,
            session_manager: session_manager_for_webhook,
            session_backend: session_backend.clone(),
            timezone_offset: config.agent.prompt.timezone_offset,
            last_channel: orchestrator.shared().last_channel,
            change_rx: Some(change_rx.clone()),
        });
        let wh_config = scheduler_config.webhook.clone();
        tokio::spawn(async move {
            crate::agents::run_webhook_server(wh_ctx, wh_config, wh_jobs).await;
        });
    }

    // ── Scheduler task (heartbeat + cron via mpsc) ──────────────────────────

    {
        let heartbeat_config = if scheduler_config.heartbeat.enabled {
            Some(scheduler_config.heartbeat.clone())
        } else {
            None
        };
        let cron_jobs = if scheduler_config.cron.enabled {
            let cron_dir = config.workspace_dir.join("cron");
            crate::agents::load_cron_jobs(&cron_dir)
        } else {
            vec![]
        };
        if heartbeat_config.is_some() || !cron_jobs.is_empty() {
            let scheduler = crate::agents::Scheduler::new(
                heartbeat_config,
                cron_jobs,
                config.agent.prompt.timezone_offset,
                scheduler_tx,
            );
            tokio::spawn(async move { scheduler.run().await; });
        }
    }

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── SIGUSR1: set shutdown flag for checkpoint exit (hot switch) ────────
    #[cfg(unix)]
    {
        use std::sync::atomic::Ordering;
        let mut sigusr1 = signal(SignalKind::user_defined1())
            .expect("failed to register SIGUSR1 handler");
        tokio::spawn(async move {
            sigusr1.recv().await;
            tracing::info!("SIGUSR1 received, setting shutdown flag");
            crate::SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
        });
    }

    // Wait for SIGINT or SIGTERM.
    tokio::spawn(async move {
        let _ = wait_for_signal().await;
        let _ = shutdown_tx.send(true);
        tracing::info!("shutdown signal received, initiating graceful shutdown...");
    });

    // ── SIGUSR2: new process ready, exit immediately ──────────────────────
    #[cfg(unix)]
    {
        let mut sigusr2 = signal(SignalKind::user_defined2())
            .expect("failed to register SIGUSR2 handler");
        tokio::spawn(async move {
            sigusr2.recv().await;
            tracing::info!("SIGUSR2 received, new process ready, exiting");
            std::process::exit(0);
        });
    }

    // Run the message dispatch loop (blocks until shutdown).
    orchestrator.run(shutdown_rx).await.context("orchestrator run error")?;

    // Graceful shutdown.
    tracing::info!("dispatch loop ended, shutting down listeners...");
    orchestrator.shutdown_listeners().await;

    tracing::info!("myclaw daemon stopped");

    // Clean up PID file
    let pid_file = std::env::temp_dir().join("myclaw.pid");
    if pid_file.exists() {
        let _ = std::fs::remove_file(&pid_file);
        tracing::debug!("PID file removed");
    }

    Ok(())
}

/// Wait for SIGINT, SIGTERM, or SIGUSR1.
async fn wait_for_signal() -> Result<()> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    tokio::select! {
        _ = sigint.recv() => {
            tracing::debug!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::debug!("received SIGTERM");
        }
        _ = sigusr1.recv() => {
            tracing::info!("received SIGUSR1 — hot switch triggered by `myclaw update`");
        }
    }
    Ok(())
}

// ── Sub-agent recovery (hot-switch detection) ─────────────────────────────────

/// Info about a sub-agent that was running when the daemon was killed.
#[derive(Debug, Clone)]
pub struct UnfinishedSubAgent {
    pub agent_name: String,
    pub task_id: String,
    pub task_preview: String,
}

/// Scan the sessions directory for `subagent_running_*.json` marker files left
/// behind by a previous daemon process that was killed while sub-agents were
/// still executing.
fn scan_unfinished_subagents(sessions_root: &std::path::Path) -> Vec<UnfinishedSubAgent> {
    let mut unfinished = Vec::new();
    let entries = match std::fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(_) => return unfinished,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("subagent_running_") && name.ends_with(".json") {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                    unfinished.push(UnfinishedSubAgent {
                        agent_name: state["agent_name"].as_str().unwrap_or("unknown").to_string(),
                        task_id: state["task_id"].as_str().unwrap_or("unknown").to_string(),
                        task_preview: state["task_preview"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }
    }
    unfinished
}

/// Remove all stale `subagent_running_*.json` marker files.  Called once after
/// the orchestrator has been informed about unfinished sub-agents so the markers
/// don't linger across future restarts.
fn cleanup_stale_subagent_markers(sessions_root: &std::path::Path) {
    let entries = match std::fs::read_dir(sessions_root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("subagent_running_") && name.ends_with(".json") {
            let _ = std::fs::remove_file(entry.path());
            tracing::info!(file = %name, "cleaned up stale sub-agent marker");
        }
    }
}
