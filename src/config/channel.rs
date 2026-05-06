//! Channel adapter configurations.

use serde::{Deserialize, Serialize};

// ── WechatConfig ──────────────────────────────────────────────────────────────

/// WeChat iLink Bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WechatConfig {
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
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_poll_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

impl Default for WechatConfig {
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

// ── TelegramConfig ────────────────────────────────────────────────────────────

/// Telegram Bot API channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramConfig {
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
    /// Per-channel proxy URL.
    pub proxy_url: Option<String>,
    /// Whether this channel is enabled.
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
}

fn default_approval_timeout() -> u64 {
    120
}

// ── ChannelConfigs ────────────────────────────────────────────────────────────

/// All channel configurations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelConfigs {
    pub wechat: Option<WechatConfig>,
    pub telegram: Option<TelegramConfig>,
}

impl ChannelConfigs {
    /// List names of enabled channels.
    pub fn enabled_channels(&self) -> Vec<&str> {
        let mut names = Vec::new();
        if self.wechat.as_ref().is_some_and(|c| c.enabled) {
            names.push("wechat");
        }
        if self.telegram.as_ref().is_some_and(|c| c.enabled) {
            names.push("telegram");
        }
        names
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_wechat_config() {
        let toml_str = r#"
api_base = "https://ilink.bot.weixin.qq.com"
bot_token = "${WECHAT_BOT_TOKEN}"
poll_timeout = 45
allowed_users = ["wxid_abc123"]
"#;
        let config: WechatConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.api_base, "https://ilink.bot.weixin.qq.com");
        assert_eq!(config.bot_token.as_deref(), Some("${WECHAT_BOT_TOKEN}"));
        assert_eq!(config.poll_timeout, 45);
        assert!(config.enabled);
    }

    #[test]
    fn deserialize_telegram_config() {
        let toml_str = r#"
bot_token = "${TELEGRAM_BOT_TOKEN}"
allowed_users = ["*"]
mention_only = false
approval_timeout_secs = 60
"#;
        let config: TelegramConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bot_token, "${TELEGRAM_BOT_TOKEN}");
        assert_eq!(config.allowed_users, vec!["*"]);
        assert!(!config.mention_only);
        assert_eq!(config.approval_timeout_secs, 60);
        assert!(config.enabled);
    }

    #[test]
    fn channel_configs_enabled_list() {
        let mut channels = ChannelConfigs::default();
        assert!(channels.enabled_channels().is_empty());

        channels.telegram = Some(TelegramConfig {
            bot_token: "tok".into(),
            allowed_users: vec!["*".into()],
            enabled: true,
            ..Default::default()
        });
        assert_eq!(channels.enabled_channels(), vec!["telegram"]);

        // Disabled channel
        channels.wechat = Some(WechatConfig {
            api_base: "https://example.com".into(),
            enabled: false,
            ..Default::default()
        });
        assert_eq!(channels.enabled_channels(), vec!["telegram"]);
    }
}
