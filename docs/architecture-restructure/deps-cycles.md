# 模块环检测（Tarjan SCC，use 语句边）


## 环: registry ↔ mcp ↔ storage ↔ tools ↔ channels ↔ providers ↔ config ↔ agents
- registry → providers: 16 处 / 2 文件 (registry/mod.rs, registry/routing.rs)
- registry → config: 0 处 / 0 文件 ()
- mcp → providers: 0 处 / 0 文件 ()
- mcp → agents: 0 处 / 0 文件 ()
- storage → channels: 5 处 / 1 文件 (storage/inbound_spool.rs)
- storage → providers: 2 处 / 1 文件 (storage/json_file.rs)
- tools → storage: 3 处 / 2 文件 (tools/agent_resume.rs, tools/session_query.rs)
- tools → channels: 7 处 / 3 文件 (tools/ask_user.rs, tools/friends.rs, tools/send_message.rs)
- tools → providers: 70 处 / 30 文件 (tools/agent_kill.rs, tools/agent_list.rs, tools/agent_resume.rs, tools/ask_user.rs, tools/calculator.rs, tools/cronjob_tool.rs)
- tools → config: 1 处 / 1 文件 (tools/shell_env.rs)
- tools → agents: 42 处 / 19 文件 (tools/agent_kill.rs, tools/agent_list.rs, tools/agent_resume.rs, tools/ask_user.rs, tools/cronjob_tool.rs, tools/delegate.rs)
- channels → providers: 3 处 / 1 文件 (channels/client.rs)
- channels → config: 6 处 / 4 文件 (channels/client.rs, channels/qqbot/channel.rs, channels/telegram/channel.rs, channels/wechat.rs)
- channels → agents: 8 处 / 3 文件 (channels/client.rs, channels/telegram/channel.rs, channels/turn_stream.rs)
- providers → config: 1 处 / 1 文件 (providers/provider_factory.rs)
- providers → agents: 1 处 / 1 文件 (providers/capability_tool.rs)
- config → providers: 2 处 / 2 文件 (config/mod.rs, config/routing.rs)
- config → agents: 1 处 / 1 文件 (config/mod.rs)
- agents → registry: 4 处 / 1 文件 (agents/media_e2e_test.rs)
- agents → mcp: 1 处 / 1 文件 (agents/mcp_manager.rs)
- agents → storage: 10 处 / 6 文件 (agents/context_engine.rs, agents/orchestrator/inbound.rs, agents/orchestrator/mod.rs, agents/orchestrator/recovery.rs, agents/session/backend.rs, agents/session/manager.rs)
- agents → tools: 1 处 / 1 文件 (agents/runtime.rs)
- agents → channels: 32 处 / 14 文件 (agents/ask_router.rs, agents/commands/friends.rs, agents/orchestrator/ctx.rs, agents/orchestrator/delegation.rs, agents/orchestrator/inbound.rs, agents/orchestrator/mod.rs)
- agents → providers: 82 处 / 24 文件 (agents/agent.rs, agents/attachment.rs, agents/context_engine.rs, agents/llm_stream.rs, agents/media_e2e_test.rs, agents/memory_distill.rs)
- agents → config: 28 处 / 15 文件 (agents/agent.rs, agents/agent_registry.rs, agents/commands/config.rs, agents/context_engine.rs, agents/delegation_coordinator.rs, agents/mcp_manager.rs)

# 二元环快速清单

- **agents ↔ config**
  - agents→config: 28 use / 15 文件
  - config→agents: 1 use / 1 文件
- **agents ↔ providers**
  - agents→providers: 82 use / 24 文件
  - providers→agents: 1 use / 1 文件
- **agents ↔ channels**
  - agents→channels: 32 use / 14 文件
  - channels→agents: 8 use / 3 文件
- **agents ↔ tools**
  - agents→tools: 1 use / 1 文件
  - tools→agents: 42 use / 19 文件
- **agents ↔ mcp**
  - agents→mcp: 1 use / 1 文件
  - mcp→agents: 0 use / 0 文件
- **config ↔ providers**
  - config→providers: 2 use / 2 文件
  - providers→config: 1 use / 1 文件
