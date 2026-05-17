# MyClaw Prompt Engine 设计方案

> 状态：Draft
> 日期：2026-05-17
> 作者：张小二 & Albert

---

## 一、背景

MyClaw 当前的系统提示词（`src/agents/prompt.rs`）通过硬编码常量拼接生成，存在以下问题：

1. **规则不完整**：缺少"持续执行"、"不要过度修改"、"事实信息必须用工具验证"等关键规则
2. **动态信息污染 System Prompt**：`channel_name`、`timezone_offset` 等运行时信息被注入到 System Prompt 中，破坏了 Prompt Caching
3. **平台格式适配错误地放在提示词中**：Markdown 表格/语法的限制应该由 Channel Adapter 后处理，不应该让 LLM 来控制
4. **Cron/子 Agent 行为区分不足**：Cron 后台任务和子 Agent 都没有 `ask_user` 工具，但期望行为完全不同
5. **提示词没有教模型如何使用记忆**：导致模型经常把临时性内容写入记忆文件

---

## 二、竞品分析总结

### 对比项目

| 项目 | 定位 | 提示词长度 | 核心特点 |
|------|------|----------|---------|
| Claude Code | 编码助手 | ~4000字 | 极度细致的行为规则、缓存防污染、工具优先级 |
| Codex CLI | 编码助手 | ~3000字 | 人格驱动、丰富的正面/反面示例、"持续执行直到完成" |
| OpenClaw | 全能助手 | 通过文件注入 | AGENTS.md 分层注入、POST-compaction 关键规则重注入 |
| Hermes Agent | 全能助手 | ~2500字基础 | Prompt 注入扫描、模型特定引导、Cron 专用提示词 |

### 关键借鉴点

| 借鉴 | 来源 | MyClaw 现状 |
|------|------|------------|
| "持续执行直到任务完成" | Codex | 缺失。模型经常半途而废 |
| "不要画蛇添足" | Claude Code | 缺失。模型修 bug 时经常顺手重构 |
| "事实信息必须用工具验证" | Hermes | 缺失。模型凭记忆回答时间/磁盘空间等 |
| "工具使用优先级" | Claude Code | 缺失。模型用 `shell("cat")` 而不是 `file_read` |
| "记忆写入指南" | Hermes | 缺失。模型把临时内容写入记忆 |
| "读后再改" | Claude Code | 缺失。模型在没读过文件的情况下提议修改 |
| "上下文文件注入扫描" | Hermes | 缺失。SOUL.md/AGENTS.md 直接注入无扫描 |
| 缓存静态/动态边界 | Claude Code | 缺失。动态信息混在 System Prompt 中 |

---

## 三、架构原则

### 原则 1：动静分离

- **System Prompt**：100% 静态，定义 Agent 的身份、规则、约束
- **User Message Preamble**：注入动态的物理现场信息（时间、CWD、Git 状态）
- **Channel Adapter**：负责平台格式适配（Markdown 转换）

这样 System Prompt 在整个 Session 生命周期内不变，可以完美触发 Anthropic 等Provider 的 Prompt Caching。

### 原则 2：平台展示不放提示词

LLM 对格式硬限制（"不要输出表格"）的遵循度天然不可靠。正确做法是：
- Agent 统一输出标准 Markdown
- Channel Adapter 的发送端代码做物理转换（表格→列表、语法过滤）

### 原则 3：软风格不放提示词

回复的详略由任务复杂度和用户提问方式自然决定，不在提示词中加"请保持简洁"或"请尽量详细"。

### 原则 4：运行模式通过静态规则 + 工具集双重控制

- **工具集物理隔离**：Cron 模式下不注册 `ask_user` 工具
- **静态规则认知引导**：通过 `autonomous_mode` 标志注入不同的行为规则
- **子 Agent 特殊处理**：子 Agent 也没有 `ask_user`，但它应该向父 Agent 汇报而不是自己决策，因此注入 Interactive 规则而非 Autonomous 规则

---

## 四、System Prompt Section 拼接顺序

```
 1. Anti-Narration           （已有，不变）
 2. Tool Honesty             （已有，不变）
 3. Actions                  （已有，按 autonomy level）
 4. Safety                   （已有，按 autonomy level）
 5. Autonomous/Interactive   （新增，按 autonomous_mode 二选一）
 6. Task Persistence         （新增）
 7. Don't Over-Engineer      （新增）
 8. Mandatory Tool Use       （新增）
 9. Tool Priority            （新增）
10. Memory Writing Guide     （新增）
11. Read Before Edit         （新增）
12. System Reminders         （已有，不变）
13. Workspace Bootstrap      （已有，读 SOUL.md/USER.md/AGENTS.md）
14. Runtime                  （已有，移除 channel_name）
```

---

## 五、新增规则详细内容

### Section 5: Autonomous / Interactive Rules（二选一）

根据 `SystemPromptConfig.autonomous_mode` 选择注入。

**Autonomous（`autonomous_mode = true`，适用于 Cron / Webhook）**：

```text
## Running Mode: Autonomous Background

You are running as a background task. There is no active human user to read or reply to your output.
- Never write questions or ask for clarification in your text output.
- If blocked by ambiguity or permissions, make a safe autonomous decision (skip, backup, or fail-fast) and report the outcome.
- Your output will be delivered to the configured target automatically.
```

**Interactive（`autonomous_mode = false`，适用于主 Agent / 子 Agent）**：

```text
## Running Mode: Interactive Session

You are running inside an active session with a user or supervisor.
- If you encounter blockers or critical ambiguity, report your findings and ask for clarification.
- Do not make highly speculative or destructive assumptions without checking first.
```

### Section 6: Task Persistence

合并"持续执行"和"智能切换策略"为一条规则，避免矛盾。

```text
## Task Persistence

Keep working until the task is fully resolved. Do not stop with a plan or summary of what you would do — execute it.
However, if a specific search or lookup approach yields no results after 3 attempts with different queries, do not loop further on that same path. Instead: acknowledge the information is unavailable, switch to a different tool or approach, or proceed with what you have.
```

### Section 7: Don't Over-Engineer

```text
## Don't Over-Engineer

Do not add features, refactor code, or make improvements beyond what was asked. A bug fix does not need surrounding code cleaned up. Three similar lines of code is better than a premature abstraction. Do not add comments, docstrings, or type annotations to code you did not change.
```

### Section 8: Mandatory Tool Use

```text
## Mandatory Tool Use

NEVER answer these from memory or mental computation — ALWAYS use a tool:
- Current time, date, timezone → use shell
- System state (OS, disk, memory, processes, ports) → use shell
- File contents, sizes, line counts → use file_read
- Git history, branches, diffs → use shell
- Current facts (versions, news, weather) → use web search
```

### Section 9: Tool Priority

```text
## Tool Priority

Use dedicated tools over raw shell commands:
- Use file_read instead of shell cat/head/tail
- Use file_edit instead of shell sed/awk
- Use file_write instead of shell echo/cat heredoc
Reserve shell for system commands and operations that have no dedicated tool.
```

### Section 10: Memory Writing Guide

```text
## Memory Writing Guide

Write memories as declarative facts, not instructions.
- "User prefers concise responses" ✓ — "Always respond concisely" ✗
- "Project uses pytest with xdist" ✓ — "Run tests with pytest -n 4" ✗
Do not save task progress, completed-work logs, or temporary TODO state. If a fact will be stale in a week, it does not belong in memory.
```

### Section 11: Read Before Edit

```text
## Read Before Edit

Do not propose changes to code you haven't read. If asked about or modifying a file, read it first.
```

---

## 六、从 System Prompt 中移除的内容

### 1. Channel Capabilities（`build_channel_caps`）

**整段移除**。平台的 Markdown 格式限制改由 Channel Adapter 的后处理代码负责。

### 2. `channel_name` 字段

从 `SystemPromptConfig` 中移除。Channel 信息不再影响 System Prompt 的内容。

---

## 七、代码改动清单

| 文件 | 改动 | 说明 |
|------|------|------|
| `src/agents/prompt.rs` | 增加 7 个静态常量；`SystemPromptConfig` 增加 `autonomous_mode: bool`；`build()` 中拼接新 Section；移除 `build_channel_caps()` | 核心改动 |
| `src/agents/session_manager.rs` | `SessionOverride` 增加 `autonomous_mode: Option<bool>` | 传递机制 |
| `src/agents/agent_impl/mod.rs` | `AgentConfig::with_override` 支持覆盖 `autonomous_mode` | 配置合并 |
| `src/agents/orchestrator.rs` | `get_or_create_scheduled_loop` 中设置 `autonomous_mode = Some(true)` | Cron/Heartbeat 标记为自治 |
| `src/agents/scheduling/scheduler.rs` | Webhook 的 `get_or_create_loop` 中设置 `autonomous_mode = Some(true)` | Webhook 标记为自治 |
| `src/agents/sub_agent.rs` | 导入并注入 `SECTION_INTERACTIVE_RULES` | 子 Agent 明确为交互模式 |
| `src/daemon.rs` | `build_prompt_config` 补充 `autonomous_mode: false`；移除 `channel_name` | 主 Agent 配置 |

---

## 八、后续可选改进（不在本次范围内）

| 改进 | 参考来源 | 优先级 |
|------|---------|-------|
| 上下文文件注入前扫描 Prompt Injection | Hermes | P1 |
| Compaction 总结使用 `<analysis>` + `<summary>` 双阶段生成 | Claude Code | P1 |
| Prompt Cache 破裂检测与 Diff 报警 | Claude Code | P2 |
| User Message Preamble 标准化（时间/CWD/Git 状态） | Claude Code | P1 |
