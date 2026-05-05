//! `myclaw doctor` — environment diagnostics.

use anyhow::Result;
use std::path::PathBuf;

use crate::cli::Cli;

pub async fn run(_cli: &Cli, fix: bool) -> Result<()> {
    let mut issues = Vec::new();
    let mut ok_count = 0usize;

    println!("🔍 MyClaw Doctor — Diagnostics\n");

    // 1. Config file
    let config_path = find_config();
    match &config_path {
        Some(path) => {
            match myclaw::config::ConfigLoader::from_file(path) {
                Ok(cfg) => {
                    println!("✅ Config file: {}", path.display());
                    println!("   Default model: {}", cfg.defaults.model);
                    ok_count += 1;
                }
                Err(e) => {
                    println!("❌ Config file found but invalid: {}", path.display());
                    println!("   Error: {e}");
                    issues.push(format!("Fix config file: {e}"));
                }
            }
        }
        None => {
            println!("⚠️  No config file found (searched: myclaw.toml, ~/.myclaw/myclaw.toml, /etc/myclaw/myclaw.toml)");
            issues.push("Create a config file with `myclaw config init`".to_string());
        }
    }

    // 2. Providers
    if let Some(cfg) = try_load_config() {
        if cfg.providers.is_empty() {
            println!("⚠️  No providers configured");
            issues.push("Add at least one provider in config (e.g. [providers.openai])".to_string());
        } else {
            for name in cfg.providers.keys() {
                println!("✅ Provider: {name}");
                ok_count += 1;
            }
        }

        // 3. Channels
        let has_telegram = cfg.channels.telegram.is_some();
        let has_wechat = cfg.channels.wechat.is_some();
        if has_telegram {
            println!("✅ Channel: telegram");
            ok_count += 1;
        }
        if has_wechat {
            println!("✅ Channel: wechat");
            ok_count += 1;
        }
        if !has_telegram && !has_wechat {
            println!("⚠️  No channels configured (daemon will have no input source)");
        }

        // 4. Workspace directory
        let ws = cfg.workspace_dir.clone();
        if ws.exists() {
            println!("✅ Workspace: {}", ws.display());
            ok_count += 1;
        } else {
            println!("❌ Workspace directory missing: {}", ws.display());
            if fix {
                std::fs::create_dir_all(&ws)?;
                println!("   🔧 Created: {}", ws.display());
                ok_count += 1;
            } else {
                issues.push(format!("Create workspace: mkdir -p {}", ws.display()));
            }
        }

        // 5. MCP servers
        if !cfg.mcp_servers.is_empty() {
            println!("✅ MCP servers: {} configured", cfg.mcp_servers.len());
            ok_count += 1;
        }

        // 6. Session DB
        let db_path = ws.join("sessions.db");
        if db_path.exists() {
            println!("✅ Session DB: {}", db_path.display());
            ok_count += 1;
        } else {
            println!("ℹ️  Session DB: not yet created (will be created on first run)");
        }
    }

    // Summary
    println!("\n─── Summary ───");
    println!("  ✅ {ok_count} checks passed");
    if !issues.is_empty() {
        println!("  ⚠️  {} issue(s) found:", issues.len());
        for (i, issue) in issues.iter().enumerate() {
            println!("     {}. {issue}", i + 1);
        }
    } else {
        println!("  🎉 All checks passed!");
    }

    Ok(())
}

fn find_config() -> Option<PathBuf> {
    for path in &["myclaw.toml", "~/.myclaw/myclaw.toml", "/etc/myclaw/myclaw.toml"] {
        let p = PathBuf::from(shellexpand::tilde(path).to_string());
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn try_load_config() -> Option<myclaw::config::AppConfig> {
    let path = find_config()?;
    myclaw::config::ConfigLoader::from_file(&path).ok()
}
