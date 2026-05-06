//! `myclaw chat` — interactive chat session (REPL).

use anyhow::Result;
use std::sync::Arc;

use crate::cli::Cli;

pub async fn run(cli: &Cli, prompt: Option<&str>, agent: Option<&str>, model: Option<&str>, print: bool) -> Result<()> {
    let cfg = super::load_config(cli)?;
    super::init_tracing(&cfg);

    let registry = myclaw::registry::Registry::from_config(cfg.providers.clone(), &cfg.routing)
        .map_err(|e| anyhow::anyhow!("failed to build registry: {}", e))?;
    let registry_arc: Arc<dyn myclaw::ServiceRegistry> = Arc::new(registry);

    let mut tools = myclaw::ToolRegistry::new();
    let mem = myclaw::tools::MemoryStore::new();
    for t in myclaw::tools::builtin_tools_with_memory(mem) {
        tools.register(t);
    }
    tools.register(Arc::new(myclaw::tools::ListDirTool::new()));
    let skills_arc: Arc<parking_lot::RwLock<myclaw::SkillManager>> =
        Arc::new(parking_lot::RwLock::new(myclaw::SkillManager::new()));
    tools.register(Arc::new(myclaw::tools::SkillTool::new(Arc::clone(&skills_arc))));
    let tools_arc = Arc::new(tools);

    let agent_config = myclaw::AgentConfig::default();
    let mut agent_factory = myclaw::Agent::new(
        Arc::clone(&registry_arc),
        tools_arc,
        skills_arc,
        agent_config,
    );

    if let Some(m) = model {
        agent_factory = agent_factory.with_model(m.to_string());
    }

    let session_key = agent.unwrap_or("cli");
    let session = myclaw::Session::new(session_key.to_string());
    let mut agent_loop = agent_factory.loop_for(session);

    // Non-interactive (--print) or single prompt mode.
    if print || prompt.is_some() {
        let input = prompt.unwrap_or("Hello");
        let response = agent_loop.run(input, None, None).await?;
        println!("{}", response);
        return Ok(());
    }

    // Interactive REPL.
    eprintln!("MyClaw Chat — type 'exit' or press Ctrl-D to quit.");
    eprintln!("Model: {}", cfg.defaults.model);
    eprintln!();

    loop {
        eprint!("> ");
        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            Ok(0) => break, // EOF (Ctrl-D)
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {}", e);
                break;
            }
        }
        let input = input.trim();
        if input.is_empty() { continue; }
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            break;
        }

        match agent_loop.run(input, None, None).await {
            Ok(response) => println!("{}\n", response),
            Err(e) => eprintln!("error: {}\n", e),
        }
    }

    Ok(())
}
