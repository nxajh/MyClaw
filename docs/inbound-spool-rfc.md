# RFC: 入站消息持久化 Spool（跨通道至少一次投递）

- 状态：已实施（2026-08-13，P1 存储层 + P2 集成 + P3 恢复/测试，CI 绿；待部署）
- 日期：2026-08-13
- 范围：qqbot / telegram / wechat 三通道统一；orchestrator 入站路径；startup recovery

## 1. 背景与问题

进程重启（hot switch / crash）时，用户已发送但 agent 尚未回复的消息会丢失。根因是**通道确认与消息处理耦合**，消息从通道到达 session 历史之前全程在内存管道中，无持久化屏障：

```
通道 WS/拉取 → 通道内部 mpsc → orchestrator spawn_listener → msg_tx
→ msg_rx (mpsc, 容量 100) → 事件循环 → inbound::dispatch → session 历史（唯一落盘点）
```

任何一步之前进程重启，消息即丢失。startup recovery（`recovery::run_startup`）只恢复**磁盘上已进入 session 历史的 incomplete turn**，对"队列中未开始处理"的消息无从恢复。

### 三通道现状（已核实代码）

| 通道 | 接收机制 | 确认/游标 | 重启丢消息？ |
|---|---|---|---|
| Telegram | getUpdates 长轮询 | offset **先持久化到磁盘再处理**（`telegram/channel.rs` L196-201） | 未确认的重拉（不丢）；已确认未处理的丢 |
| WeChat | getupdates 拉取 + buf 游标 | buf 在**内存** `SharedState`（`wechat.rs` L485），先推进再处理；有内存 dedup（`check_and_record` L1769） | 处理中那批丢；buf 回退导致重拉/重复 |
| QQBot | WS 事件推送，无 ack/重放 | 无 | 队列中未处理 + 断线期间推送全部丢失 |

共同病根：**无统一持久化屏障**。Telegram 只是恰好靠"服务器保留未确认 update 可重拉"掩盖了这一点（且它以"先确认后处理"换取了不重复，处理中重启仍丢）。

## 2. 设计目标 / 非目标

**目标**
1. 三通道统一：进程重启（hot switch / crash / SIGKILL）后，**已收到但未处理**的消息不丢（至少一次投递）。
2. 不重复：重放 + 通道重拉场景下，同一消息只处理一次（幂等去重）。
3. 改动集中：新增一个 spool 模块 + orchestrator 三个集成点；通道侧零强制改动。

**非目标**
1. 弥补 QQ 平台断线期间不补发的消息（平台限制，无法解决）。
2. Telegram debounce buffer 内未 flush 的原始消息（毫秒级窗口，见 §9）。
3. 含文件附件的消息持久化（文件 body 是运行时资源，第一版附件消息整体跳过 spool，见 §6.3）。
4. 背压/流量整形（spool 是可靠性层，不是队列系统）。

## 3. 架构

```
通道收到消息 (qqbot WS / tg getUpdates / wechat get_updates)
   ↓ 构造 ChannelInboundMessage（含通道消息 ID）
[spawn_listener 统一入口]  ← 所有通道必经
   ↓ ① spool.append(channel, account, msg)  原子写盘（fsync）
   ↓ ② msg_tx.send(InboundEnvelope { seq, channel, account, msg })
[orchestrator 事件循环]
   ↓ ③ inbound::dispatch(...).await（拦截链 → dispatch_turn → session 历史持久化 + spawn turn）
   ↓ ④ spool.mark_done(seq)                删除文件（或写墓碑）
[启动 recovery]
   ↓ ⑤ 扫描 spool 剩余 Pending 条目 → 按 seq 升序 → 重新 dispatch（去重见 §7）
```

**spool 写入点选择在 `spawn_listener`**（`orchestrator/mod.rs` L393，消费 `channel.listen()` 的循环体）：
- 所有通道消息统一经过，一处写入，通道侧零改动；
- 在 `msg_tx.send` 之前 → 队列中的消息已有磁盘副本；
- 通道内部游标（tg offset / wechat buf）推进发生在 listen 内、早于 spawn_listener——第一版**不改**（意味着 tg 存在 ms 级窗口：offset 已推进但 spool 未写，见 §9）。

## 4. 数据结构与存储

### 4.1 SpoolEntry（每消息一个 JSON 文件）

```rust
#[derive(Serialize, Deserialize)]
pub struct SpoolEntry {
    pub seq: u64,                 // 单调递增，全局唯一
    pub channel: String,          // "qqbot" | "telegram" | "wechat"
    pub account: String,          // account_id
    pub msg: PersistedChannelMessage, // 复用 message.rs 现有序列化结构
    pub status: SpoolStatus,      // Pending | Done
    pub created_at: u64,          // unix secs
}

pub enum SpoolStatus { Pending, Done }
```

- `PersistedChannelMessage`（`message.rs` L317）：`id / sender_id / receiver / text / timestamp / interruption_scope_id`——复用现有结构，`ChannelInboundMessage::to_persisted()` 已有。
- **去重键**：`(channel, account, msg.id)`。三通道 id 均稳定：qqbot 事件 `id`（`parse_c2c_message`/`parse_group_message`）、telegram `update_id` 或 debounce 合成 `debounced_{ts}`（ts 单调）、wechat `msg_id`。

### 4.2 目录布局

```
~/.myclaw/workspace/.state/inbound_spool/
└── {seq}.json              # 每条消息一个文件；status: Pending 或 Done（墓碑）
```

- **写入**：`{seq}.json` 写内容 + `fsync`（tmp 文件 + rename，与 `completion_queue.rs` 同款原子写）。
- **seq**：`AtomicU64`，open 时扫描目录取文件名最大值（损坏文件也计入，与 `completion_queue.rs` 同款容错）。**无独立 seq 文件**（实现偏差：省一次磁盘写，文件号空洞无害——seq 只是单调 id）。
- **完成**：**不删除文件**，重写 `status: Done`（墓碑）。原因：wechat buf 回退会重拉历史消息，需要能查询"该 id 已处理过"（§7 去重）。文件只增 → 定期 compact。
- **compact**（`compact_if_needed`，启动时调用）：若文件数 > 5000 或最老墓碑超 7 天，删除全部 Done 文件（Pending 保留，seq 继续递增）。**实现偏差：不重排 seq**（重写 Pending 文件零收益；空洞无害），原子替换仅对单个文件写入，目录级重写非必需。
- **恢复 seq**：open 时扫描目录取 max+1。

## 5. 模块与 API

新文件：`src/storage/inbound_spool.rs`（与 `completion_queue.rs` 同层；RFC 初稿写 `src/channels/spool.rs`，实施改为 storage 层——spool 是存储组件，且复用 completion_queue 的文件骨架，同层更一致）

```rust
pub struct InboundSpool {
    dir: PathBuf,
    seq: AtomicU64,                  // open 时从文件名扫描恢复
    baseline_seq: u64,               // open 时的 max seq（重放水位线，§8.3）
    seen: Mutex<HashSet<String>>,    // 去重键 {channel}\0{account}\0{msg.id}（Pending + Done，open 时加载）
    pending: Mutex<Vec<SpoolEntry>>, // Pending 条目（与磁盘同步）
}

impl InboundSpool {
    /// 打开 spool；扫描目录构建 seen 索引、pending 集合与 seq；记录 baseline_seq。
    pub fn open(dir: PathBuf) -> std::io::Result<Self>;

    /// 写入一条 Pending 记录并 fsync。若 (channel, account, msg.id) 已存在（Pending 或 Done）则返回 Ok(None)（去重）。
    /// 附件消息不经过本方法（调用方判断，§6.3）。返回分配的 seq。
    pub fn append(&self, channel: &str, account: &str, msg: &ChannelInboundMessage)
        -> std::io::Result<Option<u64>>;

    /// 标记完成：将 {seq}.json 重写为 status: Done（fsync）。磁盘为真相——先写墓碑再更新内存 pending；
    /// 写失败则条目保持 Pending（下次重放再试，至少一次）。返回 false = 未知/已 Done（非错误）。
    pub fn mark_done(&self, seq: u64) -> std::io::Result<bool>;

    /// 启动恢复用：返回 Pending 且 seq <= baseline_seq 的条目（按 seq 升序）。
    pub fn pending(&self) -> Vec<SpoolEntry>;

    /// compact：清理 Done 墓碑，保留 Pending，不重排 seq（启动时调用）。
    pub fn compact_if_needed(&self) -> std::io::Result<bool>;
}
```

**依赖注入**：
- `OrchestratorCtx` 增加 `inbound_spool: Option<Arc<InboundSpool>>`（`ctx.rs` L176，目录打不开时 None = fail-open）；
- `spawn_listener` 从参数取 spool（`Arc`），`None` 时跳过 spool 逻辑（向后兼容、测试用）。
- **实现偏差：无 `[channels] inbound_spool_enabled` 配置项**——第一版无条件启用（fail-open 已足够），配置开关列为后续优化项。

## 6. 集成点

### 6.1 spawn_listener：写入（`orchestrator/mod.rs` L393）

```rust
while let Some(msg) = rx.recv().await {
    // 附件消息（files 非空）绕过 spool 整体——文件 body 是运行时资源（§6.3）。
    // 判定在调用方：append 保持纯去重语义，Ok(None) 无歧义 = "已处理，跳过投递"。
    let seq = if msg.content.files.is_empty() {
        match &spool {
            Some(spool) => match spool.append(&channel_type, &account_id, &msg) {
                Ok(Some(seq)) => seq,
                Ok(None) => continue,   // 去重：已处理过（wechat buf 回退重拉）
                Err(e) => { error!(...); 0 }  // 降级：仍投递，仅失去重启恢复保证
            },
            None => 0,                  // spool 关闭/未打开：原逻辑
        }
    } else { 0 };
    if msg_tx.send(InboundEnvelope { seq, channel: channel_type.clone(), account: account_id.clone(), msg }).await.is_err() { return; }
}
```

**msg_tx 消息类型扩展**：`((String, String), ChannelInboundMessage)` 元组 → `InboundEnvelope { seq, channel, account, msg }`（`seq: 0` 表示未入 spool：spool 关闭、附件消息、或 append 失败降级）。事件循环 `OrchestratorEvent::Inbound` 携带 envelope 的 seq。

### 6.2 事件循环：标记完成（`orchestrator/mod.rs` L590-610）

```rust
OrchestratorEvent::Inbound { channel_type, account_id, message, seq } => {
    inbound::dispatch(&self.ctx, (channel_type, account_id), message).await;
    if seq != 0 {
        if let Some(spool) = &self.ctx.inbound_spool {
            if let Err(e) = spool.mark_done(seq) { warn!(...); }
        }
    }
}
```

**mark_done 时机 = dispatch 返回后**：`dispatch`（`inbound.rs` L64）同步走完 7 个拦截链（含限流 drop、ask 路由、dispatch_turn 持久化 + spawn turn）后返回。此时消息已进入 session 历史（`dispatch_turn` L512 "Record + persist the inbound message"），turn 由 `tokio::spawn` 后台执行——session recovery 机制已接管后续。**限流/drop 路径同样标记 Done**（避免重放绕过限流）。

**实现确认（2026-08-13）**：与上述代码一致（`mod.rs` L596-610）。`seq == 0`（spool 关闭 / 附件消息 / append 失败降级）跳过 mark_done；mark_done 写失败仅告警，条目保持 Pending——下次重启重放，至少一次。重放路径不走事件循环，`mark_done` 由 `recover_inbound_spool` 内每个重放任务自己调（§6.4）。

### 6.3 附件消息

`ChannelInboundMessage` 可能带 `files`（QQ 图片/语音/视频，通道层已下载到本地）。`to_persisted()` 丢弃文件 body（运行时资源）。**第一版：含非空 files 的消息跳过 spool——文件 body 不持久化**。判断在**调用方** `spawn_listener`（`mod.rs` L428 `msg.content.files.is_empty()`）：files 为空才调 `append`，非空直接 `seq = 0` 正常投递。因此 `append` 保持**纯去重语义**：`Ok(None)` 无歧义 = "该 (channel, account, msg.id) 已处理过，跳过投递"（不再承担"不支持持久化"语义）。

**附件消息不入 seen**（不调 append 自然不产生去重键）：wechat buf 回退重拉时附件消息会再次投递——正确行为，因附件本就未持久化、无重放去重需求；若入 seen 反而会被去重丢消息。

### 6.4 startup recovery：重放（`recovery.rs`）

独立函数 `recover_inbound_spool(ctx)`（`recovery.rs` L354），在 `run_startup` 末尾挂接（L343，`recover_completion_queue` L337 之后——两条独立恢复线，先恢复 completion queue、再重放入站，session 注册均已就绪）：

```rust
fn recover_inbound_spool(ctx: &Arc<OrchestratorCtx>) {
    let Some(spool) = ctx.inbound_spool.clone() else { return; }; // 降级/测试：无事可做
    let pending = spool.pending();      // seq <= baseline_seq，升序（§8.3）
    if pending.is_empty() { return; }
    for entry in pending {
        let msg = entry.msg.into_runtime(); // PersistedChannelMessage → ChannelInboundMessage
        tokio::spawn(async move {           // 独立任务重放，不阻塞 recovery 主流程
            inbound::dispatch(&ctx, account, msg).await;
            if let Err(e) = spool.mark_done(seq) { warn!(...); } // dispatch 返回后 mark_done
        });
    }
}
```

- `PersistedChannelMessage::into_runtime()` 已实现（`message.rs` L349）：还原 `ChannelInboundMessage`（`sender: MessageSender::new(sender_id)`，`content: ChannelMessageContent::text(text)`，files 空）。
- 重放走完整 `dispatch` 链（含 ask 路由、限流、crash-recovery 拦截）——行为与实时消息一致。
- 重放顺序按 seq 升序；每个重放任务**自己调 `mark_done`**（重放不走事件循环，无法复用 §6.2）；dispatch 内部 panic 则条目保持 Pending，下次重启再试（至少一次）。
- 无 spool（`None`）时为空操作（`replay_noop_without_spool` 测试覆盖）。

## 7. 去重与幂等

| 场景 | 机制 |
|---|---|
| 通道重拉（wechat buf 回退） | `append` 查 seen 索引（含 Done 墓碑），同 `(channel, account, msg.id)` 跳过 |
| hot switch 竞态（旧进程未 mark_done 已退出） | 时序保证：事件循环 break 前已 dispatch 的消息同步完成 dispatch + mark_done（§8.3） |
| 重放自身 | `pending()` 只返回文件存在的 Pending 条目；重放任务完成后 mark_done，下次启动不再出现 |
| tg debounce 合成 id | `debounced_{first_ts}` 单调稳定，去重键可用 |

**seen 索引**：`HashSet<(channel, account, msg.id)>`，启动时从目录全部文件（Pending + Done）加载。文件数受 compact 控制（§4.2），内存占用可忽略。

## 8. 恢复时序

### 8.1 正常启动

```
daemon 启动 → channels 就绪 → orchestrator run()
→ spawn 的 recovery 任务（无旧进程等待）→ run_startup：
   ① 现有 incomplete-turn session 恢复（并行）
   ② spool.pending() 重放（§6.4）
→ 事件循环开始消费（新消息正常路径）
```

### 8.2 crash / SIGKILL

```
进程死亡 → systemd 重启 → 正常启动路径（§8.1）
→ spool 重放未处理消息；session incomplete-turn 恢复并存
```

### 8.3 hot switch（SIGUSR1）

```
旧进程：事件循环检测 shutdown → 同步完成当前 dispatch（含 mark_done）→ break
       → daemon fork+exec 新进程（继承 socket fd）
新进程：hot switch mode → 等旧进程退出（RECOVERY_WAIT_OLD_TIMEOUT=90s，hot_switch.rs L45）
       → run_startup：session 恢复 + spool 重放
```

- 已开始 dispatch 的消息在旧进程 break 前完成 mark_done → 新进程重放跳过；
- 队列中未 dispatch 的消息：旧进程 break 丢弃，但文件仍 Pending → 新进程重放 → 不丢；
- 新进程在等待期间已通过继承的 socket 接收新消息 → append 到同一 spool 目录 → 重放只取等待前存在的 Pending？**注意**：`pending()` 在等待结束后调用，可能包含新进程刚 append 的新消息——重放它们会**提前处理**（未按实时顺序）但**不重复**（重放任务 dispatch 后 mark_done；同时事件循环也在处理同一消息 → **双处理风险！**）。
  **竞态修复**：`pending()` 只返回 `seq <= baseline_seq`（open 时记录的当前 max seq，**含等号**）的条目。新消息 seq 更大，不被重放，走正常事件循环。实现：`InboundSpool::open` 记录 `baseline_seq`，`pending()` 过滤 `seq <= baseline_seq`（`inbound_spool.rs` L244-253，`pending_respects_baseline_watermark` 测试覆盖）。

### 8.4 双处理防护汇总

1. mark_done 在 dispatch 返回后同步执行（事件循环内）；
2. 重放水位线 `baseline_seq` 排除新进程收到的新消息；
3. 重放任务完成后 mark_done；
4. seen 去重兜底（极端竞态下同一 msg.id 二次 append 被跳过）。

## 9. 可靠性边界与权衡

1. **tg/wechat 确认时序**（第一版不改）：游标推进在通道 listen 内、早于 spawn_listener 的 spool 写入。窗口：游标已推进但 spool 未写之间崩溃 → 该批消息丢失（tg offset 落盘后不重拉；wechat buf 在内存，崩溃后 buf 回退反而重拉 → 由 spool 去重）。**ms 级窗口**，文档注明；可选加固：把 tg `persist_offset` / wechat buf 推进延后到"spool append 成功"（通道侧各改 2-3 行），作为后续优化项。
2. **spool 写失败降级**：append 失败 → 告警 + 仍投递（当前进程不丢，仅失去重启恢复保证）。磁盘满属运维故障。
3. **QQ 断线期间消息**：平台不补发，spool 无法覆盖（非目标）。
4. **tg debounce buffer**：合并前原始消息在内存，重启丢失（窗口 = debounce_ms，毫秒级）。
5. **fsync 性能**：每条消息一次 fsync（~1-5ms）。聊天消息量（<10 msg/s）无压力；tg 重连后批量拉取（一次几十条）时 spawn_listener 短暂阻塞几百 ms，可接受。后续可优化为批量 fsync（每 50ms 或 50 条）。
6. **磁盘占用**：消息文件 + 墓碑，compact 控制（§4.2）。聊天量下 7 天 < 数万文件，可忽略。

## 10. 测试清单（2026-08-13 全绿，CI run 31716733711）

**单元（inbound_spool.rs，7 个）**
1. ✅ append 写入文件 + seq 递增 + fsync 后文件可读（`append_persists_pending_entry`）
2. ✅ 同 (channel, account, msg.id) 二次 append → Ok(None)（去重，`append_dedupes_by_key`）
3. ✅ mark_done 写墓碑；pending() 不返回 Done（`mark_done_writes_tombstone`）
4. ~~含 files 的消息 append → 跳过持久化~~ 不适用——附件判断上移 spawn_listener（§6.3），append 无此分支
5. ✅ open 恢复 seq + 构建 seen 索引（含墓碑）+ pending 集合（`reopen_recovers_pending_and_seq_and_seen`，原 5+6 合并）
6. ✅ 同上（含墓碑 seen）
7. ✅ pending() 按 seq 升序 + `seq <= baseline_seq` 水位线过滤（`pending_respects_baseline_watermark`）
8. ✅ compact：清理墓碑、保留 Pending、**不重排 seq**（`compact_removes_tombstones_keeps_pending`，语义见 §4.2 偏差）
9. ✅ open 容错：损坏文件跳过但 seq 计入（`open_skips_corrupt_entries_but_counts_their_seq`）

**集成（orchestrator / recovery）**
10. ⚠️ dispatch 返回后 mark_done（事件循环 `mod.rs` L596-610）——未做独立 mock-spool 测试；与 12 的重放路径共用同一 mark_done 语义
11. ⚠️ 限流 drop 路径也 mark_done——与 10 同一代码路径（dispatch 返回后无条件 mark_done），未单独测试
12. ✅ recovery 重放 Pending → 走完整 dispatch 链 → 轮询断言 mark_done（`replay_persisted_inbound_and_marks_done`：append→drop→重开→recover→等 pending 清空）
13. ✅ 重放任务 mark_done → pending() 排除 Done → 二次启动不重放（同一测试断言 `pending().is_empty()` + `len()==0`）
14. ✅ spool 关闭（None）时 recovery 为空操作（`replay_noop_without_spool`）
15. ✅ hot switch baseline 水位线——单元级覆盖（测试 7）

**回归**
16. ✅ 三通道现有 listen/dispatch 测试全绿（CI 全量 `cargo test`）
17. ✅ clippy -D warnings（CI `build.yml` L46）

## 11. 实施记录（走 CI，x86 禁本地编译）

1. ✅ `src/storage/inbound_spool.rs`（InboundSpool + 7 单测 §10）——模块位置偏差见 §5（初稿写 `src/channels/spool.rs`）
2. ✅ `PersistedChannelMessage::into_runtime()`（`message.rs` L349）
3. ✅ `OrchestratorCtx` 注入 `inbound_spool: Option<Arc<InboundSpool>>`（`ctx.rs` L176）；目录 `.state/inbound_spool`（`mod.rs` L283，spawn_listener for 循环之前打开，fail-open）
   ⚠️ 配置项 `[channels] inbound_spool_enabled` **未实现**（偏差见 §5）——第一版无条件启用，后续优化项
4. ✅ `spawn_listener` 写入 + `InboundEnvelope` 扩展（`mod.rs` L83-88 / L393 / L421-466）
5. ✅ 事件循环 mark_done（`mod.rs` L590-610）
6. ✅ `recover_inbound_spool` 重放（`recovery.rs` L343 / L354-378，含 baseline 水位线）
7. ✅ 集成测试（recovery 2 个）+ 全量测试 + clippy——CI run 31716733711 全绿
8. ✅ PR → CI 绿；**`myclaw update` 部署待授权**（2026-08-13，部署契约：先征求授权）

## 12. 风险与权衡

| 风险 | 缓解 |
|---|---|
| hot switch 双处理 | §8.4 四重防护 |
| 重放顺序 vs 实时顺序交错 | 重放走 dispatch 全链，与实时消息语义一致；水位线避免新消息被提前 |
| spool 磁盘故障 | 降级投递 + 告警（§9.2） |
| 消息量激增 | fsync 批量优化（后续）；compact 控制文件数 |
| 行为变化面 | 无配置开关（第一版无条件启用）；spool 打开失败自动降级内存投递（fail-open），通道侧零强制改动 |
