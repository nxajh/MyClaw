//! `myclaw reload` — send SIGHUP to the running daemon for config hot-reload.

use anyhow::Result;

use crate::cli::Cli;

pub async fn run(_cli: &Cli) -> Result<()> {
    super::signal::send_sighup()?;
    println!("Reload signal sent. Daemon will hot-reload config.");
    Ok(())
}
