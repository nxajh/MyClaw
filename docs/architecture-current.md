# MyClaw 当前架构（As-Is）

> **生成日期**：2026-07-09  
> **对照代码**：`master`（RFC Session 架构重构 v2 主路径已落地）  
> **权威性**：本文描述**运行中代码的真实结构**。旧版「AgentLoop / LoopRegistry / ServiceRegistry」叙事已作废。  
> **相关文档**：
> - 目标设计原稿 → [`architecture-target.md`](./architecture-target.md)（2026-05-21，主骨架已实现，细节以本文为准）
> - 模块关系图 → [`architecture-modules.md`](./architecture-modules.md)
> - 重构清单 → [`refactor-progress.md`](./refactor-progress.md)（61/61 主项完成）
> - 源码树索引 → [`architecture.md`](./architecture.md)（自动提取，非运行时语义）

---

## 组件关系总览

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                         daemon.rs (Composition Root)                         │
│  加载 config → 装配 Provider / Tools / Skills / MCP / SessionBackend          │
│  构造 AgentRuntime、SessionManager、AskRouter、DelegationCoordinator          │
│  装配 OrchestratorCtx + Orchestrator，启动 Channel / Scheduler 监听           │
│                                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐    │
│  │ProviderRegistry│  │ ToolRegistry │  │ SkillManager │  │  McpManager  │    │
│  │(LLM providers)│  │ (tool impls) │  │ Arc<RwLock>  │  │  (opt-in)    │    │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘    │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐    │
│  │ContextEngine │  │ ToolExecutor │  │  LoopBreaker │  │AgentRegistry │    │
│  │ (compaction) │  │  (timeout)   │  │ (per-turn)   │  │ name→Agent   │    │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────┬───────┘    │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐         │            │
│  │  AskRouter   │  │ UserResolver │  │ Workspace    │         │            │
│  │(pending asks)│  │ rk→user_id   │  │ Watcher      │         │            │
│  └──────────────┘  └──────────────┘  └──────────────┘         │            │
│                                                                │            │
│  ┌───────────────────────────┐   ┌─────────────────────────────┴─────────┐ │
│  │      AgentRuntime         │   │           SessionManager              │ │
│  │  providers / tools /      │   │  backend / agents / resolver          │ │
│  │  skills / agents /        │   │  contexts: rk → Arc<SessionContext>   │ │
│  │  context_engine /         │   │  ★ 每 routing_key 至多一个活跃 ctx    │ │
│  │  tool_executor /          │   └──────────────────┬────────────────────┘ │
│  │  loop_breaker / defaults  │                      │                      │
│  │  + mcp_manager?           │                      ▼                      │
│  │  + search_cooldown?       │           SessionContext (per session)      │
│  │  + task_state?            │                                             │
│  └────────────┬──────────────┘                                             │
│               │                                                            │
│  ┌────────────┴────────────────────────────────────────────────────────┐   │
│  │  Orchestrator                                                       │   │
│  │    owns: msg_rx / listener_handles / delegation_rx / scheduler_rx   │   │
│  │    holds: Arc<OrchestratorCtx>                                      │   │
│  │      ├─ channels: ChannelRegistry (type,account)→Channel            │   │
│  │      ├─ sessions: Arc<SessionManager>                               │   │
│  │      ├─ ask: Arc<AskRouter>                                         │   │
│  │      ├─ runtime: AgentRuntime                                       │   │
│  │      ├─ delegator: Option<Arc<DelegationCoordinator>>               │   │
│  │      └─ scheduler: Option<SharedScheduler>                          │   │
│  └──────────┬───────────────────────────────┬──────────────────────────┘   │
│             │                               │                              │
│     ┌───────┴───────┐               ┌───────┴────────┐                     │
│     ▼               ▼               ▼                ▼                     │
│  Telegram      ClientChannel     QQBot / WeChat   Scheduler/Webhook         │
│  Channel       (WebUI WS)        Channel          (cron / heartbeat)        │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

## 消息流转（用户发消息 → 回复）

```
用户
 │
 ▼
Channel.listen()
 │  ChannelInboundMessage
 ▼
Orchestrator 主循环（统一 OrchestratorEvent）
 │
 ├─ 解析 SessionKey / routing_key = "channel:account:sender"
 │
 └─ inbound 责任链（顺序固定，单测钉死）:
      ask_reply → callback → crash_recovery → slash_command → dispatch_turn
           │
           │  ask_reply 命中 AskRouter.pending → fulfill，不再进 turn
           │
           ▼  dispatch_turn
      SessionManager.get_or_create_context(sk)
           │
           ▼
      SessionContext.process_turn(msg, channel, runtime)
           │  turn_lock 串行化本 session
           │  附件 diff → history
           │  解析 TurnContext（model override / permission / prompt）
           │
           ▼
      Agent::run(&mut Session, turn_ctx, &AgentRuntime)
           │  1. allowed_tools = runtime.tools ∩ agent.config 过滤
           │  2. 解析 provider + model_id
           │       - 有 session/turn model override → 指定模型
           │       - 否则 → routing 默认 Chat 链
           │  3. 若 resolved model 有 MediaPolicy：filter_modality_redundant_tools
           │       （先 resolve model，再按「本轮 messages 是否已内联原生媒体」
           │        drop 冗余 view_image / view_video / hear_audio；无 policy 则跳过）
           │  4. ContextEngine 预压缩（必要时）
           │  5. provider.chat(stream) → tool 循环 / LoopBreaker
           │  6. persist + 可选 memory_fork
           │  7. 经 turn_stream / channel 回推
           ▼
      channel.send_message → 用户
```

调度（cron/heartbeat）、委派完成回填、启动恢复均复用  
`get_or_create_context` + `process_turn` / `Agent::run`，**不再**各自临时拼一套 loop。

---

## 核心类型（与源码字段对齐）

### `Agent` — 纯身份

```
src/agents/agent.rs
  Agent { config: SubAgentConfig }
  run(&self, session: &mut Session, turn_ctx: TurnContext, runtime: &AgentRuntime)
      -> Result<TurnResult>
  run_recovery(...)  // 中断恢复 Case A/B/C
```

不再存在 `AgentLoop`、`loop_for_with_persist`、工厂式 `Agent`。

### `AgentRuntime` — 进程级共享 bundle

```
src/agents/runtime.rs
  providers:       Arc<dyn ProviderRegistry>
  tools:           Arc<ToolRegistry>
  skills:          Arc<RwLock<SkillManager>>
  agents:          Arc<AgentRegistry>
  context_engine:  Arc<ContextEngine>
  tool_executor:   Arc<ToolExecutor>
  loop_breaker:    Arc<LoopBreaker>
  defaults:        RuntimeDefaults { permission_mode, prompt }
  mcp_manager:     Option<Arc<McpManager>>
  search_cooldown: Option<Arc<SearchProviderCooldown>>
  task_state:      Option<Arc<RwLock<TaskState>>>
```

目标文档写「8 字段」；实现在核心 8 项之上扩展了 mcp / search_cooldown / task_state。

### `SessionManager` / `SessionContext`

```
src/agents/session/manager.rs
  backend:  Arc<dyn SessionBackend>
  agents:   Arc<AgentRegistry>
  resolver: Arc<UserResolver>
  contexts: RwLock<HashMap<routing_key, Arc<SessionContext>>>

src/agents/session_context.rs
  session:       Arc<Mutex<Session>>
  agent:         Arc<Agent>
  attachments:   Mutex<AttachmentManager>
  pending_retry: Arc<Mutex<Option<String>>>
  turn_lock:     Arc<Mutex<()>>
  user_profile:  Arc<UserProfile>
  process_turn(msg, channel, runtime) -> Result<TurnResult>
```

`Session` 本体（`session/types.rs`）持有 history、override、token_tracker、  
`last_message`、`parent_session_id`、`agent_name`，以及 transient 的  
`persist` / `channel` / `turn_stream`。

### `Orchestrator` / `OrchestratorCtx`

```
src/agents/orchestrator/mod.rs   — 消费事件 rx，持有 listener
src/agents/orchestrator/ctx.rs   — 可 clone 的依赖包
src/agents/orchestrator/inbound.rs — 责任链 + dispatch_turn
src/agents/orchestrator/scheduled.rs / recovery.rs / turn.rs ...
```

同 session 串行靠 **`SessionContext.turn_lock`**，不是旧的  
`run_session_actor + LoopRegistry + Mutex<AgentLoop>` 三层间接。

### `DelegationCoordinator` / `AskRouter`

- 子会话：`SessionManager.create_sub_session` → 扁平 `sessions/{sid}/`，  
  完成事件合成 inbound 再走父 session 的 `process_turn`。
- 问用户：`AskUserTool` + `AskRouter.register/wait/fulfill`，inbound 链最前消费。

---

## 数据持有关系（简图）

```
AgentRegistry
  └─ name → Arc<Agent { config }>

SessionManager
  └─ contexts[routing_key] → Arc<SessionContext>
        ├─ Mutex<Session>          ← 对话状态唯一 owner
        ├─ Arc<Agent>
        ├─ Arc<UserProfile>
        ├─ AttachmentManager
        ├─ pending_retry
        └─ turn_lock

Orchestrator
  └─ Arc<OrchestratorCtx>
        ├─ ChannelRegistry
        ├─ SessionManager
        ├─ AskRouter
        ├─ AgentRuntime
        └─ DelegationCoordinator?

AgentRuntime ──共享──→ 所有 Agent::run / process_turn
```

---

## 通信方式

```
Channel ──ChannelInboundMessage──→ mpsc ──→ Orchestrator
                                              │
                                              ├─ AskRouter.fulfill(session_id)?
                                              │
                                              └─ SessionContext.process_turn
                                                      └─ Agent::run
                                                            ├─ turn_stream 推 Chunk/Tool*
                                                            └─ channel.send_message

Scheduler ──SchedulerEvent──→ Orchestrator ──→ 同上 process_turn
Delegation ──DelegationEvent──→ 父 session process_turn
WebUI WS ──ClientChannel──→ SessionManager API + 同一 inbound 路径
```

---

## 已删除（勿再写入新文档）

| 旧概念 | 替代 |
|--------|------|
| `AgentLoop` | `Agent::run` |
| `LoopRegistry` / `SessionHandle` / `run_session_actor` | `SessionManager` + `SessionContext.turn_lock` |
| `ServiceRegistry` | `ProviderRegistry` |
| `loop_for_with_persist` / 工厂 `Agent` | `Agent { config }` + runtime 入参 |
| Session move 进 loop 的双缓存 | `contexts` 表 + `Mutex<Session>` |
| 各路径临时拼 loop（cron/webhook/sub） | 统一 `process_turn` |

源码中若仍见 `AgentLoop` 字样，多为历史注释（如 daemon H57、watcher 文案），**无对应类型**。

---

## 媒体与模型决议（代码已有、旧图未写）

```
TurnContext.model_id（Option：显式 override 或 None→routing 默认）
        │
        ▼
Agent::run 内 resolve provider + model_id
        │
        ▼
providers.get_chat_media_policy(model_id)  → Option<MediaPolicy>
        │
        ▼  Some(policy) 时
filter_modality_redundant_tools(allowed_tools, messages, policy, model_id)
  - 依据 native_media_availability(messages, policy)：
    本轮请求里该模态是否已内联原生媒体
  - 是则 drop 对应辅助工具：view_image / view_video / hear_audio
  - 无 policy（None）则整段跳过
  - 过滤点在 model 决议之后（对 override 与默认路由同一路径）
```

附件进入 history 的路径：`session_context` / channel inbound → `ContentPart::File` 等；  
协议层（OpenAI 等）再渲染为 provider 原生多模态字段。详见 `providers/media.rs`、  
`agents/agent.rs` 中 modality 过滤逻辑。

---

## 与目标文档的细差（已知、可接受）

1. **Orchestrator 拆为 `Orchestrator` + `OrchestratorCtx`**（target 图未单独画出 ctx）。
2. **`AgentRuntime` 扩展字段**（mcp_manager / search_cooldown / task_state）。
3. **流式**：实现使用 per-turn `TurnStream`（channel.create_stream），而非仅靠  
   `Channel::push_event` 一种形态（target 曾设想内化到 Channel trait）。
4. **消息类型名**：运行时主路径为 `ChannelInboundMessage` / `ChannelOutboundMessage`。
5. **SchedulerContext / WebhookContext**：主路径已收编；瘦残留若仍在树中属  
   nice-to-have 清理（见 `refactor-progress` E34 备注）。

---

## 源码入口速查

| 职责 | 路径 |
|------|------|
| Composition Root | `src/daemon.rs` |
| Orchestrator | `src/agents/orchestrator/` |
| SessionManager | `src/agents/session/manager.rs` |
| SessionContext | `src/agents/session_context.rs` |
| Agent / run | `src/agents/agent.rs` |
| AgentRuntime | `src/agents/runtime.rs` |
| Providers | `src/providers/` |
| Channels | `src/channels/` |
| Tools | `src/tools/` |
| Config | `src/config/` |
