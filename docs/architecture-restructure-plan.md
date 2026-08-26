# MyClaw 架构重构方案（决策层）

> 状态：方案待审（已根据评审意见修正）。批准后按迁移序列执行，每步独立 PR，可单独回滚。
> 基准：origin/master @ 7cf5f9c（含 #151 分析修正 + 传播修正）。
> 关联：#151（分析）、#146（agent.rs 拆分待合并）、#144/#141（修复，delegation 整合应在 #144 修复后做）。

## 1. 目标分层（可断言化）

### 1.1 分层定义

MyClaw 单 crate 内逻辑分层（无编译器边界，靠依赖方向断言维持）：

| 层 | 模块 | 允许依赖 | 断言 |
|---|---|---|---|
| **L0 契约层** | api（新建） | 零 `use crate::` | `grep -r "use crate::" src/api/` 输出空 |
| **L1 基础层** | ids, config, str_utils, storage（纯存储部分）, scheduling_types | L0 + 基础内部 | `grep -r "use crate::" src/{ids,config,str_utils,scheduling_types}/` 仅匹配 L0/L1 模块 |
| **L2 服务层** | providers, memory, identity | L0 + L1 | `grep -r "use crate::" src/{providers,memory,identity}/` 仅匹配 L0/L1 |
| **L3 工具层** | tools | L0 + L1 + L2（不引 L4/L5） | `grep -r "use crate::" src/tools/` 不匹配 agents/scheduling/commands/channels |
| **L4 运行时层** | agents（runtime 核心）, scheduling_runtime, commands | L0-L3 | `grep -r "use crate::" src/{agents,scheduling_runtime,commands}/` 不匹配 channels/daemon |
| **L5 渠道层** | channels | L0-L4（顶层驱动） | `grep -r "use crate::" src/channels/` 仅匹配 L0-L4 |
| **L6 组合根** | daemon, cli, webui | 全引 | 唯一合法的"全知"点 |

### 1.2 断言脚本

`scripts/verify-layering.sh`：

```bash
#!/bin/bash
# 验证分层合规性，退出码 0=合规，非 0=违规

set -e

violations=0

# L0 契约层：零 use crate::
if grep -rq "use crate::" src/api/ 2>/dev/null; then
  echo "❌ L0 api 层违规引用："
  grep -rn "use crate::" src/api/
  violations=$((violations + 1))
fi

# L1 基础层：仅引 L0 + 基础内部
for mod in ids config str_utils scheduling_types; do
  if grep -rq "use crate::\(providers\|memory\|identity\|tools\|agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/$mod/ 2>/dev/null; then
    echo "❌ L1 $mod 违规引用："
    grep -rn "use crate::\(providers\|memory\|identity\|tools\|agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/$mod/
    violations=$((violations + 1))
  fi
done

# L2 服务层：仅引 L0 + L1
for mod in providers memory identity; do
  if grep -rq "use crate::\(tools\|agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/$mod/ 2>/dev/null; then
    echo "❌ L2 $mod 违规引用："
    grep -rn "use crate::\(tools\|agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/$mod/
    violations=$((violations + 1))
  fi
done

# L3 工具层：不引 L4/L5
if grep -rq "use crate::\(agents\|scheduling_runtime\|commands\|channels\)" src/tools/ 2>/dev/null; then
  echo "❌ L3 tools 违规引用 L4/L5："
  grep -rn "use crate::\(agents\|scheduling_runtime\|commands\|channels\)" src/tools/
  violations=$((violations + 1))
fi

# L4 运行时层：不引 L5/L6
for mod in agents scheduling_runtime commands; do
  if grep -rq "use crate::\(channels\|daemon\|cli\|webui\)" src/$mod/ 2>/dev/null; then
    echo "❌ L4 $mod 违规引用 L5/L6："
    grep -rn "use crate::\(channels\|daemon\|cli\|webui\)" src/$mod/
    violations=$((violations + 1))
  fi
done

# L5 渠道层：仅引 L0-L4
if grep -rq "use crate::\(daemon\|cli\|webui\)" src/channels/ 2>/dev/null; then
  echo "❌ L5 channels 违规引用 L6："
  grep -rn "use crate::\(daemon\|cli\|webui\)" src/channels/
  violations=$((violations + 1))
fi

# L2 mcp：不引 L4/L5/L6
if grep -rq "use crate::\(agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/mcp/ 2>/dev/null; then
  echo "❌ L2 mcp 违规引用 L4+："
  grep -rn "use crate::\(agents\|scheduling\|commands\|channels\|daemon\|cli\|webui\)" src/mcp/
  violations=$((violations + 1))
fi

if [ $violations -eq 0 ]; then
  echo "✅ 分层合规"
  exit 0
else
  echo "❌ $violations 处违规"
  exit 1
fi
```

### 1.3 竞品参照落点

| 竞品 | 借鉴点 | MyClaw 落点 |
|---|---|---|
| zeroclaw | api 零依赖契约层 + tools 不知 runtime | L0 api 层 + L3 tools 不引 L4 |
| codex | 契约独立成 crate | L0 api 层（单 crate 内逻辑隔离） |
| openclaw | context-engine 顶层目录 | L4 agents 内 context_engine 独立子目录 |
| claude-code | 12 级渐进 harness 阶梯 | L4 agents 按管线阶段分层（turn_state/tool_phase/run_recovery 等，#146 已拆） |
| grok-build | 过度拆分反面 | 不以文件数增加为目标函数 |

## 2. 全量处置表（模块级）

> 218 文件太多，先做 21 模块级处置，关键文件级细化见 §3。

| 模块 | 行数 | 现状 | 处置 | 去向 | 理由 |
|---|---:|---|---|---|---|
| api | 0 | 不存在 | **新建** | L0 | 契约层：Tool/Session/Channel/MessageSender/MessageReceiver trait + 类型 + LoopBreakerConfig |
| ids | 209 | 底层 | **留** | L1 | 教科书式底层（被 13 文件引用零反向） |
| config | 2780 | 顶层 | **留** | L1 | 配置面该顶层；LoopBreakerConfig 移到 api |
| str_utils | 477 | 顶层 | **留** | L1 | 纯工具；YAML frontmatter 部分随 skill 域走 |
| storage | 3477 | 顶层 | **部分留** | L1 | 纯存储留；completion_queue/inbound_spool 迁 L4 orchestrator |
| providers | 10321 | 顶层 | **留** | L2 | 厂商接入 + capability trait；微文件合并 |
| memory | 775 | 顶层 | **留** | L2 | 纯文件 wiki，零内部依赖 |
| identity | 0 | 不存在 | **新建** | L2 | 从 agents 迁出：known_users/user_registry/user_profile/user_messages（3063 行），tools 和 agents 都消费 |
| scheduling_types | 0 | 不存在 | **新建** | L1 | 从 agents/scheduling 迁出：cron_types.rs（291 行纯类型），tools 和 runtime 都消费 |
| tools | 16784 | 顶层 | **留** | L3 | 内置工具实现；Tool trait 移到 api |
| agents | 38157 | 顶层 | **拆** | L4 | 借居域迁出（identity/scheduling/commands） |
| scheduling_runtime | 0 | 不存在 | **新建** | L4 | 从 agents 迁出：scheduler.rs 等运行时（4291 行），需回调 agent turn |
| commands | 0 | 不存在 | **新建** | L4 | 从 agents 迁出：commands/（2433 行） |
| channels | 15597 | 顶层 | **留** | L5 | 渠道适配层；client.rs 迁 webui；Channel trait 迁 api |
| webui | 0 | 不存在 | **新建** | L6 | 从 channels 迁出：client.rs（2852 行 WebUI API 后端） |
| registry | 768 | 顶层 | **并入 providers** | L2 | 实为 providers facade（26 处 providers 引用），消假分层边 |
| mcp | 2923 | 顶层 | **留** | L2 | MCP server 生命周期；mcp→agents::session 违规在 Phase 1.5 修复 |
| cli | 1826 | 顶层 | **留** | L6 | bin 侧；重复组合根逻辑与 daemon 对齐 |
| daemon | 2024 | 顶层 | **留** | L6 | 合法组合根但超载：run() 1005 行拆 builder；热切换/sd_notify 下沉 lifecycle 模块 |
| migration | 1619 | 顶层 | **留** | L6 | plan-based 迁移引擎，唯一耦合 ids，干净 |
| tui | 559 | 顶层 | **留** | L6 | 最干净模块，零内部依赖 |
| hot_switch/update_state/signal/sys_info | ~673 | 顶层 | **留** | L6 | 小工具，daemon+cli 双端共用 |

**处置汇总**：
- 新建 5 模块：api（L0）、identity（L2）、scheduling_types（L1）、scheduling_runtime/commands（L4）、webui（L6）
- 拆 1 模块：agents（借居域迁出）
- 并入 1 模块：registry→providers
- 留 15 模块（部分内部调整）

## 3. 关键文件级处置

> 巨文件（≥1400 行）+ 借居域文件 + 错位文件。

### 3.1 agents 借居域迁出

| 文件 | 行数 | 去向 | 理由 |
|---|---:|---|---|
| agents/known_users.rs | 1496 | identity/known_users.rs | 身份域，零内聚（簇内互引 ~3） |
| agents/user_registry.rs | 845 | identity/user_registry.rs | 身份域 |
| agents/user_profile.rs | 368 | identity/user_profile.rs | 身份域 |
| agents/user_messages.rs | 354 | identity/user_messages.rs | 身份域文案 |
| agents/scheduling/* | 4582 | scheduling/* | 调度域，与 agent 循环仅一根线相连 |
| agents/commands/* | 2433 | commands/* | 渠道接口逻辑 |

### 3.2 channels 错位迁出

| 文件 | 行数 | 去向 | 理由 |
|---|---:|---|---|
| channels/client.rs | 2852 | webui/client.rs | WebUI API 后端，非渠道 |

### 3.3 storage 编排域迁出

| 文件 | 行数 | 去向 | 理由 |
|---|---:|---|---|
| storage/completion_queue.rs | 329 | agents/orchestrator/completion_queue.rs | 语义属 orchestrator（delegation 通知） |
| storage/inbound_spool.rs | 556 | agents/orchestrator/inbound_spool.rs | 语义属 orchestrator（渠道 at-least-once） |

### 3.4 providers 合并组

| 合并组 | 文件 | 行数 | 去向 | 理由（CCP/深模块判据） |
|---|---|---:|---|---|
| 微厂商 | anthropic.rs(53)/deepseek.rs(56)/qwen.rs(54)/kimi.rs(56) | 219 | providers/vendor_overrides.rs | 唯一调用方 provider_factory，合并后对外符号数 < 各部分之和 |
| 微 capability trait | embedding(31)/stt(39)/tts(43)/video(45)/image(52) | 210 | providers/capability_media.rs | 单 trait + 少量 DTO，合并后内聚 |
| 共享工具 | shared.rs(181)+http.rs(20) | 201 | providers/infra.rs | 同域"厂商共享工具" |

### 3.5 channels 共享层上提

| 重复职责 | 行数 | 去向 | 理由 |
|---|---:|---|---|
| typing keepalive 三份 | ~170 | channels/shared/typing.rs | 骨架同构，仅 API 端点不同 |
| 入站 debounce 三份三样 | ~220 | channels/shared/debounce.rs | 统一为 qqbot 的 struct 风格 |
| 退避/重连骨架三份 | ~90 | channels/shared/backoff.rs | 可共享 |
| 发送管线同构骨架 | ~200 | channels/shared/pipeline.rs | debounce→转换→分块→媒体→限流模板方法 |

### 3.6 巨文件拆分

| 文件 | 行数 | 处置 | 理由 |
|---|---:|---|---|
| scheduler.rs | 3849 | 拆 webhook 子系统独立 scheduling/webhook.rs | 三块边界清晰（webhook/cron ticking/job CRUD） |
| telegram/channel.rs | 3589 | 拆 TurnStream+限流 | 测试占 628 行，业务 2961 行 |
| qqbot/channel.rs | 3195 | 拆限流/防抖/重连 | 已拆 keyboard/token/markdown_sanitize，继续拆 |
| delegation_coordinator.rs | 2734 | 拆 checkpoint 组/worktree 组 | delegate_with_parent 589 行巨函数 |
| wechat.rs | 2245 | 目录化（加密/API/渠道） | 单文件含三职责 |
| daemon.rs | 2024 | 拆 builder + 生命周期下沉 | run() 1005 行、build_tools 14 参数 |
| tools/shell.rs | 2012 | 拆 ProcEntry 持久化 | 三职责（持久化/reaper/收养） |
| context_engine.rs | 1993 | **按算法族拆 impl 不拆文件** | 无巨函数（top 210），单 impl 50 函数 |
| session_context.rs | 1952 | 拆 TTS 组+挂起状态机组 | process_turn 657 行巨函数 |
| channels/message.rs | 1950 | 拆 model.rs + chunking.rs | 半数是通用分块算法（~860 行） |

### 3.7 改名消歧

| 现状 | 改为 | 理由 |
|---|---|---|
| context_engine.rs | compaction_engine.rs | 与 session_context.rs 命名碰撞（两个"Context"巨型异物） |
| agents/recovery.rs | agents/startup_recovery.rs | 三 recovery 同名三义（startup/turn/breakpoint） |
| orchestrator/recovery.rs | orchestrator/turn_recovery.rs | 同上 |
| session/recovery.rs | session/breakpoint_detect.rs | 同上 |

## 4. 迁移序列（拓扑序）

> 每步独立 PR，可单独回滚。冲突窗口标注。

### Phase 0：零成本常量搬迁（1 PR）

1. **搬 llm_stream 常量到 providers**：`REQUEST_SEND_TIMEOUT`/`ERROR_BODY_TIMEOUT` 从 `agents/llm_stream.rs` 移到 `providers/shared.rs`
2. **消 4 文件 inline 引用**：protocols 4 文件 `crate::agents::llm_stream::X` → `crate::providers::shared::X`
3. **验收**：`verify-layering.sh` L2 合规；CI 全绿

**冲突窗口**：无。与 #144/#146/#147-149 无依赖。

### Phase 1：Tool trait 宿主错位修复（1 PR）

1. **新建 api 模块**：`src/api/mod.rs` + `src/api/tool.rs`（Tool/ToolResult/ToolSpec/ToolSource trait）
2. **收窄 Session 为 ToolContext**：`api/tool.rs` 定义 `ToolContext { owner, session_id, reply_target, last_message }` 值对象
3. **改 Tool trait 签名**：`execute(&self, args, ctx: &ToolContext)` 替代 `session: &Session`
4. **改 29 文件工具实现**：A 类 8 文件纯签名改；B 类 ~19 文件从 Session 读字段改为从 ToolContext 读
5. **daemon 构造 ToolContext**：daemon.rs build_tools 时构造 ToolContext 注入
6. **验收**：`verify-layering.sh` L0/L2/L3 合规；tools→agents Session 依赖归零；CI 全绿

**冲突窗口**：无。与 #144/#146/#147-149 无依赖。

### Phase 1.5：消息契约迁移（1 PR）

1. **移动 Channel trait + 消息类型到 api**：`channels/message.rs` 中的 `Channel` trait、`MessageSender`、`MessageReceiver`、`ChannelInboundMessage`、`ChannelOutboundMessage` 等移到 `api/channel.rs` + `api/message.rs`
2. **更新所有引用路径**：agents/channels/tools/storage 等模块的 `use crate::channels::Channel` 改为 `use crate::api::Channel`
3. **channels/message.rs 保留**：分块算法（`split_message_chunk` 等）留在 channels，作为共享文本引擎
4. **验收**：`verify-layering.sh` L5 合规；agents→channels 归零（除实现 Channel trait 的文件）；CI 全绿

**冲突窗口**：无。Phase 1 之后、Phase 2 之前。

### Phase 1.6：mcp→agents 违规修复（已随 Phase 1.5 完成，无需独立 PR）

1. **收窄 mcp 对 agents::session 的依赖**：`mcp/tool.rs:71` 和 `mcp/deferred.rs` 的 `use crate::agents::session::Session` 改为使用 api 层的 Session trait 或 ToolContext
2. **验收**：`verify-layering.sh` L2 合规；mcp→agents 归零；CI 全绿

**执行记录**：Phase 1+1.5（PR #153）签名迁移时已同步完成——`mcp/tool.rs:71` 已是 `_ctx: &crate::api::tool::ToolContext`，`src/mcp/` 全目录零 agents 引用（2026-08-25 复核确认）。验收三项全部达标。

**冲突窗口**：无。

### Phase 1.7：LoopBreakerConfig 下沉（1 PR）

1. **移动 LoopBreakerConfig 到 api**：`LoopBreakerConfig`（原定义于 `agents/loop_breaker.rs:50`，方案初稿误记为 `config/loop_breaker.rs`）移到 `api/loop_breaker.rs`；运行时状态机 `LoopBreaker`/`LoopBreakerCounter` 留在 agents
2. **更新引用路径**：`config/mod.rs:79`、`cli/cmd_chat.rs`、`cli/cmd_exec.rs`、`daemon.rs` 改为 `use crate::api::loop_breaker::LoopBreakerConfig`；`agents/loop_breaker.rs` 保留 `pub use` 兼容 re-export
3. **验收**：`verify-layering.sh` L1 合规；config→agents 归零；CI 全绿

**冲突窗口**：无。

### Phase 2：agents 借居域迁出（4 PR）

2a. **identity 域迁出到 L2**（1 PR）
- 新建 `src/identity/` 模块（L2，和 providers/memory 同级）
- 移动 known_users/user_registry/user_profile/user_messages（3063 行）
- 更新 agents/commands/tools/daemon 引用路径
- 验收：`verify-layering.sh` L2 合规；agents 行数 < 35000

> **实施记录（PR #158）**：`user_messages.rs` 未迁——其错误映射 `match agents::error::AgentError::LoopBreak`（L4 类型）且消费方全在 agents 内部，强迁即 L2→L4 反向依赖。`format_ts` 随迁下沉 `str_utils`（L1），消除 identity 对 L4 的唯一依赖。实际迁移 2723 行，agents 38263 → **35540**：`< 35000` 阈值原按含 user_messages（354 行）估算，现预计 Phase 2c 后达成（2b 后 35249、2c 后约 30958）。`verify-layering.sh` 已随本 PR 落地并以 `continue-on-error` 接入 CI（早于原定 Phase 11，用于尽早暴露债务）。首跑完整违规基线 = 3 类 42 行，均为存量：
> - **L1 config→providers（4 行）**：`config/provider.rs:11-12`、`config/routing.rs:7`、`config/mod.rs:868`（测试）。处置：Capability trait 入 api 层时改引 L0 自然消除（Phase 3）。
> - **L3 tools→agents（23 行）**：cronjob_tool(5)/friends(3)/skill_tool(3)/skill_manage(2)/agent_kill(2)/skills_list(2)/tool_search(2)/agent_resume(1)/agent_list(1)/send_message(1)/ask_user(1)/delegate(1)。处置：scheduling 部分随 2b/2c，skill/delegation 部分待 tools 收窄（Phase 2d 后）。
> - **L4 agents→channels（15 行）**：orchestrator(6)/session(2)/scheduler(1)/tool_executor(1)/session_context(1)/ask_router(1)/skill_extract(1)/commands/friends(1)。处置：Channel trait 及消息类型完整迁 api（Phase 1.5 遗留，Phase 3）。
> identity 自身零违规（仅引 ids/config/str_utils + 层内互引）。

2b. **scheduling_types 迁出到 L1**（1 PR）
- 新建 `src/scheduling_types/` 模块（L1）
- 移动 `agents/scheduling/cron_types.rs`（291 行纯类型）
- 更新 tools/cronjob_tool.rs 引用路径（从 `agents::scheduling::cron_types` 改为 `scheduling_types`）
- 验收：`verify-layering.sh` L1 合规；tools→agents scheduling 依赖归零

2c. **scheduling_runtime 迁出到 L4**（1 PR）
- 新建 `src/scheduling_runtime/` 模块（L4）
- 移动 scheduler.rs/work_unit.rs/cron_loader.rs（4291 行运行时）
- 更新 daemon 引用路径
- 验收：`verify-layering.sh` L4 合规；agents 行数 < 31000

> **实施记录（PR #160）**：实际 4290 行，agents 35248 → 30953（达标）。清单外必要改动：`agents/mod.rs` 的 `mod orchestrator` → `pub(crate) mod orchestrator`（scheduler 引用其 6+ 符号，迁出后私有不可见）。**已知遗留（评审发现，verify-layering.sh 漏检）**：`scheduling_runtime → agents::orchestrator`（SchedulerEvent/CronTrigger/run_scheduled_turn/OrchestratorCtx/is_silent_ok 等）与 `agents → scheduling_runtime`（mod.rs re-export、context_engine 用 work_unit）双向并存，构成同层两模块循环依赖（SCC）。脚本只查层间单向违规，不查同层模块互引。处置方向（Phase 3 / §5.1 解环时）：把 scheduler 所需符号下沉更底层，或 scheduling_runtime 定义回调 trait 由 agents 实现（方向反转）。Phase 11 layering 转 strict 前须先解此环，否则门禁无出处可查。

2d. **commands 域迁出到 L4**（1 PR）
- 新建 `src/commands/` 模块（L4）
- 移动 commands/（2433 行）
- 更新 agents/channels 引用路径
- 验收：`verify-layering.sh` L4 合规；agents 行数 < 29000

> **实施记录（PR #161）**：实际 2422 行（9 文件），agents 30953 → 28530（达标）。11 处路径改写（域内 6 + inbound 1 + client.rs 1 + tools 3），域内 `super::` 相对引用零改动。零可见性放宽、零新增跨模块耦合（commands→agents 纯单向，评审核实"四个 PR 里唯一没引入新循环依赖的一次"）。

> **Phase 2 总结（2026-08-26 完结，4 PR 全合并）**：agents 38263 → 28530（**-9733 行 / -25.4%**）。
> - 2a `#158`：identity → L2（known_users/user_registry/user_profile，2723 行；user_messages 留驻因 AgentError 依赖；format_ts 下沉 str_utils）
> - 2b `#159`：scheduling_types → L1（cron_types，291 行 100% rename）
> - 2c `#160`：scheduling_runtime → L4（scheduler/work_unit/cron_loader，4290 行；遗留 agents↔scheduling_runtime SCC，见 2c 实施记录）
> - 2d `#161`：commands → L4（9 文件 2422 行，零遗留）
> 借居域归位后 agents 仅存自身核心（agent loop/session/orchestrator/prompt/delegation 等）。分层违规基线（Phase 3 起点）：L1 config→providers 4 行、L3 tools→L4 22 行、L4→channels 15 行（agents 13 + scheduling_runtime 1 + commands 1），均存量守恒。

**冲突窗口**：
- **#144（delegation bug）**：应在 #144 修复后做 2a/2b/2c（先修 bug 后搬家）
- **#146（agent.rs 拆分）**：待合并，agent/ 目录已拆，与 2a/2b/2c 无文件交集

### Phase 3：channels 错位迁出（1 PR）

1. **新建 webui 模块**：`src/webui/mod.rs`
2. **移动 client.rs**：`channels/client.rs` → `webui/client.rs`（2852 行）
3. **更新 daemon 引用路径**
4. **验收**：`verify-layering.sh` L5 合规；channels→agents 归零（除 TurnEvent）

**冲突窗口**：无。

> **实施记录（3a = PR #163，3b = PR #164）**：原计划 1 PR，实际按风险拆为 3a/3b 两个纯搬家 PR，后续 3c/3d。
> - **3a `#163`**：webui → L6（`channels/client.rs` → `src/webui/client.rs`，2852 行 100% rename；channels 15447 → 12591）。channels 侧**不做** re-export（channels→webui 是 L5→L6 反向依赖，实测脚本立即报违规），消费方仅 daemon 2 处直接改。
> - **3b `#164`**：Channel 契约六组下沉 api——TurnEvent/VersionedEvent（git mv 自 agents，241 行零改动）、RunMode（自 config/agent.rs 剪出，config 侧 re-export）、TurnStream/FoldCandidate、ChannelSecurityPolicy+AllowList+GroupAuthMode、ChannelInboundMessage、Channel trait+CallbackAction。31 文件 +492/−471，api 504 → 1222。channels 侧全符号 re-export；L4 侧 15 处 use + 67 处全限定改引 api，**L4→channels 归零**（评审核实本地复现）。遗留（PR 描述如实披露）：api 内 11 处 `crate::channels::` 全限定引用（trait 默认方法引用未下沉契约类型族：ProcessingStatus/ToolEvent/ChannelCapabilities+MINIMAL_CAPABILITIES+LenUnit/MessageScope/AuthDecision/GroupStat/security::evaluate），脚本只匹配 `use crate::` 前缀漏检——**处置归 3c**（下沉该批类型，或加严脚本 L0 检查至全路径匹配使债务显性化）。
> - **SCC 解环归属澄清（本轮评审指出）**：#163 PR 描述"后续"曾把 agents↔scheduling_runtime SCC 解环一并列入 3b 范围，3b 实际未做（行为性改动不混入纯搬家 PR，见本节开头原则）。该 SCC（2c 遗留，见 §2c 实施记录）在此明确归属 **3d**：SchedulerEvent/CronTrigger 下沉 + scheduling_runtime 定义回调 trait 由 agents 实现（run_scheduled_turn/OrchestratorCtx/is_silent_ok 方向反转），独立 PR。Phase 11 layering 转 strict 前必须完成，否则门禁无出处可查。
> - Phase 3 后续序列：**3c** = Capability trait 入 api（消 L1 config→providers 4 行：provider.rs:11-12、routing.rs:7、mod.rs:868）+ 3b 残留处置；**3d** = SCC 解环。
>
> **实施记录（3c = PR #165）**：三件事一 PR。① `providers/capability.rs` 整体 git mv → `api/capability.rs`（156 行纯类型，serde+fmt 零 crate 依赖），RotationStrategy 随迁（自 credential_pool.rs 剪出，config 唯一 L1→L2 违规点）；providers 侧留 re-export shim（8 类型 + RotationStrategy），`providers::capability::*` 与 `providers::RotationStrategy` 路径零改动。② 3b 残留 9 类型 + 1 函数下沉：LenUnit/ChannelCapabilities+MINIMAL_CAPABILITIES/ProcessingStatus/ToolEvent/GroupStat → api/message.rs；MessageScope/AuthDecision/evaluate（+全部鉴权测试）→ api/security.rs；channels 侧 re-export 保持，`warn_if_locked_down` 留驻（依赖 &dyn Channel）。api 内 11 处 `crate::channels::` 全限定引用全部本地化，**api 成为纯 L0**。③ verify-layering.sh L0 检查加严为全路径匹配（`crate::` 任意位置，不再只看 use 行）——封闭 2c SCC / 3b 残留两次踩中的同类盲区。验收：L1 config 4 行归零，加严后 L0 通过；L3 tools 22 行存量不变（Phase 2d 后待处置）。
> **3c 评审补丁（同 PR 第二 commit）**：评审发现全路径盲区第 3 次出现（config/mod.rs:1003 测试代码 `crate::providers::Capability::Chat`，L1 检查仍是 use 行口径漏检），并建议全层加严。落实：① mod.rs:1003 去前缀（作用域已有 import）；② **L1–L5 检查全部加严为全路径匹配**；③ 预检比评审多找出 4 处——provider.rs:49/52/53 `crate::providers::AuthStyle`（From impl 桥）顺手下沉 AuthStyle 入 api/capability.rs（2 变体纯枚举，与 RotationStrategy 同构），L1 全路径归零；agents/agent.rs:771/895 `crate::channels::message::ToolEvent`（3b 盲区漏掉的真实 L4→L5 违规）改引 api，L4 全路径保持零。全层加严后 L3 从 22 行显性化为 39 行（+17 处函数体全限定：shell/send_message/friends/delegate 等，均为存量真实债务；首报 37 经评审实测订正为 39，处置归属后续 Phase）。
> **实施记录（3d = PR #166）**：agents↔scheduling_runtime SCC 解环（2c 遗留，行为性改动独立 PR）。消 scheduling_runtime→agents 全部 7 边，agents→scheduling_runtime 保留单向（合法）：① SchedulerEvent/CronTrigger 下沉 `scheduling_types/event.rs`（L1，消费方 orchestrator/daemon 经 re-export 路径零改动）；② **OrchestratorHook 回调 trait 方向反转**——scheduling_runtime 定义（run_scheduled_turn + outbound_channel 两方法）、OrchestratorCtx 实现（ctx.rs 薄委托）、daemon 装配注入 WebhookContext.hook；record_webhook_run 改用 WebhookContext 自有 scheduler 字段（同一 Arc 实例，行为等价）；③ tokens.rs 整体 git mv → providers/tokens.rs（352 行纯函数，agents 侧 re-export shim，work_unit 改引 providers）；④ is_silent_ok 就地搬入 scheduler.rs 测试模块（唯一调用者），3 个 webhook 测试改用本地 MockChannel+TestHook（不再依赖 agents::orchestrator::test_support）。运行时行为零变化（同一函数同一数据流，仅依赖箭头反转）。验收：`grep crate::agents src/scheduling_runtime/` 归零；脚本仍只 L3 39 行（未新增违规）；Phase 11 layering 转 strict 的最后前置障碍解除。

### Phase 4：registry 并入 providers（1 PR）

1. **移动 registry 文件**：`registry/mod.rs` + `registry/routing.rs` → `providers/registry/`
2. **更新 daemon/cli 引用路径**
3. **删除 registry/ 顶层模块**
4. **验收**：`verify-layering.sh` L2 合规；模块数 -1

> **实施记录（Phase 4 = 本 PR）**：git mv 整目录零内容改动（routing.rs 0 行变更，mod.rs 仅 1 行 `use self::routing::` 前缀修正）。registry 依赖面即"providers facade"实证：15 处 `crate::providers::*` use + 函数体多处全限定（capability/provider_registry/fallback/media 等），加 `crate::config::provider/routing`（L2→L1 合法）——迁入 providers 后全部同层或合法向下。消费方仅 3 文件 9 处直接改路径（不留 crate::registry shim，消费面小且全部同仓）：lib.rs（`pub mod registry` 删除、`pub use providers::Registry` 保持 `myclaw::Registry` 对外不变）、daemon.rs build_registry ×2、agents/media_e2e_test.rs ×4。providers/mod.rs 加 `pub mod registry` + `pub use registry::Registry`。验收：`grep crate::registry src/` 归零；顶层模块 24→23（−1 达标）；verify-layering.sh 唯一违规仍为 L3 tools 39 行存量（与 #165/#166 基线一致，无新增）。

**冲突窗口**：无。

### Phase 5：storage 编排域迁出（1 PR）

1. **移动 completion_queue.rs**：`storage/completion_queue.rs` → `agents/orchestrator/completion_queue.rs`（329 行）
2. **移动 inbound_spool.rs**：`storage/inbound_spool.rs` → `agents/orchestrator/inbound_spool.rs`（556 行）
3. **更新 storage/agents 引用路径**
4. **验收**：`verify-layering.sh` L1 合规；storage→channels 归零

> **实施记录（Phase 5 = 本 PR）**：两个 git mv（completion_queue.rs 0 行变更；inbound_spool.rs 仅 2 行 use 改写）。**预检发现直接搬会引入新 L4→channels 违规**（inbound_spool 2 处 `crate::channels::`——#164 已清零的类别），5 个所引类型经查全部已下沉 api/message.rs，改指 `crate::api::message::` 本体（channels 侧本就是 re-export，零行为变化）。消费面 22 处全在 agents/orchestrator/ 5 文件（ctx/delegation/mod/recovery/inbound），按 4 符号精确 sed 至 `crate::agents::orchestrator::`（同文件 session 类符号不动）；orchestrator/mod.rs 加 2 个 pub mod + 6 符号 re-export（与 storage/mod.rs 原导出面逐字对齐），删除原 `use crate::storage::InboundSpool`。**顺手清偿**：session.rs/json_file.rs 存量 5 处 `crate::channels::PersistedChannelMessage`（同符号）改指 api 本体，达成方案验收"storage→channels 归零"——否则该验收项不成立。验收：storage 内 crate::channels 归零；layering 仍仅 L3 39 行存量（无新增）；`crate::storage::{CompletionNotice*,InboundSpool,DeliveryState}` 双口径归零。

**冲突窗口**：无。

### Phase 6：recovery 家族统一门面（1 PR）

1. **改名消歧**：
   - `agents/recovery.rs` → `agents/startup_recovery.rs`
   - `orchestrator/recovery.rs` → `orchestrator/turn_recovery.rs`
   - `session/recovery.rs` → `session/breakpoint_detect.rs`
2. **新建 recovery 门面**：`agents/recovery/mod.rs` re-export 三入口
3. **更新 daemon 引用路径**
4. **验收**：`grep -r "recovery" src/` 无歧义；CI 全绿

**冲突窗口**：无。

### Phase 7：context 家族改名（1 PR）

1. **改名**：`agents/context_engine.rs` → `agents/compaction_engine.rs`
2. **更新引用路径**
3. **验收**：`grep -r "ContextEngine" src/` 输出空；CI 全绿

**冲突窗口**：无。

### Phase 8：巨文件拆分（多 PR，按优先级）

8a. **scheduler.rs 拆 webhook**（1 PR）
8b. **telegram/channel.rs 拆 TurnStream+限流**（1 PR）
8c. **qqbot/channel.rs 拆限流/防抖/重连**（1 PR）
8d. **delegation_coordinator.rs 拆 checkpoint/worktree**（1 PR）
8e. **wechat.rs 目录化**（1 PR）
8f. **daemon.rs 拆 builder + lifecycle**（1 PR）
8g. **tools/shell.rs 拆 ProcEntry**（1 PR）
8h. **session_context.rs 拆 TTS+挂起**（1 PR）
8i. **channels/message.rs 拆 model+chunking**（1 PR）

**冲突窗口**：
- **#146（agent.rs 拆分）**：待合并，agent/ 目录已拆，与 8a-8i 无文件交集
- **Phase 2（借居域迁出）**：应在 Phase 2 后做 8a（scheduler.rs 随 scheduling 迁出后拆）

### Phase 9：channels 共享层上提（1 PR）

1. **新建 channels/shared/**：typing.rs + debounce.rs + backoff.rs + pipeline.rs
2. **三渠道改用共享层**：telegram/qqbot/wechat 删重复代码
3. **验收**：三渠道行数各减 ~200；CI 全绿

**冲突窗口**：无。

### Phase 10：providers 合并组（1 PR）

1. **合并微厂商**：anthropic/deepseek/qwen/kimi → vendor_overrides.rs
2. **合并微 capability trait**：embedding/stt/tts/video/image → capability_media.rs
3. **合并共享工具**：shared+http → infra.rs
4. **验收**：providers 文件数 -6；CI 全绿

**冲突窗口**：无。

### Phase 11：CI 门禁接入（1 PR）

1. **提交 verify-layering.sh 到 scripts/**：修复脚本遗漏（mcp 检查）
2. **修改 .github/workflows/build.yml**：在 Check 步骤后加 `./scripts/verify-layering.sh`
3. **验收**：CI 必跑分层检查；新 PR 无法破坏分层

**冲突窗口**：无。所有 Phase 完成后接入，作为长期防腐蚀手段。

## 5. 验收清单

### 5.1 分层合规

- [ ] `verify-layering.sh` 退出码 0（7 模块 SCC 解环）
- [ ] 依赖方向全合规：L0→L1→L2→L3→L4→L5→L6 单向

### 5.2 符号守恒

- [ ] 关键符号 before/after 计数一致（Tool/Session/Channel/MessageReceiver 等）
- [ ] 外部 API 无破坏性变更（re-export 保持）

### 5.3 全局变更放大 A/B

- [ ] 重构前后 90 天 churn 对比：agents 1135 → < 800（借居域迁出后）
- [ ] 巨文件数对比：19 个 ≥1400 行 → < 10 个

### 5.4 CI 全绿

- [ ] 每步 PR Check→Clippy(-D warnings)→Test→Release 全绿
- [ ] 无新增 warning

### 5.5 部署 smoke

- [ ] 每步 PR 合并后部署 smoke：daemon 启动→发消息→工具执行→渠道投递
- [ ] 无回归

## 6. 风险表

| 风险 | 等级 | 缓解 |
|---|---|---|
| agents 借居域迁出（1135 churn） | **高** | 分 3 PR（identity/scheduling/commands），每步独立验收 |
| Tool trait 收窄（29 文件改动） | **中** | Phase 1 独立 PR，先搬常量再改签名 |
| 依赖路径更新遗漏 | **中** | `verify-layering.sh` 自动化检查；CI clippy -D warnings |
| 巨文件拆分引入 bug | **中** | 每步独立 PR + smoke 验收；符号守恒验证 |
| 多 PR 冲突窗口 | **低** | 拓扑序排列；不跨步骤合并 |

## 7. 回滚边界

- **每步独立 PR**：可单独 revert，不影响后续步骤
- **不跨步骤合并**：Phase N 未验收前不开 Phase N+1 PR
- **符号守恒**：每步 PR 验收关键符号计数一致，回滚后恢复
- **CI 全绿**：每步 PR 合并前 CI 全绿，回滚后恢复

## 8. 设计准则映射

### 8.1 Ousterhout（全进）

| 准则 | 落点 |
|---|---|
| dependencies 是分层理论根基 | §1.1 分层定义（依赖方向断言） |
| pull complexity down | L0 契约层（复杂度下沉到基础） |
| define errors out of existence | §3.7 改名消歧（错误定义消除） |
| deep module | §3.4 providers 合并组（合并后对外符号数 < 各部分之和） |
| different layer different abstraction | §1.1 分层定义（每层抽象不同） |
| flat better than hierarchical | §1.1 分层定义（6 层非 10 层） |

### 8.2 Clean Code（精选 4 条）

| 准则 | 落点 |
|---|---|
| DIP（依赖倒置） | §3 Phase 1（Tool trait 收窄 Session→ToolContext） |
| ADP（抽象包原则） | §1.1 分层定义（7 模块 SCC 解环） |
| SDP（稳定依赖原则） | §1.1 分层定义（L0→L1→L2→L3→L4→L5→L6 单向） |
| CCP（共同复用原则） | §3.4 providers 合并组（合并后对外符号数 < 各部分之和） |

### 8.3 竞品参照（6 项）

见 §1.3 竞品参照落点。

---

**方案完成。待用户审阅后按迁移序列执行。**
