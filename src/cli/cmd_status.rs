//! `myclaw status` — show agent, session, and system status.

use anyhow::Result;

use crate::cli::Cli;

pub async fn run(cli: &Cli, format: &str) -> Result<()> {
    let cfg = super::load_config_opt(cli);

    match format {
        "json" => print_json_status(&cfg)?,
        _ => print_text_status(&cfg),
    }
    Ok(())
}

fn print_text_status(cfg: &Option<myclaw::config::AppConfig>) {
    println!("🤖 MyClaw Status\n");

    println!("  Version: {}", env!("MYCLAW_VERSION"));

    match cfg {
        Some(cfg) => {
            println!("  Config: ✅ loaded ({})", cfg.config_path.display());
            println!("  Default model: {}", cfg.defaults.model);
            println!("  Workspace: {}", cfg.workspace_dir.display());

            let providers: Vec<_> = cfg.providers.keys().collect();
            println!("  Providers: {}", if providers.is_empty() { "none".to_string() } else { providers.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") });

            let channels: Vec<&str> = [
                cfg.channels.telegram.is_some().then_some("telegram"),
                cfg.channels.wechat.is_some().then_some("wechat"),
            ].into_iter().flatten().collect();
            println!("  Channels: {}", if channels.is_empty() { "none".to_string() } else { channels.join(", ") });

            let agents = myclaw::agents::agent_loader::load_agents_from_dir(&cfg.workspace_dir.join("agents"));
            println!("  Sub-agents: {}", agents.len());
            println!("  MCP servers: {}", cfg.mcp_servers.len());
        }
        None => {
            println!("  Config: ⚠️  not found");
        }
    }
}

fn print_json_status(cfg: &Option<myclaw::config::AppConfig>) -> Result<()> {
    let mut status = serde_json::json!({
        "version": env!("MYCLAW_VERSION"),
        "config_loaded": cfg.is_some(),
    });
    if let Some(c) = cfg {
        status["config_path"] = serde_json::json!(c.config_path.to_string_lossy().as_ref());
        status["default_model"] = serde_json::json!(c.defaults.model);
        status["workspace"] = serde_json::json!(c.workspace_dir.to_string_lossy().as_ref());
        status["providers"] = serde_json::json!(c.providers.keys().collect::<Vec<_>>());
        let agents = myclaw::agents::agent_loader::load_agents_from_dir(&c.workspace_dir.join("agents"));
        status["sub_agents"] = serde_json::json!(agents.len());
        status["mcp_servers"] = serde_json::json!(c.mcp_servers.len());
    }
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}
