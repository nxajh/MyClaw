//! MyClaw — binary entry point (Composition Root).

use anyhow::Result;
use clap::Parser;

mod cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli_args = cli::Cli::parse();

    match cli_args.command {
        // `myclaw run` — start the daemon (backward compat)
        Some(cli::Commands::Run { config }) => {
            let cfg = resolve_config_or_die(config.as_deref().or(cli_args.config.as_deref()));
            cli::init_tracing(&cfg);
            myclaw::daemon::run(cfg).await?;
        }

        // `myclaw chat` — interactive chat
        Some(cli::Commands::Chat { ref prompt, ref agent, ref model, print }) => {
            cli::cmd_chat::run(&cli_args, prompt.as_deref(), agent.as_deref(), model.as_deref(), print).await?;
        }

        // `myclaw exec` — non-interactive single prompt
        Some(cli::Commands::Exec { ref prompt, ref agent, ref model, ref format }) => {
            cli::cmd_exec::run(&cli_args, prompt, agent.as_deref(), model.as_deref(), format).await?;
        }

        // `myclaw doctor` — diagnostics
        Some(cli::Commands::Doctor { fix }) => {
            cli::cmd_doctor::run(&cli_args, fix).await?;
        }

        // `myclaw config` — show/set config
        Some(cli::Commands::Config { ref action }) => {
            cli::cmd_config::run(&cli_args, action.clone()).await?;
        }

        // `myclaw status` — system status
        Some(cli::Commands::Status { ref format }) => {
            cli::cmd_status::run(&cli_args, format).await?;
        }

        // `myclaw tools` — list tools/agents/mcp
        Some(cli::Commands::Tools(ref cmd)) => {
            cli::cmd_tools::run(&cli_args, cmd.clone()).await?;
        }

        // `myclaw completion` — shell completions
        Some(cli::Commands::Completion { shell }) => {
            cli::cmd_completion::run(shell)?;
        }

        // `myclaw update` — download latest artifact and hot-switch
        Some(cli::Commands::Update) => {
            cli::cmd_update::run_update()?;
        }

        // `myclaw reload` — hot-reload daemon config
        Some(cli::Commands::Reload) => {
            cli::cmd_reload::run(&cli_args).await?;
        }

        // `myclaw restart` — graceful daemon restart
        Some(cli::Commands::Restart) => {
            cli::cmd_restart::run(&cli_args).await?;
        }

        // `myclaw stop` — graceful shutdown via SIGTERM
        Some(cli::Commands::Stop) => {
            cli::cmd_stop::run(&cli_args).await?;
        }

        // `myclaw version` — detailed version info
        Some(cli::Commands::Version) => {
            println!("MyClaw {}", env!("MYCLAW_VERSION"));
            println!("  Target: {}", std::env::consts::ARCH);
            println!("  OS: {}", std::env::consts::OS);
        }

        // `myclaw tui` — launch TUI client
        #[cfg(feature = "tui")]
        Some(cli::Commands::Tui { ref url }) => {
            cli::cmd_tui::run(Some(url)).await?;
        }

        // No subcommand → show help (arg_required_else_help handles this normally)
        None => {
            let cfg = resolve_config_or_die(cli_args.config.as_deref());
            cli::init_tracing(&cfg);
            myclaw::daemon::run(cfg).await?;
        }
    }

    Ok(())
}

/// Resolve config from CLI flag, env var, or default search paths.
fn resolve_config_or_die(explicit_path: Option<&str>) -> myclaw::config::AppConfig {
    if let Some(path) = explicit_path {
        let expanded = shellexpand::tilde(path).to_string();
        return myclaw::daemon::load_config_from(&expanded)
            .unwrap_or_else(|e| {
                eprintln!("Error: Failed to load config from {path}: {e}");
                std::process::exit(1);
            });
    }

    if let Ok(env_path) = std::env::var("MYCLAW_CONFIG") {
        return myclaw::daemon::load_config_from(&env_path)
            .unwrap_or_else(|e| {
                eprintln!("Error: Failed to load config from MYCLAW_CONFIG={env_path}: {e}");
                std::process::exit(1);
            });
    }

    myclaw::daemon::load_config().unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        eprintln!("Hint: Run `myclaw config init` to create a default config file.");
        std::process::exit(1);
    })
}
