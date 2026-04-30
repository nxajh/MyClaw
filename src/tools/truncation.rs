//! 工具输出截断 — 框架层统一处理
//!
//! 保留头部和尾部，中间用截断标记替代。

use std::cmp;

/// 估算 token 数（粗略：4 字符 ≈ 1 token）
pub fn approx_tokens(text: &str) -> usize {
    (text.len() + 3) / 4
}

/// 截断工具输出文本
///
/// 策略：保留头部 80% 和尾部 15%，中间标记截断
/// 如果文本未超过限制，原样返回
pub fn truncate_output(text: &str, max_tokens: usize) -> String {
    let est_tokens = approx_tokens(text);
    if est_tokens <= max_tokens {
        return text.to_string();
    }

    // 按 token 预算计算字符数
    let max_chars = max_tokens * 4;
    let head_chars = max_chars * 80 / 100;
    let tail_chars = max_chars * 15 / 100;

    // 确保不越界
    let char_count = text.chars().count();
    let head_chars = cmp::min(head_chars, char_count);
    let tail_chars = cmp::min(tail_chars, char_count - head_chars);

    // 按字符边界截取
    let head: String = text.chars().take(head_chars).collect();
    let tail: String = text.chars().skip(char_count - tail_chars).collect();

    let omitted_chars = char_count - head_chars - tail_chars;
    let omitted_tokens = approx_tokens(&text[head.len()..text.len() - tail.len()]);

    format!(
        "{}\n\n... [~{} chars / ~{} tokens omitted] ...\n\n{}",
        head, omitted_chars, omitted_tokens, tail
    )
}

/// 截断工具输出的 ToolResult 字段
///
/// 截断 `output` 字符串内容
pub fn truncate_tool_result(output: &str, max_tokens: usize) -> String {
    truncate_output(output, max_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_text_unchanged() {
        let text = "hello world";
        assert_eq!(truncate_output(text, 10000), text);
    }

    #[test]
    fn test_long_text_truncated() {
        let text = "a".repeat(100_000); // ~25000 tokens
        let result = truncate_output(&text, 5000);
        assert!(result.len() < text.len());
        assert!(result.contains("omitted"));
    }

    #[test]
    fn test_preserves_head_and_tail() {
        let head = "START_MARKER_";
        let tail = "_END_MARKER";
        let middle = "x".repeat(100_000);
        let text = format!("{}{}{}", head, middle, tail);
        let result = truncate_output(&text, 5000);
        assert!(result.starts_with("START_MARKER_"));
        assert!(result.contains("END_MARKER"));
    }

    #[test]
    fn test_approx_tokens() {
        assert_eq!(approx_tokens("hello"), 2); // 5 chars → 2 tokens
        assert_eq!(approx_tokens("a".repeat(40).as_str()), 10);
    }

    #[test]
    fn test_truncate_tool_result() {
        let text = "a".repeat(100_000);
        let result = truncate_tool_result(&text, 5000);
        assert!(result.contains("omitted"));
    }

    #[test]
    fn test_boundary_exact_limit() {
        // Exactly at limit — should NOT truncate
        let text = "a".repeat(40_000); // 10000 tokens
        let result = truncate_output(&text, 10_000);
        assert_eq!(result, text);
    }
}
