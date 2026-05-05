//! `myclaw completion` — generate shell completion scripts.

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli as RootCli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = RootCli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, &name, &mut std::io::stdout());
    Ok(())
}
