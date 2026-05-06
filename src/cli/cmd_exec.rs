//! `myclaw exec` — non-interactive single prompt execution.

use anyhow::Result;
use crate::cli::Cli;

pub async fn run(cli: &Cli, prompt: &str, agent: Option<&str>, model: Option<&str>, format: &str) -> Result<()> {
    let cfg = super::load_config(cli)?;
    super::init_tracing(&cfg);

    println!("🤖 MyClaw Exec");
    println!("[exec] Prompt: {prompt}");
    if let Some(a) = agent {
        println!("[exec] Agent: {a}");
    }
    if let Some(m) = model {
        println!("[exec] Model: {m}");
    }
    println!("[exec] Format: {format}");
    println!("ℹ️  Exec mode not yet fully implemented.");
    Ok(())
}
