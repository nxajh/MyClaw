# Orchestrator 重构 RFC

> 状态:已实施(全部验收项达成,415 测试 / clippy --all-targets -D warnings 全绿)
> 范围:`src/agents/orchestrator.rs`(1142 行)+ `orchestrator_event.rs` + `orchestrator_scheduled.rs`
> 原则:**合理性 > 优雅 > 其它**。一次性大重构,不保留任何为兼容旧形态而存在的设计。

本文档描述把当前的 orchestrator god-file 重构成按职责拆分的模块组的目标设计、类型契约与落地清单。重构是**纯结构重排,对外行为不变**;正确性由"每个拦截器单测 + inbound 链顺序 golden 测试 + 全量回归 + clippy `-D warnings`"兜底。

---

## 1. 动机:要消灭的 7 个坏味道

逐行审计 `orchestrator.rs` 后,真正的病灶有 7 个,每一个对应一处设计决策:

| # | 坏味道 | 证据(原行号) | 新设计的回应 |
|---|---|---|---|
| 1 | **God-object**:`Orchestrator` 10 字段,业务全是 `impl Orchestrator` 直接 `&self` 取字段 | 整个文件 | 把"依赖"与"运行时"拆成两个类型 |
| 2 | **take-once 仪式**:`Arc<TokioMutex<Option<Receiver>>>` ×3 + `Arc<TokioMutex<Vec<Handle>>>` | 86–105 | `run(self)` 消费自身,receiver 按值持有 |
| 3 | **手搓扇入**:3 个 adapter task + `event_tx` 克隆 + 末尾 `.abort()` 三连 | 380–505 | 三个事件源建模成 `Stream`,`merge` 合流 |
| 4 | **230 行 inbound 巨函数**:8 件事挤在一个 `match` 臂 | 514–745 | 显式责任链(Interceptor pipeline) |
| 5 | **recovery 两份近乎复制粘贴**,唯一差别是"完成后投递到哪" | 835–997 | 一个 `recover_one` + 完成 sink |
| 6 | **delegation 伪造 ChannelMessage 再递归回灌**,需 box 递归、还有"sender 必须等于父 sender"暗坑 | 1056–1118 | delegation 直接调共享 `dispatch_turn`,不伪造 inbound |
| 7 | **字符串当类型**:`splitn(3,':')` / `format!("{}:{}:{}")` / 11 字段的 `SchedulerEvent::Cron` / `run_cron_task` 11 位置参数 | 122、324、55–66、811 | 引入 `SessionKey`/`SubAgentKey` 值类型、`CronTrigger`(并丢弃 4 个死字段) |

---

## 2. 目标与非目标

**目标**
- 把 1142 行的 `orchestrator.rs` 拆成单一职责模块,`runtime.rs` 仅剩约 120 行(结构 + `run` 骨架)。
- inbound 处理变成可脱离整机单测的责任链。
- 删除 god-object 的 `&self` 取字段模式,改为显式依赖包 `OrchestratorCtx`。
- 删除 `Arc<Mutex<Option<Receiver>>>` take-once 仪式。
- 用值类型替换裸字符串键与超长参数表。

**非目标**
- 不改变任何对外可观察行为(消息路由、回复时机、恢复语义)。
- 不改 `SessionContext::process_turn` / `Agent::run` / `run_recovery` 的内部语义。
- 不引入新的运行时依赖框架(actor 库等)。

---

## 3. 目标架构:三个名词

重构围绕**把 god-object 劈成三个职责清晰的名词**:

```
OrchestratorCtx   依赖包(全是 Arc,随便 clone)。所有 service / interceptor 只认它。
Orchestrator      运行时:独占"只能消费一次"的资源(合流后的事件流、listener 句柄)。只有 run(self)。
事件源 (Streams)  inbound / scheduler / delegation 各自是一个 Stream,合流进 run。
```

**关键洞察**:需要被 `Arc` 共享、被 spawn 出去的 task 长期持有的,是"依赖",不是"事件循环"。当前代码因为没分开,被迫 `run(self: Arc<Self>)` 并把所有 receiver 包成 `Arc<Mutex<Option<>>>`。分开后:

- cron / heartbeat task 拿 `Arc<OrchestratorCtx>`(clone),不再需要 `Arc<Orchestrator>`;
- `run` 按值消费 `self`,receiver 直接 move 进合流流 —— **坏味道 2、3 一起消失**;
- webhook server 当前靠 `orchestrator.session_manager()` / `.channels()` / `.scheduler()` 这些 accessor 拿东西 —— 这些字段**正好就是 `OrchestratorCtx`**。组装根(daemon.rs)直接持有 `Arc<OrchestratorCtx>`,同时交给 webhook 和 orchestrator。**全部 accessor 方法删除**。

**实测落地形态**(与初稿草图的差异见下方注):

```rust
// ctx.rs —— 保留原字段名(self.X → self.ctx.X 纯前缀,churn 最小)
pub struct OrchestratorCtx {
    pub channels:        Arc<DashMap<(String,String), Arc<dyn Channel>>>,
    pub session_manager: Arc<SessionManager>,
    pub ask_router:      Arc<AskRouter>,
    pub agent_runtime:   AgentRuntime,
    pub delegator:       Option<Arc<DelegationCoordinator>>,
    pub scheduler:       Option<SharedScheduler>,
}
impl OrchestratorCtx {
    pub fn session_context_for(&self, sk: &str) -> Arc<SessionContext>;
    pub fn channel(&self, account: &(String,String)) -> Option<Arc<dyn Channel>>; // 查表收口
}

// mod.rs —— 运行时只独占"消费一次"的 receiver / handle;事件流在 run() 内合流(不作字段存储)
pub struct Orchestrator {
    ctx:              Arc<OrchestratorCtx>,
    msg_rx:           Option<mpsc::Receiver<((String,String), ChannelMessage)>>,
    listener_handles: Vec<JoinHandle<()>>,            // run() 返回前 abort
    delegation_rx:    Option<mpsc::Receiver<DelegationEvent>>,
    scheduler_rx:     Option<mpsc::Receiver<SchedulerEvent>>,
}
impl Orchestrator {
    pub async fn run(self, shutdown: watch::Receiver<bool>, unfinished: Vec<UnfinishedSubAgent>) -> Result<()>;
}
```

> **与初稿草图的两处差异**(刻意,见 §12):①字段名保留原样(`session_manager`/`agent_runtime`/`ask_router`)而非 `sessions`/`runtime`/`ask`——纯 `self.` → `self.ctx.` 前缀替换,改动面最小;②`channels` 仍为裸 `Arc<DashMap>`(本身即注册表),未引入 `ChannelRegistry` newtype,查表由 `OrchestratorCtx::channel()` 收口;③合流后的事件流是 `run()` 内的局部变量,不是 `events` 字段。

---

## 4. 值类型:让"会话键"成为一等公民

会话键当前是裸 `String`,到处 `splitn`/`format!`。引入值类型:

```rust
/// 用户会话键:channel_type:account_id:sender
pub struct SessionKey { pub channel: String, pub account: String, pub sender: String }
impl SessionKey {
    pub fn parse(s: &str) -> Option<Self>;
    pub fn account_key(&self) -> (String, String);   // 频道查表用 (OrchestratorCtx::channel)
}
impl Display for SessionKey;  // "ct:ac:sender"

/// 子代理会话键是 2 段(agent_name:sub_session_id),形态不同 —— 单独建模,不硬塞进 SessionKey
pub struct SubAgentKey { pub agent: String, pub sub_session: String }
```

`parse_session_key` / `Self::session_key` / 所有 `splitn(3,':')` 与 `format!("{}:{}:{}")` 全部删除。delegation 那个"synthetic.sender 必须等于父 sender"的暗坑(原 1087 行注释)随之消失 —— 我们传 `SessionKey` 值,不再把键塞进伪造消息字段里再解析回来。

调度事件同步去字符串化(**实测落地形态**):

```rust
// event.rs
pub enum OrchestratorEvent {
    Inbound { channel_type: String, account_id: String, message: ChannelMessage },
    Scheduled(SchedulerEvent),                 // Heartbeat | Cron(CronTrigger)
    Delegation(DelegationEvent),
    AskReply { session_id: String, reply: ChannelMessage },
    Shutdown,
}

// mod.rs
pub enum SchedulerEvent { Heartbeat { target_channel: Option<String>, target_account: Option<String> }, Cron(CronTrigger) }

pub struct CronTrigger {          // 替掉 11 字段的 SchedulerEvent::Cron
    pub session_key: String,      // 可能是 `_cron_<id>` 等非三段式键 → 不强转 SessionKey
    pub prompt: String,
    pub target_channel: Option<String>,
    pub target_account: Option<String>,
    pub job_id: String,
    pub model: Option<String>,
}
```

`run_cron_task` 的 11 个位置参数随之坍缩成 `run_cron_task(ctx, trigger)`。

> **与初稿草图的差异**(刻意):①`Inbound` 用展开的 `channel_type/account_id/message` 而非 `account` 元组;事件枚举名沿用既有 `SchedulerEvent`(非 `Tick(SchedulerTrigger)`),并保留 `Shutdown` 变体。②`CronTrigger` 直接**丢弃** 4 个一路死传的字段(delivery/enabled_tools/disabled_tools/provider),不引入 `DeliveryTarget`/`TurnOverrides` 聚合类型;`session_key` 保持 `String`。

---

## 5. 事件循环:合流,不手搓(**实测落地形态**)

合流在 `run()` 内就地完成(`tokio_stream::StreamExt::merge`),不抽 `build_events` 自由函数、不存 `events` 字段:

```rust
pub async fn run(mut self, mut shutdown_rx, unfinished) -> anyhow::Result<()> {
    let rx = self.msg_rx.take()?;
    recovery::run_startup(&self.ctx.session_manager, &self.ctx.agent_runtime,
                          &self.ctx.channels, &unfinished, &self.ctx.delegator);

    // 三事件源 → 单一 Stream<OrchestratorEvent>,merge 合流(无 adapter task)
    let mut events: Pin<Box<dyn Stream<Item = OrchestratorEvent> + Send>> =
        Box::pin(ReceiverStream::new(rx).map(|((ct, ac), msg)|
            OrchestratorEvent::Inbound { channel_type: ct, account_id: ac, message: msg }));
    if let Some(drx) = self.delegation_rx.take() {
        events = Box::pin(events.merge(ReceiverStream::new(drx).map(OrchestratorEvent::Delegation)));
    }
    if let Some(srx) = self.scheduler_rx.take() {
        events = Box::pin(events.merge(ReceiverStream::new(srx).map(OrchestratorEvent::Scheduled)));
    }

    loop {
        if *shutdown_rx.borrow() || crate::is_shutting_down() { break; }   // 热切换检查点
        let event = tokio::select! {
            ev = events.next() => match ev { Some(e) => e, None => break },
            _ = shutdown_rx.changed() => break,
        };
        match event {
            OrchestratorEvent::Inbound { channel_type, account_id, message } =>
                inbound::dispatch(&self.ctx, (channel_type, account_id), message).await,
            OrchestratorEvent::Delegation(e) => delegation::wake(&self.ctx, e).await,
            OrchestratorEvent::Scheduled(e)  => self.handle_scheduler_event(e).await,
            OrchestratorEvent::AskReply { session_id, reply } => { self.ctx.ask_router.fulfill(&session_id, reply); }
            OrchestratorEvent::Shutdown => break,
        }
    }
    for h in self.listener_handles.drain(..) { h.abort(); }   // self 拥有 listeners
    Ok(())
}
```

→ 不再有 3 个 adapter task、不再 clone `event_tx`、不再末尾 `.abort()` 三连。

> 差异:合流内联在 `run()`(非 `build_events` 自由函数 + `select_all`);调度分派走 `self.handle_scheduler_event`(`&self`,deps 在 ctx)。

---

## 6. Inbound 责任链(本次重构核心)

原 `handle_channel_event` 的 UserMessage 分支是一条线性拦截链,每步**要么短路、要么透传(可改写消息)**。显式建模:

```rust
enum Flow { Stop, Next(ChannelMessage) }   // Next 携带可被改写的消息(Retry 改 content 靠它)

#[async_trait]
trait Interceptor: Send + Sync {
    async fn handle(&self, ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage) -> Flow;
}
```

`inbound/mod.rs` 的 runner:

```rust
pub async fn dispatch(ctx: &OrchestratorCtx, account: (String, String), msg: ChannelMessage) {
    let key = SessionKey { channel: account.0, account: account.1, sender: msg.sender.clone() };
    scheduler_bookkeeping(ctx, &key, &msg).await;     // record_user_message,纯副作用,不入链

    let chain: [&dyn Interceptor; 5] = [&AskReply, &Callback, &CrashRecovery, &SlashCommand, &DispatchTurn];
    let mut msg = msg;
    for stage in chain {
        match stage.handle(ctx, &key, msg).await {
            Flow::Stop    => return,
            Flow::Next(m) => msg = m,
        }
    }
}
```

每个拦截器一个文件,职责单一、可脱离整机单测:

| 拦截器 | 文件 | 行为 | 原行号 |
|---|---|---|---|
| `AskReply` | `ask.rs` | `ctx.ask.fulfill` 命中 → `Stop`(消费 inbound) | 533–546 |
| `Callback` | `callback.rs` | `Retry` → 取 `pending_retry`、改写 content → `Next`;`Abort` → 回 ack → `Stop` | 558–613 |
| `CrashRecovery` | `recovery_prompt.rs` | 检测 `incomplete_turn`,发 retry/abort 按钮提示 → `Stop` | 615–647 |
| `SlashCommand` | `command.rs` | slash 命令 spawn → `Stop` | 656–698 |
| `DispatchTurn` | `dispatch.rs` | **终端**,永远 `Stop`:`record_inbound` + 持久化 + `spawn(process_turn)` + 失败回 `MSG_TURN_FAILED` | 700–743 |

`retry_abort_prompt`(原 1017 行)归到 `callback.rs` / `recovery_prompt.rs` 共用的小工具。

> 收益:现在测"abort 是否回 ack"得把整个 orchestrator 跑起来;新设计里 `Callback.handle(mock_ctx, key, abort_msg)` 一行断言 `Flow::Stop` 并检查 mock channel 收到 ack。

---

## 7. Delegation:作为"系统唤醒意图",不伪造 inbound

`dispatch.rs` 暴露终端函数(被 `DispatchTurn` 拦截器和 delegation 共用):

```rust
// inbound/dispatch.rs
pub async fn dispatch_turn(ctx: &OrchestratorCtx, key: &SessionKey, msg: ChannelMessage);
```

delegation 不再伪造一条"看起来像用户发的"消息去重跑 ask/callback/command 链(那些链对它永远不匹配),而是**直接调终端**:

```rust
// delegation.rs
pub async fn wake(ctx: &OrchestratorCtx, ev: DelegationEvent) {
    let (key, note) = system_note_from(ev)?;          // 解析父键 + 渲染系统通知文本
    if ctx.channels.get(&key).is_none() { warn!(); return; }
    dispatch_turn(ctx, &key, ChannelMessage::system_note(&key, note)).await;
}
```

→ 删掉 box 递归(原 `handle_delegation_event` 的 `Pin<Box<dyn Future>>`)、删掉 sender 暗坑。

---

## 8. Recovery 去重 + 抽出"turn 参数解析"

`startup_recover_sessions` 与 `startup_recover_subagents`(原 835–997)有 ~90% 复制:`turn_lock` → 锁 session → 挂 persist → **(组装 turn_ctx)** → `run_recovery` → 清 persist。差别只有两点:遍历的键来源、完成后投递到哪。统一:

```rust
// recovery.rs
enum CompletionSink {
    Channel  { reply_target: String },                          // 普通会话:回贴到 channel
    Delegate { task_id: String, parent: SessionKey, reply_target: String },  // 子代理:发 DelegationEvent::Completed
}
async fn recover_one(ctx: Arc<OrchestratorCtx>, key: String, sink: CompletionSink);  // 唯一一份

pub fn run_startup(ctx: &Arc<OrchestratorCtx>, unfinished: &[UnfinishedSubAgent]) {
    for sk in ctx.sessions.incomplete_user_sessions()   { spawn(recover_one(.., Channel { .. })); }
    for sa in unfinished                                { spawn(recover_one(.., Delegate { .. })); }
}
```

而原 860–878 行那段"取 `session_override` → 合并进 `prompt_config`(permission_mode/run_mode) → `build_system_prompt` → 组 `TurnContext`"在**两个 recovery 里各抄一遍、scheduled 里还有第三遍**。它是真实领域概念——"把会话覆盖项解析成本回合的有效参数"——抽成单一归属:

```rust
// turn.rs
pub fn resolve_turn<'a>(session: &Session, runtime: &AgentRuntime, sys: &'a str) -> TurnContext<'a>;
```

recovery、scheduled、(理想情况下 `process_turn` 内部)都调它,杜绝三处漂移。

---

## 9. 最终模块树

实际落地结构(扁平文件而非子目录,关联紧密的拦截器同处一文件,更易读):

```
agents/orchestrator/
├── mod.rs           # Orchestrator 结构 + new() + run(self) + handle_scheduler_event + SchedulerEvent/CronTrigger + OrchestratorParts (437 行)
├── ctx.rs           # OrchestratorCtx(依赖包)+ session_context_for / channel() 收口 (52)
├── key.rs           # SessionKey / SubAgentKey + 单测 (121)
├── event.rs         # OrchestratorEvent (45)
├── turn.rs          # ResolvedTurn::resolve / turn_context (55)
├── inbound.rs       # Interceptor + Flow + 5 拦截器 + dispatch(runner) + dispatch_turn + retry_abort_prompt + 链顺序 golden 测试 (371)
├── delegation.rs    # wake():系统唤醒,复用 dispatch_turn (74)
├── recovery.rs      # 启动恢复(会话 + 子代理)统一 spawn_recovery + CompletionSink (177)
└── scheduled.rs     # run_scheduled_turn / heartbeat / cron(CronTrigger)/ send_to_target_internal (214)
```

`orchestrator.rs` 1142 行 → `mod.rs` 437 行;9 个单一职责文件共 1546 行(含新增测试与文档)。

> 与初稿 §9 的差异:未拆出 `runtime.rs`/`listener.rs`/`parts.rs`(留在 `mod.rs`),`inbound`/`scheduled` 用单文件而非子目录——关联极紧的拦截器/调度逻辑同处一文件比强行多目录更易读。详见 §12 说明。

---

## 10. 落地清单(实施顺序)

一次性大重构,内部按依赖顺序分步落地,**每步独立提交、保持 `cargo build`/`cargo test` 绿**(实际提交顺序):

1. 目录化:`orchestrator.rs` → `orchestrator/{mod,event,scheduled}.rs`(纯机械)。
2. **`key.rs`**:`SessionKey` / `SubAgentKey` + 单测;替换全部裸键解析。
3. **`turn.rs`**:`ResolvedTurn::resolve` —— 消除 recovery 两处 TurnContext 组装重复。
4. **`recovery.rs`**:两份 recovery 合并为 `spawn_recovery` + `CompletionSink` + `run_startup`。
5. **`ctx.rs`**:`OrchestratorCtx` 依赖包;`self.X` → `self.ctx.X`;删 accessor 与 `SharedSessions`;scheduled / webhook(`WebhookContext.ctx`)/ daemon 改用 `Arc<OrchestratorCtx>`。
6. **`inbound.rs` + `delegation.rs`**:`Interceptor`/`Flow`/5 拦截器/chain runner + `dispatch_turn`;delegation 走 `dispatch_turn`;删 `handle_channel_event` 等;链顺序 golden 测试。
7. **`run(self)` + 合流**:receiver/handle 改 owned;`tokio_stream::merge`;删 take-once 仪式与 `shutdown_listeners`;daemon 去 `Arc`。
8. **`CronTrigger` + webhook 去重**:折叠 `SchedulerEvent::Cron`(丢 4 死字段);`run_scheduled_task` 委托 `run_scheduled_turn`。
9. **`test_support.rs` + 拦截器行为单测**:可注入 mock 的 `OrchestratorCtx` 夹具。
10. **全量验证**:`cargo clippy --all-targets -- -D warnings` + `cargo test`(415)全绿。

---

## 11. 风险与缓解(一次性大重构的代价)

合理性第一,坦白讲清:

1. **爆炸半径大**:删 accessor 同时改动 webhook server、daemon 组装根、scheduled。缓解:纯结构重排、行为不变,靠"每拦截器单测 + inbound 链顺序 golden 测试 + 全量回归 + clippy `-D warnings`"兜底,而非靠小步可回滚。
2. **`process_turn` 边界**:终端分发走 `process_turn`,recovery 走 `run_recovery`;`resolve_turn` 要服务两条路而不改语义,落地前核对 `process_turn` 内部是否已自带等价解析(避免双重解析)。
3. **子代理键 2 段**:刻意用 `SubAgentKey` 单独建模,不为"统一"硬塞进 `SessionKey` 导致 `Option` 字段(那是新坏味道)。
4. **不可二分定位**:大改一次合入,bisect 失效;测试覆盖须在合入前补齐,尤其 inbound 链顺序与短路语义(历史上最易出隐性回归处)。

---

## 12. 验收标准与进度

实施按 §10 顺序分步落地,每步保持 `cargo build` / `cargo test` 绿、独立提交。

**已完成**
- [x] `orchestrator/` 目录模块组建立(`orchestrator.rs` / `_event.rs` / `_scheduled.rs` → `orchestrator/{mod,event,scheduled}.rs`)。
- [x] `SessionKey` / `SubAgentKey` 值类型;`splitn(3, ':')` 仅存在于 `SessionKey::parse`,裸 `format!("{}:{}:{}")` 会话键消除。
- [x] `turn.rs` `ResolvedTurn`:per-turn 参数解析单一归属,消除 recovery 两处重复的 TurnContext 组装。
- [x] 两份 recovery 合并为 `recovery.rs` 的单一 `spawn_recovery` + `CompletionSink`(`run_startup` 驱动两条循环)。

- [x] `ctx.rs`:`OrchestratorCtx` 依赖包;`Orchestrator` 持 `Arc<OrchestratorCtx>`,内部访问走 `self.ctx`。删除全部 per-field accessor 与 `SharedSessions`,只留 `ctx()`。
- [x] inbound 责任链:`Interceptor` + `Flow` + 5 拦截器(ask/callback/crash-recovery/command/dispatch)+ chain runner;含链顺序 golden 测试 + 各拦截器行为单测(`test_support.rs` 提供可 mock 的 `OrchestratorCtx`)。
- [x] `scheduled` 去重 + `CronTrigger`(丢弃 4 个死字段:delivery/enabled_tools/disabled_tools/provider);消除第 4 处 scheduled-turn 重复——`scheduler.rs::run_scheduled_task` 改为委托 `run_scheduled_turn`。
- [x] delegation 改走 `dispatch_turn`,不再构造 synthetic `ChannelMessage` 跑完整 inbound 链(去掉 box 递归与 sender 暗坑)。
- [x] 事件源 `Stream` 化 + `tokio_stream::merge` 合流(取代 3 个 adapter task)。
- [x] `run(self)` 化,消除全部 `Arc<TokioMutex<Option<Receiver>>>` take-once 仪式;`run` 自行 abort listeners(删 `shutdown_listeners`)。
- [x] webhook 迁移到 `Arc<OrchestratorCtx>`(`WebhookContext.ctx`);daemon 传 `orchestrator.ctx()`。
- [x] 收尾:`cargo clippy --all-targets -- -D warnings` 全绿、`cargo test` 415 绿。

**说明 / 后续**
- `ChannelRegistry` newtype 未单独引入:`channels` 仍是 `OrchestratorCtx` 内的 `Arc<DashMap>`(本身即注册表),`OrchestratorCtx::channel(&account)` 收口查找;刻意避免大面积改动,satisfy 依赖包的核心目标。
- inbound 拦截器测试已落地:`test_support.rs` 提供可注入 mock 的 `OrchestratorCtx`(内存 `SessionManager` + 全新 `AskRouter` + no-op `ProviderRegistry`/`AgentRuntime` + 记录式 `MockChannel`)。覆盖 ask-reply / callback(retry 改写、abort ack、无 pending 通知)/ crash-recovery / `retry_abort_prompt` 32 字符前缀截断;链顺序由 golden 测试钉死。**`slash_command` / `dispatch_turn` 终端**会 spawn `process_turn`(走 LLM),其行为单测需要真实 provider,留作后续(端到端层面)。
- cron `session_key` 保持 `String`(可能是 `_cron_<id>` / `_hooks_agent` 等非三段式键),不强转 `SessionKey`。
