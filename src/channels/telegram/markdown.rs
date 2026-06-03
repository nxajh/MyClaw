/// Escape HTML special characters for Telegram's HTML parse mode.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Inline formatting tags. Managed with a stack so the emitted HTML is always
/// properly nested, even when the markdown markers overlap
/// (e.g. `~~a **b~~ c**`), which Telegram's HTML parser would otherwise reject.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Bold,
    Italic,
    Strike,
}

impl Tag {
    fn open(self) -> &'static str {
        match self {
            Tag::Bold => "<b>",
            Tag::Italic => "<i>",
            Tag::Strike => "<s>",
        }
    }
    fn close(self) -> &'static str {
        match self {
            Tag::Bold => "</b>",
            Tag::Italic => "</i>",
            Tag::Strike => "</s>",
        }
    }
}

fn open_tag(stack: &mut Vec<Tag>, out: &mut String, tag: Tag) {
    stack.push(tag);
    out.push_str(tag.open());
}

/// Close `tag`, temporarily closing and reopening any tags stacked above it so
/// the result stays properly nested.
fn close_tag(stack: &mut Vec<Tag>, out: &mut String, tag: Tag) {
    let Some(pos) = stack.iter().rposition(|&t| t == tag) else {
        return;
    };
    let above = stack.split_off(pos + 1);
    for &t in above.iter().rev() {
        out.push_str(t.close());
    }
    out.push_str(tag.close());
    stack.pop(); // remove the target tag itself
    for &t in &above {
        out.push_str(t.open());
        stack.push(t);
    }
}

/// Convert LLM Markdown output to Telegram-supported HTML.
///
/// Supports: bold, italic, strikethrough, inline code, code blocks (with optional language),
/// headings, links, blockquotes, and horizontal rules.
///
/// Formatting inside code blocks and inline code is preserved as-is (no nested parsing).
pub fn markdown_to_telegram_html(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() * 2);

    // Stack of currently-open inline tags, kept properly nested.
    let mut stack: Vec<Tag> = Vec::new();

    let chars: Vec<char> = markdown.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // ── Fenced code block (```) ─────────────────────────────────────
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            // Collect the optional language identifier (e.g. "rust", "python").
            let mut lang = String::new();
            let mut j = i + 3;
            while j < len && chars[j] != '\n' {
                lang.push(chars[j]);
                j += 1;
            }
            // Skip the newline after the opening fence.
            if j < len && chars[j] == '\n' {
                j += 1;
            }
            // Find the closing fence.
            let start = j;
            while j < len {
                if j + 2 < len && chars[j] == '`' && chars[j + 1] == '`' && chars[j + 2] == '`' {
                    break;
                }
                j += 1;
            }
            let mut code: String = chars[start..j].iter().collect();
            // Trim exactly one trailing newline (common in fenced blocks).
            if code.ends_with('\n') {
                code.pop();
            }
            let escaped = escape_html(&code);
            let trimmed_lang = lang.trim();
            // Treat empty or "text" as no language.
            let has_lang = !trimmed_lang.is_empty() && trimmed_lang != "text";
            if !has_lang {
                out.push_str(&format!("<pre>{}</pre>", escaped));
            } else {
                out.push_str(&format!(
                    "<pre><code class=\"language-{}\">{}</code></pre>",
                    trimmed_lang, escaped
                ));
            }
            // Advance past the closing fence.
            i = if j + 3 <= len { j + 3 } else { len };
            continue;
        }

        // ── Inline code (`) ─────────────────────────────────────────────
        if chars[i] == '`' {
            let end = chars[i + 1..]
                .iter()
                .position(|&c| c == '`')
                .map(|p| i + 1 + p);
            if let Some(e) = end {
                let code: String = chars[i + 1..e].iter().collect();
                out.push_str(&format!("<code>{}</code>", escape_html(&code)));
                i = e + 1;
                continue;
            }
            // No closing backtick — treat as literal.
            out.push('`');
            i += 1;
            continue;
        }

        // ── Headings (# …) → bold ───────────────────────────────────────
        if chars[i] == '#' {
            let mut j = i;
            while j < len && chars[j] == '#' {
                j += 1;
            }
            // Must have a space after the hashes, and be at line start.
            if j < len && chars[j] == ' ' && (i == 0 || chars[i - 1] == '\n') {
                // Skip leading space.
                j += 1;
                let line_start = j;
                while j < len && chars[j] != '\n' {
                    j += 1;
                }
                let heading_text: String = chars[line_start..j].iter().collect();
                out.push_str(&format!("<b>{}</b>", escape_html(heading_text.trim())));
                if j < len {
                    out.push('\n');
                    j += 1;
                }
                i = j;
                continue;
            }
        }

        // ── Blockquote (> …) ────────────────────────────────────────────
        if chars[i] == '>' && (i == 0 || chars[i - 1] == '\n') {
            let mut j = i + 1;
            if j < len && chars[j] == ' ' {
                j += 1;
            }
            let line_start = j;
            while j < len && chars[j] != '\n' {
                j += 1;
            }
            let quote_text: String = chars[line_start..j].iter().collect();
            out.push_str(&format!("❝ {}", escape_html(&quote_text)));
            if j < len {
                out.push('\n');
                j += 1;
            }
            i = j;
            continue;
        }

        // ── Horizontal rule (---, ***, ___) → ───────────────────────────
        if (chars[i] == '-' || chars[i] == '*' || chars[i] == '_')
            && (i == 0 || chars[i - 1] == '\n')
        {
            let c = chars[i];
            let mut j = i;
            while j < len && chars[j] == c {
                j += 1;
            }
            // Must be at least 3 repeats, followed by newline or EOF, with only whitespace.
            if j - i >= 3 {
                let rest: String = chars[i..j].iter().collect();
                if rest.chars().all(|ch| ch == c || ch == ' ' || ch == '\t')
                    && (j >= len || chars[j] == '\n')
                {
                    out.push_str("───");
                    if j < len {
                        out.push('\n');
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }

        // ── Links [text](url) ───────────────────────────────────────────
        if chars[i] == '[' {
            if let Some(bracket_end) = chars[i + 1..].iter().position(|&c| c == ']') {
                let real_bracket = i + 1 + bracket_end;
                if real_bracket + 1 < len && chars[real_bracket + 1] == '(' {
                    if let Some(paren_end) = chars[real_bracket + 2..]
                        .iter()
                        .position(|&c| c == ')')
                    {
                        let real_paren = real_bracket + 2 + paren_end;
                        let link_text: String = chars[i + 1..real_bracket].iter().collect();
                        let link_url: String =
                            chars[real_bracket + 2..real_paren].iter().collect();
                        out.push_str(&format!(
                            "<a href=\"{}\">{}</a>",
                            escape_html(&link_url),
                            escape_html(&link_text)
                        ));
                        i = real_paren + 1;
                        continue;
                    }
                }
            }
        }

        // ── Strikethrough (~~) ──────────────────────────────────────────
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if stack.contains(&Tag::Strike) {
                close_tag(&mut stack, &mut out, Tag::Strike);
            } else {
                open_tag(&mut stack, &mut out, Tag::Strike);
            }
            i += 2;
            continue;
        }

        // ── Bold (**) ───────────────────────────────────────────────────
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if stack.contains(&Tag::Bold) {
                close_tag(&mut stack, &mut out, Tag::Bold);
            } else {
                open_tag(&mut stack, &mut out, Tag::Bold);
            }
            i += 2;
            continue;
        }

        // ── Italic (* or _) ─────────────────────────────────────────────
        // Must be preceded by whitespace/start and followed by non-whitespace,
        // or preceded by non-whitespace and followed by whitespace/end.
        if (chars[i] == '*' || chars[i] == '_') && !stack.contains(&Tag::Bold) {
            let prev_ok = i == 0
                || chars[i - 1].is_whitespace()
                || chars[i - 1].is_ascii_punctuation();
            let next_ok = i + 1 < len && !chars[i + 1].is_whitespace();

            if stack.contains(&Tag::Italic) {
                // Closing: must be preceded by non-whitespace.
                let prev_non_ws = i > 0 && !chars[i - 1].is_whitespace();
                if prev_non_ws {
                    close_tag(&mut stack, &mut out, Tag::Italic);
                    i += 1;
                    continue;
                }
            } else if prev_ok && next_ok {
                open_tag(&mut stack, &mut out, Tag::Italic);
                i += 1;
                continue;
            }
        }

        // ── Plain text (escape HTML) ────────────────────────────────────
        match chars[i] {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
        i += 1;
    }

    // Close any tags still open at the end, innermost first.
    while let Some(t) = stack.pop() {
        out.push_str(t.close());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that all `<b>/<i>/<s>` tags in `html` are properly nested (a
    /// closing tag always matches the most recently opened one).
    fn tags_well_nested(html: &str) -> bool {
        let mut stack: Vec<&str> = Vec::new();
        let bytes = html.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'<' {
                if let Some(close) = html[i..].find('>') {
                    let tag = &html[i..i + close + 1];
                    match tag {
                        "<b>" | "<i>" | "<s>" => stack.push(tag),
                        "</b>" | "</i>" | "</s>" => {
                            let want = match tag {
                                "</b>" => "<b>",
                                "</i>" => "<i>",
                                _ => "<s>",
                            };
                            match stack.pop() {
                                Some(top) if top == want => {}
                                _ => return false,
                            }
                        }
                        _ => {}
                    }
                    i += close + 1;
                    continue;
                }
            }
            i += 1;
        }
        stack.is_empty()
    }

    #[test]
    fn basic_emphasis() {
        assert_eq!(markdown_to_telegram_html("**bold**"), "<b>bold</b>");
        assert_eq!(markdown_to_telegram_html("~~gone~~"), "<s>gone</s>");
        assert_eq!(markdown_to_telegram_html("a *it* b"), "a <i>it</i> b");
    }

    #[test]
    fn nested_emphasis_is_well_formed() {
        let html = markdown_to_telegram_html("~~strike **bold** more~~");
        assert!(tags_well_nested(&html), "not well nested: {html}");
        assert_eq!(html, "<s>strike <b>bold</b> more</s>");
    }

    #[test]
    fn overlapping_markers_are_reanchored() {
        // Overlapping (improperly nested) markdown must still yield valid,
        // properly-nested HTML via close-and-reopen.
        let html = markdown_to_telegram_html("~~a **b~~ c**");
        assert!(tags_well_nested(&html), "not well nested: {html}");
    }

    #[test]
    fn unclosed_tags_auto_close() {
        let html = markdown_to_telegram_html("**bold and ~~strike");
        assert!(tags_well_nested(&html), "not well nested: {html}");
    }

    #[test]
    fn code_block_preserved() {
        let html = markdown_to_telegram_html("```\nlet x = 1;\n```");
        assert_eq!(html, "<pre>let x = 1;</pre>");
    }

    #[test]
    fn link_rendered() {
        assert_eq!(
            markdown_to_telegram_html("[hi](http://e.com)"),
            "<a href=\"http://e.com\">hi</a>"
        );
    }
}
