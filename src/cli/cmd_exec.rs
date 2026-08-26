//! `myclaw exec` — non-interactive single prompt execution.

use anyhow::Result;
use std::sync::Arc;

use crate::cli::Cli;

pub async fn run(
    cli: &Cli,
    prompt: &str,
    agent: Option<&str>,
    model: Option<&str>,
    format: &str,
) -> Result<()> {
    let cfg = super::load_config(cli)?;
    super::init_tracing(&cfg);
    myclaw::tools::shell_env::init(cfg.shell.clone());

    let registry = myclaw::providers::registry::Registry::from_config(cfg.providers.clone(), &cfg.routing)
        .map_err(|e| anyhow::anyhow!("failed to build registry: {}", e))?;
    let registry_arc: Arc<dyn myclaw::ProviderRegistry> = Arc::new(registry);

    let mut tools = myclaw::ToolRegistry::new();
    let (builtin, _shell_registry, _shell_tool) = myclaw::tools::builtin_tools(None, None);
    for t in builtin {
        tools.register(t);
    }
    tools.register(Arc::new(myclaw::tools::ListDirTool::new()));
    let skills_arc: Arc<parking_lot::RwLock<myclaw::SkillManager>> =
        Arc::new(parking_lot::RwLock::new(myclaw::SkillManager::new()));
    // P1-B2: memory pool is {base_dir}/memory; audit log lives under data.
    let memory_root = cfg.memory_root();
    let base_dir = cfg.base_dir.clone();
    tools.register(Arc::new(myclaw::tools::SkillTool::new(Arc::clone(
        &skills_arc,
    ))));
    tools.register(Arc::new(myclaw::tools::SkillsListTool::new(Arc::clone(
        &skills_arc,
    ))));
    tools.register(Arc::new(myclaw::tools::SkillManageTool::new(
        Arc::clone(&skills_arc),
        cfg.skills_root(),
        cfg.agents_skills_dir_opt(),
    )));
    // Memory tools — P1-B2 flat memory root, ownership via frontmatter.
    let resolver = Arc::new(myclaw::UserResolver::new());
    tools.register(Arc::new(myclaw::tools::MemoryListTool::new(
        memory_root.clone(),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::MemoryViewTool::new(
        memory_root.clone(),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::MemorySearchTool::new(
        memory_root.clone(),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::MemoryManageTool::new(
        memory_root,
        base_dir,
        resolver,
    )));
    let tools_arc = Arc::new(tools);

    // RFC v2: Agent + AgentRuntime. CLI doesn't need delegation.
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
            myclaw::api::loop_breaker::LoopBreakerConfig::default(),
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

    session.add_user(prompt.to_string());
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
    let response = result.text;

    match format {
        "json" => {
            let out = serde_json::json!({"response": response});
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        _ => println!("{}", response),
    }

    Ok(())
}
