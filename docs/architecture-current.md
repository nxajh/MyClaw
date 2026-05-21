# MyClaw 当前架构图

> 生成日期：2026-05-21
> 基于 master 分支代码分析

## 组件关系总览

```
┌─────────────────────────────────────────────────────────────────────┐
│                         daemon.rs (Composition Root)                │
│                                                                     │
│  创建并持有以下全局单例：                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐              │
│  │ServiceRegistry│  │ ToolRegistry │  │ SkillManager │              │
│  │  (LLM providers)│  │  (tool impls) │  │  (skills)    │              │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘              │
│         │                  │                  │                      │
│         └──────────┬───────┴──────────┬──────┘                      │
│                    ▼                  ▼                              │
│              ┌──────────┐    ┌────────────────┐                     │
│              │  Agent   │    │SubAgentDelegator│──┐                  │
│              │ (factory) │    │DelegationManager │  │                  │
│              └────┬─────┘    └────────────────┘  │                  │
│                   │                              │                  │
│                   │  ┌───────────────────────┐   │                  │
│                   │  │ SessionManager         │   │                  │
│                   │  │  ├─ backend            │   │                  │
│                   │  │  ├─ active (user→sid)  │   │                  │
│                   │  │  └─ cache (sid→Session)│   │                  │
│                   │  └───────────┬───────────┘   │                  │
│                   │              │               │                  │
│  ┌────────────────┴──────────────┴───────────────┴──────────┐       │
│  │                    Orchestrator                           │       │
│  │  ┌─────────────────────────────────────────────────────┐ │       │
│  │  │ LoopRegistry                                        │ │       │
│  │  │  sessions: DashMap<sk, Arc<SessionHandle>>           │ │       │
│  │  └─────────────────────────────────────────────────────┘ │       │
│  │                                                           │       │
│  │  channels: DashMap<(type, account), Arc<dyn Channel>>     │       │
│  │  pending_asks: DashMap<sk, (oneshot::Sender, target)>    │       │
│  └───────────────────────────┬──────────────────────────────┘       │
│                              │                                      │
│         ┌────────────────────┼────────────────────┐                 │
│         ▼                    ▼                    ▼                  │
│  ┌─────────────┐   ┌──────────────┐   ┌──────────────┐             │
│  │TelegramChannel│  │ClientChannel │   │ QQBotChannel │             │
│  │              │   │  (WebUI WS)  │   │              │             │
│  └─────────────┘   └──────┬───────┘   └──────────────┘             │
│                           │                                         │
│                    ┌──────┴──────┐                                  │
│                    │ WebSocket   │                                  │
│                    │ connections │                                  │
│                    └─────────────┘                                  │
│                                                                     │
│  ┌──────────────────┐  ┌────────────────┐                          │
│  │ WebhookContext   │  │ SchedulerContext│  ← cron/heartbeat        │
│  │ (scheduler task) │  │ (scheduler task)│                          │
│  └──────────────────┘  └────────────────┘                          │
└─────────────────────────────────────────────────────────────────────┘
```

## 消息流转（用户发消息到收到回复）

```
                    用户
                     │
                     ▼
            ┌────────────────┐
            │    Channel     │  Telegram / WebUI WS / QQ Bot
            │  .listen()     │
            └───────┬────────┘
                    │ ChannelMessage
                    ▼
            ┌────────────────┐
            │  Orchestrator  │  主事件循环 (tokio select)
            │   .run()       │
            └───────┬────────┘
                    │
                    ├─ 1. 解析 routing_key = "channel:account:sender"
                    ├─ 2. session_manager.get_or_create(sk)     ← 拿 Session
                    ├─ 3. registry.get_or_create(sk, reply_target)
                    │       │
                    │       ├─ SessionManager.get_or_create(sk)    ← 拿 Session
                    │       ├─ agent.loop_for_with_persist(session) ← Session move 进 AgentLoop
                    │       ├─ 绑 ask_user 闭包 (capture channels Arc)
                    │       ├─ 绑 delegate 闭包 (capture delegator Arc)
                    │       ├─ spawn run_session_actor(sk, loop_, rx)
                    │       └─ 存 sessions[sk] = SessionHandle
                    │
                    └─ 4. handle.tx.send(TurnMessage)
                            │
                            ▼
                    ┌──────────────────┐
                    │ run_session_actor│  tokio task, per session
                    │ while let msg =  │
                    │   rx.recv()      │
                    └───────┬──────────┘
                            │
                            ▼
                    ┌──────────────────┐
                    │ run_message_task │
                    └───────┬──────────┘
                            │
                    ┌───────┴──────────┐
                    │                  │
              streaming=true     streaming=false
                    │                  │
                    ▼                  ▼
            loop_.lock()        loop_.lock()
            .run_streamed()     .run()
                    │                  │
                    └────────┬─────────┘
                             ▼
                    ┌──────────────────┐
                    │    AgentLoop     │
                    │                  │
                    │  1. 读 self.session.history
                    │  2. 构建 LLM request (RequestBuilder)
                    │  3. 调 ServiceRegistry → LLM API
                    │  4. 解析 response:
                    │     ├─ tool_call → tool_executor.execute(&mut self.session)
                    │     ├─ text → self.session.add_assistant_text()
                    │     └─ persist_hook.persist_message()
                    │  5. 循环 (LoopBreaker 控制)
                    │  6. 可能触发 CompactionPolicy → compact
                    │  7. 返回最终文本
                    └────────┬─────────┘
                             │
                             ▼
                    channel.send(SendMessage)
                             │
                             ▼
                           用户
```

## 数据持有关系

```
Agent (全局单例)
  ├─ owns: registry ────────────→ ServiceRegistry (Arc<dyn>)
  ├─ owns: tools ───────────────→ ToolRegistry (Arc)
  ├─ owns: skills ──────────────→ SkillManager (Arc<RwLock>)
  ├─ owns: config ──────────────→ AgentConfig
  ├─ owns: system_prompt ───────→ String
  ├─ owns: sub_agent_configs ───→ Vec<SubAgentConfig> (Arc<RwLock>)
  │
  └─ method: loop_for_with_persist(session) → AgentLoop
       ↓ (Session 被 move 进 AgentLoop)
  
AgentLoop (per session, 缓存在 LoopRegistry)
  ├─ owns: session ────────────→ Session (history, metadata, override)
  ├─ owns: registry ────────────→ ServiceRegistry (Arc clone from Agent)
  ├─ owns: tool_executor ──────→ DefaultToolExecutor (Arc clone of ToolRegistry)
  ├─ owns: request_builder ────→ RequestBuilder (prompt + resources)
  ├─ owns: compactor ──────────→ CompactionExecutor
  ├─ owns: policy ─────────────→ CompactionPolicy (token tracking)
  ├─ owns: loop_breaker ───────→ LoopBreaker (tool call counting)
  ├─ owns: persist_hook ───────→ Option<Arc<dyn PersistHook>>
  ├─ owns: config ─────────────→ AgentConfig (with session override)
  └─ owns: pending_retry ──────→ Option<String>

SessionHandle (per session, 缓存在 LoopRegistry.sessions)
  ├─ owns: loop_ ──────────────→ Arc<Mutex<AgentLoop>>
  └─ owns: tx ─────────────────→ mpsc::Sender<TurnMessage>

LoopRegistry (Orchestrator 内部)
  ├─ ref:  sessions ───────────→ Arc<DashMap<sk, Arc<SessionHandle>>>
  ├─ ref:  agent ──────────────→ Agent (Arc)
  ├─ ref:  session_manager ────→ SessionManager (Arc)
  ├─ ref:  channels ───────────→ DashMap<(type,account), Arc<dyn Channel>>
  ├─ ref:  pending_asks ───────→ DashMap<sk, (oneshot::Sender, target)>
  ├─ ref:  sub_delegator ──────→ Option<SubAgentDelegator>
  ├─ ref:  delegation_manager ─→ Option<DelegationManager>
  ├─ ref:  persist_backend ────→ Arc<dyn SessionBackend>
  └─ ref:  change_rx ──────────→ Option<watch::Receiver<ChangeSet>>

SessionManager (全局单例, Orchestrator 持有)
  ├─ owns: backend ────────────→ Arc<dyn SessionBackend> (JSON files)
  ├─ owns: active ─────────────→ RwLock<HashMap<user_id, session_id>>
  └─ owns: cache ──────────────→ RwLock<HashMap<session_id, Session>>
       └─ Session 只在 "还没被 AgentLoop 认领" 时存在于此
          认领后 move 到 AgentLoop，cache 中 remove

Orchestrator (全局单例)
  ├─ owns: channels ───────────→ Arc<DashMap<(type,account), Arc<dyn Channel>>>
  ├─ owns: sessions ───────────→ Arc<DashMap<sk, Arc<SessionHandle>>> (共享给 LoopRegistry)
  ├─ owns: agent ──────────────→ Agent
  ├─ owns: session_manager ────→ Arc<SessionManager>
  ├─ owns: msg_rx ─────────────→ Mutex<Option<mpsc::Receiver<ChannelMessage>>>
  ├─ owns: pending_asks ───────→ Arc<DashMap<sk, (oneshot::Sender, target)>>
  ├─ owns: persist_backend ────→ Arc<dyn SessionBackend>
  ├─ owns: mcp_manager ────────→ Option<Arc<McpManager>>
  └─ owns: listener_handles ───→ Vec<JoinHandle<()>>

SubAgentDelegator (全局单例)
  ├─ owns: registry ───────────→ ServiceRegistry (Arc)
  ├─ owns: tool_impls ─────────→ ToolRegistry (Arc)
  ├─ owns: skills ─────────────→ SkillManager (Arc<RwLock>)
  ├─ owns: sub_agent_configs ──→ Vec<SubAgentConfig> (Arc<RwLock>)
  └─ owns: agent_config ───────→ AgentConfig
       └─ 委派时临时构建 AgentLoop（不走 LoopRegistry）

Channel 实现
  TelegramChannel
    ├─ owns: bot_token, api_base
    └─ method: listen() → poll getUpdates, send() → sendMessage API

  ClientChannel (WebUI)
    ├─ owns: config (bind addr, auth_token)
    ├─ owns: session_manager ──→ Arc<RwLock<Option<Arc<SessionManager>>>>
    ├─ owns: tool_specs ───────→ Arc<RwLock<Vec<ToolSpec>>>
    ├─ owns: connections ──────→ DashMap<u64, ClientConnection>
    ├─ owns: stream_contexts ──→ DashMap<String, StreamContext>
    ├─ owns: loop_registry ────→ Arc<RwLock<Option<Arc<DashMap<...>>>>
    └─ method: listen() → TcpListener + WebSocket upgrade
               API requests → handle_api_request() → 直接操作 SessionManager
```

## 通信方式

```
Channel ──ChannelMessage──→ mpsc ──→ Orchestrator.run()
                                           │
                                           ├── handle.tx.send(TurnMessage) ──→ mpsc ──→ run_session_actor
                                           │                                                      │
                                           │                                              loop_.lock().await
                                           │                                              AgentLoop.run()
                                           │                                                      │
                                           │                                              channel.send(SendMessage) ──→ 用户
                                           │
                                           ├── ask_user handler ──→ channel.send(question) ──→ 用户
                                           │                  ──→ pending_asks.insert(sk, oneshot_tx)
                                           │
                                           └── 用户回复 ──→ pending_asks.remove(sk)
                                                             ──→ oneshot_tx.send(answer) ──→ AgentLoop 等待中

WebUI API ──WebSocket JSON──→ ClientChannel.handle_api_request()
                              ├── sessions.list/switch/create/delete → SessionManager 直接操作
                              ├── tools.list → tool_specs
                              ├── config.get/set → config_path
                              └── skills.list → skill_manager

Scheduler (cron/heartbeat) ──SchedulerEvent──→ Orchestrator.run()
                                           └── 复用同一套 get_or_create + SessionHandle

SubAgentDelegator ──delegate_async()──→ spawn tokio task
                                       └── 临时构建 AgentLoop（不经过 LoopRegistry）
                                           session_key = "sub:<parent_sk>:<agent_name>"
```

## 关键问题标注

```
❶ Session 被 move 进 AgentLoop
   SessionManager.cache 中 remove → LoopRegistry.sessions 中持有
   两层缓存，switch 时需要 evict 同步

❷ LoopRegistry 是 Orchestrator 内部的平行缓存
   与 SessionManager 独立运作，同一份数据两个 owner

❸ get_or_create 承担了 5+ 职责
   创建 Session + 构建 AgentLoop + 绑 handler + spawn actor + 存缓存

❹ run_session_actor 纯粹为了串行化
   channel + actor + mutex 三层间接，mutex 只有 actor 自己用

❺ SubAgentDelegator 绕过 LoopRegistry
   临时构建 AgentLoop，不经过统一的 session 管理

❻ Agent 既是配置又是工厂
   loop_for_with_persist() 每次 new 一个 AgentLoop，职责不清
```
