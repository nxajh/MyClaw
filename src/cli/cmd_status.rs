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

    // Daemon runtime info
    match myclaw::signal::find_daemon_pid() {
        Ok(pid) => {
            println!("  PID: {}", pid);
            println!("  Status: ✅ running");
            if let Some((hours, mins)) = read_uptime(pid) {
                println!("  Uptime: {}h {}m", hours, mins);
            }
        }
        Err(_) => {
            println!("  Status: ⚠️  not running (or PID not found)");
        }
    }

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
                cfg.channels.qqbot.is_some().then_some("qqbot"),
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

    match myclaw::signal::find_daemon_pid() {
        Ok(pid) => {
            status["pid"] = serde_json::json!(pid);
            status["running"] = serde_json::json!(true);
            if let Some((hours, mins)) = read_uptime(pid) {
                status["uptime"] = serde_json::json!(format!("{}h {}m", hours, mins));
            }
        }
        Err(_) => {
            status["running"] = serde_json::json!(false);
        }
    }

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

/// Read process uptime from /proc.
fn read_uptime(pid: i32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() <= 21 {
        return None;
    }
    let start_ticks: u64 = fields[21].parse().ok()?;
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let sys_uptime: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    let ticks_per_sec = 100u64; // common default
    let start_secs = start_ticks / ticks_per_sec;
    let running_secs = sys_uptime as u64 - start_secs;
    Some((running_secs / 3600, (running_secs % 3600) / 60))
}
