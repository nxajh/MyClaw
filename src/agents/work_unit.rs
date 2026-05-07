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

/// Rough per-message token estimator (4 chars ≈ 1 token + metadata overhead).
/// Mirrors `estimate_message_tokens` in agent_impl without creating a circular import.
fn estimate_msg_tokens(msg: &ChatMessage) -> u64 {
    use crate::providers::ContentPart;
    let text_len: usize = msg.parts.iter().map(|p| match p {
        ContentPart::Text { text } => text.len(),
        ContentPart::Thinking { thinking } => thinking.len(),
        ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. } => 400,
    }).sum();
    let tool_len: usize = msg.tool_calls.as_ref().map_or(0, |tcs| {
        tcs.iter().map(|tc| tc.arguments.len() + 32).sum()
    });
    ((text_len + tool_len) as u64).div_ceil(4) + 4
}

/// Find the latest compaction boundary whose compressible prefix fits within `compress_budget`.
///
/// `compress_budget` = max input tokens the summarizer (target model) can accept in one call,
/// computed as `target_window − system_prompt_tokens − summary_output_reserve`.
///
/// Walks work unit boundaries **front to back**, accumulating compressed-prefix tokens.
/// Returns the **latest** boundary where `prefix_tokens ≤ compress_budget` — the maximum
/// amount that can be handed to the summarizer in one pass.  Everything after the boundary
/// is retained unchanged.
///
/// At least `retain_work_units` work units are always kept in the retained tail.
/// Terminates early once the prefix exceeds the budget (prefix only grows monotonically).
///
/// Returns `None` if even the smallest prefix (one work unit) exceeds the budget,
/// or if there are too few work units to compress anything.
pub fn find_compaction_boundary_for_budget(
    history: &[ChatMessage],
    compress_budget: u64,
    retain_work_units: usize,
) -> Option<usize> {
    let units = extract_work_units(history);

    if units.len() <= retain_work_units {
        return None;
    }

    let max_compress_count = units.len() - retain_work_units;

    // Walk front-to-back, accumulating prefix tokens incrementally.
    // Keep the latest boundary where the prefix still fits in compress_budget.
    let mut prefix_tokens: u64 = 0;
    let mut best_boundary: Option<usize> = None;

    for compress_count in 1..=max_compress_count {
        let prev = if compress_count == 1 { 0 } else { units[compress_count - 1].user_start };
        let boundary = units[compress_count].user_start;

        for msg in &history[prev..boundary] {
            prefix_tokens += estimate_msg_tokens(msg);
        }

        if prefix_tokens <= compress_budget {
            best_boundary = Some(boundary); // latest valid boundary so far
        } else {
            break; // prefix only grows; no later boundary can fit either
        }
    }

    best_boundary
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
        assert_eq!(units[1].user_start, 0); // 回溯到 history[0] 的 user 消息
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
