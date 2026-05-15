# MyClaw Context Engine 重构方案 (V7 终极演进版)

> 生成日期：2026-05-15
> 状态：最终确认版（合并 V1/V5 + OCP 特殊工具设计 + Same-Role 统一处理 + 8项优化修正）

---

## 一、设计目标

解决 `AgentLoop` 职责过重（20+ 字段）的问题，划清 `Session`、`ContextEngine`、`ToolExecutor` 的边界。

---

## 二、架构层级与数据流

```text
┌─────────────────────────────────────────────────────────────┐
│                          Orchestrator                         │
│  (持有 ResourceProvider，拦截 slash 命令，处理 channel)           │
└─────────────────────┬───────────────────────────────────────┘
                      │ 构造时注入
                      ▼
┌─────────────────────────────────────────────────────────────┐
│                           AgentLoop                            │
│  纯控制流：                                                     │
│  1. session.add_user_text(...)                                │
│  2. engine.refresh_attachments(session)                       │
│  3. let result = engine.build_request(session, cw)            │
│  4. engine.clear_attachments()                                │
│  5. if engine.should_compact(cw):                              │
│       let b = engine.compaction_boundary(session);            │
│       let r = compactor.execute(session, b, model);           │
│       session.apply_compaction(&r);                           │
│       engine.update_for_compaction(r.removed, r.summary);     │
│  6. loop {                                                    │
│       response = registry.provider.call(messages, specs);     │
│       match response:                                         │
│         Text → session.add_assistant_text(text); break;       │
│         ToolCalls →                                            │
│           for call: tool_executor.execute(call, session);     │
│           rebuild → continue loop                             │
│     }                                                         │
└──────────────┬──────────────────────────┬─────────────────────┘
               │                          │
               ▼                          ▼
┌──────────────────────────┐  ┌──────────────────────────┐
│         Session          │  │       ContextEngine      │
│                          │  │                          │
│  纯状态 + 自动持久化       │  │  纯同步变换（无副作用）      │
│  • history               │  │  • build_request()       │
│  • message_ids           │  │  • should_compact()      │
│  • summary_metadata      │  │  • compaction_boundary() │
│  • session_override      │  │  • token_total()         │
│  • persist_backend       │  │  • update_usage()        │
│                          │  │  • update_for_compaction()│
│  方法:                    │  │  • estimate_tokens_range()│
│  • add_*/rollback_to()   │  │  • refresh_attachments() │
│  • add_message()         │  │  • clear_attachments()   │
│  • apply_compaction()    │  └─────────────┬────────────┘
│  • drop_pre_boundary()   │                │
└──────────────────────────┘                ▼
                              ┌──────────────────────────┐
                              │      ResourceProvider    │
                              │  热加载资源容器 (Arc)        │
                              │  • skills (RwLock)       │
                              │  • sub_agents (RwLock)   │
                              │  • mcp_instructions      │
                              │  • prompt_config         │
                              └──────────────────────────┘
┌──────────────────────────┐  ┌──────────────────────────┐
│       ToolExecutor       │  │   CompactionExecutor     │
│     (Arc 共享)            │  │                          │
│                          │  │  registry ──► ServiceReg │
│  • execute(call, session)│  │  resources ► ResourceProv│
│  • build_tool_specs()    │  │  tool_executor ► ToolExec│
│                          │  │                          │
│  SpecialToolHandler:     │  │  • execute(session, b, m)│
│    • AskUserTool         │  │  • empty_result()        │
│    • DelegateTool        │  └──────────────────────────┘
└──────────────────────────┘
```

---

## 三、核心接口定义

### 3.1 ContextEngine — 纯同步变换，不持有 LLM 依赖

```rust
pub struct BuildResult {
    pub messages: Vec<ChatMessage>, // system prompt + merged history + attachments
}

pub trait ContextEngine: Send + Sync {
    /// 1. 纯同步变换：构建最终发给 LLM 的消息列表
    /// - sanitize_history（移除 orphan tool results）
    /// - merge_same_roles（合并连续 user/assistant，tool 不合并）
    /// - inject attachments 为 system-reminder user 消息
    /// 纯 &self，无副作用。调用方负责 clear_attachments。
    fn build_request(&self, session: &Session, context_window: u64) -> BuildResult;

    /// 2. 判断当前 token 是否达到压缩阈值
    fn should_compact(&self, ctx_window: u64) -> bool;

    /// 3. 返回压缩边界（用于 AgentLoop 执行压缩）
    /// 内部自行计算 budget = context_window * threshold - system_prompt_tokens - tool_spec_tokens
    /// tool_spec_tokens 通过 set_tool_spec_tokens() 预设，compaction_boundary 直接读取
    /// 返回的索引保证是安全截断点，不会破坏 tool_call/tool_result 配对
    fn compaction_boundary(&self, session: &Session) -> Option<usize>;

    /// 4. 更新 token 追踪（LLM 响应后）
    fn update_usage(&mut self, usage: &ChatUsage);

    /// 5. 压缩后更新 token 追踪（从 ContextEngine 内部的 TokenTracker 中扣除）
    fn update_for_compaction(&mut self, removed_tokens: u64, summary_tokens: u64);

    /// 6. 估算指定消息范围的 token 数
    fn estimate_tokens_for_range(&self, start: usize, end: usize) -> u64;

    /// 7. 查询 token 统计
    fn token_total(&self) -> u64;
    fn last_usage(&self) -> (u64, u64, u64); // (input, cached, output)

    /// 8. 刷新附件增量状态
    /// IO 失败时返回 Err，由调用方决定忽略或终止当前 turn
    fn refresh_attachments(&mut self, session: &Session) -> anyhow::Result<()>;

    /// 9. 结算 pending attachments（build_request 后调用）
    fn clear_attachments(&mut self);

    /// 10. 设置 tool spec token 估算值（AgentLoop 在 tool specs 变更时调用）
    fn set_tool_spec_tokens(&mut self, tokens: u64);
}
```

**关键变更 vs V6：**
- `compact()` 从 trait 移除。压缩执行（调 LLM）留在 `AgentLoop`，`ContextEngine` 只保留策略判断（`should_compact` / `compaction_boundary`）。
- `build_request` 改为 `&self`，无副作用。`clear_attachments` 显式分离。
- `build_request` 不再返回 `anyhow::Result`，纯变换不可能失败。
- `refresh_attachments` 返回 `Result`，暴露 IO 失败。

### 3.2 Session — 自动持久化，简化 API

```rust
pub trait PersistBackend: Send + Sync {
    // ── 写接口 ──
    fn persist_message(&self, session_id: &str, msg: &ChatMessage) -> Option<i64>;
    fn truncate_messages(&self, session_id: &str, keep: usize);
    fn save_compaction(&self, session_id: &str, summary: &SummaryRecord);
    fn rotate_history(&self, session_id: &str, surviving: &[(i64, ChatMessage)]);
    fn save_token_count(&self, session_id: &str, total: u64);
    fn save_session_override(&self, session_id: &str, override_json: &str);
    fn save_reply_target(&self, session_id: &str, target: &str);

    // ── 读接口 ──
    fn load_messages(&self, session_id: &str) -> Vec<(i64, ChatMessage)>;
    fn load_summary(&self, session_id: &str) -> Option<SummaryMetadata>;
    fn load_session_override(&self, session_id: &str) -> Option<String>;
    fn load_token_count(&self, session_id: &str) -> Option<u64>;
}

pub struct Session {
    pub id: String,
    pub owner: String,
    pub history: Vec<ChatMessage>,
    pub message_ids: Vec<i64>,
    pub compact_version: u32,
    pub summary_metadata: Option<SummaryMetadata>,
    pub last_total_tokens: Option<u64>,
    pub session_override: SessionOverride,
    pub incomplete_turn: bool,
    pub breakpoint_items: Vec<BreakpointItem>,
    pub last_reply_target: Option<String>,
    persist: Option<Arc<dyn PersistBackend>>,
}

impl Session {
    /// 通用添加消息（自动持久化 + message_id 对齐）
    pub fn add_message(&mut self, msg: ChatMessage) {
        let id = self.persist.as_ref()
            .and_then(|p| p.persist_message(&self.id, &msg));
        self.history.push(msg);
        self.message_ids.push(id.unwrap_or(0));
    }

    /// 便捷方法，内部调 add_message
    pub fn add_user_text(&mut self, text: String);
    pub fn add_assistant_text(&mut self, text: String);
    pub fn add_tool_result(&mut self, tool_call_id: String, content: String, is_error: bool);

    /// 回滚到指定长度（自动持久化）
    pub fn rollback_to(&mut self, len: usize) {
        self.history.truncate(len);
        self.message_ids.truncate(len);
        if let Some(ref p) = self.persist {
            p.truncate_messages(&self.id, len);
        }
    }

    /// 应用压缩结果（替换 history + 持久化）
    /// 内部调用 compaction_boundary 确保不破坏 tool_call/tool_result 配对
    pub fn apply_compaction(&mut self, result: &CompactionResult);

    /// 丢弃 boundary 之前的历史（降级方案，不生成摘要）
    pub fn drop_pre_boundary(&mut self, boundary: usize);

    pub fn history(&self) -> &[ChatMessage] { &self.history }

    /// 从持久化后端恢复 session（静态工厂方法）
    pub fn restore(
        id: String,
        owner: String,
        backend: Arc<dyn PersistBackend>,
    ) -> anyhow::Result<Self>;
}
```

**关键变更：**
- 删除 `add_assistant_with_tools(text, tool_calls, thinking)`。thinking block 的 parts 顺序由调用方（AgentLoop）在构造 `ChatMessage` 时处理，Session 只提供通用的 `add_message(msg)`。
- `rollback_to` 内部自动持久化，AgentLoop 中不再手动调 `truncate_messages`。
- **新增 `PersistBackend` 读接口**：`load_messages`, `load_summary`, `load_session_override`, `load_token_count`，支持 Session 恢复。
- **新增 `Session::restore`** 静态工厂方法，替代当前分散在各处的恢复逻辑。

### 3.3 ToolExecutor — OCP 特殊工具抽象

```rust
#[async_trait::async_trait]
pub trait SpecialToolHandler: Send + Sync {
    fn name(&self) -> &str;
    async fn execute(&self, call: &ToolCall, session: &mut Session) -> anyhow::Result<ToolResult>;
}

pub struct DefaultToolExecutor {
    tools: Arc<ToolRegistry>,
    special_handlers: HashMap<String, Box<dyn SpecialToolHandler>>,
    timeout_secs: u64,
}

impl DefaultToolExecutor {
    pub fn register(&mut self, handler: Box<dyn SpecialToolHandler>);

    /// autonomy 检查由调用方（AgentLoop）在调用前完成
    pub async fn execute(&self, call: &ToolCall, session: &mut Session) -> anyhow::Result<ToolResult>;

    /// 根据 autonomy 状态过滤工具列表
    /// autonomy=false 时，过滤掉标记为 autonomy_only 的特殊工具（如 ask_user）
    pub fn build_tool_specs(&self, autonomy: bool) -> Vec<ToolSpec>;
}
```

**关键变更：**
- `autonomy` 检查移到 `AgentLoop`（调用 `tool_executor.execute()` 之前）。`ToolExecutor` 不感知 autonomy 概念，职责更单一。
- 通过 `SpecialToolHandler` trait 注册 `ask_user` 和 `delegate`，实现开闭原则。
- **`build_tool_specs` 接收 `autonomy: bool`**：由 ToolExecutor 内部过滤 autonomy-only 工具，避免模型看到不可用的工具后浪费交互轮次。

### 3.4 ResourceProvider — 热加载资源容器

```rust
pub struct ResourceProvider {
    pub skills: Arc<RwLock<SkillManager>>,
    pub sub_agents: Arc<RwLock<Vec<SubAgentConfig>>>,
    pub mcp_instructions: Vec<(String, String)>,
    pub prompt_config: SystemPromptConfig,
}
```

### 3.5 CompactionExecutor — 压缩执行封装

`CompactionExecutor` 从 `impl AgentLoop` 中提取为独立结构体，负责「生成摘要」的完整流程（构建 prompt → 调 LLM → 质量审计）。不直接修改 `Session.history`，只返回结果由调用方应用。

```rust
pub struct CompactionResult {
    pub boundary: usize,        // 截断点
    pub summary: String,        // 生成的摘要文本
    pub summary_tokens: u64,    // 摘要 token 估算
    pub removed_tokens: u64,    // 被替换的历史消息 token 数
    pub compacted_count: usize, // 被压缩的消息数量
}

pub struct CompactionExecutor {
    registry: Arc<dyn ServiceRegistry>,
    resources: Arc<ResourceProvider>,
    tool_executor: Arc<dyn ToolExecutor>,
    max_summary_rounds: usize,
    max_summary_tokens: usize,
}

impl CompactionExecutor {
    pub fn new(
        registry: Arc<dyn ServiceRegistry>,
        resources: Arc<ResourceProvider>,
        tool_executor: Arc<dyn ToolExecutor>,
    ) -> Self;

    /// 执行压缩摘要生成
    /// - 构建 compaction prompt（复用 resources 的 system prompt，保证缓存前缀一致）
    /// - 调 LLM 生成摘要（可能多轮，可能调 tool）
    /// - 质量审计（长度、关键信息保留）
    /// - 返回 CompactionResult
    /// 
    /// 注意：接收 &mut Session 是因为 ToolExecutor::execute 需要 &mut Session。
    /// summarize 过程中的 tool 调用只能修改文件系统（memory 操作），不能修改 session.history。
    /// CompactionExecutor 内部只向 summarizer 暴露 memory 相关 tools（file_write/file_edit/file_read/shell）。
    pub async fn execute(
        &self,
        session: &mut Session,  // mut：ToolExecutor 接口要求，但不修改 history
        boundary: usize,
        model_id: &str,
    ) -> anyhow::Result<CompactionResult>;

    /// 降级方案：生成空结果，由调用方直接丢弃历史
    pub fn empty_result(boundary: usize, removed_tokens: u64) -> CompactionResult;
}
```

**设计要点：**
- **不持有 AgentLoop 引用**：依赖注入 `registry` / `resources` / `tool_executor`，彻底解耦。
- **Session 可变但不修改 history**：`execute` 接收 `&mut Session`（因 ToolExecutor 接口要求），但保证不修改 `session.history`。summarize 过程中只暴露 memory 相关 tools（`file_write` / `file_edit` / `file_read` / `shell`），不暴露 `ask_user` / `delegate` 等需要完整 session 状态的工具。
- **system prompt 复用**：通过 `resources` 构建与主 turn 完全一致的 system prompt，保证 provider KV cache 命中。
- **质量审计内置**：`audit_summary_quality` 作为 `CompactionExecutor` 私有方法，不合格时返回 Err，由 AgentLoop 决定降级为 `drop_pre_boundary`。

---

## 四、AgentLoop 精简后（8 个字段）

```rust
pub struct AgentLoop {
    pub(crate) session: Session,
    pub(crate) engine: Box<dyn ContextEngine>,
    pub(crate) tool_executor: Arc<dyn ToolExecutor>,  // Arc 共享（与 compactor 共用同一实例）
    pub(crate) compactor: CompactionExecutor,          // 压缩执行（从 impl AgentLoop 提取）
    pub(crate) registry: Arc<ServiceRegistry>,         // provider 选择 + 注入 compactor
    pub(crate) config: AgentConfig,
    pub(crate) loop_breaker: LoopBreaker,
    pub(crate) pending_retry_message: Option<String>,
}
```

**被移除的字段（共 13 个）：**
- `tools` → 移入 `ToolExecutor`
- `system_prompt` → 移入 `ContextEngine`
- `ask_user_handler` → 移入 `AskUserTool`（注册到 ToolExecutor）
- `delegate_handler` → 移入 `DelegateTool`
- `sub_delegator` → 移入 `DelegateTool`
- `token_tracker` → 移入 `ContextEngine`
- `persist_hook` → 移入 `Session`
- `attachments` → 移入 `ContextEngine`
- `mcp_instructions` → 移入 `ResourceProvider`
- `skills` → 移入 `ResourceProvider`
- `sub_agent_configs` → 移入 `ResourceProvider`
- `skills_dir` / `agents_dir` / `change_rx` → 移入 Orchestrator

**新增/调整：**
- **`compactor: CompactionExecutor`**：从 `impl AgentLoop` 的 732 行 compact 方法中提取为独立结构体。AgentLoop 只负责「何时 compact」的控制流决策，`compactor` 负责「如何生成摘要」的完整执行流程。
- **`tool_executor` 改为 `Arc<dyn ToolExecutor>`**：与 `compactor` 共享同一实例，避免重复构造。构造时 `Arc::new(executor)` 后 clone 给两者。
- **`registry`** 保留在 AgentLoop 中，主要用途变为「provider 选择」和「注入 compactor」。compact 的 LLM 调用由 `compactor` 内部通过注入的 `registry` 完成。

---

## 五、merge_same_roles 规则

`ContextEngine::build_request` 内部执行，在 sanitize 之后：

| 连续角色 | 合并策略 | 说明 |
|---------|---------|------|
| `user` + `user` | 文本用 `\n\n` 拼接，parts 合并 | system-reminder 注入可能导致连续 user |
| `assistant` + `assistant` | 文本拼接，tool_calls 合并到同一个消息 | 异常情况（如 recovery）可能出现 |
| `tool` + `tool` | **不合并** | 每个 tool result 有独立 tool_call_id，合并会破坏配对 |

Provider adapter（Anthropic / OpenAI / GLM）中删除所有 same-role merge 逻辑，直接假设输入已是交替序列。

---

## 六、compaction_boundary 安全截断规则

`ContextEngine::compaction_boundary` 返回的索引 **必须是安全截断点**，不能破坏 `tool_call` / `tool_result` 配对。

### budget 计算

`compaction_boundary` 内部自行计算 budget，不需要外部传入：

```text
budget = context_window * compact_threshold
       - system_prompt_tokens      // 从 ResourceProvider 构建的 system prompt 估算
       - tool_spec_tokens          // 通过 set_tool_spec_tokens() 预设的值
```

- `context_window` 和 `compact_threshold` 从 `DefaultContextEngine` 持有的配置中获取。
- `tool_spec_tokens` 由 AgentLoop 在初始化或 tool specs 变更时调用 `engine.set_tool_spec_tokens(n)` 设置，`compaction_boundary` 直接读取。

### 安全截断点判定

```
消息序列索引：  0    1       2       3    4    5
角色：        user assistant tool assistant user assistant
                                              ↑
                                        budget 允许保留到此处
```

- 若 budget 允许保留到索引 `N`，需检查索引 `N-1` 是否为 `tool` 角色。
- 若 `N-1` 是 `tool`，需向前追溯找到对应的 `tool_call`（在 `assistant` 消息中），将截断点调整到该 `assistant` 消息之前。
- 若调整后截断点 < `first_user`（首次 user 消息索引），则本次不压缩（返回 `None`）。

### apply_compaction 防御性检查

```rust
impl Session {
    pub fn apply_compaction(&mut self, result: &CompactionResult) {
        let boundary = result.boundary;
        // 防御：确保截断点不破坏配对
        if boundary > 0 && self.history.get(boundary - 1).map(|m| m.role) == Some(Role::Tool) {
            panic!("compaction boundary {} breaks tool pair", boundary);
        }
        // ... 执行截断和替换
    }
}
```

**参考历史：** `compaction_tool_pair_bug.md` — 截断破坏 tool_call/tool_result 配对导致 400 错误；drop_oldest 改用 first_user 对齐。

---

## 七、AgentLoop 控制流（run_turn_core）

```rust
async fn run_turn_core(&mut self, user_text: String) -> anyhow::Result<TurnResult> {
    // 1. 接收用户输入
    self.session.add_user_text(user_text);

    // 2. 刷新附件（IO 失败可记录日志但不阻断）
    if let Err(e) = self.engine.refresh_attachments(&self.session) {
        log::warn!("refresh_attachments failed: {}", e);
    }

    // 3. 构建请求消息
    let mut build_result = self.engine.build_request(&self.session, self.config.context_window);

    // 4. 结算附件
    self.engine.clear_attachments();

    // 5. 检查是否需要压缩
    if self.engine.should_compact(self.config.context_window) {
        if let Some(boundary) = self.engine.compaction_boundary(&self.session) {
            match self.compactor.execute(&mut self.session, boundary, &self.config.model_id).await {
                Ok(result) => {
                    self.session.apply_compaction(&result)?;
                    self.engine.update_for_compaction(result.removed_tokens, result.summary_tokens);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "compaction failed, dropping pre-boundary history");
                    let removed = self.engine.estimate_tokens_for_range(0, boundary);
                    self.session.drop_pre_boundary(boundary);
                    self.engine.update_for_compaction(removed, 0);
                }
            }

            // 压缩后重新构建请求
            build_result = self.engine.build_request(&self.session, self.config.context_window);
        }
    }

    // 6. LLM 交互循环（text → 结束，tool_calls → 执行后继续）
    let specs = self.tool_executor.build_tool_specs(self.config.autonomy);
    loop {
        let response = self.registry.provider.call(&build_result.messages, &specs).await?;
        self.engine.update_usage(&response.usage);

        match response.content {
            Content::Text(text) => {
                self.session.add_assistant_text(text);
                break;
            }
            Content::ToolCalls(calls) => {
                for call in calls {
                    // autonomy 检查在调用 execute 之前
                    if !self.config.autonomy && is_autonomy_only_tool(&call.name) {
                        return Err(anyhow!("autonomy disabled but model called {}", call.name));
                    }
                    let result = self.tool_executor.execute(&call, &mut self.session).await?;
                    self.session.add_tool_result(call.id, result.content, result.is_error);
                }
                // tool 结果已加入 session，重新构建请求继续循环
                build_result = self.engine.build_request(&self.session, self.config.context_window);
            }
        }
    }

    Ok(TurnResult::default())
}
```

**关键特征：**
- `run_turn_core` 只负责控制流编排，不做任何消息变换。
- LLM 交互是循环：text → 结束，tool_calls → 执行后重建请求继续。
- `compaction_boundary` 不需要外部传入 budget，内部自行计算。

---

## 八、重构实施步骤 (Phases)

**Phase 1：PersistBackend + Session 自动持久化**
- 定义 `PersistBackend` trait（7 个写方法 + 4 个读方法）。
- Session 新增 `persist: Option<Arc<dyn PersistBackend>>`。
- Session 实现 `add_message()` / `rollback_to()` / `apply_compaction()` / `restore()`，内部自动持久化。
- **清理：** 删除 AgentLoop 中所有 `if let Some(ref hook)` 调用点（run.rs 6 处 + compaction.rs 3 处）。
- **验证：** AgentLoop 中无 `persist_hook` 字段，编译通过。
- **编译检查点：** Phase 1 完成后 `cargo check` 必须通过。

**Phase 2：ResourceProvider 收拢**
- 定义 `ResourceProvider`，在 Orchestrator 层创建并持有。
- Orchestrator 构造 AgentLoop 时注入 `Arc<ResourceProvider>`。
- **桥接方案：** Phase 2 到 Phase 4 之间，AgentLoop 临时持有 `Arc<ResourceProvider>`，通过它直接访问 skills/mcp_instructions 构建 system prompt。Phase 4 后此访问移入 ContextEngine。
- **清理：** 从 AgentLoop 移除 `skills`, `sub_agent_configs`, `mcp_instructions`, `skills_dir`, `agents_dir`, `change_rx`。
- **验证：** AgentLoop 不持有任何文件路径或 watcher。
- **编译检查点：** Phase 2 完成后 `cargo check` 必须通过。

**Phase 3：ToolExecutor + SpecialToolHandler 注册式**
- 定义 `SpecialToolHandler` trait。
- 实现 `AskUserTool`（持有 `AskUserHandler` closure）和 `DelegateTool`（持有 `DelegateHandler` + `SubAgentDelegator`）。
- 创建 `DefaultToolExecutor`，支持 `register(handler)`。
- **`build_tool_specs` 改为接收 `autonomy: bool`**，内部过滤 autonomy-only 工具。
- **清理：** 从 AgentLoop 移除 `ask_user_handler`, `delegate_handler`, `sub_delegator`。
- autonomy 检查从 ToolExecutor 移到 AgentLoop（execute_tool 调用前）。
- **验证：** 新增特殊工具不需要改 ToolExecutor 代码。
- **编译检查点：** Phase 3 完成后 `cargo check` 必须通过。

**Phase 4：ContextEngine 组装 + same-role merge 集中**
- 定义 `ContextEngine` trait（`build_request(&self, ...)` 纯同步，无副作用）。
- 实现 `DefaultContextEngine`（持有 `TokenTracker`, `AttachmentManager`, `ResourceProvider`）。
- `build_request` 内部执行：sanitize → merge_same_roles → inject attachments。
- `merge_same_roles`：合并连续 user/assistant，tool 不合并。
- **`compaction_boundary` 实现安全截断逻辑**，确保不破坏 tool_call/tool_result 配对。
- **`refresh_attachments` 返回 `anyhow::Result<()>`**。
- **清理：** 修改 `src/providers/anthropic.rs`（删除 same-role merge），检查 OpenAI/GLM/Kimi 等 adapter。
- **验证：** `run_turn_core` 中 `build_messages()` 替换为 `engine.build_request()`。
- **编译检查点：** Phase 4 完成后 `cargo check` 必须通过。

**Phase 5：AgentLoop 精简 + CompactionExecutor 封装**
- AgentLoop 字段精简为 8 个（含 `registry` 和 `compactor`）。
- **CompactionExecutor 封装**：从 `impl AgentLoop` 的 `compaction.rs`（732 行）提取为独立结构体。
  - `CompactionExecutor::execute()` 负责：构建 prompt → 调 LLM → 多轮 tool 交互 → 质量审计 → 返回 `CompactionResult`。
  - `CompactionExecutor` 注入 `registry` / `resources` / `tool_executor`，不持有 AgentLoop 引用。
  - `Session::apply_compaction(result)` 负责：替换 history + 持久化 + 更新 compact_version。
- `ContextEngine` 只保留 `should_compact()` / `compaction_boundary()` 策略判断。
- `run_turn_core` 重写为纯控制流（参考第七章）。
- **验证：** `run_turn_core` ≤ 200 行，`compaction.rs` 从 `impl AgentLoop` 中移除。
- **编译检查点：** Phase 5 完成后 `cargo check` 必须通过。

**Phase 6：子 agent 支持**
- 子 agent 构造独立 `ResourceProvider`（有限 skills/agents）。
- 子 agent 构造独立 `ContextEngine`。
- 子 agent 可选 `PersistBackend`（内存模式不持久化）。
- **验证：** 现有子 agent 功能不受影响。
- **编译检查点：** Phase 6 完成后 `cargo test` 必须通过。

---

## 九、已决定的变更记录 (Changelog vs V6)

1. **`compact()` 从 ContextEngine 移除**：压缩执行（调 LLM）回归 AgentLoop，ContextEngine 只保留策略判断。避免 ContextEngine 持有 ServiceRegistry/ToolRegistry。
2. **`build_request` 改为 `&self` + 无副作用**：`clear_attachments` 显式分离，符合纯变换语义。
3. **`build_request` 不再返回 `anyhow::Result`**：纯同步变换不可能失败。
4. **统一 Same-Role Merging**：合并连续 user/assistant，tool 不合并。Provider adapter 删除相关逻辑。
5. **Session 简化 API**：删除 `add_assistant_with_tools`，提供通用 `add_message(msg)`。thinking block 顺序由调用方处理。
6. **`rollback_to` 内部自动持久化**：AgentLoop 不再手动调 `truncate_messages`。
7. **autonomy 检查移到 AgentLoop**：ToolExecutor 不感知 autonomy，职责更单一。
8. **`build_tool_specs` 接收 `autonomy: bool`**：由 ToolExecutor 过滤不可用的工具，避免模型浪费交互轮次。
9. **`refresh_attachments` 返回 `Result`**：暴露 IO 失败，由调用方决定处理方式。
10. **`PersistBackend` 补充读接口**：`load_messages`, `load_summary`, `load_session_override`, `load_token_count`。
11. **`Session::restore` 静态工厂方法**：统一 session 恢复入口。
12. **`compaction_boundary` 安全截断**：返回的索引保证不破坏 tool_call/tool_result 配对，`apply_compaction` 防御性 panic。
13. **CompactionExecutor 封装**：从 `impl AgentLoop` 的 `compaction.rs`（732 行）提取为独立结构体。注入 `registry`/`resources`/`tool_executor`，返回 `CompactionResult` 由调用方应用。
14. **ToolExecutor 改为 `Arc` 共享**：AgentLoop 和 CompactionExecutor 共享同一实例，避免重复构造。
15. **ContextEngine 补充方法**：`update_for_compaction()` / `estimate_tokens_for_range()`，支持压缩后 token 追踪更新。
16. **Session 补充 `drop_pre_boundary()`**：降级方案，压缩失败时直接丢弃历史。
17. **`compaction_boundary` 内部计算 budget**：不再需要外部传入 `budget` 参数，ContextEngine 内部自行计算 `context_window * threshold - system_prompt - tool_specs`。
18. **`apply_compaction` 参数统一为 `&CompactionResult`**：避免 move，与 `run_turn_core` 调用一致。
19. **`run_turn_core` LLM 交互循环**：text → 结束，tool_calls → 执行后重建请求继续循环。
20. **`tokio_task_local_analysis.md` 标注为 future reference**：当前重构不涉及并发 tool 执行。
21. **CompactionExecutor 接收 `&mut Session`**：因 ToolExecutor 接口要求。但约束 summarize 过程中只暴露 memory tools，不修改 `session.history`。
22. **ContextEngine 新增 `set_tool_spec_tokens()`**：AgentLoop 在 tool specs 变更时调用，`compaction_boundary` 内部读取用于 budget 计算，无需持有 ToolExecutor 引用。

---

## 十、风险与注意事项

### 10.1 async fn in trait

Rust 1.75+ 支持 `async fn in trait`。如果 MSRV < 1.75，使用 `async-trait` crate。

### 10.2 Provider adapter 清理范围

Anthropic adapter（`anthropic.rs`）有 same-role merge 逻辑（lines 202-232）。OpenAI/GLM/Kimi 等 adapter 需要逐个检查是否也有类似逻辑。Phase 4 需全面排查。

### 10.3 Session 恢复时的持久化

`recover_incomplete_turn` 会调用 `session.add_tool_result()` 和 `session.add_assistant_text()`，这些消息会触发自动持久化。这是正确的行为（填补缺失记录），无需特殊处理。

### 10.4 /btw 旁路提问

`/btw` 使用独立请求，不走 AgentLoop。**不受本次重构影响。**

### 10.5 Phase 间编译连续性

每个 Phase 末尾必须保证 `cargo check` 通过。Phase 2→4 的桥接方案：AgentLoop 临时持有 `Arc<ResourceProvider>` 用于 system prompt 构建，Phase 4 后移入 ContextEngine。

### 10.6 context_window 参数预留

`build_request` 接收 `context_window: u64` 作为预留参数。当前实现中仅用于可能的未来扩展（如按 token 预算滑动窗口截断），不做实际 truncation。如需启用，在 `DefaultContextEngine` 中接入 `TokenTracker` 估算逻辑。

---

## 十一、代码布局建议

```
src/
├── agent/
│   ├── mod.rs
│   ├── loop.rs              # AgentLoop（精简后，8 个字段）
│   ├── context_engine.rs    # ContextEngine trait + DefaultContextEngine
│   ├── session.rs           # Session + PersistBackend trait
│   ├── tool_executor.rs     # ToolExecutor trait + DefaultToolExecutor
│   ├── resource_provider.rs # ResourceProvider
│   └── compaction.rs        # CompactionExecutor（从 impl AgentLoop 提取）
```
