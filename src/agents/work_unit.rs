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

/// Find the earliest compaction boundary where the retained tail fits within `tail_budget`.
///
/// Walks work unit boundaries **large-to-small** (most compression → least compression).
/// Starts from maximum compression (retaining only `retain_work_units` units) and
/// incrementally grows the retained tail by moving work units back one at a time,
/// stopping as soon as the tail would exceed `tail_budget`.
///
/// This direction is optimal for the pre-fallback scenario where significant compression
/// is typically required: we validate that maximum compression is even feasible in one
/// O(n) pass over the retained tail, then back off with O(1) incremental updates per step.
///
/// Returns `Some(boundary)` where `history[..boundary]` is summarised and
/// `history[boundary..]` (tail) fits within `tail_budget`.
/// At least `retain_work_units` work units are always kept.
///
/// If even maximum compression can't satisfy the budget, returns the max-compression
/// boundary as a best-effort fallback.
///
/// Returns `None` when there are too few work units to compress anything, or when
/// `conservative_total_tokens <= tail_budget` (already fits, no compaction needed).
pub fn find_compaction_boundary_for_budget(
    history: &[ChatMessage],
    conservative_total_tokens: u64,
    tail_budget: u64,
    retain_work_units: usize,
) -> Option<usize> {
    let units = extract_work_units(history);

    if units.len() <= retain_work_units {
        return None;
    }

    if conservative_total_tokens <= tail_budget {
        return None; // already fits, no compaction needed
    }

    let max_compress_count = units.len() - retain_work_units;
    // Maximum-compression boundary: compress units[0..max_compress_count], retain the rest.
    let max_boundary = units[max_compress_count].user_start;

    // Compute tail tokens at maximum compression (O(n) scan of the retained portion).
    let mut tail_tokens: u64 = history[max_boundary..].iter().map(estimate_msg_tokens).sum();

    if tail_tokens > tail_budget {
        // Even maximum compression can't satisfy the budget — return max boundary as fallback.
        return Some(max_boundary);
    }

    // Walk from max_compress_count-1 down to 1.
    // At each step we "un-compress" one more work unit (grow the retained tail).
    // Stop when the tail would exceed tail_budget; the previous boundary is the answer.
    let mut best_boundary = max_boundary;

    for compress_count in (1..max_compress_count).rev() {
        // The unit being moved from the compressed prefix back into the retained tail
        // spans history[units[compress_count].user_start .. units[compress_count+1].user_start].
        let unit_start = units[compress_count].user_start;
        let unit_end = units[compress_count + 1].user_start;

        for msg in &history[unit_start..unit_end] {
            tail_tokens += estimate_msg_tokens(msg);
        }

        if tail_tokens > tail_budget {
            // Growing the tail further breaks the budget.
            // best_boundary (from the previous step) is the minimum-compression answer.
            return Some(best_boundary);
        }

        // Still fits with less compression — record this boundary and keep backing off.
        best_boundary = units[compress_count].user_start;
    }

    // Minimum compression (compress only unit[0]) also satisfies the budget.
    Some(units[1].user_start)
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
