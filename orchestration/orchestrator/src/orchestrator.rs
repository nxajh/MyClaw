//! Orchestrator — connects channels and agent loops.

use anyhow::Context;
use channels::{Channel, ChannelMessage, SendMessage};
use dashmap::DashMap;
use runtime::{Agent, AgentConfig, AgentLoop, SessionManager, SkillsManager};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

const CHANNEL_QUEUE_SIZE: usize = 100;

// ═══════════════════════════════════════════════════════════════════════════════
// Orchestrator
// ═══════════════════════════════════════════════════════════════════════════════

pub struct Orchestrator {
    /// Channels, keyed by name (e.g. "telegram", "wechat").
    channels: Arc<DashMap<String, Arc<dyn Channel>>>,
    /// Per-session agent loops: "channel:sender" → Arc<Mutex<AgentLoop>>.
    /// Wrapped in Arc so spawned tasks don't borrow &self Orchestrator.
    sessions: Arc<DashMap<String, Arc<TokioMutex<AgentLoop>>>>,
    agent: Agent,
    session_manager: SessionManager,
    /// The message receiver, owned and consumed by run().
    /// Wrapped so run() can extract it without conflicting with Arc'd fields.
    #[allow(clippy::type_complexity)]
    msg_rx: Arc<TokioMutex<Option<mpsc::Receiver<(String, ChannelMessage)>>>>,
    /// Listener task handles — taken and awaited on shutdown.
    listener_handles: Vec<JoinHandle<()>>,
}

impl Orchestrator {
    /// Build and return (Orchestrator, sender_for_parent).
    pub fn new(
        config: &config::AppConfig,
    ) -> anyhow::Result<(Self, mpsc::Sender<(String, ChannelMessage)>)> {
        let (msg_tx, msg_rx) = mpsc::channel(CHANNEL_QUEUE_SIZE);
        let msg_tx = Arc::new(msg_tx);

        let mut registry =
            registry::Registry::from_config(config.providers.clone(), &config.routing)
                .context("failed to build registry")?;

        for (provider_key, provider_cfg) in &config.providers.clone() {
            let api_key =
                provider_cfg
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

                // Determine which capabilities this model needs.
                // Each capability may require a separate trait object, but multiple
                // capabilities backed by the same concrete type (e.g. OpenAiProvider
                // implements Chat + Embedding + Image + TTS) share one instance.
                //
                // Strategy: create one ProviderHandle per provider key (not per model),
                // then extract trait objects for each capability the model declares.
                // For now, we create a handle per model to keep things simple.

                // Chat is always needed — register via the ProviderHandle pattern.
                let handle = providers::ProviderHandle::from_url(
                    api_key.clone(),
                    &provider_cfg.base_url,
                ).with_context(|| format!(
                    "cannot determine provider type from base_url '{}' (key='{}')",
                    provider_cfg.base_url, provider_key
                ))?;

                // For each capability, clone the handle and extract the trait object.
                // ProviderHandle consumes self, so we recreate it each time.
                use config::provider::Capability as CfgCap;

                for cap in &model_cfg.capabilities {
                    match cap {
                        CfgCap::Chat | CfgCap::Vision | CfgCap::NativeTools => {
                            // Chat is always registered below; skip here to avoid
                            // double-registering with a fresh handle.
                        }
                        CfgCap::Embedding => {
                            if let Some(emb) = providers::ProviderHandle::from_url(
                                api_key.clone(), &provider_cfg.base_url,
                            ).and_then(|h| h.into_embedding_provider()) {
                                registry.register_embedding(emb, model_id.clone());
                            }
                        }
                        CfgCap::ImageGeneration => {
                            if let Some(img) = providers::ProviderHandle::from_url(
                                api_key.clone(), &provider_cfg.base_url,
                            ).and_then(|h| h.into_image_provider()) {
                                registry.register_image(img, model_id.clone());
                            }
                        }
                        CfgCap::TextToSpeech => {
                            if let Some(tts) = providers::ProviderHandle::from_url(
                                api_key.clone(), &provider_cfg.base_url,
                            ).and_then(|h| h.into_tts_provider()) {
                                registry.register_tts(tts, model_id.clone());
                            }
                        }
                        CfgCap::VideoGeneration => {
                            if let Some(vid) = providers::ProviderHandle::from_url(
                                api_key.clone(), &provider_cfg.base_url,
                            ).and_then(|h| h.into_video_provider()) {
                                registry.register_video(vid, model_id.clone());
                            }
                        }
                        CfgCap::Search => {
                            if let Some(srch) = providers::ProviderHandle::from_url(
                                api_key.clone(), &provider_cfg.base_url,
                            ).and_then(|h| h.into_search_provider()) {
                                registry.register_search(srch, model_id.clone());
                            }
                        }
                        CfgCap::SpeechToText => {
                            if let Some(stt) = providers::ProviderHandle::from_url(
                                api_key.clone(), &provider_cfg.base_url,
                            ).and_then(|h| h.into_stt_provider()) {
                                registry.register_stt(stt, model_id.clone());
                            }
                        }
                    }
                }

                // Always register ChatProvider (every model with Chat/Vision/NativeTools needs it)
                let chat_provider: Box<dyn providers::ChatProvider> = handle.into_chat_provider();
                registry.register_chat(chat_provider, model_id.clone());
            }
        }

        let mut skills = SkillsManager::new();

        // Register built-in tools.
        let builtin = tools::builtin_tools_with_memory(tools::MemoryStore::new());
        for tool in builtin {
            let name = tool.name().to_string();
            skills.register_tool(&name, tool);
        }
        tracing::info!(tool_count = skills.tool_count(), "builtin tools registered");

        let agent_config = AgentConfig {
            max_tool_calls: config.agent.max_tool_calls,
            max_history: config.agent.max_history,
            prompt_config: system_prompt_config_from(&config.agent.prompt),
        };
        let agent = Agent::new(Arc::new(registry), skills, agent_config);

        // Session persistence: SQLite in workspace dir.
        let db_path = config.workspace_dir.join("sessions.db");
        let session_backend = match memory_storage::SqliteSessionBackend::open(&db_path.to_string_lossy()) {
            Ok(db) => {
                tracing::info!(path = %db_path.display(), "session database opened");
                Arc::new(db) as Arc<dyn session::SessionBackend>
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to open session database, falling back to in-memory");
                Arc::new(runtime::InMemoryBackend::new()) as Arc<dyn session::SessionBackend>
            }
        };
        let session_manager = SessionManager::new(session_backend);

        let channels = Arc::new(DashMap::new());
        let sessions = Arc::new(DashMap::new());

        let mut orchestrator = Orchestrator {
            channels: channels.clone(),
            sessions: sessions.clone(),
            agent,
            session_manager,
            msg_rx: Arc::new(TokioMutex::new(Some(msg_rx))),
            listener_handles: Vec::new(),
        };

        orchestrator.spawn_channel_listeners(config, msg_tx.clone())?;
        info!(channels = orchestrator.channels.len(), "orchestrator initialized");

        Ok((orchestrator, (*msg_tx).clone()))
    }

    fn spawn_channel_listeners(
        &mut self,
        config: &config::AppConfig,
        msg_tx: Arc<mpsc::Sender<(String, ChannelMessage)>>,
    ) -> anyhow::Result<()> {
        if let Some(ref cfg) = config.channels.telegram {
            if cfg.enabled {
                let ch: Arc<dyn Channel> = Arc::new(
                    channels::telegram::TelegramChannel::new(cfg.clone())
                );
                let handle = self.spawn_listener("telegram", ch.clone(), msg_tx.clone());
                self.channels.insert("telegram".into(), ch);
                self.listener_handles.push(handle);
                info!("telegram listener started");
            }
        }

        if let Some(ref cfg) = config.channels.wechat {
            if cfg.enabled {
                let ch: Arc<dyn Channel> = Arc::new(
                    channels::wechat::WechatChannel::new(cfg.clone())
                );
                let handle = self.spawn_listener("wechat", ch.clone(), msg_tx.clone());
                self.channels.insert("wechat".into(), ch);
                self.listener_handles.push(handle);
                info!("wechat listener started");
            }
        }

        if self.channels.is_empty() {
            warn!("no channels enabled");
        }

        Ok(())
    }

    fn spawn_listener(
        &self,
        channel_name: &'static str,
        channel: Arc<dyn Channel>,
        msg_tx: Arc<mpsc::Sender<(String, ChannelMessage)>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let rx = match channel.listen().await {
                Ok(r) => r,
                Err(e) => {
                    error!(channel = %channel_name, err = %e, "listen failed");
                    return;
                }
            };
            let mut rx = rx;
            while let Some(msg) = rx.recv().await {
                if msg_tx.send((channel_name.to_string(), msg)).await.is_err() {
                    break;
                }
            }
            info!(channel = %channel_name, "listener ended");
        })
    }

    fn session_key(channel: &str, sender: &str) -> String {
        format!("{channel}:{sender}")
    }

    fn get_or_create_loop(sessions: &DashMap<String, Arc<TokioMutex<AgentLoop>>>, agent: &Agent, session_manager: &SessionManager, sk: &str) -> Arc<TokioMutex<AgentLoop>> {
        if let Some(existing) = sessions.get(sk) {
            return existing.clone();
        }
        let session = session_manager.get_or_create(sk);
        let loop_ = agent.loop_for(session);
        let entry: Arc<TokioMutex<AgentLoop>> = Arc::new(TokioMutex::new(loop_));
        sessions.insert(sk.into(), entry.clone());
        entry
    }

    /// Main message loop. Consumes self.msg_rx.
    pub async fn run(&self) -> anyhow::Result<()> {
        use channels::Channel;

        let rx = {
            let mut guard = self.msg_rx.lock().await;
            guard.take().context("run() already called or msg_rx was None")?
        };

        let sessions = self.sessions.clone();
        let agent = self.agent.clone();
        let channels = self.channels.clone();

        let mut rx = rx;

        loop {
            if crate::daemon::is_shutdown_requested() {
                tracing::info!("shutdown requested, exiting message loop");
                break;
            }

            let msg = tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => m,
                    None => break,
                },
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => continue,
            };

            let (channel_name, msg) = msg;
            let sk = Self::session_key(&channel_name, &msg.sender);
            let content = msg.content.clone();
            let reply_target = msg.reply_target.clone();
            let channel_name_clone = channel_name.clone();
            let loop_ = Self::get_or_create_loop(&sessions, &agent, &self.session_manager, &sk);

            // Clone Arc for each task so the move into async block doesn't affect later iterations.
            let ch = channels.clone();
            tokio::spawn(async move {
                let response = {
                    let mut guard = loop_.lock().await;
                    guard.run(&content).await
                };

                let channel: Option<Arc<dyn Channel>> = {
                    ch.get(&channel_name_clone).map(|r| r.clone())
                };

                let channel = match channel {
                    Some(c) => c,
                    None => return,
                };

                match response {
                    Ok(text) if !text.is_empty() => {
                        tracing::info!(session = %sk, text_len = text.len(), "sending response");
                        let send_msg = SendMessage {
                            recipient: reply_target,
                            content: text,
                            subject: None,
                            thread_ts: None,
                            cancellation_token: None,
                            attachments: vec![],
                            image_urls: None,
                        };
                        if let Err(e) = channel.send(&send_msg).await {
                            error!(session = %sk, err = %e, "send failed");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!(session = %sk, err = %e, "loop failed");
                    }
                }
            });
        }

        info!("all listeners stopped, exiting");
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SystemPromptConfig conversion
// ═══════════════════════════════════════════════════════════════════════════════

use runtime::SystemPromptConfig;

fn system_prompt_config_from(cfg: &config::agent::PromptConfig) -> SystemPromptConfig {
    SystemPromptConfig {
        workspace_dir: String::new(),
        model_name: cfg.model_name.clone().unwrap_or_default(),
        autonomy: convert_autonomy(config::agent::AutonomyLevel::Default),
        skills_mode: runtime::SkillsPromptInjectionMode::Compact,
        compact: cfg.compact,
        max_chars: cfg.max_chars,
        bootstrap_max_chars: cfg.bootstrap_max_chars,
        native_tools: cfg.native_tools,
        channel_name: cfg.channel_name.clone(),
        host_info: None,
    }
}

fn convert_autonomy(level: config::agent::AutonomyLevel) -> runtime::AutonomyLevel {
    use config::agent::AutonomyLevel as Ca;
    use runtime::AutonomyLevel as Ra;
    match level {
        Ca::Full => Ra::Full,
        Ca::Default => Ra::Default,
        Ca::ReadOnly => Ra::ReadOnly,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Shutdown support
// ═══════════════════════════════════════════════════════════════════════════════


impl Orchestrator {
    /// Take ownership of listener handles for graceful shutdown.
    /// Returns None if already taken.
    pub fn take_listener_handles(&mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.listener_handles)
    }

    /// Abort all listener handles (call after run() returns).
    pub async fn shutdown_listeners(&mut self) {
        let handles = self.take_listener_handles();
        for h in handles {
            h.abort();
        }
        tracing::info!("all listener tasks aborted");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    #[test]
    fn test_session_key() {
        assert_eq!(
            super::Orchestrator::session_key("wechat", "o9cq80zXpSX1Hz0ph_QNs591k4PA"),
            "wechat:o9cq80zXpSX1Hz0ph_QNs591k4PA"
        );
    }
}
