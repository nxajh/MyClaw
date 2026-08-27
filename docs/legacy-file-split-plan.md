# 遗留巨文件拆分方案（§5.3 收尾计划）

> 状态：草案（待用户批准后按 P0→P4 分批实施）
> 日期：2026-08-28
> 背景：#151 架构重构 Phase 0–11 已收官，§5.3 指标遗留两项未达：巨文件 13 个（≥1400 行）未降到 <10；churn 待 2026-11 底复测。本方案解决第一项。
> 先例：orchestrator.rs、session_context.rs、daemon.rs（8f）、channels/message.rs（8i）均已目录化成功；agent.rs 拆分（RFC #145 / PR #146）因让位 #151 而 CLOSED，方案可复活。

## 0. 原则

1. **纯移动重构，零行为变更**——语句顺序逐句保持；「顺手合并重复」= 行为变更，禁止。
2. **外部路径稳定**——`pub`/`pub(crate)` 符号经 mod.rs `pub(crate) use` 转发，消费者零改动。
3. **CI-only 验证**——禁止本地 cargo（no-local-build-on-micro）；每批一轮 CI，主代理跟修。
4. **验收实测**——子代理报告不可信，wc / 符号守恒 / 外部 diff 逐项实测。
5. 流程细节执行 `myclaw-module-split` skill（RED 已提炼过，不再重跑）。

## 1. 现状剖面（2026-08-28 实测）

| # | 文件 | 行数 | 结构特征 | churn/90d | 层 | 拆后目标 |
|---|------|------|----------|-----------|-----|----------|
| 1 | webui/client.rs | 2852 | bus(43–175) + channel(175–1284) + **api handler 900 行**(1370–2233) + reconstruct_history + tests(2391–) | **1** | L5* | 5 文件 |
| 2 | channels/qqbot/channel.rs | 2833 | 协议常量 + 文本工具群(display_width/split_by_visual_lines/gfm) + Channel impl；旁边已有 flow/keyboard/sanitize/token/types | **66** | L5 | 3 文件 |
| 3 | channels/telegram/channel.rs | 2647 | 3 个 impl 块(135/1099/1730) + tests 462 行 | **67** | L5 | 4 文件 |
| 4 | agents/delegation_coordinator/mod.rs | 2609 | **1300 行 impl**(202–1528) + 3 个 trait impl + tests 中置（2581 的 AgentLifecycle impl 在 tests mod 之后——组织异味） | 待测 | L4 | 5 文件 |
| 5 | agents/agent.rs | 2529 | **1120 行 impl Agent**(47–1168) + 工具过滤/exec_marker/stream 收集/retry 群 + tests 760 行 | **113** | L4 | 11 文件（RFC #145 现成） |
| 6 | agents/compaction_engine.rs | 1993 | **1000 行 impl**(60–1064) + fold 纯函数群 + summarizer prompt + evidence 提取 + audit | 待测 | L4 | 5 文件 |
| 7 | scheduling_runtime/scheduler.rs | 1986 | 3 个 impl 块 + **fold 迁移函数群**(864–1040) + 时间函数(parse_interval/compute_next_run) | 待测 | L4 | 4 文件 |
| 8 | agents/orchestrator/delegation.rs | 1646 | 通知分发函数群 + **tests 770 行（占半文件）** | 待测 | L4 | 3 文件 |
| 9 | migration.rs | 1619 | 类型(plan/step/report) + migrate_* 每域函数（users/jobs/sessions/tasks/archive） | 低 | L1 | 5 文件 |
| 10 | scheduling_runtime/webhook.rs | 1533 | 类型 + template 渲染 + **server(309–668)** + dispatch/agent/wake handlers | 待测 | L4 | 4 文件 |
| 11 | identity/known_users.rs | 1516 | 类型群(entry/mail/verdict) + **824 行 impl**(198–1022) + tests | 待测 | L2 | 3 文件 |
| 12 | storage/json_file.rs | 1492 | 记录类型 + **双 impl**(136–640, 644–1314) + tests | 待测 | L2 | 4 文件 |
| 13 | tools/memory_tool.rs | 1406 | 搜索评分纯函数群(token/score/snippet) + 审计/版本化 + 主 tool | 待测 | L3 | 3 文件 |

\* webui/client.rs 的 Channel 实现属 L5 职责；layering 脚本按目录前缀递归匹配（已核实），层内目录化不影响判定。

**边界观察名单**（<1400 不拆，避免为指标而指标）：agents/orchestrator/turn_recovery.rs 1397、daemon/mod.rs 1299、tools/cronjob_tool.rs 1228、agents/orchestrator/inbound.rs 1213。终态若全库最大文件是 daemon/mod.rs 1299，即历史最优。

## 2. 逐文件拆分设计

### P0 — webui/client.rs → webui/client/（探路批）

churn=1（全库最冷）× 规模第一（2852），零冲突、立收益。

| 新文件 | 内容（源行段） | 预估行数 |
|--------|----------------|----------|
| mod.rs | 声明 + re-export（ClientChannel 等） | ~80 |
| bus.rs | Subscriber / SessionOutputBus(43–175) + bus_key_candidates(1036–1075) | ~220 |
| channel.rs | ClientConnection / ClientChannel / impl Channel(175–1235) + ClientTurnStream | ~1100 |
| api/mod.rs + api/skills.rs, api/memory.rs, api/history.rs | ApiContext + handle_api_request(1317–2233) 按路由域内分 + reconstruct_history(2233–2391) | ~1000（每块 ≤400） |
| tests.rs | 2391–2852 | ~460 |

拆后最大文件 channel.rs ~1100 < 1400 ✓。

### P1 — 冷文件组（migration / json_file / known_users / orchestrator/delegation）

- **migration.rs → migration/**：types.rs（Backup/Step/Plan/Report）+ plan.rs（build_plan/run_auto）+ jobs.rs + sessions.rs + users.rs（migrate_users/resolver/archive + fqid 工具）。每文件 ≤450。一次性代码，churn≈0，纯机械。
- **storage/json_file.rs → json_file/**：records.rs（CompactionEntry/SegmentRecord/SessionMeta/ActiveMap）+ backend.rs（impl JsonFileBackend 136–640）+ session_backend.rs（impl SessionBackend 644–1314）+ tests.rs。两个 trait impl 天然是两个文件。
- **identity/known_users.rs → known_users/**：types.rs（ContactDirection/Status/Entry/UserMail/Outcome/Verdict/KnownUser/Persisted/Legacy）+ registry.rs（impl 198–1022）+ routing.rs（routing_key/rk_for）+ tests.rs。824 行 impl 若内聚不再强拆（阈值自检：≤1400 达标即可，深模块比优先于行数）。
- **orchestrator/delegation.rs → delegation/**：notice.rs（构造/格式化函数群 57–674）+ dispatch.rs（split_batch_ids/dispatch_notice_batch 674–876）+ tests.rs（877–1646）。tests 独立即半文件出仓。

**达标点：P0+P1 完成 → ≥1400 文件数 13−5=8 < 10，§5.3 指标达成。**

### P2 — 调度与引擎组（scheduler / webhook / memory_tool / compaction_engine）

- **scheduler.rs → scheduler/**：jobs_file.rs（load_jobs_from_dirs + fold_* 迁移群 864–1040——这批是 job 文件格式兼容代码，天然一块）+ timing.rs（parse_interval/compute_next_run）+ core.rs（struct + 3 个 impl 块的运行时方法）+ tests.rs。
- **webhook.rs → webhook/**：types.rs（Def/Context/JobDef/Auth/Guard）+ template.rs（render_template/navigate_json）+ server.rs（run_webhook_server/handle_request 309–668）+ dispatch.rs（dispatch_webhook_turn/handle_hooks_agent/wake + filter 工具）+ tests.rs。
- **memory_tool.rs → memory_tool/**：search.rs（query_tokens/token_matches/field_match_score/memory_search_score/best_snippet 纯函数群——无状态可测性陡增）+ audit.rs（MemoryAudit/append/redact/archive_version）+ ops.rs（主 tool 分支）+ tests.rs。
- **compaction_engine.rs → compaction_engine/**：engine.rs（impl CompactionEngine 60–1064）+ fold.rs（hard_fold/truncate/user_text/media_markers 纯函数群）+ summarizer.rs（build_summarizer_prompt/SummaryResponse/audit_quality）+ evidence.rs（append_evidence_index/extract_shell_commands/commit_hashes/ci_runs）+ tests.rs。impl 1004 行 > 800 目标线 → RFC 阶段决定是否再内分（compress 主流程 vs incremental 分支）。

### P3 — 高热 agents 组（delegation_coordinator / agent.rs）

- **delegation_coordinator/mod.rs**：先修组织异味（2581 的 AgentLifecycle impl 挪到 tests 之前）再拆：registry.rs（RunningEntry/入队/查询）+ lifecycle.rs（spawn/resume/kill/gc）+ delegator.rs（impl AgentDelegator）+ messenger.rs（impl AgentMessenger）+ agent_lifecycle.rs（impl AgentLifecycle）+ tests.rs。1300 行 impl 是主战场，批次按 trait 边界切。
- **agent.rs**：复活 docs/agent-module-split-rfc.md（#145，已 MERGED 进 master）：agent/ 11 文件（mod 薄壳 + turn_loop/turn_state/tool_phase/injections/finalize/stream_collect/retry/tool_filter/exec_marker/tests）。唯二外部消费者 fold_absent_tool（context_engine.rs）路径同步。churn=113 全库最高——**需 feature freeze 窗口**（建议与一次部署周期绑定）。

### P4 — Channel 双雄（qqbot / telegram，最后）

前置依赖：channel-role-split RFC（状态「实施中」）收尾——双职责解构是行为级改动，必须先于文件级拆分完成，否则 rebase 地狱。

- **qqbot/channel.rs → qqbot/channel/**（与现有 flow/keyboard/…并列）：protocol.rs（GATEWAY/TOKEN/API_BASE/OP_* 常量 + user_agent）+ text.rs（display_width/estimate_visual_lines/split_by_visual_lines/gfm/fence/strip_internal_tags——评估哪些可上提 channels/shared 替代私有副本，**上提属行为级合并，本计划外单列**）+ channel.rs（impl Channel + WS 会话主体）。2833 → 最大 ~1200。
- **telegram/channel.rs → telegram/channel/**：三个 impl 块天然三文件（api.rs 135–1099 发送与编辑 / session.rs 1099–1730 会话与状态 / channel.rs impl Channel 1730–2185）+ tests.rs（462 行出仓）。

## 3. 执行规程（每文件统一）

1. **轻量 RFC**（本方案 §2 已给骨架，实施时补剖面行号 grep 核实 + 符号映射表 + 阈值一致性自检：§2 预估 vs 验收线）。
2. **coder 分批**（每文件 2–5 批，从纯函数批 → impl 块批 → tests 迁移批），四硬约束：纯移动/禁本地 cargo/push 后 detach 立即结束不等 CI/tracing 字段显式赋值。
3. **主代理**：gh run watch 后台跟 CI；clippy -D warnings 机械修复在主代理 worktree 直接做。
4. **实测复核**：wc -l 逐文件、`#[test]`+`#[tokio::test]` 计数守恒、关键符号唯一性、git diff 外部消费者为空。
5. **质量闸**：深模块比（对外符号/私有行数，转发壳=红旗，mod.rs 壳豁免）、信息隐藏（flag 收 struct）、变更放大 A/B（2–3 类代表性改动 touch 文件数）。
6. **module_score.py 找回**（评审更正：源分支已删但 `refs/pull/146/head` 引用仍存——`git fetch origin refs/pull/146/head && git show FETCH_HEAD:scripts/module_score.py > scripts/module_score.py`，170 行 advisory 脚本，无需重写）：基线跑一次（13 文件 + churn 补测），每文件拆后复测，advisory 不阻断。

## 4. 验收（对齐 §5.3）

- [ ] ≥1400 行文件数 <10（P0+P1 达成即勾）
- [ ] 终态（P0–P4 全完成）：全库最大 .rs 文件 = daemon/mod.rs 1299（或新产生的 <1400 文件）
- [ ] 每文件：CI 三绿（build/layering/migrate-script-tests）× 每批
- [ ] 每文件：测试计数守恒、外部 diff 空、深模块比不劣化
- [ ] module_score 基线/终态 JSON 进 PR 描述
- [ ] 2026-11 底 churn 复测时，本计划搬移噪声计入说明

## 5. 风险与对策

| 风险 | 对策 |
|------|------|
| qqbot/telegram/agent 高 churn（66/67/113 commits/90d） | 排 P3/P4 殿后；拆分窗口内冻结对应文件 feature 开发；与部署周期绑定 |
| channel-role-split 在途 | P4 硬前置：确认其收尾（合并+部署）后才动 qqbot/telegram |
| 拆分期间 PR #149（CI 效率）若落地 | 取消 push 双触发后 CI 轮次成本下降，利好本计划 |
| impl 巨块拆后可见性爆炸 | mod.rs `pub(crate) use` 转发优先；深模块比红线拦截 |
| migration.rs 拆分价值争议 | 它占 13 席之一、churn≈0 拆分成本最低；按域分文件同时改善一次性代码的可审计性——不是凑指标 |

## 6. 节奏估算

- P0：1 文件 / 1 周（含 module_score 重写）
- P1：4 文件 / 2 周（冷文件机械批，可两文件并行委派）
- P2：4 文件 / 2–3 周
- P3：2 文件 / 2 周（等窗口）
- P4：2 文件 / 2 周（等 channel-role-split 收尾）
- **合计 6–9 周**；P0+P1（前 3 周）即达成 §5.3 指标，其后按带宽推进。

## 7. 与既有工作的衔接

- 分支清理：`docs/agent-module-split-rfc`（已 MERGED 可删远端）、`refactor/daemon-module-split`（Phase 8f 已完成可删）——实施 P0 前顺手清。
- issue #147/#148/#149 均与本计划执行效率相关，落地顺序不阻塞。
