# MyClaw 架构重构分析（事实层）

> 状态：分析进行中。本文档只陈述实测事实，不做方案决策（决策在 architecture-restructure-plan.md）。
> 基准：origin/master @ 7875e54（含 #143）。PR #146（agent.rs 拆分）OPEN 待合并——agent.rs 相关事实按 master 现状记录，#146 产物视为既成方向。

## 1. 分析方法

- **工具**：`module_deps.py`（静态解析 218 个 .rs 文件的 use 语句 + 正文 inline `crate::X::` 路径，双源；Tarjan SCC 环检测）。脚本现放 `/tmp/arch-analysis/module_deps.py`，方案批准后随 plan 入库 `scripts/`。
- **盘点**：4 个并行子代理按模块分工（agents / tools+providers / channels / 根+小模块+跨模块家族），统一 schema，产出经实测复核。
- **竞品参照**：zeroclaw（同源上游）、codex、openclaw、claude-code 的分层结构从 workspace/ 本地源码实测，不凭记忆。
- **约束**：分析期零代码改动；禁止本地 cargo（内存受限环境）。

## 2. 量化总览

| 模块 | 行数 | 文件数 | 90 天 churn（触及文件次数） |
|---|---:|---:|---:|
| agents | 38157 | 68 | **1135**（第二名 3.5 倍） |
| tools | 16784 | 33 | 302 |
| channels | 15597 | 15 | 326 |
| providers | 10321 | 42 | 230 |
| 根文件（daemon/migration/lib 等） | 4997 | 7 | daemon.rs 123 / lib.rs 14 |
| storage | 3477 | 9 | 65 |
| mcp | 2923 | 11 | 10 |
| config | 2780 | 10 | 55 |
| cli | 1826 | 15 | 69 |
| memory | 775 | 1 | 14 |
| registry | 768 | 2 | 12 |
| tui | 559 | 2 | — |
| ids | 209 | 1 | — |
| **合计** | **~99k** | **218** | 747 commits |

巨文件 Top：scheduler.rs 3849 / telegram channel.rs 3589 / qqbot channel.rs 3195 / channels client.rs 2852 / delegation_coordinator.rs 2734 / agent.rs 2529（#146 已拆 11 文件待合并）/ wechat.rs 2245 / daemon.rs 2024 / shell.rs 2012（#143 后）/ context_engine.rs 1993 / session_context.rs 1952 / channels message.rs 1950。

## 3. 全量依赖图（use 语句源）

### 3.1 边矩阵

（完整矩阵见 `/tmp/arch-analysis/deps-module-matrix.md`，随 plan 入库）关键边降序：

| 边 | use 处数 | 文件数 | 性质 |
|---|---:|---:|---|
| agents → providers | 82 | 24 | 合法（引擎用服务） |
| tools → providers | 39 | 30 | **部分违规**：含 `Tool` trait 本体（30 文件引）——Tool trait 定义在 providers/capability_tool.rs:48 |
| tools → agents | 26 | 19 | **违规**：引 agents 运行时服务（明细见 3.3） |
| agents → channels | 14 | 14 | **违规方向**（引擎反向引渠道类型：MessageReceiver/ChannelMessageContent/ChannelInboundMessage/Channel 等） |
| agents → config | 28 | 15 | 常见但量大 |
| registry → providers | 16 | 2 | |
| agents → storage | 10 | 6 | |
| channels → agents | 5 | 3 | **违规**（client.rs / telegram/channel.rs / turn_stream.rs） |
| storage → channels | 1 | 1 | **违规**（inbound_spool.rs 存渠道类型 ChannelInboundMessage/PersistedChannelMessage） |
| providers → agents | 5 | 5 | **违规**（capability_tool.rs:6 `use crate::agents::session::Session` + protocols 4 文件 inline 引用 `crate::agents::llm_stream::{REQUEST_SEND_TIMEOUT, ERROR_BODY_TIMEOUT}`） |
| config → agents | 1 | 1 | 轻微（config/mod.rs:79 引 LoopBreakerConfig） |
| agents → tools | 1 | 1 | 轻微（runtime.rs 引 SearchProviderCooldown/TaskBoards） |
| agents ↔ mcp | 1 | 1 |（mcp→agents 仅 inline） |

### 3.2 环检测：7 模块巨型 SCC

Tarjan 结果：**{agents, channels, tools, providers, config, storage, mcp} 全部处于同一强连通分量**——模块图不存在无环分层，任何"模块 A 严格低于模块 B"的断言当前都不成立。registry 单向依赖 providers，不构成环。

二元环 5 组：agents↔config、agents↔providers、agents↔channels、agents↔tools、agents↔mcp（inline）。

### 3.3 反向边符号级明细（双源验证）

**tools → agents（42 use / 19 文件）** 引用符号归类：
- `agents::session`（91 处引用，19 文件几乎全体）——工具执行上下文需要 Session
- 运行时服务具体类型：DelegationCoordinator(3 文件)、SessionManager(shell.rs)、SkillManager+Skill(3 文件)、SharedScheduler(cronjob_tool)、AgentDelegator(delegate.rs)、ChannelRegistry(friends.rs)、KnownUsersRegistry/UserRegistry/UserResolver(friends/send_message)、ToolRegistry(tool_search.rs)
- 常量：SUB_AGENT_TIMEOUT_MAX_SECS(delegate.rs)

→ tools 引的不是抽象，是 agents 的具体服务对象。结构性根因见 3.4。

**agents → channels（32 use / 14 文件）** 引用符号：MessageReceiver(17)/ChannelMessageContent(16)/ChannelInboundMessage(14)/Channel(13)——agent 引擎的消息模型直接使用渠道层类型（无中间消息契约层）。

**channels → agents（8 use / 3 文件）**：client.rs、telegram/channel.rs、turn_stream.rs（符号明细待子代理 C）。

**storage → channels（5 use / 1 文件）**：inbound_spool.rs 持久化入站消息直接用渠道类型——存储层与消息模型耦合。

**config → agents（1）**：LoopBreakerConfig。**agents → tools（1）**：runtime.rs:26 SearchProviderCooldown + TaskBoards（runtime 上下文持有任务板）。**providers → agents（1）**：见 3.4。

### 3.4 死结根因：Tool trait 宿主错位

- `pub trait Tool` 定义于 `providers/capability_tool.rs:48`（不在 tools）
- 其方法签名需要 `agents::session::Session`（capability_tool.rs:6）
- 工具实现在 tools/，实现 Tool 必须同时引 providers（trait 本体）+ agents（Session 类型）

一条 trait 错位同时制造三条违规边：providers→agents、tools→providers（70 use 中 Tool/ToolResult 占大头）、tools→agents。**修这一处的收益 > 修零散 N 处**（方案层定夺，此处仅记录事实链）。

### 3.5 daemon.rs 组合根事实

daemon.rs（2024 行）use 仅引 agents/channels，但 inline 引用覆盖 11 个模块（tools 41 处、agents 33、config 23、hot_switch 8、providers 7、channels 6、storage 4、signal 3、registry 2、update_state/migration/memory 各 1）——它是事实上的 composition root，同时直接插手工具注册/渠道启动/热切换/信号/迁移触发（剖面待子代理 D）。

## 4. 逐模块职责盘点

> 四份完整原始报告（各含全文件清单表）见附录 `docs/architecture-restructure/`；本节为提炼汇总。

### 4.1 agents（38157 行 / 68 文件，churn 1135）

**职责**：agent 会话运行时（消息→prompt→LLM 工具循环→压缩→委派→回投）+ 借居的四个异域。

**名实不符（26% 借居，≈10000 行）**：
- 用户身份域 ~3063 行 4 文件（known_users 1496/user_registry 845/user_profile 368/user_messages 354）——簇内互引仅 ~3，几乎零内聚，消费者是 tools/friends、send_message、daemon 而非 agent 循环
- 调度域 scheduling/ 4582 行——cron+webhook 调度器含独立 HTTP server（scheduler.rs:2014），与 agent 循环仅一根线（CronTrigger）相连
- slash 命令层 commands/ 2433 行——渠道接口逻辑
- misc（attachment/loop_breaker/media_e2e_test）2415 行临时堆放

**功能簇内聚度实测**（互引计数）：orchestrator 最强（387）> delegation+recovery（175）> agent-core（131）> session（131）> commands（164，入引多误报）> context/prompt（59）> workspace/skills（81）> scheduling（**12，弱**）> user-identity（**3，近零**）。

**delegation 家族结论**（专查）：4 文件是干净的 类型(delegation.rs)/接口(delegator.rs)/实现(delegation_coordinator.rs)/消费(orchestrator/delegation.rs) 四层，**无同名重复，不合并**；但应目录化聚拢 + 三个 recovery 改名消歧（recovery.rs=启动扫描/orchestrator/recovery.rs=turn 恢复/session/recovery.rs=断点检测，coordinator 内 resume_timed_out 第四义）。

**巨文件剖面**（生产/测试行数拆分 + top 函数）：
- scheduler.rs 3849（2701/1148）：145 函数无巨怪，天然边界=webhook 子系统（≈108 处）/cron ticking/job CRUD+迁移三块
- delegation_coordinator.rs 2734（1821/913）：**delegate_with_parent 589 行(L562)**；边界=同步路径/异步组/检查点持久化组/worktree 管理
- agent.rs 2529：run_inner 895 行——#146 已拆（存档）
- context_engine.rs 1993（1327/666）：无巨函数（top 210），1 个 impl 50 函数——**按算法族拆 impl 而非拆文件**
- session_context.rs 1952（1376/576）：**process_turn 657 行(L584)**；TTS 组（L1252-1375）是展示职责混入会话域证据
- known_users.rs 1496：63 函数单 impl，四代迁移代码（L294/L717/L849）可隔离

**依赖方向**（agents→providers 117 处/33 文件、→channels 90 处/19 文件、→tools 12 文件）；providers 反向引 agents 10 处（protocols×4 引 llm_stream 常量 + capability_tool 引 Session/turn_event）。

**异味精选**：agents/mod.rs 66 个 pub use 门面过重（外部 39 文件全经此，符号归属被掩盖）；media_e2e_test.rs 469 行 cfg(test) 混生产目录；orchestrator/delegation.rs 46% 是测试；delegator.rs:4 注释指向已不存在的旧路径。

### 4.2 tools（16784 行 / 33 文件）+ providers（10321 行 / 42 文件）

**tools 职责**：内置工具实现，全部 `impl crate::providers::Tool`。名实基本相符，但 **ToolRegistry 不在 tools 而在 agents/tool_registry.rs**——同一概念三处分布（trait 在 providers、registry 在 agents、实现在 tools）。

**providers 职责**：厂商接入 + capability trait 家族。名实不符一处：**域 trait Tool/ToolResult/ToolSpec 定义在 providers/capability_tool.rs** 且反向依赖 agents::session::Session。

**tools→agents 依赖五分类（实测 29 文件，比 use 源统计 19 多——inline 补齐）**：
- **A 类（8 文件）纯签名**：形参 `_session: &Session` 函数体不用——改 trait 签名即消除，成本≈0
- **B 类（~19 文件）用 Session 实体**：读 owner/身份/定位 TaskBoard
- **C 类引调度实体**：cronjob_tool.rs:8-12 引 scheduling 6 类型+5 校验函数（最重白盒耦合，1584 行近半是 agents 类型解析）
- **D 类引注册表**：ToolRegistry(tool_search)/SkillManager(3 文件)/DelegationCoordinator(3 文件)/SessionManager(shell OnceLock)
- **E 类引用户/社交域**：send_message/friends 引 KnownUsersRegistry/UserRegistry/UserMail/DeliveryVerdict/ChannelRegistry——**证明身份/社交域应从 agents 拆出为独立模块，而非 tools 改造**

**providers→agents 实测 5 文件**：
- capability_tool.rs:6 `use crate::agents::session::Session`（Tool trait 签名）——**第一杠杆点**：收窄为 `ToolContext{owner,session_id,reply_target,...}` 值对象，一次改动消掉 A/B 两类 29 文件引用的根
- protocols 4 文件（anthropic/messages.rs、google/generate_content.rs、openai/chat_completions.rs、openai/responses.rs）inline 引用 `crate::agents::llm_stream::{REQUEST_SEND_TIMEOUT, ERROR_BODY_TIMEOUT}`——搬常量到 providers 即归零，顺手消 4 份重复超时样板（~40×4 行）

**providers 合并组证据（CCP/深模块判据）**：
- 微厂商 4 文件（anthropic 53/deepseek 56/qwen 54/kimi 56=219 行，唯一调用方 provider_factory）→ 合一
- 微 capability trait 5 文件（embedding 31/stt 39/tts 43/video 45/image 52=210 行）→ 合 capability_media.rs
- shared.rs+http.rs（181+20=201 行同域"厂商共享工具"）→ 合 infra.rs
- search.rs(30)+glm_mcp.rs(323) 同目录聚合

**工具注册结构**：builtin_tools()（tools/mod.rs）只装无状态核心；**有状态工具全部由 daemon.rs build_tools()(:491-690，15+ 参数) 注册**注入。

**剖面**：shell.rs 2012（ProcEntry 持久化/reaper/收养三职责+726 行测试）；cronjob_tool.rs 1584 本质是 scheduling 的参数解析适配层；error_class.rs 1074 是 providers 最大文件（分类+格式化+恢复策略，拆分候选）。

### 4.3 channels（15597 行 / 15 文件）

**职责**：四渠道消息适配层。名实两处大偏差：
- **client.rs 2852 不是渠道**：是 WebUI 本地 HTTP+WS JSON-RPC 服务器（30 个 API 方法：sessions/memory/skills/config/daemon 管理），OnceLock 注入 SessionManager/SkillManager/UserResolver 反向依赖 agents——**channels→agents 反向边主要源头**
- message.rs 1950 一半（~860 行）是通用 markdown 感知分块算法（split_message_chunk），非消息模型

**三渠道对照（量化）**：typing keepalive 三份（~170 行）、debounce 三份三样（~220 行）、security_policy 构造三份、退避重连骨架三份——**合计 700-900 行同职责代码可上提共享层**。真差异（必须留子类）：协议连接/平台 markdown 方言/CDN 媒体编解码/被动回复限额/reaction/Telegram 流式。

**依赖分界实测**：qqbot/wechat **零** agents 依赖 ✓；channels→agents 仅 3 文件=client.rs（5 类符号）+telegram（TurnEvent）+turn_stream（TurnEvent）——**TurnEvent 类型在 agents 被 channels 引用，事件契约归属倒置**。channels→storage 零 ✓。message.rs 是全仓消费的稳定公共 API（agents 9 处+storage 4 处+tools 3 处）。

**巨函数**：client.rs handle_api_request **861 行**（17 命名空间单 match）、start() 721 行；telegram poll_loop 505 行；qqbot send_message 328 行。

### 4.4 根文件 + 小模块 + 跨模块家族

**根文件**：
- daemon.rs 2024 = 合法组合根但超载：run() 单函数 **1005 行**（L880-1884）；build_registry 306 行；build_tools 14 参数；`crate::` 限定引用 134 处横跨全部模块；热切换/sd_notify/迁移 fail-fast 文案混入装配流
- migration.rs 1619：plan-based FQID 迁移引擎，daemon 启动自动（run_auto 失败仅 warn）+ 手动 `myclaw migrate-namespace`；唯一耦合 crate::ids，干净

**小模块判定**：
- **registry 不成立为独立顶层**：768 行 26 处 providers 引用，实为 providers 能力路由 facade——应并入 providers 消掉假分层边
- **storage 部分成立**：memory/private/shared/session-backend 是纯存储域；但 completion_queue（delegation 通知）与 inbound_spool（渠道 at-least-once）语义属 orchestrator，物理放 storage 错位
- ids/memory/tui/config/mcp/cli 成立（mcp/tool_trait.rs 仅 7 行 re-export providers::Tool 佐证 Tool trait 错位）

**反向依赖 3 处**：mcp→agents::session（tool.rs:71/deferred.rs）、config→agents::loop_breaker（mod.rs:79）、registry→providers 26 处

**跨模块家族散落度**（最关键）：
- **session 家族（最大，6 顶层模块）**：agents/session/(2568)+session_context(1952)+storage/session.rs(501)+json_file.rs(1492)+tools×2+channels/client+migration 6.3+orchestrator 内部——trait(SessionBackend)在 storage、运行时状态机在 agents、目录迁移在根，schema 变更需跨 3+ 模块
- **recovery 家族（4 入口命名分裂）**：agents/recovery.rs(109 启动扫描)+orchestrator/recovery.rs(1397 turn 恢复)+session/recovery.rs(65 断点检测)+storage recover_* 重放；头注释自证"Extracted from daemon.rs"
- **delegation 家族（5 处+tools）**：agents 内 4 件套已有清晰层次，主要问题是 completion_queue 物理在 storage
- **context 家族（命名碰撞）**：context_engine.rs(1993 token 压缩) vs session_context.rs(1952 回合状态机)，两个"Context"巨型异物
- **scheduler 家族（4 处）**：agents/scheduling/ + config/scheduler.rs(99) + tools/cronjob_tool.rs(1584) + orchestrator/scheduled.rs(361)
- **identity/user 家族**：4 文件平铺 agents 根（3063 行，无目录化）+ migration.rs user 段 + tools/friends.rs + commands/link.rs

**CLI 重复组合根**：cmd_chat/cmd_exec 各 ~50 行手搓 registry+tools，与 daemon 装配漂移风险。

**启动链**：main.rs:10 → resolve_config_or_die → daemon::run 24 子步（详细行号见附录 inventory-root-small-crosscut.md §4）。

## 5. 巨文件剖面（汇总）

> 完整剖面（含 top10 函数+行数+天然边界）见各子代理报告。本节为跨模块视角汇总。

| 文件 | 行数 | 生产/测试 | 关键巨函数 | 处置方向 |
|---|---|---|---|---|
| scheduler.rs | 3849 | 2701/1148 | 无巨怪（top 279）；三块边界清晰 | webhook 子系统独立 |
| telegram/channel.rs | 3589 | 2961/628 | poll_loop 505 | 拆 TurnStream+限流 |
| qqbot/channel.rs | 3195 | 2752/443 | send_message 328 | 拆限流/防抖/重连 |
| channels/client.rs | 2852 | 2398/454 | handle_api_request **861**+start 721 | **迁出渠道层独立 webui/** |
| delegation_coordinator.rs | 2734 | 1821/913 | delegate_with_parent **589** | 拆 checkpoint 组/worktree 组 |
| agent.rs | 2529 | — | run_inner 895 | #146 已拆 11 文件 |
| wechat.rs | 2245 | 2107/138 | listen 242 | 目录化（加密/API/渠道） |
| daemon.rs | 2024 | — | run() **1005** | 拆 builder + 生命周期下沉 |
| tools/shell.rs | 2012 | 1286/726 | set_session_manager 109 | 拆 ProcEntry 持久化 |
| context_engine.rs | 1993 | 1327/666 | 无（top 210），单 impl 50 函数 | **按算法族拆 impl 不拆文件** |
| session_context.rs | 1952 | 1376/576 | process_turn **657** | 拆 TTS 组+挂起状态机组 |
| channels/message.rs | 1950 | 1618/332 | split_message_chunk 系列 | 拆 model.rs + chunking.rs |
| migration.rs | 1619 | — | — | 保持（plan-based 迁移引擎内聚） |
| tools/cronjob_tool.rs | 1584 | — | parse_webhook_channel 89 | 随 scheduling 迁移 |
| orchestrator/delegation.rs | 1560 | 840/719 | 无（闭包/长 match） | 46% 测试，可随生产同迁 |
| known_users.rs | 1496 | — | 无巨怪（top 35-59），单 impl 63 函数 | 隔离四代迁移代码 |
| storage/json_file.rs | 1492 | — | — | 保持（纯存储） |
| tools/memory_tool.rs | 1406 | — | lint_memory_content 59 | 保持（4 工具+lint/PII/审计内聚） |
| orchestrator/recovery.rs | 1397 | — | — | 重命名为 turn_recovery.rs |

## 6. 跨模块家族全景（汇总）

| 家族 | 散落度 | 物理位置数 | 聚拢收益 |
|---|---|---|---|
| session* | 6 顶层模块 | agents(×3)+storage+tools+channels+根 | **高**——schema 变更跨 3+ 模块 |
| recovery* | 4 入口命名分裂 | agents(×3)+storage | **高**——崩溃恢复语义统一 |
| delegation* | 5 处+tools | agents(×4)+storage+tools | 中高——agents 内已分层，completion_queue 物理错位 |
| context* | 3 处 | agents(×2)+config | 中——命名碰撞至少改名 |
| scheduler* | 4 处 | agents+config+tools+orchestrator | 中——已相对聚拢 |
| message/notification | 5 处 | channels+agents+tools+storage | 中——channels/message.rs 是耦合枢纽 |
| prompt | 聚拢良好 | agents+config+sys_info | 低 |
| identity/user | 4 文件平铺 | agents+migration+tools+commands | 中低——主体已在 agents，缺目录化 |

## 7. 竞品参照（分层与模块划分）

参照原则：对表不照抄；规模差异声明（竞品 18-147 crate vs MyClaw 99k 单 crate，参照的是职责切分逻辑而非规模）；每条标注"可借鉴方向"，选择理由在 plan 层定。

### 7.1 zeroclaw（同源上游，18 crate，实测 Cargo.toml 依赖方向）

分层（底→顶，实测）：
1. `zeroclaw-api`：**零内部依赖**——纯契约层（消息/类型契约的最底层）
2. 基础：config / log / infra / spawn
3. 服务：providers / memory
4. `zeroclaw-tools`：依赖 {api, providers, memory, config, infra, log}——**完全不知道 runtime 存在**
5. `zeroclaw-runtime`（agent 循环）：聚合 tools + providers + memory + commands + sop-graph
6. `zeroclaw-channels`：依赖 runtime——**渠道在最顶层驱动 runtime**（单向）

对表：
| 决策点 | zeroclaw | MyClaw 现状 |
|---|---|---|
| Tool trait 宿主 | tools 层只依赖 api 契约+providers 能力，不知道 runtime | Tool trait 在 providers 且签名需要 agents::Session（3.4 死结） |
| 渠道与引擎方向 | channels → runtime 单向（渠道驱动引擎） | agents → channels 32 use + channels → agents 8 use 双向纠缠 |
| 消息契约 | api crate 零依赖承载 | MessageReceiver/ChannelInboundMessage 等在 channels，被 agents 32 处引用 |
| 存储与消息模型 | —（memory 独立 crate） | storage/inbound_spool 直接存 channels 类型 |

### 7.2 codex（~90+ crate，实测）

- `codex-core` 聚合 60+ 内部 crate（core 是组装/编排层，类似 MyClaw agents 的聚合角色），19 个 crate 引 core（app-server/cli 等上层）
- 契约独立成 crate：codex-protocol、codex-app-server-protocol、codex-extension-api——**协议/契约先于实现独立**，是 api 零依赖层的同型做法
- crate 化动机含并行编译（MyClaw 无此诉求，不作目标）
- 参照点：契约层独立（protocol crate）对应 MyClaw 3.4 死结的另一种解法——Session/Tool 契约下沉独立层，而非搬进 tools

### 7.3 openclaw（TS，实测）

- 双层：`src/`（应用层 30+ 目录：channels/agents/context-engine/daemon/cron/chat/commands/...）+ `packages/`（可复用核心库：agent-core/llm-core/ai/gateway-protocol/markdown-core/...）
- **context-engine 是顶层目录**（MyClaw 的 context_engine.rs 在 agents 内 1993 行）——上下文引擎被视为独立于 agent 循环的一等子系统
- channel-first 起源但结构上 channels 只是 src/ 平级目录之一

### 7.4 claude-code（TS 单 bundle，反混淆源码分析）

与 MyClaw 最同构：**无编译器边界，纯靠内部分层维持结构**。其结构手段（供单 crate 方案可持续性判断）：
- 12 级渐进 harness 阶梯（loop→tool dispatch→plan→subagent→...→worktree 隔离）——能力分层而非空间分层，每级渐进叠加
- 子系统边界清晰：compaction 子系统（14+ 文件）、memdir 层级记忆、subagent AsyncLocalStorage 上下文隔离
- 模块间通信收敛：Team/SendMessage 统一请求-响应协议
- 对表：MyClaw agent/ 拆分（#146）后 11 文件≈turn 管线阶段化，方向与 harness 阶梯一致；delegation 家族 4 文件各自通信协议 vs Team/SendMessage 单协议

### 7.5 grok-build（147 crate）反面参照

过度拆分：147 crate 中大量薄 crate（单 trait/单功能），维护成本高于收益。MyClaw 重构不应以"文件数/crate 数增加"为目标函数。

### 7.6 hermes-agent / Jarvis（Python 单包）

轻结构参照：hermes 闭环学习（skill 自建自改）依赖的是子系统约定而非编译边界；Jarvis 分层继承（Agent→CodeAgent→垂直应用）。证明单包可行，但二者体量小于 MyClaw。

## 8. 事实层结论汇总

### 8.1 依赖图结论

1. **7 模块巨型 SCC**：{agents, channels, tools, providers, config, storage, mcp} 全部处于同一强连通分量——当前模块图无任何无环分层可言。registry 单向依赖 providers，不构成环
2. **Tool trait 宿主错位是三边死结共同根因**：trait 在 providers/capability_tool.rs:48，签名持有 agents::session::Session，实现在 tools——同时制造 providers→agents、tools→providers（39 use）、tools→agents（26 use）三条违规边。收窄 Session 为 ToolContext 值对象可一次消掉 A/B 两类 29 文件引用
3. **agents→channels 14 use 的消息模型耦合**是最重双向纠缠边（MessageReceiver/ChannelMessageContent/ChannelInboundMessage/Channel 等）
4. **storage→channels 1 use 为新发现违规**（inbound_spool.rs 存渠道类型）
5. **channels→agents 5 use 中 client.rs 占 3**（60%），另两处在 turn_stream.rs/telegram/channel.rs（引用 TurnEvent）——qqbot/wechat 零 agents 依赖
6. **providers→agents 实测 5 文件**：capability_tool.rs Session（use）+ protocols 4 文件 inline 引用 llm_stream 的 2 个 Duration 常量（搬常量即归零）
7. **churn 高度集中**：agents 1135 次（3.5x 第二名）——重构收益窗口与风险窗口同在 agents

### 8.2 模块结构结论

1. **agents 名实不符 26%**：借居身份域 3063 行（零内聚）+ scheduling 4582 行 + commands 2433 行
2. **registry 不成立为独立顶层**：实为 providers facade（768 行 26 处 providers 引用）
3. **storage 部分成立**：completion_queue/inbound_spool 语义属 orchestrator，物理错位
4. **client.rs 不是渠道**：是 WebUI API 后端（30 个 API 方法），应迁出渠道层
5. **message.rs 半数是通用分块算法**（~860 行），非消息模型
6. **daemon.rs 合法组合根但超载**：run() 1005 行、build_tools 14 参数、build_registry 306 行
7. **CLI 重复组合根**：cmd_chat/cmd_exec 各 ~50 行手搓 registry+tools，漂移风险

### 8.3 跨模块家族结论

1. **session 家族最散（6 顶层模块）**：trait 在 storage、运行时在 agents、迁移在根——schema 变更跨 3+ 模块
2. **recovery 家族 4 入口命名分裂**：startup/turn/breakpoint/storage 重放，无统一门面
3. **delegation 家族 agents 内已分层**：类型/接口/实现/消费四层清晰，不合并；但 completion_queue 物理在 storage
4. **context 家族命名碰撞**：context_engine(1993 token 压缩) vs session_context(1952 回合状态机)
5. **identity/user 家族主体已在 agents**：4 文件平铺根（3063 行），缺目录化

### 8.4 巨文件结论

1. **19 个 ≥1400 行文件**（含测试占比 14-46%），生产代码实际 ~2100-2960 行
2. **最大巨函数**：client.rs handle_api_request **861 行**、daemon.rs run() **1005 行**、session_context.rs process_turn **657 行**、delegation_coordinator.rs delegate_with_parent **589 行**
3. **context_engine.rs 无巨函数**（top 210），单 impl 50 函数——按算法族拆 impl 不拆文件
4. **scheduler.rs 三块边界清晰**：webhook 子系统（~108 处）/cron ticking/job CRUD+迁移

### 8.5 竞品参照结论

1. **zeroclaw（同源上游 18 crate）**：api 零依赖契约层→基础→服务→tools（不知 runtime）→runtime→channels（顶层驱动）——与 MyClaw 镜像差异
2. **codex（~90 crate）**：契约独立成 crate（protocol/app-server-protocol/extension-api）——协议先于实现
3. **openclaw（TS 双层）**：context-engine 是顶层目录（独立于 agent 循环的一等子系统）
4. **claude-code（TS 单 bundle）**：12 级渐进 harness 阶梯（能力分层非空间分层）+ Team/SendMessage 统一协议——与 MyClaw 单 crate 情境最同构
5. **grok-build（147 crate）**：过度拆分反面教材——不以文件/crate 数增加为目标函数

### 8.6 重构杠杆点排序（事实层，方案层定夺）

1. **第一杠杆：Tool trait 宿主错位**——收窄 Session 为 ToolContext 值对象，一次改动消 29 文件引用根
2. **第二杠杆：agents 借居域迁出**——身份域 3424 行（零内聚）+ scheduling 4582 行（仅一根线相连）+ commands 2433 行 → 独立顶层模块
3. **第三杠杆：client.rs 迁出渠道层**——WebUI API 后端独立 webui/ 模块
4. **第四杠杆：session 家族聚拢**——trait(SessionBackend)从 storage 迁到 agents/session 或独立 session crate
5. **第五杠杆：registry 并入 providers**——消假分层边
6. **第六杠杆：storage 编排域错位**——completion_queue/inbound_spool 迁 orchestrator
7. **第七杠杆：recovery 家族统一门面**——startup/turn/breakpoint 改名消歧 + 统一入口
8. **第八杠杆：context 家族改名**——context_engine→CompactionEngine 或类似

---

**分析完成。方案决策在 `docs/architecture-restructure-plan.md`（待用户审阅后编写）。**
