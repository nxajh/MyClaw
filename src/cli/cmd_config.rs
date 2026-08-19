//! `myclaw config` — show or set configuration values.

use anyhow::Result;
use clap::Subcommand;

use crate::cli::Cli;

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigAction {
    /// Show the full resolved configuration.
    Show,

    /// Get a specific config value by dotted path (e.g. "routing.chat.models").
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
            if let Some(route) = cfg.routing.get(myclaw::providers::Capability::Chat) {
                if let Some(model) = route.models.first() {
                    println!("[routing.chat]");
                    println!("models = [\"{}\"]", model);
                    println!();
                }
            }
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
            if !cfg.mcp_servers.is_empty() {
                println!("[[mcp_servers]]");
                for server in &cfg.mcp_servers {
                    println!("  name = \"{}\"", server.name);
                    println!("  command = \"{}\"", server.command);
                }
            }

            // Sub-agents are loaded from workspace/agents/, not from config
            let agents = myclaw::agents::agent_loader::load_agents_from_dir(
                &cfg.agents_root(),
            );
            if !agents.is_empty() {
                println!("[[agents]] (from data dir agents/)");
                for agent in &agents {
                    println!("  name = \"{}\"", agent.name);
                    if let Some(ref desc) = agent.description {
                        println!("  description = \"{desc}\"");
                    }
                }
                println!();
            }
        }
        ConfigAction::Get { path } => {
            println!(
                "⚠️  config get \"{path}\" — not yet implemented (AppConfig is not serde-serializable)"
            );
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

# data_dir already defaults to ~/.myclaw when omitted (see
# config::default_data_dir — the single source of truth every other path
# default, migration.rs, and scripts/migrate-layout.py derive from). Spelled
# out explicitly here anyway so the base of the tree memory/users/workspace
# all nest under is never ambiguous from reading this file alone.
data_dir = "~/.myclaw"
workspace_dir = "~/.myclaw/workspace"

[routing.chat]
strategy = "fallback"
models = ["minimax-m2.7"]

[agent]
permission_mode = "default"

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
"#
    .to_string()
}
