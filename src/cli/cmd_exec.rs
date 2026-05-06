//! `myclaw exec` — non-interactive single prompt execution.

use anyhow::Result;
use std::sync::Arc;

use crate::cli::Cli;

pub async fn run(cli: &Cli, prompt: &str, agent: Option<&str>, model: Option<&str>, format: &str) -> Result<()> {
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

    let response = agent_loop.run(prompt, None, None).await?;

    match format {
        "json" => {
            let out = serde_json::json!({"response": response});
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        _ => println!("{}", response),
    }

    Ok(())
}
