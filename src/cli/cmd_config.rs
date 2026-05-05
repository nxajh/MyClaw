//! `myclaw config` — show or set configuration values.

use anyhow::Result;
use clap::Subcommand;

use crate::cli::Cli;

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigAction {
    /// Show the full resolved configuration.
    Show,

    /// Get a specific config value by dotted path (e.g. "defaults.model").
    Get {
        /// Dotted config path.
        path: String,
    },

    /// Set a config value (writes to the config file).
    Set {
        /// Dotted config path.
        path: String,
        /// Value to set.
        value: String,
    },

    /// Initialize a new config file with defaults.
    Init {
        /// Output path (default: ~/.myclaw/myclaw.toml).
        #[arg(short, long)]
        output: Option<String>,
    },
}

pub async fn run(cli: &Cli, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let cfg = super::load_config(cli)?;
            // AppConfig is not Serialize, so display key fields manually.
            println!("# MyClaw Configuration (resolved)");
            println!("workspace_dir = \"{}\"", cfg.workspace_dir.display());
            println!("config_path = \"{}\"", cfg.config_path.display());
            println!();
            println!("[defaults]");
            println!("model = \"{}\"", cfg.defaults.model);
            println!();
            if !cfg.providers.is_empty() {
                println!("[providers]");
                for name in cfg.providers.keys() {
                    println!("  {name} = <configured>");
                }
                println!();
            }
            if cfg.channels.telegram.is_some() {
                println!("[channels.telegram]");
                println!("  bot_token = <configured>");
                println!();
            }
            if cfg.channels.wechat.is_some() {
                println!("[channels.wechat]");
                println!("  bot_token = <configured>");
                println!();
            }
            if !cfg.agents.is_empty() {
                println!("[[agents]]");
                for agent in &cfg.agents {
                    println!("  name = \"{}\"", agent.name);
                    if let Some(ref desc) = agent.description {
                        println!("  description = \"{desc}\"");
                    }
                }
                println!();
            }
            if !cfg.mcp_servers.is_empty() {
                println!("[[mcp_servers]]");
                for server in &cfg.mcp_servers {
                    println!("  name = \"{}\"", server.name);
                    println!("  command = \"{}\"", server.command);
                }
            }
        }
        ConfigAction::Get { path } => {
            println!("⚠️  config get \"{path}\" — not yet implemented (AppConfig is not serde-serializable)");
            println!("   Use `myclaw config show` to see the full resolved config.");
        }
        ConfigAction::Set { path, value } => {
            println!("⚠️  config set not yet implemented (path={path}, value={value})");
            println!("   Edit your config file directly for now.");
        }
        ConfigAction::Init { output } => {
            let out_path = output
                .map(|p| shellexpand::tilde(&p).to_string())
                .unwrap_or_else(|| shellexpand::tilde("~/.myclaw/myclaw.toml").to_string());
            let p = std::path::Path::new(&out_path);
            if p.exists() {
                anyhow::bail!("Config file already exists: {}", p.display());
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let default_config = generate_default_config();
            std::fs::write(p, &default_config)?;
            println!("✅ Created config file: {}", p.display());
        }
    }
    Ok(())
}

fn generate_default_config() -> String {
    r#"# MyClaw Configuration
# See https://github.com/nxajh/MyClaw for documentation.

workspace_dir = "~/.myclaw/workspace"

[defaults]
model = "minimax-m2.7"

[agent]
autonomy_level = "default"

[memory]
storage = "sqlite"

# Example provider:
# [providers.openai]
# api_key = "${OPENAI_API_KEY}"
#
# [providers.openai.chat]
# base_url = "https://api.openai.com/v1"
#
# [providers.openai.chat.models.gpt-4o]
# input = ["text"]
# output = ["text"]
# context_window = 128000

# Example channel:
# [channels.telegram]
# bot_token = "${TELEGRAM_BOT_TOKEN}"
# allowed_users = ["*"]
"#.to_string()
}
