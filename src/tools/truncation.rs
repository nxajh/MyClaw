//! Tool output truncation — framework-level uniform handling.
//!
//! Preserves head and tail, replacing the middle with a truncation marker.
//! Uses UTF-8 byte counts (not character counts) for budget calculations,
//! which gives more consistent behavior across ASCII and CJK text.

use std::cmp;

/// Estimate token count (rough heuristic).
/// Uses a byte-based ratio that works better for CJK: ~4 bytes ≈ 1 token
/// for ASCII, but CJK characters are 3 bytes each and roughly 1-2 tokens
/// per character, so the byte ratio is a closer approximation than char count.
pub fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Truncate tool output text to fit within a token budget.
///
/// Strategy: preserve head (80%) and tail (15%), mark middle as truncated.
/// If text fits within the limit, return unchanged.
pub fn truncate_output(text: &str, max_tokens: usize) -> String {
    let est_tokens = approx_tokens(text);
    if est_tokens <= max_tokens {
        return text.to_string();
    }

    // Calculate byte budget from token budget.
    let max_bytes = max_tokens * 4;
    let head_bytes = max_bytes * 80 / 100;
    let tail_bytes = max_bytes * 15 / 100;

    let total_bytes = text.len();

    // Find char boundaries near the byte offsets to avoid splitting multi-byte chars.
    let head_end = find_char_boundary_near(text, head_bytes.min(total_bytes));
    let tail_start = find_char_boundary_near_from_end(text, total_bytes.saturating_sub(tail_bytes));

    let head = &text[..head_end];
    let tail = &text[tail_start..];

    let omitted_bytes = tail_start - head_end;
    let omitted_tokens = approx_tokens(&text[head_end..tail_start]);

    format!(
        "{}\n\n... [~{} bytes / ~{} tokens omitted] ...\n\n{}",
        head, omitted_bytes, omitted_tokens, tail
    )
}

/// Truncate tool output ToolResult field.
pub fn truncate_tool_result(output: &str, max_tokens: usize) -> String {
    truncate_output(output, max_tokens)
}

/// Find the nearest char boundary at or before `byte_pos`.
fn find_char_boundary_near(text: &str, byte_pos: usize) -> usize {
    if byte_pos >= text.len() {
        return text.len();
    }
    let mut pos = byte_pos;
    while !text.is_char_boundary(pos) && pos > 0 {
        pos -= 1;
    }
    pos
}

/// Find the nearest char boundary at or after `byte_pos`.
fn find_char_boundary_near_from_end(text: &str, byte_pos: usize) -> usize {
    if byte_pos >= text.len() {
        return text.len();
    }
    let mut pos = byte_pos;
    while !text.is_char_boundary(pos) && pos < text.len() {
        pos += 1;
    }
    pos
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
        assert_eq!(approx_tokens(&"a".repeat(40)), 10);
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

    #[test]
    fn test_cjk_truncation_no_split() {
        // CJK text should not split multi-byte chars at truncation boundary.
        let head = "开始标记_";
        let tail = "_结束标记";
        let middle = "中".repeat(50_000);
        let text = format!("{}{}{}", head, middle, tail);
        let result = truncate_output(&text, 5000);
        assert!(result.starts_with("开始标记_"));
        assert!(result.contains("结束标记"));
        // Result should be valid UTF-8 (no panic = success)
        let _ = result.chars().count();
    }
}
