# RFC: 两级记忆架构（用户级 + Agent 级提炼）

> 状态：草案 v1
> 日期：2026-08-08
> 背景：多用户方向确认后（一个 agent 服务所有用户），记忆体系需要从"单一全局池"升级为"用户级 + Agent 级"两级结构

---

## 一、背景与动机

多用户方向已确认：MyClaw 是独立个体（张小二），一个 agent 同时服务多个用户，可并发对话。随之而来的记忆语义：

- **用户级**：对用户 B，agent 必须完整记住 B 的全部细节（per-user 完整保留）
- **Agent 级**：从用户 A 对话中学到的经验/知识（脱敏）可用于服务用户 B（跨用户共享）

用户明确的设计意图：**memory fork 是用户级**；agent 级记忆由"扫描全部用户级记忆、提取通用方法论/流程/规则"的独立聚合过程产生，**fork 阶段不做分层判断**。触发时机为**闲时**：长时间无用户消息且有新增用户记忆时。

## 二、现状分析（关键发现）

| 事实 | 位置 | 说明 |
|------|------|------|
| `memory_manage` 写入**全局目录** | `src/tools/memory_tool.rs` `action_add`/`action_replace` | target = `workspace_dir/memory/{name}.md`，`user_id` 仅用于审计 |
| `scan_merged()` 读取时合并两层 | `memory_tool.rs:113` | 全局 `memory/` + `users/{user_id}/memory/`，per-user 同名优先 |
| `users/{id}/memory/` **无写入代码** | — | `users/` 目录主用途是 `profile.toml`（`src/agents/user_profile.rs`）；现有约 30 个用户记忆文件为历史遗留 |
| fork 调用 `memory_manage`（写全局） | `src/agents/memory_fork.rs` | `ForkInput` 含 `session_owner`（routing key）字段，`ALLOWED = [memory_list, memory_view, memory_search, memory_manage]`，`MAX_ROUNDS = 3` |
| Scheduler 有 60s cron tick 循环 | `src/agents/scheduling/scheduler.rs:263` | `run()` 循环含 `cron_ticker`（60s）+ `heartbeat_ticker`；`record_user_message` 记录用户消息 |
| session 有 `last_activity` 追踪 | `src/storage/session.rs` | 每个 session 的 meta 记录最后活动时间 |

**核心偏差**：现状所有记忆（无论来自哪个用户）都进同一个全局池 `memory/`，无 per-user 隔离、无跨用户提炼。全局池中已有约 30+ 文件（含方法论类如 `analyze-before-code-change.md`、`arm_caddy_openlist_n8n_proxy_fix.md`）——这些事实上是"Agent 级"内容，可作为初始资产。

## 三、目标与非目标

### 目标

1. 两级存储：用户级（per-user 隔离、私事保真）+ Agent 级（脱敏、跨用户共享）
2. fork 只写用户级，不做分层判断
3. 闲时自动提炼：有新增用户记忆 + 系统空闲 → 聚合提炼为 Agent 级
4. 读取路径不变：`scan_merged` 继续合并两层注入

### 非目标

- 不做每用户私有 agent 实例（已否决）
- 不做 fork 阶段的分层分类（用户明确否决——分层是提炼阶段的职责）
- 不引入新记忆工具（复用 `memory_manage` + `scope`）

## 四、总体架构

```
┌─ fork（对话/compaction 后）───────────────┐
│  提取 → scope=user（强制）                 │
│  → users/{user_id}/memory/*.md            │  ← 用户私事，per-user 隔离
└───────────────────────────────────────────┘
          │ mtime 变更（新增/修改）
          ▼
┌─ 提炼（闲时 cron 检查触发）────────────────┐
│  扫描全部 users/*/memory/*.md             │
│  LLM 提取通用方法论/流程/规则              │
│  scope=agent → memory/*.md（脱敏）         │  ← 跨用户共享
└───────────────────────────────────────────┘
          │
          ▼
┌─ 读取（scan_merged，现状不变）─────────────┐
│  全局 memory/ + users/{user_id}/memory/    │  ← 两层合并注入
└───────────────────────────────────────────┘
```

## 五、详细设计

### 5.1 存储布局

```
workspace/
├── memory/                        ← Agent 级（提炼产物 + 存量初始资产）
│   ├── analyze-before-code-change.md
│   └── ...
├── users/
│   └── {user_id}/                 ← 用户级（user_id = routing_key）
│       ├── profile.toml           （现状已有）
│       └── memory/
│           └── *.md               （fork 写入目标，现状无写入代码）
└── .state/
    └── distill.json               ← 提炼状态（last_distill_ts 等）
```

### 5.2 `memory_manage` 增加 `scope` 参数

- 参数：`scope: "user" | "agent"`，**默认 `"user"`**
- `scope=user` → 写 `users/{user_id}/memory/{name}.md`（无 user_id 上下文时拒绝）
- `scope=agent` → 写 `memory/{name}.md`（现状路径）
- 校验：`scope=user` 必须解析到 user_id（session owner）；`scope=agent` 必须通过脱敏守卫（见 5.5）
- `replace`/`remove` 同样按 scope 定位文件；`list`/`view`/`search` 的 `scan_merged` 语义不变
- audit log 增加 `scope` 字段

**默认值变更说明**：现状默认行为（写全局）改为默认 `"user"`。影响面：现有调用方仅 fork 与主动 `memory_manage`；`scan_merged` 合并读取保证功能不丢。存量全局文件保留为 Agent 级初始资产（见 5.7 迁移）。

### 5.3 fork 强制 `scope=user`

- `memory_fork.rs` 的提取 prompt 明确指示：所有 `memory_manage` 调用必须 `scope="user"`
- 代码层强制：fork 的 memory_manage 执行路径若检测到 `scope != "user"` 或缺失，自动修正为 `"user"`（防止 prompt 被绕过）
- 同步调整 fork prompt 中关于"跨用户通用性"的措辞——fork 阶段**不评判**通用性，只提取该用户对话中的持久事实

### 5.4 提炼任务 `memory_distill`

新模块 `src/agents/memory_distill.rs`，复用 fork 框架（LLM 工具调用循环）：

- **输入**：全部 `users/*/memory/*.md`（frontmatter + body），按用户分组标记来源
- **输出**：`scope=agent` 写入 `memory/`（方法论 / 流程 / 规则 / 可复用经验）
- **prompt 要点**：
  - 先 `memory_search` 检查 Agent 级已有条目，**同名/同主题用 `replace` 合并**，避免重复堆积
  - 只在"跨用户可复用"（方法论、流程、规则、通用经验）时写入；单用户私事、一次性事件不写
  - **必须脱敏**：输出不含 user_id / routing key / 姓名 / 联系方式 / 组织名等标识
  - 工具白名单与 fork 相同：`memory_list / memory_view / memory_search / memory_manage`，`MAX_ROUNDS` 从 3 提高到 5（输入是多文件）
- **幂等**：提炼产物用 `replace` 而非 `add`；`last_distill_ts` 推进后，该批变更不重复处理
- **token 上限**：输入超限（如 >60K chars）时按文件分批，每批独立提炼；单文件过大截断 body
- **模型**：复用 fork 的 `model_id`（同 provider，前缀缓存友好）；推理模型更优（提炼需归纳）

### 5.5 闲时触发机制

**状态**：`workspace/.state/distill.json`：

```json
{
  "last_distill_ts": "2026-08-08T10:00:00Z",
  "last_attempt_ts": "2026-08-08T10:00:00Z",
  "consecutive_failures": 0,
  "in_progress": false
}
```

**调度**：Scheduler 增加内部 distill tick（复用 `cron_ticker` 60s 循环，每 15 分钟检查一次，不注册为用户可见 cron job）。

**触发条件**（全部满足才执行）：

1. **闲时**：所有 session 的 `last_activity` 距今 > `idle_threshold`（默认 30 分钟）。检测方式：扫描 `workspace/sessions/*/meta.json` 取最大 `last_activity`，或由 orchestrator 维护全局 `last_inbound_ts`（二选一，见开放问题 Q1）
2. **有新增**：存在 `users/*/memory/*.md` 的 `mtime > last_distill_ts`
3. **未在进行**：`in_progress = false`（单飞锁，防止与手动触发重叠）

**失败处理**：
- 提炼失败（LLM 错误/超时）：`consecutive_failures + 1`，**不推进** `last_distill_ts`，下次 tick 重试；`consecutive_failures >= 3` 时暂停 2 小时（退避），成功后清零
- 成功：推进 `last_distill_ts = now`，清零失败计数

### 5.6 脱敏守卫（Agent 级写入）

`scope=agent` 写入前对 content 做静态检查，命中任一特征则拒绝写入并返回错误（提示先脱敏）：

- routing key 模式：`^[a-z]+:[a-z0-9]+:[A-Za-z0-9_:-]+$`（如 `telegram:myclaw:6270938644`）
- 长数字串（≥8 位连续数字，user_id 特征）
- 邮箱 / 手机号 / URL 中带用户标识（按常见正则）

说明：静态守卫是底线（防意外），主要防线是提炼 prompt 的脱敏指令 + 提炼产物审查。守卫只拦截"明显 PII"，不追求完备（如姓名无法静态识别——提炼 prompt 负责）。

### 5.7 迁移与兼容

| 项 | 处理 |
|----|------|
| 存量 `memory/*.md`（全局 30+ 文件） | 视为 Agent 级初始资产，原地保留 |
| 存量 `users/*/memory/*.md`（约 30 个） | 视为用户级，原地保留 |
| 现有调用方（fork / 主动 memory_manage） | fork 改走 user 级；主动调用默认 user（行为变更见 5.2） |
| `scan_merged` 读取 | 不变，两层合并已正确 |
| audit log | 增加 `scope` 字段，向后兼容 |

## 六、边界情况

| 场景 | 处理 |
|------|------|
| 提炼期间 fork 并发写用户记忆 | 目录不同无冲突；提炼读扫描时快照，变更留待下批 |
| 提炼期间有用户发消息 | 提炼 LLM 调用与主对话并发（闲时触发已大幅降低概率）；如并发，提炼继续（只读用户记忆 + 写全局） |
| 用户删除 | 用户级记忆随目录清理；Agent 级提炼物不受影响（已脱敏） |
| 提炼产物与存量全局条目冲突 | prompt 要求 `replace` 合并；代码层 `add` 同名冲突仍返回错误，由 LLM 改走 replace |
| 单用户部署（现状） | fork 写 `users/{id}/memory/`，`scan_merged` 合并读取，行为等价；主动写的方法论显式 `scope=agent` |

## 七、实施计划

### P0（本轮）

1. `memory_manage` 增加 `scope` 参数 + 用户级写入路径 + 脱敏守卫（memory_tool.rs）
2. fork 强制 `scope=user`（memory_fork.rs prompt + 执行路径校验）
3. `memory_distill.rs` 提炼任务（复用 fork 框架）
4. 闲时触发：`.state/distill.json` + Scheduler distill tick
5. 单测 + 手动演练验证（见下）

### P1（后续）

- ~~用户画像保真：fork 的 durability gate 目前会丢细节，提炼/画像需保留 per-user 完整细节~~ **已完成（2026-08-08，commit b974371）**：`compact_fork_messages` 保留全部 user 消息（细节载体）+ 全部 system，仅裁剪 assistant 尾部（上限 40 条 user）；fork prompt 新增「User profile fidelity」（画像类事实豁免 durability gate、完整细节保真）+「Replace without data loss」（replace 前 `memory_view` 读旧全文，保留仍有效旧细节）。CI 663 tests 全绿。

### 验证方案

1. 单测：scope 路由（user/agent 写入位置）、脱敏守卫拦截、同名 replace 幂等
2. 手动演练：造 2-3 条用户级记忆（含一条私事、一条通用方法论）→ 触发 distill → 验证 Agent 级产物只含脱敏方法论、无 PII
3. 回归：现有 fork 流程、scan_merged 注入、`myclaw doctor`

## 八、已定决策（原开放问题）

- **Q1 已定**：闲时检测用 orchestrator 维护的全局 `last_inbound_ts`（消息入口加一行更新，轻量实时），不做 sessions 扫描。
- **Q2 已定**：参数配置化到 `myclaw.toml` 的 `[memory]` 段：`distill_idle_secs`（默认 1800）、`distill_interval_secs`（默认 900）。
- **Q3 已定**：提炼用全量输入 + `replace` 去重起步，记忆量大后再做增量。

## 九、实施记录

- 2026-08-08：RFC v1 定稿，P0 开工。
- 2026-08-08：P0 完成（P0-1~P0-5，commit 3f08895/29fb127/068741b，CI 660 tests，手动演练通过）。
- 2026-08-08：P1 完成（commit b974371，CI 663 tests）：fork 保真——compact 保留全部 user 消息、prompt 画像保真 + replace view-then-merge。
