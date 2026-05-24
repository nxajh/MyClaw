//! `myclaw exec` — non-interactive single prompt execution.

use anyhow::Result;
use std::sync::Arc;

use crate::cli::Cli;

pub async fn run(cli: &Cli, prompt: &str, agent: Option<&str>, model: Option<&str>, format: &str) -> Result<()> {
    let cfg = super::load_config(cli)?;
    super::init_tracing(&cfg);

    let registry = myclaw::registry::Registry::from_config(cfg.providers.clone(), &cfg.routing)
        .map_err(|e| anyhow::anyhow!("failed to build registry: {}", e))?;
    let registry_arc: Arc<dyn myclaw::ProviderRegistry> = Arc::new(registry);

    let mut tools = myclaw::ToolRegistry::new();
    for t in myclaw::tools::builtin_tools() {
        tools.register(t);
    }
    tools.register(Arc::new(myclaw::tools::ListDirTool::new()));
    let skills_arc: Arc<parking_lot::RwLock<myclaw::SkillManager>> =
        Arc::new(parking_lot::RwLock::new(myclaw::SkillManager::new()));
    let workspace_dir = std::path::PathBuf::from(&cfg.workspace_dir);
    tools.register(Arc::new(myclaw::tools::SkillTool::new(Arc::clone(&skills_arc))));
    tools.register(Arc::new(myclaw::tools::SkillsListTool::new(Arc::clone(&skills_arc))));
    tools.register(Arc::new(myclaw::tools::SkillManageTool::new(
        Arc::clone(&skills_arc),
        workspace_dir.clone(),
    )));
    // Memory tools
    let kd = cfg.workspace_dir.to_string_lossy().to_string();
    tools.register(Arc::new(myclaw::tools::MemoryListTool::new(kd.clone())));
    tools.register(Arc::new(myclaw::tools::MemoryViewTool::new(kd.clone())));
    tools.register(Arc::new(myclaw::tools::MemorySearchTool::new(kd.clone())));
    tools.register(Arc::new(myclaw::tools::MemoryManageTool::new(kd)));
    let tools_arc = Arc::new(tools);

    let mut runtime = myclaw::AgentRuntime::new(
        registry_arc,
        tools_arc,
        skills_arc,
        myclaw::AgentRegistry::new(),
    )
    .with_dirs(workspace_dir.clone(), std::path::PathBuf::from(&cfg.knowledge_dir));

    if let Some(m) = model {
        runtime.agent_config.model_override = Some(m.to_string());
    }

    let session_key = agent.unwrap_or("cli");
    let session = myclaw::Session::new(session_key.to_string());
    let mut session_wrap = runtime.create_session(session, None);

    let response = session_wrap.run(prompt, None, None).await?;

    match format {
        "json" => {
            let out = serde_json::json!({"response": response});
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        _ => println!("{}", response),
    }

    Ok(())
}