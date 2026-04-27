//! MyClaw — binary entry point (Composition Root).

use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "myclaw", about = "MyClaw daemon")]
enum Commands {
    /// Run the MyClaw daemon (starts all configured channels and agents).
    Run {
        /// Path to config file (default: search in ~/.myclaw/, /etc/myclaw/).
        #[structopt(short, long)]
        config: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = Commands::from_args();

    match cmd {
        Commands::Run { config } => {
            let cfg = match config {
                Some(path) => orchestrator::daemon::load_config_from(&path)?,
                None => {
                    if let Ok(env_path) = std::env::var("MYCLAW_CONFIG") {
                        orchestrator::daemon::load_config_from(&env_path)?
                    } else {
                        orchestrator::daemon::load_config()?
                    }
                }
            };

            orchestrator::daemon::init_tracing(&cfg);
            orchestrator::daemon::run(cfg).await?;
        }
    }

    Ok(())
}
