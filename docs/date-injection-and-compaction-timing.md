# MyClaw: 日期注入 + Compaction 时机改进

## 1. 日期注入

### 1.1 问题

MyClaw 当前没有日期注入。模型不知道当前日期，无法：
- 判断"昨天"、"上周"等相对时间
- 在记忆文件的 `created_at` 字段填入正确日期
- 判断时间敏感操作（如 CI 是否已经跑了一段时间）

### 1.2 方案

通过 `AttachmentManager` 的 system-reminder 机制注入日期，复用已有的增量注入框架。

**注入方式**：
- 首次：`build_messages()` 时注入一条 system-reminder，包含当前日期
- 日期变化：检测到日期变化时，注入一条新的 system-reminder 更新日期
- Compaction 后：如果日期 system-reminder 被压缩掉，重新注入
- 不写入 system prompt（避免 cache break）

**时区**：从 `SystemPromptConfig` 新增的 `timezone_offset: i32` 字段读取。默认值 `8`（UTC+8）。YAML 配置示例：

```yaml
prompt:
  timezone_offset: 8
```

### 1.3 设计

#### AttachmentManager 扩展

```rust
pub struct AttachmentManager {
    pending: HashMap<AttachmentKind, Delta>,
    memory_index: Option<String>,
    last_injected_date: Option<String>,  // "YYYY-MM-DD"
}
```

新增方法：

```rust
/// 检查日期变化，需要时生成 system-reminder。
pub fn diff_date(&mut self, timezone_offset: i32, history: &[ChatMessage]) {
    let now_utc = chrono::Utc::now();
    let local = now_utc + chrono::Duration::hours(timezone_offset as i64);
    let current_date = local.format("%Y-%m-%d").to_string();
    let current_weekday = local.format("%A").to_string();

    // 检查 history 中是否已有当日 system-reminder
    let date_in_history = history.iter().rev().any(|msg| {
        let text = msg.text_content();
        text.contains("<system-reminder>")
            && text.contains(&format!("Current date: {}", current_date))
    });

    if date_in_history {
        // History 中已有当日消息，无需重复注入
        self.last_injected_date = Some(current_date);
        return;
    }

    // History 中没有当日消息（首次、日期变化、或 compaction 后）
    let is_date_change = self.last_injected_date.is_some();
    self.last_injected_date = Some(current_date.clone());

    let msg = if is_date_change {
        format!(
            "The date has changed. Today's date is now {} ({}).",
            current_date,
            current_weekday,
        )
    } else {
        format!(
            "Current date: {} ({}, UTC{}). Use this for any date-relative references.",
            current_date,
            current_weekday,
            if timezone_offset >= 0 { format!("+{}", timezone_offset) }
            else { format!("{}", timezone_offset) }
        )
    };

    self.pending.insert(
        AttachmentKind::DateInjection,
        Delta { added: vec![msg], removed: vec![] },
    );
}
```

**为什么不只依赖 `last_injected_date`**：compaction 会删除 history 中的旧 system-reminder。如果只看内存中的 `last_injected_date`，compaction 后会误以为已经注入过了，跳过注入。所以需要同时检查 history。

**为什么还保留 `last_injected_date`**：用来区分"首次注入"和"日期变化"。首次注入包含时区信息，日期变化只通知新日期。

#### AttachmentKind 扩展

```rust
enum AttachmentKind {
    SkillListing,
    AgentListing,
    McpInstructions,
    MemoryListing,
    DateInjection,  // 新增
}
```

`pending_keys()` 追加 `DateInjection => "date"`。
`build_message()` 追加 `render_date(delta)` 调用。

#### render_date

```rust
fn render_date(delta: &Delta) -> String {
    delta.added.join("\n")
}
```

#### 调用时机

**统一在 `build_messages()` 内调用**，跟 `check_changes()` 并列：

```rust
async fn build_messages(&mut self) -> anyhow::Result<Vec<ChatMessage>> {
    // ...
    self.check_changes();

    // Date injection (every time messages are built)
    let tz = self.config.prompt_config.timezone_offset;
    self.attachments.diff_date(tz, &self.session.history);

    // Skills/agents/MCP diffs...
    // ...
}
```

这样无论是 `run()` 还是 `chat_loop` 的后续迭代，只要调用 `build_messages()` 就会自动检查日期。不需要在多处手动调用。

### 1.4 改动清单

| 文件 | 改动 |
|------|------|
| `src/agents/attachment.rs` | 新增 `DateInjection` kind、`last_injected_date` 字段、`diff_date()` 方法、`render_date()` |
| `src/agents/agent_impl.rs` | `build_messages()` 内调用 `diff_date()` |
| `src/agents/prompt.rs` | `SystemPromptConfig` 新增 `timezone_offset: i32` 字段 |
| `config YAML` | `prompt.timezone_offset: 8` |

### 1.5 Claude Code 对比

| 维度 | Claude Code | MyClaw |
|------|-------------|--------|
| 首次注入 | messages[0] user context | system-reminder |
| 日期变化 | `date_change` attachment | system-reminder（复用同一机制） |
| Compaction 后 | cache-aware 不重建 prefix | 扫描 history 判断是否需要重注入 |
| 时区 | 本地系统时区 | 配置文件指定 |
| Cache 影响 | 不破坏 prefix cache | 不破坏 prefix cache |

---

## 2. Compaction 时机改进

### 2.1 问题

当前 compaction 只在 **API 响应后** 检查（L1012）。这意味着：

```
用户消息 → build_messages() → API call → LLM 响应（tool calls）
→ 执行工具 → tool result 很大（如 file_read 返回整个文件）
→ token_tracker.record_pending(巨大的 result)
→ 回到 loop 顶部
→ build_messages() → messages 已经包含巨大的 tool result
→ API call（context 已经超限）→ 可能报错
→ 收到响应后才检查 maybe_compact  ← 太晚了
```

关键问题：`build_messages()` 之后、API call 之前没有 compaction 检查。

### 2.2 风险场景

1. **file_read 大文件**：LLM 读了一个 5000 行的文件，tool result 很大
2. **连续工具调用**：LLM 连续调了 5 个 file_read，每个都返回大内容
3. **delegate_task 结果**：子 agent 返回大量输出

在这些场景下，tool result 被加入 history 后，下一轮的 context 可能已经超过 threshold。但 compaction 检查要等到 **下一轮 API 响应后** 才触发——中间有一次 API call 是带着超限 context 发出去的。

### 2.3 方案

**在 `build_messages()` 之前调用 `maybe_compact()`**。这样 build_messages 使用的是压缩后的 history。

当前 flow：
```
loop {
    let (provider, model_id) = get_provider();
    let messages = build_messages();       // 可能包含大量 tool result
    API call                               // 可能超限
    处理响应
    maybe_compact();                       // ← 太晚了
}
```

改进后 flow：
```
loop {
    let (provider, model_id) = get_provider();
    maybe_compact(&model_id);              // 先检查，需要就压缩
    let messages = build_messages();       // 压缩后的 history → 安全的 messages
    API call                               // 不会超限
    处理响应
    maybe_compact();                       // 保留：精确 token 数据
}
```

关键区别：`maybe_compact()` 移到 `build_messages()` **之前**。这样：
1. 先检查是否需要压缩（用 estimate token 数据）
2. 压缩后 history 已更新
3. `build_messages()` 用压缩后的 history 构建 messages
4. 不需要"压缩后 rebuild"——因为 build 在 compact 之后

### 2.4 具体改动

当前 `chat_loop` 代码结构：

```rust
loop {
    let (provider, model_id) = get_provider();    // L866

    let mut messages = if first_iteration {
        first_iteration = false;
        initial_messages.clone()
    } else {
        self.build_messages().await?               // L904
    };

    // ... attach images, build tools, API call ...
}
```

改为：

```rust
loop {
    let (provider, model_id) = get_provider();    // L866

    // Pre-API compaction: tool results from the previous round may have
    // pushed context over threshold. Compact before building messages.
    // No-op on first iteration (context won't exceed threshold).
    if let Err(e) = self.maybe_compact(&model_id).await {
        tracing::warn!(error = %e, "pre-API compaction failed, continuing");
    }

    let mut messages = if first_iteration {
        first_iteration = false;
        initial_messages.clone()
    } else {
        self.build_messages().await?
    };

    // ... attach images, build tools, API call ...
}
```

**首次迭代不受影响**：`maybe_compact()` 在首次迭代时检查 token count，远低于 threshold，直接 return。`initial_messages` 正常使用。

**后续迭代**：tool result 被加入 history → `record_pending()` 更新 token estimate → 回到 loop → `maybe_compact()` 检查 estimate → 超限就压缩 → `build_messages()` 用压缩后的 history。

### 2.5 注意事项

1. **保留 post-API `maybe_compact`（L1012）**：API 响应提供精确 token 数据，比 pre-API 的 estimate 更准确。两个检查点互补：pre-API 用 estimate 防溢出，post-API 用精确数据做最终判断。

2. **`maybe_compact_for_fallback`（L857）保留**：pre-fallback 检查的是 fallback model 的 context window，不是 threshold。这是独立逻辑。

3. **token tracker 精度**：pre-API 检查用的是 `record_pending()` 的累计 estimate。不如 API 响应后的精确数据，但作为提前保护足够。最坏情况是 estimate 偏低导致没触发 pre-API compaction，post-API 检查会兜底。

4. **compaction 内部的 mini chat_loop 不受影响**：`do_inline_summarize` 有自己的 mini loop，不走 `chat_loop`。

### 2.6 改动清单

| 文件 | 改动 |
|------|------|
| `src/agents/agent_impl.rs` | `chat_loop` loop 体开头（`get_provider` 之后、`build_messages` 之前）增加 `maybe_compact()` 调用 |

### 2.7 对比

| 维度 | 改进前 | 改进后 |
|------|--------|--------|
| 检查点 | 2（pre-fallback + post-API） | 3（pre-fallback + pre-API + post-API） |
| 最大超限窗口 | 1 次 API call | 0（API 前就压缩） |
| Token 精度 | post-API 精确 | pre-API estimate + post-API 精确 |
| 性能影响 | 无 | 每轮多一次 token count 比较（O(1)，忽略不计） |
