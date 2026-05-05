//! `myclaw tools` — list available tools, agents, MCP servers.

use anyhow::Result;
use clap::Subcommand;

use crate::cli::Cli;

#[derive(Debug, Clone, Subcommand)]
pub enum ToolsCommand {
    /// List all built-in tools.
    List {
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// List configured sub-agents.
    Agents {
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// List configured MCP servers.
    Mcp {
        /// Output format: text (default) or json.
        #[arg(long, default_value = "text")]
        format: String,
    },
}

pub async fn run(cli: &Cli, cmd: ToolsCommand) -> Result<()> {
    match cmd {
        ToolsCommand::List { format } => list_tools(&format),
        ToolsCommand::Agents { format } => list_agents(cli, &format),
        ToolsCommand::Mcp { format } => list_mcp(cli, &format),
    }
}

fn list_tools(format: &str) -> Result<()> {
    let tools: Vec<(&str, &str)> = vec![
        ("shell", "Execute shell commands"),
        ("file_read", "Read file contents"),
        ("file_write", "Write content to files"),
        ("file_edit", "Edit files by replacing exact strings"),
        ("list_dir", "List directory contents"),
        ("glob_search", "Search files by glob pattern"),
        ("content_search", "Search file contents by regex"),
        ("web_fetch", "Fetch web page content"),
        ("web_search", "Search the web"),
        ("http_request", "Make HTTP requests"),
        ("calculator", "Evaluate math expressions"),
        ("memory_store", "Store facts in memory"),
        ("memory_recall", "Recall stored memories"),
        ("memory_forget", "Delete memories"),
        ("ask_user", "Ask user a question"),
        ("delegate_task", "Delegate task to sub-agent"),
        ("task_manager", "Manage tasks and goals"),
        ("tool_search", "Search available tools"),
    ];

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(
            &tools.iter().map(|(n, d)| serde_json::json!({"name": n, "description": d})).collect::<Vec<_>>()
        )?),
        _ => {
            println!("📋 Built-in Tools ({} available)\n", tools.len());
            for (name, desc) in &tools {
                println!("  • {name:20} {desc}");
            }
        }
    }
    Ok(())
}

fn list_agents(cli: &Cli, format: &str) -> Result<()> {
    let cfg = super::load_config_opt(cli);

    match format {
        "json" => {
            let agents: Vec<String> = cfg.as_ref()
                .map(|c| {
                    myclaw::agents::agent_loader::load_agents_from_dir(&c.workspace_dir.join("agents"))
                        .into_iter().map(|a| a.name).collect()
                })
                .unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&agents)?);
        }
        _ => {
            println!("🤖 Configured Sub-Agents\n");
            if let Some(cfg) = cfg {
                let agents = myclaw::agents::agent_loader::load_agents_from_dir(&cfg.workspace_dir.join("agents"));
                if agents.is_empty() {
                    println!("  (none configured — add AGENT.md files to workspace/agents/)");
                }
                for agent in &agents {
                    let desc = agent.description.as_deref().unwrap_or("(no description)");
                    println!("  • {} — {desc}", agent.name);
                }
            } else {
                println!("  ⚠️  No config loaded");
            }
        }
    }
    Ok(())
}

fn list_mcp(cli: &Cli, format: &str) -> Result<()> {
    let cfg = super::load_config_opt(cli);

    match format {
        "json" => {
            let servers: Vec<_> = cfg.as_ref().map(|c| c.mcp_servers.iter().map(|s| &s.name).collect()).unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&servers)?);
        }
        _ => {
            println!("🔌 MCP Servers\n");
            if let Some(cfg) = cfg {
                if cfg.mcp_servers.is_empty() {
                    println!("  (none configured)");
                }
                for server in &cfg.mcp_servers {
                    let cmd = format!("{} {}", server.command, server.args.join(" "));
                    println!("  • {} — {cmd}", server.name);
                }
            } else {
                println!("  ⚠️  No config loaded");
            }
        }
    }
    Ok(())
}
