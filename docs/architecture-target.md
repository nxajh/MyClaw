# MyClaw 目标架构图（重构后）

> 基于 RFC: Session 架构重构 v2
> 日期：2026-05-21

## 组件关系总览

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          config.toml (全局配置)                              │
│                                                                              │
│  [locale]    timezone                                                        │
│  [prompt]    max_chars / bootstrap_max_chars / native_tools                  │
│  [agent]     permission_mode                                                 │
│  [tool_executor]   timeout_secs                                              │
│  [loop_breaker]    max_tool_calls / threshold                                │
│  [context_engine]  compact_threshold / retain_work_units                     │
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
│  │  resolver ────→ UserResolver                                        │    │
│  │  sessions ───→ RwLock<HashMap<routing_key, Arc<SessionContext>>>   │    │
│  │     ★ 不变量：所有 ctx 的 session.id 互不重复（1:1 with rk）        │    │
│  │                                                                     │    │
│  │  switch_session(rk, sid) = 冲突检查 → 重新加载 ctx                  │    │
│  │  create_sub_session() = 返回 Arc 但不进表，调用方持引用             │    │
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
│    ├─ isolation: AgentIsolation  │ ← 作为 sub-agent 时是否要 worktree
│    └─ system_prompt              │ ← AGENT.md body
│                                  │
│  cached_prompt: String           │ ← AGENT.md body 部分（builtin/profile 等
│                                  │   由 process_turn 每轮拼上去）
│                                  │
│  // 共享基础设施引用（不拥有）     │
│  registry: Arc<dyn ServiceRegistry>
│  skills: Arc<RwLock<SkillManager>>
│  context_engine: Arc<ContextEngine>
│  ask_router: Arc<AskRouter>
│  delegator: Arc<dyn AgentDelegator>
│  tool_executor: Arc<ToolExecutor>
│  loop_breaker: Arc<LoopBreaker>
│                                  │
│  run(&mut session, input, ctx) { │ ← 无状态执行一轮 turn
│    let mut counter =             │   counter 从 self.loop_breaker.new_counter()
│      self.loop_breaker.new_counter();
│    let stream = self.registry    │
│      .chat_stream(...).await?;   │
│    let resp = llm_stream::read(  │ ← 模块级函数，硬编码 timeout/byte limit
│      stream).await?;             │
│    for call in resp.tool_calls { │
│      counter.tick(&call)?;       │
│      self.tool_executor.execute( │ ← 内部用自己的 tool_timeout
│        &call, session, ...).await?;
│    }                             │
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
    // process_turn 已 resolve 为标量后传入
    system_prompt: &'a str,
    model_id: &'a str,
    thinking: Option<&'a ThinkingConfig>,
    permission_mode: PermissionMode,
    run_mode: RunMode,

    // 流式（None = Collect 模式）
    stream: Option<TurnStream<'a>>,
}

// 注：persist 由 Session 自己负责，channel/reply_target 不再传入
// （channel 由 AskRouter 内部解析，reply_target 存于 session.last_reply_target）
// ask_router / delegator / tool_executor / loop_breaker / context_engine
// 都是 Agent 字段（全局 Arc），不在 TurnContext 里

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
       │                ├─ config: AgentConfig (name/desc/tools/skills/mcp/isolation/system_prompt)
       │                ├─ cached_prompt: String
       │                └─ Arc 引用：
       │                   registry, skills, context_engine,
       │                   ask_router, delegator,
       │                   tool_executor, loop_breaker
       │
       ├─ "coder" ──→ Agent (同结构，不同配置)
       └─ "researcher" → Agent

SessionManager (全局)
  ├─ backend  ───→ Arc<dyn SessionBackend>
  ├─ agents   ───→ Arc<AgentRegistry>
  ├─ resolver ───→ Arc<UserResolver>
  └─ sessions ───→ RwLock<HashMap<routing_key, Arc<SessionContext>>>
        ★ 1:1 不变量：表内 SessionContext 的 session.id 互不重复
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
  ├─ compact_threshold / retain_work_units    ← from [context_engine]
  ├─ registry / tools (Arc 引用)
  └─ TokenTracker 不在这里 — 挪进 Session.token_tracker（per-session 状态）
  对外接口：should_compact / compact

ToolExecutor (全局单例)
  ├─ timeout: Duration                         ← from [tool_executor].timeout_secs
  └─ tools: Arc<ToolRegistry>
  对外接口：execute(call, &mut session, ...)

LoopBreaker (全局单例，发放 per-turn counter)
  ├─ max_tool_calls                            ← from [loop_breaker].max_tool_calls
  └─ threshold                                 ← from [loop_breaker].threshold
  Agent.run() 每轮调 new_counter() 创建 LoopBreakerCounter（持 policy 借用 + tool_count + VecDeque）

llm_stream（模块级，非 struct）
  const FIRST_CHUNK_TIMEOUT: Duration = 600s     ← 硬编码安全网
  const MAX_OUTPUT_BYTES: usize = 100 KB         ← 硬编码安全网
  pub async fn read(stream) / read_streamed(stream, event_tx, cancel)

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
switch_session(routing_key, target_sid)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  for (other_rk, ctx) in sessions:           ← 1:1 不变量检查
    if other_rk != rk && ctx.session.id == target_sid:
      return Err(SessionInUse { held_by: other_rk })
  
  session = backend.load(target_sid)
  ctx = Arc::new(SessionContext::new(session, ...))
  sessions[routing_key] = ctx                ← 旧 ctx Arc 引用计数自动 drop


create_session(routing_key, agent_name, name)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  session = backend.create(name)
  agent   = agents.get(agent_name)
  profile = UserProfile::load(resolver.resolve(routing_key), ...)
  ctx = Arc::new(SessionContext::new(session, agent, profile))
  sessions[routing_key] = ctx


delete_session(routing_key, target_sid)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  if sessions[routing_key].session.id == target_sid:
    sessions.remove(routing_key)
  backend.delete(target_sid)
  // 其他 rk 持有相同 sid 的情况不存在（1:1 不变量）


子代理委派
━━━━━━━━━━━
  DelegateTool.execute(args, &parent_session)
    └─ DelegationCoordinator.delegate(agent_name, task, &parent_session)
        ├─ channel        = parent_session.channel.clone()
        ├─ reply_target   = parent_session.last_reply_target.clone()
        ├─ 解析 agent.config.isolation
        ├─ 如果 worktree: 创建 git worktree
        ├─ sub_ctx = session_manager.create_sub_session(parent_session.id, agent_name)
        │             ↑ 返回 Arc<SessionContext>，不进 sessions 表
        ├─ sub_ctx.process_turn(task, channel, reply_target)
        ├─ 如果 worktree: merge + cleanup
        └─ sub_ctx Arc drop（refcount=0 → SessionContext 释放）
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
❌ WebhookContext      → WebhookHandler 退化为协议适配器，发 WebhookEvent 给 Orchestrator
❌ SubAgentDelegator   → DelegationCoordinator + AgentRegistry
❌ RequestBuilder      → 拆为 Agent.cached_prompt / SessionContext.attachments /
                         WorkspaceWatcher / TurnInput / 无状态函数
❌ CompactionPolicy    → 并入 ContextEngine（TokenTracker 挪进 Session）
❌ CompactionExecutor  → 并入 ContextEngine
❌ AskUserHandler      → AskRouter (struct + 方法)
❌ DelegateHandler     → AgentDelegator trait
❌ DefaultToolExecutor → 重命名为 ToolExecutor，timeout 内化为字段
❌ LlmResponseReader   → llm_stream::read 模块函数 + 硬编码常量
❌ AgentConfig.max_history → 死代码，删除
❌ stream_first_chunk_timeout_secs / max_output_bytes 配置 → 改硬编码安全网
❌ [defaults] / [limits] / [context] 段 → 拆为按模块的多个段
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

Webhook ──HTTP request──→ WebhookHandler （协议适配器）
            └── 签名校验 + 模板渲染 → WebhookEvent
                → mpsc → Orchestrator.run()
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
