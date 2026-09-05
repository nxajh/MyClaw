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
    user: &str,
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
    // Owner normalization for skill user-layer lookups and writes (issue
    // #101): CLI has no bound identity, so a fresh in-memory resolver is
    // used — `resolve` falls through to the input unchanged (behavior
    // identical to before the injection). Shared by skill_* and memory
    // tools. Declared before the skill tool registrations that use it.
    let resolver = Arc::new(myclaw::UserResolver::new());
    tools.register(Arc::new(myclaw::tools::SkillTool::new(
        Arc::clone(&skills_arc),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::SkillsListTool::new(
        Arc::clone(&skills_arc),
        Arc::clone(&resolver),
    )));
    tools.register(Arc::new(myclaw::tools::SkillManageTool::new(
        Arc::clone(&skills_arc),
        Arc::clone(&resolver),
        cfg.base_dir.join("users"),
        cfg.skills_root(),
        cfg.agents_skills_dir_opt(),
    )));
    // Memory tools — P1-B2 flat memory root, ownership via frontmatter.
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
        let context_engine = Arc::new(myclaw::agents::compaction_engine::CompactionEngine::new(
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
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(myclaw::Session::new(
        session_key.to_string(),
    )));
    let mut session = session.lock_owned().await;

    // #101 P2: CLI identity — required `--user` (shared helper with
    // `exec`, see `cli::resolve_cli_identity`). Same FQID shape as
    // daemon-side load_session.
    session.owner_fqid = super::resolve_cli_identity(&cfg, user)?;
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
        match agent_obj
            .run(&mut session, turn_ctx, &agent_runtime)
            .await
        {
            Ok(result) => println!("{}\n", result.text),
            Err(e) => eprintln!("error: {}\n", e),
        }
    }

    Ok(())
}
