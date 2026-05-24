//! `myclaw chat` — interactive chat session (REPL).

use anyhow::Result;
use std::sync::Arc;

use crate::cli::Cli;

pub async fn run(cli: &Cli, prompt: Option<&str>, agent: Option<&str>, model: Option<&str>, print: bool) -> Result<()> {
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

    // Non-interactive (--print) or single prompt mode.
    if print || prompt.is_some() {
        let input = prompt.unwrap_or("Hello");
        let response = session_wrap.run(input, None, None).await?;
        println!("{}", response);
        return Ok(());
    }

    // Interactive REPL.
    eprintln!("MyClaw Chat — type 'exit' or press Ctrl-D to quit.");
    let model = cfg
        .routing
        .get(myclaw::providers::Capability::Chat)
        .and_then(|r| r.models.first().cloned())
        .unwrap_or_else(|| "(none)".to_string());
    eprintln!("Model: {}", model);
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

        match session_wrap.run(input, None, None).await {
            Ok(response) => println!("{}\n", response),
            Err(e) => eprintln!("error: {}\n", e),
        }
    }

    Ok(())
}
