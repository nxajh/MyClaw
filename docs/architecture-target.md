# MyClaw 目标架构图（重构后）

> 基于 RFC: Session 架构重构
> 日期：2026-05-21

## 组件关系总览

```
┌──────────────────────────────────────────────────────────────────────┐
│                       daemon.rs (Composition Root)                   │
│                                                                      │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐               │
│  │ServiceRegistry│  │ ToolRegistry │  │ SkillManager │               │
│  │(LLM providers)│  │ (tool impls) │  │  (skills)    │               │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘               │
│         │                  │                  │                       │
│         └──────────┬───────┴──────────┬──────┘                       │
│                    │                  │                               │
│         ┌──────────┴──────────────────┴──────────┐                   │
│         │            AgentRegistry               │                   │
│         │  HashMap<name, Arc<Agent>>              │                   │
│         │                                         │                   │
│         │  ┌────────┐ ┌────────┐ ┌──────────┐    │                   │
│         │  │ "main" │ │"coder" │ │"researcher"│   │                   │
│         │  │ Agent  │ │ Agent  │ │  Agent    │   │                   │
│         │  │全套工具 │ │受限工具│ │搜索工具   │   │                   │
│         │  └────────┘ └────────┘ └──────────┘    │                   │
│         └──────────────────┬──────────────────────┘                   │
│                            │                                          │
│  ┌─────────────────────────┴────────────────────────────────────┐    │
│  │                     SessionManager                            │    │
│  │                                                               │    │
│  │  backend ────→ SessionBackend (持久化)                         │    │
│  │  agents  ────→ AgentRegistry                                  │    │
│  │  active  ────→ HashMap<routing_key, session_id>   ← 只是指针  │    │
│  │  contexts ──→ HashMap<session_id, Arc<SessionContext>>         │    │
│  │                                                               │    │
│  │  switch_session() = 改 active 指针，天然一致                    │    │
│  └───────────────────────────────────────────────────────────────┘    │
│                                                                      │
│  ┌───────────────────────────────────────────────────────────────┐    │
│  │                      Orchestrator                             │    │
│  │                                                               │    │
│  │  channels ──→ HashMap<(type, account), Arc<dyn Channel>>      │    │
│  │  pending_asks ──→ HashMap<sk, (oneshot::Sender, target)>      │    │
│  │                                                               │    │
│  │  职责：收消息 → 找 SessionContext → 投递                       │    │
│  │  不管：Session 创建、AgentLoop、actor                         │    │
│  └───────────┬──────────────────────────────┬────────────────────┘    │
│              │                              │                         │
│    ┌─────────┼──────────────┐               │                         │
│    ▼         ▼              ▼               ▼                         │
│  ┌────────┐ ┌──────────┐ ┌──────────┐  ┌──────────┐                 │
│  │Telegram│ │ClientChan│ │ QQBot    │  │Scheduler │                 │
│  │Channel │ │(WebUI WS)│ │ Channel  │  │(cron/hb) │                 │
│  └────────┘ └──────────┘ └──────────┘  └──────────┘                 │
└──────────────────────────────────────────────────────────────────────┘


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
❌ AgentLoop          → Agent.run() 是无状态方法，不需要 struct
❌ SessionHandle      → SessionContext 直接提供 process_turn()
❌ LoopRegistry       → 职责回归 SessionManager
❌ run_session_actor  → mutex 替代 channel + actor
❌ TurnMessage/TurnInput → process_turn() 直接接收参数
❌ get_or_create()    → 拆散到 SessionManager 和 SessionContext 构造

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
