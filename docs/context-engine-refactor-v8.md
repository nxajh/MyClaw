# MyClaw Context Engine 重构方案 (V8)

> 生成日期：2026-05-15
> 状态：V7 评审后修订版
>
> **相对 V7 的核心修订：**
> 1. Session 保持纯数据，不内嵌 PersistBackend（V7 问题 1）
> 2. `CompactionExecutor::execute` 接收只读切片，类型约束取代注释约定（V7 问题 2）
> 3. `ContextEngine` trait 拆解为 `CompactionPolicy` struct + `RequestBuilder` struct（V7 问题 3）
> 4. Phase 2 + Phase 4 合并，消除桥接中间状态（V7 问题 5）
> 5. 图片字段归入 `RequestBuilder`（V7 遗漏）
> 6. 新增 Phase 1：先修复增量压缩 Bug，再开始大重构
> 7. `CompactionExecutor::execute` 增加 `tool_specs: &[ToolSpec]` 参数，保证 summarizer LLM 请求与主请求 cache key 一致（修复 V7/V8 初版缓存命中率问题）

---

## 一、设计目标与原则

### 1.1 目标

解决 `AgentLoop` 职责过重（20+ 字段）的问题，拆分出职责单一的组件：

| 组件 | 职责 | 副作用 |
|------|------|--------|
| `Session` | 纯数据载体，提供原地变换方法 | 无 |
| `CompactionPolicy` | Token 追踪 + 压缩策略判断 | 无 |
| `RequestBuilder` | 消息构建 + 附件管理 + 图片状态 + 热加载 | 文件系统（refresh 时） |
| `ResourceProvider` | 热加载资源容器（Arc 共享） | 无 |
| `DefaultToolExecutor` | 主对话工具执行 | 文件系统、网络、`&mut Session` |
| `MemoryToolExecutor` | 压缩专用受限执行器（无 session 访问） | 仅 memory/ 目录读写 |
| `CompactionExecutor` | 压缩摘要生成（只读历史，返回结果） | LLM 调用 |
| `AgentLoop` | 纯控制流（10 个字段） | 组合以上所有 |

### 1.2 三条核心设计原则

**原则 A：Session 是数据载体，不是聚合根。**
Session 不内嵌 `PersistBackend`。`add_user_text()`、`apply_compaction()` 等方法都是纯内存操作。持久化由 AgentLoop 通过 `persist_hook` 在合适时机**显式触发**。

理由：持久化失败的错误路径由调用方统一处理；rollback（`session.rollback_to(len)` + `hook.truncate_messages()`）保持两步独立语义；单元测试 Session 无需 mock I/O。

**原则 B：类型约束替代注释约定。**
`CompactionExecutor::execute` 接收 `&[ChatMessage]`（只读切片），而不是 `&mut Session`。编译器强制保证不修改 history，无需靠注释承诺。`MemoryToolExecutor` 不接受 `&mut Session` 参数，彻底隔离 memory 工具与 session 状态。

**原则 C：具体类型替代 trait 抽象。**
除非有多个实现需要运行时替换，否则用 struct 不用 trait。`CompactionPolicy` 和 `RequestBuilder` 是具体类型，不抽象为 trait。`ContextEngine` trait（当前是死代码）直接删除。

---

## 二、架构层级与数据流

```
┌─────────────────────────────────────────────────────────────────┐
│                          Orchestrator                           │
│  持有 SessionManager、Agent，处理 slash 命令，分发 channel 消息    │
└──────────────────────────────┬──────────────────────────────────┘
                               │ 构造时注入 Arc<ResourceProvider>
                               ▼
┌─────────────────────────────────────────────────────────────────┐
│                           AgentLoop                             │
│                        (10 个字段)                               │
│                                                                 │
│  run_turn_core:                                                 │
│    1. recover_incomplete_turn()                                 │
│    2. request_builder.refresh(session)    → 附件 diff           │
│    3. request_builder.set_images(...)                           │
│    4. combined = request_builder.merge_attachments(user_text)   │
│    5. policy.record_pending(user_msg_tokens)                    │
│    6. session.add_user_text(combined) + persist                 │
│    7. request_builder.clear_pending()                           │
│    8. messages = request_builder.build(session)                 │
│    9. chat_loop(messages)                                       │
│                                                                 │
│  chat_loop (loop):                                              │
│    a. maybe_compact_for_fallback()                              │
│    b. provider = select_provider()                              │
│    c. maybe_compact(model_id)         [pre-API]                 │
│    d. messages = request_builder.build(session) [非首轮]         │
│    e. specs = tool_executor.build_tool_specs(autonomy)          │
│    f. response = provider.chat(messages, specs)                 │
│    g. policy.update_usage(response.usage)                       │
│    h. maybe_compact(model_id)         [post-API]                │
│    i. 无 tool_calls → add_assistant_text + persist → return     │
│    j. 有 tool_calls → execute_all + record_pending + persist    │
│       → 继续循环                                                 │
└──────┬────────────────┬───────────────┬─────────────────────────┘
       │                │               │
       ▼                ▼               ▼
┌─────────────┐  ┌────────────┐  ┌──────────────────────────────┐
│   Session   │  │Compaction  │  │        RequestBuilder        │
│             │  │Policy      │  │                              │
│ 纯数据      │  │            │  │ system_prompt: String        │
│ history     │  │TokenTracker│  │ attachments: AttachmentMgr   │
│ message_ids │  │+ threshold │  │ resources: Arc<ResourceProv> │
│ compact_ver │  │+ retain    │  │ pending_image_urls           │
│ summary_meta│  │            │  │ pending_image_base64         │
│             │  │should_     │  │                              │
│ add_*()     │  │compact()   │  │ refresh(session)             │
│ rollback_to │  │compaction_ │  │ merge_attachments(text)→str  │
│ apply_      │  │boundary()  │  │ build(session)→Vec<Msg>      │
│ compaction()│  │update_     │  │ set_images() / has_images()  │
│ drop_pre_   │  │usage()     │  │ take_images()                │
│ boundary()  │  │adjust_for_ │  │ clear_pending()              │
└─────────────┘  │compaction()│  │ estimate_tool_spec_tokens()  │
                 └────────────┘  └──────────────┬───────────────┘
                                                │ Arc
                                                ▼
                                 ┌──────────────────────────────┐
                                 │       ResourceProvider       │
                                 │                              │
                                 │ skills: Arc<RwLock<>>        │
                                 │ sub_agents: Arc<RwLock<>>    │
                                 │ mcp_instructions             │
                                 │ prompt_config                │
                                 │ skills_dir / agents_dir      │
                                 │ change_rx                    │
                                 └──────────────────────────────┘

┌──────────────────────────┐   ┌──────────────────────────────────┐
│  DefaultToolExecutor     │   │       CompactionExecutor         │
│  (Arc，主对话用)          │   │                                  │
│                          │   │ registry: Arc<dyn ServiceReg>    │
│ tools: Arc<ToolRegistry> │   │ resources: Arc<ResourceProvider> │
│ ask_user_handler         │   │ memory_executor: MemoryToolExec  │
│ delegate_handler         │   │                                  │
│ sub_delegator            │   │ execute(                         │
│                          │   │   history: &[ChatMessage],  ←只读│
│ execute(                 │   │   system_prompt: &str,           │
│   call, &mut Session     │   │   boundary: usize,               │
│ ) → ToolResult           │   │   model_id: &str,                │
│ build_tool_specs(        │   │ ) → CompactionResult             │
│   autonomy               │   └──────────────────────────────────┘
│ ) → Vec<ToolSpec>        │   ┌──────────────────────────────────┐
└──────────────────────────┘   │       MemoryToolExecutor         │
                               │  (仅 file_write/edit/read/shell) │
                               │                                  │
                               │  workspace_dir: PathBuf          │
                               │  execute(call) → ToolResult      │
                               │  tool_specs() → Vec<ToolSpec>    │
                               └──────────────────────────────────┘
```

---

## 三、核心组件接口定义

### 3.1 CompactionPolicy — Token 追踪 + 压缩策略

```rust
/// Token 追踪与压缩决策。纯计算，无副作用。
///
/// 直接复用 agent_impl/types.rs 的 TokenTracker（删除 context_engine.rs 的重复版本）。
pub struct CompactionPolicy {
    tracker: TokenTracker,
    compact_threshold: f64,
    retain_work_units: usize,
}

impl CompactionPolicy {
    pub fn from_context_config(cfg: &ContextConfig) -> Self;

    /// 从已存储的精确 token 总数恢复（会话重载时使用）
    pub fn init_from_stored(&mut self, total: u64) {
        self.tracker.update_from_usage(total, 0, 0);
    }

    /// 从历史估算（全新会话，无已存储数据）
    pub fn init_from_history(&mut self, system_prompt: &str, history: &[ChatMessage]) {
        if !system_prompt.is_empty() {
            self.tracker.record_pending(estimate_tokens(system_prompt) + 4);
        }
        for msg in history {
            self.tracker.record_pending(estimate_message_tokens(msg));
        }
    }

    /// API 响应后更新（精确数据，重置 pending estimate）
    pub fn update_usage(&mut self, input: u64, output: u64, cached: u64) {
        self.tracker.update_from_usage(input, output, cached);
    }

    /// 新增消息时累加估算（API 调用前）
    pub fn record_pending(&mut self, tokens: u64) {
        self.tracker.record_pending(tokens);
    }

    /// 是否满足压缩阈值
    pub fn should_compact(&self, context_window: u64) -> bool {
        let threshold = (context_window as f64 * self.compact_threshold) as u64;
        self.tracker.total_tokens() >= threshold
    }

    /// 计算压缩边界。
    ///
    /// budget = context_window * threshold - system_prompt_tokens - tool_spec_tokens
    /// 调用 work_unit::find_compaction_boundary_for_budget。
    /// 返回 None 表示无需压缩（内容太少或 budget 为零）。
    pub fn compaction_boundary(
        &self,
        history: &[ChatMessage],
        context_window: u64,
        system_prompt_tokens: u64,
        tool_spec_tokens: u64,
    ) -> Option<usize> {
        let budget = ((context_window as f64 * self.compact_threshold) as u64)
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(tool_spec_tokens);
        if budget == 0 { return None; }
        work_unit::find_compaction_boundary_for_budget(
            history,
            budget,
            self.retain_work_units.max(1),
        )
    }

    /// 压缩完成后调整 token 计数
    pub fn adjust_for_compaction(&mut self, removed: u64, added: u64) {
        self.tracker.adjust_for_compaction(removed, added);
    }

    pub fn token_total(&self) -> u64 { self.tracker.total_tokens() }
    pub fn last_usage(&self) -> (u64, u64, u64) {
        (self.tracker.last_input(), self.tracker.last_cached(), self.tracker.last_output())
    }
    pub fn is_fresh(&self) -> bool { self.tracker.is_fresh() }
}
```

**与 V7 的关键差异：**
- V7 用 `ContextEngine` trait（10+ 方法混合三个关注点）；本版是聚焦的 struct（7 个方法）
- 不存储 `tool_spec_tokens`，改为在 `compaction_boundary()` 调用时直接传参——调用方（AgentLoop）在调用点总是知道这个值，无需预先存储到 struct 中
- 删除 `context_engine.rs` 中与 `types.rs` 重复的 `TokenTracker`

---

### 3.2 ResourceProvider — 热加载资源容器

```rust
/// 可热加载的共享资源。
/// 由 Orchestrator 在启动时构造，通过 Arc 注入各 AgentLoop。
/// 子 agent 使用独立的 ResourceProvider（有限 skills/agents，无 change_rx）。
pub struct ResourceProvider {
    pub skills: Arc<RwLock<SkillManager>>,
    pub sub_agents: Arc<RwLock<Vec<SubAgentConfig>>>,
    pub mcp_instructions: Vec<(String, String)>,
    pub prompt_config: SystemPromptConfig,
    pub skills_dir: PathBuf,
    pub agents_dir: PathBuf,
    /// 文件变更通知。None = 子 agent（无热加载）。
    pub change_rx: Option<watch::Receiver<ChangeSet>>,
}
```

---

### 3.3 RequestBuilder — 消息构建 + 附件管理

```rust
/// 消息构建器。
/// 持有 system prompt、附件状态、图片状态。
/// 内部调用 ResourceProvider 执行热加载 diff。
pub struct RequestBuilder {
    system_prompt: String,
    attachments: AttachmentManager,
    resources: Arc<ResourceProvider>,
    /// 本轮图片（set_images 注入，take_images 消费）
    pending_image_urls: Option<Vec<String>>,
    pending_image_base64: Option<Vec<String>>,
}

impl RequestBuilder {
    pub fn new(system_prompt: String, resources: Arc<ResourceProvider>) -> Self;

    /// 热加载检查 + 附件 diff（skills / agents / MCP / memory / date）。
    /// 每轮 turn 开始时调用一次，在添加用户消息之前。
    /// IO 失败记录 warn 日志，不中断 turn（不返回 Result）。
    pub fn refresh(&mut self, session: &Session);

    /// 将 pending attachments 合并进用户消息文本，返回合并后的字符串。
    /// 典型用法：combined = builder.merge_attachments(user_text)
    ///            session.add_user_text(combined)
    ///            builder.clear_pending()
    pub fn merge_attachments(&self, user_text: &str) -> String;

    /// 结算 pending attachments（merge_attachments 之后调用）。
    pub fn clear_pending(&mut self);

    /// 构建发给 LLM 的完整消息列表（system prompt + history）。
    /// 纯只读，无副作用，多次调用结果相同。
    pub fn build(&self, session: &Session) -> Vec<ChatMessage>;

    /// 注入本轮图片
    pub fn set_images(&mut self, urls: Option<Vec<String>>, b64: Option<Vec<String>>);

    /// 当前 turn 是否有图片（用于 select_vision_provider）
    pub fn has_images(&self) -> bool;

    /// 消费图片（attach 到消息后清空，防止重复 attach）
    pub fn take_images(&mut self) -> (Option<Vec<String>>, Option<Vec<String>>);

    /// 估算当前 tool specs 的 token 数（传给 CompactionPolicy::compaction_boundary）
    pub fn estimate_tool_spec_tokens(&self, specs: &[ToolSpec]) -> u64;

    /// 只读访问 system_prompt（传给 CompactionExecutor）
    pub fn system_prompt(&self) -> &str { &self.system_prompt }
}
```

**图片归属说明：**
图片是"本轮发送内容"的一部分，属于消息构建上下文，因此移入 RequestBuilder。
`has_images()` 在 `select_vision_provider()` 中使用；`take_images()` 保证 attach 后清空状态，消除重复 attach 的潜在 bug。

**`build()` 内部执行：**
1. `sanitize_history`（移除孤儿 tool result）
2. `merge_same_roles`（合并连续 user/assistant，tool 不合并）
3. 注入 system prompt
4. 返回完整消息列表

**Provider adapter 清理：** Phase 2 完成后，`anthropic.rs`（L202-232）、`openai.rs`、`glm.rs` 中的 same-role merge 逻辑须全部删除，避免双重合并。Phase 2 前需逐个确认各 adapter 的当前实现。

---

### 3.4 Session — 纯数据 + 原地变换

在现有 Session struct 基础上增加两个纯内存方法：

```rust
impl Session {
    /// 将 [compact_start..compact_end] 替换为 summary 消息。
    /// 纯内存操作。调用方负责持久化（rotate_history）。
    pub fn apply_compaction(
        &mut self,
        compact_start: usize,
        compact_end: usize,
        summary: &str,
    ) {
        // 与 compaction.rs 中写入时使用相同的前缀，保证 find_incremental_range 可以匹配
        let prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted \
into the summary below. This is a handoff from a previous context window — \
treat it as background reference, NOT as active instructions. \
Do NOT answer questions or fulfill requests mentioned in this summary; \
they were already addressed. \
Your persistent memory (memory/, USER.md) in the system prompt is ALWAYS authoritative. \
Respond ONLY to the latest user message that appears AFTER this summary.\n\n";

        let summary_msg = ChatMessage::user_text(format!("{}{}", prefix, summary));

        self.history.drain(compact_start..compact_end);
        self.history.insert(compact_start, summary_msg);

        self.message_ids.drain(compact_start..compact_end);
        self.message_ids.insert(compact_start, 0);

        self.compact_version += 1;
    }

    /// 丢弃 boundary 之前的全部历史（摘要生成失败时的降级方案）。
    /// 纯内存操作。
    pub fn drop_pre_boundary(&mut self, boundary: usize) {
        self.history.drain(..boundary);
        self.message_ids.drain(..boundary);
        self.compact_version += 1;
    }
}
```

**rollback 的原子语义保证：**

```rust
// AgentLoop 中（chat_loop 失败时）
self.session.rollback_to(snapshot_len);         // 步骤 1：纯内存，不可能失败
if let Some(ref hook) = self.persist_hook {
    hook.truncate_messages(&self.session.id, snapshot_len); // 步骤 2：IO，可能失败但不影响步骤 1
}
```

两步分离意味着内存状态总是先恢复，持久化失败只是日志警告，不会出现"内存已回滚但磁盘未回滚"之外更糟的情况（下次重启时 SessionManager 重新加载磁盘数据即可）。

---

### 3.5 DefaultToolExecutor — 主对话工具执行

```rust
pub struct DefaultToolExecutor {
    tools: Arc<ToolRegistry>,
    ask_user_handler: Option<AskUserHandler>,
    delegate_handler: Option<DelegateHandler>,
    sub_delegator: Option<Arc<SubAgentDelegator>>,
    timeout_secs: u64,
}

impl DefaultToolExecutor {
    /// 执行工具调用。
    ///
    /// ask_user / agent_delegate 是特殊工具，需要 &mut Session 来写入问答记录
    /// （ask_user 本质上是一次对话交互，不是普通工具）。
    /// autonomy 检查由 AgentLoop 在调用前完成，executor 不感知 autonomy。
    pub async fn execute(
        &self,
        call: &ToolCall,
        session: &mut Session,
    ) -> anyhow::Result<ToolResult>;

    /// 构建 tool spec 列表，按 autonomy_level 过滤不可用工具。
    /// autonomy=ReadOnly 时过滤写工具；autonomy=false 时过滤 ask_user 等需要用户交互的工具。
    pub fn build_tool_specs(&self, autonomy: &AutonomyLevel) -> Vec<ToolSpec>;
}
```

---

### 3.6 MemoryToolExecutor — 压缩专用受限执行器

```rust
/// 压缩 summarizer 专用的工具执行器。
///
/// 只暴露 memory 相关工具：file_write / file_edit / file_read / shell（仅 rm 命令）。
/// 不接受 session 参数——memory 操作只需要文件系统访问，与 session.history 无关。
/// 用类型约束（无 &mut Session 参数）替代 V7 的注释承诺。
pub struct MemoryToolExecutor {
    workspace_dir: PathBuf,
    timeout_secs: u64,
}

impl MemoryToolExecutor {
    pub fn new(workspace_dir: impl Into<PathBuf>, timeout_secs: u64) -> Self;

    pub async fn execute(&self, call: &ToolCall) -> anyhow::Result<ToolResult>;

    /// 返回 summarizer 可见的 tool spec 子集
    pub fn tool_specs() -> Vec<ToolSpec>;
}
```

---

### 3.7 CompactionExecutor — 压缩摘要生成

```rust
pub struct CompactionResult {
    pub compact_start: usize,    // 被压缩范围的起点
    pub compact_end: usize,      // 被压缩范围的终点（开区间）
    pub summary: String,
    pub summary_tokens: u64,
    pub removed_tokens: u64,
    pub compacted_count: usize,
}

pub struct CompactionExecutor {
    registry: Arc<dyn ServiceRegistry>,
    resources: Arc<ResourceProvider>,
    memory_executor: MemoryToolExecutor,
    max_rounds: usize,
}

impl CompactionExecutor {
    pub fn new(
        registry: Arc<dyn ServiceRegistry>,
        resources: Arc<ResourceProvider>,
        workspace_dir: PathBuf,
    ) -> Self;

    /// 执行压缩，生成摘要，返回结果。
    ///
    /// 接收只读历史切片——编译器保证不修改 session.history。
    /// 摘要生成后由调用方（AgentLoop）通过 session.apply_compaction() 应用。
    ///
    /// **缓存一致性**：`tool_specs` 必须与主请求的 tool spec 列表完全相同。
    /// Provider 前缀缓存的 key = model + system_prompt + tool_definitions + messages_prefix，
    /// 任何一项不同都会导致缓存未命中。因此 summarizer LLM 请求必须使用与主对话
    /// 完全相同的 tool spec，而不是 MemoryToolExecutor 的受限子集。
    /// MemoryToolExecutor 仅用于实际工具执行（类型安全约束），不用于 LLM 请求。
    pub async fn execute(
        &self,
        history: &[ChatMessage],     // 只读
        system_prompt: &str,
        tool_specs: &[ToolSpec],     // 主请求的完整 tool spec（保证缓存 key 一致）
        boundary: usize,
        model_id: &str,
    ) -> anyhow::Result<CompactionResult>;
}
```

**`execute` 内部实现要点（私有）：**

```rust
/// 找增量压缩范围，提取旧摘要。
///
/// ★ 关键修复：匹配写入时的实际前缀（"[CONTEXT COMPACTION — REFERENCE ONLY]"）。
/// V7 原版 `find_incremental_range` 扫描 "[Context Summary]"，与写入前缀不符，
/// 导致旧摘要永远无法被识别，增量合并完全失效。
fn find_incremental_range(history: &[ChatMessage], boundary: usize) -> (usize, usize, Option<String>) {
    let last_summary = history[..boundary].iter().rposition(|m| {
        m.role == "user"
            && m.text_content().starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
    });
    match last_summary {
        Some(idx) => (idx, boundary, Some(history[idx].text_content())),
        None => (0, boundary, None),
    }
}

/// 图片剥离（summarizer 不需要图片内容）
fn strip_images(msgs: &[ChatMessage]) -> Vec<ChatMessage> { /* ... */ }

/// 构建 summarizer messages（复用 system_prompt + tool_specs，保证缓存 key 与主请求一致）
fn build_summarizer_messages(system_prompt: &str, tool_specs: &[ToolSpec], ...) -> Vec<ChatMessage> { /* ... */ }

/// Mini chat_loop（最多 max_rounds 轮）
///
/// LLM 请求：使用传入的完整 tool_specs（cache key 与主请求一致 → 缓存命中）
/// 工具执行：使用 MemoryToolExecutor（类型约束，不能访问 session → 无副作用泄漏）
/// 两层分离：LLM 看到完整 tool spec，但实际只有 memory 工具可以被执行。
async fn run_summarizer_loop(&self, messages: &mut Vec<ChatMessage>, tool_specs: &[ToolSpec], model_id: &str) -> anyhow::Result<String> { /* ... */ }

/// 质量审计（非阻塞，失败只 warn）
fn audit_summary_quality(to_compact: &[ChatMessage], summary: &str) -> (bool, Vec<String>) { /* ... */ }
```

---

## 四、AgentLoop 精简后（10 个字段）

```rust
pub struct AgentLoop {
    // ── 核心状态 ──
    pub(crate) session: Session,

    // ── 消息构建 + 附件 + 图片 + 热加载 ──
    // 替换: system_prompt, attachments, pending_image_{urls,base64},
    //       mcp_instructions, skills, sub_agent_configs,
    //       skills_dir, agents_dir, change_rx（共 10 个字段）
    pub(crate) request_builder: RequestBuilder,

    // ── Token 追踪 + 压缩策略 ──
    // 替换: token_tracker
    pub(crate) policy: CompactionPolicy,

    // ── 工具执行 ──
    // 替换: tools, ask_user_handler, delegate_handler, sub_delegator（共 4 个字段）
    pub(crate) tool_executor: Arc<DefaultToolExecutor>,

    // ── 压缩执行 ──
    pub(crate) compactor: CompactionExecutor,

    // ── 基础设施 ──
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    /// model_override 和 thinking_override 合并进 AgentConfig（via SessionOverride）
    pub(crate) config: AgentConfig,
    pub(crate) loop_breaker: LoopBreaker,
    /// 显式 IO 边界。Session 是纯数据，持久化由 AgentLoop 负责。
    pub(crate) persist_hook: Option<Arc<dyn PersistHook>>,

    // ── 重试状态 ──
    pub(crate) pending_retry_message: Option<String>,
}
```

移除的字段（共 15 个）：`system_prompt`、`attachments`、`pending_image_urls`、`pending_image_base64`、`mcp_instructions`、`skills`、`sub_agent_configs`、`skills_dir`、`agents_dir`、`change_rx`、`token_tracker`、`tools`、`ask_user_handler`、`delegate_handler`、`sub_delegator`。

`model_override`、`thinking_override` 合并进 `AgentConfig`，由 `with_override()` 方法处理。

---

## 五、run_turn_core 控制流

```rust
async fn run_turn_core(
    &mut self,
    user_message: &str,
    image_urls: Option<Vec<String>>,
    image_base64: Option<Vec<String>>,
    stream_mode: StreamMode,
) -> anyhow::Result<String> {
    // 1. 恢复中断的 turn（process 被 kill 时重执行未完成的工具调用）
    let _recovery = self.recover_incomplete_turn(&stream_mode).await?;

    // 2. 重置循环熔断
    self.loop_breaker.reset();

    // 3. 初始化 token 估算（fresh session 或 recovery 后）
    if self.policy.is_fresh() {
        if let Some(stored) = self.session.last_total_tokens {
            self.policy.init_from_stored(stored);
        } else {
            self.policy.init_from_history(
                self.request_builder.system_prompt(),
                &self.session.history,
            );
        }
    }

    // 4. 热加载检查 + 附件 diff（在添加用户消息之前，使 diff 基于当前历史）
    self.request_builder.refresh(&self.session);

    // 5. 注入本轮图片
    self.request_builder.set_images(image_urls, image_base64);

    // 6. 合并附件，估算新用户消息 token
    let combined_text = self.request_builder.merge_attachments(user_message);
    let user_msg = ChatMessage::user_text(combined_text.clone());
    self.policy.record_pending(estimate_message_tokens(&user_msg));

    // 7. 记录 rollback 快照点（BEFORE 添加用户消息）
    let snapshot_len = self.session.history.len();

    // 8. 写入 session（纯内存）+ 显式持久化
    self.session.add_user_text(combined_text);
    self.request_builder.clear_pending();
    self.persist_last_message();    // 内部调用 persist_hook.persist_message()

    // 9. 构建初始消息列表
    let initial_messages = self.request_builder.build(&self.session);

    let is_streamed = matches!(&stream_mode, StreamMode::Streamed { .. });

    // 10. chat_loop（出错时回滚内存 + 持久化）
    let text = match self.chat_loop(initial_messages, stream_mode).await {
        Ok(text) => text,
        Err(e) => {
            self.session.rollback_to(snapshot_len);
            if let Some(ref hook) = self.persist_hook {
                hook.truncate_messages(&self.session.id, snapshot_len);
            }
            return Err(e);
        }
    };

    // 11. 持久化最终 token count（供下次重启恢复用）
    if let Some(ref hook) = self.persist_hook {
        hook.save_token_count(&self.session.id, self.policy.token_total());
    }

    Ok(text)
}
```

---

## 六、maybe_compact 实现

```rust
async fn maybe_compact(&mut self, model_id: &str) -> anyhow::Result<()> {
    let context_window = match self.registry.get_chat_model_config(model_id)?
        .context_window {
        Some(cw) => cw,
        None => return Ok(()),
    };

    if !self.policy.should_compact(context_window) {
        return Ok(());
    }

    // 计算 budget（system prompt + tool specs 占用的 token 需扣除）
    let system_prompt_tokens = estimate_tokens(self.request_builder.system_prompt());
    let specs = self.tool_executor.build_tool_specs(self.config.autonomy_level());
    let tool_spec_tokens = self.request_builder.estimate_tool_spec_tokens(&specs);

    let boundary = match self.policy.compaction_boundary(
        &self.session.history,
        context_window,
        system_prompt_tokens,
        tool_spec_tokens,
    ) {
        Some(b) => b,
        None => return Ok(()),
    };

    tracing::info!(
        total = self.policy.token_total(),
        context_window,
        boundary,
        "compaction triggered"
    );

    // 执行压缩（只读历史，编译器保证不修改 session）
    // specs 同时传给 compactor，保证 summarizer LLM 请求与主请求缓存 key 完全一致
    let result = match self.compactor.execute(
        &self.session.history,
        self.request_builder.system_prompt(),
        &specs,
        boundary,
        model_id,
    ).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "compaction failed, dropping pre-boundary");
            let removed = estimate_range_tokens(&self.session.history, 0, boundary);
            let last_id = self.session.message_ids.get(boundary.saturating_sub(1)).copied().unwrap_or(0);
            self.session.drop_pre_boundary(boundary);
            self.policy.adjust_for_compaction(removed, 0);
            self.persist_compaction_drop(boundary, last_id);
            return Ok(());
        }
    };

    // 应用到 session（纯内存）
    let last_compacted_id = self.session.message_ids
        .get(result.compact_end.saturating_sub(1))
        .copied()
        .unwrap_or(0);

    self.session.apply_compaction(result.compact_start, result.compact_end, &result.summary);
    self.policy.adjust_for_compaction(result.removed_tokens, result.summary_tokens);

    // 显式持久化
    let version = self.session.compact_version;
    if let Some(ref hook) = self.persist_hook {
        hook.save_compaction(&self.session.id, &SummaryRecord {
            id: 0,
            version,
            summary: result.summary.clone(),
            up_to_message: last_compacted_id,
            token_estimate: Some(result.summary_tokens),
            created_at: chrono::Utc::now(),
        });
        let surviving = self.session.history_with_ids();
        hook.rotate_history(&self.session.id, &surviving);
    }

    // Safety net：仍然超阈值时截断保留区大 tool result
    let threshold = (context_window as f64 * self.config.context.compact_threshold) as u64;
    if self.policy.token_total() > threshold {
        self.truncate_retention_zone(result.compact_start + 1, model_id);
    }

    tracing::info!(
        compacted = result.compacted_count,
        removed_tokens = result.removed_tokens,
        summary_tokens = result.summary_tokens,
        new_total = self.policy.token_total(),
        version,
        "compaction completed"
    );

    Ok(())
}
```

---

## 七、重构实施步骤 (Phases)

Phase 依赖关系：

```
Phase 1 → Phase 2 → Phase 3 ──→ Phase 5 → Phase 6
                 ↘ Phase 4 ──↗
```

Phase 3 和 Phase 4 互不依赖，可并行开发。

---

### Phase 1：修复 Critical Bug + 提取 CompactionPolicy

> 这个 Phase 独立、风险低，应在大重构开始前优先完成。

**1-a. 修复增量压缩 Bug**

文件：`src/agents/agent_impl/compaction.rs:307`

```rust
// 修改前（错误）：
m.role == "user" && m.text_content().starts_with("[Context Summary]")

// 修改后（正确，与写入时的前缀一致）：
m.role == "user" && m.text_content().starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
```

验证：触发两次压缩，确认第二次能识别并增量合并第一次生成的摘要。

**1-b. 删除 `context_engine.rs` 死代码**

整个文件未被任何调用方使用，直接删除。同时删除 `src/agents/mod.rs` 中对应的模块声明（如有）。

**1-c. 提取 `CompactionPolicy` struct**

- 文件：新建 `src/agents/compaction_policy.rs`
- 把 `agent_impl/types.rs` 的 `TokenTracker` 扩展为 `CompactionPolicy`（增加 `compact_threshold`、`retain_work_units` 字段，实现 `should_compact()`、`compaction_boundary()` 方法）
- `compaction_boundary()` 的实现从 `compact_to_budget()` 内移入（`find_compaction_boundary_for_budget` 调用保持不变）
- `AgentLoop` 将 `token_tracker: TokenTracker` 替换为 `policy: CompactionPolicy`
- 更新所有调用点（约 15 处）

**编译检查点**：`cargo check`。

---

### Phase 2：ResourceProvider + RequestBuilder（原 Phase 2+4 合并）

> 一次性完成，消除"桥接中间状态"。

**2-a. 创建 `src/agents/resource_provider.rs`**

定义 `ResourceProvider`（见 3.2），字段来自 AgentLoop 的：
`skills`、`sub_agent_configs`、`mcp_instructions`、`skills_dir`、`agents_dir`、`change_rx`

**2-b. 创建 `src/agents/request_builder.rs`**

定义 `RequestBuilder`（见 3.3）：
- `refresh()` 吸收 `check_changes()` + `diff_skills()` + `diff_agents()` + `diff_mcp()` + `diff_memory()` + `diff_date()`
- `build()` 吸收 `build_messages()` 核心逻辑，包含 `sanitize_history`、`merge_same_roles`
- `merge_attachments()` 吸收附件合并逻辑
- `pending_image_*` 字段从 AgentLoop 移入

**2-c. 清理 Provider adapter same-role merge**

Phase 2 前先确认 `anthropic.rs`、`openai.rs`、`glm.rs`、`kimi.rs` 各自的 same-role merge 逻辑，在 `build()` 内实现统一处理后逐一删除 adapter 中的对应代码。

**2-d. 更新 AgentLoop**

移除 10 个字段，添加 `request_builder: RequestBuilder`。
更新 `run_turn_core`、`chat_loop` 中的调用点。

**2-e. 更新 `Agent::loop_for_with_persist`**

Orchestrator 创建 `Arc<ResourceProvider>`，传入 `RequestBuilder::new()`。

**编译检查点**：`cargo check`。
**验证**：热加载（技能/子 agent 变更）、附件注入、图片 attach 均正常。

---

### Phase 3：DefaultToolExecutor + MemoryToolExecutor

**3-a. 创建 `src/agents/tool_executor.rs`**

定义 `DefaultToolExecutor`（见 3.5）：
- `tools`、`ask_user_handler`、`delegate_handler`、`sub_delegator` 从 AgentLoop 移入
- ask_user / agent_delegate 的 `&mut Session` 依赖保留（这是合理的领域依赖，不应消除）
- `autonomy` 检查从 execute 内部移到 AgentLoop 的调用点（execute 调用前）

**3-b. 实现 `MemoryToolExecutor`**（同文件）

仅暴露 file_write / file_edit / file_read / shell（rm 命令）。
不接受 `&mut Session` 参数。

**3-c. 更新 AgentLoop**

移除 4 个字段，添加 `tool_executor: Arc<DefaultToolExecutor>`。

**编译检查点**：`cargo check`。
**验证**：ask_user、agent_delegate、file_read、shell 等工具调用均正常；autonomy=ReadOnly 时写工具被拦截。

---

### Phase 4：CompactionExecutor（可与 Phase 3 并行）

**4-a. Session 新增纯内存方法**（`session_manager.rs`）

`apply_compaction(compact_start, compact_end, summary)` 和 `drop_pre_boundary(boundary)`（见 3.4）。

**4-b. 创建 `src/agents/compaction_executor.rs`**

定义 `CompactionExecutor`、`CompactionResult`（见 3.7）。

把 `compact_with_boundary()` 的以下逻辑移入 `execute()`：
- `find_incremental_range()`（使用修复后的前缀——Phase 1 已修复）
- `do_inline_summarize()` → `run_summarizer_loop()`（使用 `MemoryToolExecutor`）
- `audit_summary_quality()`、`extract_file_paths()`

**4-c. 重构 `compaction.rs`**

`maybe_compact()` 改为调用 `self.compactor.execute(&self.session.history, ...)` 并通过 `session.apply_compaction()` 应用结果（见第六章）。

删除 `compact_to_budget()`、`compact_with_boundary()`、`do_inline_summarize()`（已移入 CompactionExecutor）。

**编译检查点**：`cargo check`。
**验证**：/compact 手动压缩、自动触发压缩、增量合并旧摘要、压缩失败降级（drop_pre_boundary）均正常。

---

### Phase 5：AgentLoop 精简 + 整体清理

**5-a. 合并 model_override / thinking_override 进 AgentConfig**

当前这两个字段是独立的，合并后 AgentLoop 字段数收敛到 10。

**5-b. 重写 `run_turn_core`**（如果 Phase 1-4 过程中未完全完成）

按第五章控制流最终整理。

**5-c. 删除 compaction.rs 中的残余旧代码**

确认 `compact_to_budget`、`compact_with_boundary`、`find_incremental_range`（已移入 CompactionExecutor）等不再存在于 `impl AgentLoop` 中。

**编译检查点**：`cargo check`。
**验证**：端到端测试（压缩、热加载、ask_user、agent_delegate、流式输出、rollback）全部通过。

---

### Phase 6：代码布局整理

```
src/agents/
├── mod.rs
├── agent_impl/
│   ├── mod.rs              # AgentLoop struct（10 字段）+ Agent factory
│   ├── run.rs              # run_turn_core, chat_loop
│   ├── compaction.rs       # maybe_compact, maybe_compact_for_fallback（调用 compactor）
│   ├── tools.rs            # execute_tool 委托给 tool_executor
│   └── images.rs           # select_vision_provider（读 request_builder.has_images()）
├── compaction_policy.rs    # CompactionPolicy（Phase 1 新建）
├── resource_provider.rs    # ResourceProvider（Phase 2 新建）
├── request_builder.rs      # RequestBuilder（Phase 2 新建）
├── tool_executor.rs        # DefaultToolExecutor, MemoryToolExecutor（Phase 3 新建）
├── compaction_executor.rs  # CompactionExecutor, CompactionResult（Phase 4 新建）
├── session_manager.rs      # Session（+apply_compaction, +drop_pre_boundary）, SessionManager
├── scheduling/
│   └── work_unit.rs        # WorkUnit, find_compaction_boundary_for_budget（不变）
└── ...（其余模块不变）
```

**Phase 6 清理项：**
- 删除 `context_engine.rs`（Phase 1 已完成）
- 确认 `types.rs` 中只剩 `TokenTracker`（CompactionPolicy 内部使用）、工具函数等
- 更新 `src/agents/mod.rs` 中的模块声明

---

## 八、与 V7 原版的变更对照

| # | V7 原版决策 | 本版修订 | 理由 |
|---|------------|----------|------|
| 1 | Session 内嵌 `PersistBackend`，`add_message()` 自动持久化 | Session 保持纯数据，`persist_hook` 留在 AgentLoop | rollback 保持两步原子语义；持久化错误由调用方统一处理；单元测试无需 mock I/O |
| 2 | `CompactionExecutor::execute(&mut Session)` | `CompactionExecutor::execute(&[ChatMessage])` | 类型系统强制只读约束，消除"承诺但不保证"的脆弱设计；`MemoryToolExecutor` 不需要 session |
| 3 | `ContextEngine` trait（10+ 方法，混合三个关注点） | `CompactionPolicy` struct + `RequestBuilder` struct | 无多实现需求不引入 trait；ISP 违反；`context_engine.rs` 当前就是死代码 |
| 4 | Phase 2 + Phase 4 分开，中间桥接 | Phase 2 + Phase 4 合并 | 消除双路访问 skills/system_prompt 的中间状态，降低 bug 风险 |
| 5 | 图片字段（`pending_image_*`）未提及归属 | 图片字段移入 `RequestBuilder` | 图片属于"本轮发送内容"，是消息构建上下文的一部分；`take_images()` 防止重复 attach |
| 6 | `compact()` 从 ContextEngine 移除 | 同，放入 `CompactionExecutor` | 与 V7 一致 ✅ |
| 7 | `merge_same_roles` 集中到 `build_request` 内部 | 同，在 `RequestBuilder::build()` 内执行 | 与 V7 一致 ✅ |
| 8 | `compaction_boundary` 内部读取 `tool_spec_tokens`（需 `set_tool_spec_tokens` 预设） | 调用时直接传参 `tool_spec_tokens` | 消除 CompactionPolicy 持有工具状态的耦合；调用方（AgentLoop）在调用点总是知道这个值 |
| 9 | 删除 `add_assistant_with_tools` | 保留 | ask_user 的特殊 session 写入需要它；删除后逻辑迁移成本高于收益 |
| 10 | 未提及增量压缩 Bug | Phase 1 首先修复前缀不匹配 | 不修复则所有增量合并在生产中完全失效 |
| 11 | `rollback_to` 内部自动持久化 | 保持当前设计（rollback_to 纯内存，外层显式调 truncate_messages） | 与原则 A 一致；自动持久化使补偿 I/O 的失败路径更复杂 |

---

## 九、风险与注意事项

### 9.1 `ask_user` 的 `&mut Session` 依赖

`DefaultToolExecutor` 执行 `ask_user` 时调用 `session.add_assistant_text(question)` 和 `session.add_user_text(answer)`。这是合理的领域依赖（ask_user 本质上是一个对话交互），不应该试图消除。

### 9.2 Provider adapter same-role merge 清理顺序

**必须先**确认每个 adapter 的当前行为（`anthropic.rs` L202-232 是已知的），再在 `RequestBuilder::build()` 中实现统一处理，最后删除 adapter 中的对应代码。顺序不能颠倒，否则会出现 same-role 消息被 API 拒绝的线上问题。

### 9.3 子 agent 的 ResourceProvider 构造

子 agent 需要构造独立的 `ResourceProvider`：有限的 skills/agents，无 `change_rx`（不热加载）。Orchestrator 在 `Agent::loop_for_with_persist` 决定传入的 `Arc<ResourceProvider>` 内容，这已经是自然的决策点。

### 9.4 `compact_failures` 熔断器（遗留）

原 V7 提到的熔断器（`compact_failures: usize`）可在 Phase 4 时随 CompactionExecutor 一起加入——在 `execute()` 调用位置（`maybe_compact` 中）维护连续失败计数，≥3 次后跳过压缩并 warn。字段放在 `AgentLoop`（一个 `usize`）。
