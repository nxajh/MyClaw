//! CLI module — all subcommand definitions and handlers.

pub mod cmd_chat;
pub mod cmd_completion;
pub mod cmd_config;
pub mod cmd_doctor;
pub mod cmd_exec;
pub mod cmd_status;
pub mod cmd_tools;
#[cfg(feature = "tui")]
pub mod cmd_tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// MyClaw — AI Agent daemon.
///
/// If no subcommand is specified, starts the daemon (equivalent to `run`).
#[derive(Debug, Parser)]
#[command(
    name = "myclaw",
    about = "MyClaw — AI Agent system",
    version = env!("MYCLAW_VERSION"),
    propagate_version = true,
    arg_required_else_help = true,
    subcommand_negates_reqs = true,
)]
pub struct Cli {
    /// Path to config file.
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start the MyClaw daemon (starts all configured channels and agents).
    Run {
        /// Path to config file (default: search in ~/.myclaw/, /etc/myclaw/).
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Start an interactive chat session (TUI / REPL).
    Chat {
        /// Initial prompt to send immediately.
        prompt: Option<String>,

        /// Agent name to use (from config [[agents]]).
        #[arg(short, long)]
        agent: Option<String>,

        /// Model override.
        #[arg(short, long)]
        model: Option<String>,

        /// Non-interactive mode: print response and exit.
        #[arg(short, long)]
        print: bool,
    },

    /// Run a single prompt non-interactively (alias: chat --print).
    #[command(visible_alias = "e")]
    Exec {
        /// The prompt to execute.
        prompt: String,

        /// Agent name to use.
        #[arg(short, long)]
        agent: Option<String>,

        /// Model override.
        #[arg(short, long)]
        model: Option<String>,

        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Diagnose environment, configuration, and connectivity.
    Doctor {
        /// Attempt to fix issues automatically.
        #[arg(long)]
        fix: bool,
    },

    /// Show or set configuration values.
    Config {
        #[command(subcommand)]
        action: cmd_config::ConfigAction,
    },

    /// Show agent, session, and system status.
    Status {
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// List available tools, agents, or MCP servers.
    #[command(subcommand)]
    Tools(cmd_tools::ToolsCommand),

    /// Generate shell completion scripts.
    Completion {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },

    /// Show version and build information.
    Version,

    /// Launch the TUI client connected to a running MyClaw WebSocket server.
    #[cfg(feature = "tui")]
    Tui {
        /// WebSocket URL of the MyClaw server.
        #[arg(long, default_value = "ws://127.0.0.1:18789/myclaw")]
        url: String,
    },
}

// ── Shared helpers for subcommand handlers ────────────────────────────────────

/// Default config file search locations.
const DEFAULT_CONFIG_PATHS: &[&str] = &[
    "myclaw.toml",
    "~/.myclaw/myclaw.toml",
    "/etc/myclaw/myclaw.toml",
];

/// Load config from the first found file, respecting CLI --config flag.
pub fn load_config(cli: &Cli) -> Result<myclaw::config::AppConfig> {
    let path = resolve_config_path(cli).ok_or_else(|| anyhow::anyhow!(
        "No config file found. Searched: {}. Use --config or `myclaw config init`.",
        DEFAULT_CONFIG_PATHS.join(", ")
    ))?;
    myclaw::config::ConfigLoader::from_file(&path)
}

/// Load config, returning None if no config file found (non-fatal).
pub fn load_config_opt(cli: &Cli) -> Option<myclaw::config::AppConfig> {
    let path = resolve_config_path(cli)?;
    myclaw::config::ConfigLoader::from_file(&path).ok()
}

/// Resolve the config file path from CLI args or environment.
fn resolve_config_path(cli: &Cli) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    // 1. Explicit --config flag — if specified, must exist (no fallback)
    if let Some(ref path) = cli.config {
        let p = PathBuf::from(shellexpand::tilde(path).to_string());
        if p.exists() {
            return Some(p);
        }
        // Explicit path given but doesn't exist → stop here, don't fallback
        return None;
    }

    // 2. MYCLAW_CONFIG env var
    if let Ok(env_path) = std::env::var("MYCLAW_CONFIG") {
        let p = PathBuf::from(shellexpand::tilde(&env_path).to_string());
        if p.exists() {
            return Some(p);
        }
    }

    // 3. Default search paths
    for path in DEFAULT_CONFIG_PATHS {
        let p = PathBuf::from(shellexpand::tilde(path).to_string());
        if p.exists() {
            return Some(p);
        }
    }

    None
}

/// Initialize tracing/logging based on config.
pub fn init_tracing(cfg: &myclaw::config::AppConfig) {
    myclaw::daemon::init_tracing(cfg);
}
