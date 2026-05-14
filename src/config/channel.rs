//! Channel adapter configurations.
//!
//! Each channel type supports multiple accounts (bot instances).
//! Configuration uses an `accounts` map keyed by account ID.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── WechatAccountConfig ──────────────────────────────────────────────────────

/// WeChat iLink Bot account-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WechatAccountConfig {
    /// iLink Bot API base URL.
    pub api_base: String,
    /// Bot token (if pre-authenticated; supports `${ENV_VAR}` expansion).
    pub bot_token: Option<String>,
    /// AES key for message encryption (supports `${ENV_VAR}` expansion).
    pub aes_key: Option<String>,
    /// Long-poll timeout in seconds.
    #[serde(default = "default_poll_timeout")]
    pub poll_timeout: u64,
    /// Allowed WeChat user IDs (`["*"]` = all).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Whether this account is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_poll_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

impl Default for WechatAccountConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            bot_token: None,
            aes_key: None,
            poll_timeout: default_poll_timeout(),
            allowed_users: Vec::new(),
            enabled: true,
        }
    }
}

// ── WechatChannelConfig ──────────────────────────────────────────────────────

/// WeChat channel-level configuration (contains multiple accounts).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WechatChannelConfig {
    /// Whether this channel type is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Account instances keyed by account ID.
    #[serde(default)]
    pub accounts: HashMap<String, WechatAccountConfig>,
}

// ── TelegramAccountConfig ────────────────────────────────────────────────────

/// Telegram Bot API account-level configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramAccountConfig {
    /// Telegram Bot API token (supports `${ENV_VAR}` expansion).
    pub bot_token: String,
    /// Allowed Telegram usernames or user IDs (`["*"]` = all).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Only respond when @mentioned in groups.
    #[serde(default)]
    pub mention_only: bool,
    /// Override the Telegram Bot API base URL (for local Bot API servers).
    pub api_base: Option<String>,
    /// Per-account proxy URL.
    pub proxy_url: Option<String>,
    /// Whether this account is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Approval prompt timeout in seconds.
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_secs: u64,
    /// Send acknowledgement reactions on received messages.
    #[serde(default = "default_true")]
    pub ack_reactions: bool,
    /// Workspace directory for saving downloaded attachments.
    pub workspace_dir: Option<String>,
    /// Stall watchdog threshold in seconds. Send "still thinking" message if typing exceeds this.
    /// Set to 0 to disable.
    #[serde(default = "default_stall_timeout_secs")]
    pub stall_timeout_secs: u64,
    /// Debounce window in milliseconds for merging rapid consecutive messages from the same sender.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_debounce_ms() -> u64 {
    2000
}

fn default_stall_timeout_secs() -> u64 {
    30
}

fn default_approval_timeout() -> u64 {
    120
}

// ── TelegramChannelConfig ────────────────────────────────────────────────────

/// Telegram channel-level configuration (contains multiple accounts).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    /// Whether this channel type is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Account instances keyed by account ID.
    #[serde(default)]
    pub accounts: HashMap<String, TelegramAccountConfig>,
}

// ── QQBotAccountConfig ───────────────────────────────────────────────────────

/// QQ Bot account-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QQBotAccountConfig {
    /// Whether this account is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// QQ Bot App ID.
    pub app_id: String,
    /// QQ Bot Client Secret.
    pub client_secret: String,
    /// Allowed user OpenIDs for private chat. None = allow all.
    #[serde(default)]
    pub allow_from: Option<Vec<String>>,
    /// Allowed group OpenIDs. None = allow all groups.
    #[serde(default)]
    pub group_allow_from: Option<Vec<String>>,
}

// ── QQBotChannelConfig ───────────────────────────────────────────────────────

/// QQ Bot channel-level configuration (contains multiple accounts).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QQBotChannelConfig {
    /// Whether this channel type is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Account instances keyed by account ID.
    #[serde(default)]
    pub accounts: HashMap<String, QQBotAccountConfig>,
}

// ── ChannelConfigs ────────────────────────────────────────────────────────────

/// All channel configurations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelConfigs {
    pub wechat: Option<WechatChannelConfig>,
    pub telegram: Option<TelegramChannelConfig>,
    pub qqbot: Option<QQBotChannelConfig>,
    pub client: Option<ClientConfig>,
}

impl ChannelConfigs {
    /// List names of enabled channels.
    /// A channel is enabled if its channel-level `enabled` flag is true
    /// AND it has at least one enabled account.
    pub fn enabled_channels(&self) -> Vec<&str> {
        let mut names = Vec::new();
        if self.wechat.as_ref().is_some_and(|c| c.enabled && c.accounts.values().any(|a| a.enabled)) {
            names.push("wechat");
        }
        if self.telegram.as_ref().is_some_and(|c| c.enabled && c.accounts.values().any(|a| a.enabled)) {
            names.push("telegram");
        }
        if self.qqbot.as_ref().is_some_and(|c| c.enabled && c.accounts.values().any(|a| a.enabled)) {
            names.push("qqbot");
        }
        if self.client.as_ref().is_some_and(|c| c.enabled) {
            names.push("client");
        }
        names
    }
}

// ── ClientConfig ─────────────────────────────────────────────────────────────

/// WebSocket client channel configuration (TUI / Web UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Whether this channel is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Bind address for the WebSocket server.
    #[serde(default = "default_client_bind")]
    pub bind: String,
    /// Maximum concurrent WebSocket connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// Authentication token (Bearer). None = no auth required.
    pub auth_token: Option<String>,
}

fn default_client_bind() -> String {
    "127.0.0.1:18789".to_string()
}

fn default_max_connections() -> u32 {
    10
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_client_bind(),
            max_connections: default_max_connections(),
            auth_token: None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_wechat_account_config() {
        let toml_str = r#"
api_base = "https://ilink.bot.weixin.qq.com"
bot_token = "${WECHAT_BOT_TOKEN}"
poll_timeout = 45
allowed_users = ["wxid_abc123"]
"#;
        let config: WechatAccountConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.api_base, "https://ilink.bot.weixin.qq.com");
        assert_eq!(config.bot_token.as_deref(), Some("${WECHAT_BOT_TOKEN}"));
        assert_eq!(config.poll_timeout, 45);
        assert!(config.enabled);
    }

    #[test]
    fn deserialize_telegram_account_config() {
        let toml_str = r#"
bot_token = "${TELEGRAM_BOT_TOKEN}"
allowed_users = ["*"]
mention_only = false
approval_timeout_secs = 60
"#;
        let config: TelegramAccountConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bot_token, "${TELEGRAM_BOT_TOKEN}");
        assert_eq!(config.allowed_users, vec!["*"]);
        assert!(!config.mention_only);
        assert_eq!(config.approval_timeout_secs, 60);
        assert!(config.enabled);
    }

    #[test]
    fn deserialize_multi_account_config() {
        let toml_str = r#"
[telegram]
enabled = true

[telegram.accounts.main]
bot_token = "tok1"
allowed_users = ["*"]

[telegram.accounts.ops]
bot_token = "tok2"
allowed_users = ["admin"]
enabled = true
"#;
        let config: ChannelConfigs = toml::from_str(toml_str).unwrap();
        let tg = config.telegram.as_ref().unwrap();
        assert!(tg.enabled);
        assert_eq!(tg.accounts.len(), 2);
        assert_eq!(tg.accounts["main"].bot_token, "tok1");
        assert_eq!(tg.accounts["ops"].bot_token, "tok2");
    }

    #[test]
    fn channel_configs_enabled_list() {
        let mut channels = ChannelConfigs::default();
        assert!(channels.enabled_channels().is_empty());

        let mut accounts = HashMap::new();
        accounts.insert("default".to_string(), TelegramAccountConfig {
            bot_token: "tok".into(),
            allowed_users: vec!["*".into()],
            enabled: true,
            ..Default::default()
        });
        channels.telegram = Some(TelegramChannelConfig {
            enabled: true,
            accounts,
        });
        assert_eq!(channels.enabled_channels(), vec!["telegram"]);

        // Disabled channel
        channels.wechat = Some(WechatChannelConfig {
            enabled: false,
            accounts: HashMap::new(),
        });
        assert_eq!(channels.enabled_channels(), vec!["telegram"]);
    }
}
