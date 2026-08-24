# 子代理 D 报告：src 根文件 + 小模块 + 跨模块家族全景

仓库：/home/ubuntu/.myclaw/workspace/MyClaw（只读盘点，全部数据实测）

---

## 1. 根文件职责清单

| 文件 | 行数 | 一句话职责 |
|---|---|---|
| daemon.rs | 2024 | 组合根（Composition Root）：加载配置→装配全部组件→run() 到关机；兼热切换收尾 |
| migration.rs | 1619 | RFC §6 plan-based 数据迁移引擎（FQID 化 5 项格式 + 2 项清理） |
| main.rs | ~148 | bin 入口：clap 分发 15 个子命令到 cli/* |
| lib.rs | ~62 | crate 根：SHUTDOWN_FLAG/TERMINATING_FLAG 全局原子 + 模块声明 + 大量 re-export |
| hot_switch.rs | 363 | fork+execve 热切换：env 约定、SIGUSR2 就绪、do_hot_switch、回滚 |
| str_utils.rs | 477 | 字符串工具：UTF-8 截断、防 spoofing、YAML frontmatter 解析 |
| update_state.rs | ~200 | `~/.myclaw/update-state.json` 持久状态机（staged/switching/completed/failed）+ file_sha256 |
| signal.rs | ~60 | PID 文件路径 + find_daemon_pid + send_sigterm/sigusr1（CLI 与 daemon 共用） |
| sys_info.rs | ~50 | /etc/os-release 读取 OS/Shell 信息，仅服务于 system prompt Runtime 段 |

### daemon.rs（2024 行）剖面

**顶部 use（L12-25）**：`crate::agents::{AgentDelegator + 13 个类型}`、`crate::channels::Channel`、anyhow/std/tokio。函数内 use 另有 `crate::providers::{8 个 Build*Request + CredentialPool/Factory}`（L187-193）。全文 `crate::` 限定引用 **134 处**，横跨 agents/tools/channels/config/storage/memory/mcp/registry/providers/hot_switch/signal/update_state/migration —— 它是唯一合法的"全知"点，但体量已超载。

**函数地图**：
- L43/L59 `load_config`/`load_config_from` — 配置发现
- L73 `bind_reusable` — SO_REUSEPORT socket
- L105 `init_tracing`；L133 `print_banner`
- L186-491 `build_registry` — providers/registry 装配（**306 行**）
- L493-654 `build_tools` — 工具注册表装配（**162 行**，14 个参数）
- L656 `build_skill_manager`；L676 `build_sub_agents`；L690/L723 missing/warn agent tool refs
- L752 `find_legacy_session_dirs` — P1 布局巡检
- L773 `build_session_backend`；L798 `build_channel_accounts`（渠道启动）；L860 `build_prompt_config`
- L880-1884 `run()` — **1005 行单体主函数**
- L1886 `wait_for_signal`；L2004 `reset_telegram_offset`
- L1909-2024 tests + hot-switch 尾注

**run() 直接插手的主题（行号）**：全局安全配置/media data dir/shell PATH init（L882-894）；cwd 切换（L896）；AGENT.md 校验（L906）；热切换 socket 接管 + Telegram offset 重置（L923-968）；PID 文件（L970）；memory 目录（L979）；存储布局 fail-fast ×2（L989-1064）；`migration::run_auto`（L1065）；registry/MCP/skills（L1069-1082）；时区解析+调度器双建（markdown 迁移用 dummy Scheduler L1088-1129）；memory distill 配置（L1131）；UserResolver/AskRouter/KnownUsers/UserRegistry + 两次 legacy 迁移（L1141-1170）；session backend/manager（L1172-1196）；build_tools + send_message 接线（L1198-1216）；子代理注册表 + WebSearch/媒体三工具注册（L1218-1252）；WorkspaceWatcher（L1263）；DelegationCoordinator + 单/多代理分叉（L1278-1360）；启动扫描 all_sessions + 恢复/checkpoint 关联（L1363-1422）；ClientChannel + fd 继承（L1426-1481）；prompt/MCP instructions/AgentRuntime 大块装配（L1484-1600）；OrchestratorParts 组装 + Orchestrator::new（L1602-1625）；webhook server spawn（L1650 附近）；scheduler loop spawn（L1672）；SIGUSR1/INT/TERM/USR2 处理 + sd_notify + 热切换收尾 drain/exit（L1684-1860）。

**结论：是上帝文件，但属于"合法的组合根上帝"** —— 问题不在它知道一切（这是组合根本职），而在 (a) run() 1005 行不可测；(b) build_tools 14 参数、build_registry 306 行，应拆 builder；(c) 热切换信号/sd_notify/Telegram offset 等生命周期细节混在装配逻辑里（可下沉 hot_switch/lifecycle 模块）。文件头自评 "Composition Root"，职责声明与实际一致。

### migration.rs（1619 行）剖面

迁移对象：6.1 users.json 孤儿 root→FQID；6.2 user_resolver.json 双前缀/root 段规范化；6.3 sessions/ 8-hex 目录→`<ns>/s/<uuidv7>`（含 meta.json/active.json/_cron_ 键重写）；6.4 tasks.json task_{n}→FQID；6.5 cron/jobs.json→`<ns>/job/<uuidv7>`（run_logs 改名连带）；6.6 users/ 遗留 rk 目录归档；B 清理 meta.json 遗留 task_id。
运行时机：**daemon 启动自动**（daemon.rs:1065 `run_auto`，失败仅 warn 不阻断）；**手动** `myclaw migrate-namespace`（cli/cmd_migrate.rs:37 `build_plan` 干跑→确认→apply）。结构：plan-based（L44 Backup / L89 MigrationPlan / L102 apply / L203 MigrationReport / L221 build_plan / L243 run_auto + 9 个 migrate_* 分项）。依赖仅 `crate::ids`（唯一外部耦合，干净）。被 daemon 与 cli 引用。

---

## 2. 小模块逐个判断

| 模块 | 文件/行数 | pub 符号 | 被谁依赖（crate:: 视角，bin 里是 myclaw::） | 依赖哪些模块 | 独立顶层成立？ |
|---|---|---|---|---|---|
| storage | 9 文件 3477 行 | 38 | agents（15 文件：orchestrator×5、session×2、recovery×2、delegation_coordinator 等）、daemon、tools×2 | channels（PersistedChannelMessage/ChannelInboundMessage，json_file/session/inbound_spool）、providers（ChatMessage，session/json_file）、ids | **部分成立**。memory/private/shared/session-backend 是纯存储域；但 completion_queue（delegation 通知）与 inbound_spool（渠道消息 at-least-once）是**编排域持久化**，物理放 storage、语义属 orchestrator——见 §3 |
| mcp | 11 文件 2923 行 | 43 | 仅 agents/mcp_manager.rs（bin 侧经 lib re-export） | providers（ToolSource/tool/capability_tool，tool.rs+tool_trait.rs）、agents::session（tool.rs+deferred.rs，sub-session 场景）、config 无直接依赖 | **成立**，但 mcp/tool_trait.rs 仅 7 行 re-export providers 的 Tool trait——trait 本体在 providers 是明显错位信号；deferred.rs（561 行惰性激活）依赖 agents::session 造成 mcp→agents 反向依赖 |
| config | 10 文件 2780 行 | 75 | 45 个文件全仓库横跨（agents×21、channels×5、providers×2、registry、tools×10、migration、daemon） | providers（Capability/AuthStyle/RotationStrategy，provider.rs/mod.rs/routing.rs）、agents::loop_breaker（mod.rs:79 LoopBreakerConfig） | **成立**（配置面就该顶层）。异味：config→agents::loop_breaker 单点反向依赖，LoopBreakerConfig 应下沉 providers 或独立 |
| cli | 15 文件 1826 行 | 35 | 无（bin-only，main.rs `mod cli`） | daemon::init_tracing、registry、agents、tools、config、signal、update_state、migration（cmd_chat/cmd_exec 自建 mini runtime） | 成立（bin 侧）。cmd_chat/cmd_exec 与 daemon::build_tools 存在装配逻辑重复（各 ~60 行手搓 registry+tools） |
| tui | 2 文件 559 行 | 2 | 无（feature = "tui"） | 仅外部 crate（ratatui/tokio-tungstenite），零内部依赖 | 成立，最干净的模块 |
| registry | 2 文件 768 行 | 7 | daemon、agents/media_e2e_test、cli（cmd_chat:20 cmd_exec:52 经 myclaw::registry） | providers ×13 符号（Capability/SharedApiKey/各 capability trait/FallbackChatProvider/MediaPolicy…）、config::routing | **不成立为独立顶层**。registry = providers 的能力路由层，768 行里 26 处 providers 引用；实质是 providers 的 facade，建议并入 providers（或反之 providers::registry），消掉一条假分层边 |
| memory | 1 文件 775 行 | 14 | agents×4、channels/client、daemon、tools/memory_tool | 无内部依赖（纯文件 wiki） | 成立（但单文件 775 行可目录化：索引/读写/链接解析） |
| ids | 1 文件 209 行 | 13 | agents×5、migration、storage×2、tools×5 | 无内部依赖 | 成立，教科书式底层（被 13 文件引用零反向） |

str_utils（477 行 13 pub fn）：被 agents×7 + tools×7 引用；YAML frontmatter 解析部分实际只服务 skill/agent loader——可随 skill 域走，纯字符串部分可留。sys_info（50 行）：仅 agents/prompt.rs 一个消费者。update_state/signal：daemon+cli 双端共用的小工具，成立。

---

## 3. 跨模块家族全景（最关键）

### session* 家族 —— **最大散落族，8 个物理位置**
| 文件 | 行数 | 归属 |
|---|---|---|
| agents/session_context.rs | 1952 | agents 根 |
| agents/session/{manager,types,backend,session_override,recovery}.rs + mod | 2568 | agents/session/ |
| agents/session/recovery.rs（断点检测） | 65 | agents/session/ |
| storage/session.rs（SessionBackend trait） | 501 | storage |
| storage/json_file.rs（JsonFileBackend） | 1492 | storage |
| tools/session_query.rs、sessions_yield.rs | ~600 | tools |
| channels/client.rs 内 session 管理 API | 2852（部分） | channels |
| agents/orchestrator/{key,turn,ctx}.rs 内 session 表 | — | orchestrator |
| migration.rs 6.3 会话目录迁移 | — | 根 |
分布：agents×3 层 + storage + tools + channels + 根 = **6 个顶层模块各持有碎片**。互引证据：session/manager.rs:12 `use crate::agents::session_context::SessionContext`；storage/session.rs:8 `pub use crate::providers::ChatMessage`；orchestrator/delegation.rs:22 `use crate::agents::session_context::TerminalRecord`；mcp/tool.rs:71 `crate::agents::session::Session`。**聚拢收益高**：trait（SessionBackend）在 storage、运行时状态机（SessionContext 1952 行）在 agents 根、目录迁移在根文件，三处任何 schema 变更都要跨 3 个顶层模块改。

### recovery* 家族 —— 4 处
agents/recovery.rs（109，UnfinishedSubAgent 扫描，明确注释"Extracted from daemon.rs"）；agents/orchestrator/recovery.rs（1397，turn 恢复，**全仓库最大恢复逻辑**）；agents/session/recovery.rs（65，断点检测）；storage/completion_queue.rs + inbound_spool.rs 内 `recover_*` 重放。互引：daemon.rs:1367 调 agents::recovery::scan_unfinished_subagents；orchestrator/recovery.rs:19 引 session_context::TerminalRecord。**收益高**：崩溃恢复语义被拆到 3 个模块 + 2 个 storage 重放点，命名也不统一（recovery/recover_/resume）。

### delegation* 家族 —— 5 处 + tools
agents/delegator.rs（76，AgentDelegator trait）；agents/delegation.rs（392，DelegationEvent 类型）；agents/delegation_coordinator.rs（2734，实现）；agents/orchestrator/delegation.rs（1560，事件循环侧）；storage/completion_queue.rs（329，完成通知队列）；tools/{delegate,agent_kill,agent_list,agent_resume,sessions_yield}.rs。互引密集：delegator.rs 注释直指 coordinator；orchestrator/delegation.rs:480 `crate::storage::CompletionNoticeEntry`。**收益中高**：trait/事件/实现/编排/持久化五分天下，但 agents 内四件已有清晰层次；主要问题是 completion_queue 物理在 storage。

### context* 家族 —— 3 处
agents/context_engine.rs（1993，ContextEngine 压缩 facade）；agents/session_context.rs（1952，SessionContext 每回合管线）；config 中 context_engine 配置段。两个 2000 行级"Context"命名冲突但语义不同（token 预算 vs 回合状态），互引：runtime.rs 同时持有两者。**收益中**：至少应改名区分（ContextEngine→CompactionEngine？）。

### scheduler/timer/cron 主题 —— 4 处
agents/scheduling/{scheduler 3849, cron_loader 170, cron_types 291, work_unit 268}；config/scheduler.rs（99）；tools/cronjob_tool.rs（1584，含 Scheduler 测试复用 :1269/:1439）；agents/orchestrator/scheduled.rs（361）。互引：daemon.rs:1093/1118 两次 Scheduler::new（一次 dummy 做 markdown 迁移）；cronjob_tool 直接 new 真实 Scheduler。**收益中**：scheduler.rs 3849 行是全仓最大文件（另一子代理范围），cron 配置/工具/调度核心/触发编排已基本聚在 agents+scheduling，仅 config 与 tools 两端点。

### message/notification 主题 —— 散 5 处
channels/message.rs（1950，渠道共享类型）；agents/user_messages.rs（354，bot 状态文案）；agents/ask_router.rs（198，ask_user 挂起表）；tools/send_message.rs（968）；storage/inbound_spool.rs（556）。notification 语义散在 orchestrator/{inbound,recovery,delegation}+channels×4。**收益中**：channels/message.rs 1950 行混合消息类型+流式+持久化类型，是 channels 与 storage 的耦合枢纽（storage/inbound_spool.rs:46）。

### prompt 主题 —— 聚拢良好
agents/prompt.rs（441，SystemPromptBuilder）+ config/agent.rs（168）+ sys_info.rs（50，唯一消费者）+ workspace/{skill_loader 777, agent_loader 361}（frontmatter→prompt）。SystemPrompt 符号仅在 agents 域 6 文件出现。**收益低**，仅 sys_info 可并入 prompt.rs。

### identity/user 主题 —— 4 文件全在 agents + migration
agents/{known_users 1496, user_registry 845, user_profile 368, user_messages 354} + 根 migration.rs（users.json 迁移）+ tools/friends.rs + agents/commands/link.rs。UserResolver 定义在 user_profile.rs:92 re-export。**收益中低**：主体已聚在 agents，但 4 文件平铺 agents 根（对比 session/ 有目录），可目录化 agents/user/；migration.rs 的 user 迁移段与之强耦合（ids 层已解耦格式）。

---

## 4. main.rs 启动链

1. main.rs:10 clap 解析 → 分发子命令；`run`/None 分支：resolve_config_or_die（CLI flag > MYCLAW_CONFIG env > 默认三路径，main.rs:141）
2. cli::init_tracing（cli/mod.rs:221 → daemon::init_tracing L105）
3. myclaw::daemon::run(cfg)（daemon.rs:880）：
   4. init_safety_config / providers::media::init_data_dir / tools::shell_env::init / set_current_dir(workspace)（L882-905）
   5. main AGENT.md 存在校验，缺失 bail（L906-921）
   6. 热切换检测：继承 listen/client fd、reset_telegram_offset（L923-968）
   7. 写 PID 文件；ensure memory dir（L970-983）
   8. 布局 fail-fast ×2（旧 workspace/sessions 布局 / 非裸 uuid 目录 → 拒启，指向 migrate-layout.py）（L989-1064）
   9. migration::run_auto（RFC §6，失败仅 warn）（L1065）
   10. build_registry（providers 装配 L186）；McpManager connect；build_skill_manager（L1069-1082）
   11. 时区解析；markdown→jobs 迁移（dummy Scheduler）；shared Scheduler::new + distill config（L1084-1129）
   12. UserResolver / AskRouter / KnownUsersRegistry(+migrate_legacy) / UserRegistry(+migrate_legacy_to_root)（L1141-1170）
   13. build_session_backend → shell_notice channel → build_tools（14 参数）（L1172-1198）
   14. build_sub_agents → AgentRegistry → WebSearch/ViewImage/HearAudio/ViewVideo 注册（L1217-1252）
   15. WorkspaceWatcher::spawn_managed；SessionManager::new；shell_tool 接线（L1263-1290）
   16. 单/多代理分叉：DelegationCoordinator + agent_delegate/agent_kill 等父工具 or tool_search（L1292-1360）
   17. 启动扫描 all_sessions → scan_unfinished_subagents → delegation checkpoints 关联（L1363-1422）
   18. build_channel_accounts + ClientChannel（fd 继承/SO_REUSEPORT）（L1426-1481 + L798）
   19. build_prompt_config；AgentRuntime 装配（ResourceProvider→ContextEngine→ToolExecutor→LoopBreaker→AgentRuntime::new + with_* ×8）（L1484-1600）
   20. OrchestratorParts 组装 → Orchestrator::new → friend_ctx/SessionManager 回填 channel registry（L1602-1630）
   21. webhook server spawn（run_webhook_server）；scheduler.run() spawn（L1650-1680）
   22. 信号注册（SIGUSR1 置 SHUTDOWN_FLAG / INT,TERM / USR2 新进程就绪）；sd_notify READY(+MAINPID)（L1684-1770）
   23. orchestrator.run(shutdown_rx, unfinished, all) 阻塞主循环（L1790）
   24. SHUTDOWN_FLAG 置位 → do_hot_switch fork+execv → 并发 drain turns(30s) → checkpoint_and_cancel_all → 等 SIGUSR2 → exit(0)（L1800-1860）
   25. 正常路径：清理 PID 文件返回（L1862-1870）

---

## 5. 依赖方向明细（本批文件/模块 → agents/providers/tools/channels）

**→ crate::agents**（3 处，均为反向/跨界异味）：
- mcp/tool.rs:71 等（`crate::agents::session::Session`，sub-session 上下文注入）
- mcp/deferred.rs（`crate::agents::session` 惰性激活回填）
- config/mod.rs:79（`agents::loop_breaker::LoopBreakerConfig`）

**→ crate::providers**（合法的 infra 邻接，但暴露分层含糊）：
- storage/session.rs:8（re-export ChatMessage）、:80 sha256_hex
- storage/json_file.rs:1317（ChatMessage/ContentPart）
- mcp/tool.rs + tool_trait.rs（ToolSource/Tool/ToolSpec —— Tool trait 本体竟在 providers）
- config/{mod:128,routing,provider}.rs（Capability/AuthStyle/RotationStrategy）
- registry/mod.rs ×26 + routing.rs（全能力面）
- daemon.rs L187-193（Build*Request×8/Factory/CredentialPool）

**→ crate::tools**：daemon.rs（build_tools 全程、shell_env/shell ShellCompletion）；其余小模块零依赖（好）。

**→ crate::channels**：
- storage/inbound_spool.rs:46,347（ChannelInboundMessage/PersistedChannelMessage）
- storage/json_file.rs:101,1105,1117（PersistedChannelMessage 持久化）
- storage/session.rs:314,323（SessionBackend trait 方法签名）
- daemon.rs L25（Channel trait）

bin 侧补充：cli/cmd_chat.rs:17-55 与 cmd_exec.rs:50-63 直接 new Registry/ToolRegistry/SkillManager —— CLI 重复组合根逻辑。

---

## 6. 异味清单

1. **daemon.rs:880-1884** — run() 单函数 1005 行，装配+迁移+信号+热切换+恢复五主题串联，不可单元测试。
2. **daemon.rs:493-654** — build_tools 14 参数（mcp/skills/scheduler/config/resolver/ask_router/known_users/user_registry/namespace/backend/shell_notice_tx），参数对象化信号明确。
3. **daemon.rs:186-491** — build_registry 306 行在组合根内手写 providers 装配，应属 providers/registry builder。
4. **daemon.rs:1084-1106** — 为 markdown→jobs 迁移 new 一个 dummy Scheduler（含 dummy_tx），生命周期工具当迁移器用。
5. **mcp/tool_trait.rs（7 行）** — 仅 re-export providers 的 Tool trait；Tool trait 放 providers 是结构性错位（工具协议核心类型应在 tools 或独立契约层）。
6. **mcp/{tool,deferred}.rs → agents::session** — infra 层 mcp 反向依赖应用层 agents（sub-session 注入），方向倒置。
7. **config/mod.rs:79 → agents::loop_breaker** — 配置层反向依赖应用层，LoopBreakerConfig 位置错。
8. **registry/ 独立顶层** — 768 行 26 处 providers 引用，实为 providers facade 的假分层；daemon/media_e2e/cli 三方各自 from_config。
9. **storage/session.rs:8** — `pub use crate::providers::ChatMessage`：存储层 re-export 邻层类型，掩盖真实依赖。
10. **completion_queue/inbound_spool 在 storage/** — 语义是 orchestrator 的 at-least-once 机制（文件头自述 RFC delegation-notice-queue/inbound-spool），物理归属错位；且路径写死 `{workspace_dir}/.state/` 与 P1 数据面布局（{base_dir}）不一致线索（inbound_spool.rs 文档 vs config/mod.rs:469 数据根注释）。
11. **agents/session_context.rs(1952) 与 agents/context_engine.rs(1993) 同名异物** — 两个 "Context" 巨型文件语义不同（回合状态机 vs token 压缩），命名碰撞造成导航成本；且与 agents/session/ 目录三足鼎立，session 家族 6 顶层模块散落（§3）。
12. **recovery 命名分裂** — agents/recovery.rs(109) / orchestrator/recovery.rs(1397) / session/recovery.rs(65) / storage recover_*，四个恢复入口无统一门面；agents/recovery.rs 头注释自证"Extracted from daemon.rs"是欠拆分痕迹。
13. **cli/cmd_chat.rs:17-55 与 cmd_exec.rs:50-63** — 与 daemon::build_tools/build_registry 重复的 mini 组合根（约 2×50 行），组件漂移风险（chat/exec 的工具集与 daemon 不同步）。
14. **str_utils.rs(477) 双职责** — UTF-8 截断/防 spoofing（13 处消费）与 YAML frontmatter 解析（仅 skill/agent loader 消费）混在一文件。
15. **agents/{known_users,user_registry,user_profile,user_messages}.rs 平铺 agents 根**（3063 行）+ UserResolver 藏在 user_profile.rs:92 re-export —— user 域无目录化，与 session/ 有目录形成不一致。
16. **channels/message.rs 1950 行** — 消息类型+流式管线+持久化类型三合一，是 storage↔channels 耦合的根源（json_file/inbound_spool 都 import 它）。
17. **lib.rs 巨量 re-export（66 处 pub use/mod）+ 全局 SHUTDOWN_FLAG** — 原子标志在 lib 根，daemon.rs:535 等多处 crate::SHUTDOWN_FLAG 散读，关机状态无单一 owner。
18. **daemon.rs:989-1064 两段 fail-fast eprintln 长文案** — 中文运维文案硬编码在组合根，属 CLI/迁移层职责。
