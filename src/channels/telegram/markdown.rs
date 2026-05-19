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

/// Convert LLM Markdown output to Telegram-supported HTML.
///
/// Supports: bold, italic, strikethrough, inline code, code blocks (with optional language),
/// headings, links, blockquotes, and horizontal rules.
///
/// Formatting inside code blocks and inline code is preserved as-is (no nested parsing).
pub fn markdown_to_telegram_html(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() * 2);

    // Tracks which inline formatting tags are currently open.
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;

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
            if strike {
                out.push_str("</s>");
                strike = false;
            } else {
                out.push_str("<s>");
                strike = true;
            }
            i += 2;
            continue;
        }

        // ── Bold (**) ───────────────────────────────────────────────────
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if bold {
                out.push_str("</b>");
                bold = false;
            } else {
                out.push_str("<b>");
                bold = true;
            }
            i += 2;
            continue;
        }

        // ── Italic (* or _) ─────────────────────────────────────────────
        // Must be preceded by whitespace/start and followed by non-whitespace,
        // or preceded by non-whitespace and followed by whitespace/end.
        if (chars[i] == '*' || chars[i] == '_') && !bold {
            let prev_ok = i == 0
                || chars[i - 1].is_whitespace()
                || chars[i - 1].is_ascii_punctuation();
            let next_ok = i + 1 < len && !chars[i + 1].is_whitespace();

            if italic {
                // Closing: must be preceded by non-whitespace.
                let prev_non_ws = i > 0 && !chars[i - 1].is_whitespace();
                if prev_non_ws {
                    out.push_str("</i>");
                    italic = false;
                    i += 1;
                    continue;
                }
            } else if prev_ok && next_ok {
                out.push_str("<i>");
                italic = true;
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

    // Close any tags still open at the end.
    if strike {
        out.push_str("</s>");
    }
    if italic {
        out.push_str("</i>");
    }
    if bold {
        out.push_str("</b>");
    }

    out
}
