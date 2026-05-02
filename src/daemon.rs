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
fn print_banner(config: &crate::config::AppConfig, mcp_servers: usize, mcp_tools: usize, sub_agents: usize) {
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

    if sub_agents > 0 {
        let names: Vec<&str> = config.agents.iter().map(|a| a.name.as_str()).collect();
        println!("  🤝 Sub-agents: {} ({})", sub_agents, names.join(", "));
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

/// Build ToolRegistry with all built-in + MCP tools registered.
async fn build_tools(mcp_manager: &McpManager) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    let builtin = crate::tools::builtin_tools_with_memory(crate::tools::MemoryStore::new());
    for tool in builtin {
        tools.register(tool);
    }

    // Register additional built-in tools.
    tools.register(Arc::new(crate::tools::ListDirTool::new()));
    tools.register(Arc::new(crate::tools::TaskManagerTool::new(
        crate::tools::TaskManagerTool::shared_state(),
    )));

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

/// Build the session backend (shared with SessionManager and persist hooks).
fn build_session_backend(config: &crate::config::AppConfig) -> Arc<dyn crate::storage::SessionBackend> {
    let db_path = config.workspace_dir.join("sessions.db");
    match crate::storage::SqliteSessionBackend::open(&db_path.to_string_lossy()) {
        Ok(db) => {
            tracing::info!(path = %db_path.display(), "session database opened");
            Arc::new(db)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to open session database, falling back to in-memory");
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

    channels
}

/// Convert config prompt settings into Application-layer type.
fn build_prompt_config(cfg: &crate::config::agent::PromptConfig, workspace_dir: &std::path::Path) -> SystemPromptConfig {
    SystemPromptConfig {
        workspace_dir: workspace_dir.to_string_lossy().to_string(),
        model_name: cfg.model_name.clone().unwrap_or_default(),
        autonomy: match crate::config::agent::AutonomyLevel::default() {
            crate::config::agent::AutonomyLevel::Full => AutonomyLevel::Full,
            crate::config::agent::AutonomyLevel::Default => AutonomyLevel::Default,
            crate::config::agent::AutonomyLevel::ReadOnly => AutonomyLevel::ReadOnly,
        },
        skills_mode: SkillsPromptInjectionMode::Compact,
        compact: cfg.compact,
        max_chars: cfg.max_chars,
        bootstrap_max_chars: cfg.bootstrap_max_chars,
        native_tools: cfg.native_tools,
        channel_name: cfg.channel_name.clone(),
        host_info: None,
    }
}

/// Run the MyClaw daemon, blocking until shutdown.
pub async fn run(config: crate::config::AppConfig) -> Result<()> {
    // ── Composition Root: assemble all components ──────────────────────────

    let registry = build_registry(&config)?;

    let mcp_manager = McpManager::new();
    if let Err(e) = mcp_manager.connect(&config.mcp_servers).await {
        tracing::warn!(error = %e, "MCP server connection had errors (non-fatal), continuing");
    }

    // Build tool registry (all built-in + MCP tools).
    let tools = build_tools(&mcp_manager).await;

    // Build skill manager (SKILL.md files).
    let skills = build_skill_manager(&config.workspace_dir);

    let registry_arc: Arc<dyn crate::providers::ServiceRegistry> = Arc::new(registry);

    // ── Sub-agent delegator (conditional) ──────────────────────────────────────
    //
    // Dependency chain:
    //   delegate_task tool → needs Arc<dyn TaskDelegator>
    //   SubAgentDelegator  → needs Arc<ToolRegistry> + Arc<SkillManager>
    //   parent Agent       → needs Arc<ToolRegistry> + Arc<SkillManager>
    //
    // ToolRegistry is shared via Arc. delegate_task and tool_search are added
    // to the parent's registry only — sub-agents get a filtered view.

    let (tools_arc, skills_arc, sub_agent_delegator_arc) = if config.agents.is_empty() {
        // Single-agent mode: add tool_search to base registry.
        let base_tools_arc: Arc<ToolRegistry> = Arc::new(tools);
        let tool_search = crate::tools::ToolSearchTool::new(Arc::clone(&base_tools_arc));

        // Add tool_search to a new registry (Arc is immutable, so rebuild).
        let mut final_tools = ToolRegistry::new();
        for tool in base_tools_arc.all_tools() {
            final_tools.register(tool);
        }
        final_tools.register(Arc::new(tool_search));

        (Arc::new(final_tools), Arc::new(skills), None)
    } else {
        tracing::info!(agents = config.agents.len(), "multi-agent mode enabled");

        let base_tools_arc: Arc<ToolRegistry> = Arc::new(tools);
        let skills_arc: Arc<SkillManager> = Arc::new(skills);

        let delegator = SubAgentDelegator::new(
            config.agents.clone(),
            registry_arc.clone(),
            Arc::clone(&base_tools_arc),
            Arc::clone(&skills_arc),
            config.agent.max_tool_calls,
        );
        let delegator_arc = Arc::new(delegator);

        // Build delegate_task tool.
        let delegate_tool = crate::tools::DelegateTaskTool::new(
            Arc::clone(&delegator_arc) as Arc<dyn TaskDelegator>,
        );

        // Build parent tool registry: same tools + delegate_task + tool_search.
        let mut parent_tools = ToolRegistry::new();
        for tool in base_tools_arc.all_tools() {
            parent_tools.register(tool);
        }
        parent_tools.register(Arc::new(delegate_tool));
        tracing::info!("delegate_task tool registered (multi-agent mode)");

        let tool_search = crate::tools::ToolSearchTool::new(Arc::clone(&base_tools_arc));
        parent_tools.register(Arc::new(tool_search));

        (Arc::new(parent_tools), skills_arc, Some(delegator_arc))
    };

    // ── Delegation channel (conditional — only when sub-agents configured) ─────
    let (delegation_manager, delegation_rx) = if sub_agent_delegator_arc.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agents::DelegationEvent>(100);
        (Some(Arc::new(DelegationManager::new(tx))), Some(rx))
    } else {
        (None, None)
    };

    let session_backend = build_session_backend(&config);
    let session_manager = SessionManager::new(Arc::clone(&session_backend));
    let channels = build_channels(&config);

    let agent_config = AgentConfig {
        max_tool_calls: config.agent.max_tool_calls,
        max_history: config.agent.max_history,
        prompt_config: build_prompt_config(&config.agent.prompt, &config.workspace_dir),
        context: config.agent.context.clone(),
        stream_chunk_timeout_secs: config.agent.stream_chunk_timeout_secs,
        max_output_bytes: calculate_max_output_bytes(&config, &registry_arc),
    };
    let agent = Agent::new(registry_arc, tools_arc, skills_arc, agent_config);

    let parts = OrchestratorParts {
        agent,
        session_manager,
        channels,
        sub_delegator: sub_agent_delegator_arc,
        delegation_manager,
        delegation_rx,
        persist_backend: session_backend,
    };

    // ── Launch ─────────────────────────────────────────────────────────────

    let (mut orchestrator, _msg_tx) = Orchestrator::new(parts);
    print_banner(&config, mcp_manager.server_count().await, mcp_manager.tool_count().await, config.agents.len());

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Wait for SIGINT or SIGTERM.
    tokio::spawn(async move {
        let _ = wait_for_signal().await;
        let _ = shutdown_tx.send(true);
        tracing::info!("shutdown signal received, initiating graceful shutdown...");
    });

    // Run the message dispatch loop (blocks until shutdown).
    orchestrator.run(shutdown_rx).await.context("orchestrator run error")?;

    // Graceful shutdown.
    tracing::info!("dispatch loop ended, shutting down listeners...");
    orchestrator.shutdown_listeners().await;

    tracing::info!("myclaw daemon stopped");
    Ok(())
}

/// Wait for SIGINT or SIGTERM.
async fn wait_for_signal() -> Result<()> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {
            tracing::debug!("received SIGINT");
        }
        _ = sigterm.recv() => {
            tracing::debug!("received SIGTERM");
        }
    }
    Ok(())
}
