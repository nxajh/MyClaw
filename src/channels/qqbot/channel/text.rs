/// Estimated characters per visual line in QQ mobile markdown (~19-20 CJK).
const QQ_CHARS_PER_LINE: f64 = 20.0;

/// Maximum estimated visual lines per QQ message bubble.
///
/// QQ's markdown renderer corrupts spacing at ~35-40 visual lines (tested
/// empirically with short/medium/long paragraphs). 30 provides a safe margin.
pub(super) const QQ_MAX_VISUAL_LINES_PER_BUBBLE: usize = 30;

/// Estimate display width: CJK = 1.0, ASCII = 0.5.
pub(super) fn display_width(s: &str) -> f64 {
    s.chars()
        .map(|c| if c.is_ascii() { 0.5 } else { 1.0 })
        .sum()
}

/// Estimate visual lines a text block occupies on QQ mobile.
pub(super) fn estimate_visual_lines(text: &str) -> usize {
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                1
            } else {
                let w = display_width(line);
                ((w / QQ_CHARS_PER_LINE).ceil() as usize).max(1)
            }
        })
        .sum()
}

/// Pre-split text so each bubble stays under `max_lines` estimated visual lines.
///
/// Splits land on `\n\n` boundaries (never inside fenced code blocks). Each
/// part's visual-line cost is estimated from CJK/ASCII display widths.
/// GFM table rows are kept together: if the current buffer ends with a table
/// line and the next part starts with one, the split is deferred so the table
/// is not broken across bubbles.
pub(super) fn split_by_visual_lines(text: &str, max_lines: usize) -> Vec<String> {
    let parts: Vec<&str> = text.split("\n\n").collect();

    // Pre-compute each part's visual-line cost.
    // Non-first parts include a 1-line gap (the \n\n separator).
    let costs: Vec<usize> = parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let gap = if i == 0 { 0 } else { 1 };
            gap + estimate_visual_lines(part)
        })
        .collect();

    let total: usize = costs.iter().sum();
    if total <= max_lines {
        return vec![text.to_string()];
    }

    // Accumulate parts, splitting when cumulative cost exceeds max_lines.
    // Never split inside a fenced code block, and never split between
    // adjacent GFM table rows that happen to be separated by a blank line.
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_cost = 0usize;
    let mut in_code = false;

    for (i, part) in parts.iter().enumerate() {
        let started_in_code = in_code;
        for line in part.lines() {
            if is_fence_line(line) {
                in_code = !in_code;
            }
        }

        // Detect a GFM table continuation: current ends with a table row and
        // the incoming part starts with one. A blank line inside a table
        // (produced by some generators) should not cause a mid-table split.
        let breaks_table = !current.is_empty()
            && current.lines().last().is_some_and(is_gfm_table_line)
            && part.lines().next().is_some_and(is_gfm_table_line);

        // Split before this part if it would overflow and we're outside code,
        // unless doing so would break a table continuation.
        if !current.is_empty()
            && !started_in_code
            && !breaks_table
            && current_cost + costs[i] > max_lines
        {
            chunks.push(current.trim_end().to_string());
            current.clear();
            current_cost = 0;
        }

        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(part);
        current_cost += costs[i];
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_string());
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

/// Check if a line is part of a GFM (GitHub Flavored Markdown) table.
///
/// A pipe-table row starts with `|` (the most common form). Separator rows
/// without a leading pipe (e.g. `---|:---:|---`) are also recognised.
pub(super) fn is_gfm_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Pipe table row: | col1 | col2 |  (trailing pipe optional)
    if trimmed.starts_with('|') {
        return true;
    }
    // Separator-only row without leading pipe: --- | :---: | ---
    let mut has_dash = false;
    for c in trimmed.chars() {
        match c {
            '-' => has_dash = true,
            '|' | ':' | ' ' | '\t' => {}
            _ => return false,
        }
    }
    has_dash
}

/// Check if a line is a markdown fence (``` or ~~~).
fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Strip model reasoning/thinking content and framework scaffolding tags.
pub(super) fn strip_internal_tags(text: &str) -> String {
    let patterns: &[(&str, &str)] = &[
        // XML-style thinking tags
        ("<thinking>", "</thinking>"),
        ("<think>", "</think>"),
        ("<system-reminder>", "</system-reminder>"),
        ("<previous_response>", "</previous_response>"),
    ];

    let mut result = text.to_string();
    for (open, close) in patterns {
        // Remove complete blocks
        while let Some(start) = result.find(open) {
            if let Some(end) = result[start..].find(close) {
                let end_abs = start + end + close.len();
                result.replace_range(start..end_abs, "");
            } else {
                // Unclosed tag — remove from start to end of string
                result.replace_range(start.., "");
                break;
            }
        }
        // Remove standalone tags
        result = result.replace(open, "").replace(close, "");
    }

    // Deepseek format: `think`...`/think`
    while let Some(start) = result.find("`think`") {
        if let Some(end) = result[start..].find("`/think`") {
            let end_abs = start + end + "`/think`".len();
            result.replace_range(start..end_abs, "");
        } else {
            break;
        }
    }

    result.trim().to_string()
}
