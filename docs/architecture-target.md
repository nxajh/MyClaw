# MyClaw 目标架构图（重构后）

> 基于 RFC: Session 架构重构
> 日期：2026-05-21

## 组件关系总览

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          daemon.rs (Composition Root)                        │
│                                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐    │
│  │ServiceRegistry│  │ ToolRegistry │  │ SkillManager │  │  McpManager  │    │
│  │(LLM providers)│  │ (tool impls) │  │  (skills)    │  │  (MCP 连接)  │    │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘    │
│         │                  │                  │                  │            │
│         │    MCP tools ────┘                  │                  │            │
│         │    Memory tools ────────────────────┘                  │            │
│         │    Skill tools ─────────────────────┘                  │            │
│         │                                                        │            │
│         └──────────┬──────┬──────────────────┬───────────────────┘            │
│                    │      │                  │                                │
│         ┌──────────┴──────┴──────────────────┴──────────┐                     │
│         │              AgentRegistry                    │                     │
│         │  HashMap<name, Arc<Agent>>                    │                     │
│         │                                               │                     │
│         │  ┌────────┐ ┌────────┐ ┌──────────┐          │                     │
│         │  │ "main" │ │"coder" │ │"researcher"│         │                     │
│         │  │ Agent  │ │ Agent  │ │  Agent    │         │                     │
│         │  │全套工具 │ │受限工具│ │搜索工具   │         │                     │
│         │  └────────┘ └────────┘ └──────────┘          │                     │
│         └──────────────────┬───────────────────────────┘                     │
│                            │                                                  │
│  ┌─────────────────────────┴──────────────────────────────────────────┐      │
│  │                         SessionManager                             │      │
│  │                                                                    │      │
│  │  backend ────→ SessionBackend (持久化)                              │      │
│  │  agents  ────→ AgentRegistry                                       │      │
│  │  active  ────→ HashMap<routing_key, session_id>     ← 只是指针    │      │
│  │  contexts ──→ HashMap<session_id, Arc<SessionContext>>             │      │
│  │                                                                    │      │
│  │  switch_session() = 改 active 指针，天然一致                        │      │
│  └────────────────────────────────────────────────────────────────────┘      │
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────┐     │
│  │                        Orchestrator (瘦身版)                         │     │
│  │                                                                     │     │
│  │  session_manager: SessionManager                                    │     │
│  │  channels:       HashMap<(type, account), Arc<dyn Channel>>         │     │
│  │  pending_asks:   HashMap<sk, (oneshot::Sender, target)>             │     │
│  │                                                                     │     │
│  │  职责：收消息 → 找 SessionContext → 投递                             │     │
│  └──────────┬──────────────────────────────────────┬───────────────────┘     │
│             │                                      │                         │
│  ┌──────────┼──────────────┐        ┌──────────────┤                         │
│  ▼          ▼              ▼        ▼              ▼                         │
│┌────────┐┌──────────┐┌──────────┐┌──────────┐┌──────────┐┌──────────────┐   │
││Telegram││ClientChan││ QQBot    ││ Webhook  ││Scheduler ││ FileWatcher  │   │
││Channel ││(WebUI WS)││ Channel  ││ Handler  ││(cron/hb) ││(hot-reload)  │   │
│└────────┘└──────────┘└──────────┘└──────────┘└──────────┘└──────────────┘   │
│             │                    │           │                   │             │
│             │                    │           │                   │             │
│             │            全部走 SessionManager.get_context()    │             │
│             │                    │           │         change_rx 全局共享     │
│             │                    │           │                   │             │
│             ▼                    ▼           ▼                   ▼             │
│  ┌──────────────────────────────────────────────────────────────────────┐    │
│  │                        Memory 存储                                   │    │
│  │  workspace/users/{user_id}/memory/{type}/                           │    │
│  │  由 MemoryList/View/Search/Manage 四个 tool 操作                     │    │
│  │  per-user 隔离，不再全局共享                                         │    │
│  └──────────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────────────┘


SessionContext（per session，唯一 owner）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

┌──────────────────────────────────┐
│        SessionContext            │
│                                  │
│  session: Session                │ ← 对话数据（唯一 owner）
│    ├─ history: Vec<ChatMessage>  │
│    ├─ message_ids: Vec<i64>      │
│    └─ compact_version: u32       │
│                                  │
│  agent: Arc<Agent>               │ ← 绑定的 agent（创建时选定）
│  channel: Arc<dyn Channel>       │ ← 通信通道
│  user_profile: Arc<UserProfile>  │ ← 用户信息
│  mutex: tokio::sync::Mutex<()>   │ ← turn 串行化
│                                  │
│  process_turn(msg) {             │
│    mutex.lock()                  │
│    channel.on_status(Thinking)   │
│    agent.run(&mut session, msg)  │
│    channel.on_status(Done)       │
│  }                               │
└──────────────────────────────────┘


Agent（同类型，不同配置的实例）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

┌──────────────────────────────────┐
│           Agent                  │
│                                  │
│  name: String                    │
│  config: AgentConfig             │ ← max_tool_calls, compact_threshold
│  system_prompt: String           │ ← AGENT.md body
│  tool_names: Vec<String>         │ ← 这个 agent 能用哪些 tool
│  skill_names: Vec<String>        │ ← 绑定哪些 skill
│                                  │
│  // 共享基础设施引用（不拥有）      │
│  registry: &ServiceRegistry      │
│  tool_impls: &ToolRegistry       │
│  skills: &SkillManager           │
│                                  │
│  run(&mut session, msg) {        │ ← 无状态执行一轮 turn
│    LoopBreaker (局部变量)         │
│    CompactionPolicy (局部变量)    │
│    ...调 LLM、执行 tool、写 history
│  }                               │
└──────────────────────────────────┘

"主 agent" vs "子代理" = 同一个 struct，不同实例：
┌──────────┐  ┌──────────┐  ┌──────────┐
│ Agent    │  │ Agent    │  │ Agent    │
│ "main"   │  │ "coder"  │  │"researcher"│
│          │  │          │  │          │
│ tools:   │  │ tools:   │  │ tools:   │
│  [all]   │  │  [shell, │  │  [web_   │
│          │  │   file_*]│  │   search]│
│          │  │          │  │          │
│ prompt:  │  │ prompt:  │  │ prompt:  │
│  (builtin│  │  AGENT.md│  │  AGENT.md│
│   sections│  │  body   │  │  body   │
│   only)  │  │          │  │          │
│          │  │          │  │          │
│ skills:  │  │ skills:  │  │ skills:  │
│  [all]   │  │  [code-  │  │  []      │
│          │  │   review]│  │          │
└──────────┘  └──────────┘  └──────────┘
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
                      │   .run()       │
                      └───────┬────────┘
                              │
                   ① 解析 routing_key
                   ② session_manager.get_context(routing_key)
                              │
                              ▼
                      ┌──────────────────┐
                      │  SessionContext  │
                      │                  │
                      │  process_turn()  │
                      │    │             │
                      │    ├ mutex.lock()│
                      │    │             │
                      │    ▼             │
                      │  agent.run(      │
                      │    &mut session, │
                      │    msg           │
                      │  )               │
                      │    │             │
                      │    ├─ 构建 LLM request
                      │    ├─ 调 ServiceRegistry → LLM API
                      │    ├─ tool_call → tool_executor.execute()
                      │    ├─ 写 session.history
                      │    ├─ persist_message()
                      │    └─ 返回结果    │
                      │    │             │
                      │    ▼             │
                      │  mutex.unlock()  │
                      └───────┬──────────┘
                              │
                              ▼
                     channel.send() → 用户
```

对比现在的四层间接：
```
现在：  orchestrator → tx.send() → actor(rx) → mutex.lock(AgentLoop) → run()
重构后：orchestrator → session_ctx.process_turn(msg)
                                           └→ agent.run(&mut session, msg)
```

## 数据持有关系

```
AgentRegistry (全局)
  └─ HashMap<String, Arc<Agent>>
       │
       ├─ "main" ────→ Agent
       │                ├─ registry:    Arc<dyn ServiceRegistry>    ← 引用，不拥有
       │                ├─ tool_impls:  Arc<ToolRegistry>           ← 引用，不拥有
       │                ├─ skills:      Arc<RwLock<SkillManager>>   ← 引用，不拥有
       │                ├─ config:      AgentConfig                 ← 自己的配置
       │                ├─ system_prompt: String                    ← 自己的
       │                ├─ tool_names:  Vec<String>                 ← 自己的
       │                └─ skill_names: Vec<String>                 ← 自己的
       │
       ├─ "coder" ──→ Agent (同结构，不同实例，不同配置)
       └─ "researcher" → Agent

SessionManager (全局)
  ├─ backend ────→ Arc<dyn SessionBackend>
  ├─ agents ─────→ AgentRegistry
  ├─ active ─────→ HashMap<routing_key, session_id>      ← 只是指针
  └─ contexts ──→ HashMap<session_id, Arc<SessionContext>>
                     │
                     └─ SessionContext
                          ├─ session: Session                  ← 唯一 owner ✅
                          ├─ agent: Arc<Agent>                 ← 引用 AgentRegistry 中的某个
                          ├─ channel: Arc<dyn Channel>         ← 引用 Orchestrator 中的某个
                          ├─ user_profile: Arc<UserProfile>    ← per-user
                          └─ mutex: Mutex<()>                  ← turn 串行化

Orchestrator (全局，瘦身后)
  ├─ session_manager: Arc<SessionManager>
  ├─ channels: HashMap<(type, account), Arc<dyn Channel>>
  ├─ pending_asks: HashMap<sk, (oneshot::Sender, target)>
  └─ listener_handles: Vec<JoinHandle<()>>

McpManager (全局)
  ├─ registry ────→ Arc<RwLock<Option<Arc<McpRegistry>>>>
  ├─ tools ───────→ Arc<RwLock<Vec<Arc<dyn Tool>>>>
  └─ server_count → AtomicUsize
  启动时 connect() → 把 MCP tools 注册进 ToolRegistry
  Agent 的 tool_names 决定是否使用 MCP tools

Scheduler (全局)
  ├─ jobs ────────→ RwLock<JobsFile>  (持久化到 jobs.json)
  ├─ timezone     → String
  ├─ heartbeat_config → Option<HeartbeatConfig>
  └─ event_tx ────→ mpsc::Sender<SchedulerEvent>  → Orchestrator
  触发时走 SessionManager.get_context()，不再独立持有 Agent/sessions

WebhookHandler (全局)
  ├─ session_manager: Arc<SessionManager>       ← 只需要这个引用
  ├─ timezone     → String
  └─ last_channel → Mutex<Option<String>>
  大幅简化：不再持有 Agent、sessions DashMap、SessionBackend

FileWatcher (全局)
  ├─ 监听路径: workspace/skills/, workspace/agents/
  └─ change_rx ──→ watch::Receiver<ChangeSet>  ← 全局共享
  文件变更 → 通知所有 Agent（通过全局共享的 change_rx）

Memory (per-user)
  ├─ MemoryList / MemoryView / MemorySearch / MemoryManage
  │   注册为 ToolRegistry 中的 tool
  │   Agent 的 tool_names 决定是否可用
  └─ 存储路径: workspace/users/{user_id}/memory/{type}/

UserProfile (per user)
  ├─ id: String
  ├─ name, timezone, language
  └─ preferences: HashMap<String, String>
  存储：workspace/users/{user_id}/profile.md
```

## Session 操作

```
switch_session(routing_key, session_id)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  active[routing_key] = session_id      ← 改指针
  下次 get_context() 自动返回新的 SessionContext
  不需要 evict，不需要重建 AgentLoop


create_session(routing_key, name)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  session = backend.create(name)
  context = SessionContext::new(session, agent, channel, user_profile)
  contexts[session.id] = context
  active[routing_key] = session.id


delete_session(routing_key, session_id)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  contexts.remove(session_id)            ← drop SessionContext
  if active[routing_key] == session_id:
    active.remove(routing_key)           ← 或切到默认 session


子代理委派
━━━━━━━━━━━
  sub_context = session_manager.get_or_create(sub_sk)
    → 绑定 agent: "coder"
  sub_context.process_turn(task_msg)
    → 和主 session 走完全一样的路径
```

## 删除的组件

```
❌ AgentLoop           → Agent.run() 是无状态方法，不需要 struct
❌ SessionHandle       → SessionContext 直接提供 process_turn()
❌ LoopRegistry        → 职责回归 SessionManager
❌ run_session_actor   → mutex 替代 channel + actor
❌ TurnMessage/TurnInput → process_turn() 直接接收参数
❌ get_or_create()     → 拆散到 SessionManager 和 SessionContext 构造
❌ SchedulerContext    → 统一走 SessionManager，不再独立持有 Agent/sessions
❌ WebhookContext      → 大幅简化，只持有 SessionManager 引用
❌ SubAgentDelegator   → 子代理变成 AgentRegistry 中的实例，委派走统一路径
❌ IDENTITY.md/SOUL.md → 内容写入 AGENT.md body
❌ USER.md (全局)       → per-user profile

保留不变的：
✅ McpManager          → 启动时注册 MCP tools 到 ToolRegistry，逻辑不变
✅ ToolRegistry        → 全局 tool 实现池，Agent 按 tool_names 选用
✅ Memory tools        → 4 个 tool 不变，存储路径改为 per-user
✅ FileWatcher         → 监听 skills/agents 变更，change_rx 全局共享
✅ Scheduler           → jobs.json + event_tx，触发方式不变

保留但瘦身的：
🔧 Orchestrator     → 只做路由：收消息 → 找 context → 投递
🔧 SessionManager   → 加上 contexts 管理，去掉 cache（不再有两阶段）
🔧 Agent            → 从工厂变成无状态执行器
```

## 通信方式

```
Channel ──ChannelMessage──→ mpsc ──→ Orchestrator.run()
                                          │
                                          └── session_ctx.process_turn(msg)
                                                  │
                                                  ├── agent.run(&mut session, msg)
                                                  │       │
                                                  │       └── tool 需要问用户时：
                                                  │           channel.send(question)
                                                  │           pending_asks.insert(sk, tx)
                                                  │           oneshot_rx.await → answer
                                                  │
                                                  └── channel.send(response) → 用户

WebUI API ──WebSocket JSON──→ ClientChannel
              ├── sessions.switch → session_manager.switch_session()   ← 天然一致
              ├── sessions.create → session_manager.new_session()
              ├── sessions.delete → session_manager.delete_session()
              ├── sessions.list   → session_manager.list_sessions()
              ├── tools.list      → agent_registry["main"].tool_specs
              └── config/skills   → 同现在

Scheduler ──SchedulerEvent──→ Orchestrator.run()
              └── session_manager.get_context(scheduled_sk)
                  → 同一条路径，绑定 agent: "main", config.run_mode = Background

Webhook ──HTTP request──→ WebhookHandler
            └── session_manager.get_context(webhook_sk)
                → 绑定 agent: "main", channel: last_channel
                → process_turn(webhook_payload)
                → 和用户消息走同一条路径

FileWatcher ──文件变更──→ change_rx (watch::Receiver, 全局共享)
                Agent.run() 内部读取，检测 skill/agent 配置变更

Memory ──tool 调用──→ MemoryManage/Search/List/View
          读写 workspace/users/{user_id}/memory/{type}/
          user_id 从 SessionContext.user_profile 获取

McpManager ──启动时──→ connect(config.mcp_servers)
             ├─ 注册 MCP tools 到 ToolRegistry（全局）
             └─ Agent 的 tool_names 决定哪些 MCP tool 可用
```

## 统一入口路径

```
现在（四条独立路径）：
  用户消息  → Orchestrator → LoopRegistry → AgentLoop             ← 路径 A
  Cron触发  → SchedulerContext → 临时构建 AgentLoop               ← 路径 B（绕过 LoopRegistry）
  Webhook   → WebhookContext → 临时构建 AgentLoop                 ← 路径 C（又绕过）
  子代理    → SubAgentDelegator → 临时构建 AgentLoop              ← 路径 D（再绕过）

重构后（一条统一路径）：
  全部 → SessionManager.get_context(routing_key) → SessionContext.process_turn()
  
  区别只是绑定的参数不同：
  ┌──────────┬──────────────┬───────────┬───────────────────┐
  │ 来源      │ agent        │ channel   │ run_mode          │
  ├──────────┼──────────────┼───────────┼───────────────────┤
  │ 用户消息  │ "main"       │ 来源通道  │ Interactive       │
  │ Cron     │ "main"       │ last_chan │ Background        │
  │ Webhook  │ "main"       │ last_chan │ Background        │
  │ 子代理    │ "coder" 等   │ 同父session│ Interactive      │
  │ Heartbeat│ "main"       │ 指定通道  │ Background        │
  └──────────┴──────────────┴───────────┴───────────────────┘
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
tools: [shell, file_read, file_write, file_edit]
skills: [code-review]
model: claude-sonnet-4-20250514
max_tool_calls: 30
---

You are an expert programmer. Write clean, idiomatic code.

## Behavioral Principles

Be concise. Don't over-engineer.
──────────────────────────────────────────────

Prompt 构建：
  builtin sections + AGENT.md body + user_profile + runtime + skills
```
