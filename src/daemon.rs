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
    InMemoryBackend, Orchestrator, OrchestratorParts, SessionManager,
    ToolRegistry, SkillManager, Skill, SystemPromptConfig, RunMode,
    McpManager, DelegationCoordinator, DelegationManager,
};
use crate::agents::AgentDelegator;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use crate::channels::Channel;

/// File descriptor of the SO_REUSEPORT webhook listen socket, stored so the
/// hot-switch child can inherit it.  `-1` means no socket has been bound yet.
pub static LISTEN_SOCKET_FD: AtomicI32 = AtomicI32::new(-1);

/// File descriptor of the SO_REUSEPORT client WebSocket socket.
/// Inherited by the hot-switch child to avoid EADDRINUSE during overlap.
pub static CLIENT_SOCKET_FD: AtomicI32 = AtomicI32::new(-1);

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

/// Bind a TCP listener with `SO_REUSEPORT` + `SO_REUSE_ADDRESS`.
///
/// This allows a new process to bind the same port **before** the old process
/// has released it — essential for zero-downtime hot switch.
fn bind_reusable(port: u16) -> anyhow::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address: {e}"))?;

    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
        .context("failed to create socket")?;
    socket.set_reuse_port(true).context("SO_REUSEPORT failed")?;
    socket.set_reuse_address(true).context("SO_REUSEADDR failed")?;
    // socket2 v0.5 adds SOCK_CLOEXEC by default.  Clear it so that the fd
    // survives fork+execve during hot switch.  Without this the child gets
    // EBADF (fd closed by execve) or EPERM (fd number reused as a
    // non-pollable file type before epoll_ctl(EPOLL_CTL_ADD) is called).
    #[cfg(unix)]
    socket.set_cloexec(false).context("clearing FD_CLOEXEC failed")?;
    socket
        .bind(&addr.into())
        .with_context(|| format!("failed to bind {addr}"))?;
    socket.listen(128).context("listen failed")?;

    let listener: std::net::TcpListener = socket.into();
    tracing::info!(port, "SO_REUSEPORT listener bound");
    Ok(listener)
}

/// Initialize tracing subscriber based on config.
pub fn init_tracing(config: &crate::config::AppConfig) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let level = config.logging.level.as_deref().unwrap_or("info");
    // Build RUST_LOG-style directives: global level + per-module overrides.
    let mut parts = vec![level.to_string()];
    for (module, mod_level) in &config.logging.modules {
        parts.push(format!("{}={}", module, mod_level));
    }
    let directives = parts.join(",");

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(directives));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_thread_ids(true));

    tracing::subscriber::set_global_default(subscriber)
        .expect("failed to set tracing subscriber");
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
    use crate::providers::{ProviderId, detect_from_url};
    use crate::providers::{
        ProviderFactory,
        BuildChatProviderRequest, BuildEmbeddingProviderRequest,
        BuildImageProviderRequest, BuildTtsProviderRequest,
        BuildSearchProviderRequest, BuildVideoProviderRequest, BuildSttProviderRequest,
    };

    let factory = ProviderFactory::new();
    let mut registry =
        crate::registry::Registry::from_config(config.providers.clone(), &config.routing)
            .context("failed to build registry")?;

    for (provider_key, provider_cfg) in &config.providers {
        // Resolve provider identity: explicit override > base_url inference > generic
        let provider_id = provider_cfg.provider.as_ref()
            .map(|s| ProviderId::new(s.clone()))
            .or_else(|| {
                // Try to infer from the first capability's base_url
                provider_cfg.chat.as_ref()
                    .and_then(|c| detect_from_url(&c.base_url))
                    .or_else(|| provider_cfg.embedding.as_ref().and_then(|e| detect_from_url(&e.base_url)))
                    .or_else(|| provider_cfg.search.as_ref().and_then(|s| detect_from_url(&s.base_url)))
            })
            .unwrap_or_else(|| ProviderId::new("generic"));

        tracing::debug!(provider = %provider_key, id = %provider_id, "resolved provider identity");

        // ── Chat ──────────────────────────────────────────────────────
        if let Some(ref chat) = provider_cfg.chat {
            let api_key = provider_cfg.effective_api_key(chat.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}'", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(chat.auth_style);
            let user_agent = chat.user_agent.clone();

            for (model_id, model_cfg) in &chat.models {
                tracing::debug!(
                    provider = %provider_key,
                    model = %model_id,
                    capability = "chat",
                    "registering chat provider"
                );

                let request = BuildChatProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    protocol: chat.protocol,
                    base_url: chat.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                let chat_provider = factory.build_chat_provider(request)
                    .with_context(|| format!(
                        "cannot build chat provider for base_url '{}' (key='{}')",
                        chat.base_url, provider_key
                    ))?;

                registry.register_chat(chat_provider, model_id.clone(), model_cfg.clone());
            }
        }

        // ── Embedding ─────────────────────────────────────────────────
        if let Some(ref emb) = provider_cfg.embedding {
            let api_key = provider_cfg.effective_api_key(emb.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' embedding", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(emb.auth_style);
            let user_agent = emb.user_agent.clone();

            for model_id in emb.models.keys() {
                tracing::debug!(
                    provider = %provider_key,
                    model = %model_id,
                    capability = "embedding",
                    "registering embedding provider"
                );

                let request = BuildEmbeddingProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: emb.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(emb_provider) = factory.build_embedding_provider(request) {
                    registry.register_embedding(emb_provider, model_id.clone());
                }
            }
        }

        // ── ImageGeneration ───────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.image_generation {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' image_generation", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(sec.auth_style);
            let user_agent = sec.user_agent.clone();

            for model_id in sec.models.keys() {
                let request = BuildImageProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: sec.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(img) = factory.build_image_provider(request) {
                    registry.register_image(img, model_id.clone());
                }
            }
        }

        // ── TTS ───────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.tts {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' tts", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(sec.auth_style);
            let user_agent = sec.user_agent.clone();

            for model_id in sec.models.keys() {
                let request = BuildTtsProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: sec.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(tts) = factory.build_tts_provider(request) {
                    registry.register_tts(tts, model_id.clone());
                }
            }
        }

        // ── Video ─────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.video {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' video", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(sec.auth_style);
            let user_agent = sec.user_agent.clone();

            for model_id in sec.models.keys() {
                let request = BuildVideoProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: sec.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(vid) = factory.build_video_provider(request) {
                    registry.register_video(vid, model_id.clone());
                }
            }
        }

        // ── Search ────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.search {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' search", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(sec.auth_style);
            let user_agent = sec.user_agent.clone();

            for model_id in sec.models.keys() {
                let request = BuildSearchProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: sec.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(srch) = factory.build_search_provider(request) {
                    registry.register_search(srch, model_id.clone());
                }
            }
        }

        // ── STT ───────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.stt {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = api_key
                .with_context(|| format!("no API key for '{}' stt", provider_key))?;
            let auth_style = provider_cfg.effective_auth_style(sec.auth_style);
            let user_agent = sec.user_agent.clone();

            for model_id in sec.models.keys() {
                let request = BuildSttProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: sec.base_url.clone(),
                    api_key: api_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(stt) = factory.build_stt_provider(request) {
                    registry.register_stt(stt, model_id.clone());
                }
            }
        }
    }

    // --- Wrap with FallbackChatProvider if strategy is Fallback ---
    registry.maybe_wrap_chat_fallback(&config.routing);

    Ok(registry)
}

/// Build ToolRegistry with all built-in + MCP + skill tools registered.
async fn build_tools(
    mcp_manager: &McpManager,
    skills: &Arc<parking_lot::RwLock<SkillManager>>,
    shared_scheduler: &crate::agents::SharedScheduler,
    workspace_dir: &std::path::Path,
    _knowledge_dir: &str,
    user_resolver: &Arc<crate::agents::UserResolver>,
) -> ToolRegistry {
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

    // SkillsListTool — lists skill metadata.
    tools.register(Arc::new(crate::tools::SkillsListTool::new(Arc::clone(skills))));

    // SkillManageTool — CRUD for skills.
    tools.register(Arc::new(crate::tools::SkillManageTool::new(
        Arc::clone(skills),
        workspace_dir.to_path_buf(),
    )));

    // CronJobTool — manage scheduled cron jobs.
    tools.register(Arc::new(crate::tools::CronJobTool::new(Arc::clone(shared_scheduler))));

    // Memory tools — persistent per-user memory (G43: workspace/users/{uid}/memory/).
    let wd = workspace_dir.to_path_buf();
    let r = Arc::clone(user_resolver);
    tools.register(Arc::new(crate::tools::MemoryListTool::new(wd.clone(), Arc::clone(&r))));
    tools.register(Arc::new(crate::tools::MemoryViewTool::new(wd.clone(), Arc::clone(&r))));
    tools.register(Arc::new(crate::tools::MemorySearchTool::new(wd.clone(), Arc::clone(&r))));
    tools.register(Arc::new(crate::tools::MemoryManageTool::new(wd, r)));

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
        tracing::debug!(name = %def.name, "skill registered");
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
            tracing::warn!(err = %e, "failed to open session storage, falling back to in-memory");
            Arc::new(InMemoryBackend::new())
        }
    }
}

/// Build Channel adapters from config, returning (channel_type, account_id, channel).
fn build_channel_accounts(config: &crate::config::AppConfig) -> Vec<(String, String, Arc<dyn Channel>)> {
    let mut channels: Vec<(String, String, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref cfg) = config.channels.telegram {
        if cfg.enabled {
            for (account_id, account_cfg) in &cfg.accounts {
                if account_cfg.enabled {
                    channels.push((
                        "telegram".to_string(),
                        account_id.clone(),
                        Arc::new(crate::channels::telegram::TelegramChannel::new(account_cfg.clone())),
                    ));
                }
            }
        }
    }

    #[cfg(feature = "wechat")]
    if let Some(ref cfg) = config.channels.wechat {
        if cfg.enabled {
            for (account_id, account_cfg) in &cfg.accounts {
                if account_cfg.enabled {
                    channels.push((
                        "wechat".to_string(),
                        account_id.clone(),
                        Arc::new(crate::channels::wechat::WechatChannel::new(account_cfg.clone())),
                    ));
                }
            }
        }
    }

    #[cfg(feature = "qqbot")]
    if let Some(ref cfg) = config.channels.qqbot {
        if cfg.enabled {
            for (account_id, account_cfg) in &cfg.accounts {
                if account_cfg.enabled {
                    channels.push((
                        "qqbot".to_string(),
                        account_id.clone(),
                        Arc::new(crate::channels::qqbot::QQBotChannel::new(account_cfg.clone())),
                    ));
                }
            }
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
        permission_mode: cfg.permission_mode,
        run_mode: RunMode::Interactive,
        max_chars: cfg.prompt.max_chars,
        native_tools: cfg.prompt.native_tools,
        identity_header: None,
    }
}

/// Run the MyClaw daemon, blocking until shutdown.
pub async fn run(config: crate::config::AppConfig) -> Result<()> {
    // 让进程 cwd 与 workspace_dir 一致，保证 file_read 等工具的相对路径解析
    // 和 system prompt 告诉 LLM 的 "Working directory" 一致
    std::env::set_current_dir(&config.workspace_dir).with_context(|| {
        format!("failed to set cwd to workspace_dir '{}'", config.workspace_dir.display())
    })?;

    // D27 — RFC v2 §三.A: workspace/agents/main/AGENT.md is required.
    // Without it there is no default agent to route inbound messages to.
    // Run scripts/migrate_main_agent.sh on first boot after upgrade.
    let main_agent_md = config.workspace_dir.join("agents").join("main").join("AGENT.md");
    if !main_agent_md.exists() {
        anyhow::bail!(
            "missing main agent at {}\n\
             RFC v2 requires every workspace to define a 'main' agent.\n\
             Run scripts/migrate_main_agent.sh to fold IDENTITY.md/SOUL.md into it,\n\
             or create the file manually.",
            main_agent_md.display()
        );
    }

    // ── Hot switch: enhanced startup for fork+execv child ─────────────────
    // When the new binary is started via execv (hot switch), it inherits the
    // listen socket fd and needs to: (1) take over the socket, (2) clear the
    // Telegram update offset so the new process fetches fresh updates, (3) drain
    // any queued messages that arrived during the switch, and (4) notify the
    // old process that it can exit.
    #[cfg(unix)]
    if crate::hot_switch::is_hot_switch() {
        tracing::info!("hot switch mode detected — initializing new process takeover");

        // ── Socket takeover ────────────────────────────────────────────────
        // The old process stored its listen socket fd in MYCLAW_SOCKET_FD before
        // calling execv.  Store it in LISTEN_SOCKET_FD so the webhook bind code
        // below can reuse it instead of calling bind_reusable().
        if let Some(fd) = crate::hot_switch::inherited_socket_fd() {
            tracing::info!(fd, "inherited listen socket from old process");
            LISTEN_SOCKET_FD.store(fd, Ordering::SeqCst);
        } else {
            tracing::warn!("hot switch detected but MYCLAW_SOCKET_FD not set");
        }

        if let Some(fd) = crate::hot_switch::inherited_client_socket_fd() {
            tracing::info!(fd, "inherited client WebSocket socket from old process");
            CLIENT_SOCKET_FD.store(fd, Ordering::SeqCst);
        }

        // ── Telegram offset reset ─────────────────────────────────────────
        // The old process may have persisted an update offset that covers
        // messages it never finished processing.  Clear the offset file so
        // getUpdates returns recent messages.  The Telegram channel's dedup
        // layer will filter out any duplicates.
        reset_telegram_offset();

        // ── Queue processing ──────────────────────────────────────────────
        // Queue drain is handled later (after session backend initialization)
        // in the dedicated queue processing section.  We skip it here because
        // process_all_queues deletes the queue files, and the later call needs
        // to read them using the proper session backend.

        // ── Notify old process ────────────────────────────────────────────
        // SIGUSR2 is sent after full initialization (just before orchestrator.run())
        // so the old process doesn't exit before we are truly ready to serve.
    }

    // Write PID file for hot-switch coordination (used by `myclaw update`).
    let pid_file = crate::signal::pid_file_path();
    if let Err(e) = std::fs::write(&pid_file, std::process::id().to_string()) {
        tracing::warn!(err = %e, "failed to write PID file");
    } else {
        tracing::debug!(pid = %std::process::id(), path = %pid_file.display(), "PID file written");
    }

    // Ensure knowledge directory exists
    if let Err(e) = crate::memory::ensure_memory_dir(
        config.knowledge_dir.to_str().unwrap_or("."),
    ) {
        tracing::warn!(err = %e, "failed to create knowledge directory");
    }

    // ── Composition Root: assemble all components ──────────────────────────

    let registry = build_registry(&config)?;

    let mcp_manager = McpManager::new();
    if let Err(e) = mcp_manager.connect(&config.mcp_servers).await {
        tracing::warn!(err = %e, "MCP server connection had errors");
    }

    // Build skill manager (SKILL.md files).
    let skills = build_skill_manager(&config.workspace_dir);
    let skills_arc: Arc<parking_lot::RwLock<SkillManager>> = Arc::new(parking_lot::RwLock::new(skills));

    // Resolve timezone: config.timezone (IANA) takes precedence over timezone_offset.
    let tz_name = config.agent.prompt.timezone.clone().unwrap_or_else(|| {
        // Convert legacy offset to Etc/GMT name (signs are inverted in Etc/GMT).
        let offset = config.agent.prompt.timezone_offset;
        if offset == 0 { "UTC".to_string() }
        else { format!("Etc/GMT{}", if offset > 0 { format!("-{}", offset) } else { format!("{}", -offset) }) }
    });

    // Create scheduler channel early (needed for SharedScheduler creation).
    let (scheduler_tx, scheduler_rx) = tokio::sync::mpsc::channel::<crate::agents::SchedulerEvent>(100);

    // Build shared scheduler (owns all cron job data).
    let cron_dir = config.workspace_dir.join("cron");
    let jobs_json_path = cron_dir.join("jobs.json");

    // Migrate from old markdown files if jobs.json doesn't exist yet.
    if !jobs_json_path.exists() {
        let (dummy_tx, _) = tokio::sync::mpsc::channel(1);
        let migrator = crate::agents::scheduling::scheduler::Scheduler::new(
            jobs_json_path.clone(),
            tz_name.clone(),
            None,
            dummy_tx,
            config.workspace_dir.join(".last_channel"),
            config.workspace_dir.join(".last_recipient"),
        );
        let count = migrator.migrate_from_markdown(&cron_dir);
        if count > 0 {
            tracing::debug!(count = count, "migrated cron jobs from markdown to JSON");
        }
    }

    // Read heartbeat config early for the scheduler.
    let heartbeat_config = if config.agent.scheduler.heartbeat.enabled {
        Some(config.agent.scheduler.heartbeat.clone())
    } else {
        None
    };

    let shared_scheduler = crate::agents::scheduling::scheduler::Scheduler::new(
        jobs_json_path.clone(),
        tz_name.clone(),
        heartbeat_config,
        scheduler_tx.clone(),
        config.workspace_dir.join(".last_channel"),
        config.workspace_dir.join(".last_recipient"),
    );

    // G39: shared user resolver — defaults to identity (user_id == routing_key).
    let user_resolver = Arc::new(crate::agents::UserResolver::new());

    // Build tool registry (all built-in + MCP + skill tools).
    let mut tools = build_tools(&mcp_manager, &skills_arc, &shared_scheduler, &config.workspace_dir, config.knowledge_dir.to_str().unwrap_or("."), &user_resolver).await;

    // Build sub-agent configs (AGENT.md files from workspace/agents/).
    let sub_agent_configs = build_sub_agents(&config.workspace_dir);
    let sub_agent_count = sub_agent_configs.len();
    let sub_agent_names: Vec<String> = sub_agent_configs.iter().map(|a| a.name.clone()).collect();
    let sub_agent_registry = Arc::new(crate::agents::AgentRegistry::from_vec(sub_agent_configs));

    let registry_arc: Arc<dyn crate::providers::ProviderRegistry> = Arc::new(registry);

    // Register WebSearchTool — requires ProviderRegistry for search routing.
    let search_cooldown = Arc::new(crate::tools::search_cooldown::SearchProviderCooldown::new());
    tools.register(Arc::new(crate::tools::WebSearchTool::new(
        registry_arc.clone(),
        Arc::clone(&search_cooldown),
    )));
    tracing::debug!("web_search tool registered (connected to ProviderRegistry)");

    // WorkspaceWatcher for hot-reload.
    let _watcher = crate::agents::WorkspaceWatcher::new(&config.workspace_dir, &config.knowledge_dir)?;
    // change_rx is no longer subscribed to — D25's WorkspaceWatcher::spawn_managed
    // handles agent/skill reloads autonomously (the watcher publishes the
    // ChangeSet but nothing now subscribes here).

    // Build session backend + manager early — B15: the DelegationCoordinator
    // needs SessionManager (shared backend) to create sub-sessions as
    // top-level peers instead of opening a per-parent JsonFileBackend at
    // `{sessions_root}/{parent}/subagents/`.
    let session_backend = build_session_backend(&config);
    let session_manager = Arc::new(
        SessionManager::new(Arc::clone(&session_backend))
            .with_agents(Arc::clone(&sub_agent_registry))
            .with_resolver(Arc::clone(&user_resolver)),
    );

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

        let delegator = DelegationCoordinator::new(
            sub_agent_registry.clone(),
            registry_arc.clone(),
            Arc::clone(&base_tools_arc),
            Arc::clone(&skills_arc),
            config.agent.max_tool_calls,
            Arc::clone(&session_manager),
            config.workspace_dir.join("worktrees"),
        );
        let delegator_arc = Arc::new(delegator);

        // Build agent_delegate tool. H47: now wired through `AgentDelegator`
        // (the legacy `TaskDelegator` trait has been removed).
        let delegate_tool = crate::tools::AgentDelegateTool::new(
            Arc::clone(&delegator_arc) as Arc<dyn AgentDelegator>,
        );

        // Build parent tool registry: same tools + agent_delegate + tool_search.
        let mut parent_tools = ToolRegistry::new();
        for tool in base_tools_arc.all_tools() {
            parent_tools.register(tool);
        }
        parent_tools.register(Arc::new(delegate_tool));
        tracing::debug!("agent_delegate tool registered (multi-agent mode)");

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

    // session_backend / session_manager are constructed earlier (above the
    // DelegationCoordinator block — see B15 note) so they can be shared.
    let mut channels = build_channel_accounts(&config);

    // ── Sub-agent recovery: detect interrupted sub-agents from a previous run ──
    // F37 + H50: scan SessionManager for sub-sessions with mid-turn history
    // instead of reading `subagent_running_*.json` marker files (the marker
    // mechanism has been deleted along with the corresponding writes in
    // DelegationCoordinator).
    let unfinished_subagents =
        crate::agents::recovery::scan_unfinished_subagents(&session_manager);
    if !unfinished_subagents.is_empty() {
        tracing::warn!(
            count = unfinished_subagents.len(),
            "detected unfinished sub-agents from previous run"
        );
        for sa in &unfinished_subagents {
            tracing::warn!(
                agent = %sa.agent_name,
                task_id = %sa.task_id,
                "unfinished sub-agent"
            );
        }
    }

    // ── Queue processing: drain any queued messages ────────────────────────
    // Messages may have been queued to queue.jsonl files during a hot switch
    // or if the process was killed mid-turn. Always scan on startup.
    let sessions_root = config.workspace_dir.join("sessions");
    match crate::agents::process_all_queues(&sessions_root) {
        Ok(queues) => {
            for (sid, msgs) in &queues {
                for msg in msgs {
                    session_manager.append_message(sid, msg.clone());
                }
            }
            if !queues.is_empty() {
                let total: usize = queues.values().map(|v| v.len()).sum();
                tracing::info!(
                    sessions = queues.len(),
                    total_messages = total,
                    "persisted queued messages from previous run"
                );
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to process session queues");
        }
    }

    // Create ClientChannel separately (needs session_manager for management API).
    #[cfg(feature = "client")]
    let _client_channel: Option<Arc<crate::channels::ClientChannel>> =
        config.channels.client.as_ref().filter(|c| c.enabled).map(|cfg| {
            let cc = crate::channels::ClientChannel::new(cfg.clone());
            cc.set_session_manager(session_manager.clone());
            cc.set_tool_specs(tools_arc.all_tools().iter().map(|t| t.spec()).collect());
            cc.set_workspace_dir(config.workspace_dir.clone());
            cc.set_config_path(config.config_path.clone());
            cc.set_skill_manager(skills_arc.clone());
            cc.set_provider_registry(registry_arc.clone());

            // ── WebSocket socket: SO_REUSEPORT / fd inheritance ──────────────
            // Hot switch: reuse the inherited fd so the new process can bind the
            // same port while the old process's socket is still open.
            // Normal startup: bind with SO_REUSEPORT and store the fd for the
            // next hot switch.
            #[cfg(unix)]
            {
                let inherited_fd = CLIENT_SOCKET_FD.load(Ordering::SeqCst);
                if inherited_fd >= 0 {
                    use std::os::unix::io::FromRawFd;
                    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(inherited_fd) };
                    tracing::info!(fd = inherited_fd, "client channel reusing inherited socket (hot switch)");
                    cc.set_pre_bound(std_listener);
                } else if let Ok(addr) = cfg.bind.parse::<std::net::SocketAddr>() {
                    match bind_reusable(addr.port()) {
                        Ok(l) => {
                            use std::os::unix::io::AsRawFd;
                            CLIENT_SOCKET_FD.store(l.as_raw_fd(), Ordering::SeqCst);
                            cc.set_pre_bound(l);
                        }
                        Err(e) => {
                            tracing::warn!(port = addr.port(), err = %e,
                                "SO_REUSEPORT bind failed for client channel, will rebind on listen()");
                        }
                    }
                }
            }
            Arc::new(cc)
        });
    #[cfg(feature = "client")]
    if let Some(ref cc) = _client_channel {
        channels.push(("client".to_string(), "default".to_string(), cc.clone() as Arc<dyn Channel>));
    }

    let prompt_config = build_prompt_config(&config.agent, &config.workspace_dir, &config.knowledge_dir);
    let mcp_manager_arc = Arc::new(mcp_manager);

    // Get MCP server instructions for attachment injection.
    let _mcp_instructions = mcp_manager_arc.server_instructions().await;

    // Scheduler config (used for both parts and webhook launch).
    let scheduler_config = config.agent.scheduler.clone();

    // scheduler_tx already created above; scheduler_rx goes to OrchestratorParts.

    // F35 / partial E29: build the AskRouter once and share between the
    // orchestrator (fulfill side) and AskUserTool::with_router (register
    // side). Stored as a top-level binding so future tool constructions
    // (DelegateTool etc.) can share it too.
    //
    // The matching `AskUserTool::with_router(ask_router, channels_map)`
    // registration belongs alongside the rest of `build_tools()`. That
    // function runs before `channels` is built in this function, so a
    // clean swap requires either threading `Arc<AskRouter>` into
    // build_tools earlier or constructing the real tool here and
    // overwrite-registering it after build_tools returns. Doing the
    // latter would need a `tools_mut()` accessor on the Arc<ToolRegistry>
    // we don't yet have. Left as the immediate F35 wire-up the next
    // session can finish in ~10 lines.
    let ask_router = Arc::new(crate::agents::AskRouter::new());

    // E30 final: build AgentRuntime to hand to the orchestrator. Today
    // the orchestrator's main loop still constructs AgentLoop per
    // session via `agent.loop_for_with_persist`; the AgentRuntime field
    // sits in parallel so E29 can flip the dispatch over without
    // having to re-thread plumbing first.
    let agent_runtime = {
        // Build the ResourceProvider once so ContextEngine can hold it as
        // a shared resource (rather than rebuilding per turn).
        let resources = crate::agents::resource_provider::ResourceProvider::new(
            Arc::clone(&skills_arc),
            Arc::clone(&sub_agent_registry),
            Vec::new(),
            config.workspace_dir.join("skills"),
            config.workspace_dir.join("agents"),
            config.knowledge_dir.to_string_lossy().to_string(),
            config.agent.prompt.timezone_offset,
        );
        let context_engine = Arc::new(crate::agents::context_engine::ContextEngine::new(
            &config.agent.context,
            Arc::clone(&registry_arc),
            resources,
            Arc::clone(&tools_arc),
        ));
        let tool_executor = Arc::new(crate::agents::tool_executor::ToolExecutor::new(
            Arc::clone(&tools_arc),
            config.agent.tool_timeout_secs,
        ));
        let loop_breaker = Arc::new(crate::agents::loop_breaker::LoopBreaker::new(
            crate::agents::loop_breaker::LoopBreakerConfig {
                max_tool_calls: config.agent.max_tool_calls,
                ..Default::default()
            },
        ));

        let defaults = crate::agents::runtime::RuntimeDefaults {
            permission_mode: config.agent.permission_mode,
            prompt: prompt_config,
        };

        crate::agents::AgentRuntime::new(
            Arc::clone(&registry_arc),
            Arc::clone(&tools_arc),
            Arc::clone(&skills_arc),
            Arc::clone(&sub_agent_registry),
            context_engine,
            tool_executor,
            loop_breaker,
        )
        .with_defaults(defaults)
    };

    let parts = OrchestratorParts {
        session_manager,
        channels,
        delegation_manager,
        delegation_rx,
        mcp_manager: Some(Arc::clone(&mcp_manager_arc)),
        scheduler_rx: Some(scheduler_rx),
        search_cooldown: Some(search_cooldown),
        unfinished_subagents,
        workspace_dir: config.workspace_dir.clone(),
        scheduler: Some(shared_scheduler.clone()),
        ask_router: Arc::clone(&ask_router),
        agent_runtime,
    };

    // ── Launch ─────────────────────────────────────────────────────────────

    let (orchestrator, _msg_tx) = Orchestrator::new(parts);
    let orchestrator = Arc::new(orchestrator);

    // H57: AgentLoop is gone; the ClientChannel's previous loop_registry +
    // evict_loop dance to flush stale per-session AgentLoop instances on
    // /new and /switch is no longer needed. SessionContext is invalidated
    // directly by the slash command handlers.

    print_banner(&config, mcp_manager_arc.server_count().await, mcp_manager_arc.tool_count().await, sub_agent_count, &sub_agent_names);

    // ── Scheduler tasks ────────────────────────────────────────────────────

    if scheduler_config.webhook.enabled {
        let wh_dir = config.workspace_dir.join("webhooks");
        let wh_jobs = crate::agents::load_webhook_jobs(&wh_dir);
        let wh_ctx = Arc::new(crate::agents::WebhookContext {
            orchestrator: Arc::clone(&orchestrator),
            timezone: tz_name.clone(),
        });
        let wh_config = scheduler_config.webhook.clone();

        // Bind the webhook port with SO_REUSEPORT so a hot-switch child can
        // bind the same port before the old process releases it.
        let wh_listener = {
            // If hot-switch stored a valid fd earlier, reuse it directly.
            let inherited_fd = LISTEN_SOCKET_FD.load(Ordering::SeqCst);
            if inherited_fd >= 0 {
                tracing::info!(fd = inherited_fd, "reusing inherited webhook socket from hot switch");
                // SAFETY: the fd was inherited from the parent via execv and is
                // a valid, already-bound, already-listening socket.
                use std::os::unix::io::FromRawFd;
                let std_listener = unsafe { std::net::TcpListener::from_raw_fd(inherited_fd) };
                Some(std_listener)
            } else {
                match bind_reusable(wh_config.port) {
                    Ok(l) => {
                        // Store fd for hot switch child.
                        #[cfg(unix)]
                        {
                            use std::os::unix::io::AsRawFd;
                            LISTEN_SOCKET_FD.store(l.as_raw_fd(), Ordering::SeqCst);
                        }
                        Some(l)
                    }
                    Err(e) => {
                        tracing::warn!(port = wh_config.port, err = %e,
                            "SO_REUSEPORT bind failed, webhook server will use normal bind");
                        None
                    }
                }
            }
        };

        tokio::spawn(async move {
            crate::agents::run_webhook_server(wh_ctx, wh_config, wh_jobs, wh_listener).await;
        });
    }

    // ── Scheduler task (heartbeat + cron via mpsc) ──────────────────────────

    {
        // Run the scheduler (it was created earlier with heartbeat config).
        if shared_scheduler.should_run() {
            let scheduler = Arc::clone(&shared_scheduler);
            tokio::spawn(async move { scheduler.run().await; });
        }
    }

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── SIGUSR1: set shutdown flag for checkpoint exit (hot switch) ────────
    #[cfg(unix)]
    {
        let mut sigusr1 = signal(SignalKind::user_defined1())
            .expect("failed to register SIGUSR1 handler");
        let shutdown_tx_usr1 = shutdown_tx.clone();
        tokio::spawn(async move {
            sigusr1.recv().await;
            tracing::debug!("SIGUSR1 received, setting shutdown flag");
            crate::SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
            let _ = shutdown_tx_usr1.send(true);
        });
    }

    // Wait for SIGINT or SIGTERM.
    tokio::spawn(async move {
        let _ = wait_for_signal().await;
        let _ = shutdown_tx.send(true);
        tracing::debug!("shutdown signal received, initiating graceful shutdown");
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

    // ── sd_notify: signal systemd that the daemon is ready ────────────────
    // For hot-switch startups: also tell systemd to track the new PID *before*
    // signalling the old process to exit, so systemd doesn't kill the cgroup.
    #[cfg(unix)]
    {
        if crate::hot_switch::is_hot_switch() {
            // Two-layer notification:
            //   1. Old process already sent sd_notify(MAINPID=our_pid) right after
            //      fork (do_hot_switch).  This is the preferred path because it
            //      comes from the trusted main PID.
            //   2. We also send MAINPID=self + READY here as a belt-and-suspenders
            //      fallback for the first-generation bootstrap case: if the running
            //      binary predates the MAINPID-relay fix, the old process never sent
            //      MAINPID, and only NotifyAccess=all (set in the service file)
            //      allows us to update it from here.
            let new_pid = std::process::id();
            if let Err(e) = sd_notify::notify(false, &[
                sd_notify::NotifyState::MainPid(new_pid),
                sd_notify::NotifyState::Ready,
            ]) {
                tracing::warn!(err = %e, "sd_notify MAINPID+READY failed");
            } else {
                tracing::debug!(new_pid, "sd_notify MAINPID+READY sent");
            }
            if let Some(old_pid) = crate::hot_switch::old_pid() {
                tracing::debug!(old_pid, "sending SIGUSR2 to old process — new process is ready");
                unsafe { libc::kill(old_pid as libc::pid_t, libc::SIGUSR2); }
            }
        } else {
            // Normal startup: tell systemd we are ready to accept connections.
            if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
                tracing::debug!(err = %e, "sd_notify READY failed (not running under systemd)");
            }
        }
    }

    // Run the message dispatch loop (blocks until shutdown).
    Arc::clone(&orchestrator).run(shutdown_rx).await.context("orchestrator run error")?;

    // Graceful shutdown.
    tracing::debug!("dispatch loop ended, shutting down listeners");
    orchestrator.shutdown_listeners().await;

    // ── Hot switch: fork+execv new binary, inherit listen socket ──────────
    // When SIGUSR1 set the shutdown flag (triggered by `myclaw update`), the
    // new binary is already on disk.  Fork a child, execv it with the inherited
    // listen socket fd, and wait for the child to signal readiness via SIGUSR2.
    // If the fork/execv fails, roll back (clear shutdown flag) — currently that
    // path is not reached because we exit after this block.
    #[cfg(unix)]
    if crate::is_shutting_down() {
        let socket_fd = LISTEN_SOCKET_FD.load(Ordering::SeqCst);
        tracing::debug!(socket_fd, "shutdown flag set, executing hot switch (fork+execv)");
        let client_fd = CLIENT_SOCKET_FD.load(Ordering::SeqCst);
        if let Err(e) = tokio::task::block_in_place(|| crate::hot_switch::do_hot_switch(socket_fd, client_fd)) {
            tracing::warn!(err = %e, "hot switch failed, daemon will exit normally");
        }
        // do_hot_switch either: (a) exits via the SIGUSR2 handler when new
        // process is ready, or (b) returns Err on failure. Either way we
        // fall through to normal exit below.
    }

    tracing::info!("myclaw daemon stopped");

    // Clean up PID file
    let pid_file = crate::signal::pid_file_path();
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
            tracing::debug!("received SIGUSR1 — hot switch triggered by `myclaw update`");
        }
    }
    Ok(())
}

// ── Hot-switch helpers ──────────────────────────────────────────────────────

/// Reset the persisted Telegram update offset so that `getUpdates` returns
/// recent messages instead of skipping everything the old process already
/// fetched.  The dedup layer in TelegramChannel will filter any duplicates.
fn reset_telegram_offset() {
    let data_dir = directories::ProjectDirs::from("", "", "myclaw")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".myclaw"));
    let offset_path = data_dir.join("telegram_offset");
    if offset_path.exists() {
        if let Err(e) = std::fs::remove_file(&offset_path) {
            tracing::warn!(err = %e, path = %offset_path.display(),
                "failed to remove telegram offset file");
        } else {
            tracing::info!(path = %offset_path.display(),
                "telegram offset cleared — new process will fetch fresh updates");
        }
    }
}

// ── Sub-agent recovery (hot-switch detection) ─────────────────────────────────
// Moved to `src/agents/recovery.rs` so that Application-layer types
// (Orchestrator, OrchestratorParts) can reference `UnfinishedSubAgent` without
// depending on the Composition Root (`daemon`).  Re-exported via `agents::`.
//
// Helpers used above:
//   crate::agents::recovery::scan_unfinished_subagents
//   crate::agents::recovery::cleanup_stale_subagent_markers
