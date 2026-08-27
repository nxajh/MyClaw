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

use crate::agents::AgentDelegator;
use crate::agents::{
    AgentMessenger, DelegationCoordinator, InMemoryBackend, McpManager, Orchestrator,
    OrchestratorParts, RunMode, SessionManager, Skill, SkillManager, SystemPromptConfig,
    ToolRegistry,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::channels::Channel;

pub(crate) mod builder;
pub(crate) mod lifecycle;
pub(crate) use builder::*;
pub(crate) use lifecycle::*;


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
            return crate::config::ConfigLoader::from_file(&p).context("failed to load config");
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
    socket
        .set_reuse_address(true)
        .context("SO_REUSEADDR failed")?;
    // socket2 v0.5 adds SOCK_CLOEXEC by default.  Clear it so that the fd
    // survives fork+execve during hot switch.  Without this the child gets
    // EBADF (fd closed by execve) or EPERM (fd number reused as a
    // non-pollable file type before epoll_ctl(EPOLL_CTL_ADD) is called).
    #[cfg(unix)]
    socket
        .set_cloexec(false)
        .context("clearing FD_CLOEXEC failed")?;
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
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let level = config.logging.level.as_deref().unwrap_or("info");
    // Build RUST_LOG-style directives: global level + per-module overrides.
    // Suppress noisy connection-pool / TLS layers that add no diagnostic value.
    let mut parts = vec![
        level.to_string(),
        "hyper_util=off".to_string(),
        "hyper=off".to_string(),
        "rustls=off".to_string(),
        "h2=off".to_string(),
    ];
    for (module, mod_level) in &config.logging.modules {
        parts.push(format!("{}={}", module, mod_level));
    }
    let directives = parts.join(",");

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directives));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_thread_ids(true));

    tracing::subscriber::set_global_default(subscriber).expect("failed to set tracing subscriber");
}

/// Print startup banner with config summary.
fn print_banner(
    config: &crate::config::AppConfig,
    mcp_servers: usize,
    mcp_tools: usize,
    sub_agent_count: usize,
    sub_agent_names: &[String],
) {
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
        println!(
            "  🤝 Sub-agents: {} ({})",
            sub_agent_count,
            names.join(", ")
        );
    }

    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();
}

// ═══════════════════════════════════════════════════════════════════════════════
// Composition Root — assemble all components
// ═══════════════════════════════════════════════════════════════════════════════

/// Build the Registry and register all providers from config.

/// Run the MyClaw daemon, blocking until shutdown.
pub async fn run(config: crate::config::AppConfig) -> Result<()> {
    // Initialize global safety config from the loaded config.
    crate::config::init_safety_config(config.safety.clone());

    // Initialize the global data dir so provider-layer media rendering
    // (which has no Session/AppConfig in scope) can resolve
    // `sessions/<id>/files/...` marker paths without going through cwd.
    crate::providers::media::init_data_dir(config.base_dir.clone());

    // Issue #84: tool-shell PATH fix-ups (static fallback always on; the
    // login-shell probe below is non-blocking — it's a spawned background
    // task, not awaited here).
    crate::tools::shell_env::init(config.shell.clone());

    // 让进程 cwd 与 workspace_dir 一致，保证 file_read 等工具的相对路径解析
    // 和 system prompt 告诉 LLM 的 "Working directory" 一致
    std::env::set_current_dir(&config.workspace_dir).with_context(|| {
        format!(
            "failed to set cwd to workspace_dir '{}'",
            config.workspace_dir.display()
        )
    })?;

    // D27 — RFC v2 §三.A: workspace/agents/main/AGENT.md is required.
    // Without it there is no default agent to route inbound messages to.
    // Run scripts/migrate_main_agent.sh on first boot after upgrade.
    let main_agent_md = config.agents_root().join("main").join("AGENT.md");
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
        reset_telegram_offset(&config.base_dir);

        // ── Queue processing ──────────────────────────────────────────────
        // Queue drain is handled later (after session backend initialization)
        // in the dedicated queue processing section.  We skip it here because
        // process_all_queues deletes the queue files, and the later call needs
        // to read them using the proper session backend.

        // ── Notify old process ────────────────────────────────────────────
        // SIGUSR2 is sent after full initialization, but Light C defers it until
        // after channels are up and *before* orchestrator.run() so readiness is
        // real. Old process no longer exits on SIGUSR2 alone; it exits after
        // do_hot_switch returns.
    }

    // Write PID file for hot-switch coordination (used by `myclaw update`).
    let pid_file = crate::signal::pid_file_path();
    if let Err(e) = std::fs::write(&pid_file, std::process::id().to_string()) {
        tracing::warn!(err = %e, "failed to write PID file");
    } else {
        tracing::debug!(pid = %std::process::id(), path = %pid_file.display(), "PID file written");
    }

    // Ensure memory root exists
    if let Err(e) = crate::memory::ensure_memory_dir(config.memory_root().to_str().unwrap_or(".")) {
        tracing::warn!(err = %e, "failed to create memory root");
    }

    // ── RFC §6 数据迁移（启动自动）──────────────────────────────────────────
    // users.json / user_resolver.json / sessions/ / tasks.json / cron/jobs.json
    // 与 users/ 遗留目录。必须在 Scheduler（jobs.json）、UserResolver、
    // UserRegistry、JsonFileBackend（sessions/）等组件加载之前执行，否则迁移
    // 后的 FQID 与组件内存态不一致。失败不阻断启动（注册表对旧数据降级兼容，
    // .bak 备份保留可手动恢复）。
    let base_dir = config.base_dir.clone();
    // P1 布局重构 fail-fast：检测到旧布局（workspace/sessions 存在而
    // {base_dir}/sessions 缺失）时拒绝启动 —— 布局迁移由外置脚本完成
    // （见 docs/storage-layout-and-trigger-redesign.md §5），daemon 不做
    // 双读兼容。先停机执行迁移脚本再重启。
    if config.workspace_dir.join("sessions").exists() && !config.sessions_root().exists() {
        eprintln!(
            "检测到旧存储布局：{} 存在而 {} 缺失。\n\
             布局迁移已改为外置脚本，daemon 不再自动迁移。\n\
             请先停机执行（务必显式传入下面两个路径 —— 脚本的内置默认值\n\
             可能与本次 daemon 实际解析出的 workspace_dir/base_dir 不一致，\n\
             传错路径会把数据搬到 daemon 读不到的地方）：\n\
             python3 scripts/migrate-layout.py --dry-run --workspace {} --base {}\n\
             确认无误后：\n\
             python3 scripts/migrate-layout.py --apply --workspace {} --base {}\n\
             （详见 docs/storage-layout-and-trigger-redesign.md §5），完成后重启 daemon。",
            config.workspace_dir.join("sessions").display(),
            config.sessions_root().display(),
            config.workspace_dir.display(),
            base_dir.display(),
            config.workspace_dir.display(),
            base_dir.display(),
        );
        anyhow::bail!(
            "old storage layout detected (workspace/sessions exists, {}/sessions missing); run scripts/migrate-layout.py first",
            base_dir.display()
        );
    }
    // P1-A 裸 uuid 目录 fail-fast：{base_dir}/sessions 下不允许任何非裸-uuid
    // 目录存在（`.legacy` 归档目录除外）。JsonFileBackend 的读路径能容忍
    // 遗留 `myclaw_s_<uuid>` 命名（避免静默丢会话），但那是运行时兜底，不是
    // 允许这种状态长期存在——裸化是 B9 迁移步骤的职责，发现残留就拒绝启动，
    // 逼着先跑迁移脚本，而不是让两种目录命名无限期共存。
    {
        let stale = super::builder::find_legacy_session_dirs(&config.sessions_root());
        if !stale.is_empty() {
            eprintln!(
                "检测到 {} 个非裸-uuid 会话目录（举例：{}）。\n\
                 P1-A 裸 uuid 目录布局要求 {{base_dir}}/sessions 下只允许裸 uuid \n\
                 目录（`.legacy` 归档除外）。请先停机执行：\n\
                 python3 scripts/migrate-layout.py --dry-run --workspace {} --base {}\n\
                 确认无误后：\n\
                 python3 scripts/migrate-layout.py --apply --workspace {} --base {}\n\
                 完成后重启 daemon。",
                stale.len(),
                stale.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
                config.workspace_dir.display(),
                base_dir.display(),
                config.workspace_dir.display(),
                base_dir.display(),
            );
            anyhow::bail!(
                "{} legacy-named session directories found under {}/sessions; run scripts/migrate-layout.py first",
                stale.len(),
                base_dir.display()
            );
        }
    }
    if let Err(e) = crate::migration::run_auto(&config.workspace_dir, &base_dir, &config.system.namespace)
    {
        tracing::warn!(err = %e, "migration: 自动迁移失败（继续以原数据启动）");
    }

    // ── Composition Root: assemble all components ──────────────────────────

    let registry = build_registry(&config)?;

    let mcp_manager = McpManager::new();
    if let Err(e) = mcp_manager.connect(&config.mcp_servers).await {
        tracing::warn!(err = %e, "MCP server connection had errors");
    }

    // Build skill manager (SKILL.md files).
    let skills = build_skill_manager(&config);
    let skills_arc: Arc<parking_lot::RwLock<SkillManager>> =
        Arc::new(parking_lot::RwLock::new(skills));

    // Resolve timezone: config.timezone (IANA) takes precedence over timezone_offset.
    let tz_name = config.prompt.timezone.clone().unwrap_or_else(|| {
        // Convert legacy offset to Etc/GMT name (signs are inverted in Etc/GMT).
        let offset = config.prompt.timezone_offset;
        if offset == 0 {
            "UTC".to_string()
        } else {
            format!(
                "Etc/GMT{}",
                if offset > 0 {
                    format!("-{}", offset)
                } else {
                    format!("{}", -offset)
                }
            )
        }
    });

    // Create scheduler channel early (needed for SharedScheduler creation).
    let (scheduler_tx, scheduler_rx) =
        tokio::sync::mpsc::channel::<crate::agents::SchedulerEvent>(100);

    // Build shared scheduler (owns all cron job data).
    // P1-B2: job storage moved to the data side — {base_dir}/jobs with
    // per-job {id}/meta.json; the legacy single-file jobs.json (workspace
    // or data side) is a read-only fallback inside Scheduler::new.
    let jobs_root = config.jobs_root();
    let cron_dir = config.workspace_dir.join("cron");

    // Migrate from old markdown files if the jobs store is empty.
    if !jobs_root.join("jobs.json").exists()
        && std::fs::read_dir(&jobs_root).map(|mut rd| rd.next().is_none()).unwrap_or(true)
    {
        let (dummy_tx, _) = tokio::sync::mpsc::channel(1);
        let migrator = crate::scheduling_runtime::scheduler::Scheduler::new(
            jobs_root.clone(),
            &config.system.namespace,
            tz_name.clone(),
            None,
            dummy_tx,
            config.last_channel_path(),
            config.last_recipient_path(),
        );
        let count = migrator.migrate_from_markdown(&cron_dir);
        if count > 0 {
            tracing::debug!(count = count, "migrated cron jobs from markdown");
        }
    }

    // Idle-time memory distillation config (None disables the distill tick).
    let distill_config = if config.memory.distill_enabled {
        Some(crate::scheduling_runtime::scheduler::DistillConfig {
            idle_secs: config.memory.distill_idle_secs,
            interval_secs: config.memory.distill_interval_secs,
        })
    } else {
        None
    };

    let shared_scheduler = crate::scheduling_runtime::scheduler::Scheduler::new(
        jobs_root,
        &config.system.namespace,
        tz_name.clone(),
        distill_config,
        scheduler_tx.clone(),
        config.last_channel_path(),
        config.last_recipient_path(),
    );

    // Global base dir (known_users.json, user_resolver.json) — computed early
    // (before the RFC §6 auto-migration) so the migration, the registry and the
    // identity resolver share one location.

    // G39: shared user resolver — defaults to identity (user_id == routing_key).
    // P3: persisted; `/link` folds routing_keys into one user_id via `set`.
    let user_resolver = Arc::new(crate::agents::UserResolver::persistent(&base_dir));

    // Shared AskRouter — wired into both AskUserTool (register side, inside
    // build_tools) and the Orchestrator (fulfill side, set on OrchestratorParts
    // below). Same Arc, single inbox.
    let ask_router = Arc::new(crate::agents::AskRouter::new());

    // Global user registry — replaces per-channel KnownSenders/RateLimiter.
    // Orchestrator records every inbound message; slash commands query.
    // P3: with_resolver folds contacts/mailbox keys for linked identities.
    let known_users = Arc::new(
        crate::agents::KnownUsersRegistry::new(&base_dir)
            .with_resolver(Arc::clone(&user_resolver)),
    );
    known_users.migrate_legacy(&base_dir);

    // P4 用户实体注册表（uid/email/username，P1-B1 目录化
    // `{base_dir}/users/{uuid}/meta.json`；旧 users.json 仅兜底读）。存量
    // identity 一次性迁移归 `myclaw/u/root`（幂等：root 已存在即跳过）。
    // P4 第二波：namespace 取自 `[system] namespace`（默认 myclaw，存量
    // users.json/resolver 绑定零影响；改 namespace 的迁移见 RFC §2.2，本波不做）。
    let user_registry = Arc::new(crate::agents::UserRegistry::with_namespace(
        &base_dir,
        &config.system.namespace,
    ));
    user_registry.migrate_legacy_to_root(&known_users, &user_resolver);

    // Build session backend + manager early — B15: the DelegationCoordinator
    // needs SessionManager (shared backend) to create sub-sessions as
    // top-level peers instead of opening a per-parent JsonFileBackend at
    // `{sessions_root}/{parent}/subagents/`.
    // Also built before build_tools so SessionQueryTool can share the backend.
    let session_backend = build_session_backend(&config);

    // issue #129: always-on completion channel for `background: true` shell
    // commands — unlike the delegation channel below, not conditional on
    // sub-agent configuration (shell backgrounding is a core single-agent
    // feature too).
    let (shell_notice_tx, shell_notice_rx) =
        tokio::sync::mpsc::channel::<crate::tools::shell::ShellCompletion>(100);

    // Build tool registry (all built-in + MCP + skill tools + ask_user).
    let (mut tools, task_boards, send_message_tool, friend_ctx, shell_registry, shell_tool) = build_tools(
        &mcp_manager,
        &skills_arc,
        &shared_scheduler,
        &config,
        config.memory_root().to_str().unwrap_or("."),
        &user_resolver,
        Arc::clone(&ask_router),
        &known_users,
        &user_registry,
        &config.system.namespace,
        Arc::clone(&session_backend),
        shell_notice_tx,
    )
    .await;
    // P1 cross-user delivery (RFC §3.5): give send_message access to the
    // known-users registry so `recipient=@nick` can resolve contacts and
    // deliver to the peer's user-level mailbox.
    send_message_tool.set_known_users(Arc::clone(&known_users));
    // P4: cross-user recipient 解析（u/uid / 邮箱 → FQID）与发送者显示名。
    send_message_tool.set_user_registry(Arc::clone(&user_registry));

    // Build sub-agent configs (AGENT.md files from workspace/agents/).
    let sub_agent_configs = build_sub_agents(&config);
    let sub_agent_count = sub_agent_configs.len();
    let sub_agent_names: Vec<String> = sub_agent_configs.iter().map(|a| a.name.clone()).collect();
    let sub_agent_registry = Arc::new(crate::agents::AgentRegistry::from_vec(
        sub_agent_configs.clone(),
    ));

    let registry_arc: Arc<dyn crate::providers::ProviderRegistry> = Arc::new(registry);

    // Register WebSearchTool — requires ProviderRegistry for search routing.
    let search_cooldown = Arc::new(crate::tools::SearchProviderCooldown::new());
    tools.register(Arc::new(crate::tools::WebSearchTool::new(
        registry_arc.clone(),
        Arc::clone(&search_cooldown),
    )));
    tracing::debug!("web_search tool registered (connected to ProviderRegistry)");

    // Register media retrieval tools — require ProviderRegistry for model lookup.
    tools.register(Arc::new(crate::tools::ViewImageTool::new(
        Arc::clone(&registry_arc),
        config.base_dir.clone(),
    )));
    tools.register(Arc::new(crate::tools::HearAudioTool::new(
        Arc::clone(&registry_arc),
        config.base_dir.clone(),
    )));
    tools.register(Arc::new(crate::tools::ViewVideoTool::new(
        Arc::clone(&registry_arc),
        config.base_dir.clone(),
    )));

    // WorkspaceWatcher for hot-reload (self-maintaining mode): edits to
    // `agents/` (AGENT.md) and `skills/` (SKILL.md) are picked up live via
    // `AgentRegistry::reload_from_dir` / `SkillManager::reload` — no daemon
    // restart needed.
    // P1: agents/skills 热加载目录随 base dir（系统配置面）。
    // Issue #83: also watches `~/.agents/skills` (shared skill library)
    // when enabled — a symlink `skills update` refreshes there takes
    // effect here without a daemon restart.
    let agents_skills_dir = config.agents_skills_dir_opt();
    let _watcher = crate::agents::WorkspaceWatcher::spawn_managed(
        config.skills_root(),
        agents_skills_dir.clone(),
        config.agents_root(),
        &config.memory_root(),
        sub_agent_registry.as_ref().clone(),
        skills_arc.clone(),
    )?;

    let session_manager = Arc::new(
        SessionManager::new(Arc::clone(&session_backend))
            .with_agents(Arc::clone(&sub_agent_registry))
            .with_resolver(Arc::clone(&user_resolver)),
    );
    // issue #140: ShellTool was built before SessionManager existed (same
    // ordering constraint ClientChannel/DelegationCoordinator hit) — wire it
    // in now so `shell`'s tool calls can register pending async work
    // (`ShellTool::register_pending` → `SessionContext::add_pending_task`).
    shell_tool.set_session_manager(Arc::clone(&session_manager));

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
            Arc::clone(&session_manager),
            config.worktrees_root(),
            &config.system.namespace,
            config.delegation.max_depth,
            shell_registry.clone(),
        );
        let delegator_arc = Arc::new(delegator);

        // Clean up stale worktrees from crashed/timed-out sub-agent runs.
        delegator_arc.cleanup_stale_worktrees();

        // RFC agent-messaging §3: wire the agent-to-agent bus into
        // send_message (main → sub via sub_session_id, sub → parent via the
        // DelegationEvent channel). Set-once; single-agent mode never
        // calls this, so `recipient` targeting errors there.
        send_message_tool.set_messenger(Arc::clone(&delegator_arc) as Arc<dyn AgentMessenger>);

        // Build agent_delegate tool. H47: now wired through `AgentDelegator`
        // (the legacy `TaskDelegator` trait has been removed).
        let delegate_tool = crate::tools::AgentDelegateTool::new(
            Arc::clone(&delegator_arc) as Arc<dyn AgentDelegator>
        );

        // Build parent tool registry: same tools + agent_delegate + agent_list
        // + agent_kill + sessions_yield + tool_search.
        let mut parent_tools = ToolRegistry::new();
        for tool in base_tools_arc.all_tools() {
            parent_tools.register(tool);
        }
        parent_tools.register(Arc::new(delegate_tool));
        tracing::debug!("agent_delegate tool registered (multi-agent mode)");

        // agent_list / agent_kill let the parent inspect and terminate running
        // sub-agents. Both depend on the DelegationCoordinator, so they live in
        // the multi-agent branch (single-agent mode has no coordinator).
        parent_tools.register(Arc::new(crate::tools::AgentListTool::new(Arc::clone(
            &delegator_arc,
        ))));
        parent_tools.register(Arc::new(crate::tools::AgentKillTool::new(Arc::clone(
            &delegator_arc,
        ))));
        // agent_resume (timeout layer 3): revive a timed-out sub-agent with a
        // fresh budget — continues the preserved sub-session instead of
        // re-delegating from scratch.
        parent_tools.register(Arc::new(crate::tools::AgentResumeTool::new(Arc::clone(
            &delegator_arc,
        ))));
        tracing::debug!("agent_list / agent_kill / agent_resume tools registered (multi-agent mode)");

        // sessions_yield (RFC delegation-notice-queue §3): deterministic turn
        // hand-off for the parent agent after spawning async sub-agents.
        parent_tools.register(Arc::new(crate::tools::SessionsYieldTool::new()));
        tracing::debug!("sessions_yield tool registered (multi-agent mode)");

        let tool_search = crate::tools::ToolSearchTool::new(Arc::clone(&base_tools_arc));
        parent_tools.register(Arc::new(tool_search));

        (Arc::new(parent_tools), Some(delegator_arc))
    };

    warn_missing_agent_tool_references(&sub_agent_configs, &tools_arc);

    // ── Delegation channel (conditional — only when sub-agents configured) ─────
    // The DelegationCoordinator is the single owner of the sender and
    // the running-task table (RFC §三.C). DelegationManager no longer
    // exists as a separate type.
    let delegation_rx = if let Some(ref delegator) = sub_agent_delegator_arc {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agents::DelegationEvent>(100);
            delegator.set_event_sender(tx.clone());
        Some(rx)
    } else {
        None
    };

    // session_backend / session_manager are constructed earlier (above the
    // DelegationCoordinator block — see B15 note) so they can be shared.
    let mut channels = build_channel_accounts(&config);

    // ── Sub-agent recovery: detect interrupted sub-agents from a previous run ──
    // F37 + H50: scan SessionManager for sub-sessions with mid-turn history
    // instead of reading `subagent_running_*.json` marker files (the marker
    // mechanism has been deleted along with the corresponding writes in
    // DelegationCoordinator).
    // P4 (§7): scan the session store ONCE at startup and share the result
    // with sub-agent recovery and orchestrator run_startup below.
    let all_sessions = session_manager.list_all_sessions();
    let unfinished_subagents =
        crate::agents::recovery::scan_unfinished_subagents(&session_manager, &all_sessions);

    // Load durable checkpoints from the previous run. Tasks with a checkpoint
    // status of "checkpointed" or "running" were interrupted by a clean
    // shutdown (not a crash) and will be resumed via the normal recovery path.
    // Tasks without a checkpoint that appear "unfinished" are crash remnants.
    let checkpoints: Vec<crate::storage::DelegationCheckpoint> =
        session_manager.backend().load_delegation_checkpoints();
    if !checkpoints.is_empty() {
        tracing::info!(
            count = checkpoints.len(),
            checkpointed = checkpoints.iter().filter(|c| c.status == "checkpointed").count(),
            running = checkpoints.iter().filter(|c| c.status == "running").count(),
            "loaded delegation checkpoints from previous run"
        );
    }

    // Correlate: unfinished sub-agents that have a matching checkpoint are
    // confirmed resumable (clean shutdown). Those WITHOUT a checkpoint are
    // crash remnants — log them at warn level so operators can distinguish.
    // Checkpoints carrying a terminal status (方案 A tombstone) are neither:
    // the task already finished and `run_startup` skips it. Both sides key on
    // the sub-session id (the checkpoint primary key).
    let checkpoint_sub_session_ids: std::collections::HashSet<&str> =
        checkpoints.iter().map(|c| c.sub_session_id.as_str()).collect();
    let terminal_sub_session_ids: std::collections::HashSet<&str> = checkpoints
        .iter()
        .filter(|c| {
            matches!(
                c.status.as_str(),
                "completed" | "failed" | "timed_out" | "cancelled"
            )
        })
        .map(|c| c.sub_session_id.as_str())
        .collect();
    for sa in &unfinished_subagents {
        if checkpoint_sub_session_ids.contains(sa.sub_session_id.as_str()) {
            if terminal_sub_session_ids.contains(sa.sub_session_id.as_str()) {
                tracing::info!(
                    agent = %sa.agent_name,
                    session_id = %sa.sub_session_id,
                    "unfinished sub-agent with terminal checkpoint (tombstone — skipped on recovery)"
                );
            } else {
                tracing::info!(
                    agent = %sa.agent_name,
                    session_id = %sa.sub_session_id,
                    "resumable sub-agent (checkpointed — clean shutdown)"
                );
            }
        } else {
            tracing::warn!(
                agent = %sa.agent_name,
                session_id = %sa.sub_session_id,
                "unfinished sub-agent without checkpoint (crash remnant)"
            );
        }
    }

    if !unfinished_subagents.is_empty() {
        tracing::warn!(
            count = unfinished_subagents.len(),
            "detected unfinished sub-agents from previous run"
        );
    }

    // Create ClientChannel separately (needs session_manager for management API).
    #[cfg(feature = "client")]
    let _client_channel: Option<Arc<crate::webui::ClientChannel>> =
        config.channels.client.as_ref().filter(|c| c.enabled).map(|cfg| {
            let cc = crate::webui::ClientChannel::new(cfg.clone());
            cc.set_session_manager(session_manager.clone());
            cc.set_tool_specs(tools_arc.all_tools().iter().map(|t| t.spec()).collect());
            cc.set_workspace_dir(config.workspace_dir.clone());
            cc.set_memory_root(config.memory_root());
            cc.set_config_path(config.config_path.clone());
            cc.set_skill_manager(skills_arc.clone());
            cc.set_provider_registry(registry_arc.clone());
            cc.set_user_resolver(user_resolver.clone());

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
        channels.push((
            "client".to_string(),
            "default".to_string(),
            cc.clone() as Arc<dyn Channel>,
        ));
    }

    let prompt_config = build_prompt_config(
        &config.agent,
        &config.prompt,
        &config.base_dir,
        &config.workspace_dir,
        &config.memory_root(),
    );
    let mcp_manager_arc = Arc::new(mcp_manager);

    // Get MCP server instructions for attachment injection.
    let _mcp_instructions = mcp_manager_arc.server_instructions().await;

    // Scheduler config (used for both parts and webhook launch).
    let scheduler_config = config.scheduler.clone();

    // scheduler_tx already created above; scheduler_rx goes to OrchestratorParts.
    // ask_router was built above (before build_tools) so AskUserTool can
    // register with the same instance the orchestrator fulfills from.

    // Build the AgentRuntime handed to the orchestrator: the shared
    // infrastructure bundle (providers, tools, skills, agents,
    // context_engine, executors) that every per-turn `Agent::run` reads.
    let agent_runtime = {
        // Build the ResourceProvider once so CompactionEngine can hold it as
        // a shared resource (rather than rebuilding per turn).
        let resources = crate::agents::resource_provider::ResourceProvider::new(
            Arc::clone(&skills_arc),
            Arc::clone(&sub_agent_registry),
            Vec::new(),
            config.skills_root(),
            config.agents_root(),
            config.memory_root().to_string_lossy().to_string(),
            config.prompt.timezone_offset,
        );
        let context_engine = Arc::new(crate::agents::compaction_engine::CompactionEngine::new(
            &config.context_engine,
            Arc::clone(&registry_arc),
            resources,
            Arc::clone(&tools_arc),
        ));
        let tool_executor = Arc::new(
            crate::agents::tool_executor::ToolExecutor::new(
                config.tool_executor.timeout_secs,
            )
            .with_ask_router(Arc::clone(&ask_router)),
        );
        let loop_breaker = Arc::new(crate::agents::loop_breaker::LoopBreaker::new(
            crate::api::loop_breaker::LoopBreakerConfig {
                max_tool_calls: config.loop_breaker.max_tool_calls,
                ..Default::default()
            },
        ));

        let defaults = crate::agents::runtime::RuntimeDefaults {
            permission_mode: config.agent.permission_mode,
            prompt: prompt_config,
            auto_tts: config.agent.auto_tts,
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
        .with_mcp_manager(Arc::clone(&mcp_manager_arc))
        .with_search_cooldown(Arc::clone(&search_cooldown))
        .with_task_boards(Arc::clone(&task_boards))
        .with_sessions_dir(config.sessions_root())
        // P4 第二波：注册表供输出渲染（`<ref>` → `@昵称(u/uid)`，流式/Done/fallback）。
        .with_user_registry(Arc::clone(&user_registry))
    };
    // Issue #83: so `/reload` (agents/commands/reload.rs) can layer in the
    // shared skill library too, instead of dropping it on manual reload.
    let agent_runtime = match agents_skills_dir.clone() {
        Some(dir) => agent_runtime.with_agents_skills_dir(dir),
        None => agent_runtime,
    };

    // DelegationCoordinator was constructed before the runtime existed
    // (circular dep: runtime needs the AgentRegistry the coordinator
    // also holds). Install the runtime now that both sides are ready —
    // sub-agent turns will read it via the cell.
    if let Some(ref delegator) = sub_agent_delegator_arc {
        delegator.set_runtime(agent_runtime.clone());
    }

    let parts = OrchestratorParts {
        session_manager,
        channels,
        delegator: sub_agent_delegator_arc.clone(),
        delegation_rx,
        scheduler_rx: Some(scheduler_rx),
        shell_notice_rx: Some(shell_notice_rx),
        workspace_dir: config.workspace_dir.clone(),
        base_dir: base_dir.clone(),
        scheduler: Some(shared_scheduler.clone()),
        ask_router: Arc::clone(&ask_router),
        known_users: Arc::clone(&known_users),
        user_registry: Arc::clone(&user_registry),
        agent_runtime,
        shell_registry: Some(shell_registry.clone()),
    };

    // ── Launch ─────────────────────────────────────────────────────────────

    let (orchestrator, _msg_tx) = Orchestrator::new(parts);

    // Friend tools need the live channel registry for §4.3 peer
    // notifications (framework template push, zero LLM tokens).
    friend_ctx.set_channels(orchestrator.ctx().channels.clone());

    // RFC channel-role-split §1.2: back-fill the live channel registry into
    // the SessionManager (built before the Orchestrator existed — circular
    // dep, same reason friend_ctx gets it after construction). Sessions
    // materialized BEFORE this point have no registry wired; every
    // SessionContext created from here on resolves channels via
    // `Session::resolve_channel()`.
    orchestrator.ctx().sessions.set_channel_registry(
        orchestrator.ctx().channels.clone().into(),
    );

    // H57: AgentLoop is gone; the ClientChannel's previous loop_registry +
    // evict_loop dance to flush stale per-session AgentLoop instances on
    // /new and /switch is no longer needed. SessionContext is invalidated
    // directly by the slash command handlers.

    print_banner(
        &config,
        mcp_manager_arc.server_count().await,
        mcp_manager_arc.tool_count().await,
        sub_agent_count,
        &sub_agent_names,
    );

    // ── Scheduler tasks ────────────────────────────────────────────────────

    if scheduler_config.webhook.enabled {
        // Webhook channels live on unified jobs (an optional `webhook` object
        // in `{jobs_root}/{uuid}/meta.json`, §3.4 orthogonal model). A job
        // without a webhook channel simply isn't in the route table — only
        // the built-in /hooks/* endpoints remain besides it.
        let wh_ctx = Arc::new(crate::agents::WebhookContext {
            hook: Arc::clone(orchestrator.ctx())
                as Arc<dyn crate::scheduling_runtime::scheduler::OrchestratorHook>,
            timezone: tz_name.clone(),
            scheduler: Arc::clone(&shared_scheduler),
        });
        let wh_config = scheduler_config.webhook.clone();

        // Bind the webhook port with SO_REUSEPORT so a hot-switch child can
        // bind the same port before the old process releases it.
        let wh_listener = {
            // If hot-switch stored a valid fd earlier, reuse it directly.
            let inherited_fd = LISTEN_SOCKET_FD.load(Ordering::SeqCst);
            if inherited_fd >= 0 {
                tracing::info!(
                    fd = inherited_fd,
                    "reusing inherited webhook socket from hot switch"
                );
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
            crate::agents::run_webhook_server(wh_ctx, wh_config, wh_listener).await;
        });
    }

    // ── Scheduler task (cron via mpsc) ────────────────────────────────────────

    {
        // Run the scheduler loop (cron jobs + distill checks).
        if shared_scheduler.should_run() {
            let scheduler = Arc::clone(&shared_scheduler);
            tokio::spawn(async move {
                scheduler.run().await;
            });
        }
    }

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── SIGUSR1: set shutdown flag for checkpoint exit (hot switch) ────────
    #[cfg(unix)]
    {
        let mut sigusr1 =
            signal(SignalKind::user_defined1()).expect("failed to register SIGUSR1 handler");
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

    // ── SIGUSR2: new process ready (do NOT exit immediately) ──────────────
    // Light C: SIGUSR2 means "new process is ready / status may go completed".
    // The old process must finish drain + do_hot_switch bookkeeping, then
    // exit(0) from the main path after do_hot_switch returns. Exiting here
    // races with incomplete tool-result persist and fuels recovery re-exec loops.
    #[cfg(unix)]
    {
        let mut sigusr2 =
            signal(SignalKind::user_defined2()).expect("failed to register SIGUSR2 handler");
        tokio::spawn(async move {
            sigusr2.recv().await;
            tracing::info!(
                "SIGUSR2 received — new process ready; old process will exit after hot-switch bookkeeping"
            );
            crate::hot_switch::mark_new_process_ready();
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
            if let Err(e) = sd_notify::notify(
                false,
                &[
                    sd_notify::NotifyState::MainPid(new_pid),
                    sd_notify::NotifyState::Ready,
                    sd_notify::NotifyState::Errno(0),
                ],
            ) {
                tracing::warn!(err = %e, "sd_notify MAINPID+READY failed");
            } else {
                tracing::debug!(new_pid, "sd_notify MAINPID+READY+ERRNO=0 sent");
            }
            if let Some(old_pid) = crate::hot_switch::old_pid() {
                tracing::debug!(
                    old_pid,
                    "sending SIGUSR2 to old process — new process is ready"
                );
                unsafe {
                    libc::kill(old_pid as libc::pid_t, libc::SIGUSR2);
                }
            }
            // Mark update-state completed so `myclaw status` can confirm the switch.
            if let Err(e) = crate::update_state::UpdateState::mark_completed(new_pid) {
                tracing::warn!(err = %e, "failed to write update-state completed");
            }
        } else {
            // Normal startup: tell systemd we are ready to accept connections.
            if let Err(e) = sd_notify::notify(
                false,
                &[
                    sd_notify::NotifyState::Ready,
                    sd_notify::NotifyState::Errno(0),
                ],
            ) {
                tracing::debug!(err = %e, "sd_notify READY failed (not running under systemd)");
            }
        }
    }

    // Capture shared handles before `run` consumes the orchestrator so Light C
    // can drain turns *after* fork while the new process is coming up.
    let deferred_turn_tracker = orchestrator.turn_tracker();
    let deferred_delegator = orchestrator.delegator();

    // Run the message dispatch loop (blocks until shutdown). `run` consumes the
    // orchestrator and aborts its listener tasks before returning.
    // On hot-switch path, turn drain is deferred until after fork (below).
    orchestrator
        .run(shutdown_rx, unfinished_subagents, all_sessions)
        .await
        .context("orchestrator run error")?;

    tracing::debug!("dispatch loop ended, listeners aborted");

    // ── Hot switch: fork first, then drain in-flight turns ────────────────
    // Light C sequence (no A / no USR3):
    //   1. Fork+exec new binary immediately so readiness is independent of
    //      long shell tools.
    //   2. Wait for SIGUSR2 (new ready) with polling — old does NOT exit(0)
    //      in the signal handler.
    //   3. Drain turns / sub-agents so tool results (incl. short `myclaw
    //      update`) persist while the new process is already serving.
    //   4. Exit 0. New process deferred recovery until we die.
    #[cfg(unix)]
    if crate::SHUTDOWN_FLAG.load(std::sync::atomic::Ordering::SeqCst) {
        let socket_fd = LISTEN_SOCKET_FD.load(Ordering::SeqCst);
        tracing::debug!(
            socket_fd,
            "shutdown flag set, executing hot switch (fork+execv first, drain after)"
        );
        let client_fd = CLIENT_SOCKET_FD.load(Ordering::SeqCst);

        // Phase 1: fork in a blocking task (polls for SIGUSR2 readiness).
        // Phase 2: drain turns concurrently so short `myclaw update` can finish
        // and persist while the new process initializes. Contract B: update
        // must not poll for completed, so drain should not hang on update alone.
        let switch_handle = tokio::task::spawn_blocking(move || {
            crate::hot_switch::do_hot_switch(socket_fd, client_fd)
        });

        tracing::info!(
            active = deferred_turn_tracker.active_count(),
            "hot switch: draining in-flight turns concurrent with new process startup"
        );
        deferred_turn_tracker
            .drain(std::time::Duration::from_secs(30))
            .await;
        if let Some(delegator) = deferred_delegator.as_ref() {
            delegator.checkpoint_and_cancel_all();
        }
        tracing::info!("hot switch: post-fork drain complete");

        let switch_result = switch_handle.await;
        match switch_result {
            Ok(Ok(())) => {
                tracing::info!(
                    "hot switch: new process ready and turns drained — old process exiting cleanly"
                );
                let pid_file = crate::signal::pid_file_path();
                if pid_file.exists() {
                    if let Ok(contents) = std::fs::read_to_string(&pid_file) {
                        if contents.trim() == std::process::id().to_string() {
                            let _ = std::fs::remove_file(&pid_file);
                        }
                    }
                }
                std::process::exit(0);
            }
            Ok(Err(e)) => {
                tracing::error!(
                    err = %e,
                    "hot switch failed — exiting with non-zero code for systemd restart"
                );
                std::process::exit(1);
            }
            Err(e) => {
                tracing::error!(
                    err = %e,
                    "hot switch join failed — exiting with non-zero code for systemd restart"
                );
                std::process::exit(1);
            }
        }
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::filters::ToolFilter;
    use crate::config::sub_agent::{AgentIsolation, SubAgentConfig};
    use crate::providers::{Tool, ToolResult};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::api::tool::ToolContext,
        ) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn detects_missing_agent_tool_references() {
        let agent = SubAgentConfig {
            name: "coder".to_string(),
            system_prompt: String::new(),
            tools: ToolFilter::Allow(vec!["shell".to_string(), "ghost_tool".to_string()]),
            skills: Default::default(),
            mcp: Default::default(),
            max_tool_calls: None,
            description: None,
            model: None,
            isolation: AgentIsolation::default(),
            timeout: None,
        };
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("shell")));

        let missing = super::builder::missing_agent_tool_references(&[agent], &registry);

        assert_eq!(
            missing,
            vec![("coder".to_string(), vec!["ghost_tool".to_string()])]
        );
    }

    #[test]
    fn find_legacy_session_dirs_flags_prefixed_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(sessions.join("019fe564-15dd-7a40-af78-ed900edac08c")).unwrap();
        std::fs::create_dir_all(sessions.join("myclaw_s_019fe564-15dd-7a40-af78-ed900edac08d"))
            .unwrap();
        std::fs::create_dir_all(sessions.join(".legacy")).unwrap();
        std::fs::create_dir_all(sessions.join(".migration-backups")).unwrap();
        std::fs::write(sessions.join("active.json"), "{}").unwrap();

        let stale = super::builder::find_legacy_session_dirs(&sessions);
        assert_eq!(
            stale,
            vec!["myclaw_s_019fe564-15dd-7a40-af78-ed900edac08d".to_string()]
        );
    }

    #[test]
    fn find_legacy_session_dirs_empty_when_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let stale = super::builder::find_legacy_session_dirs(&dir.path().join("sessions"));
        assert!(stale.is_empty());
    }
}

// ── Hot-switch helpers ──────────────────────────────────────────────────────

/// Reset the persisted Telegram update offset so that `getUpdates` returns
/// recent messages instead of skipping everything the old process already
/// fetched.  The dedup layer in TelegramChannel will filter any duplicates.

// ── Sub-agent recovery (hot-switch detection) ─────────────────────────────────
// Moved to `src/agents/startup_recovery.rs` (#151 Phase 6: recovery family
// disambiguation; facade re-exports it via `agents::recovery`) so that
// Application-layer types (Orchestrator, OrchestratorParts) can reference
// `UnfinishedSubAgent` without depending on the Composition Root (`daemon`).
//
// Helpers used above:
//   crate::agents::recovery::scan_unfinished_subagents
