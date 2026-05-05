//! `myclaw chat` — interactive chat session.

use anyhow::Result;
use crate::cli::Cli;

pub async fn run(cli: &Cli, prompt: Option<&str>, agent: Option<&str>, model: Option<&str>, print: bool) -> Result<()> {
    let cfg = super::load_config(cli)?;
    super::init_tracing(&cfg);

    // TODO: implement interactive TUI / REPL
    // For now, a simple single-turn chat via the agent.
    let prompt_text = prompt.unwrap_or("Hello!");
    println!("🤖 MyClaw Chat (model: {})", cfg.defaults.model);

    // Use the orchestrator to run a single agent turn.
    // This will be fleshed out when the TUI module is added.
    println!("[chat] Prompt: {prompt_text}");
    if let Some(agent_name) = agent {
        println!("[chat] Agent: {agent_name}");
    }
    if let Some(model_name) = model {
        println!("[chat] Model override: {model_name}");
    }
    if print {
        println!("[chat] Non-interactive (print) mode");
    }
    println!("ℹ️  Interactive chat not yet implemented. Use `myclaw run` to start the daemon.");
    Ok(())
}
