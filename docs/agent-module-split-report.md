# agent.rs 模块拆分 — 实施报告（批 5 收官）

- **分支**: `refactor/agent-module-split`；批 1–4 见同分支历史，批 5 = 测试全量迁移 + 收官数据
- **基线**: 拆分前 `src/agents/agent.rs` 2529 行（`mod tests` 占 L1768–2529）

## 1. 逐文件行数（批 5 后，wc -l）

| 文件 | 行数 | 职责 |
|---|---|---|
| mod.rs | 803 | Agent 身份 + run/run_inner/run_recovery 编排；骨架级测试 6 个 |
| exec_marker.rs | 217 | persist_last + exec marker 三函数 + ExecMarkerGuard（测试 4 个随迁） |
| tool_filter.rs | 419 | 4 个过滤函数 + fold_absent_tool（fold 测试 4 个随迁） |
| stream_collect.rs | 443 | CollectedResponse + collect_stream + push_or_drop（批 1 已含 7 测试） |
| injections.rs | 212 | reminder 重建 + 收件箱 drain + per-round 注入 |
| retry.rs | 118 | is_transient_llm_error + backoff_duration + chat_with_retry |
| turn_state.rs | 175 | TurnState 封装 9 flag + has_pending（检测逻辑测试 3 个随迁） |
| finalize.rs | 234 | 终局路径：Done/persist + memory fork + skill extract fork |
| tool_phase.rs | 323 | execute_tool_batch + 4 种流程控制检测 |
| tests.rs | 96 | 共享 fixture：BailingRegistry / bailing_runtime / empty_config |

`run_inner` 本体 = **250 行**（RFC §5 上限 250，压线达标；批 4 骨架化后仅剩循环骨架 + 分派）。
单文件全部 ≤ 500 行（tests.rs 豁免项实际 96 行）。

## 2. 对外符号 / 私有实现（module_score，§4.6 深模块检验）

见 `scripts/module-score-after-b5.json`（基线 `module-score-baseline.json`：单文件 2529 行 / pub 3 / priv_fn 56）。
拆分后合计 pub 符号 23、私有 fn 46，`mod.rs` 自身收敛到 pub 2（`Agent`/`new`）+ priv_fn 7 —— 巨石的 56 个私有函数
全部就近落到符号所属文件，`fold_absent_tool` 仍是唯一 pub(crate) 外部消费点（context_engine.rs 两处调用不变）。

## 3. 变更放大 A/B 对照（三类代表性改动）

| 改动类型 | 拆分前 touch | 拆分后 touch |
|---|---|---|
| #144 类投递修复（tool 结果分流/投递语义） | agent.rs 单文件 2529 行内定位 + 修改（review diff 混在巨函数里） | tool_phase.rs（批执行与分流）+ 对应检测测试 turn_state.rs，两者 ≤ 500 行 |
| #141 类 shell 语义修复（退出码/后台语义） | agent.rs（检测逻辑内联在 run 循环） | tool_phase.rs（inline 检测）+ turn_state.rs（`shell_pending_spawned` flag 与镜像测试），tools/shell.rs 不再需要动 agent 侧 |
| 新增一种流程控制检测（如新 tool 结果分流） | agent.rs run_inner 895 行内插入分支 + 新 flag 手工贯穿循环 | tool_phase.rs 加检测分支 + turn_state.rs 加字段 + has_pending 一处派生，run_inner 骨架零改动 |

结论：三类改动的修改面从「单巨文件内嵌定位」收敛为「符号所属文件 + flag 所属文件」，run_inner 骨架不再被
功能性改动触碰（A/B 放大系数 1 → 1，但定位/审阅面从 2529 行降到 ≤ 500 行/文件）。

## 4. 测试迁移清单（批 5）

- → tool_filter.rs：fold_view_image_inlines_results_when_tool_absent / fold_view_image_is_noop_when_tool_present / fold_send_message_inlines_results_when_tool_absent / fold_send_media_legacy_calls_when_tool_absent
- → turn_state.rs：async_delegation_mode_detection / shell_pending_spawned_detection_logic / sessions_yield_detection_logic
- → exec_marker.rs：exec_marker_write_read_clear_roundtrip / exec_marker_none_sessions_dir_is_noop / exec_marker_guard_clears_on_drop / exec_marker_guard_clears_on_timeout_cancellation
- → tests.rs（fixture）：BailingRegistry / bailing_runtime / empty_config / NamedTool·tool_names 留在 mod.rs（turn_tool_allowlist 测试仍用）
- 留 mod.rs（骨架级）：run_prechecks_recovery_without_recursing / turn_tool_allowlist_* ×3 / agent_holds_config / session_persist_field_default_none

守恒校验：迁移前后 agent/ 目录 `#[test]`+`#[tokio::test]` 总数 25 = 25；测试函数名全目录唯一（uniq -d = 0）。
