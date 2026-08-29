use crate::agents::{
    InMemoryBackend, McpManager, RunMode, SkillManager, SystemPromptConfig, ToolRegistry,
};
use anyhow::Context;
use std::sync::Arc;

use crate::channels::Channel;

pub(crate) fn build_registry(
    config: &crate::config::AppConfig,
) -> anyhow::Result<crate::providers::registry::Registry> {
    use crate::providers::{
        BuildChatProviderRequest, BuildEmbeddingProviderRequest, BuildImageProviderRequest,
        BuildSearchProviderRequest, BuildSttProviderRequest, BuildTtsProviderRequest,
        BuildVideoProviderRequest, CredentialPool, ProviderFactory, SharedApiKey,
        SharedCredentialPool,
    };
    use crate::providers::{ProviderId, detect_from_url, well_known};

    let factory = ProviderFactory::new();
    let mut registry = crate::providers::registry::Registry::from_config(
        config.providers.clone(),
        &config.routing,
    )
    .context("failed to build registry")?;

    for (provider_key, provider_cfg) in &config.providers {
        // Resolve provider identity: explicit override > base_url inference > generic
        let provider_id = provider_cfg
            .provider
            .as_ref()
            .map(|s| ProviderId::new(s.clone()))
            .or_else(|| {
                // Try to infer from the first capability's base_url
                provider_cfg
                    .chat
                    .as_ref()
                    .and_then(|c| detect_from_url(&c.base_url))
                    .or_else(|| {
                        provider_cfg
                            .embedding
                            .as_ref()
                            .and_then(|e| detect_from_url(&e.base_url))
                    })
                    .or_else(|| {
                        provider_cfg
                            .search
                            .as_ref()
                            .and_then(|s| detect_from_url(&s.base_url))
                    })
            })
            .unwrap_or_else(|| ProviderId::new("generic"));

        tracing::debug!(provider = %provider_key, id = %provider_id, "resolved provider identity");

        // ── Chat ──────────────────────────────────────────────────────
        if let Some(ref chat) = provider_cfg.chat {
            let api_keys = provider_cfg.effective_api_keys(chat.api_key.as_deref());
            anyhow::ensure!(!api_keys.is_empty(), "no API key for '{}'", provider_key);

            // Shared API key cell — all models under this provider share one cell.
            let shared_key = SharedApiKey::new(api_keys[0].clone());

            // Create a credential pool only when multiple keys are configured.
            let pool = if api_keys.len() > 1 {
                let p = CredentialPool::new(
                    provider_key.clone(),
                    api_keys.clone(),
                    provider_cfg.rotation_strategy,
                );
                let shared = SharedCredentialPool::new(p);
                tracing::info!(
                    provider = %provider_key,
                    key_count = api_keys.len(),
                    strategy = ?provider_cfg.rotation_strategy,
                    "multi-key credential pool created"
                );
                Some(shared)
            } else {
                None
            };

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
                    api_key: shared_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                    hosted_tools: chat.hosted_tools.clone(),
                };

                let chat_provider = factory.build_chat_provider(request).with_context(|| {
                    format!(
                        "cannot build chat provider for base_url '{}' (key='{}')",
                        chat.base_url, provider_key
                    )
                })?;

                registry.register_chat(
                    chat_provider,
                    model_id.clone(),
                    model_cfg.clone(),
                    Some(provider_id.clone()),
                    chat.protocol,
                );

                // Attach credential pool so the fallback chain can rotate keys.
                if let Some(ref pool) = pool {
                    registry.attach_credential_pool(model_id, pool.clone(), shared_key.clone());
                }
            }
        }

        // ── Embedding ─────────────────────────────────────────────────
        if let Some(ref emb) = provider_cfg.embedding {
            let api_key = provider_cfg.effective_api_key(emb.api_key.as_deref());
            let api_key =
                api_key.with_context(|| format!("no API key for '{}' embedding", provider_key))?;
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
            let is_edge_tts = provider_id.as_str() == well_known::EDGE_TTS;
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key = if is_edge_tts {
                // Edge TTS is free and needs no authentication.
                api_key.unwrap_or_default()
            } else {
                api_key.with_context(|| format!("no API key for '{}' tts", provider_key))?
            };
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
            let api_key =
                api_key.with_context(|| format!("no API key for '{}' video", provider_key))?;
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
            let api_keys = provider_cfg.effective_api_keys(sec.api_key.as_deref());
            anyhow::ensure!(
                !api_keys.is_empty(),
                "no API key for '{}' search",
                provider_key
            );

            let shared_key = SharedApiKey::new(api_keys[0].clone());

            let pool = if api_keys.len() > 1 {
                let p = CredentialPool::new(
                    provider_key.clone(),
                    api_keys.clone(),
                    provider_cfg.rotation_strategy,
                );
                let shared = SharedCredentialPool::new(p);
                tracing::info!(
                    provider = %provider_key,
                    key_count = api_keys.len(),
                    strategy = ?provider_cfg.rotation_strategy,
                    "multi-key credential pool created for search"
                );
                Some(shared)
            } else {
                None
            };

            let auth_style = provider_cfg.effective_auth_style(sec.auth_style);
            let user_agent = sec.user_agent.clone();

            for model_id in sec.models.keys() {
                let request = BuildSearchProviderRequest {
                    provider_key: provider_key.clone(),
                    provider_id: provider_id.clone(),
                    base_url: sec.base_url.clone(),
                    api_key: shared_key.clone(),
                    auth_style: auth_style.into(),
                    user_agent: user_agent.clone(),
                };

                if let Some(srch) = factory.build_search_provider(request) {
                    registry.register_search(srch, model_id.clone());

                    if let Some(ref pool) = pool {
                        registry.attach_credential_pool(model_id, pool.clone(), shared_key.clone());
                    }
                }
            }
        }

        // ── STT ───────────────────────────────────────────────────────
        if let Some(ref sec) = provider_cfg.stt {
            let api_key = provider_cfg.effective_api_key(sec.api_key.as_deref());
            let api_key =
                api_key.with_context(|| format!("no API key for '{}' stt", provider_key))?;
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_tools(
    mcp_manager: &McpManager,
    skills: &Arc<parking_lot::RwLock<SkillManager>>,
    shared_scheduler: &crate::agents::SharedScheduler,
    config: &crate::config::AppConfig,
    _memory_root: &str,
    user_resolver: &Arc<crate::agents::UserResolver>,
    ask_router: Arc<crate::agents::AskRouter>,
    known_users: &Arc<crate::agents::KnownUsersRegistry>,
    user_registry: &Arc<crate::agents::UserRegistry>,
    namespace: &str,
    session_backend: Arc<dyn crate::storage::SessionBackend>,
    // issue #129: where `background: true` shell commands report completion.
    shell_notice_tx: tokio::sync::mpsc::Sender<crate::tools::shell::ShellCompletion>,
) -> (
    ToolRegistry,
    Arc<crate::tools::TaskBoards>,
    Arc<crate::tools::SendMessageTool>,
    Arc<crate::tools::FriendToolsCtx>,
    crate::tools::shell::ShellRegistry,
    Arc<crate::tools::shell::ShellTool>,
) {
    let mut tools = ToolRegistry::new();
    let (builtin, shell_registry, shell_tool) =
        crate::tools::builtin_tools(Some(config.sessions_root()), Some(shell_notice_tx.clone()));
    for tool in builtin {
        tools.register(tool);
    }

    // Reconcile tracked shell processes against restart reality before any
    // tool call can reach the registry — see `tools::shell` module docs.
    // `myclaw restart` (SIGUSR1 hot switch) preserves shell children;
    // anything else (`myclaw stop`, systemctl, a crash) goes through this
    // deployment's `KillMode=control-group` and kills them along with the
    // daemon, so those entries are marked lost rather than probed for
    // liveness. A `background: true` entry still `running` across a hot
    // switch keeps its completion notice armed (issue #129).
    crate::tools::shell::adopt_after_restart(
        &config.sessions_root(),
        &shell_registry,
        crate::hot_switch::is_hot_switch(),
        Some(shell_notice_tx),
    )
    .await;

    // AskUserTool resolves the channel via `Session::resolve_channel()` at
    // execute time — no per-tool channels map. Bound to the shared
    // `AskRouter` so Orchestrator's inbound dispatch can fulfill its waits
    // via the same router instance.
    tools.register(Arc::new(crate::tools::AskUserTool::new(ask_router)));

    // Register additional built-in tools.
    // Keep the Arc to SendMessageTool: the daemon later wires the
    // agent-to-agent bus into it (multi-agent mode) via `set_messenger`.
    let send_message_tool = Arc::new(crate::tools::SendMessageTool::with_namespace(namespace));
    tools.register(Arc::clone(&send_message_tool) as Arc<dyn crate::providers::Tool>);
    tools.register(Arc::new(crate::tools::ListDirTool::new()));
    // Task tools — P1-B1: per-session boards at
    // `{sessions_root}/{uuid}/tasks.json` (the `Session` passed to execute
    // resolves the board; no process-global task state anymore).
    let task_boards = Arc::new(crate::tools::TaskBoards::new(
        config.sessions_root(),
        namespace,
    ));
    for tool in crate::tools::new_task_tools((*task_boards).clone()) {
        tools.register(tool);
    }

    // SkillTool — loads skill body on demand. Owner normalization via the
    // shared UserResolver (routing key → FQID) for user-layer shadowing.
    tools.register(Arc::new(crate::tools::SkillTool::new(
        Arc::clone(skills),
        Arc::clone(user_resolver),
    )));

    // SkillsListTool — lists skill metadata.
    tools.register(Arc::new(crate::tools::SkillsListTool::new(
        Arc::clone(skills),
        Arc::clone(user_resolver),
    )));

    // SkillManageTool — CRUD for skills. Writes only ever go to the owner's
    // user layer. `ctx.owner` arrives as the session routing key in daemon
    // tool execution, so the shared UserResolver is injected to normalize it
    // to the owner FQID before any user-layer path/registry lookup (issue
    // #101, same injection as the memory tools).
    // `agents_skills_dir_opt()` (issue #83) is passed just so a post-write
    // refresh doesn't drop the shared-library skills from the live
    // SkillManager.
    tools.register(Arc::new(crate::tools::SkillManageTool::new(
        Arc::clone(skills),
        Arc::clone(user_resolver),
        config.base_dir.join("users"),
        config.skills_root(),
        config.agents_skills_dir_opt(),
    )));

    // CronJobTool — manage scheduled cron jobs. #101 P2: the shared
    // UserResolver is injected so `create` can attribute the job to its
    // creator (routing key → FQID, same normalization as skill_manage).
    tools.register(Arc::new(crate::tools::CronJobTool::new(
        Arc::clone(shared_scheduler),
        Arc::clone(user_resolver),
    )));

    // Memory tools — P1-B2: single flat memory root ({base_dir}/memory),
    // ownership expressed via frontmatter `scope`/`user_id` (not by path).
    let kd = config.memory_root();
    let r = Arc::clone(user_resolver);
    tools.register(Arc::new(crate::tools::MemoryListTool::new(
        kd.clone(),
        Arc::clone(&r),
    )));
    tools.register(Arc::new(crate::tools::MemoryViewTool::new(
        kd.clone(),
        Arc::clone(&r),
    )));
    tools.register(Arc::new(crate::tools::MemorySearchTool::new(
        kd.clone(),
        Arc::clone(&r),
    )));
    tools.register(Arc::new(crate::tools::MemoryManageTool::new(
        kd,
        config.base_dir.clone(),
        r,
    )));

    // Session query tool — provenance lookup (session IDs + message IDs).
    tools.register(Arc::new(crate::tools::SessionQueryTool::new(
        Arc::clone(&session_backend),
        Arc::clone(user_resolver),
    )));

    // Friend tools (RFC §4.2) — main-agent only, share one ctx so the
    // daemon can inject the live ChannelRegistry for §4.3 notifications.
    // P4: UserRegistry 负责 `u/uid`/邮箱 → FQID 解析与显示名渲染。
    let friend_ctx = Arc::new(crate::tools::FriendToolsCtx::with_namespace(
        Arc::clone(known_users),
        Arc::clone(user_registry),
        namespace,
    ));
    tools.register(Arc::new(crate::tools::FriendRequestTool::new(Arc::clone(
        &friend_ctx,
    ))));
    tools.register(Arc::new(crate::tools::FriendAcceptTool::new(Arc::clone(
        &friend_ctx,
    ))));
    tools.register(Arc::new(crate::tools::FriendDeclineTool::new(Arc::clone(
        &friend_ctx,
    ))));
    tools.register(Arc::new(crate::tools::FriendListTool::new(Arc::clone(
        &friend_ctx,
    ))));

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
    (
        tools,
        task_boards,
        send_message_tool,
        friend_ctx,
        shell_registry,
        shell_tool,
    )
}

/// Build SkillManager from SKILL.md files in the base dir (P1: `{base_dir}/skills`)
/// layered with the cross-agent shared library `~/.agents/skills` when
/// enabled (`[skills] include_agents_dir`, default on — issue #83).
pub(crate) fn build_skill_manager(config: &crate::config::AppConfig) -> SkillManager {
    let mut manager = SkillManager::new();
    let skills_dir = config.skills_root();
    let agents_dir = config.agents_skills_dir_opt();

    let base_dir = config.base_dir.clone();
    let users_dir = base_dir.join("users");

    let user_skills_map = crate::agents::skill_loader::load_all_users_skills(&users_dir);
    let agent_defs = crate::agents::skill_loader::load_skills_from_dir(&skills_dir);
    let shared_defs = if let Some(d) = &agents_dir {
        crate::agents::skill_loader::load_skills_from_dir(d)
    } else {
        Vec::new()
    };

    manager.reload_from_definitions(user_skills_map, agent_defs, shared_defs);
    tracing::info!(skill_count = manager.skill_count(), "skill manager built");
    manager
}

/// Build sub-agent configs from AGENT.md files in workspace.
///
/// Sub-agents are defined in `workspace/agents/<name>/AGENT.md` — each file
/// contains YAML front matter (metadata) and Markdown body (system prompt).
pub(crate) fn build_sub_agents(
    config: &crate::config::AppConfig,
) -> Vec<crate::config::sub_agent::SubAgentConfig> {
    let agents_dir = config.agents_root();
    let agents = crate::agents::agent_loader::load_agents_from_dir(&agents_dir);
    if !agents.is_empty() {
        tracing::info!(
            agent_count = agents.len(),
            "sub-agents loaded from base dir"
        );
    }
    agents
}

pub(crate) fn missing_agent_tool_references(
    agents: &[crate::config::sub_agent::SubAgentConfig],
    registry: &ToolRegistry,
) -> Vec<(String, Vec<String>)> {
    use crate::config::filters::{AllKeyword, NameFilter};
    use std::collections::HashSet;

    let available: HashSet<String> = registry.tool_names_sorted().into_iter().collect();
    let mut missing = Vec::new();

    for agent in agents {
        let referenced: Vec<String> = match &agent.tools {
            NameFilter::AllKeyword(AllKeyword(list)) => list
                .iter()
                .filter(|name| name.as_str() != "all")
                .cloned()
                .collect(),
            NameFilter::Allow(list) => list.clone(),
            NameFilter::Deny(deny) => deny.except.clone(),
        };

        let agent_missing: Vec<String> = referenced
            .into_iter()
            .filter(|name| !available.contains(name))
            .collect();
        if !agent_missing.is_empty() {
            missing.push((agent.name.clone(), agent_missing));
        }
    }

    missing
}

pub(crate) fn warn_missing_agent_tool_references(
    agents: &[crate::config::sub_agent::SubAgentConfig],
    registry: &ToolRegistry,
) {
    for (agent, missing_tools) in missing_agent_tool_references(agents, registry) {
        for tool in &missing_tools {
            tracing::warn!(
                agent = %agent,
                tool = %tool,
                "AGENT.md references unregistered tool; will fail at runtime as Unknown tool"
            );
        }
        tracing::warn!(
            agent = %agent,
            missing_tool_count = missing_tools.len(),
            "AGENT.md tool references missing from registry"
        );
    }
}

/// List top-level directory names under `sessions_dir` that are neither a
/// bare uuid nor a known archive dir — i.e. session directories still on
/// the pre-P1-A `myclaw_s_<uuid>` naming. Missing `sessions_dir` yields no
/// results (nothing to flag before it's ever been created).
///
/// Two archive dirs are intentional and excluded: `.legacy` (migrate-layout.py's
/// P1 layout migration, B11) and `.migration-backups` (this crate's own
/// in-process RFC §6 namespace/FQID migration, `src/migration.rs` — a
/// completely separate migration system from the Python script, predating it).
pub(crate) fn find_legacy_session_dirs(sessions_dir: &std::path::Path) -> Vec<String> {
    const KNOWN_ARCHIVE_DIRS: &[&str] = &[".legacy", ".migration-backups"];
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if KNOWN_ARCHIVE_DIRS.contains(&name.as_str()) || uuid::Uuid::parse_str(&name).is_ok() {
                None
            } else {
                Some(name)
            }
        })
        .collect()
}

/// Build the session backend (shared with SessionManager and persist hooks).
pub(crate) fn build_session_backend(
    config: &crate::config::AppConfig,
) -> Arc<dyn crate::storage::SessionBackend> {
    let sessions_dir = config.sessions_root();
    match crate::storage::JsonFileBackend::open_with_namespace(
        &sessions_dir,
        &config.system.namespace,
    ) {
        Ok(backend) => {
            tracing::info!(path = %sessions_dir.display(), "session storage opened");
            match backend.migrate_global_message_ids() {
                Ok(n) if n > 0 => tracing::info!(
                    migrated = n,
                    "session storage migrated to global message IDs"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "session storage migration failed"),
            }
            Arc::new(backend)
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to open session storage, falling back to in-memory");
            Arc::new(InMemoryBackend::with_namespace(&config.system.namespace))
        }
    }
}

/// Build Channel adapters from config, returning (channel_type, account_id, channel).
pub(crate) fn build_channel_accounts(
    config: &crate::config::AppConfig,
) -> Vec<(String, String, Arc<dyn Channel>)> {
    let mut channels: Vec<(String, String, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref cfg) = config.channels.telegram {
        if cfg.enabled {
            for (account_id, account_cfg) in &cfg.accounts {
                if account_cfg.enabled {
                    channels.push((
                        "telegram".to_string(),
                        account_id.clone(),
                        Arc::new(
                            crate::channels::telegram::TelegramChannel::new(account_cfg.clone())
                                .with_base_dir(config.base_dir.clone()),
                        ),
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
                        Arc::new(crate::channels::wechat::WechatChannel::new(
                            account_id.clone(),
                            account_cfg.clone(),
                        )),
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
                        Arc::new(crate::channels::qqbot::QQBotChannel::new(
                            account_id.clone(),
                            account_cfg.clone(),
                        )),
                    ));
                }
            }
        }
    }

    channels
}

/// Convert config sections into Application-layer prompt config.
pub(crate) fn build_prompt_config(
    agent: &crate::config::agent::AgentConfig,
    prompt: &crate::config::agent::PromptConfig,
    base_dir: &std::path::Path,
    workspace_dir: &std::path::Path,
    memory_root: &std::path::Path,
) -> SystemPromptConfig {
    SystemPromptConfig {
        base_dir: base_dir.to_string_lossy().to_string(),
        workspace_dir: workspace_dir.to_string_lossy().to_string(),
        memory_root: memory_root.to_string_lossy().to_string(),
        permission_mode: agent.permission_mode,
        run_mode: RunMode::Interactive,
        max_chars: prompt.max_chars,
        native_tools: prompt.native_tools,
        identity_header: None,
    }
}
