//! `myclaw status` — show agent, session, and system status.

use anyhow::Result;

use crate::cli::Cli;

/// Helper: first chat-routing model serves as the "default model" label
/// previously stored in `[defaults] model = ...`.
fn default_model(cfg: &myclaw::config::AppConfig) -> Option<&str> {
    cfg.routing
        .get(myclaw::providers::Capability::Chat)
        .and_then(|r| r.models.first().map(|s| s.as_str()))
}

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

    print_last_update_text();

    match cfg {
        Some(cfg) => {
            println!("  Config: ✅ loaded ({})", cfg.config_path.display());
            println!(
                "  Default model: {}",
                default_model(cfg).unwrap_or("(none)")
            );
            println!("  Workspace: {}", cfg.workspace_dir.display());

            let providers: Vec<_> = cfg.providers.keys().collect();
            println!(
                "  Providers: {}",
                if providers.is_empty() {
                    "none".to_string()
                } else {
                    providers
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );

            let channels: Vec<&str> = [
                cfg.channels.telegram.is_some().then_some("telegram"),
                cfg.channels.wechat.is_some().then_some("wechat"),
                cfg.channels.qqbot.is_some().then_some("qqbot"),
            ]
            .into_iter()
            .flatten()
            .collect();
            println!(
                "  Channels: {}",
                if channels.is_empty() {
                    "none".to_string()
                } else {
                    channels.join(", ")
                }
            );

            let agents = myclaw::agents::agent_loader::load_agents_from_dir(
                &cfg.agents_root(),
            );
            println!("  Sub-agents: {}", agents.len());
            println!("  MCP servers: {}", cfg.mcp_servers.len());

            // Issue #102: drafts are written to skills_root() (= {base_dir}
            // /skills), not workspace_dir/skills — read from where the
            // daemon actually serves skills from (skills_root() layered
            // with the shared ~/.agents/skills dir) so this count matches
            // reality instead of always reporting zero.
            let skills_dir = cfg.skills_root();
            let loaded = myclaw::agents::workspace::skill_loader::load_skills_layered(
                &skills_dir,
                cfg.agents_skills_dir_opt().as_deref(),
            )
            .len();
            let drafts =
                myclaw::agents::workspace::skill_loader::list_draft_skill_names(&skills_dir);
            println!(
                "  Skills: {} loaded / {} draft{} pending",
                loaded,
                drafts.len(),
                if drafts.len() == 1 { "" } else { "s" }
            );
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
        status["default_model"] = serde_json::json!(default_model(c).unwrap_or(""));
        status["workspace"] = serde_json::json!(c.workspace_dir.to_string_lossy().as_ref());
        status["providers"] = serde_json::json!(c.providers.keys().collect::<Vec<_>>());
        let agents =
            myclaw::agents::agent_loader::load_agents_from_dir(&c.agents_root());
        status["sub_agents"] = serde_json::json!(agents.len());
        status["mcp_servers"] = serde_json::json!(c.mcp_servers.len());

        // Issue #102: see the matching comment in print_text_status.
        let skills_dir = c.skills_root();
        let loaded = myclaw::agents::workspace::skill_loader::load_skills_layered(
            &skills_dir,
            c.agents_skills_dir_opt().as_deref(),
        )
        .len();
        let drafts = myclaw::agents::workspace::skill_loader::list_draft_skill_names(&skills_dir);
        status["skills"] = serde_json::json!({
            "loaded": loaded,
            "drafts_pending": drafts.len(),
            "draft_names": drafts,
        });
    }
    if let Ok(Some(u)) = myclaw::update_state::UpdateState::load() {
        status["last_update"] = serde_json::json!({
            "status": u.status.as_str(),
            "run_id": u.run_id,
            "commit": u.commit,
            "binary_path": u.binary_path,
            "binary_sha256": u.binary_sha256,
            "old_pid": u.old_pid,
            "new_pid": u.new_pid,
            "error": u.error,
            "updated_at": u.updated_at,
        });
    }
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

fn print_last_update_text() {
    match myclaw::update_state::UpdateState::load() {
        Ok(Some(u)) => {
            println!("  Last update: {}", u.status.as_str());
            if let Some(ref id) = u.run_id {
                println!("    run_id: {id}");
            }
            if let Some(ref sha) = u.commit {
                println!("    commit: {sha}");
            }
            if let Some(ref path) = u.binary_path {
                println!("    binary_path: {path}");
            }
            if let Some(ref h) = u.binary_sha256 {
                let short = if h.len() > 12 { &h[..12] } else { h.as_str() };
                println!("    binary_sha256: {short}…");
            }
            if let Some(pid) = u.old_pid {
                println!("    old_pid: {pid}");
            }
            if let Some(pid) = u.new_pid {
                println!("    new_pid: {pid}");
            }
            if let Some(ref err) = u.error {
                println!("    error: {err}");
            }
            println!("    updated_at: {}", u.updated_at);
        }
        Ok(None) => {}
        Err(e) => {
            println!("  Last update: ⚠️  unreadable ({e})");
        }
    }
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
