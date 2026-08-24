# src/agents/ 模块盘点（master 现状，38157 行 / 68 文件）

> 盘点日期 2026-08-24。行数 `wc -l` 实测；pub 符号 `grep -c '^\s*pub '`；外部引用 `grep 'crate::agents'` 于 src/agents 之外。
> **注意**：`agent.rs`（2529 行）在 PR #146 分支已拆为 `agent/` 目录（待合并），本文按 master 单文件盘点，剖面中单独标注。

## 1. 模块一句话职责 + 名实相符

**职责**：agent 会话运行时——接收渠道消息 → 构造 prompt → 驱动 LLM 工具循环 → 上下文压缩 → 子代理委派 → 回投渠道；外加用户身份注册表与 slash 命令层。mod.rs:1 自述 "Agent loop, session management, and prompt construction"。

**名实不符（"agents" 名下不是 agent 的事）**：
- **用户身份域**（known_users.rs 1496 + user_registry.rs 845 + user_profile.rs 368 + mention.rs 361 + user_messages.rs 354 ≈ 3424 行，9.0%）：联系人/好友/邮箱/用户名注册表，与 agent 循环零耦合（user-identity 簇内互引仅 ~3，见 §3）。外部消费者是 tools/send_message、tools/friends、daemon，不是 agent 循环本体。
- **调度域 scheduling/**（4582 行，12.0%）：cron/webhook 定时任务系统，含独立 HTTP webhook server（scheduler.rs:2014 `run_webhook_server`）。这是"任务调度器"，与 AI agent 无内在关系，只因 cron 触发要跑 turn 才住在 agents 下。
- **slash 命令层 commands/**（2433 行，6.4%）：`/help` `/model` `/friends` 等 UI 命令分发，是渠道接口层逻辑（commands/mod.rs:1 自称 "intercepts /command in the orchestrator layer"）。
- **identity/prompt 文案**：user_messages.rs 是纯中文文案常量表（"User-facing message strings"，user_messages.rs:1）。
- 结论：68 文件中约 **21 个（≈11300 行，30%）与 agent 循环无内聚关系**，是身份、调度、命令、文案四块借居。

## 2. 全文件清单（68 文件）

行数=wc -l；pub=pub 符号数；外部引用=src/agents 之外引用该模块（符号名 grep 验证，"仅内部"=只在 agents 内被引）。测试文件标注 #[cfg(test)]。

| 路径 | 行数 | 职责 | pub | 外部引用 |
|---|---|---|---|---|
| scheduling/scheduler.rs | 3849 | cron+webhook 调度器与 HTTP server | 130 | daemon, tools/cronjob_tool |
| delegation_coordinator.rs | 2734 | 子代理创建/worktree/异步运行/检查点 | 32 | daemon, lib, storage/session, tools/{agent_kill,agent_list,agent_resume,send_message}, tools/mod |
| agent.rs | 2529 | Agent::run 主循环（LLM 工具循环） | 5 | 全仓核心（daemon/cli/tools/channels/providers 经 re-export） |
| context_engine.rs | 1993 | 上下文压缩/摘要/折叠 | 15 | daemon, cli/{cmd_exec,cmd_chat} |
| session_context.rs | 1952 | 会话上下文+process_turn+挂起/恢复 | 35 | daemon, tools/{shell,ask_user}, providers/capability_chat, channels/{client,message}, lib |
| orchestrator/delegation.rs | 1560 | 子代理完成唤醒路由进父会话 | 3 | 仅 orchestrator 内部 |
| known_users.rs | 1496 | 联系人/好友/邮箱注册表 | 54 | config, tools/{send_message,friends}, daemon |
| orchestrator/recovery.rs | 1397 | 启动恢复中断 turn/子代理 | 0 | 仅 orchestrator 内部 |
| orchestrator/inbound.rs | 1213 | 入站消息分发→turn | 0 | 仅 orchestrator 内部 |
| memory_distill.rs | 1087 | 空闲期跨用户记忆蒸馏 | 17 | 仅内部（orchestrator/scheduled 调） |
| attachment.rs | 1034 | skills/agents/MCP 列表增量注入 system-reminder | 13 | 仅内部（agent.rs 调） |
| loop_breaker.rs | 912 | 工具循环死循环检测 | 20 | config, daemon, cli/{cmd_exec,cmd_chat} |
| user_registry.rs | 845 | 用户名/邮箱注册与认证 | 40 | config, tools/{send_message,friends}, daemon |
| session/manager.rs | 829 | SessionManager：会话表+轮转+锁 | 36 | 仅内部（daemon 经 re-export） |
| workspace/skill_loader.rs | 777 | skill 定义加载/校验 | 17 | 仅内部 |
| orchestrator/mod.rs | 741 | Orchestrator 事件主循环 | 45 | daemon, lib |
| memory_fork.rs | 719 | turn 末后台记忆提取 fork | 11 | 仅内部（agent.rs 调） |
| session/types.rs | 684 | Session 结构+token_tracker | 55 | 仅内部 |
| tool_executor.rs | 619 | 工具执行器（超时/并发/取消） | 4 | tools/file_ops, daemon, cli/{cmd_exec,cmd_chat} |
| commands/info.rs | 614 | /help /status /context 等 12 命令 | 12 | 仅 commands/mod 分发 |
| skill_extract.rs | 541 | 会话蒸馏草稿 skill+提醒 | 11 | 仅内部（agent.rs 调） |
| session/backend.rs | 503 | JSONL 会话持久化后端 | 6 | 仅内部 |
| session/session_override.rs | 472 | /session 临时覆盖与净化 | 13 | 仅内部 |
| media_e2e_test.rs | 469 | 多模态 e2e 测试 [test] | 0 | 无 |
| prompt.rs | 441 | 系统提示词组装 | 14 | 仅内部 |
| delegation.rs | 392 | 委派事件/邮件类型+邮箱 | 26 | orchestrator/mod, delegation_coordinator, delegator（均在 agents 内） |
| user_profile.rs | 368 | 用户画像与 UserResolver | 17 | config, tools/{send_message,session_query,memory_tool}, daemon, channels/client, lib, cli |
| workspace/agent_loader.rs | 361 | agents/ 目录配置加载 | 3 | 仅内部 |
| orchestrator/scheduled.rs | 361 | cron/distill 触发执行 | 0 | 仅 orchestrator/mod |
| mention.rs | 361 | @提及入站解析/ref 出站渲染 | 7 | 仅内部（orchestrator/inbound 调） |
| user_messages.rs | 354 | 面向用户中文文案常量 | 9 | 仅内部 |
| tokens.rs | 352 | token 估算 | 18 | 仅内部 |
| commands/session.rs | 333 | /session /export 命令 | 7 | 仅 commands/mod |
| scheduling/cron_types.rs | 291 | cron job 定义 serde 类型 | 40 | tools/cronjob_tool（经 re-export） |
| orchestrator/ctx.rs | 277 | OrchestratorCtx 依赖包 | 30 | 仅 orchestrator 内部 |
| scheduling/work_unit.rs | 268 | 上下文工作单元 | 7 | 仅内部 |
| commands/link.rs | 261 | /link 渠道绑定命令 | 2 | 仅 commands/mod |
| turn_event.rs | 241 | turn 事件流/版本化事件 | 29 | channels/turn_stream, providers 协议×4 |
| commands/register.rs | 236 | /register /username /email | 3 | 仅 commands/mod |
| workspace/watcher.rs | 214 | agents/skills 目录热重载监听 | 11 | 仅内部 |
| workspace/skills.rs | 206 | SkillManager 运行时 skill 执行 | 24 | tools/{skill_tool,skills_list_tool,skill_manage_tool} 经 re-export |
| ask_router.rs | 198 | ask_user 待答 futures 表 | 4 | tools/ask_user, daemon |
| mcp_manager.rs | 193 | MCP server 生命周期管理 | 8 | 仅内部（runtime 调） |
| turn.rs | 190 | TurnResult/挂起/子结果类型 | 35 | tools 多处经 re-export |
| runtime.rs | 189 | AgentRuntime 单例依赖包 | 28 | daemon, cli |
| scheduling/cron_loader.rs | 170 | jobs 目录加载 | 2 | 仅内部 |
| commands/config.rs | 167 | /config /settings /autonomy | 3 | 仅 commands/mod |
| orchestrator/test_support.rs | 163 | 测试桩 [test] | 2 | 无 |
| skill_draft_reminder.rs | 160 | 草稿 skill 积压每日提醒 | 1 | 仅内部（scheduled 调） |
| commands/model.rs | 153 | /model /models /think | 3 | 仅 commands/mod |
| agent_registry.rs | 145 | name→Arc<Agent> 表 | 11 | 仅内部 |
| orchestrator/key.rs | 122 | channel:account:sender 路由键 | 11 | 仅 orchestrator 内部 |
| recovery.rs | 109 | 启动恢复类型 UnfinishedSubAgent | 10 | daemon（scan 后传 orchestrator） |
| mod.rs | 98 | re-export 门面（66 个 pub use） | 66 | 全仓 39 文件经此引用 |
| llm_stream.rs | 93 | LLM 流事件适配 | 7 | 仅内部（agent.rs 调） |
| delegator.rs | 76 | AgentDelegator/AgentMessenger trait | 2 | tools/delegate, daemon, lib |
| error.rs | 66 | AgentError | 1 | 仅内部 |
| session/recovery.rs | 65 | 断点检测（incomplete turn） | 6 | 仅内部 |
| tool_registry.rs | 60 | 工具名→dyn Tool 表 | 7 | 仅内部（runtime 调） |
| orchestrator/turn.rs | 55 | 单 turn 执行封装 | 8 | 仅 orchestrator 内部 |
| orchestrator/event.rs | 55 | OrchestratorEvent 枚举 | 1 | mod.rs re-export |
| resource_provider.rs | 53 | CLI 资源提供 trait | 3 | cli 经 re-export |
| commands/reload.rs | 52 | /reload 命令 | 2 | 仅 commands/mod |
| session/mod.rs | 15 | re-export | 10 | — |
| workspace/mod.rs | 4 | mod 声明 | 4 | — |
| scheduling/mod.rs | 4 | mod 声明 | 4 | — |

外部引用模块分布（grep `crate::agents`，src/agents 外 39 文件）：**tools/ 33 文件**（重度：shell 26 处、skill_manage_tool 17）、daemon.rs 33 处、channels/ 3 文件、providers/ 5 文件、mcp/ 2 文件、config 1 文件。agents 实为被 tools 层反向重度依赖的域核心。

## 3. 功能簇聚类（词法互引统计，含 mod 名词匹配）

| 簇 | 文件 | 行数 | 簇内互引 | 簇外出引 | 簇外入引 | 内聚判断 |
|---|---|---|---|---|---|---|
| delegation+recovery | 7 | 6333 | ~175 | ~1355 | ~159 | 强内聚，但内部又分两层（见 §4） |
| context/prompt（context_engine,prompt,tokens,memory_distill,memory_fork,llm_stream） | 6 | 4684 | ~59 | ~325 | ~203 | 中内聚：都围绕"上下文与 token" |
| scheduling | 5 | 4582 | ~12 | ~245 | ~48 | **弱内聚**：scheduler 巨石 vs 3 个小类型文件 |
| session（session/ 5 + session_context.rs） | 6 | 4455 | ~131 | ~904 | ~196 | 中内聚：session_context 是消费方 |
| agent-core（agent,runtime,tool_*,registry,ask_router,error） | 9 | 3957 | ~131 | ~602 | ~746 | 强内聚（被全簇引用） |
| user-identity | 5 | 3424 | **~3** | ~127 | ~169 | **几乎零内聚**：5 文件互相不引用，只是同域堆放 |
| orchestrator（mod,ctx,inbound,turn,event,key,scheduled） | 10 | 3418 | ~387 | ~408 | ~1474 | 最强内聚（应用层枢纽，全簇被引） |
| commands | 9 | 2433 | ~164 | ~501 | ~1679 | 强内聚但入引多为词面误报（"session"等词），实际仅 commands/mod 分发 |
| misc（attachment,loop_breaker,media_e2e_test） | 3 | 2415 | ~2 | ~215 | ~41 | 临时堆放：三者互不相关 |
| workspace/skills | 7 | 2263 | ~81 | ~172 | ~147 | 中内聚 |
| mcp | 1 | 193 | 0 | ~15 | ~7 | 单文件 |

## 4. delegation 家族专查（4 文件 + 2 个 recovery）

**分工与引用链**（`use` 实测）：
- `delegation.rs`（392 行）：**纯类型层**——DelegationStatus/DelegationEvent/AgentMail/SubAgentMailbox + 邮件渲染。被 delegation_coordinator.rs:32、delegator.rs:13、orchestrator/mod.rs:29 引用。无状态。
- `delegator.rs`（76 行）：**纯接口层**——`AgentDelegator` + `AgentMessenger` trait，给 tools/delegate 解耦用（delegator.rs:1-10 自述）。实现方是 coordinator。
- `delegation_coordinator.rs`（2734 行）：**实现层**——worktree 创建、子会话、深度检查、checkpoint、resume_timed_out、recover_async、impl AgentDelegator(L1689)/AgentMessenger(L1759)。
- `orchestrator/delegation.rs`（1560 行）：**消费层**——子代理完成后如何唤醒父会话（路由到 active/inactive session，orchestrator/delegation.rs:1-17）。

**API 重叠面**：四者无同名函数（类型/接口/实现/消费四层），**是分层而非重复**。真正的重复风险在 recovery 域：`recovery.rs`（109 行，启动扫描类型）、`orchestrator/recovery.rs`（1397 行，turn 恢复调度）、`session/recovery.rs`（65 行，断点检测）三处 "recovery" 各指一事，加上 coordinator 内 `resume_timed_out`(L1465)+`recover_async`(L1551) 与 orchestrator/recovery.rs 的恢复路径职责交界模糊（coordinator 恢复子代理本体、orchestrator 恢复路由）。

**结论**：delegation 四件套**不合并**，是干净的 trait/impl/event/consumer 分层；但建议统一挪进 `agents/delegation/` 目录（delegation.rs 类型已私下被 3 处 `use crate::agents::delegation` 跨层引用，目录化收益明确）。三个 recovery 应改名消歧（startup_recovery / turn_recovery / breakpoint_detect）。

## 5. 巨文件剖面（>1500 行）

| 文件 | 行数 | 生产/测试 | struct/enum/trait | impl 块 | 函数总数 | >300 行函数 |
|---|---|---|---|---|---|---|
| scheduling/scheduler.rs | 3849 | 2701/1148 | 13 | 7 | 145 | handle_request 279（未超） |
| delegation_coordinator.rs | 2734 | 1821/913 | 3 | 3 | 57 | **delegate_with_parent 589 (L562)** |
| agent.rs | 2529 | 1766/763 | 5 | 4 | 73 | **run_inner 895 (L81)** ⚠PR#146 已拆 |
| context_engine.rs | 1993 | 1327/666 | 3 | 1 | 50 | 无（top: maybe_compact 210, compact_until_fit 139, do_summarize 127, execute_compaction 126, collect_summary_stream 112, hard_fold_history 104, build_summarizer_prompt 91） |
| session_context.rs | 1952 | 1376/576 | 4 | 3 | 58 | **process_turn 657 (L584)** |
| orchestrator/delegation.rs | 1560 | 840/719 | 0 | 0 | 4 | 无（巨函数藏在闭包/长 match 中） |

**scheduler.rs top10**：handle_request 279(L2079, webhook HTTP)、mark_run_result 103(L782)、run 95(L420, 主循环)、update_job 91(L646)、migrate_from_markdown 86(L1294)、webhook_dispatch_returns_202… 86(L3672, 测试)、new 83(L277)、handle_hooks_agent 76(L2427, 测试)、run_webhook_server 63(L2014)、load_jobs_from_dirs 52(L1159)。**天然边界**：①webhook 子系统（WebhookDef/filters/run_webhook_server/handle_request/handle_hooks_agent ≈108 处 webhook 词）可独立成 scheduling/webhook.rs；②cron ticking（run/mark_run_result）；③job CRUD+markdown 迁移（update_job/load_jobs_from_dirs/migrate_from_markdown）。

**delegation_coordinator.rs top**：delegate_with_parent 589(L562)、spawn_delegate_async 271(L1176)、recover_async 130(L1551)、resume_timed_out 85(L1465)、checkpoint_and_cancel_all 49(L328)。**天然边界**：同步路径（delegate_with_parent）与异步路径（spawn_delegate_async+resume+recover_async+checkpoint 组）调用方不同步；checkpoint 持久化组（persist_terminal_checkpoint L1158/timed_out_checkpoints L307/load_checkpoints L383/checkpoint_and_cancel_all L328）自成一块；worktree 管理（worktree_branch_name L107/cleanup_stale_worktrees L396）自成一块。

**agent.rs**（⚠ PR #146 已拆为 agent/ 目录待合并，此处剖面仅存档）：run_inner 895(L81) 单函数覆盖工具过滤→provider 解析→fallback→循环→流收集→delegation 边界→memory fork→skill extract 全流程；collect_stream 215(L1508)、run_recovery 150(L993)。生产代码 1766 行、5 个 pub 符号——拆分必要性 PR #146 已论证。

**context_engine.rs**：函数普遍 90-210 行、无巨怪，1 个 impl 块 50 函数同住 `impl ContextEngine`。天然边界：compaction 决策组（should_compact/compaction_boundary*/maybe_compact）、summarize 组（do_summarize/collect_summary_stream/build_summarizer_prompt）、fold 组（hard_fold_history/execute_full_fold_compaction/full_fold_range_*）、evidence 组（append_evidence_index）。**更适合按算法族拆 impl 而非拆文件**。

**session_context.rs**：process_turn 657(L584) 为全仓次大函数（入站持久化→沉默决策→RAII 挂起→工具循环调用→TTS 归一→终端记录）；normalize_symbols_for_tts 67(L1309)+prepare_text_for_tts 54(L1252) 是**TTS 展示职责混入会话域**的边界证据。天然边界：TTS 组、挂起/通知状态机组（add_pending_task/bump_notice_turn/take_delegation_notices…L343-542）、process_turn 本体。

**known_users.rs**：63 函数全在 1 个 impl KnownUsersRegistry，含四代迁移代码（migrate_legacy L294、rekey_legacy_to L717、migrate_identity L849、insert_or_update L379）——迁移组可隔离；top 函数 35-59 行无巨怪，问题是**单 impl 巨型注册表**而非巨函数。

## 6. 依赖方向明细（agents → providers/tools/channels）

共 33 文件引 providers、12 文件引 tools、19 文件引 channels（grep `crate::providers|tools|channels` 实测计数）。

**→ crate::providers**（117 处 inline + use；高频符号 ChatMessage×10 处 use、ContentPart/ToolCall、ErrorCategory/ClassifiedError/ProviderHttpError、fallback 标签、media、capability_chat 27 处 inline）：重灾区 agent.rs(27)、user_messages.rs(15)、session_context.rs(15)、session/session_override.rs(9)、orchestrator/recovery.rs(9)、commands/info.rs(9)、session/types.rs(8)。providers 也反向引 agents（protocols×4、capability_tool 共 10 处 `crate::agents::turn_event`）——**agents↔providers 双向耦合**。

**→ crate::tools**（12 文件）：orchestrator/recovery.rs(6)、orchestrator/delegation.rs(5)、delegation_coordinator.rs(4)、runtime.rs(3)、media_e2e_test(4)；具体符号 `tools::shell` 26 处 inline（loop_breaker 的 shell 检测）、SearchProviderCooldown、truncation、builtin_tools。tools 层 33 文件反向引 agents——**tools↔agents 是全仓最强双向依赖对**。

**→ crate::channels**（90 处 inline + use；Channel/ChannelInboundMessage/ChannelOutboundMessage/MessageReceiver/TurnStream）：session_context.rs(18)、agent.rs(15)、orchestrator/delegation.rs(14)、orchestrator/scheduled.rs(7)、skill_extract.rs(5)、delegation_coordinator.rs(4)、ask_router.rs(4)。channels 仅 3 文件反向引 agents（client/telegram/turn_stream）——单向为主。

## 7. 异味清单（文件:行号 证据）

1. **超 300 行函数**：agent.rs:81 `run_inner` 895 行（PR #146 拆分待合并）；delegation_coordinator.rs:562 `delegate_with_parent` 589 行；session_context.rs:584 `process_turn` 657 行。
2. **职责混杂（域内借居）**：known_users.rs/user_registry.rs/user_profile.rs/mention.rs/user_messages.rs 共 3424 行身份域代码住在 agents（簇内互引 ~3，§3）；scheduling/ 4582 行任务调度（scheduler.rs:2014 内嵌 HTTP server）与 agent 循环仅靠 CronTrigger 一根线相连。
3. **命名与内容不符**：①三个 recovery 三义——agents/recovery.rs（启动扫描）、orchestrator/recovery.rs（turn 恢复）、session/recovery.rs（断点检测），delegation_coordinator.rs:1465 resume_timed_out 再加第四义；②delegation_coordinator.rs 与 orchestrator/delegation.rs 同名模块异层，`use crate::agents::delegation`（orchestrator/mod.rs:29）与目录内 `mod delegation` 并存易混；③delegator.rs:4 注释自称实现在 "scheduler/delegation module"——**指向已不存在的旧路径**，注释漂移。
4. **重复/漂移实现**：①"webhook route slug 校验"逻辑在 scheduler.rs:583-595 与 cronjob 工具描述重复维护；②session_context.rs TTS 组（L1252-1375）与展示格式化职责在 channels 层有重叠；③mcp_manager.rs:8 注释 "ToolRegistry + ToolRegistry" 重复词，文档粗粝；④media_e2e_test.rs（469 行 #[cfg(test)]）混在生产目录。
5. **测试占比失衡**：orchestrator/delegation.rs 1560 行中 719 行（46%）是测试，生产仅 840 行；scheduler.rs 测试 1148 行——巨文件一半是测试代码，拆分时可随生产代码同迁。
6. **mod.rs 门面过重**：agents/mod.rs 98 行中 66 个 pub use，外部 39 文件全经此门面引用——符号归属被掩盖（如 tools/shell.rs 引 26 处 crate::agents 无法看出实际来自 loop_breaker），目录化时应改外部直接路径引用。
7. **impl 巨块**：known_users.rs 1 个 impl 63 函数（L198-1496）、delegation_coordinator.rs impl DelegationCoordinator 1 块跨 L225-1819（生产 1594 行）、context_engine.rs 单 impl 50 函数——结构性而非函数性巨石。
