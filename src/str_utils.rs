//! UTF-8-safe string truncation utilities.
//!
//! Rust's `&str[..n]` and `String::truncate(n)` operate on **byte** offsets
//! and panic if `n` falls inside a multi-byte character. All functions here
//! accept **character** counts and safely convert to byte offsets.

/// Return the byte offset of the `max_chars`-th character, or the full string
/// length if the string is shorter.
///
/// ```
/// assert_eq!(char_offset("hello", 3), 3);
/// assert_eq!(char_offset("你好世界", 2), 6);  // 每个汉字 3 bytes
/// assert_eq!(char_offset("abc", 10), 3);     // shorter than limit
/// ```
pub fn char_offset(s: &str, max_chars: usize) -> usize {
    s.char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Return a `&str` slice containing at most `max_chars` characters.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    &s[..char_offset(s, max_chars)]
}

/// Truncate to `max_chars` characters, appending `"..."` if truncated.
/// Collapses to the first line.
pub fn truncate_line(s: &str, max_chars: usize) -> String {
    let first_line = s.lines().next().unwrap_or("");
    if first_line.chars().count() <= max_chars {
        first_line.to_string()
    } else {
        format!("{}...", truncate_chars(first_line, max_chars - 3))
    }
}
