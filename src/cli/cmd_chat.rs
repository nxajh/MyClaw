//! `myclaw chat` — interactive chat session (REPL).

use anyhow::Result;
use std::sync::Arc;

use crate::cli::Cli;

pub async fn run(
    cli: &Cli,
    prompt: Option<&str>,
    agent: Option<&str>,
    model: Option<&str>,
    print: bool,
) -> Result<()> {
    let cfg = super::load_config(cli)?;
    super::init_tracing(&cfg);

    let registry = myclaw::registry::Registry::from_config(cfg.providers.clone(), &cfg.routing)
        .map_err(|e| anyhow::anyhow!("failed to build registry: {}", e))?;
    let registry_arc: Arc<dyn myclaw::ProviderRegistry> = Arc::new(registry);

    let mut tools = myclaw::ToolRegistry::new();
    for t in myclaw::tools::builtin_tools(None) {
        tools.register(t);
    }
    tools.register(Arc::new(myclaw::tools::ListDirTool::new()));
    let skills_arc: Arc<parking_lot::RwLock<myclaw::SkillManager>> =
        Arc::new(parking_lot::RwLock::new(myclaw::SkillManager::new()));
    let workspace_dir = std::path::PathBuf::from(&cfg.workspace_dir);
    tools.register(Arc::new(myclaw::tools::SkillTool::new(Arc::clone(
        &skills_arc,
    ))));
    tools.register(Arc::new(myclaw::tools::SkillsListTool::new(Arc::clone(
        &skills_arc,
    ))));
    tools.register(Arc::new(myclaw::tools::SkillManageTool::new(
        Arc::clone(&skills_arc),
        workspace_dir.clone(),
    )));
    // Memory tools (G43: workspace/users/{uid}/memory/, identity resolver in CLI mode)
    let resolver = Arc::new(myclaw::UserResolver::new());
    tools.register(Arc::new(myclaw::tools::MemoryListTool::new(
        workspace_dir.clone(),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::MemoryViewTool::new(
        workspace_dir.clone(),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::MemorySearchTool::new(
        workspace_dir.clone(),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::MemoryManageTool::new(
        workspace_dir,
        resolver,
    )));
    let tools_arc = Arc::new(tools);

    // RFC v2: Agent (executor) + AgentRuntime (resources). CLI doesn't
    // need per-session caching or sub-agent delegation, so we build a
    // minimal runtime with an empty AgentRegistry + default executors.
    let agent_runtime = {
        let sub_agents = Arc::new(myclaw::agents::AgentRegistry::new());
        let resources = myclaw::agents::resource_provider::ResourceProvider::new(
            Arc::clone(&skills_arc),
            Arc::clone(&sub_agents),
            Vec::new(),
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
            String::new(),
            0,
        );
        let context_engine = Arc::new(myclaw::agents::context_engine::ContextEngine::new(
            &Default::default(),
            Arc::clone(&registry_arc),
            resources,
            Arc::clone(&tools_arc),
        ));
        let tool_executor = Arc::new(myclaw::agents::tool_executor::ToolExecutor::new(180));
        let loop_breaker = Arc::new(myclaw::agents::loop_breaker::LoopBreaker::new(
            myclaw::agents::loop_breaker::LoopBreakerConfig::default(),
        ));
        myclaw::AgentRuntime::new(
            Arc::clone(&registry_arc),
            Arc::clone(&tools_arc),
            Arc::clone(&skills_arc),
            sub_agents,
            context_engine,
            tool_executor,
            loop_breaker,
        )
    };

    let main_config = myclaw::config::sub_agent::SubAgentConfig {
        name: "main".to_string(),
        description: None,
        system_prompt: String::new(),
        tools: myclaw::config::filters::ToolFilter::all(),
        skills: Default::default(),
        mcp: Default::default(),
        model: model.map(|s| s.to_string()),
        max_tool_calls: None,
        isolation: Default::default(),
        timeout: None,
    };
    let agent_obj = myclaw::Agent::new(main_config);

    let session_key = agent.unwrap_or("cli");
    let mut session = myclaw::Session::new(session_key.to_string());
    let model_owned = model.map(|s| s.to_string());

    // Non-interactive (--print) or single prompt mode.
    if print || prompt.is_some() {
        let input = prompt.unwrap_or("Hello");
        session.add_user(input.to_string());
        let turn_ctx = myclaw::TurnContext {
            system_prompt: "",
            model_id: model_owned.as_deref(),
            thinking: None,
            permission_mode: myclaw::PermissionMode::default(),
            run_mode: myclaw::RunMode::default(),
        };
        let result = agent_obj
            .run(&mut session, turn_ctx, &agent_runtime)
            .await?;
        println!("{}", result.text);
        return Ok(());
    }

    // Interactive REPL.
    eprintln!("MyClaw Chat — type 'exit' or press Ctrl-D to quit.");
    let model_display = cfg
        .routing
        .get(myclaw::providers::Capability::Chat)
        .and_then(|r| r.models.first().cloned())
        .unwrap_or_else(|| "(none)".to_string());
    eprintln!("Model: {}", model_display);
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
        if input.is_empty() {
            continue;
        }
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            break;
        }

        session.add_user(input.to_string());
        let turn_ctx = myclaw::TurnContext {
            system_prompt: "",
            model_id: model_owned.as_deref(),
            thinking: None,
            permission_mode: myclaw::PermissionMode::default(),
            run_mode: myclaw::RunMode::default(),
        };
        match agent_obj.run(&mut session, turn_ctx, &agent_runtime).await {
            Ok(result) => println!("{}\n", result.text),
            Err(e) => eprintln!("error: {}\n", e),
        }
    }

    Ok(())
}
