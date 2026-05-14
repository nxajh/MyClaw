//! config — Central configuration for MyClaw.
//!
//! Defines all configuration types and provides TOML file loading with
//! environment variable expansion (`${ENV_VAR}`).
//!
//! # Configuration file structure
//!
//! ```toml
//! workspace_dir = "~/.myclaw/workspace"
//!
//! [providers.openai]
//! api_key = "${OPENAI_API_KEY}"
//!
//! [providers.openai.chat]
//! base_url = "https://api.openai.com/v1"
//!
//! [providers.openai.chat.models.gpt-4o]
//! input = ["text", "image"]
//! output = ["text"]
//! context_window = 128000
//! max_output_tokens = 16384
//!
//! [providers.openai.embedding]
//! base_url = "https://api.openai.com/v1"
//!
//! [providers.openai.embedding.models.text-embedding-3-small]
//! dimensions = 1536
//!
//! [routing.chat]
//! strategy = "fallback"
//! models = ["gpt-4o"]
//!
//! [channels.wechat]
//! api_base = "https://ilink.bot.weixin.qq.com"
//! bot_token = "${WECHAT_BOT_TOKEN}"
//!
//! [channels.telegram]
//! bot_token = "${TELEGRAM_BOT_TOKEN}"
//! allowed_users = ["*"]
//!
//! [agent]
//! autonomy_level = "default"
//!
//! [memory]
//! storage = "sqlite"
//!
//! [[mcp_servers]]
//! name = "filesystem"
//! command = "npx"
//! args = ["mcp-server-filesystem"]
//! ```

pub mod agent;
pub mod channel;
pub mod mcp;
pub mod memory;
pub mod provider;
pub mod routing;
pub mod scheduler;
pub mod sub_agent;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use agent::AgentConfig;
use channel::ChannelConfigs;
use mcp::McpServerConfig;
use memory::MemoryConfig;
use provider::ProviderConfig;
use routing::RoutingConfig;

// ── Defaults ──────────────────────────────────────────────────────────────────

fn default_defaults_model() -> String {
    "minimax-m2.7".to_string()
}

// ── Defaults section ──────────────────────────────────────────────────────────

/// Global default settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    /// Default model for routing.
    #[serde(default = "default_defaults_model")]
    pub model: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            model: default_defaults_model(),
        }
    }
}

// ── RawConfig (1:1 mapping of TOML file) ──────────────────────────────────────

/// Raw configuration as parsed from TOML — before env var expansion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawConfig {
    /// Workspace directory path.
    #[serde(default)]
    workspace_dir: Option<String>,

    /// 知识库目录路径。默认为 {workspace_dir}/memory。
    #[serde(default)]
    knowledge_dir: Option<String>,

    /// Provider configurations.
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,

    /// Routing configuration.
    #[serde(default)]
    routing: RoutingConfig,

    /// Channel configurations.
    #[serde(default)]
    channels: ChannelConfigs,

    /// Agent configuration.
    #[serde(default)]
    agent: AgentConfig,

    /// Memory configuration.
    #[serde(default)]
    memory: MemoryConfig,

    /// MCP server configurations.
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,

    /// Global defaults.
    #[serde(default)]
    defaults: Defaults,

    /// Logging configuration.
    #[serde(default)]
    logging: LoggingConfig,
}

// ── LoggingConfig ─────────────────────────────────────────────────────────────

/// Logging configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Default log level.
    #[serde(default)]
    pub level: Option<String>,

    /// Per-module log levels.
    #[serde(default)]
    pub modules: HashMap<String, String>,
}

// ── AppConfig (public, resolved) ──────────────────────────────────────────────

/// Fully resolved application configuration.
///
/// This is the main config type consumed by all subsystems.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Workspace directory (absolute path).
    pub workspace_dir: PathBuf,
    /// Knowledge directory (absolute path). Defaults to {workspace_dir}/memory.
    pub knowledge_dir: PathBuf,
    /// Path to the config file.
    pub config_path: PathBuf,
    /// Provider configurations.
    pub providers: HashMap<String, ProviderConfig>,
    /// Routing configuration.
    pub routing: RoutingConfig,
    /// Channel configurations.
    pub channels: ChannelConfigs,
    /// Agent configuration.
    pub agent: AgentConfig,
    /// Memory configuration.
    pub memory: MemoryConfig,
    /// MCP server configurations.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Global defaults.
    pub defaults: Defaults,
    /// Logging configuration.
    pub logging: LoggingConfig,
}

// ── ConfigLoader ──────────────────────────────────────────────────────────────

/// Loads and resolves configuration from TOML files.
pub struct ConfigLoader;

impl ConfigLoader {
    /// Load configuration from a TOML file.
    ///
    /// Resolves `~` in paths and `${ENV_VAR}` in string values.
    pub fn from_file(path: impl AsRef<Path>) -> Result<AppConfig> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let raw: RawConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Self::resolve(raw, path.to_path_buf())
    }

    /// Load from a TOML string (useful for testing).
    pub fn from_toml(content: &str) -> Result<AppConfig> {
        let raw: RawConfig = toml::from_str(content)
            .context("Failed to parse config string")?;
        Self::resolve(raw, PathBuf::new())
    }

    /// Resolve a raw config into an AppConfig.
    fn resolve(mut raw: RawConfig, config_path: PathBuf) -> Result<AppConfig> {
        // Expand environment variables in all string fields.
        Self::expand_env_vars(&mut raw);

        // Resolve workspace_dir.
        let workspace_dir = raw.workspace_dir
            .map(|dir| Self::expand_path(&dir))
            .unwrap_or_else(|| {
                directories::ProjectDirs::from("", "", "myclaw")
                    .map(|d| d.data_dir().join("workspace"))
                    .unwrap_or_else(|| PathBuf::from(".myclaw/workspace"))
            });

        // Resolve knowledge_dir: explicit path or default to {workspace_dir}/memory.
        let knowledge_dir = raw.knowledge_dir
            .map(|d| Self::expand_path(&d))
            .unwrap_or_else(|| workspace_dir.join("memory"));

        Ok(AppConfig {
            workspace_dir,
            knowledge_dir,
            config_path,
            providers: raw.providers,
            routing: raw.routing,
            channels: raw.channels,
            agent: raw.agent,
            memory: raw.memory,
            mcp_servers: raw.mcp_servers,
            defaults: raw.defaults,
            logging: raw.logging,
        })
    }

    /// Expand `${ENV_VAR}` patterns in all string fields.
    fn expand_env_vars(config: &mut RawConfig) {
        // Expand provider-level and capability-level api_keys.
        for provider in config.providers.values_mut() {
            if let Some(ref key) = provider.api_key {
                provider.api_key = Some(Self::expand_string(key));
            }
            // Expand capability-level api_keys
            if let Some(ref mut chat) = provider.chat {
                if let Some(ref key) = chat.api_key {
                    chat.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut emb) = provider.embedding {
                if let Some(ref key) = emb.api_key {
                    emb.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.image_generation {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.tts {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.stt {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.video {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
            if let Some(ref mut sec) = provider.search {
                if let Some(ref key) = sec.api_key {
                    sec.api_key = Some(Self::expand_string(key));
                }
            }
        }

        // Expand channel tokens (iterate over accounts).
        if let Some(ref mut wechat) = config.channels.wechat {
            for (_, account) in wechat.accounts.iter_mut() {
                if let Some(ref token) = account.bot_token {
                    account.bot_token = Some(Self::expand_string(token));
                }
                if let Some(ref key) = account.aes_key {
                    account.aes_key = Some(Self::expand_string(key));
                }
            }
        }
        if let Some(ref mut tg) = config.channels.telegram {
            for (_, account) in tg.accounts.iter_mut() {
                account.bot_token = Self::expand_string(&account.bot_token);
            }
        }
    }

    /// Expand `${VAR}` and `$VAR` patterns in a string.
    fn expand_string(s: &str) -> String {
        shellexpand::env(s).unwrap_or_else(|_| s.into()).to_string()
    }

    /// Expand `~` and create absolute path.
    fn expand_path(path: &str) -> PathBuf {
        let expanded = shellexpand::tilde(path).to_string();
        PathBuf::from(expanded)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Capability;

    #[test]
    fn parse_minimal_config() {
        let toml_str = r#"
[providers.openai]

[providers.openai.chat]
base_url = "https://api.openai.com/v1"

[providers.openai.chat.models.gpt-4o]
input = ["text"]
output = ["text"]

[routing.chat]
strategy = "fixed"
models = ["gpt-4o"]
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();
        assert!(config.providers.contains_key("openai"));
        assert_eq!(config.routing.len(), 1);
        assert!(config.routing.get(Capability::Chat).is_some());
    }

    #[test]
    fn parse_empty_config() {
        let config = ConfigLoader::from_toml("").unwrap();
        assert!(config.providers.is_empty());
        assert!(config.channels.enabled_channels().is_empty());
        assert_eq!(config.agent.autonomy_level, agent::AutonomyLevel::Default);
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
workspace_dir = "/tmp/myclaw"

[providers.openai]
api_key = "test-key"

[providers.openai.chat]
base_url = "https://api.openai.com/v1"

[providers.openai.chat.models.gpt-4o]
input = ["text", "image"]
output = ["text"]
max_output_tokens = 16384
context_window = 128000

[providers.openai.chat.models.gpt-4o.pricing]
input = 2.5
output = 10.0

[providers.openai.embedding]
base_url = "https://api.openai.com/v1"

[providers.openai.embedding.models.text-embedding-3-small]
dimensions = 1536

[routing.chat]
strategy = "fallback"
models = ["gpt-4o"]

[channels.wechat]
enabled = true

[channels.wechat.accounts.default]
api_base = "https://ilink.bot.weixin.qq.com"
bot_token = "test-wechat-token"
allowed_users = ["wxid_abc123"]

[channels.telegram]
enabled = true

[channels.telegram.accounts.default]
bot_token = "test-telegram-token"
allowed_users = ["*"]

[agent]
max_tool_calls = 50
autonomy_level = "full"

[agent.prompt]
compact = true

[memory]
storage = "sqlite"
db_path = "test.db"

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["mcp-server-filesystem"]

[defaults]
model = "gpt-4o"

[logging]
level = "INFO"
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();

        // Workspace
        assert_eq!(config.workspace_dir, PathBuf::from("/tmp/myclaw"));

        // Providers
        assert_eq!(config.providers.len(), 1);
        let openai = &config.providers["openai"];
        assert!(openai.chat.is_some());
        let chat = openai.chat.as_ref().unwrap();
        assert_eq!(chat.base_url, "https://api.openai.com/v1");
        assert!(chat.models.contains_key("gpt-4o"));

        // Routing
        let chat_route = config.routing.get(crate::providers::Capability::Chat).unwrap();
        assert_eq!(chat_route.models, vec!["gpt-4o"]);

        // Channels
        assert_eq!(config.channels.enabled_channels(), vec!["wechat", "telegram"]);

        // Agent
        assert_eq!(config.agent.max_tool_calls, 50);

        // Memory
        assert_eq!(config.memory.db_path, "test.db");

        // MCP
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "filesystem");

        // Logging
        assert_eq!(config.logging.level.as_deref(), Some("INFO"));
    }

    #[test]
    fn env_var_expansion() {
        unsafe { std::env::set_var("TEST_MYCLAW_KEY", "secret123"); }

        let toml_str = r#"
[providers.test]

[providers.test.chat]
base_url = "https://api.test.com"
api_key = "${TEST_MYCLAW_KEY}"

[providers.test.chat.models.test-model]
input = ["text"]
output = ["text"]
"#;
        let config = ConfigLoader::from_toml(toml_str).unwrap();
        assert_eq!(
            config.providers["test"].chat.as_ref().unwrap().api_key.as_deref(),
            Some("secret123")
        );

        unsafe { std::env::remove_var("TEST_MYCLAW_KEY"); }
    }
}
