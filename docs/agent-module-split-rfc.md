# RFC: agent.rs 模块拆分 — 2529 行巨文件的目录化

- **状态**: 实施完成（待 PR review，2026-08-24）
- **日期**: 2026-08-24
- **来源**: 无提示 RED 基线（子代理裸做拆分方案）+ 三处流程修正
- **关联**: issue #140（has_pending 语义，已合并）、在途 issue #141/#144（冲突窗口见 §4.3）

## 0. 问题陈述

`src/agents/agent.rs` 2529 行，实为「一个大函数 + 散装辅助 + 760 行测试」：

- `run_inner()` 895 行（L81–975）：单层 loop 内混至少 10 个关注点（循环守卫、压缩后
  reminder 重建、收件箱注入、LLM 调用重试、token 追踪、空响应重试、终局路径、
  工具批执行 + 4 种流程控制检测），9 个可变 flag/计数器跨循环贯穿
- 辅助函数按到达顺序堆积，无内聚分组
- #140/#142 期间多处改动交错落在同一文件，review 面 Diff 噪声大

纯移动重构，**不改任何行为**。公共 API（`agents::Agent` re-export、`run`/`run_recovery`
签名）不变，外部零改动。

## 1. 现状剖面（行号已核实）

| 行区间 | 内容 | 职责 |
|---|---|---|
| 1–80 | `Agent` 结构体 + `run()` 入口 | 身份 + 分派 |
| 81–975 | `run_inner()` | LLM↔工具循环全部逻辑 |
| 993–1142 | `run_recovery()` | 崩溃恢复（Case A/B/C） |
| 1148–1217 | `allowed_tools` + `filter_turn_scoped_tools` | 工具集过滤（config 层 / 会话渠道层） |
| 1219–1291 | `native_media_availability` + `filter_modality_redundant_tools` | 媒体模态过滤 |
| 1293–1352 | `fold_absent_tool()`（**pub(crate)**） | 孤儿工具调用折叠 |
| 1354–1460 | `persist_last` + exec marker 三函数 + `ExecMarkerGuard` | 持久化 + 崩溃循环防护 |
| 1461–1732 | `CollectedResponse` + `collect_stream()` | LLM 流收集 |
| 1733–1767 | `is_transient_llm_error` + `backoff_duration` | 重试策略 |
| 1768–2529 | `mod tests` | 测试 + 公共 fixture（`bailing_runtime` 等） |

**唯一外部消费者**：`fold_absent_tool` 被 `context_engine.rs:101/107` 调用（send_file/
send_media 按需工具折叠），拆走后两处路径同步改。

## 2. 目标形态：`src/agents/agent/` 目录，`mod.rs` 薄壳

```
src/agents/agent/
  mod.rs            Agent 结构体、new、run、run_recovery 骨架（~150 行）
  turn_loop.rs      run_inner 主体：循环骨架 + 顶部守卫（≤250 行）
  turn_state.rs     TurnState：9 个循环 flag 的结构体封装
  tool_phase.rs     工具批执行 execute_tool_batch + 4 种流程控制检测
  injections.rs     压缩后 reminder 重建 + 收件箱 drain + deadline/turn_injections
  finalize.rs       终局路径：Done/defer_collapse/persist + memory fork + skill extract fork
  stream_collect.rs CollectedResponse + collect_stream + push_or_drop
  retry.rs          is_transient_llm_error + backoff_duration + chat_with_retry 包装
  tool_filter.rs    4 个过滤函数 + fold_absent_tool
  exec_marker.rs    exec marker 全家 + ExecMarkerGuard + persist_last + last_user_text + llm_usage
  tests.rs          公共测试 fixture（bailing_runtime/BailingRegistry）
```

纯函数测试按符号就近搬到对应子模块 `#[cfg(test)]`。

## 3. 符号映射与可见性

| 符号 | 去向 | 可见性 |
|---|---|---|
| `Agent`/`new`/`run` | mod.rs | 不变（pub） |
| `run_recovery` | mod.rs | pub（签名不动） |
| `run_inner` + 9 flag | turn_loop.rs + turn_state.rs | 私有 |
| 工具批 for 循环 | tool_phase.rs | 私有 |
| reminder 重建 / 注入块 | injections.rs | 私有 |
| 终局块 | finalize.rs | 私有 |
| `collect_stream`/`CollectedResponse` | stream_collect.rs | 私有 / pub(super) |
| retry 循环 | retry.rs::chat_with_retry | 私有 |
| `fold_absent_tool` | tool_filter.rs | **pub(crate) 保留**，context_engine.rs:101/107 改新路径 |
| 其余过滤 4 函数 | tool_filter.rs | pub(super) |
| exec marker + persist 杂项 | exec_marker.rs | pub(super) |

**设计决策（基线遗留问题）**：提取的子函数放 `impl Agent { }` 跨文件分块，不改自由
函数传参——Rust 原生允许跨文件 impl 同一类型，调用点零改动，语义最接近现状。

## 4. 实施设计（含对基线方案的三处修正）

### 4.1 反馈循环：CI-only（修正①）

基线原案「每步 cargo check」违反 no-local-build-on-micro（cargo check 同样起 rustc，
可 OOM 引发热切换雪崩）。**本地零编译**，步骤按 CI 轮次批量化（§4.4），每批 push 分支
→ CI check + test → 通过再下一批。本地仅静态自查（file_read 复核 + grep 引用面）。

### 4.2 执行路由：worktree + PR 管线（修正②）

基线原案在主仓库 `git checkout -b` 原地执行——废弃。标准路由：

coder 子代理（worktree 隔离，分支 `refactor/agent-module-split`）→ PR →
review-github-pr 深审 → myclaw-pr-deploy-pipeline 合并部署验收。子代理超时按
coder-subagent-worktree-crash-recovery 抢救。

### 4.3 冲突窗口（修正③）

执行前核对在途 issue：#144 动 delegation.rs/session_context.rs/telegram/channel.rs，
#141 动 shell.rs。与 agent.rs + context_engine.rs 无直接交集，但若 #144 先实施，
本分支 rebase 后再动。开工时以 `gh issue view` 实时状态为准。

### 4.4 分批计划（每批 = 一次 CI 轮次）

1. **批 1**：零依赖纯函数搬移——exec_marker / tool_filter / stream_collect / retry
   四文件 + context_engine.rs 两处路径改 + 对应纯函数测试随迁
2. **批 2**：`TurnState` 封装 9 个 flag（行为不变替换）
3. **批 3**：提取 injections（reminder 重建 + 注入）+ retry 包装（chat_with_retry）
4. **批 4**：提取 finalize + tool_phase（run_inner 降至骨架）
5. **批 5**：测试全量迁移归位（fixture 抽 tests.rs）+ 全量验证

### 4.5 顺序语义红线（提取时语句顺序逐句保持）

- L865–873：tool result 先落盘再走异步通知——刻意设计
- Done 事件先于 persist（L596 注释）——刻意设计
- sessions_yield / loop-breaker / shell-pending 三处「剥离剩余 tool_calls」逻辑各自
  独立，**不做合并**（合并属行为变更）
- issue #140 has_pending 语义、单 preview 等已确认行为一律不动

### 4.6 拆分质量闸（Ousterhout deep-module 判据，防止假拆分）

方案确认时与 review 时各过一遍：

- **深模块检验**：逐新文件列「对外符号数 / 私有实现行数」。接口窄、实现厚为合格；
  文件主体是转发/重新导出 = shallow 红旗（mod.rs 命名空间壳豁免）
- **信息隐藏检验**：TurnState 至少把不变式（has_pending 派生等）收进方法。9 字段裸袋
  + `&mut` 传遍子模块 = 只搬位置，依赖未减
- **变更放大 A/B**：取 #144 类代表性改动，数拆分前/后需 touch 的文件数——拆分的
  意义在该数字下降，不降反升即失败
- 不适用条款（define errors out of existence / pull complexity down / 命名注释）
  属行为级重构准则，纯移动拆分不引入

## 5. 验收标准

- [ ] CI 编译零新增警告、全量测试通过（本批 CI 绿后在 PR 勾选）
- [x] `run_inner` 本体 ≤ 250 行；单文件 ≤ 500 行（tests.rs 除外）
- [x] §4.6 三闸全过（逐文件符号表 + A/B 实测数进 PR 描述）
- [x] git diff 路径仅 `src/agents/agent*` + `src/agents/context_engine.rs`
- [x] 公共 API 不变：`agents::Agent` re-export、`run`/`run_recovery` 签名
- [ ] PR 管线部署后 smoke：daemon 启动 + 基本 turn 正常（合并部署阶段勾选）
- [x] mod.rs 处保留一行模块地图注释（11 文件职责一行一个）

## 6. 风险

| 风险 | 缓解 |
|---|---|
| CI 轮次延迟（批量大返工成本高，批量小轮次多） | 5 批折中；批 1 风险最低先行探路管线 |
| TurnState 提取时 `&mut` 交错 borrow 冲突 | 批 2 独立成批，失败可单独回滚 |
| 顺序语义被无意改动 | §4.5 红线逐句核对；review-github-pr 重点审 |
| #144/#141 先合入 | §4.3 开工前核对 + rebase |

## 7. 后续（非本次范围）

- `session_context.rs`（1952 行）同模式拆分：第二个目标，复用本次全部经验
- 拆分完成后按 myclaw-skill-writing 会话蒸馏路径提炼架构/模块拆分 skill：
  正文素材 = 本次执行坑 + RED 基线三失败点（CI-only / worktree 路由 / 冲突窗口）
  + Ousterhout deep-module 判据（拆出模块接口面是否够窄）
