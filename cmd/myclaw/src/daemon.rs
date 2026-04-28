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
use myclaw_runtime::{
    Agent, AgentConfig, InMemoryBackend, Orchestrator, OrchestratorParts, SessionManager,
    SkillsManager, SystemPromptConfig, AutonomyLevel, SkillsPromptInjectionMode,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use myclaw_channels::Channel;

/// Default config file locations.
const DEFAULT_CONFIG_PATHS: &[&str] = &[
    "myclaw.toml",
    "~/.myclaw/myclaw.toml",
    "/etc/myclaw/myclaw.toml",
];

/// Load configuration from the first found config file.
pub fn load_config() -> Result<myclaw_config::AppConfig> {
    for path in DEFAULT_CONFIG_PATHS {
        let expanded = shellexpand::tilde(path).to_string();
        let p = PathBuf::from(expanded);
        if p.exists() {
            tracing::info!(path = %p.display(), "loading config");
            return myclaw_config::ConfigLoader::from_file(&p)
                .context("failed to load config");
        }
    }
    anyhow::bail!(
        "No config file found. Looked in: {}",
        DEFAULT_CONFIG_PATHS.join(", ")
    );
}

/// Load configuration from a specific path.
pub fn load_config_from(path: &str) -> Result<myclaw_config::AppConfig> {
    let expanded = shellexpand::tilde(path).to_string();
    let p = PathBuf::from(expanded.clone());
    if !p.exists() {
        anyhow::bail!("Config file not found: {}", expanded);
    }
    tracing::info!(path = %p.display(), "loading config");
    myclaw_config::ConfigLoader::from_file(&p).context("failed to load config")
}

/// Initialize tracing subscriber based on config.
pub fn init_tracing(config: &myclaw_config::AppConfig) {
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
fn print_banner(config: &myclaw_config::AppConfig) {
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
        .get(myclaw_config::provider::Capability::Chat)
        .map(|e| e.models.join(" → "))
    {
        println!("  🗺️  Chat route: {}", chat_route);
    }

    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();
}

// ═══════════════════════════════════════════════════════════════════════════════
// Composition Root — assemble all components
// ═══════════════════════════════════════════════════════════════════════════════

/// Build the Registry and register all providers from config.
fn build_registry(config: &myclaw_config::AppConfig) -> anyhow::Result<myclaw_registry::Registry> {
    let mut registry =
        myclaw_registry::Registry::from_config(config.providers.clone(), &config.routing)
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

            let handle = myclaw_providers::ProviderHandle::from_url(
                api_key.clone(),
                &provider_cfg.base_url,
            ).with_context(|| format!(
                "cannot determine provider type from base_url '{}' (key='{}')",
                provider_cfg.base_url, provider_key
            ))?;

            use myclaw_config::provider::Capability as CfgCap;

            for cap in &model_cfg.capabilities {
                match cap {
                    CfgCap::Chat | CfgCap::Vision | CfgCap::NativeTools => {}
                    CfgCap::Embedding => {
                        if let Some(emb) = myclaw_providers::ProviderHandle::from_url(
                            api_key.clone(), &provider_cfg.base_url,
                        ).and_then(|h| h.into_embedding_provider()) {
                            registry.register_embedding(emb, model_id.clone());
                        }
                    }
                    CfgCap::ImageGeneration => {
                        if let Some(img) = myclaw_providers::ProviderHandle::from_url(
                            api_key.clone(), &provider_cfg.base_url,
                        ).and_then(|h| h.into_image_provider()) {
                            registry.register_image(img, model_id.clone());
                        }
                    }
                    CfgCap::TextToSpeech => {
                        if let Some(tts) = myclaw_providers::ProviderHandle::from_url(
                            api_key.clone(), &provider_cfg.base_url,
                        ).and_then(|h| h.into_tts_provider()) {
                            registry.register_tts(tts, model_id.clone());
                        }
                    }
                    CfgCap::VideoGeneration => {
                        if let Some(vid) = myclaw_providers::ProviderHandle::from_url(
                            api_key.clone(), &provider_cfg.base_url,
                        ).and_then(|h| h.into_video_provider()) {
                            registry.register_video(vid, model_id.clone());
                        }
                    }
                    CfgCap::Search => {
                        if let Some(srch) = myclaw_providers::ProviderHandle::from_url(
                            api_key.clone(), &provider_cfg.base_url,
                        ).and_then(|h| h.into_search_provider()) {
                            registry.register_search(srch, model_id.clone());
                        }
                    }
                    CfgCap::SpeechToText => {
                        if let Some(stt) = myclaw_providers::ProviderHandle::from_url(
                            api_key.clone(), &provider_cfg.base_url,
                        ).and_then(|h| h.into_stt_provider()) {
                            registry.register_stt(stt, model_id.clone());
                        }
                    }
                }
            }

            let chat_provider: Box<dyn myclaw_providers::ChatProvider> = handle.into_chat_provider();
            registry.register_chat(chat_provider, model_id.clone());
        }
    }

    // --- Wrap with FallbackChatProvider if strategy is Fallback ---
    // This must happen after all providers are registered above.
    registry.maybe_wrap_chat_fallback(&config.routing);

    Ok(registry)
}

/// Build SkillsManager with all built-in tools registered.
fn build_skills() -> SkillsManager {
    let mut skills = SkillsManager::new();
    let builtin = myclaw_tools::builtin_tools_with_memory(myclaw_tools::MemoryStore::new());
    for tool in builtin {
        let name = tool.name().to_string();
        skills.register_tool(&name, tool);
    }
    tracing::info!(tool_count = skills.tool_count(), "builtin tools registered");
    skills
}

/// Build SessionManager with SQLite backend (falls back to in-memory).
fn build_session_manager(config: &myclaw_config::AppConfig) -> SessionManager {
    let db_path = config.workspace_dir.join("sessions.db");
    let backend: Arc<dyn myclaw_session::SessionBackend> =
        match myclaw_memory_storage::SqliteSessionBackend::open(&db_path.to_string_lossy()) {
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
fn build_channels(config: &myclaw_config::AppConfig) -> Vec<(&'static str, Arc<dyn Channel>)> {
    let mut channels: Vec<(&'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref cfg) = config.channels.telegram {
        if cfg.enabled {
            channels.push((
                "telegram",
                Arc::new(myclaw_channels::telegram::TelegramChannel::new(cfg.clone())),
            ));
        }
    }

    if let Some(ref cfg) = config.channels.wechat {
        if cfg.enabled {
            channels.push((
                "wechat",
                Arc::new(myclaw_channels::wechat::WechatChannel::new(cfg.clone())),
            ));
        }
    }

    channels
}

/// Convert config prompt settings into Application-layer type.
fn build_prompt_config(cfg: &myclaw_config::agent::PromptConfig) -> SystemPromptConfig {
    SystemPromptConfig {
        workspace_dir: String::new(),
        model_name: cfg.model_name.clone().unwrap_or_default(),
        autonomy: match myclaw_config::agent::AutonomyLevel::default() {
            myclaw_config::agent::AutonomyLevel::Full => AutonomyLevel::Full,
            myclaw_config::agent::AutonomyLevel::Default => AutonomyLevel::Default,
            myclaw_config::agent::AutonomyLevel::ReadOnly => AutonomyLevel::ReadOnly,
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
pub async fn run(config: myclaw_config::AppConfig) -> Result<()> {
    // ── Composition Root: assemble all components ──────────────────────────

    let registry = build_registry(&config)?;
    let skills = build_skills();
    let session_manager = build_session_manager(&config);
    let channels = build_channels(&config);

    let agent_config = AgentConfig {
        max_tool_calls: config.agent.max_tool_calls,
        max_history: config.agent.max_history,
        prompt_config: build_prompt_config(&config.agent.prompt),
    };
    let agent = Agent::new(Arc::new(registry), skills, agent_config);

    let parts = OrchestratorParts {
        agent,
        session_manager,
        channels,
    };

    // ── Launch ─────────────────────────────────────────────────────────────

    let (mut orchestrator, _msg_tx) = Orchestrator::new(parts);
    print_banner(&config);

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
