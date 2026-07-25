//! QQ Bot markdown `$` sanitization (msg_type=2).
//!
//! QQ treats bare `$` / `$$` as formula delimiters. Model output often uses `$99`
//! for currency, which mis-pairs and breaks rendering. This module implements
//! **tier A**: protect real math and code, escape remaining bare `$` to `\$`.
//!
//! Pipeline: sanitize **before** `split_message_chunk`. Idempotent on `\$`.

/// Max codepoint length of `$...$` body still considered for inline math.
const MAX_INLINE_MATH_CHARS: usize = 200;

/// Escape bare `$` for QQ markdown so currency does not trigger formula mode,
/// while preserving:
/// - fenced code (line-level `` ``` `` / `~~~`)
/// - inline code
/// - already-escaped `\$`
/// - display math `$$...$$`
/// - inline math `$...$` when content looks like math (not pure currency)
pub fn sanitize_qq_markdown_dollars(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len().saturating_add(16));
    let mut i = 0usize;
    let mut in_fence = false;
    let mut fence_marker = '`';
    let mut fence_len = 3usize;

    while i < n {
        let at_line_start = i == 0 || chars[i - 1] == '\n';

        if at_line_start {
            if let Some(fl) = parse_fence_line_at(&chars, i) {
                if in_fence {
                    if fl.is_closing_candidate
                        && fl.marker == fence_marker
                        && fl.len >= fence_len
                    {
                        emit_slice(&chars, i, fl.after_line, &mut out);
                        i = fl.after_line;
                        in_fence = false;
                        continue;
                    }
                } else {
                    emit_slice(&chars, i, fl.after_line, &mut out);
                    i = fl.after_line;
                    in_fence = true;
                    fence_marker = fl.marker;
                    fence_len = fl.len;
                    continue;
                }
            }
        }

        if in_fence {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        // Inline code: one or two backticks (triple mid-line is plain text here).
        if chars[i] == '`' {
            let mut j = i;
            while j < n && chars[j] == '`' {
                j += 1;
            }
            let run = j - i;
            if run >= 3 {
                for _ in 0..run {
                    out.push('`');
                }
                i = j;
                continue;
            }
            // Emit opener and scan for matching closer.
            for _ in 0..run {
                out.push('`');
            }
            i = j;
            while i < n {
                if chars[i] == '`' {
                    let mut k = i;
                    while k < n && chars[k] == '`' {
                        k += 1;
                    }
                    if k - i == run {
                        for _ in 0..run {
                            out.push('`');
                        }
                        i = k;
                        break;
                    }
                }
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        // Already escaped: \$
        if chars[i] == '\\' && i + 1 < n && chars[i + 1] == '$' {
            out.push('\\');
            out.push('$');
            i += 2;
            continue;
        }

        if chars[i] == '$' {
            // Display math $$...$$
            if i + 1 < n && chars[i + 1] == '$' {
                if let Some(close) = find_closing_display(&chars, i + 2) {
                    emit_slice(&chars, i, close + 2, &mut out);
                    i = close + 2;
                    continue;
                }
                // Unclosed $$ → literal dollars.
                out.push('\\');
                out.push('$');
                out.push('\\');
                out.push('$');
                i += 2;
                continue;
            }

            // Currency-looking open ($ + optional spaces + digit): always literal.
            // Never allow pairing as inline math — multi-amount prose like
            // `$24.45…**$5万亿**…**$1,800**` was previously glued into one
            // false math span by looks_like_math.
            if is_currency_looking_open(&chars, i) {
                out.push('\\');
                out.push('$');
                i += 1;
                continue;
            }

            // Inline $...$ (same line, bounded length).
            if let Some(close) = find_closing_inline_dollar(&chars, i + 1) {
                let body_len = close - (i + 1);
                if body_len <= MAX_INLINE_MATH_CHARS {
                    let body: String = chars[i + 1..close].iter().collect();
                    if is_currency_only(&body) {
                        // $123$ / $1,234.56$ → escape both delimiters.
                        out.push('\\');
                        out.push('$');
                        emit_slice(&chars, i + 1, close, &mut out);
                        out.push('\\');
                        out.push('$');
                        i = close + 1;
                        continue;
                    }
                    if looks_like_math(&body) {
                        emit_slice(&chars, i, close + 1, &mut out);
                        i = close + 1;
                        continue;
                    }
                }
            }

            // Bare / unpaired / non-math pair open: escape.
            out.push('\\');
            out.push('$');
            i += 1;
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

fn emit_slice(chars: &[char], start: usize, end: usize, out: &mut String) {
    for c in chars.iter().take(end).skip(start) {
        out.push(*c);
    }
}

struct FenceLineAt {
    marker: char,
    len: usize,
    is_closing_candidate: bool,
    /// Codepoint index after the line (past trailing `\n` if any).
    after_line: usize,
}

/// Line-level fence at codepoint `start` (must be line start).
fn parse_fence_line_at(chars: &[char], start: usize) -> Option<FenceLineAt> {
    let n = chars.len();
    let mut i = start;
    let mut spaces = 0usize;
    while i < n && spaces < 3 && chars[i] == ' ' {
        spaces += 1;
        i += 1;
    }
    if i >= n {
        return None;
    }
    let marker = chars[i];
    if marker != '`' && marker != '~' {
        return None;
    }
    let mark_start = i;
    while i < n && chars[i] == marker {
        i += 1;
    }
    let len = i - mark_start;
    if len < 3 {
        return None;
    }
    // Rest of line until \n.
    let rest_start = i;
    while i < n && chars[i] != '\n' {
        i += 1;
    }
    let rest = &chars[rest_start..i];
    let is_closing_candidate = rest.iter().all(|c| *c == ' ' || *c == '\t');
    // Backtick fence info may not contain backticks (CM).
    if marker == '`' && rest.contains(&'`') && !is_closing_candidate {
        return None;
    }
    let after_line = if i < n && chars[i] == '\n' { i + 1 } else { i };
    Some(FenceLineAt {
        marker,
        len,
        is_closing_candidate,
        after_line,
    })
}

/// Index of the first `$` of a closing `$$`, or None.
fn find_closing_display(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == '\\' {
            i = (i + 2).min(chars.len());
            continue;
        }
        if chars[i] == '$' && chars[i + 1] == '$' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of closing inline `$` on the same line (not part of `$$`).
fn find_closing_inline_dollar(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '\n' {
            return None;
        }
        if chars[i] == '\\' {
            i = (i + 2).min(chars.len());
            continue;
        }
        if chars[i] == '$' {
            if i + 1 < chars.len() && chars[i + 1] == '$' {
                return None;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `$` at `i` followed by optional spaces then a digit (currency open).
fn is_currency_looking_open(chars: &[char], i: usize) -> bool {
    let mut j = i + 1;
    while j < chars.len() && chars[j] == ' ' {
        j += 1;
    }
    j < chars.len() && chars[j].is_ascii_digit()
}

/// Pure currency body between dollars, e.g. `99`, `1,234.56`, ` 12.5 `.
fn is_currency_only(content: &str) -> bool {
    let t = content.trim();
    if t.is_empty() {
        return false;
    }
    // digits with optional group commas and optional fractional part
    let b = t.as_bytes();
    let mut i = 0usize;
    let mut saw_digit = false;
    let mut saw_dot = false;
    // leading digits / groups
    while i < b.len() {
        if b[i].is_ascii_digit() {
            saw_digit = true;
            i += 1;
            continue;
        }
        if b[i] == b',' && saw_digit {
            // expect exactly 3 digits after comma for grouping (lenient: at least 1)
            i += 1;
            let mut g = 0;
            while i < b.len() && b[i].is_ascii_digit() {
                g += 1;
                i += 1;
            }
            if g == 0 {
                return false;
            }
            continue;
        }
        if b[i] == b'.' && !saw_dot && saw_digit {
            saw_dot = true;
            i += 1;
            let mut f = 0;
            while i < b.len() && b[i].is_ascii_digit() {
                f += 1;
                i += 1;
            }
            if f == 0 {
                return false;
            }
            continue;
        }
        return false;
    }
    saw_digit
}

/// Heuristic: paired `$...$` body looks like math rather than prose/currency.
///
/// Tightened after multi-amount Chinese prose was misclassified as math because
/// of ASCII words (e.g. SWIFT) and markdown `*` bold markers.
fn looks_like_math(content: &str) -> bool {
    if content.is_empty() || is_currency_only(content) {
        return false;
    }
    // CJK prose between dollars is never treated as math.
    if content.chars().any(|c| {
        ('\u{4e00}'..='\u{9fff}').contains(&c)
            || ('\u{3400}'..='\u{4dbf}').contains(&c)
            || ('\u{f900}'..='\u{faff}').contains(&c)
    }) {
        return false;
    }
    // LaTeX commands always count as math.
    if content.contains('\\') {
        return true;
    }
    let mut has_alpha = false;
    let mut has_digit = false;
    let mut has_strong = false;
    let mut has_weak_op = false;
    for c in content.chars() {
        if c.is_ascii_alphabetic() {
            has_alpha = true;
        }
        if c.is_ascii_digit() {
            has_digit = true;
        }
        // Strong math tokens. Do NOT count `*` (markdown bold) or lone `-`.
        if matches!(
            c,
            '^' | '_'
                | '='
                | '{'
                | '}'
                | '['
                | ']'
                | '|'
                | '·'
                | '×'
                | '÷'
                | '±'
                | '<'
                | '>'
        ) {
            has_strong = true;
        }
        if matches!(c, '+' | '/' | '(' | ')') {
            has_weak_op = true;
        }
    }
    // $E=mc^2$, $x^2+1$, $a+b$ — need letter(s) plus math structure.
    if has_strong {
        return true;
    }
    if has_alpha && (has_weak_op || has_digit) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_bare_currency() {
        assert_eq!(sanitize_qq_markdown_dollars("价 $99"), "价 \\$99");
        assert_eq!(
            sanitize_qq_markdown_dollars("价 $ 99 元"),
            "价 \\$ 99 元"
        );
        assert_eq!(
            sanitize_qq_markdown_dollars("$1,234.56"),
            "\\$1,234.56"
        );
    }

    #[test]
    fn idempotent_on_already_escaped() {
        assert_eq!(sanitize_qq_markdown_dollars("\\$99"), "\\$99");
        assert_eq!(
            sanitize_qq_markdown_dollars(&sanitize_qq_markdown_dollars("价 $99")),
            "价 \\$99"
        );
    }

    #[test]
    fn protects_display_math() {
        let s = "块 $$a^2+b^2=c^2$$ 尾";
        assert_eq!(sanitize_qq_markdown_dollars(s), s);
    }

    #[test]
    fn protects_inline_math() {
        let s = "式 $E=mc^2$ 尾";
        assert_eq!(sanitize_qq_markdown_dollars(s), s);
        let s2 = "式 $x^2+1$ 尾";
        assert_eq!(sanitize_qq_markdown_dollars(s2), s2);
    }

    #[test]
    fn currency_wrapped_in_dollars_escaped() {
        assert_eq!(
            sanitize_qq_markdown_dollars("只有 $123$ 刀"),
            "只有 \\$123\\$ 刀"
        );
        assert_eq!(
            sanitize_qq_markdown_dollars("$1,234.56$"),
            "\\$1,234.56\\$"
        );
    }

    #[test]
    fn dual_bare_currency_no_false_math() {
        let out = sanitize_qq_markdown_dollars("售价 $99 现价 $199 结束");
        assert_eq!(out, "售价 \\$99 现价 \\$199 结束");
    }

    #[test]
    fn money_plus_math_mix() {
        let out = sanitize_qq_markdown_dollars("价格 $99，公式 $x^2+1$，块 $$y=1$$");
        assert_eq!(
            out,
            "价格 \\$99，公式 $x^2+1$，块 $$y=1$$"
        );
    }

    #[test]
    fn formula_then_money() {
        let out = sanitize_qq_markdown_dollars("$a+b$ 然后 $50");
        assert_eq!(out, "$a+b$ 然后 \\$50");
    }

    #[test]
    fn leaves_fenced_code_untouched() {
        let s = "pre\n```\n$99\n$x$\n```\npost $1";
        let out = sanitize_qq_markdown_dollars(s);
        assert!(out.contains("```\n$99\n$x$\n```"), "{out}");
        assert!(out.ends_with("post \\$1") || out.contains("post \\$1"), "{out}");
    }

    #[test]
    fn leaves_inline_code_untouched() {
        let out = sanitize_qq_markdown_dollars("用 `$var` 与 $99");
        assert_eq!(out, "用 `$var` 与 \\$99");
    }

    #[test]
    fn does_not_touch_formula_body_slash_dollar() {
        // We do not rewrite inside protected math; body kept as authored.
        let s = r"$f=\$x$";
        assert_eq!(sanitize_qq_markdown_dollars(s), s);
    }

    #[test]
    fn multi_amount_chinese_prose_all_escaped() {
        // Regression: $24.45 … **$5** … **$1,800** was glued into false math.
        let inp = "CIPS的交易量$24.45万亿听起来很大，但对比一下——SWIFT日均交易量超过**$5万亿**，一年是**$1,800万亿+**。CIPS只占SWIFT的~1.3%。";
        let out = sanitize_qq_markdown_dollars(inp);
        assert_eq!(
            out,
            "CIPS的交易量\\$24.45万亿听起来很大，但对比一下——SWIFT日均交易量超过**\\$5万亿**，一年是**\\$1,800万亿+**。CIPS只占SWIFT的~1.3%。"
        );
        assert!(!out.contains("$24.45"), "bare $24.45 must not remain: {out}");
        assert!(!out.contains("**$5"), "bare **$5 must not remain: {out}");
        assert!(!out.contains("**$1,800"), "bare **$1,800 must not remain: {out}");
    }

    #[test]
    fn bold_and_tilde_currency() {
        assert_eq!(
            sanitize_qq_markdown_dollars("一年是**$1,800万亿+**。"),
            "一年是**\\$1,800万亿+**。"
        );
        assert_eq!(
            sanitize_qq_markdown_dollars("已完成~$550亿交易"),
            "已完成~\\$550亿交易"
        );
        assert_eq!(
            sanitize_qq_markdown_dollars("冻结$3,000亿储备"),
            "冻结\\$3,000亿储备"
        );
    }

    #[test]
    fn currency_open_never_pairs_through_markdown() {
        // Even with ASCII words and ** between amounts, each $digit is escaped.
        let out = sanitize_qq_markdown_dollars("about $12 trillion and **$8** more");
        assert_eq!(out, "about \\$12 trillion and **\\$8** more");
    }
}
