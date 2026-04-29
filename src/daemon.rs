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
    SkillsManager, SystemPromptConfig, AutonomyLevel, SkillsPromptInjectionMode,
    McpManager, SubAgentDelegator,
};
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

    for (provider_key, provider_cfg) in &config.providers.clone() {
        let api_key = provider_cfg
            .api_key
            .as_ref()
            .with_context(|| format!("no API key for '{}'", provider_key))?;

        for (model_id, model_cfg) in &provider_cfg.models {
            tracing::info!(
                provider = %provider_key,
                model = %model_id,
                capabilities = ?model_cfg.capabilities,
                "registering provider for model"
            );

            let user_agent = provider_cfg.chat.user_agent.as_deref();

            let handle = crate::providers::ProviderHandle::from_url_with_user_agent(
                api_key.clone(),
                &provider_cfg.base_url,
                user_agent,
            ).with_context(|| format!(
                "cannot determine provider type from base_url '{}' (key='{}')",
                provider_cfg.base_url, provider_key
            ))?;

            use crate::providers::Capability;

            for cap in &model_cfg.capabilities {
                match cap {
                    Capability::Chat | Capability::Vision | Capability::NativeTools => {}
                    Capability::Embedding => {
                        if let Some(emb) = crate::providers::ProviderHandle::from_url_with_user_agent(
                            api_key.clone(), &provider_cfg.base_url, user_agent,
                        ).and_then(|h| h.into_embedding_provider()) {
                            registry.register_embedding(emb, model_id.clone());
                        }
                    }
                    Capability::ImageGeneration => {
                        if let Some(img) = crate::providers::ProviderHandle::from_url_with_user_agent(
                            api_key.clone(), &provider_cfg.base_url, user_agent,
                        ).and_then(|h| h.into_image_provider()) {
                            registry.register_image(img, model_id.clone());
                        }
                    }
                    Capability::TextToSpeech => {
                        if let Some(tts) = crate::providers::ProviderHandle::from_url_with_user_agent(
                            api_key.clone(), &provider_cfg.base_url, user_agent,
                        ).and_then(|h| h.into_tts_provider()) {
                            registry.register_tts(tts, model_id.clone());
                        }
                    }
                    Capability::VideoGeneration => {
                        if let Some(vid) = crate::providers::ProviderHandle::from_url_with_user_agent(
                            api_key.clone(), &provider_cfg.base_url, user_agent,
                        ).and_then(|h| h.into_video_provider()) {
                            registry.register_video(vid, model_id.clone());
                        }
                    }
                    Capability::Search => {
                        if let Some(srch) = crate::providers::ProviderHandle::from_url_with_user_agent(
                            api_key.clone(), &provider_cfg.base_url, user_agent,
                        ).and_then(|h| h.into_search_provider()) {
                            registry.register_search(srch, model_id.clone());
                        }
                    }
                    Capability::SpeechToText => {
                        if let Some(stt) = crate::providers::ProviderHandle::from_url_with_user_agent(
                            api_key.clone(), &provider_cfg.base_url, user_agent,
                        ).and_then(|h| h.into_stt_provider()) {
                            registry.register_stt(stt, model_id.clone());
                        }
                    }
                }
            }

            let chat_provider: Box<dyn crate::providers::ChatProvider> = handle.into_chat_provider();
            registry.register_chat(chat_provider, model_id.clone());
        }
    }

    // --- Wrap with FallbackChatProvider if strategy is Fallback ---
    // This must happen after all providers are registered above.
    registry.maybe_wrap_chat_fallback(&config.routing);

    Ok(registry)
}

/// Build SkillsManager with all built-in tools registered.
/// Optionally registers the `delegate_task` tool when sub-agents are configured.
async fn build_skills(
    mcp_manager: &McpManager,
    sub_agent_delegator: Option<Arc<SubAgentDelegator>>,
) -> SkillsManager {
    let mut skills = SkillsManager::new();
    let builtin = crate::tools::builtin_tools_with_memory(crate::tools::MemoryStore::new());
    for tool in builtin {
        let name = tool.name().to_string();
        skills.register_tool(&name, tool);
    }

    // Inject MCP tools (if any servers are configured and connected).
    if mcp_manager.is_connected().await {
        let mcp_tools = mcp_manager.tools().await;
        let count = mcp_tools.len();
        for tool in mcp_tools {
            let name = tool.name().to_string();
            skills.register_tool(&name, tool);
        }
        tracing::info!(mcp_tools = count, "MCP tools registered in SkillsManager");
    } else {
        tracing::debug!("MCP manager not connected, skipping MCP tool injection");
    }

    // Inject delegate_task tool when sub-agents are configured.
    if let Some(delegator) = sub_agent_delegator {
        let delegate_tool = crate::tools::DelegateTaskTool::new(delegator);
        skills.register_tool("delegate_task", Arc::new(delegate_tool));
        tracing::info!("delegate_task tool registered (multi-agent mode)");
    }

    tracing::info!(tool_count = skills.tool_count(), "skills manager built");
    skills
}

/// Build SessionManager with SQLite backend (falls back to in-memory).
fn build_session_manager(config: &crate::config::AppConfig) -> SessionManager {
    let db_path = config.workspace_dir.join("sessions.db");
    let backend: Arc<dyn crate::storage::SessionBackend> =
        match crate::storage::SqliteSessionBackend::open(&db_path.to_string_lossy()) {
            Ok(db) => {
                tracing::info!(path = %db_path.display(), "session database opened");
                Arc::new(db)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to open session database, falling back to in-memory");
                Arc::new(InMemoryBackend::new())
            }
        };
    SessionManager::new(backend)
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
fn build_prompt_config(cfg: &crate::config::agent::PromptConfig) -> SystemPromptConfig {
    SystemPromptConfig {
        workspace_dir: String::new(),
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

    // Build skills first (without delegate_task tool — we'll add it after if needed).
    let skills = build_skills(&mcp_manager, None).await;
    let registry_arc: Arc<dyn crate::providers::ServiceRegistry> = Arc::new(registry);
    let mut skills_arc: Arc<SkillsManager> = Arc::new(skills);

    // Build sub-agent delegator if sub-agents are configured.
    let sub_agent_delegator: Option<Arc<SubAgentDelegator>> = if config.agents.is_empty() {
        None
    } else {
        tracing::info!(agents = config.agents.len(), "multi-agent mode enabled");
        let delegator = SubAgentDelegator::new(
            config.agents.clone(),
            registry_arc.clone(),
            Arc::clone(&skills_arc),
            config.agent.max_tool_calls,
        );
        // Register delegate_task tool in the router agent's skills.
        let delegate_tool = crate::tools::DelegateTaskTool::new(Arc::new(delegator.clone()));
        if let Some(skills_mut) = Arc::get_mut(&mut skills_arc) {
            skills_mut.register_tool("delegate_task", Arc::new(delegate_tool));
        } else {
            tracing::warn!("could not register delegate_task tool — Arc already shared");
        }
        tracing::info!("delegate_task tool registered (multi-agent mode)");
        Some(Arc::new(delegator))
    };

    let session_manager = build_session_manager(&config);
    let channels = build_channels(&config);

    let agent_config = AgentConfig {
        max_tool_calls: config.agent.max_tool_calls,
        max_history: config.agent.max_history,
        prompt_config: build_prompt_config(&config.agent.prompt),
    };
    let agent = Agent::new(registry_arc, skills_arc, agent_config);

    let parts = OrchestratorParts {
        agent,
        session_manager,
        channels,
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
