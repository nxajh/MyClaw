/// Prepare text for speech synthesis.
///
/// Multi-stage cleanup so the TTS engine receives a clean spoken script:
/// 1. Strip `<think>` reasoning blocks (models emit these; users want to
///    see reasoning, not hear it).
/// 2. Strip markdown formatting (headings, emphasis, code, links, lists,
///    tables, blockquotes, horizontal rules).
/// 3. Normalize symbols to spoken words (°C → "度", % → "百分之",
///    $ → "美元", → → "到", emoji removal).
pub(crate) fn prepare_text_for_tts(input: &str) -> String {
    use regex::Regex;

    // ── Stage 1: Remove <think> blocks ──
    let re_think = Regex::new(r"(?s)<think[\s>].*?</think>").unwrap();
    let re_think_open = Regex::new(r"(?s)<think[\s>].*\z").unwrap();
    let mut text = re_think.replace_all(input, " ").to_string();
    text = re_think_open.replace_all(&text, " ").to_string();

    // ── Stage 2: Strip markdown ──
    let mut out = String::with_capacity(text.len());
    let re_heading = Regex::new(r"^#{1,6}\s*").unwrap();
    let re_quote = Regex::new(r"^\s*>\s?").unwrap();
    let re_bullet = Regex::new(r"^\s*[-*+•]\s+").unwrap();
    let re_number = Regex::new(r"^\s*\d+\.\s+").unwrap();
    let re_hr_dash = Regex::new(r"^\s*-{3,}\s*$").unwrap();
    let re_hr_star = Regex::new(r"^\s*\*{3,}\s*$").unwrap();
    let re_hr_under = Regex::new(r"^\s*_{3,}\s*$").unwrap();
    let re_link = Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap();
    let re_image = Regex::new(r"!\[[^\]]*\]\([^)]*\)").unwrap();
    let re_bold = Regex::new(r"\*\*|__").unwrap();

    for line in text.lines() {
        let mut l = line.to_string();

        if l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~") {
            continue;
        }
        l = re_heading.replace_all(&l, "").to_string();
        l = re_quote.replace_all(&l, "").to_string();
        l = re_bullet.replace_all(&l, "").to_string();
        l = re_number.replace_all(&l, "").to_string();
        if re_hr_dash.is_match(&l) || re_hr_star.is_match(&l) || re_hr_under.is_match(&l) {
            continue;
        }
        l = l.replace('|', " ");
        l = re_link.replace_all(&l, "$1").to_string();
        l = re_image.replace_all(&l, "").to_string();
        l = l.replace('`', "");
        l = re_bold.replace_all(&l, "").to_string();
        l = l.replace(['*', '_'], "");

        out.push_str(&l);
        out.push('\n');
    }
    let re_newlines = Regex::new(r"\n{3,}").unwrap();
    let result = re_newlines.replace_all(&out, "\n\n");
    let mut result = result.trim().to_string();

    // ── Stage 3: Normalize symbols for speech ──
    result = normalize_symbols_for_tts(&result);

    result
}

/// Expand common symbols and shorthand into words a TTS engine reads well.
/// Focused on Chinese-language usage patterns.
pub(crate) fn normalize_symbols_for_tts(text: &str) -> String {
    use regex::Regex;
    let mut t = text.to_string();

    // Temperature: 25°C → "25度", 25°C → "25摄氏度" (keep it short for Chinese)
    let re_temp_c = Regex::new(r"(?i)([-+]?\d+(?:\.\d+)?)\s*°\s*C\b").unwrap();
    t = re_temp_c.replace_all(&t, "${1}摄氏度").to_string();
    let re_temp_f = Regex::new(r"(?i)([-+]?\d+(?:\.\d+)?)\s*°\s*F\b").unwrap();
    t = re_temp_f.replace_all(&t, "${1}华氏度").to_string();
    // Bare degree sign → 度
    let re_degree = Regex::new(r"([-+]?\d+(?:\.\d+)?)\s*°").unwrap();
    t = re_degree.replace_all(&t, "${1}度").to_string();
    t = t.replace('°', "度");

    // Percentage: 50% → "百分之50"
    let re_percent = Regex::new(r"(\d+(?:\.\d+)?)\s*%").unwrap();
    t = re_percent.replace_all(&t, "百分之${1}").to_string();
    t = t.replace('%', "百分之");

    // Currency: $50 → "50美元", ¥50 → "50元", €50 → "50欧元", £50 → "50英镑"
    let re_usd = Regex::new(r"\$\s*(\d+(?:[,.]\d+)*)").unwrap();
    t = re_usd.replace_all(&t, "${1}美元").to_string();
    t = t.replace('¥', "元");
    t = t.replace('€', "欧元");
    t = t.replace('£', "英镑");

    // Arrows and operators
    t = t.replace('→', " 到 ");
    t = t.replace('⇒', " 到 ");
    t = t.replace('≈', " 约 ");
    t = t.replace("~", " 约 ");

    // Common separators
    t = t.replace('&', " 和 ");
    t = t.replace("•", " ");

    // Emojis — broad Unicode pictograph ranges. Most TTS engines read them
    // as awkward labels ("grinning face with smile") or skip them entirely.
    let re_emoji = Regex::new(concat!(
        "[",
        "\u{1F600}-\u{1F64F}", // emoticons
        "\u{1F300}-\u{1F5FF}", // symbols & pictographs
        "\u{1F680}-\u{1F6FF}", // transport & map
        "\u{1F700}-\u{1F77F}",
        "\u{1F780}-\u{1F7FF}",
        "\u{1F800}-\u{1F8FF}",
        "\u{1F900}-\u{1F9FF}", // supplemental symbols
        "\u{1FA00}-\u{1FAFF}",
        "\u{2600}-\u{26FF}",   // misc symbols (☀ ☂ ☎ etc.)
        "\u{2700}-\u{27BF}",   // dingbats (✂ ✅ ✨ etc.)
        "\u{1F1E6}-\u{1F1FF}", // regional indicators (flags)
        "]+",
    )).unwrap();
    t = re_emoji.replace_all(&t, " ").to_string();

    // Variation selectors (invisible formatting chars after emoji)
    let re_vs = Regex::new("[\u{FE0F}\u{FE0E}]").unwrap();
    t = re_vs.replace_all(&t, "").to_string();

    // Collapse whitespace left by removals
    let re_multi_space = Regex::new(r"[ \t]{2,}").unwrap();
    t = re_multi_space.replace_all(&t, " ").to_string();
    let re_space_punct = Regex::new(r"\s+([，。！？；：、,.!?;:])").unwrap();
    t = re_space_punct.replace_all(&t, "${1}").to_string();

    t.trim().to_string()
}

#[cfg(test)]
mod test_strip_markdown {
    use super::*;

    #[test]
    fn test_basic_stripping() {
        assert_eq!(prepare_text_for_tts("**hello**"), "hello");
        assert_eq!(prepare_text_for_tts("# 标题"), "标题");
        assert_eq!(prepare_text_for_tts("- 列表项"), "列表项");
        assert_eq!(prepare_text_for_tts("`代码`"), "代码");
        assert_eq!(prepare_text_for_tts("[链接文字](https://example.com)"), "链接文字");
    }

    #[test]
    fn test_complex() {
        let input = "## 标题\n\n**重点**和*斜体*文字。\n\n- 第一项\n- 第二项\n\n```\ncode block\n```\n";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("```"));
        assert!(!result.contains("**"));
        assert!(!result.contains("##"));
        assert!(result.contains("重点"));
        assert!(result.contains("第一项"));
    }

    #[test]
    fn test_think_block_removal() {
        let input = "<think>let me analyze this</think>你好世界";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("think"));
        assert!(!result.contains("analyze"));
        assert!(result.contains("你好世界"));
    }

    #[test]
    fn test_think_block_multiline() {
        let input = "<think>\nstep 1: do something\nstep 2: more analysis\n</think>\nThe answer is 42.";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("step 1"));
        assert!(result.contains("The answer is 42"));
    }

    #[test]
    fn test_symbol_expansion() {
        assert!(prepare_text_for_tts("温度25°C").contains("摄氏度"));
        assert!(prepare_text_for_tts("优惠50%").contains("百分之50"));
        assert!(prepare_text_for_tts("$100").contains("美元"));
        assert!(prepare_text_for_tts("¥50").contains("元"));
        assert!(prepare_text_for_tts("A → B").contains("到"));
    }

    #[test]
    fn test_emoji_removal() {
        let result = prepare_text_for_tts("你好😀世界");
        assert!(!result.contains("😀"));
        assert!(result.contains("你好"));
        assert!(result.contains("世界"));
    }

    #[test]
    fn test_combined() {
        let input = "## 📊 报告\n\n<think>分析数据中</think>\n\n**关键指标**：25°C，增长50%\n\n- 收入：$1000\n- 趋势：↑📈\n";
        let result = prepare_text_for_tts(input);
        assert!(!result.contains("📊"));
        assert!(!result.contains("📈"));
        assert!(!result.contains("think"));
        assert!(!result.contains("**"));
        assert!(!result.contains("##"));
        assert!(result.contains("报告"));
        assert!(result.contains("摄氏度"));
        assert!(result.contains("百分之50"));
        assert!(result.contains("美元"));
    }
}
