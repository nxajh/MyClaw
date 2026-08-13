# Turn 挂起延续 RFC（Turn Suspension & Continuation）

> 状态:已实现(2026-08-10 讨论收敛 + 审查修正;P0-1/P0-2/P0-3/P1-1/P1-2/P1-3 已合入 master,P1-4 测试补齐 + RFC 定稿,本提交;2026-08-10 二次审查修正:输出静默保留,silenced_outputs 回填机制取消,改以 Claude Code 式事前约束保证连续性,§3.3;2026-08-10 三次修正:静默轮输出不屏蔽,转换压缩为 `[进度]` 消息发出,不终结 turn,§3.3;2026-08-10 四次修正 fix v2(E2E 双委托复测):`[进度]` 改由**系统生成**(wake 终态摘要,不转发模型 EndTurn 回复),silenced 轮 `EndTurn` 语义化为 `Continue`(含 history usage 同步补丁),约束语强化"输出不转发",§3.3;2026-08-10 五次修正 fix v2 方案 B:挂起期间 telegram 仅显示**一条 live-edit 进度预览**(跨恢复轮原地编辑,最终轮删除),**绝不逐轮发独立消息**;挂起期间消息一律不发用户(模型输出只落盘);非 telegram 通道(无 edit+delete)挂起期间完全静默;`[进度]` 独立消息机制整体移除(`build_progress_text`/`format_progress_message` 删除),§3.3;2026-08-12 六次修正:挂起轮模型输出**按普通轮次处理**(正常流式 + commentary + 正常投递),移除整个 ⏳/✅ progress preview 机制(`render_progress_preview`/`update_progress_preview`/`take_progress_preview_for_cleanup`/折叠/`ProgressPreview` 结构/`progress_text` 字段全删),§3.3;2026-08-12 七次修正:投递门按「流式是否已实际显示」细化,§3.3;2026-08-12 架构修正:has_pending 语义重定义(origin 轮 `async_delegation_spawned` || 恢复轮 `silenced`),单 preview 跨 turn 接管(恢复轮保留历史行追加,最终轮 collapse),挂起期间用户消息显式排队(静默,最终轮后按序 drain),§3.7/§4;2026-08-13 八次修正:完成通知投递队列持久化(delegation-notice-queue-rfc §5)——`run_startup` 在 suspension 恢复循环**之后**挂 `recover_completion_queue` 独立恢复线,全量重入队(弃用 results-skip),与 `recover_suspension` 交叉幂等,§4)
> 范围:`agents/agent.rs`(EndTurn 挂起标记) + `agents/delegation_coordinator.rs`(终态事件) + `agents/orchestrator/delegation.rs`(wake) + `agents/orchestrator/inbound.rs`(dispatch 判定 + 恢复锁) + `agents/session_context.rs`(挂起状态挂载) + `storage/session.rs`(持久化)
> 原则:**主 agent 派发 async 子 agent 后 turn 不结束**;每个子 agent 终态事件各自唤醒主 agent 处理(不聚合等待);全部收尾后主 agent 汇总输出,完整 turn 才结束。
> 实现机制(审查修正):**方案 X**——`Agent::run` 不改内部循环,照常返回 `TurnResult`;挂起是**语义概念**(history 连续),恢复 = 事件到达时 `process_turn` 一次(复用现有 wake → dispatch 路径),新增的只是挂起状态管理 + 静默轮输出转换(不终结 turn,§3.3) + turn 边界判定。

## 0. 背景

对比三家子代理机制源码(Codex / OpenClaw / Hermes)后,myclaw 现状是:

- **sync** `agent_delegate`:同轮阻塞,主 agent 在同一个 tool call 里等结果(Codex `wait_agent` 同款)。
- **async** `agent_delegate`:fire-and-forget,立即返回 task_id,子 agent 终态/消息通过 `DelegationEvent` → `orchestrator/delegation.rs::wake` → **新 turn**(事件驱动),主 agent 的原始 turn 早已在派发后 EndTurn 结束。

用户定义的第三种模型(方案 C):**挂起延续**——主 agent 的 turn 从派发 async 子 agent 起挂起,子 agent 结果逐个到达、逐个唤醒处理(不聚合),全部收尾后主 agent 汇总输出,**这个 turn 才算结束**。本质 = Codex 同轮阻塞的自动版(免显式 wait)+ Hermes batch 聚合的分散版(按完成先后逐个注入)。

与 A/B 的关键差异:

| 维度 | A sync(现状) | B async(现状) | C 挂起延续(本 RFC) |
|---|---|---|---|
| 派发后主 agent | 阻塞等单个结果 | turn 结束,事件驱动新 turn | turn 挂起,等全部收尾 |
| 多子 agent 并行 | 不支持(串行) | 支持 | 支持 |
| 结果回传形态 | tool 返回值 | 逐事件新 turn,主 agent 上下文割裂 | 逐事件注入同一挂起 turn,上下文连续 |
| 用户视角 | 派发即等 | 多次独立回复 | 一次输入 → 一个完整回复(中间进度通知) |
| turn 边界 | 单次 | 每次事件一个新 turn | 完整 agentic run 一个 turn |

## 1. 目标与非目标

**目标**
- async 派发后主 agent turn 挂起(不结束),全部子 agent 收尾后汇总输出才结束。
- 每个子 agent 终态(Completed/Failed/TimedOut)到达 → **各自**唤醒恢复主 agent,注入该事件(带状态标签),不等待其他未完成子 agent。
- 挂起期间用户插话:普通消息排队(现有 turn_lock 机制,零新增);`/btw` 旁路即时回答(现有 slash command,独立上下文,零新增)。
- 挂起状态持久化,重启后恢复(遗留 pending 按 Failed 处理)。
- 递归嵌套深度上限可配置。

**非目标**
- 不引入 `wait` 参数(验证:cron/定时走 `orchestrator/scheduled.rs` 独立调度路径,不经 `agent_delegate`;所有 delegate 调用来自主 agent LLM 决策,派发即为等结果,无 fire-and-forget 场景)。
- 不聚合注入(用户明确:多个子 agent 完成时间不同步,先完成的空等最慢的不可接受)。
- 不改 sync 语义(保持阻塞返回结果)。
- 不新增"打断挂起"重机制(btw 已覆盖即时插话)。
- 不做子 agent ↔ 子 agent 通信。

## 2. 核心语义

### 2.1 turn 定义与实现机制(方案 X)

turn = 一次用户/系统输入触发的**完整 agentic run**(内含多轮 LLM API 调用与工具调用),以向用户输出最终回复为结束。

**方案 X:挂起是语义概念,不是进程内协程挂起。** `Agent::run` 内部循环不改:每次 `process_turn` 都是完整的 run,到 `StopReason::EndTurn` 返回 `TurnResult`(附挂起标记 `has_pending`)。"同一 turn"由 **history 连续性**体现——挂起轮次的注入与输出照常落盘,模型在下次恢复时看到完整连续上下文,与用户视角的"一次输入一次完整回复"一致。

```
用户输入 ──→ process_turn(run) ──→ tool agent_delegate(async) ──┐
                                     EndTurn: pending 非空 ──→ 挂起态(记录,返回)
                                                              │
              ┌────────────────────────────────────────────────┘
              │ t1 终态(Completed) → wake → dispatch_turn(注入 t1,run)
              │                       输出以进度通知发出(不终结;约束语告知模型) → EndTurn: pending=[t2] → 挂起
              │
              ┌────────────────────────────────────────────────┘
              │ t2 终态(TimedOut) → wake → dispatch_turn(注入 t2,run)
              │                       pending 空 → 汇总输出给用户 → 完整 turn 结束
```

### 2.2 状态机与挂起状态

```
Running ──EndTurn 且 pending 非空──→ Suspended(仅记录,run 已返回)
Suspended ──终态事件 t_i──→ Running(dispatch_turn 注入 t_i,run 一轮,输出进度通知+约束语)
Running ──EndTurn 且 pending 空──→ Finished(汇总输出,完整 turn 结束)
```

挂起状态定义(挂 SessionContext,持久化见 §5):

```rust
pub struct TurnSuspension {
    pub origin_turn_seq: u64,                  // 触发挂起的 turn 序号(无 TurnId 类型,用自增序号)
    pub suspended_at: u64,                     // 挂起开始 unix 秒(重启恢复时长统计)
    pub pending: Vec<String>,                  // 未收尾子 agent task_id
    pub results: Vec<SubResult>,               // 已收尾结果,按完成顺序追加
    pub progress_by_task: HashMap<String, Vec<String>>, // 每任务丢弃的 Progress 暂存(task_id → 文本列表)
}

pub struct SubResult {
    pub task_id: String,
    pub status: SubStatus,                     // Completed | Failed | TimedOut
    pub content: String,                       // 终态消息内容(summary / 错误 / 超时)
    pub sent_message_count: u64,               // 子 agent 中途主动发消息数(降噪判定)
    pub progress: Vec<String>,                 // 丢弃的 Progress 合并于此(永不注入上下文)
}
```

> ⚠️ **progress_by_task 暂存(P0-2 定稿)**:`Progress` 消息永不注入上下文(§2.3),在任务收尾前先按 `task_id` 暂存于 `progress_by_task`(serde,可随 §5 持久化);终态到达时 `record_terminal` 内 fold 进该任务 `SubResult.progress` 并从暂存移除。时序边界:挂起登记(§3.1 EndTurn)之前到达的 Progress 无暂存目标、直接丢弃(不可恢复,信息量小);登记之后到达的 Progress 全部暂存,恢复轮次终态注入时以结果条目呈现。

### 2.3 事件模型(四类保留,不聚合)

`DelegationEvent` 保持四类独立,不并入 Message:

| 事件 | 触发 | 恢复注入内容 |
|---|---|---|
| `Message{kind: Progress}` | 子 agent `send_message(recipient="parent")` 中途汇报 | **永不注入上下文**(挂起与非挂起场景统一):合并进该任务 `progress` 列表,终态注入时以结果条目呈现 |
| `Message{kind: Final}` | 子 agent 主动发最终消息 | 注入 `[子代理消息]`(现有形态) |
| `Completed{summary}` | wrapper 检测子 agent 正常结束 | 注入完成通知;`sent_message_count>0` 时 summary 降级为纯元数据(保留降噪) |
| `Failed{error}` | wrapper 检测子 agent run 内部错误(provider 失败/工具 bail/panic) | 注入失败通知 |
| `TimedOut` | 墙钟超时(downcast `DelegationTimeout`) | 注入超时通知 |

> ⚠️ **新增字段**:现有 `DelegationEvent::Message(AgentMessage)` 无 kind 区分(`delegation.rs` L130),`Message{kind: Progress\|Final}` 为**本 RFC 新增**——`AgentMessage` 加 `kind: MessageKind` 字段,默认 `Final`(兼容既有发送方)。

每个事件独立走恢复路径(各自 `tokio::spawn` + turn 判定),不攒批。

## 3. 挂起生命周期

### 3.1 派发挂起

`agent_delegate(mode="async")` 返回 task_id 后,主 agent run 继续;到 `StopReason::EndTurn` 时检测挂起状态 `pending` 非空 → **记录挂起态并返回**(run 照常返回 `TurnResult`,附 `has_pending` 标记)。`agent.rs` 现有 EndTurn 返回点(`agent.rs` L445-566)为改造锚点(仅加标记,不动内部循环)。

### 3.2 终态恢复(逐事件)

子 agent 终态到达 → `delegation_coordinator.rs` 现有 L817-871 终态清理(无条件执行,不依赖事件)保留 → 事件沿现有链路(send_to_parent → mpsc → `orchestrator/mod.rs` L501-503 → `delegation.rs::wake`)到达:

- **挂起会话**(`SessionContext.turn_suspension` 非空,active 或非 active 皆然):走 `inbound::dispatch_turn`(**复用现有路径**,非 active 维持 `process_non_active` 临时 SessionContext)→ 注入该事件 → run 一轮 → EndTurn 判定。
- **非挂起会话**:维持现状(事件驱动新 turn),唯一变化是 Progress 不注入(§2.3)。
- **恢复锁(新增)**:终态事件到达与用户消息共用 `turn_tracker.track()` 排队(`dispatch_turn` L512 现有机制)——t1、t2 同时终态时两个恢复串行执行,杜绝双 LLM 循环并发读同一 history。

恢复注入形态:单事件注入(带状态标签 `[子代理 t1 已完成]` 或 `[子代理 t1 失败: ...]`),复用现有注入管线(`delegation.rs` L91-153 Message 变体合成),每条独立 msg_id 可寻址。

### 3.3 静默轮输出转换:按普通轮次处理(不终结 turn,输出正常投递) + ask_user 禁用

**方案 C(2026-08-12 六次修正)**:挂起轮(中间恢复轮)的模型输出**按普通轮次处理**——与普通轮次完全一致:正常流式(`session.turn_stream` 始终 `channel.create_stream`,不再置 `None`)+ commentary 💬 + 正常投递(流式 `FinalDelivered` 或 fallback `send_message` 兜底)。用户看到的是普通中间消息(进度说明),**不再有 ⏳/✅ 系统进度预览行、不再有单消息折叠**。中间恢复轮(`pending` 非空)与普通轮次的唯一区别:

- **turn 不终结**:模型输出 `EndTurn` 被语义化为 `Continue`(见下),`turn_result.has_pending` 置位,调度层继续挂起;后续终态事件继续恢复,直到最终轮(loud)输出完整汇总。
- **ask_user 禁用**(见下,防死锁)。

**机制(六次修正移除全部 progress preview 机制)**:以下组件整体删除,不再有任何 ⏳/✅ 系统行、不再有单消息折叠:

- `TurnSuspension.progress_preview` 字段 + `ProgressPreview` 结构(`#[serde(default)]` 兼容不再需要;旧 `suspension.json` 中的 `progress_preview` 字段作为未知字段被 serde 忽略——结构无 `deny_unknown_fields`,serde 默认忽略未知字段,往返序列化不携带)。
- `SessionContext::render_progress_preview` / `update_progress_preview` / `take_progress_preview_for_cleanup` 及 `fold_base_msg` / `residual_stream_msg_to_delete` 纯函数。
- `ChannelInboundMessage.progress_text` 字段与 `progress_text_for_notice` 直通链(`wake` 元组 7→6 项,去 `progress_body`)。
- 最终轮删除预览块、预览清理钩子、origin 折叠块、`delivered_msg_id` 折叠变量。

**投递路径(六次修正;2026-08-12 七次修正细化)**:silenced 轮 `turn_stream` 始终创建,模型输出流式显示(commentary 💬 由 telegram streaming 自动处理)。fallback send 门按「流式是否已实际显示」区分,而非一刀切放开:`if delivery != FinalDelivered && (delivery == Pending || !(silenced || has_pending))`——

- `delivery == Pending`(无流式通道 qqbot/wechat/client):仍投递,无流式时这是用户看到中间进度的唯一途径(与普通轮次一致)。
- `delivery == Visible`(流式已显示,如 Telegram)且为挂起轮(silenced 恢复轮或 origin 挂起轮 `has_pending`):**不投递**——挂起轮无工具后 final answer,`turn_result.text` 即工具前 commentary 全文,流式已以 💬 行显示,再投递独立消息即重复(origin turn 双消息现场:预览块 💬 行 + 独立裸文本 + TTS 语音,三份同文本)。
- 最终轮(`!silenced && !has_pending`):与普通轮一致正常投递完整汇总。

保留:`silenced_override`(wake 时意图判定)、`semantic_stop_reason`(EndTurn→Continue)、`SILENCE_GUIDANCE`(文案改为"本轮输出将作为进度说明正常发送给用户,请输出简洁中间进展")、TTS 扩为 `&& !silenced && !has_pending`(中间进度消息——含 origin 挂起轮——不转语音;最终轮恢复 TTS)、`on_status` Thinking/Done 仍 `if !silenced`。

**连续性由事前约束保证(2026-08-10 审查修正;约束语 fix v2 强化;2026-08-12 六次修正文案更新)**:挂起轮输出**会真实发送给用户**(作为进度说明),约束语相应更新为——"本轮为中间恢复轮:任务尚未全部完成,本轮对话不会终结;你的本轮输出将作为进度说明正常发送给用户。请输出简洁的中间进展(如已完成哪些子任务、剩余哪些),不要生成最终结论或收尾语,待全部子代理结果到达后,在最终轮输出完整汇总答复"。模型知道本轮不终结 → 不会提交最终结论 → 恢复轮自然重新汇总,无需任何回填。与 Claude Code 同构:其 fork 异步用 "results will arrive in a subsequent message" 预告恢复轮使模型不割裂,我们把同一手段用于挂起语境。

约束不保证 100% 遵守:最坏情况是中间轮写了长内容——但内容不会丢:本轮输出照常发送给用户(进度说明,可能偏长),完整内容照常落盘 history §2.1,模型翻 history 仍可引用;模型已被明确告知本轮不终结,不会误以为已给最终答复;恢复轮重新生成完整答复。信息不丢(history 落盘)、认知不错位(已告知不终结),故无需回填机制。

**EndTurn → Continue 语义映射(fix v2,2026-08-10)**:silenced 轮次的 `TurnResult.stop_reason` 若为 `EndTurn`,在 `process_turn` 内被**语义化为 `StopReason::Continue`**(纯函数 `semantic_stop_reason(silenced, has_pending, stop_reason)`,`capability_chat.rs` 新增变体,provider 永不产生)——turn 不终结,后续终态事件继续恢复;同时**同步改写 history 最后一条 assistant 消息的 `usage.stop_reason` 为 "Continue"**(`llm_usage` 用 `format!("{:?}", …)` 落盘,否则 history.jsonl 仍显示 "EndTurn")。仅最终轮(loud)保留 EndTurn。用户可观测点(history usage 字符串)与实际语义一致。

**静默判定时机修正(2026-08-10, E2E 恢复轮1 竞态修复;2026-08-12 六次修正更新理由)**:静默与否的判定**不在 turn 开始时读活快照**,而在**唤醒/路由时捕获意图**(终态事件 `record_terminal` 收集后同一同步段: `pending` 非空 → 中间轮 → `silenced_override=Some(true)`;空 → 最终轮 → `Some(false)`),随合成 `ChannelInboundMessage.silenced_override` 传入 `process_turn`。原因(六次修正后):通知轮经 `dispatch_turn` 排队(turn_lock),排队期间后续终态事件可能已清空 `pending`——活快照会把**中间轮误判为最终轮**,`has_pending` 判定(EndTurn→Continue 映射、ask_user 禁用、TTS/on_status 抑制)与 wake 时附加的约束语数据源不一致(E2E 恢复轮1 即此竞态)。约束语与 silenced 意图同源(wake 时 `has_pending_delegations()`,与 `!snap.pending.is_empty()` 等价,record_terminal 与 route_notice 之间无 await 点)。用户消息轮无 override(`None`),仍用 turn 开始时活快照(§4 排队语义不变)。`recover_suspension`(重启恢复)同样经 route_notice 捕获意图(未覆盖任务按 Failed 收集后,covered 仍 pending → 中间轮;全收 → 最终轮)。

**ask_user 在挂起恢复轮次禁用**(防死锁:用户回答走普通消息排队,而排队要等 turn 结束,turn 又在等回答):`ask_user` 工具检测当前为挂起恢复轮次 → 返回错误"当前 turn 挂起中,无法提问",引导主 agent 将问题写入最终汇总或改用 `/btw`。

### 3.4 全部收尾

`pending` 归零 → 当前事件注入 + run → 最终轮次输出完整汇总(正常发送;可带聚合引导头,如"所有子代理结果已收齐,请汇总并输出最终结论"——引导头可选,默认不加,依赖事件注入内容自然驱动)→ 完整 turn 结束,清除挂起状态。

### 3.5 多轮派发(级联)

挂起恢复轮次中主 agent 可再次 `agent_delegate(async)`(如处理 t1 结果后决定派发 t3):新 task_id 追加进 `pending`,状态机自然支持(该轮 EndTurn 时 pending 非空 → 继续挂起)。

### 3.6 挂起最长等待

挂起持续时间上限 ≈ max(pending 子 agent 墙钟超时),即子 agent 超时(默认 600s)并行计时,最慢者超时后终态事件必达。无需额外挂起超时机制;子 agent 超时配置即兜底。

### 3.7 架构修正(2026-08-12):turn 不挂起 + 单 preview 跨 turn 接管 + 用户消息排队

**背景**:七次修正后挂起轮输出按普通轮次处理,但挂起期间到达的用户消息经 `turn_lock` 排队后作为**独立用户轮**运行,`turn_result.has_pending` 又被无条件重写为 `has_pending_delegations()`(旧 `session_context.rs` L685),于是命令轮/独立用户轮被投递门(`suspended_turn = silenced || has_pending`)误拦、以中间轮姿态投递。本次架构修正重定义 `has_pending` 语义,并以**单 preview 跨 turn 接管** + **显式排队**替换 turn_lock 隐式排队。

**has_pending 语义重定义**(用户确认边界①/②):「本 turn 属于挂起序列」= **origin 轮**(`agent.rs` EndTurn 时 `async_delegation_spawned` 标记)|| **恢复轮**(`silenced`)。`turn_result.has_pending = turn_result.has_pending || silenced`(origin 轮标记 + 恢复轮 silenced,不再无条件 `= has_pending_delegations()`)。命令轮(挂起期间独立用户轮,`silenced=None` 且非 origin)`has_pending=false` → 正常投递。投递门/TTS 门(`!silenced && !has_pending`)沿用七次修正,不变。

**origin 轮不挂起(模型可继续)**:`agent_delegate(mode="async")` 返回 task_id 后主 agent run **继续**(可输出其他内容、继续工具调用);到 EndTurn 时仅检测 `async_delegation_spawned` 置 `has_pending` 标记并返回,不改内部循环。`SILENCE_GUIDANCE` 相应补充:"发出委托后你可以继续处理其他任务;子代理完成时会主动唤醒你并注入结果,结果未齐时不得输出最终结论"(保留既有断言子串「中间恢复轮」「不会终结」「将作为进度说明展示给用户」「输出简洁的中间进展」「不要生成最终结论」「完整汇总答复」)。

**单 preview 跨 turn 接管(边界②:保留历史行追加)**:`TurnSuspension` 新增 `preview: Option<PreviewState{reply_target, msg_id, text}>`(serde default,持久化进 `suspension.json`;daemon 重启后恢复轮同样经 `route_notice` → `silenced_override=Some` 判定接管)。origin 轮与每个 silenced 恢复轮在 `finish()` 前把流式预览身份 + 当前正文经 `fold_candidate` 回写(`set_preview`);下一个委托通知轮 `create_stream_folding` 接管该消息——**edit-in-place 保留历史行追加**(inherited_preview 头段 + 后续行追加,不清空重写),`fold_candidate` 须在 `finish()` 前调用。**最终轮**(loud)Done 才 collapse 成 summary;恢复轮(silenced)Done 经 `defer_collapse`(TurnStream trait 方法,默认 no-op;**作用于 stream 本身,非 Channel trait**)保留 preview 行不 collapse。非活跃 session(`process_non_active`,channel=None)不接管(无流式)。origin 轮(非 silenced)不 defer_collapse → Done collapse 成 summary(与 OpenClaw 单消息 draft 折叠语义一致)。edit 失败 fallback send 后新 msg_id 经 fold_candidate 回写。

**用户消息排队(边界①:静默排队,不发确认)**:`DispatchTurn` 顶部拦截 `silenced_override=None && !text.starts_with('/') && has_pending_delegations()` → `enqueue_user_message`(SessionContext `VecDeque<ChannelInboundMessage>`,runtime-only)并 return——挂起期间用户消息**不再触发独立用户轮**。最终轮(该 turn `Ok` 且 `!has_pending` 且 session 不再挂起)后 drain 第一条 `take_user_message` → 重新走完整 chain(`dispatch`,同 account_key),天然串行(turn_lock);下一条由下一轮 drain 接续,每层 drain→dispatch→spawn 均返回,无栈递归。ask-reply/callback/已知命令已被前面拦截器消费,不会误排队;`/` 前缀未知命令不排队(用户主动指令不得阻塞在挂起序列后)。**队列不持久化**(daemon 重启即丢,RFC 注明)。

## 4. 用户插话(挂起期间)

| 输入 | 行为 | 机制 |
|---|---|---|
| 普通消息 | **显式排队**(静默,不发确认),等挂起序列结束(最终轮)后按序 drain,重新走完整 chain | `DispatchTurn` 顶部拦截(§3.7)+ `pending_user_messages` VecDeque + 最终轮后 `take_user_message` → `dispatch`;队列 runtime-only 不持久化 |
| `/btw <问题>` | 即时旁路回答,独立上下文,不进历史 | 现有 `cmd_btw`(`commands/info.rs` L312),命令拦截器 `tokio::spawn` 独立执行,不拿 turn_lock,零新增 |

验证:`/btw` 构造全新 messages(system + 问题),不 touch session history;`SlashCommand` 拦截器在 turn 分发前拦截(`inbound.rs` L367+),挂起期间可用。

## 5. 持久化与恢复

- `TurnSuspension`(§2.2)序列化落盘为**独立文件** `sessions/<sid>/suspension.json`(会话持久化为 `sessions/<sid>/` 目录 + 消息级 append/load,见 `storage/session.rs`;无 session.json 单体文件)。
- **重启恢复语义(审查修正)**:子 agent 是 daemon 进程内 tokio task(`spawn_delegate_async`),**daemon 重启即全部中断,终态事件不会再到达**。恢复逻辑:daemon 启动扫描挂起文件 → 遗留 `pending` 全部按 `Failed{error:"daemon 重启,子代理中断"}` 处理 → 挂起 turn 恢复(注入失败通知)→ 主 agent 汇总,完整 turn 结束。
- 已收尾 `results` 不丢;`suspended_at` 用于恢复时向主 agent 说明挂起时长。
- `recovery.rs` 既有恢复路径同步改造:run_recovery 的 Delegate 分支(发 Completed)保留;Err 分支补 Failed 广播(见 §7)。

## 6. 递归嵌套

✅ **已实现(P1-2, `8a97219`)**。配置项 `delegation.max_depth`(默认 3,含主 agent 层,`config/mod.rs` `[delegation]`)。

- 深度计算(实现机制与初稿不同):以 `parent_session_id` 链向上 walk(`delegation_coordinator.rs::session_depth`,最多 64 层防御上限;不可解析/不存在的 session 回退 1),子代理深度 = 父深度 + 1 —— 而非初稿的"深度计数器挂 SessionContext"。
- 超限时 `agent_delegate` 返回错误(工具层 `check_depth` bail "maximum delegation depth exceeded"),不产生挂起、不登记 pending。
- 子 agent 内部再派发同样受限于自身深度(每层 +1)。

## 7. 已知缺口(随本 RFC 一并修复)

| 缺口 | 现状 | 修复 |
|---|---|---|
| `agent_kill`(cancel → abort)无事件通知 | `delegation_coordinator.rs` L296-307 `handle.abort()`,父 agent 收不到 kill 通知 | ✅ **已修复(P1-3, `40c7e16`)**:kill 后广播 `Failed{error:"cancelled"}`(不新增变体,保持四类事件) |
| `recovery.rs` `run_recovery` Err 分支无 Failed 广播 | L136-138 仅日志 | ✅ **已修复(P1-3, `40c7e16`)**:补 Failed 事件广播,挂起主 agent 可感知子 agent 恢复失败 |

## 8. 实施任务拆分

- ✅ **P0-1** `TurnSuspension` 结构 + `SessionContext` 挂载 + `Agent::run` EndTurn 挂起标记(`TurnResult` 加 `has_pending`,§2.2/§3.1)—— `712fa93`
- ✅ **P0-2** 终态事件 → 挂起恢复驱动(`dispatch_turn` 复用 + 恢复锁 turn_tracker 串行化 + 非挂起会话维持现状;`AgentMessage` 加 `kind` 字段,Progress 统一不注入,§2.3/§3.2)—— `0eb24e0`
- ✅ **P0-3** 输出静默(2026-08-10 三次修正后:静默轮输出转换 `[进度]` 消息发出,不终结 turn)+ ask_user 挂起轮次禁用 + 最终汇总结束语义(§3.3/§3.4)—— `0fafc85`(审查修正:静默保留,加事前约束语保证连续性,silenced_outputs 回填取消;`355e68a` 注入约束语;本提交实现输出转换)。**五次修正(方案 B)覆盖**:`[进度]` 独立消息机制移除,改为单条 live-edit 进度预览(§3.3)。**六次修正(2026-08-12)覆盖**:progress preview 机制整体移除,挂起轮模型输出按普通轮次处理(正常流式 + commentary + 正常投递),唯一区别仍是 EndTurn → Continue(不终结)+ ask_user 禁用(§3.3)
- ✅ **P1-1** 挂起状态持久化 `suspension.json` + 重启恢复(遗留 pending 按 Failed,§5)—— `f76f201` + `211270c`
- ✅ **P1-2** 递归嵌套上限 config(`delegation.max_depth` 默认 3,§6)—— `8a97219`
- ✅ **P1-3** 缺口修复:agent_kill 广播 Failed{cancelled} + recovery Err 补 Failed 广播(§7)—— `40c7e16`
- ✅ **P1-4** 测试(挂起/恢复/逐事件注入/并发终态/超时/重启恢复/深度上限)+ 本 RFC 更新 + CI——本提交

### P1-4 测试覆盖

| 模块(全部 `#[cfg(test)]`,模块内) | 覆盖点 |
|---|---|
| `session_context.rs` `mod suspension_tests` | 登记/has_pending 翻转、add_pending 幂等、progress 折叠进终态结果(含 sent_message_count)、3 任务乱序完成按完成顺序收集、未挂起 record_terminal→None、clear 语义(保留/清除/幂等)、8 线程并发不丢结果、persist-restore 往返(重建后结果保留、清空后 None)、corrupt/空 JSON 忽略 |
| `delegation_coordinator.rs` `mod tests` | resolve_timeout(默认/tool>config/钳 1800/0 值)、session_depth 未知回退 1、三层链边界(max_depth=3: main Ok/sub1 Ok/sub2 Err)、max_depth=1 全拒、spawn_async 登记 pending+running(task_id 含 `/t/`)、深度超限不登记、未知 agent 拒绝、cancel 广播 Failed{cancelled}(session_id=父会话)+running 移除、cancel 未知 false |
| `orchestrator/delegation.rs` `mod tests` | wake 四类终态注入(Completed/Failed/TimedOut)、Completed sent_message_count>0 降噪、Progress 抑制不唤醒、progress 先到再 Completed 折叠进 r.progress、未知 session no-op |
| `orchestrator/recovery.rs` `mod tests` | CompletionSink::Delegate deliver→Completed(session_id=parent_session_id,E29 固化)、fail→Failed、recover_suspension 未覆盖按 Failed+covered 保留、全 covered no-op |

## 9. Delegation 持久化 checkpoint/resume（P2）

### 9.1 背景

P1-1 的重启恢复语义（§5）将遗留 `pending` 全部按 `Failed{error:"daemon 重启,子代理中断"}` 处理。这在 hot-switch 场景下过于悲观：daemon 主动 shutdown（SIGINT/SIGTERM/hot-switch fork）时，运行中的 async 子代理被 drain timeout 强制中断，但 drain timeout 本身不是业务失败。

### 9.2 设计

**Checkpoint 结构**（`storage/session.rs::DelegationCheckpoint`）：

```json
{
  "task_id": "test/t/019f...",
  "parent_session_id": "test/s/parent...",
  "sub_session_id": "test/s/sub...",
  "agent_name": "coder",
  "status": "running" | "checkpointed",
  "started_at": "2026-08-11T...",
  "timeout_secs": 600,
  "allowed_tools": ["shell", "file_edit"],
  "last_checkpoint": "2026-08-11T..."
}
```

存储路径：`sessions/delegations/<task_id>.json`（JsonFileBackend），原子写入。

**生命周期**：

| 事件 | 操作 |
|---|---|
| `spawn_delegate_async` → `delegate_with_parent` 创建 sub-session | `save_delegation_checkpoint(status="running")` |
| 任务终态（Completed/Failed/TimedOut） | `delete_delegation_checkpoint` |
| `agent_kill`（cancel） | `delete_delegation_checkpoint` |
| daemon shutdown / hot-switch | `checkpoint_and_cancel_all()`：所有 running → `status="checkpointed"` + abort |
| daemon startup | `load_delegation_checkpoints()`：有 checkpoint 的 unfinished 子代理 = clean shutdown，可安全 resume |

**Shutdown 路径**：orchestrator `shutdown_all` 和 daemon hot-switch post-fork 均调用 `checkpoint_and_cancel_all()` 替代 `drain(60s)`。不再把 drain timeout 当业务失败。

**Startup 路径**：`scan_unfinished_subagents` 照常运行（基于 sub-session history）。daemon startup 额外 `load_delegation_checkpoints()` 区分：
- **有 checkpoint** = clean shutdown → 通过正常 recovery 路径 resume（`recover_async`）
- **无 checkpoint** = crash remnant → warn 日志，按现有逻辑处理

### 9.3 不变式

- checkpoint 文件在任务终态时被删除（`spawn_delegate_async` closure + `recover_async` closure + `cancel`）
- checkpoint status 只有两个值：`"running"`（spawn 时写入）和 `"checkpointed"`（shutdown 时更新）
- `DelegationStatus` 枚举新增 `Checkpointed` 变体（非 terminal — 可 resume）

### 9.4 测试覆盖

| 模块 | 覆盖点 |
|---|---|
| `json_file.rs` tests | checkpoint roundtrip（save/load/delete）、多 checkpoint + corrupt 文件跳过 |
| `delegation_coordinator.rs` tests | backend checkpoint roundtrip、`checkpoint_and_cancel_all` 清空 running + 写 checkpoint、`load_checkpoints` 返回持久化数据 |
