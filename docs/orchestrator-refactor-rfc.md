# Orchestrator 重构 RFC

> 状态:草案 → 实施中
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
| 7 | **字符串当类型**:`splitn(3,':')` / `format!("{}:{}:{}")` / 11 字段的 `SchedulerEvent::Cron` / `run_cron_task` 11 位置参数 | 122、324、55–66、811 | 引入 `SessionKey`、`CronTrigger`、`TurnOverrides` 值类型 |

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

```rust
pub struct OrchestratorCtx {
    pub sessions:   Arc<SessionManager>,
    pub runtime:    AgentRuntime,
    pub channels:   ChannelRegistry,          // 包一层,不再裸 DashMap
    pub ask:        Arc<AskRouter>,
    pub scheduler:  Option<SharedScheduler>,
    pub delegator:  Option<Arc<DelegationCoordinator>>,
}

pub struct Orchestrator {
    ctx:       Arc<OrchestratorCtx>,
    events:    BoxStream<'static, OrchestratorEvent>,  // 已合流
    listeners: Vec<JoinHandle<()>>,                    // Drop 时 abort
}

impl Orchestrator {
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<()>;
}
```

`ChannelRegistry` 包裹裸 `Arc<DashMap<(String,String), Arc<dyn Channel>>>`,提供 `get(&SessionKey)`,收口散落各处的 `channels.get(&(ct.clone(), ac.clone()))`。

---

## 4. 值类型:让"会话键"成为一等公民

会话键当前是裸 `String`,到处 `splitn`/`format!`。引入值类型:

```rust
/// 用户会话键:channel_type:account_id:sender
pub struct SessionKey { pub channel: String, pub account: String, pub sender: String }
impl SessionKey {
    pub fn parse(s: &str) -> Option<Self>;
    pub fn account_key(&self) -> (String, String);   // ChannelRegistry 查表用
}
impl Display for SessionKey;  // "ct:ac:sender"

/// 子代理会话键是 2 段(agent_name:sub_session_id),形态不同 —— 单独建模,不硬塞进 SessionKey
pub struct SubAgentKey { pub agent: String, pub sub_session: String }
```

`parse_session_key` / `Self::session_key` / 所有 `splitn(3,':')` 与 `format!("{}:{}:{}")` 全部删除。delegation 那个"synthetic.sender 必须等于父 sender"的暗坑(原 1087 行注释)随之消失 —— 我们传 `SessionKey` 值,不再把键塞进伪造消息字段里再解析回来。

调度事件同步去字符串化:

```rust
pub enum OrchestratorEvent {
    Inbound { account: (String, String), msg: ChannelMessage },
    Tick(SchedulerTrigger),       // Heartbeat | Cron(CronTrigger)
    Delegation(DelegationEvent),
    AskReply { session_id: String, reply: ChannelMessage },
}

pub struct CronTrigger {          // 替掉 11 字段的 SchedulerEvent::Cron
    pub key: SessionKey,
    pub prompt: String,
    pub target: DeliveryTarget,   // Option<channel> + Option<account> 收成一个
    pub job_id: String,
    pub delivery: Option<DeliveryConfig>,
    pub overrides: TurnOverrides, // model/provider/enabled_tools/disabled_tools 收成一个
}
```

`run_cron_task` 的 11 个位置参数随之坍缩成 `run_cron_task(ctx, trigger)`。

---

## 5. 事件循环:合流,不手搓

```rust
pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    Recovery::run_startup(&self.ctx);          // §8:恢复,fire-and-forget spawn

    loop {
        let ev = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            ev = self.events.next() => match ev { Some(e) => e, None => break },
        };
        if *shutdown.borrow() || crate::is_shutting_down() { break; }   // 热切换检查点
        match ev {
            OrchestratorEvent::Inbound { account, msg } => Inbound::dispatch(&self.ctx, account, msg).await,
            OrchestratorEvent::Delegation(e)            => Delegation::wake(&self.ctx, e).await,
            OrchestratorEvent::Tick(t)                  => Scheduled::dispatch(self.ctx.clone(), t),
            OrchestratorEvent::AskReply { session_id, reply } => { self.ctx.ask.fulfill(&session_id, reply); }
        }
    }
    Ok(())   // self drop → listeners abort
}
```

`self.events` 由 `event.rs` 合流:

```rust
fn build_events(
    inbound_rx: mpsc::Receiver<((String,String), ChannelMessage)>,
    sched_rx:   Option<mpsc::Receiver<SchedulerTrigger>>,
    deleg_rx:   Option<mpsc::Receiver<DelegationEvent>>,
) -> BoxStream<'static, OrchestratorEvent> {
    let inbound = ReceiverStream::new(inbound_rx).map(|(a, m)| OrchestratorEvent::Inbound { account: a, msg: m });
    // sched/deleg 为 None 时用 stream::empty();select_all 合流
    select_all([inbound.boxed(), ticks, deleg]).boxed()
}
```

→ 不再有 3 个 adapter task、不再 clone `event_tx`、不再末尾 `.abort()` 三连。

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

```
agents/orchestrator/
├── mod.rs            # 模块文档 + 对外 re-export
├── ctx.rs           # OrchestratorCtx(依赖包)+ ChannelRegistry
├── key.rs           # SessionKey / SubAgentKey
├── runtime.rs       # Orchestrator{ctx,events,listeners} + run(self) + Drop(abort)
├── event.rs         # OrchestratorEvent + SchedulerTrigger + build_events(合流)
├── listener.rs      # spawn_listener(重连退避)→ inbound 流
├── turn.rs          # resolve_turn / TurnOverrides / DeliveryTarget
├── parts.rs         # OrchestratorParts(组装根入参)+ new()
├── inbound/
│   ├── mod.rs       # Interceptor trait + Flow + dispatch(chain runner)
│   ├── ask.rs
│   ├── callback.rs
│   ├── recovery_prompt.rs
│   ├── command.rs
│   └── dispatch.rs  # DispatchTurn(终端)+ 共享 dispatch_turn 函数
├── scheduled/
│   ├── mod.rs       # dispatch(trigger)
│   ├── heartbeat.rs # 预检(读 HEARTBEAT.md/状态/due 过滤)+ 执行,合在一处
│   └── cron.rs      # CronTrigger → run
├── delegation.rs    # wake():系统唤醒,复用 dispatch_turn
└── recovery.rs      # 启动恢复(会话 + 子代理)统一为 recover_one
```

`orchestrator.rs` 从 1142 行 → `runtime.rs` ≈ 120 行;每个 interceptor 30–60 行。

---

## 10. 落地清单(实施顺序)

虽是一次性大重构,内部仍按依赖顺序分步推进,每步保持 `cargo build` 可过:

1. **`key.rs`**:`SessionKey` / `SubAgentKey` + 单测(纯函数,零依赖,先落)。
2. **`ctx.rs`**:`OrchestratorCtx` + `ChannelRegistry`。
3. **`event.rs` / `listener.rs`**:`OrchestratorEvent` + `SchedulerTrigger` + 合流 + listener 流。
4. **`turn.rs`**:`resolve_turn` / `TurnOverrides` / `DeliveryTarget`。
5. **`inbound/`**:`Interceptor` + `Flow` + 5 个拦截器 + chain runner,逐拦截器补单测。
6. **`scheduled/`**:heartbeat 预检+执行合并;cron 用 `CronTrigger`。
7. **`delegation.rs` / `recovery.rs`**:wake 走 `dispatch_turn`;recovery 合并为 `recover_one`。
8. **`runtime.rs` / `parts.rs`**:`Orchestrator` + `run(self)` + `new()`。
9. **删除旧文件**:`orchestrator.rs` / `orchestrator_event.rs` / `orchestrator_scheduled.rs`。
10. **改 `mod.rs` 与调用方**:`daemon.rs`、webhook server 改用 `Arc<OrchestratorCtx>`,删 accessor。
11. **全量验证**:`cargo clippy --all-targets -- -D warnings` + `cargo test` 全绿。

---

## 11. 风险与缓解(一次性大重构的代价)

合理性第一,坦白讲清:

1. **爆炸半径大**:删 accessor 同时改动 webhook server、daemon 组装根、scheduled。缓解:纯结构重排、行为不变,靠"每拦截器单测 + inbound 链顺序 golden 测试 + 全量回归 + clippy `-D warnings`"兜底,而非靠小步可回滚。
2. **`process_turn` 边界**:终端分发走 `process_turn`,recovery 走 `run_recovery`;`resolve_turn` 要服务两条路而不改语义,落地前核对 `process_turn` 内部是否已自带等价解析(避免双重解析)。
3. **子代理键 2 段**:刻意用 `SubAgentKey` 单独建模,不为"统一"硬塞进 `SessionKey` 导致 `Option` 字段(那是新坏味道)。
4. **不可二分定位**:大改一次合入,bisect 失效;测试覆盖须在合入前补齐,尤其 inbound 链顺序与短路语义(历史上最易出隐性回归处)。

---

## 12. 验收标准

- [ ] `orchestrator.rs` / `orchestrator_event.rs` / `orchestrator_scheduled.rs` 删除,代之以 `orchestrator/` 模块组。
- [ ] 无任何 `Arc<TokioMutex<Option<Receiver>>>`;`run` 签名为 `async fn run(self, ...)`。
- [ ] 无裸会话键字符串解析(`splitn(3, ':')` 仅存在于 `SessionKey::parse`)。
- [ ] delegation 不再构造 synthetic `ChannelMessage` 走完整 inbound 链。
- [ ] 两份 recovery 合并为单一 `recover_one`。
- [ ] 每个 inbound 拦截器有独立单测;inbound 链顺序有 golden 测试。
- [ ] `cargo clippy --all-targets -- -D warnings` 与 `cargo test` 全绿。
