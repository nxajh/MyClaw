# MyClaw 目标架构图（重构后）

> 基于 RFC: Session 架构重构 v2
> 日期：2026-05-21

## 组件关系总览

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          config.toml (全局配置)                              │
│                                                                              │
│  [runtime]   timezone                                                        │
│  [limits]    max_tool_calls / max_history / tool_timeout_secs / ...          │
│  [context]   compact_threshold / retain_work_units                           │
│  [defaults]  permission_mode / model                                         │
│  [prompt]    max_chars / bootstrap_max_chars / native_tools                  │
│  [providers] [channels] [mcp_servers] [scheduler] [users]                    │
└──────────────────────────────────────────────────────────────────────────────┘
                                       │ 由 daemon.rs 加载
                                       ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│                          daemon.rs (Composition Root)                        │
│                                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐    │
│  │ServiceRegistry│  │ ToolRegistry │  │ SkillManager │  │  McpManager  │    │
│  │(LLM providers)│  │ (tool impls) │  │  Arc<RwLock> │  │  (MCP 连接)  │    │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘    │
│                                            ▲                                 │
│                                            │ 自动 reload                     │
│  ┌──────────────┐  ┌──────────────┐  ┌────┴─────────┐  ┌──────────────┐    │
│  │ContextEngine │  │  AskRouter   │  │ Workspace    │  │ Delegation   │    │
│  │ (compaction) │  │(pending_asks)│  │ Watcher      │  │ Coordinator  │    │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘    │
│                                                                              │
│  ┌──────────────┐  ┌──────────────┐                                         │
│  │ UserResolver │  │AgentRegistry │                                         │
│  │(rk→user_id)  │  │HashMap<name, │                                         │
│  └──────────────┘  │  Arc<Agent>> │                                         │
│                    └──────┬───────┘                                          │
│                           │                                                  │
│  ┌────────────────────────┴────────────────────────────────────────────┐    │
│  │                       SessionManager                                │    │
│  │                                                                     │    │
│  │  backend  ────→ SessionBackend (持久化)                              │    │
│  │  agents   ────→ AgentRegistry                                       │    │
│  │  active   ────→ HashMap<routing_key, session_id>     ← 仅指针       │    │
│  │  contexts ───→ HashMap<session_id, Arc<SessionContext>>             │    │
│  │                                                                     │    │
│  │  switch_session() = 改 active 指针，天然一致                         │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │                       Orchestrator (瘦身版)                          │   │
│  │                                                                      │   │
│  │  session_manager: Arc<SessionManager>                                │   │
│  │  channels:        HashMap<(type, account), Arc<dyn Channel>>         │   │
│  │  ask_router:      Arc<AskRouter>                                     │   │
│  │                                                                      │   │
│  │  职责：收消息 → 找 SessionContext → 投递（无 actor、无 mutex 间接）   │   │
│  └──────────┬──────────────────────────────────────┬────────────────────┘   │
│             │                                      │                         │
│  ┌──────────┼──────────────┐        ┌──────────────┤                         │
│  ▼          ▼              ▼        ▼              ▼                         │
│┌────────┐┌──────────┐┌──────────┐┌──────────┐┌──────────┐                   │
││Telegram││ClientChan││ QQBot    ││ Webhook  ││Scheduler │  全部走           │
││Channel ││(WebUI WS)││ Channel  ││ Handler  ││(cron/hb) │  SessionManager   │
│└────────┘└──────────┘└──────────┘└──────────┘└──────────┘  .get_context()   │
└──────────────────────────────────────────────────────────────────────────────┘


SessionContext（per session，唯一 owner）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

┌──────────────────────────────────────┐
│        SessionContext                │
│                                      │
│  session: Mutex<Session>             │ ← 对话数据（唯一 owner）
│    ├─ history: Vec<ChatMessage>      │
│    ├─ message_ids: Vec<i64>          │
│    ├─ compact_version: u32           │
│    └─ session_override: SessionOverride
│                                      │
│  agent: Arc<Agent>                   │ ← 绑定 agent（创建时选定）
│  user_profile: Arc<UserProfile>      │ ← 用户信息
│                                      │
│  attachments: Mutex<                 │ ← per-session 增量通告状态
│    AttachmentManager>                │   （跨 turn 保留，不持久化）
│  pending_retry: Mutex<Option<String>>│ ← 空回复时的待重试消息
│                                      │
│  turn_lock: tokio::Mutex<()>         │ ← turn 串行化
│                                      │
│  process_turn(input, channel,        │
│               reply_target, env) {   │
│    turn_lock.lock()                  │
│    channel.on_status(Thinking)       │
│    agent.run(&mut session, input,    │
│              &turn_ctx)              │
│    channel.on_status(Done)           │
│  }                                   │
└──────────────────────────────────────┘

注意：channel 不存储，每轮注入 — 同一 session 可跨通道访问。


Agent（同类型，不同配置的实例）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

┌──────────────────────────────────┐
│           Agent                  │
│                                  │
│  config: AgentConfig             │
│    ├─ name                       │
│    ├─ description                │
│    ├─ tools: ToolFilter          │ ← 能用哪些内置 tool
│    ├─ skills: SkillFilter        │ ← 绑哪些 skill
│    ├─ mcp: McpFilter             │ ← 允许哪些 MCP server
│    └─ system_prompt              │ ← AGENT.md body
│                                  │
│  cached_prompt: String           │ ← 启动时算好的完整 prompt（不含 profile）
│                                  │
│  // 共享基础设施引用（不拥有）     │
│  registry: Arc<dyn ServiceRegistry>
│  tool_impls: Arc<ToolRegistry>   │
│  skills: Arc<RwLock<SkillManager>>
│  context_engine: Arc<ContextEngine>
│                                  │
│  run(&mut session, input, ctx) { │ ← 无状态执行一轮 turn
│    LoopBreaker (局部变量)         │   limits 从 ctx.global 读
│    ...调 LLM、执行 tool、写 history
│  }                               │
└──────────────────────────────────┘

"主 agent" vs "子代理" = 同一个 struct，不同实例：
┌──────────┐  ┌──────────┐  ┌──────────────┐
│ Agent    │  │ Agent    │  │ Agent        │
│ "main"   │  │ "coder"  │  │ "researcher" │
│          │  │          │  │              │
│ tools:   │  │ tools:   │  │ tools:       │
│  All     │  │ Allow([  │  │ Allow([      │
│          │  │  shell,  │  │  web_search])│
│ skills:  │  │  file_*])│  │              │
│  All     │  │ skills:  │  │ skills: None │
│          │  │ Allow([  │  │              │
│ mcp:     │  │  code-   │  │ mcp: None    │
│  All     │  │  review])│  │              │
│          │  │ mcp:     │  │              │
│ prompt:  │  │ Allow([  │  │ prompt:      │
│ AGENT.md │  │  github])│  │ AGENT.md     │
│   body   │  │ prompt:  │  │   body       │
│          │  │ AGENT.md │  │              │
│          │  │   body   │  │              │
└──────────┘  └──────────┘  └──────────────┘
```

## TurnContext / TurnInput / TurnResult

```rust
struct TurnContext<'a> {
    channel: &'a dyn Channel,
    reply_target: &'a str,
    user_profile: &'a UserProfile,
    attachments: &'a mut AttachmentManager,

    ask_router: &'a AskRouter,
    delegator: &'a dyn AgentDelegator,
    persist: &'a dyn PersistHook,

    global: &'a GlobalConfig,
    session_override: &'a SessionOverride,

    stream: Option<TurnStream<'a>>,
}

struct TurnInput {
    text: String,
    image_urls: Option<Vec<String>>,
    image_base64: Option<Vec<String>>,
}

struct TurnResult {
    text: String,
    stop_reason: StopReason,
    pending_retry: Option<String>,
}
```

## 消息流转

```
                              用户
                               │
                               ▼
                      ┌────────────────┐
                      │    Channel     │
                      │  .listen()     │
                      └───────┬────────┘
                              │ ChannelMessage
                              ▼
                      ┌────────────────┐
                      │  Orchestrator  │
                      └───────┬────────┘
                              │
                   ① 解析 routing_key
                   ② session_manager.get_context(routing_key)
                              │
                              ▼
                      ┌──────────────────┐
                      │  SessionContext  │
                      │                  │
                      │  process_turn(   │
                      │    input,        │
                      │    channel,      │ ← 每轮注入
                      │    reply_target, │
                      │    env)          │
                      │    │             │
                      │    ├ turn_lock   │
                      │    │             │
                      │    ▼             │
                      │  agent.run(      │
                      │    &mut session, │
                      │    input,        │
                      │    &turn_ctx     │
                      │  )               │
                      │    │             │
                      │    ├─ 构建 LLM request
                      │    ├─ 调 ServiceRegistry → LLM API
                      │    ├─ tool_call → tool_executor.execute()
                      │    ├─ 写 session.history
                      │    ├─ persist_message()
                      │    └─ 返回 TurnResult
                      └───────┬──────────┘
                              │
                              ▼
                     channel.send() → 用户
```

对比现在的四层间接：
```
现在：  orchestrator → tx.send() → actor(rx) → mutex.lock(AgentLoop) → run()
重构后：orchestrator → session_ctx.process_turn(input, channel, ...)
                                           └→ agent.run(&mut session, ...)
```

## 数据持有关系

```
AgentRegistry (全局)
  └─ HashMap<String, Arc<Agent>>
       │
       ├─ "main" ────→ Agent
       │                ├─ config: AgentConfig（仅 5 字段）
       │                ├─ cached_prompt: String
       │                ├─ registry/tool_impls/skills (Arc 引用)
       │                └─ context_engine (Arc 引用)
       │
       ├─ "coder" ──→ Agent (同结构，不同配置)
       └─ "researcher" → Agent

SessionManager (全局)
  ├─ backend ────→ Arc<dyn SessionBackend>
  ├─ agents ─────→ Arc<AgentRegistry>
  ├─ active ─────→ HashMap<routing_key, session_id>      ← 仅指针
  └─ contexts ──→ HashMap<session_id, Arc<SessionContext>>
                     │
                     └─ SessionContext
                          ├─ session: Mutex<Session>            ← 唯一 owner ✅
                          ├─ agent: Arc<Agent>                  ← 引用 AgentRegistry
                          ├─ user_profile: Arc<UserProfile>
                          ├─ attachments: Mutex<AttachmentManager>
                          ├─ pending_retry: Mutex<Option<String>>
                          └─ turn_lock: tokio::Mutex<()>

Orchestrator (全局，瘦身后)
  ├─ session_manager: Arc<SessionManager>
  ├─ channels: HashMap<(type, account), Arc<dyn Channel>>
  ├─ ask_router: Arc<AskRouter>
  └─ listener_handles: Vec<JoinHandle<()>>

ContextEngine (全局单例)
  ├─ threshold: f64                    ← 从 GlobalConfig.context 读
  ├─ retain_work_units: usize          ← 从 GlobalConfig.context 读
  └─ registry (Arc 引用)
  对外接口：should_compact / compact，session/agent 不感知配置细节

WorkspaceWatcher (全局，自维护)
  ├─ skills: Arc<RwLock<SkillManager>>      ← 直接 own，文件变更自动更新
  ├─ sub_agents: Arc<RwLock<Vec<AgentConfig>>>
  └─ 内部 spawn 文件监听 task
  Agent.run() 读 RwLock 时自动看到最新值，
  AttachmentManager 在下一轮 turn 算 diff 时通告变化

McpManager (全局)
  启动时 connect() → 注册 MCP tools 到 ToolRegistry
  每个 tool 标注来源 server name → Agent.mcp filter 据此过滤

Scheduler (全局)
  ├─ jobs (RwLock<JobsFile>)
  ├─ heartbeat_config
  └─ event_tx → mpsc → Orchestrator
  触发时走 SessionManager.get_context()，不持 Agent/sessions

UserResolver (全局)
  └─ explicit: HashMap<routing_key, user_id>（来自 config.toml）
     未配置时透传，user_id = routing_key

Memory (per-user)
  └─ MemoryList/View/Search/Manage tool
     存储路径：workspace/users/{user_id}/memory/{type}/

UserProfile (per user)
  ├─ id, name, timezone, language, preferences, free_form
  └─ 存储：workspace/users/{user_id}/profile.md + preferences.md
```

## Session 操作

```
switch_session(routing_key, session_id)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  active[routing_key] = session_id      ← 改指针
  下次 get_context() 自动返回新的 SessionContext
  不需要 evict，不需要重建 AgentLoop


create_session(routing_key, agent_name, ...)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  session = backend.create(name)
  agent   = agents.get(agent_name)
  profile = UserProfile::load(user_resolver.resolve(routing_key), ...)
  context = SessionContext::new(session, agent, profile)
  contexts[session.id] = context
  active[routing_key] = session.id


delete_session(routing_key, session_id)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  contexts.remove(session_id)            ← drop SessionContext
  if active[routing_key] == session_id:
    active.remove(routing_key)           ← 或切到默认 session


子代理委派
━━━━━━━━━━━
  delegation_coord.delegate(agent_name, task, parent_sk, ...)
    ├─ 解析 SessionOverride.isolation
    ├─ 如果 worktree: 创建 git worktree
    ├─ sub_sk = "sub:{parent_sk}:{agent_name}:{uuid}"
    ├─ sub_ctx = session_manager.create_session(sub_sk, agent_name, ...)
    ├─ sub_ctx.process_turn(task, parent_channel, ...)
    └─ 如果 worktree: merge + cleanup
```

## 删除的组件

```
❌ AgentLoop           → Agent.run() 是无状态方法，不需要 struct
❌ SessionHandle       → SessionContext 直接提供 process_turn()
❌ LoopRegistry        → 职责回归 SessionManager
❌ run_session_actor   → turn_lock 替代 channel + actor
❌ TurnMessage/TurnInput → process_turn() 直接接收参数
❌ get_or_create()     → 拆散到 SessionManager.create_session
❌ SchedulerContext    → 统一走 SessionManager
❌ WebhookContext      → 大幅简化，只持 SessionManager 引用
❌ SubAgentDelegator   → DelegationCoordinator + AgentRegistry
❌ RequestBuilder      → 拆为 Agent.cached_prompt / SessionContext.attachments /
                         WorkspaceWatcher / TurnInput / 无状态函数
❌ CompactionPolicy    → 并入 ContextEngine
❌ CompactionExecutor  → 并入 ContextEngine
❌ AskUserHandler      → AskRouter
❌ DelegateHandler     → AgentDelegator trait
❌ IDENTITY.md/SOUL.md/RULES.md → 内容写入 main/AGENT.md body
❌ USER.md (全局)       → per-user UserProfile

保留不变的：
✅ McpManager          → 启动时注册 MCP tools，逻辑不变
✅ ToolRegistry        → 全局 tool 实现池
✅ Memory tools        → 4 个 tool 不变，存储路径改 per-user
✅ FileWatcher (改名 WorkspaceWatcher) → 自维护 RwLock
✅ Scheduler           → jobs.json + event_tx，触发方式不变

保留但瘦身的：
🔧 Orchestrator     → 只做路由：收消息 → 找 context → 投递
🔧 SessionManager   → contexts 管理，cache 去掉
🔧 Agent            → 工厂 → 无状态执行器
🔧 prompt.rs        → 去掉文件读取，prompt 组合参数化
```

## 通信方式

```
Channel ──ChannelMessage──→ mpsc ──→ Orchestrator.run()
                                          │
                                          └── session_ctx.process_turn(
                                                  input, channel, reply_target, env)
                                                  │
                                                  ├── agent.run(&mut session, input, &turn_ctx)
                                                  │       │
                                                  │       └── tool 需要问用户时：
                                                  │           ask_router.ask(sk, channel, ...)
                                                  │            ├─ channel.send(question)
                                                  │            ├─ pending_asks.insert(sk, tx)
                                                  │            └─ oneshot_rx.await → answer
                                                  │
                                                  └── channel.send(response) → 用户

WebUI API ──WebSocket JSON──→ ClientChannel
              ├── sessions.switch → session_manager.switch_session()  ← 天然一致
              ├── sessions.create → session_manager.create_session()
              ├── sessions.delete → session_manager.delete_session()
              ├── sessions.list   → session_manager.list_sessions()
              ├── tools.list      → agents["main"].tool_specs
              └── config/skills   → 同现在

Scheduler ──SchedulerEvent──→ Orchestrator.run()
              └── session_manager.get_context(scheduled_sk)
                  → 走统一路径，SessionOverride.run_mode = Background

Webhook ──HTTP request──→ WebhookHandler
            └── session_manager.get_context(webhook_sk)
                → 走统一路径，SessionOverride.run_mode = Background

WorkspaceWatcher ──文件变更──→ 直接更新 skills/sub_agents RwLock
                Agent.run 读 RwLock，AttachmentManager 在 turn 内 diff 出变化

Memory ──tool 调用──→ MemoryManage/Search/List/View
          读写 workspace/users/{user_id}/memory/{type}/
          user_id 从 SessionContext.user_profile 获取

McpManager ──启动时──→ connect(config.mcp_servers)
             ├─ 注册 MCP tools 到 ToolRegistry（全局）
             └─ Agent.mcp filter 决定哪些 MCP tool 可用
```

## 统一入口路径

```
现在（四条独立路径）：
  用户消息  → Orchestrator → LoopRegistry → AgentLoop             ← 路径 A
  Cron触发  → SchedulerContext → 临时构建 AgentLoop               ← 路径 B
  Webhook   → WebhookContext → 临时构建 AgentLoop                 ← 路径 C
  子代理    → SubAgentDelegator → 临时构建 AgentLoop              ← 路径 D

重构后（一条统一路径）：
  全部 → SessionManager.get_context(routing_key)
       → SessionContext.process_turn(input, channel, reply_target, env)
       → agent.run(&mut session, input, &turn_ctx)

  区别只是 sk 生成规则和 SessionOverride：
  ┌──────────┬──────────────┬───────────┬─────────────┬────────────┐
  │ 来源      │ agent        │ channel   │ run_mode    │ 备注       │
  ├──────────┼──────────────┼───────────┼─────────────┼────────────┤
  │ 用户消息  │ "main"       │ 来源通道  │ Interactive │            │
  │ Cron     │ "main"       │ last_chan │ Background  │            │
  │ Webhook  │ "main"       │ last_chan │ Background  │            │
  │ 子代理    │ "coder" 等   │ 同父 ctx  │ Interactive │ worktree?  │
  │ Heartbeat│ "main"       │ 指定通道  │ Background  │ ephemeral  │
  └──────────┴──────────────┴───────────┴─────────────┴────────────┘
```

## Agent 配置

```
workspace/agents/
├── main/
│   └── AGENT.md
├── coder/
│   └── AGENT.md
└── researcher/
    └── AGENT.md

──────────────────────────────────────────────
# workspace/agents/coder/AGENT.md
---
name: coder
description: "Expert programmer for writing and editing code"
tools: [shell, file_read, file_write, file_edit]
skills: [code-review]
mcp: [github, filesystem]
---

You are an expert programmer. Write clean, idiomatic code.

## Behavioral Principles

Be concise. Don't over-engineer.
──────────────────────────────────────────────

Prompt 构建：
  builtin_sections(permission_mode, run_mode, native_tools)
  + AGENT.md body
  + user_profile.to_prompt_section()
  + runtime_info()
  + skill_instructions(filter)
```
