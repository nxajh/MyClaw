# RFC: 异步委派完成通知改造 —— yield 工具 + turn 后 drain + 投递队列持久化

- 状态：**已实施**（P0/P1/P2 已合入 master，CI 867 passed；本文档随 P3 定稿，§5 记录 P2 实施偏差，2026-08-13）
- 日期：2026-08-13
- 范围：orchestrator delegation wake 路径；`SessionContext` suspension 状态机；agent 工具面；daemon 启动恢复
- 参考实现：openclaw `sessions_yield`（`src/agents/tools/sessions-yield-tool.ts`）、hermes `completion_queue` + turn 后 drain（`tools/process_registry.py` L172-174/L1312）、openclaw 持久化投递队列（`src/infra/session-delivery-queue-*` + `src/agents/subagent-completion-delivery.ts`）

## 1. 问题（已诊断，见 2026-08-12 根因分析）

子代理完成事件（`DelegationEvent::Completed/Failed/TimedOut/Message`）从产生到注入主代理上下文的链路：

```
coordinator 发 DelegationEvent
  → wake() (orchestrator/delegation.rs L60)
    → record_terminal (suspension: pending → results)   ← 已正确
    → route_notice (L262)
      → dispatch_turn (inbound.rs L512)
        → spawn process_turn
          → turn_lock.lock().await (session_context.rs L499)  ← 排队等锁
```

三个叠加事实（2026-08-12 已确认）：

1. **主代理侧无 inbox**：`Session.sub_agent_inbox` 是子代理侧（parent→sub 方向）收件箱；主代理收子代理事件全部走 `wake → notice turn → turn_lock` 排队路径。
2. **`SILENCE_GUIDANCE` 引导模型不结束 turn**（`orchestrator/delegation.rs` L38）："结果未齐时不得输出最终结论，待全部子代理结果到达后，在最终轮输出完整汇总答复"——模型被引导继续工作/等待，不主动 EndTurn。
3. **死锁窗口**：结果要进主代理上下文必须等 turn 结束释放 `turn_lock`；被引导不结束 turn → 结果进不来 → 直到模型被迫停下（loop_breaker 上限 / 用户 /stop / 模型放弃）。

**三家参考实现（openclaw / claude-code / hermes-agent）均无此问题**，因为它们：
- 完成事件**先入独立队列**（与 turn_lock 解耦），不排队等锁；
- 主代理 turn **自然快速结束**（fork 立即返回 / 显式 yield / prompt 从不引导"不得输出结论"）；
- 主代理 idle 后**自动 drain 队列并启动新 turn**（无需用户输入）。

## 2. 方案总览

三部分，对应三家各自最强的机制：

| 部分 | 抄自 | 解决 |
|---|---|---|
| A. `sessions_yield` 工具 + prompt 改写 | openclaw | 主代理不结束 turn（确定性让位） |
| B. 完成事件入队 + turn 后 drain | hermes | 通知卡 `turn_lock`（与当前 turn 解耦） |
| C. 投递队列持久化 + 重启恢复 | openclaw | daemon 重启丢通知（可靠性） |

三者正交：A 管"主代理何时让位"，B 管"结果如何到达"，C 管"到达过程不丢"。

```
coordinator 发 DelegationEvent
  → wake()
    → record_terminal (不变)
    → 渲染 notice → ① 落盘(§5) → ② 入队(per-session mpsc)   ← 新增，不再 dispatch_turn
    → try_lock 判定：忙碌 → 只入队；空闲 → 立即 drain(§4)
  → process_turn 尾部 drain：取空队列 → 逐条跑 notice turn（复用现有 process_turn）
  → daemon 启动：扫描 Pending → 重新入队 → drain
```

## 3. A. `sessions_yield` 工具（openclaw 式确定性让位）

### 3.1 工具定义

```rust
/// 模型声明"本轮到此为止，等待子代理结果"。
/// 运行时确定性结束当前 turn（无论模型之后还输出什么）。
pub struct SessionsYieldInput {
    pub message: Option<String>,  // 可选：让位说明（透传为进度文案）
}
```

- 名称：`sessions_yield`（与 openclaw 同名，语义对齐）
- 注册：与 `agent_delegate` 并列的工具面；**常驻可见**（openclaw `catalogMode: "direct-only"`——不参与工具搜索压缩）
- 执行：
  1. 设置 `session.yield_requested = true`（同步 std 原子，无 await）
  2. 若带 message，写入当前 turn 的进度预览（复用现有 preview 机制，作为 💬 行）
  3. 返回 `{"status":"yielded","message":...}`

### 3.2 runtime 集成（agent.rs）

`agent.rs` 工具执行循环中，与 `loop_breaker` 检查并列：**每个工具执行后检查 `session.yield_requested`**，置位则：

```rust
// 等价 openclaw markYieldAborted → terminal {kind:"aborted", source:"yield_cleanup"}
if session.yield_requested.swap(false, Ordering::SeqCst) {
    // 丢弃本批剩余未执行 tool_calls（strip_trailing_tool_calls，现有逻辑复用）
    // 以 EndTurn 结束（has_pending = async_delegation_spawned，复用 L642）
    return Ok(TurnResult { stop_reason: EndTurn, has_pending: async_delegation_spawned, .. });
}
```

- `has_pending` 语义不变：yield 时若本 turn spawn 过异步委派 → 挂起而非结束（**这是关键**——yield 不是"放弃"，是"让位等结果"，suspension 序列照常）
- 未 spawn 异步委派时调 yield → 普通 EndTurn（无害，模型语义错误由 prompt 引导纠正）
- 模型不调 yield 也完全无害：自然 EndTurn 时 `has_pending` 照常（现有逻辑），完成事件照常走队列。yield 是**加速 + 确定性**，不是唯一路径

### 3.3 prompt 改写（替换 SILENCE_GUIDANCE）

`orchestrator/delegation.rs` L38 的 `SILENCE_GUIDANCE` 重写为：

```
[系统提示] 本轮为中间恢复轮：任务尚未全部完成，你的本轮输出将作为进度说明展示给用户。
你可以继续处理其他任务；若需要等待子代理结果，请调用 sessions_yield 结束当前轮——
子代理完成时会自动唤醒你并把结果作为下一条消息注入。绝不轮询（不要反复查询子代理状态）。
```

关键变化：
- **删掉**"结果未齐时不得输出最终结论""待全部子代理结果到达后，在最终轮输出完整汇总答复"——这两句是死锁根源
- **新增**"调用 sessions_yield 结束当前轮"——给模型一个合规的让位方式
- **新增**"绝不轮询"——openclaw 原文（`system-prompt.ts` L124 "never poll"）；hermes/claude-code 同款
- 保留"结果将作为下一条消息注入"——Claude Code fork pre-announcement 语义，模型知道结果会来，不会以为被遗弃

### 3.4 兜底策略（不依赖模型自觉）

即使模型既不调 yield 也不自然 EndTurn（继续输出/调工具），现在也**不会死锁**：
- 完成事件已入队（B 部分），不占 `turn_lock`；
- 主代理 turn 无论如何会在某点结束（loop_breaker / 模型输出完 / 用户消息），尾部 drain 兜底；
- 极端情况（模型无限输出）：与现状相同由 loop_breaker 截断，但通知不再丢失（持久化后重启也能恢复）。

## 4. B. 完成事件入队 + turn 后 drain（hermes 式）

### 4.1 数据结构（内存队列）

`SessionContext` 新增（与 `sub_agent_inbox` 对称但方向相反）：

```rust
/// 主代理侧：子代理完成通知投递队列（sub→parent 方向）。
/// wake 只入队，不 spawn turn；process_turn 尾部 drain。
pub delegation_notice_queue: Mutex<VecDeque<DelegationNotice>>,
```

```rust
pub struct DelegationNotice {
    pub id: String,              // synthetic_id：delegation:{sub_session_id} / delegation-msg:{msg_id}
    pub content: String,         // 渲染好的通知文本（wake 现有渲染逻辑，含 progress 折叠）
    pub silenced_override: Option<bool>, // 入队时快照（见 §4.4 竞态）
}
```

> P1 实现（2026-08-13）取最小字段集：`status` / `sent_message_count` / `enqueued_at`
> 由 P2 持久化引入（§5.2 的 `CompletionNoticeEntry`），P1 的 drain 分流只用
> `silenced_override`（`id` 兼作去重键）。

### 4.2 wake() 改造

`orchestrator/delegation.rs::wake`：

1. **record_terminal 不变**（suspension 更新是同步、幂等的，先于入队——与现状一致）
2. **渲染 notice 不变**（Completed/Failed/TimedOut/Message 的 content 构造、progress 折叠、降噪——现状逻辑全部保留）
3. **入队替代 dispatch_turn**：
   - `silenced_override` 在入队时快照（= `has_pending_delegations()` 的同步结果，同现状 route_notice L289-290）
   - `delegation_notice_queue.push(notice)`
4. **唤醒判定**（替代现在的无条件 dispatch_turn）：
   - `turn_lock.try_lock()` 成功 → **空闲**：立即 drain（等价现在 dispatch_turn 的即时性，但 drain 是批量语义）
   - 失败 → **忙碌**：只入队，等 `process_turn` 尾部 drain

> 注：`try_lock` 判定与方案 1 的判定相同，但方向相反——方案 1 是"忙碌 → turn 内注入"，本方案是"忙碌 → 排队等 turn 结束，尾部 drain"。无 turn 内注入，与三家参考实现一致（hermes 注释明示 "never spliced into an in-flight turn"）。

### 4.3 drain 触发点（三个）

1. **turn 尾部**（`dispatch_turn` 的 turn task，`process_turn` 返回、`turn_lock` 释放后）：
   drain 队列，逐条跑 notice turn（复用现有 `process_turn` 路径，此刻锁空闲，立即执行）。这是主路径。
   P1 实现（2026-08-13）落在这里而非 `session_context.rs` —— `process_turn` 尾部无
   `OrchestratorCtx` 访问权（`SessionContext` 不持有 ctx），drain 需要 ctx 解析 session/
   channel/active 判定，故放在持有 ctx 的 `dispatch_turn` spawn task 尾部。
2. **wake 入队时发现空闲**（§4.2）：立即 drain，保持完成事件的即时性（主代理 idle 时结果不延迟）。
3. **daemon 启动恢复**（§5.4）：扫描持久化 Pending → 重新入队 → drain。

**drain 实现**（hermes `drain_completion_queue` 同构）：

```rust
pub async fn drain_delegation_notices(ctx, session_id) {
    let notices = take_all(&queue);          // 一次取空（非阻塞）
    for notice in notices {
        if seen_ids.contains(&notice.id) { continue; }   // 去重
        seen_ids.insert(notice.id);
        // 复用现有 route_notice 的 active/non-active 分流 + process_turn
        route_notice_locked(ctx, session_id, notice).await;
    }
}
```

- **批量语义**：一次取空，逐条处理。多个子代理同时完成 → 在一个 drain 点全部消化，逐个注入（每个 notice 是独立 turn，保持上下文边界）。
- **去重**：drain 内按 `id` 去重（同 id 只跑一次）。跨 drain 的重复由 `record_terminal` 幂等 + `synthetic_id` 唯一性保证（一个 sub_session_id 的终端事件只产生一条 `delegation:{id}`）。
- **用户消息优先级**：`dispatch_turn` 现有排队条件（L521-531）不变——suspension 期间用户消息静默排队，恢复轮统一 drain。drain 的 notice turn 之间用户消息不插入（保持 suspension 序列原子性，与现状一致）。

### 4.4 竞态与 suspension 状态机适配

| 现状点 | 适配 |
|---|---|
| `bump_notice_turn`（dispatch 时计数） | 改为 **drain 时**计数（notice turn 真正开始时）。队列中未 drain 的通知不算 in-flight |
| `finish_notice_turn`（RAII 释放） | 不变（process_turn 尾部 notice_guard 释放） |
| `clear_suspension_if_collected` 条件 | 增加"队列空"：`pending.is_empty() && notice_turns_in_flight==0 && delegation_notice_queue.is_empty()`——队列非空说明还有通知要注入，suspension（及其 results/preview）必须存活到最后一个通知注入完 |
| `has_notice_turns_in_flight` | 语义扩展：true 当 `notice_turns_in_flight>0 \|\| 队列非空`（dispatch_turn 排队条件复用，防止用户消息插队到未 drain 的通知前） |
| `silenced_override` | 入队时快照，drain 时直接用（不重算）——避免"入队后 pending 清空，drain 时重算误判为 loud"竞态（现状 route_notice L289-290 的修复逻辑原样迁移到入队点） |
| `enqueue_user_message` 排队 | 不变（suspension/notice in flight 期间用户消息排队） |

## 5. C. 投递队列持久化（openclaw 式）

### 5.1 目标

daemon 重启（hot switch / crash / SIGKILL）时，**已产生但未注入**的完成通知不丢（至少一次投递）。现状：完成事件在内存中，wake 后进程若死，通知丢失（子代理结果已产生，父代理永远不知道）。

### 5.2 存储（复用 inbound-spool RFC 基础设施）

与 `docs/inbound-spool-rfc.md` 的 `SpoolEntry` 同构，新增一类条目（或复用同一存储目录）：

```rust
#[derive(Serialize, Deserialize)]
pub struct CompletionNoticeEntry {
    pub seq: u64,               // 单调递增
    pub id: String,             // synthetic_id（去重键）
    pub sub_session_id: String,
    pub parent_session_id: String,
    pub status: Option<String>, // "completed" | "failed" | "timed_out" | null(Message)
    pub content: String,
    pub silenced_override: Option<bool>,
    pub sent_message_count: u64,
    pub enqueued_at: u64,
    pub delivery_state: DeliveryState,  // Pending | Delivered
}

pub enum DeliveryState { Pending, Delivered }
```

- 每事件一个 JSON 文件，tmp/rename 原子写、启动扫描目录。
- **决策（2026-08-13 确认）**：JSON 文件。MyClaw 现有全部持久化是 JSON 文件（suspension/users/meta/cron/checkpoint），无 SQLite/嵌入式 DB 依赖（引入 rusqlite = C 编译成本 + 二进制体积 + 供应链面）；完成事件低频、无查询/索引/事务需求。未来若 spool 升级 SQLite，completion 队列跟随——不独立选型。
- **目录（P2 实施偏差 #1）**：独立目录 `.state/completion_queue/`（**不**与 spool 同目录）。原因：inbound-spool 尚未实施，且其未来 compact 会重写自身目录只保留 Pending——混放时 completion 条目会被 spool compact 误删；`.state/` 已有先例（daemon.rs tasks.json L531）。
- **seq（P2 实施）**：各自独立单调 seq，**无前缀**——恢复时从**文件名**解析 seq（内容损坏也计入，防 append 复用文件名冲突），损坏条目 warn + 跳过。
- **实施形态**：`CompletionNoticeStore { dir, seq: AtomicU64, seen: Mutex<HashSet<String>>, pending: Mutex<Vec<Entry>> }`——`open(dir)` 扫描恢复 + 清 Delivered 残留 + seq=文件名 max；`append(entry)` seen 去重（同 id 返回 None）、seq=++、tmp+write+sync_all+rename、入 pending；`mark_delivered(id)` 删文件 + 移出 pending（seen 保留）；`pending()` 供恢复扫描。**fail-open**：`open` Err → error! + `None`（降级 P1 内存投递，daemon 照常运行）；append Err → warn + 继续内存投递。

### 5.3 写入 / 确认

- **写入**：wake 入队**前**（`msg_tx.send` 之前，同 spool §3 的写入点原则）——落盘成功才入内存队列。fsync 保证。
- **落盘链（P2 实施）**：wake 尾部构造 `NoticeMeta { sub_session_id, status: Option<SubStatus>, sent_message_count }` 透传 `route_notice`；**仅活跃分支**（sctx 存在）enqueue 前 append——非活跃走 `process_non_active` 不落盘（无队列可入，窗口小）、`recover_suspension` 的 `recovery:` 合成通知不落盘、channel 缺失回退不落盘。status 存小写字符串（手写 match：completed/failed/timed_out，不依赖 serde 默认变体名）。
- **确认**：`process_turn` **返回 Ok** 后标记 `Delivered`。确认点是"notice 内容已持久化到 session 历史"（`process_turn` L579 "Record inbound and persist"），不是"进入 process_turn"更不是"LLM 处理完"——Err（LLM 调用失败等）时内容未进历史，保持 `Pending`，跨重启重投（投递层 at-least-once；同进程内不重试，见 §9 风险 1）。
- **Delivered = 删除文件（P2 实施偏差 #3）**：原稿"删除文件或写墓碑"，实施为**删除文件**（自压缩，无墓碑）。原因：dedup 键 id 每终端事件唯一（synthetic_id），重启后只扫 Pending——墓碑无跨重启去重价值；进程内 seen set 承担同进程去重。
- **标记点（P2 实施）**：两处——① `dispatch_turn_spawn` 闭包 `process_turn` Ok 后按 msg.id 前缀 `delegation` 判 notice（id 即 dedup 键）mark_delivered；② `process_non_active` 增 `notice_id: Option<String>` 参数，Ok 后 mark（drain 的 non-active 回退传 `Some(notice.id)`，否则已落盘条目永为 Pending 每次重启重投）。`recovery:`/用户消息 id 不匹配前缀，mark 返回 false 无操作。

### 5.4 重启恢复

daemon 启动（`recovery::run_startup` 扩展，挂接在 suspension 恢复循环**之后**，独立恢复线）：

1. **全量重入队（P2 实施偏差 #2，弃用原 results-skip）**：扫描全部 `Pending` 条目 → 按 seq 升序 → 逐条重新入队。原稿"若对应 sub_session 已在 suspension results 中 → 直接标记 Delivered 跳过"**弃用**——原因：`record_terminal` 发生在 notice turn **之前**，results 存在 ≠ 通知已投递；按 results 跳过会在 crash-between 窗口（results 已写、通知未注入）丢通知。重入队只会在"Ok 后 mark 前崩溃"微秒窗口产生重复——at-least-once 明确接受。
2. 对每个 parent_session_id：恢复的 session 若 active → registered_context 入队（materialization 竞争回退 `process_non_active`）+ spawn drain；否则 spawn `process_non_active(Some(notice_id))`（临时上下文注入历史，Ok 后 mark）。
3. 死信（P2 实施）：`get_by_id` 失败 / owner 解析失败 → 直接 mark Delivered + warn（投递目标已消失，与 §5.5 一致）。
4. 与 `recover_suspension`（P1-1 现有）的交互：suspension.json 恢复 + completion 队列恢复是两条独立恢复线，`record_terminal` 幂等保证交叉不重复。

### 5.5 僵尸清理（无硬截止 deadline）

openclaw 有投递硬截止（`deadlineAt` 30min + dead-letter），但其适用前提是**即时投递语义**（交互式会话，用户在线等结果，超时用户已不关心）。MyClaw 有 `process_non_active` 路径——session 切走后通知注入历史，用户切回时可见，完成事件时效性远低于 openclaw（用户可能几小时后甚至几天后切回才看到通知）。30 分钟硬截止会误杀合法延迟投递。

**决策（2026-08-13 确认）**：不设 deadline。僵尸清理交给 drain 本身——drain 时 `ctx.sessions.get_by_id(parent_session_id)` 解析失败 → 标记 `Delivered` + warn 日志（与现状 `route_notice` 的 "session not found" 行为一致）。投递目标消失（session 被删/无法解析）是 dead-letter 的唯一合理触发，每次 drain 顺带清理，队列天然不堆积，无需 timer/DeadLetter 状态机。

## 6. 与现有机制的边界

| 机制 | 关系 |
|---|---|
| suspension 恢复轮（P0-2/P0-3） | **复用**：notice turn 仍走 process_turn + silenced 语义；yield 的 EndTurn 复用 `has_pending` 挂起逻辑 |
| preview 单消息折叠（2026-08-12） | 不变：silenced 轮输出仍折叠进 preview；yield 的 message 参数作为 💬 行写入 preview |
| 子代理 inbox（parent→sub） | 不动：这是子代理侧收件箱，方向相反 |
| 用户消息排队（enqueue_user_message） | 不变：suspension/notice in flight 期间排队，恢复轮 drain |
| `process_non_active` | 不变：drain 时 active 判定沿用 route_notice 现有分流 |
| inbound-spool RFC | 同构 JSON 文件机制；两者独立实施、**独立目录**（completion 队列 `.state/completion_queue/`，§5.2），completion 队列自建最小版存储，不依赖 spool 的 seq/文件基础设施 |
| agent_kill / 超时 / recovery 广播 | 不变：所有 DelegationEvent 都走 wake 入队路径，覆盖 Completed/Failed/TimedOut |

## 7. 任务拆分

### P0: yield 工具 + prompt 改写（最小闭环，先解决"不结束 turn"）
- P0-1: `sessions_yield` 工具（定义 + 注册 + `yield_requested` 标志）
- P0-2: agent.rs 工具循环检查 yield 标志 → 确定性 EndTurn（has_pending 复用）
- P0-3: `SILENCE_GUIDANCE` 重写（删"不得输出最终结论"，加 yield 引导 + 绝不轮询）
- P0-4: 测试（yield 后 EndTurn + has_pending；未 spawn 时 yield = 普通 EndTurn）

### P1: 内存队列 + turn 后 drain（解决"卡 turn_lock"）
- P1-1: `DelegationNotice` 结构 + `SessionContext.delegation_notice_queue`
- P1-2: wake() 入队改造（保留 record_terminal + 渲染；try_lock 判定空闲/忙碌）
- P1-3: `process_turn` 尾部 drain + `drain_delegation_notices` 实现
- P1-4: suspension 状态机适配（bump 时机、clear 条件加队列空、has_notice_turns_in_flight 扩展）
- P1-5: 测试（wake 入队不 spawn、忙碌排队尾部 drain、空闲立即 drain、多事件批量、去重）

### P2: 持久化 + 重启恢复（解决"重启丢通知"）
- P2-1: `CompletionNoticeEntry` 存储（文件/SQLite，与 spool 决策对齐）
- P2-2: wake 入队前落盘 + 确认点标记 Delivered
- P2-3: daemon 启动恢复扫描 → 重新入队 → drain（含 active/non-active 分流）
- P2-4: 测试（落盘/确认/重启恢复/与 suspension 恢复交叉幂等）

### P3: 收尾
- P3-1: 更新 `docs/turn-suspension-rfc.md` 相关段 + 本 RFC 定稿（**已完成 2026-08-13**：本文档状态转"已实施"，§5 记录 P2 三处实施偏差——独立目录 / results-skip 弃用 / Delivered 删文件）
- P3-2: CI 全绿 + `myclaw update` 部署（x86 Micro 禁本地编译，走 CI）（**待部署：需用户明确确认 `myclaw update`**）

## 8. 验收标准

1. spawn 异步委派后，主代理可立即结束 turn（yield 或自然 EndTurn），不被 SILENCE_GUIDANCE 卡住
2. 子代理完成 → 通知入队 → 主代理 turn 结束后自动注入（无需用户输入触发）
3. 主代理 turn 进行中（忙碌）时完成事件不丢、不延迟到 turn 中途注入（等尾部 drain）
4. 多个子代理同时完成 → 批量 drain，逐个注入，无重复、无丢失
5. daemon 重启后，未投递的完成通知恢复并注入（至少一次）
6. 用户消息在 suspension 期间排队语义不变，不被完成通知插队
7. 原有 suspension 恢复轮 / preview 折叠 / 降噪 / progress 折叠行为全部保持

## 9. 风险与回退

- **风险 1：drain 循环内 notice turn 失败**（LLM 调用失败等）→ **决策（2026-08-13 确认）**：投递层 at-least-once，消费层不重试——`process_turn` 返回 Ok（内容已持久化到 session 历史）才标记 `Delivered`；Err 保持 `Pending`，跨重启恢复时重投（§5.4）。**同进程内不重试**：LLM 层已有内部重试（瞬态错误 L389 / 上下文压缩 L441 / 空响应 L479），返回 Err 的必然是重试无效的错误（认证、上下文不可压缩超限），立即重试大概率再失败还占 turn_lock；重启恢复是更干净的重试通道。此语义与 openclaw（`maxRetries=∞` 但只重试投递动作、不追踪 LLM 消费结果）和 hermes（drain 后不 requeue 消费失败）一致。
- **风险 2：yield 与 loop_breaker 交互**：yield 检查在工具执行后、loop_breaker 检查前（或并列）——yield 优先（模型显式让位不该被截断成 LoopBreak 错误）。
- **风险 3：`has_notice_turns_in_flight` 语义扩展影响 dispatch_turn 排队**：用户消息在"队列非空"时排队——若队列长期非空（drain 失败），用户消息被卡。缓解：drain 是同步快速路径（取空即处理），失败保持 Pending 但不阻塞队列（下次 drain 或重启重试，见风险 1），队列不会长期非空。
- **风险 4（P2 实施确认）：重复投递窗口**——"Ok 后 mark 前崩溃"（微秒级）会致重启后重投同一通知。at-least-once 明确接受：重复注入比丢失代价低（父代理看到两次完成通知，可忽略；丢失则父代理永远不知结果）。
- **回退**：`wake` 保留旧路径（入队失败/队列不存在时直接 dispatch_turn）；功能开关（config `delegation.notice_queue`）可一键回退。
