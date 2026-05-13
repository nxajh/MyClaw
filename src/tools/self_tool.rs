//! Self-management tool — agent lifecycle management (reload, restart, update, status, etc.).

use async_trait::async_trait;
use crate::providers::{Tool, ToolResult};
use serde_json::json;

/// Agent self-management tool.
#[derive(Default)]
pub struct SelfTool;

impl SelfTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SelfTool {
    fn name(&self) -> &str {
        "self"
    }

    fn description(&self) -> &str {
        "Self-management of the myclaw daemon: reload config, restart, update binary, check status, diagnose issues, manage tools and configuration"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["reload", "restart", "update", "status", "tools", "config", "doctor", "version"],
                    "description": "The self-management action to perform."
                },
                "sub_action": {
                    "type": "string",
                    "description": "Sub-action for config (get/set/init/show) and tools (list/agents/mcp). Optional."
                },
                "key": {
                    "type": "string",
                    "description": "Config key for config get/set (dotted path, e.g. 'scheduler.heartbeat.enabled'). Optional."
                },
                "value": {
                    "type": "string",
                    "description": "Value for config set. Optional."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'action' is required"))?;

        match action {
            "reload" => do_reload(),
            "restart" => do_restart(),
            "update" => do_update(),
            "status" => do_status(),
            "tools" => do_tools(args["sub_action"].as_str()),
            "config" => do_config(args["sub_action"].as_str(), args["key"].as_str(), args["value"].as_str()),
            "doctor" => do_doctor(),
            "version" => do_version(),
            _ => Ok(ToolResult {
                output: format!("Unknown action: '{}'. Valid: reload, restart, update, status, tools, config, doctor, version", action),
                error: Some("invalid action".to_string()),
                success: false,
            }),
        }
    }
}

fn do_reload() -> anyhow::Result<ToolResult> {
    match crate::signal::send_sighup() {
        Ok(()) => Ok(ToolResult {
            output: "SIGHUP sent. Daemon will hot-reload config.".to_string(),
            error: None,
            success: true,
        }),
        Err(e) => Ok(ToolResult {
            output: format!("Failed to send reload signal: {}", e),
            error: Some(e.to_string()),
            success: false,
        }),
    }
}

fn do_restart() -> anyhow::Result<ToolResult> {
    match crate::signal::send_sigusr1() {
        Ok(()) => Ok(ToolResult {
            output: "SIGUSR1 sent. Daemon will restart shortly.".to_string(),
            error: None,
            success: true,
        }),
        Err(e) => Ok(ToolResult {
            output: format!("Failed to send restart signal: {}", e),
            error: Some(e.to_string()),
            success: false,
        }),
    }
}

fn do_update() -> anyhow::Result<ToolResult> {
    let output = run_cli_command("update")?;
    Ok(ToolResult {
        output,
        error: None,
        success: true,
    })
}

fn do_status() -> anyhow::Result<ToolResult> {
    let mut lines = Vec::new();

    // Version
    lines.push(format!("Version: {}", env!("MYCLAW_VERSION")));

    // PID & uptime
    match crate::signal::find_daemon_pid() {
        Ok(pid) => {
            lines.push(format!("PID: {}", pid));
            // Try to read uptime from /proc
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
                // Parse starttime (field 22) from /proc/pid/stat
                let fields: Vec<&str> = stat.split_whitespace().collect();
                if fields.len() > 21 {
                    if let Ok(start_ticks) = fields[21].parse::<u64>() {
                        if let Ok(uptime) = std::fs::read_to_string("/proc/uptime") {
                            if let Ok(sys_uptime) = uptime.split_whitespace().next()
                                .unwrap_or("0").parse::<f64>()
                            {
                                let ticks_per_sec = 100u64; // common default
                                let start_secs = start_ticks / ticks_per_sec;
                                let running_secs = sys_uptime as u64 - start_secs;
                                let hours = running_secs / 3600;
                                let mins = (running_secs % 3600) / 60;
                                lines.push(format!("Uptime: {}h {}m", hours, mins));
                            }
                        }
                    }
                }
            }
            lines.push("Status: running".to_string());
        }
        Err(_) => {
            lines.push("Status: not running (or PID not found)".to_string());
        }
    }

    // Config
    match crate::signal::find_daemon_pid() {
        _ => {} // Already reported above
    }

    // Try to load config for static info
    if let Some(cfg) = try_load_config() {
        lines.push(format!("Default model: {}", cfg.defaults.model));
        lines.push(format!("Workspace: {}", cfg.workspace_dir.display()));

        let channels: Vec<&str> = [
            cfg.channels.telegram.is_some().then_some("telegram"),
            cfg.channels.wechat.is_some().then_some("wechat"),
            cfg.channels.qqbot.is_some().then_some("qqbot"),
        ].into_iter().flatten().collect();
        lines.push(format!("Channels: {}", if channels.is_empty() { "none".to_string() } else { channels.join(", ") }));
    }

    Ok(ToolResult {
        output: lines.join("\n"),
        error: None,
        success: true,
    })
}

fn do_tools(sub_action: Option<&str>) -> anyhow::Result<ToolResult> {
    let sub = sub_action.unwrap_or("list");
    let output = run_cli_command(&format!("tools {}", sub))?;
    Ok(ToolResult {
        output,
        error: None,
        success: true,
    })
}

fn do_config(sub_action: Option<&str>, key: Option<&str>, value: Option<&str>) -> anyhow::Result<ToolResult> {
    let mut cmd = String::from("config");
    match sub_action.unwrap_or("show") {
        "show" => cmd.push_str(" show"),
        "get" => {
            if let Some(k) = key {
                cmd.push_str(&format!(" get {}", k));
            } else {
                return Ok(ToolResult {
                    output: "config get requires 'key' parameter".to_string(),
                    error: Some("missing key".to_string()),
                    success: false,
                });
            }
        }
        "set" => {
            match (key, value) {
                (Some(k), Some(v)) => cmd.push_str(&format!(" set {} {}", k, v)),
                _ => {
                    return Ok(ToolResult {
                        output: "config set requires 'key' and 'value' parameters".to_string(),
                        error: Some("missing key/value".to_string()),
                        success: false,
                    });
                }
            }
        }
        "init" => cmd.push_str(" init"),
        other => cmd.push_str(&format!(" {}", other)),
    }

    let output = run_cli_command(&cmd)?;
    Ok(ToolResult {
        output,
        error: None,
        success: true,
    })
}

fn do_doctor() -> anyhow::Result<ToolResult> {
    let output = run_cli_command("doctor")?;
    Ok(ToolResult {
        output,
        error: None,
        success: true,
    })
}

fn do_version() -> anyhow::Result<ToolResult> {
    Ok(ToolResult {
        output: format!(
            "MyClaw {}\n  Target: {}\n  OS: {}",
            env!("MYCLAW_VERSION"),
            std::env::consts::ARCH,
            std::env::consts::OS,
        ),
        error: None,
        success: true,
    })
}

/// Run a myclaw CLI subcommand and capture output.
fn run_cli_command(subcmd: &str) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let output = std::process::Command::new(exe)
        .args(subcmd.split_whitespace())
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        Ok(stdout.to_string())
    } else {
        let msg = if stderr.is_empty() {
            stdout.to_string()
        } else {
            format!("{}\n{}", stdout, stderr)
        };
        Ok(msg)
    }
}

/// Try to load config from default paths.
fn try_load_config() -> Option<crate::config::AppConfig> {
    for path in &["myclaw.toml", "~/.myclaw/myclaw.toml", "/etc/myclaw/myclaw.toml"] {
        let p = std::path::PathBuf::from(shellexpand::tilde(path).to_string());
        if p.exists() {
            if let Ok(cfg) = crate::config::ConfigLoader::from_file(&p) {
                return Some(cfg);
            }
        }
    }
    None
}
