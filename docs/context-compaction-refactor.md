# MyClaw 上下文压缩重构方案

## 背景

当前 `maybe_compact` 存在以下问题：
1. `TokenTracker` 每次 `run()` 重置，丢弃上一轮 API 校准的真实 usage
2. 新增用户消息未计入 tracker，压缩决策时看不到这条消息
3. `compact_ratio=1.0` 固定压缩全部历史只保留最后 1 条，工具调用链断裂
4. 没有增量压缩，每次把全部早期历史喂给 summarizer
5. sub-agent summarizer 使用不同 system prompt，无法利用前缀缓存
6. `trim_oldest` fallback 只是删除，不做任何信息保留

## 目标

- 压缩决策准确（基于真实 usage + 增量估算）
- 工具调用链完整（保留最近 2 个 work unit）
- 压缩成本低（增量压缩 + 内联 summarizer + 前缀缓存）
- 行为可预测（固定策略，减少配置项）

---

## Phase 1: TokenTracker 修复与配置调整

### 1.1 TokenTracker 不再 reset

**文件**: `src/agents/agent_impl.rs`

**修改前**:
```rust
pub async fn run(&mut self, user_message: &str, ...) -> Result<String> {
    self.token_tracker.reset();  // ← 删除
    for msg in &self.session.history {
        self.token_tracker.record_pending(estimate_message_tokens(msg));
    }
    // ...
}
```

**修改后**:
```rust
pub async fn run(&mut self, user_message: &str, ...) -> Result<String> {
    // 新会话/恢复后初始化：如果 tracker 为空，从 history 估算
    if self.token_tracker.is_fresh() {
        // system prompt（不在 history 中，需单独计入）
        if !self.system_prompt.is_empty() {
            self.token_tracker.record_pending(
                estimate_tokens(&self.system_prompt) + 4  // metadata overhead
            );
        }
        for msg in &self.session.history {
            self.token_tracker.record_pending(estimate_message_tokens(msg));
        }
    }
    
    // 新增用户消息立即计入 tracker
    let user_msg = ChatMessage::user_text(user_message.to_string());
    self.token_tracker.record_pending(estimate_message_tokens(&user_msg));
    
    self.session.add_user_text(user_message.to_string());
    // ...
}
```

> **为什么 system prompt 需要计入？**
> `session.history` 不含 system prompt，但 `build_messages()` 会单独加上它。
> API 返回的 `input_tokens` 包含 system prompt，所以 `update_from_usage` 后 tracker 自然包含。
> 但在**首次 API 调用前**（新会话或恢复后），`is_fresh()` 估算不含 system prompt 会导致
> `total_tokens` 偏低，`maybe_compact` 的阈值判断偏松。补上 system prompt 后，
> 新会话第一轮的压缩决策也是准确的。

**TokenTracker 新增方法**:
```rust
impl TokenTracker {
    pub fn is_fresh(&self) -> bool {
        self.last_input_tokens == 0
            && self.last_cached_tokens == 0
            && self.pending_estimated_tokens == 0
    }
}
```

**TokenTracker::total_tokens 修正**:

上一轮的 `output` 在本轮已变成 input 的一部分（assistant 消息在 history 中），因此 `total_tokens()` 必须包含 `last_output_tokens`。

```rust
pub fn total_tokens(&self) -> u64 {
    self.last_input_tokens
        .saturating_add(self.last_cached_tokens)
        .saturating_add(self.last_output_tokens)  // ← 新增
        .saturating_add(self.pending_estimated_tokens)
}
```

### 1.2 配置项调整

**文件**: `src/config/agent.rs`

**修改前**:
```rust
pub struct ContextConfig {
    pub compact_threshold: f64,
    pub compact_ratio: f64,  // ← 删除
}
```

**修改后**:
```rust
pub struct ContextConfig {
    /// 触发压缩的阈值比例（默认 0.7）
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold: f64,
    
    /// 保留的最近完整 work unit 数量（默认 2）
    #[serde(default = "default_retain_work_units")]
    pub retain_work_units: usize,
}

fn default_retain_work_units() -> usize { 2 }
```

**截断策略统一为单层**：

原方案有两层截断：
1. 各 tool 的 `max_output_tokens()`（工具执行时截断）
2. `ContextConfig.max_tool_output_tokens`（压缩兜底时截断）

两层配置概念重复且容易混乱。统一为**以各 tool 的 `max_output_tokens()` 为唯一截断点**：
- 工具执行时按自身阈值截断（下调后的值已足够小）
- `truncate_retention_zone` 降级为 safety net（只处理 MCP tool 等不遵守限制的异常大消息）
- 删除 `ContextConfig.max_tool_output_tokens` 配置项

**兼容性**: `compact_ratio` 字段从 `ContextConfig` 中删除。
- 标准 serde 行为下，旧配置中的未知字段会被静默忽略，不会导致反序列化失败。
- 如需在日志中输出 deprecation warning，可在 `ContextConfig` 的自定义 `Deserialize` 实现中检测 `compact_ratio` 键的存在并 warn。
- `AgentConfig` 未设置 `#[serde(deny_unknown_fields)]`，因此兼容旧配置无需额外处理。

---

## Phase 2: Work Unit 与边界检测

### 2.1 Work Unit 定义

**新增文件**: `src/agents/work_unit.rs`

```rust
//! Work Unit — 可压缩的最小对话单元。
//!
//! 一个 work unit = 触发该轮对话的 user 消息 + assistant 回复 + assistant 调用的所有 tool results。
//! 纯文本 assistant 消息（无 tool_calls）也是一个独立 work unit（user + assistant）。

use std::collections::HashSet;
use crate::providers::ChatMessage;

#[derive(Debug, Clone)]
pub struct WorkUnit {
    /// 触发该 work unit 的 user 消息索引（assistant 前面最近的 user）
    pub user_start: usize,
    /// assistant 消息在历史中的索引
    pub start: usize,
    /// 最后一个匹配的 tool result 索引（若无 tool calls 则等于 start）
    pub end: usize,
}

/// 从历史消息中提取所有 work units
///
/// 一个 work unit 从 user 消息开始，包含随后的 assistant 及其 tool results。
pub fn extract_work_units(history: &[ChatMessage]) -> Vec<WorkUnit> {
    let mut units = Vec::new();
    let mut i = 0;

    while i < history.len() {
        // 跳过非 assistant 消息
        if history[i].role != "assistant" {
            i += 1;
            continue;
        }

        let start = i;

        // 向前回溯：找到触发该 assistant 的 user 消息
        let user_start = history[..start].iter().rposition(|m| m.role == "user")
            .unwrap_or(start); // 若找不到 user，则退化为 assistant 自身

        let mut tool_ids = HashSet::new();
        if let Some(ref calls) = history[i].tool_calls {
            for call in calls {
                tool_ids.insert(call.id.clone());
            }
        }

        // 无 tool calls 的纯文本 assistant = 独立 work unit
        if tool_ids.is_empty() {
            units.push(WorkUnit { user_start, start, end: start });
            i += 1;
            continue;
        }

        // 向后消费所有匹配的 tool results
        let mut end = start;
        let mut j = start + 1;
        while j < history.len() && history[j].role == "tool" {
            if let Some(ref tcid) = history[j].tool_call_id {
                if tool_ids.contains(tcid) {
                    end = j;
                    j += 1;
                    continue;
                }
            }
            break; // 遇到不匹配的 tool result 或下一个 assistant
        }

        units.push(WorkUnit { user_start, start, end });
        i = j; // 跳到下一个 work unit 的起点
    }

    units
}

/// 找到压缩边界索引。
///
/// 返回值为 `boundary`，表示 `history[boundary..]` 应完整保留。
/// 边界前推到保留 work unit 的 user_start，确保 user 指令不丢失。
/// 若对话还短（work units <= retain_count），返回 history.len()（不压缩）。
pub fn find_compaction_boundary(history: &[ChatMessage], retain_count: usize) -> usize {
    if history.len() <= 1 {
        return history.len();
    }

    let units = extract_work_units(history);

    if units.len() <= retain_count {
        // 对话还短，不压缩
        history.len()
    } else {
        // 保留最近 retain_count 个 work unit，边界前推到 user 消息
        units[units.len() - retain_count].user_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ToolCall;

    fn make_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    #[test]
    fn test_extract_work_units() {
        let mut assistant1 = ChatMessage::assistant_text("查看目录");
        assistant1.tool_calls = Some(vec![make_tool_call("call_1", "list_dir")]);

        let mut assistant2 = ChatMessage::assistant_text("读取文件");
        assistant2.tool_calls = Some(vec![make_tool_call("call_2", "file_read")]);

        let mut tool1 = ChatMessage::text("tool", "dir: src/");
        tool1.tool_call_id = Some("call_1".to_string());
        let mut tool2 = ChatMessage::text("tool", "fn main()");
        tool2.tool_call_id = Some("call_2".to_string());

        let history = vec![
            ChatMessage::user_text("分析代码"),
            assistant1,
            tool1,
            assistant2,
            tool2,
        ];
        let units = extract_work_units(&history);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].user_start, 0); // user "分析代码"
        assert_eq!(units[0].start, 1);
        assert_eq!(units[0].end, 2);
        assert_eq!(units[1].user_start, 3); // 前面无 user，退化为 start
    }

    #[test]
    fn test_boundary_with_user_preserved() {
        let mut assistant1 = ChatMessage::assistant_text("看目录");
        assistant1.tool_calls = Some(vec![make_tool_call("call_1", "list_dir")]);

        let mut assistant2 = ChatMessage::assistant_text("读文件");
        assistant2.tool_calls = Some(vec![make_tool_call("call_2", "file_read")]);

        let mut assistant3 = ChatMessage::assistant_text("运行测试");
        assistant3.tool_calls = Some(vec![make_tool_call("call_3", "shell")]);

        let mut tool1 = ChatMessage::text("tool", "src/");
        tool1.tool_call_id = Some("call_1".to_string());
        let mut tool2 = ChatMessage::text("tool", "content");
        tool2.tool_call_id = Some("call_2".to_string());
        let mut tool3 = ChatMessage::text("tool", "pass");
        tool3.tool_call_id = Some("call_3".to_string());

        let history = vec![
            ChatMessage::user_text("第一轮"),
            assistant1,
            tool1,
            ChatMessage::user_text("第二轮"),
            assistant2,
            tool2,
            ChatMessage::user_text("第三轮"),
            assistant3,
            tool3,
        ];
        // 3 个 work units，保留 2 个，边界应在第二轮的 user 消息（index 3）
        assert_eq!(find_compaction_boundary(&history, 2), 3);
    }
}
```

### 2.2 在 AgentLoop 中注册模块

**文件**: `src/agents/mod.rs`（确认已有 module 声明）

```rust
pub mod work_unit;
```

---

## Phase 3: 增量压缩与内联 Summarizer

### 3.1 增量压缩范围检测

**文件**: `src/agents/agent_impl.rs`

```rust
/// 找到可压缩的增量范围，同时提取旧摘要文本。
///
/// 如果有旧 summary，把它本身也纳入压缩范围，与新内容合并生成新摘要。
/// history 中始终只保留一条 summary user message。
/// 如果没有旧 summary，压缩 [0..boundary] 全部早期历史。
fn find_incremental_range(&self, boundary: usize) -> (usize, usize, Option<String>) {
    let history = &self.session.history;

    // 找到最后一个 summary 的索引（summary 现在用 user message role）
    let last_summary = history[..boundary].iter().rposition(|m| {
        m.role == "user" && m.text_content().starts_with("[Context Summary]")
    });

    match last_summary {
        Some(idx) => {
            let existing = history[idx].text_content();
            (idx, boundary, Some(existing))  // 旧 summary 一并纳入压缩
        }
        None => (0, boundary, None),
    }
}
```

### 3.2 摘要的 Role 选择：User Message

**为什么用 user message 而不是 system message？**

| 维度 | System Message | User Message | Assistant Message |
|------|---------------|-------------|-------------------|
| 语义 | 指令/身份 | 用户提供的上下文材料 | 模型的回复 |
| 注意力权重 | 模型当作"约束"，可能不够"活跃" | 模型当作"待处理信息"，响应更积极 | 角色错位 |
| 多次压缩 | 容易堆积多个 system msg | 可合并为单条 user msg | 不合理 |
| 与 true system prompt 关系 | 污染/挤占指令带宽 | 分离清晰 | — |

**结论**：摘要本质上是"已发生对话的压缩"，不是指令，也不是模型输出。把它作为**用户提供的背景材料**（user message）角色最准确。

**多次压缩的合并策略**：history 里始终只保留**一条** `[Context Summary]` user message。增量压缩时，把旧摘要作为 base，和新内容一起生成合并摘要，然后替换旧条目。

### 3.3 内联 Summarizer

**文件**: `src/agents/agent_impl.rs`（在 `AgentLoop` impl 中新增）

```rust
/// 内联 summarizer —— 复用主请求的 system prompt 和相同模型。
///
/// 两层 fallback：
/// 1. cache-sharing 模式：tool definitions + thinking config 和主请求一致，最大化前缀缓存命中
/// 2. sub-delegator fallback：完全独立的子代理
///
/// 注意：不设置"去掉 tools 的降级模式"。cache-sharing 失败的主因是 API 错误
///（超时、网络），去掉 tools 无法解决；模型调用工具的场景已被 prompt 约束
/// + max_tokens: 500 防护，不需要额外 fallback。
async fn summarize_inline(
    &self,
    to_compact: &[ChatMessage],
    existing_summary: Option<&str>,
    model_id: &str,
) -> anyhow::Result<String> {
    // 优先级 1：cache-sharing 模式（最大化前缀缓存命中）
    match self.do_inline_summarize(to_compact, existing_summary, model_id).await {
        Ok(s) if !s.trim().is_empty() => return Ok(s),
        Ok(_) => tracing::warn!("summarize returned empty"),
        Err(e) => tracing::warn!(error = %e, "summarize failed"),
    }

    // 优先级 2：sub-delegator fallback
    tracing::warn!("inline summarize failed, falling back to sub-delegator");
    self.fallback_summarize(to_compact)
}

/// 内联 summarizer 的实际实现。
///
/// 构造的请求和主请求在以下参数上完全一致（确保 provider 的 prefix cache 最大化命中）：
/// - system_prompt
/// - tool_definitions
/// - thinking config
/// - model_id
///
/// 只有 messages 前缀和最后一条 user message 不同。
async fn do_inline_summarize(
    &self,
    to_compact: &[ChatMessage],
    existing_summary: Option<&str>,
    model_id: &str,
) -> anyhow::Result<String> {
    let provider = self.registry.get_chat_provider(Capability::Chat)?;

    let mut messages = Vec::new();

    // 1. system prompt（和主请求一致）
    if !self.system_prompt.is_empty() {
        messages.push(ChatMessage::system_text(&self.system_prompt));
    }

    // 2. 待压缩历史（主请求 messages 的前缀子集）
    for msg in to_compact {
        messages.push(msg.clone());
    }

    // 3. summarizer instruction
    let prompt = match existing_summary {
        Some(base) => format!(
            "Previous context summary:\n{}\n\n\
             Merge the above events into the previous summary. \
             Keep it under 300 characters. Focus on: user goals, \
             key decisions, file paths, and errors.",
            base
        ),
        None => "请用简洁的中文总结上述对话历史，保留以下内容：\n\
                 - 用户的原始目标和当前任务\n\
                 - 关键决策和结论\n\
                 - 涉及的文件路径和代码位置\n\
                 - 遇到的错误和修复方案\n\
                 省略工具输出的原始内容（如大段代码、日志），只保留关键指标。\n\
                 不超过300字。".to_string(),
    };
    messages.push(ChatMessage::user_text(prompt));

    // 4. tool definitions（和主请求一致，cache key 匹配）
    let tools = self.build_tool_specs();

    // 5. thinking config（和主请求一致，cache key 匹配）
    let thinking = self.registry.get_chat_model_config(model_id)
        .ok()
        .and_then(|cfg| {
            if cfg.reasoning {
                Some(ThinkingConfig { enabled: true, effort: None })
            } else {
                None
            }
        });

    let req = ChatRequest {
        model: model_id,
        messages: &messages,
        tools: if tools.is_empty() { None } else { Some(&tools[..]) },
        thinking,
        max_tokens: Some(500),
        stream: true,
        ..Default::default()
    };

    let stream = provider.chat(req)?;
    let response = self.collect_stream(stream).await?;

    // 记录缓存命中情况（用于监控）
    if let Some(ref usage) = response.usage {
        if let Some(cached) = usage.cached_input_tokens {
            tracing::info!(
                cached_tokens = cached,
                total_input = usage.input_tokens.unwrap_or(0),
                "summarizer cache hit"
            );
        }
    }

    Ok(response.text)
}

/// Fallback：使用 sub-delegator 子代理做摘要（独立 system prompt，无前缀缓存收益）。
fn fallback_summarize(&self, to_compact: &[ChatMessage]) -> anyhow::Result<String> {
    if let Some(ref delegator) = self.sub_delegator {
        let mut text_for_summary = String::new();
        for msg in to_compact {
            let text = msg.text_content();
            if !text.is_empty() {
                text_for_summary.push_str(&format!("[{}] {}\n\n", msg.role, text));
            }
        }
        // 注意：delegate 是 async 的，这里需要改为 async fn
        // 实际实现中 fallback_summarize 应该也是 async
        anyhow::bail!("sub-delegator fallback not yet async-compatible in this context")
    } else {
        anyhow::bail!("no sub-delegator configured for fallback summarization")
    }
}
```

> **注意**：`fallback_summarize` 实际上也应该是 `async` 的。
> 简化起见，上面的伪代码展示了 fallback 策略的意图。
> 实际实现时，`summarize_inline` 内部直接 match 两个 async 分支即可。

### 3.3 重写 maybe_compact

**文件**: `src/agents/agent_impl.rs`

```rust
async fn maybe_compact(&mut self, model_id: &str) -> anyhow::Result<()> {
    let model_config = self.registry.get_chat_model_config(model_id)?;
    let context_window = match model_config.context_window {
        Some(cw) => cw,
        None => return Ok(()),
    };
    
    let threshold = (context_window as f64 * self.config.context.compact_threshold) as u64;
    let total = self.token_tracker.total_tokens();
    
    if total <= threshold {
        return Ok(());
    }
    
    tracing::info!(
        total_tokens = total,
        threshold,
        context_window,
        "starting context compaction"
    );
    
    let history_len = self.session.history.len();
    if history_len <= 1 {
        return Ok(());
    }
    
    // 1. 找到保留边界（保留最近 N 个 work unit）
    let retain_count = self.config.context.retain_work_units.max(1);
    let boundary = work_unit::find_compaction_boundary(&self.session.history, retain_count);
    
    if boundary >= history_len {
        tracing::info!("no compaction needed: conversation within retention");
        return Ok(());
    }
    if boundary == 0 {
        tracing::info!("no compaction needed: all history must be retained");
        return Ok(());
    }
    
    // 2. 找到增量压缩范围，同时提取旧摘要（用于增量合并）
    let (compact_start, compact_end, existing_summary) = self.find_incremental_range(boundary);
    let to_compact = &self.session.history[compact_start..compact_end];
    
    if to_compact.is_empty() {
        tracing::info!("no new content to compact");
        return Ok(());
    }
    
    tracing::info!(
        compact_start,
        compact_end,
        boundary,
        retain_count,
        has_existing_summary = existing_summary.is_some(),
        "compaction range determined"
    );
    
    // 3. 生成摘要（增量合并旧摘要）
    let summary = self.summarize_inline(to_compact, existing_summary.as_deref(), model_id).await?;
    
    if summary.trim().is_empty() {
        tracing::warn!("summarizer returned empty, falling back to truncation");
        self.truncate_retention_zone(boundary, model_id);
        return Ok(());
    }
    
    // 4. 替换历史
    let version = self.session.compact_version + 1;
    let summary_msg = ChatMessage::user_text(
        format!("[Context Summary] {}", summary)
    );
    let summary_tokens = estimate_message_tokens(&summary_msg);
    
    // 记录被压缩的最后一条消息 ID
    let last_compacted_id = self.session.message_ids
        .get(compact_end.saturating_sub(1))
        .copied()
        .unwrap_or(0);
    
    self.session.history.drain(compact_start..compact_end);
    self.session.history.insert(compact_start, summary_msg);
    
    self.session.message_ids.drain(compact_start..compact_end);
    self.session.message_ids.insert(compact_start, 0);
    
    self.session.compact_version = version;
    self.session.summary_metadata = Some(SummaryMetadata {
        version,
        token_estimate: summary_tokens,
        up_to_message: last_compacted_id,
    });
    
    // 5. 持久化摘要
    if let Some(ref hook) = self.persist_hook {
        hook.save_compaction(&self.session.key, &SummaryRecord {
            id: 0,  // 后端自动分配
            version,
            summary: summary.clone(),
            up_to_message: last_compacted_id,
            token_estimate: Some(summary_tokens),
            created_at: chrono::Utc::now(),
        });
    }
    
    // 6. 调整 token tracker
    let removed_tokens: u64 = to_compact.iter().map(estimate_message_tokens).sum();
    self.token_tracker.adjust_for_compaction(removed_tokens, summary_tokens);
    
    let new_total = self.token_tracker.total_tokens();
    tracing::info!(
        compacted_messages = to_compact.len(),
        summary_tokens,
        removed_tokens,
        new_total_tokens = new_total,
        version,
        "context compaction completed"
    );
    
    // 7. 兜底：如果仍然超阈值，safety net 截断
    if new_total > threshold {
        self.truncate_retention_zone(boundary, model_id);
    }
    
    Ok(())
}
```

### 3.4 删除旧的 trim_oldest，改为 safety net fallback

**文件**: `src/agents/agent_impl.rs`

```rust
/// Safety net：当 summarizer 失败或保留区仍然超阈值时的兜底策略。
///
/// 正常情况下各 tool 的 max_output_tokens 已在执行时截断了输出。
/// 此函数处理的是异常场景（如 MCP tool 不遵守限制、ask_user 返回超长文本等）。
///
/// 1. 截断保留区内异常大的 tool results（安全上限：context_window 的 5%）
/// 2. 如果仍然超阈值，降级删除最早的一个保留 work unit
fn truncate_retention_zone(&mut self, boundary: usize, model_id: &str) {
    // 安全上限：context_window 的 5%，兜底用
    let safety_max_tokens = self.registry.get_chat_model_config(model_id)
        .ok()
        .and_then(|cfg| cfg.context_window)
        .map(|cw| (cw / 20) as usize)
        .unwrap_or(5_000);
    
    // 1. 截断保留区内异常大的 tool results
    for i in boundary..self.session.history.len() {
        if self.session.history[i].role != "tool" {
            continue;
        }
        let text = self.session.history[i].text_content();
        let est = estimate_tokens(&text);
        if est > safety_max_tokens as u64 {
            let truncated = crate::tools::truncation::truncate_output(&text, safety_max_tokens);
            self.session.history[i].parts = vec![
                crate::providers::ContentPart::Text { text: truncated }
            ];
            
            let old_est = est;
            let new_est = estimate_tokens(&self.session.history[i].text_content()) as u64;
            self.token_tracker.adjust_for_compaction(old_est, new_est);
            
            tracing::warn!(
                idx = i,
                old_tokens = old_est,
                new_tokens = new_est,
                "safety-net truncated oversized tool result in retention zone"
            );
        }
    }
    
    // 2. 仍然超阈值？降级删除最早的一个保留 work unit
    if self.token_tracker.total_tokens() > self.calculate_threshold(model_id) {
        self.drop_oldest_retained_work_unit(boundary);
    }
}

/// 删除保留区内最早的一个完整 work unit（最后手段）。
fn drop_oldest_retained_work_unit(&mut self, boundary: usize) {
    let retained = &self.session.history[boundary..];
    let units = work_unit::extract_work_units(retained);
    
    // 至少保留 1 个 work unit，不全部删光
    if units.len() <= 1 { return; }
    
    let unit = &units[0];
    let start = boundary + unit.user_start;
    let end = boundary + unit.end + 1;
    
    let to_remove = &self.session.history[start..end];
    let removed_tokens: u64 = to_remove.iter().map(estimate_message_tokens).sum();
    
    self.session.history.drain(start..end);
    self.session.message_ids.drain(start..end);
    self.token_tracker.adjust_for_compaction(removed_tokens, 0);
    
    tracing::warn!(
        dropped_start = start,
        dropped_end = end,
        removed_tokens,
        "dropped oldest retained work unit after truncation insufficient"
    );
}
```

---

## Phase 4: 兜底截断与监控验证

### 4.1 Session 新增 compact_version 与 SummaryMetadata

**文件**: `src/agents/session_manager.rs`

```rust
/// 摘要元数据，存储在 Session 内存中，不依赖文本解析。
#[derive(Debug, Clone)]
pub struct SummaryMetadata {
    pub version: u32,
    pub token_estimate: u64,
    pub up_to_message: i64,
}

pub struct Session {
    pub key: String,
    pub history: Vec<ChatMessage>,
    pub message_ids: Vec<i64>,
    pub compact_version: u32,           // ← 新增
    pub summary_metadata: Option<SummaryMetadata>,  // ← 新增
}

impl Session {
    pub fn new(key: String) -> Self {
        Self {
            key,
            history: Vec::new(),
            message_ids: Vec::new(),
            compact_version: 0,
            summary_metadata: None,
        }
    }
}
```

**为什么不用文本解析？**

摘要中的 version、token 数量、up_to_message 等元数据如果嵌入到文本中，
解析既脆弱又容易与摘要内容本身冲突（如摘要中出现"已发生"等字样）。
用独立的内存数据结构，恢复时从数据库 `SummaryRecord` 直接填充，
避免任何文本解析。

### 4.2 SummaryRecord 加 version 字段

**文件**: `src/storage/session.rs`

```rust
/// A persisted summary record from context compaction.
#[derive(Debug, Clone)]
pub struct SummaryRecord {
    pub id: i64,              // 后端主键（可能自增）
    pub version: u32,         // ← 新增：session 级压缩版本号，由调用方传入
    pub summary: String,
    pub up_to_message: i64,
    pub token_estimate: Option<u64>,
    pub created_at: DateTime<Utc>,
}
```

**`id` 与 `version` 的职责分离**：
- `id`：后端数据库主键，用于存储定位，可能是自增的
- `version`：session 级逻辑序号，必须连续，由 `Session.compact_version + 1` 决定

`save_compaction` 时传入 `version`；`load_latest_summary` 返回的 `SummaryRecord.version` 直接用于恢复 `Session.compact_version`。

### 4.3 恢复时设置 compact_version 与 summary_metadata

**文件**: `src/agents/session_manager.rs`

不再需要文本解析，从 `SummaryRecord` 直接填充：

```rust
// get_or_create 的 summary-based recovery 分支中：
let session = match self.backend.load_latest_summary(key) {
    Some(summary) => {
        let incremental = self.backend.load_incremental(key, summary.up_to_message);
        let mut history = Vec::with_capacity(incremental.len() + 1);
        let mut message_ids = Vec::with_capacity(incremental.len() + 1);

        history.push(ChatMessage::user_text(
            format!("[Context Summary] {}", summary.summary)
        ));
        message_ids.push(0);

        for (id, msg) in incremental {
            history.push(msg);
            message_ids.push(id);
        }

        Session {
            key: key.to_string(),
            history,
            message_ids,
            compact_version: summary.version,       // ← 直接从数据库恢复
            summary_metadata: Some(SummaryMetadata { // ← 填充内存元数据
                version: summary.version,
                token_estimate: summary.token_estimate.unwrap_or(0),
                up_to_message: summary.up_to_message,
            }),
        }
    }
    // ...
};
```

### 4.3 工具输出上限调整（配合压缩策略）

**文件**: `src/tools/file_ops.rs`

```rust
fn max_output_tokens(&self) -> usize {
    10_000  // 从 50,000 降到 10,000，大文件用 offset/limit 分页
}
```

**文件**: `src/tools/web.rs`

```rust
fn max_output_tokens(&self) -> usize {
    8_000  // 从 20,000 降到 8,000
}
```

**文件**: `src/tools/shell.rs`

```rust
fn max_output_tokens(&self) -> usize {
    3_000  // 从 5,000 降到 3,000
}
```

### 4.4 Usage 解析增强（监控缓存命中）

**文件**: `src/providers/anthropic.rs`

```rust
fn parse_anthropic_sse(line: &str) -> Option<StreamEvent> {
    // ... 已有逻辑 ...
    
    match ty {
        "message" => {
            if let Some(usage) = evt.get("usage") {
                let cu = ChatUsage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    cached_input_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                    reasoning_tokens: usage.get("reasoning_tokens").and_then(|v| v.as_u64()),
                    cache_write_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()),
                };
                return Some(SE::Usage(cu));
            }
            None
        }
        // ...
    }
}
```

---

## Phase 5: 设计决策备忘

### 5.1 压缩触发时机：每次 chat_loop 迭代前

```rust
// chat_loop 内部
loop {
    let (provider, model_id) = ...;
    self.maybe_compact(&model_id).await?;  // ← 每次迭代前检查
    let mut messages = self.build_messages().await?;
    // 发送请求...
}
```

**为什么不在 `run()` 开始时只触发一次？**

如果一轮内有多次 tool call（常见场景：读取文件 → 分析 → 执行命令 → 检查结果），
每次迭代新增的 tool results 可能数千 tokens。只在 `run()` 开始时检查一次，
第 3-4 次迭代前 history 可能膨胀到超出 context window，导致 API 返回
`context_length_exceeded` 错误。

**安全性**：每次迭代前触发压缩不会打断当前推理链。因为 work unit 保留策略保护了
最近 N 个完整单元，当前迭代中的 assistant + 其 tool results 都在保留区内，
被压缩的只是更早的历史。

### 5.2 Orchestrator 可见性：当前不需要

**当前架构**：

```
Orchestrator ──▶ AgentLoop (via Arc<TokioMutex>)
                    └── session.history (私有)
                    └── PersistHook ──▶ Backend (SQLite)
```

Orchestrator 不直接操作 history，压缩事件通过 `PersistHook::save_compaction` 落到后端。
后端恢复 session 时 `load_latest_summary` 已能正确处理。当前不需要 Orchestrator
实时感知压缩。

**如果未来需要提示用户"历史已压缩"**，可扩展 `run()` 返回值：

```rust
pub struct RunResult {
    pub text: String,
    pub compaction_happened: bool,
    pub compaction_version: Option<u32>,
}
```

Orchestrator 在 `run()` 返回后检查 `compaction_happened`，向 channel 发送系统提示。
这属于 Phase 2 增强，不影响当前主干逻辑。

### 5.3 SessionManager 缓存一致性

Orchestrator 中存在两个 Session 来源：
1. `SessionManager.active` map — 用于系统消息追加
2. `AgentLoop.session` — 真实的对话历史

AgentLoop 对 `self.session.history` 的修改（包括压缩）**不会同步回 SessionManager**。
这是**已有问题**，不是压缩引入的。

当前代码中 `SessionManager.active` 仅在 `append_message`（系统消息）时使用。
AgentLoop 的消息通过 `PersistHook` 直接写后端。**确认原则**：
- 所有对 history 的写操作走 `PersistHook`
- 读操作从 `AgentLoop` 获取
- `SessionManager.active` 缓存如果失去同步价值，可考虑移除

### 5.4 摘要质量审计

摘要生成后，执行轻量级质量检查，不阻塞主流程，仅记录 warn 日志。

**文件**: `src/agents/agent_impl.rs`

```rust
/// 检查摘要是否保留了原始对话中的关键信息。
/// 返回 (通过, 原因列表)。
fn audit_summary_quality(
    to_compact: &[ChatMessage],
    summary: &str,
) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();
    
    // 检查 1：长度合理（不超过 500 字）
    if summary.chars().count() > 500 {
        reasons.push(format!(
            "summary too long: {} chars (limit 500)",
            summary.chars().count()
        ));
    }
    
    // 检查 2：原始对话中有文件路径，摘要是否保留了一些？
    let original_paths = extract_file_paths(to_compact);
    if !original_paths.is_empty() {
        let preserved = original_paths.iter()
            .filter(|p| summary.contains(*p))
            .count();
        if preserved == 0 && original_paths.len() <= 5 {
            // 路径不多但一个都没保留，可能有问题
            reasons.push(format!(
                "no file paths preserved (original had {})",
                original_paths.len()
            ));
        }
    }
    
    // 检查 3：原始对话有 tool error，摘要是否提到错误？
    let has_errors = to_compact.iter().any(|m| {
        m.role == "tool" && m.is_error == Some(true)
    });
    if has_errors {
        let mentions_error = summary.contains("错误")
            || summary.contains("error")
            || summary.contains("失败")
            || summary.contains("异常");
        if !mentions_error {
            reasons.push("original had tool errors but summary doesn't mention them");
        }
    }
    
    (reasons.is_empty(), reasons)
}

/// 从对话中提取文件路径模式（简化版）。
fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String> {
    let re = regex::Regex::new(r"(?:/[\w/.-]+\.\w{1,5})|(?:src/[\w/.-]+)").unwrap();
    let mut paths = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(0) {
                let p = m.as_str().to_string();
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
    }
    paths
}
```

在 `maybe_compact` 中摘要生成后调用：

```rust
let summary = self.summarize_inline(to_compact, existing_summary.as_deref(), model_id).await?;
if !summary.trim().is_empty() {
    let (ok, reasons) = audit_summary_quality(to_compact, &summary);
    if !ok {
        tracing::warn!(
            reasons = ?reasons,
            "summary quality audit failed (non-blocking)"
        );
    }
}
```

### 5.5 为什么不需要分块压缩

MyClaw 使用内联 summarizer（与主请求共用同一模型），待压缩内容 `to_compact` 是完整 history 的一个子集。
触发压缩时，模型刚才还在处理 `system_prompt + full_history`（已成功），summarizer 只需处理
`system_prompt + to_compact + summarizer_prompt`，**总量始终小于主请求**。
因此不存在 summarizer 超出 context window 的场景，分块压缩不适用。

对比 OpenClaw 需要分块是因为：
- OpenClaw 的 summarizer 可能使用不同模型（更小的 context window）
- OpenClaw 的 `to_compact` 可能包含跨越多次压缩周期积累的大量历史

这两个条件在 MyClaw 中都不成立。

### 5.6 Codex（OpenAI）上下文管理分析

Codex CLI 是 OpenAI 的编码 Agent。Codex 的上下文管理架构与其他项目有本质区别：

**Codex 把压缩完全委托给后端**。

OpenClaw 通过 Codex 扩展（`extensions/codex`）集成了 Codex 的 app-server 模式。
压缩流程：

```
OpenClaw: maybeCompactCodexAppServerSession()
  → client.request("thread/compact/start", { threadId })
  → 等待 "thread/compacted" 通知（最长 5 分钟超时）
  → 返回 { compacted: true, details: { backend: "codex-app-server" } }
```

Codex app-server 自己管理 thread 的上下文。压缩是一个 RPC 调用——
OpenClaw 发送 `thread/compact/start`，Codex 后端执行压缩并返回 `thread/compacted` 通知。
压缩的细节（保留多少、怎么摘要）完全在 Codex 后端内部，不对外暴露。

**对 MyClaw 的启发**：无直接借鉴意义。Codex 是"胖后端 + 瘦客户端"架构，MyClaw 是无后端 Agent。

### 5.7 Claude Code 上下文管理分析

Claude Code 是 Anthropic 的官方 CLI 编码 Agent（`src/services/compact/` 共 3960 行）。

#### Micro-compact：无损缩减 tool result

**目标**：在不调用 LLM、不改变消息结构的前提下，缩减上下文体积。

**两种触发路径**：

**路径 A：时间触发（缓存过期）**

```
条件：距离最后一次 assistant 消息 > gapThresholdMinutes（可配置）
操作：遍历所有消息，找到"可压缩的" tool_use ID（file_read/shell/grep/glob/
      web_search/web_fetch/file_edit/file_write）
      保留最近 keepRecent 个（默认 1），其余 tool_result 的 content 替换为
      "[Old tool result content cleared]"
效果：直接修改消息内容，节省 token 但丢失 tool output 细节
```

为什么按时间？因为 Anthropic 的 prompt cache 有 TTL。缓存过期后，下一次请求需要重写完整前缀。
此时 tool result 的内容已经不在缓存中了，清除它可以减少重写的 token 量。

**路径 B：计数触发（cache editing API）**

```
条件：已注册的 tool result 数量 > triggerThreshold
操作：不修改消息内容，而是通过 Anthropic 的 cache editing API（cache_edits）
      在 API 层面删除指定的 tool_result block
      本地消息保持不变，下次请求时由 API 层动态裁剪
效果：不破坏已缓存的 prompt 前缀，cache hit 率不降
```

核心区别：路径 A 改本地消息（缓存已经冷了，无所谓），路径 B 只在 API 层改（缓存还是热的，要保护）。

#### Auto-compact：LLM 摘要替代

**目标**：当上下文接近溢出时，用 LLM 生成的摘要替代全部历史消息。

**触发条件**：

```
tokenCount >= contextWindow - 13,000（AUTOCOMPACT_BUFFER_TOKENS）
```

**具体操作步骤**：

1. **剥离图片**：把 user message 中的 image/document block 替换为 `[image]`/`[document]` 文本占位符，避免摘要请求本身超出限制

2. **发送摘要请求**：
   - 通过 `runForkedAgent` 发送，复用主对话的 prompt cache（实验证实 cache miss 从 98% 显著降低）
   - 请求内容：完整历史消息 + 压缩 prompt
   - Prompt 结构：要求先在 `<analysis>` 块中分析，再在 `<summary>` 块中输出 9 个结构化段落：
     1. Primary Request and Intent
     2. Key Technical Concepts
     3. Files and Code Sections（含完整代码片段）
     4. Errors and Fixes
     5. Problem Solving
     6. All User Messages
     7. Pending Tasks
     8. Current Work
     9. Optional Next Step

3. **Prompt-too-long 自修复**：如果摘要请求本身超出 context window（`prompt_too_long`），按 API round 分组截断头部，最多重试 2 次

4. **替换历史**：生成 boundary marker（系统消息，标记压缩边界）+ summary user message，全部旧消息被丢弃

5. **压缩后重建**（关键步骤）：
   - 重新注入最近读过的文件内容（最多 5 个，每个 ≤ 5000 tokens）
   - 重新注入已调用的 skill 说明（总共 ≤ 25,000 tokens）
   - 重新注入 tool schema、agent listing、MCP instructions
   - 执行 SessionStart hooks（重新初始化状态）
   - 执行 PostCompact hooks（允许用户自定义压缩后行为）

6. **熔断器**：连续 3 次压缩失败后停止重试，避免浪费 API 调用

#### 与 MyClaw 方案的对比

| 维度 | Claude Code Auto-compact | MyClaw 新方案 |
|------|-------------------------|--------------|
| **触发阈值** | `contextWindow - 13,000`（固定缓冲区） | `contextWindow * 0.7`（比例阈值） |
| **摘要请求方式** | Forked-agent，复用主对话 cache | 内联请求，复用 system prompt 前缀 |
| **摘要输出** | 结构化 9 段（含完整代码片段），可长达 20K tokens | 简洁中文摘要，≤300 字 / 500 tokens |
| **保留策略** | 全量替换：丢弃全部旧消息，只保留摘要 + 重建附件 | 部分保留：早期历史 → 摘要，保留最近 2 个完整 work unit |
| **增量压缩** | ❌ 每次全量重做 | ✅ 只压缩新增部分，合并旧摘要 |
| **压缩后重建** | 重新注入文件/skill/tool schema（50K+ tokens） | 无（依赖摘要保留关键信息 + system prompt 持续注入） |
| **图片处理** | 剥离图片为 `[image]` 占位符 | ✅ 已加（5.9） |
| **熔断器** | 3 次失败后停止 | ✅ 已加（5.10） |
| **cache 一致性** | forked agent 复用全部参数 | ✅ 已加（5.8）两层 fallback |
| **micro-compact** | 时间/计数触发，清除旧 tool result | ❌ 无（MyClaw 的 tool 层截断已覆盖此需求） |

**关键差异的本质原因**：

Claude Code 采用**全量替换**策略（丢弃全部历史，只保留摘要 + 重建附件），
因此必须在摘要中保留尽可能多的细节（完整代码片段、全部 user messages）。
代价是摘要请求的成本很高（input ≈ 原始上下文大小，output ≈ 20K tokens）。

MyClaw 采用**部分保留**策略（丢弃早期历史，但保留最近 2 个完整 work unit）。
保留区包含最近几轮的原始对话（user 指令、assistant 回复、tool results），
所以摘要只需要覆盖更早的上下文，保留"用户目标 + 关键决策 + 文件路径 + 错误"即可。
摘要请求的 input 只是早期历史（不是全部），output 只要 500 tokens，成本低一个数量级。

简单说：Claude Code 的摘要是模型唯一的"记忆"，所以必须详尽；
MyClaw 的摘要只是补充，最近的 work unit 才是主要上下文。

### 5.8 内联 summarizer 的 cache 一致性与 fallback 策略

**Provider 的 cache key 由什么组成？**

provider 的 prompt cache 通常以请求的**完整前缀**作为 cache key，包括：
- system prompt
- tool definitions
- messages 前缀（按 token 粒度匹配）

**当前方案缺失**：`do_inline_summarize` 不发送 tool_definitions 和 thinking_config，
cache key 和主请求不一致，前缀缓存 100% miss。

**Claude Code 的做法**：forked agent 复用主请求的全部构造参数（system prompt + tools + thinking + messages prefix），
所以 cache key 完全一致，命中率高。失败时 fallback 到独立 streaming API（不同 system prompt、无 tools、disabled thinking）。

**MyClaw 的修正策略**：两层 fallback（见 3.3 代码）：

| 层级 | 模式 | tool definitions | thinking | cache | 失败场景 |
|------|------|-----------------|---------|-------|---------|
| **P1** | cache-sharing | ✅ 和主请求一致 | ✅ 和主请求一致 | 最大化命中 | API 错误、空返回 |
| **P2** | sub-delegator | 独立 system prompt | 独立 | 不命中 | inline 完全失败 |

**为什么不加"去掉 tools 的降级模式"？**

cache-sharing 失败的主因是 API 错误（超时、网络），去掉 tools 无法解决。
模型调用工具的场景已被以下机制防护：
1. Prompt 明确约束"只输出摘要，不要调用工具"
2. `max_tokens: 500` 限制输出空间
3. `collect_stream` 只收集 text，tool_calls 被忽略

因此两层 fallback 足够：cache-sharing → sub-delegator。

**实现细节**：

```rust
// cache-sharing 模式下，构造的请求和主请求在以下参数上一致：
// - system_prompt
// - tool_definitions（cache key 的一部分）
// - thinking config（某些 provider 纳入 cache key）
// - model_id
// 只有 messages 前缀和最后一条 user message 不同
```

### 5.9 图片剥离

如果 user message 包含图片（ImageUrl/ImageB64），summarizer 请求前剥离为 `[image]` 占位符：

```rust
let mut messages_for_summary = Vec::new();
for msg in to_compact {
    let mut cleaned = msg.clone();
    cleaned.parts = cleaned.parts.into_iter().map(|part| {
        match part {
            ContentPart::ImageUrl { .. } => ContentPart::Text { text: "[image]".into() },
            ContentPart::ImageB64 { .. } => ContentPart::Text { text: "[image]".into() },
            other => other,
        }
    }).collect();
    messages_for_summary.push(cleaned);
}
```

### 5.10 熔断器

连续压缩失败后停止重试，避免无限浪费 API 调用：

```rust
// AgentLoop 结构体中新增
compact_failures: usize,
const MAX_COMPACT_FAILURES: usize = 3;

// maybe_compact 入口
if self.compact_failures >= MAX_COMPACT_FAILURES {
    tracing::warn!("compaction circuit breaker active, skipping");
    return Ok(());
}

// 成功时重置
self.compact_failures = 0;

// 失败时递增
self.compact_failures += 1;
```

---

## 修改清单汇总

| 文件 | 修改类型 | 内容 |
|------|---------|------|
| `src/config/agent.rs` | 修改 | 删除 `compact_ratio`，新增 `retain_work_units`，删除 `max_tool_output_tokens`（截断统一到 tool 层） |
| `src/agents/agent_impl.rs` | 大改 | TokenTracker 不 reset + `total_tokens` 含 output + `is_fresh()` 含 system prompt、新增用户消息计入、重写 `maybe_compact`（含熔断器）、新增 `summarize_inline`（含图片剥离） + fallback + 质量审计、`truncate_retention_zone` 简化为 safety net、替换 `trim_oldest` |
| `src/agents/work_unit.rs` | 新增 | `WorkUnit`（含 `user_start`）、`extract_work_units`、`find_compaction_boundary` |
| `src/agents/session_manager.rs` | 修改 | `Session` 加 `compact_version` + `SummaryMetadata`、恢复时从 `SummaryRecord.version` 填充 |
| `src/agents/mod.rs` | 修改 | 声明 `work_unit` 模块 |
| `src/storage/session.rs` | 修改 | `SummaryRecord` 加 `version` 字段 |
| `src/tools/file_ops.rs` | 修改 | `max_output_tokens` 50K → 10K |
| `src/tools/web.rs` | 修改 | `max_output_tokens` 20K → 8K |
| `src/tools/shell.rs` | 修改 | `max_output_tokens` 5K → 3K |
| `src/providers/anthropic.rs` | 修改 | Usage 解析增加 `cache_read_input_tokens` / `cache_creation_input_tokens` |

---

## 预期效果

| 指标 | 修改前 | 修改后 |
|------|--------|--------|
| 压缩决策准确性 | 每次 reset 后盲估，误差大 | 基于真实 usage 累加，准确 |
| 工具链完整性 | 保留最后 1 条，经常断裂 | 保留最近 2 个完整 work unit |
| 单次压缩输入 | 全量历史（30K+ tokens） | 增量新增（3-5K tokens） |
| summarizer 成本 | 子代理独立请求，无缓存 | 内联请求，前缀自动缓存 |
| 超长 tool result | 单条 50K tokens | 上限 10K（tool 层截断）+ safety net 兜底 |
| 配置复杂度 | compact_threshold + compact_ratio + max_history | compact_threshold + retain_work_units |
