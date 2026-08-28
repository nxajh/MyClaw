use super::*;

use super::text::{
    display_width, estimate_visual_lines, is_gfm_table_line, split_by_visual_lines,
    strip_internal_tags,
};
use crate::channels::message::{
    ChannelInboundMessage, ChannelMessageContent, MessageReceiver, MessageSender,
};

#[test]
fn split_short_text_no_split() {
    let text = "para1\n\npara2\n\npara3";
    assert_eq!(split_by_visual_lines(text, 30), vec![text]);
}

#[test]
fn split_long_short_paragraphs() {
    // 25 short CJK paragraphs → each ~2 visual lines + gap = ~75 total
    let paras: Vec<String> = (1..=25)
        .map(|i| format!("段落{i:02}: 这是一段测试文字。"))
        .collect();
    let text = paras.join("\n\n");
    let chunks = split_by_visual_lines(&text, 30);
    assert!(chunks.len() > 1);
    // No chunk should exceed ~30 visual lines
    for chunk in &chunks {
        let lines = estimate_visual_lines(chunk);
        assert!(lines <= 35, "chunk has {lines} visual lines (>35)");
    }
}

#[test]
fn split_long_paragraphs_fewer_per_bubble() {
    // 10 long CJK paragraphs → each ~5 visual lines + gap = ~59 total
    let paras: Vec<String> = (1..=10)
        .map(|i| format!("段落{i}: 这是一段很长的测试文字，每段约一百个字符左右，用于测试渲染器对长段落的处理能力和行数估算的准确性。第{i}段结束。"))
        .collect();
    let text = paras.join("\n\n");
    let chunks = split_by_visual_lines(&text, 30);
    assert!(chunks.len() > 1, "should split: 10 long paras");
}

#[test]
fn split_never_inside_code_block() {
    let mut paras = Vec::new();
    for i in 1..=10 {
        paras.push(format!("para{i}"));
    }
    paras.push(
        "```\ncode line 1\n\ncode line 2\n\n```\n\nafter code".to_string(),
    );
    for i in 11..=25 {
        paras.push(format!("para{i}"));
    }
    let text = paras.join("\n\n");
    let chunks = split_by_visual_lines(&text, 30);
    for chunk in &chunks {
        let fence_count = chunk.matches("```").count();
        assert_eq!(
            fence_count % 2,
            0,
            "code fence split across chunks: {fence_count} occurrences"
        );
    }
}

#[test]
fn split_code_block_not_split_across_chunks() {
    let mut paras = Vec::new();
    paras.push("intro text".to_string());
    for i in 1..=18 {
        paras.push(format!("paragraph {i}"));
    }
    paras.push(
        "```\ntitle inside code\n\ncontent line 1\n\ncontent line 2\n```".to_string(),
    );
    for i in 19..=25 {
        paras.push(format!("trailing paragraph {i}"));
    }
    let text = paras.join("\n\n");
    let chunks = split_by_visual_lines(&text, 30);
    for chunk in &chunks {
        let opens = chunk.matches("```").count();
        assert_eq!(
            opens % 2,
            0,
            "fences must be balanced: found {opens} in chunk"
        );
    }
}

#[test]
fn display_width_cjk_vs_ascii() {
    // 10 CJK chars = 10.0
    assert_eq!(display_width("这是一段中文测试文字"), 10.0);
    // 20 ASCII chars = 10.0
    assert_eq!(display_width("abcdefghijklmnopqrst"), 10.0);
    // Mixed
    assert_eq!(display_width("中文ab"), 1.0 + 1.0 + 0.5 + 0.5);
}

#[test]
fn quote_message_extraction() {
    // Reply event with message_type=103 and msg_elements
    let data = serde_json::json!({
        "id": "MSG123",
        "message_type": 103,
        "content": "that's interesting",
        "author": { "member_openid": "user_a" },
        "group_openid": "group_1",
        "msg_elements": [
            { "content": "original message text", "elem_type": 1 }
        ]
    });
    let quoted = extract_quote_content(&data);
    assert_eq!(quoted.as_deref(), Some("original message text"));

    // Normal message (no quote) — should return None
    let normal = serde_json::json!({
        "id": "MSG456",
        "content": "hello world",
        "author": { "member_openid": "user_b" },
        "group_openid": "group_1"
    });
    assert!(extract_quote_content(&normal).is_none());

    // msg_elements present but empty array
    let empty_elements = serde_json::json!({
        "id": "MSG789",
        "msg_elements": []
    });
    assert!(extract_quote_content(&empty_elements).is_none());

    // msg_elements present but content is empty string
    let empty_content = serde_json::json!({
        "message_type": 103,
        "msg_elements": [{ "content": "   " }]
    });
    assert!(extract_quote_content(&empty_content).is_none());
}

// ── Feature 2: Group history ──────────────────────────────────────────────

/// Build a minimal test channel with the given group config.
fn test_channel(group_config: Vec<(&str, Option<usize>)>) -> QQBotChannel {
    use crate::config::channel::{GroupConfig, QQBotAccountConfig};
    let mut gc = std::collections::HashMap::new();
    for (gid, limit) in group_config {
        gc.insert(
            gid.to_string(),
            GroupConfig {
                history_limit: limit,
                ..Default::default()
            },
        );
    }
    let config = QQBotAccountConfig {
        enabled: true,
        app_id: "test_app".to_string(),
        client_secret: "test_secret".to_string(),
        allowed_users: None,
        allowed_groups: Some(vec!["*".to_string()]),
        group_config: gc,
        debounce_window_ms: 0,
        debounce_separator: "\n\n---\n\n".to_string(),
        tts: false,
    };
    QQBotChannel::new("test".to_string(), config)
}

#[test]
fn group_history_capped_at_limit() {
    let ch = test_channel(vec![("*", Some(3))]);
    // Insert 5 messages; limit is 3 → only last 3 should remain.
    for i in 0..5 {
        ch.record_group_history("grp1", &format!("user{i}"), &format!("msg{i}"));
    }
    let history = ch.group_history.lock();
    let entries = history.get("grp1").unwrap();
    assert_eq!(entries.len(), 3, "history should be capped at 3");
    // First two entries (msg0, msg1) should have been evicted.
    assert_eq!(entries[0].1, "msg2");
    assert_eq!(entries[1].1, "msg3");
    assert_eq!(entries[2].1, "msg4");
}

#[test]
fn group_history_inject_format() {
    let ch = test_channel(vec![("*", Some(20))]);
    // Record some prior messages.
    ch.record_group_history("grp1", "alice", "hello");
    ch.record_group_history("grp1", "bob", "how are you?");
    // Record the current message.
    ch.record_group_history("grp1", "carol", "@bot what's up");

    let mut msg = ChannelInboundMessage {
        id: "m1".to_string(),
        sender: MessageSender::new("carol".to_string()),
        receiver: MessageReceiver::new("group:grp1".to_string()),
        content: ChannelMessageContent::text("@bot what's up".to_string()),
        timestamp: 0,
        interruption_scope_id: None,
        silenced_override: None,
        run_mode: Default::default(),
    };
    ch.inject_group_history(&mut msg, "grp1");

    // The injected text should contain the two prior messages but NOT the
    // current message (carol's @bot what's up) in the history block.
    assert!(msg.content.text.contains("[Chat history begins]"));
    assert!(msg.content.text.contains("[alice] hello"));
    assert!(msg.content.text.contains("[bob] how are you?"));
    assert!(msg.content.text.contains("[Chat history ends]"));
    assert!(msg.content.text.contains("[Current message]"));
    assert!(msg.content.text.contains("@bot what's up"));
    // The current message should NOT appear in the history block.
    let history_section = msg
        .content
        .text
        .split("[Chat history ends]")
        .next()
        .unwrap_or("");
    assert!(
        !history_section.contains("[carol]"),
        "current sender should not appear in history block"
    );
}

#[test]
fn group_history_inject_empty_no_change() {
    // When there is only the current message (no prior history), injection
    // should be a no-op.
    let ch = test_channel(vec![]);
    ch.record_group_history("grp1", "alice", "hello");

    let mut msg = ChannelInboundMessage {
        id: "m1".to_string(),
        sender: MessageSender::new("alice".to_string()),
        receiver: MessageReceiver::new("group:grp1".to_string()),
        content: ChannelMessageContent::text("hello".to_string()),
        timestamp: 0,
        interruption_scope_id: None,
        silenced_override: None,
        run_mode: Default::default(),
    };
    let original = msg.content.text.clone();
    ch.inject_group_history(&mut msg, "grp1");
    assert_eq!(msg.content.text, original, "text should be unchanged");
}

// ── Feature 3: Per-group config resolution ────────────────────────────────

#[test]
fn per_group_config_resolves() {
    let ch = test_channel(vec![("group_specific", Some(10)), ("*", Some(5))]);

    // Explicit per-group entry wins.
    assert_eq!(ch.resolve_group_history_limit("group_specific"), 10);
    // Unknown group falls back to wildcard "*".
    assert_eq!(ch.resolve_group_history_limit("unknown_group"), 5);
}

#[test]
fn per_group_config_default_when_no_wildcard() {
    // No wildcard, no explicit entry → default 20.
    let ch = test_channel(vec![("group_a", Some(3))]);
    assert_eq!(ch.resolve_group_history_limit("group_a"), 3);
    assert_eq!(ch.resolve_group_history_limit("group_b"), 20);
}

// ── Outbound safety: reply limiter, SSRF, rate limiter, sanitize ──────────

#[test]
fn ssrf_blocks_localhost() {
    assert!(is_ssrf_blocked("http://127.0.0.1:8080/img.png"));
    assert!(is_ssrf_blocked("https://localhost/file"));
    assert!(is_ssrf_blocked("http://192.168.1.1/secret"));
    assert!(is_ssrf_blocked("http://10.0.0.1/internal"));
    assert!(is_ssrf_blocked("http://169.254.169.254/metadata"));
}

#[test]
fn ssrf_allows_public() {
    assert!(!is_ssrf_blocked("https://example.com/image.png"));
    assert!(!is_ssrf_blocked("https://multimedia.nt.qq.com/audio.wav"));
}

#[test]
fn strip_internal_tags_removes_thinking() {
    // XML-style thinking tags
    let input = "<thinking>secret reasoning</thinking>Hello world";
    assert_eq!(strip_internal_tags(input), "Hello world");

    // <think> tags
    let input2 = "Hi <think>hidden</think> there";
    assert_eq!(strip_internal_tags(input2), "Hi  there");

    // system-reminder tags
    let input3 = "<system-reminder>do not leak</system-reminder>visible text";
    assert_eq!(strip_internal_tags(input3), "visible text");

    // Deepseek format with backticks
    let input4 = "`think`reasoning here`/think`answer";
    assert_eq!(strip_internal_tags(input4), "answer");

    // No tags → unchanged
    assert_eq!(strip_internal_tags("plain text"), "plain text");
}

#[test]
fn table_aware_split_doesnt_break_table() {
    let max_lines = 10;
    let mut table = String::from("| Name | Value |\n| --- | --- |");
    for i in 0..20 {
        if i == 10 {
            table.push_str("\n\n");
        } else {
            table.push('\n');
        }
        table.push_str(&format!("| row {i} | data {i} |"));
    }
    let text = format!("Intro paragraph here.\n\n{table}");
    let chunks = split_by_visual_lines(&text, max_lines);
    let chunk_with_row10 = chunks.iter().find(|c| c.contains("row 10"));
    assert!(chunk_with_row10.is_some(), "row 10 should appear in some chunk");
    assert!(
        chunk_with_row10.unwrap().contains("row 9"),
        "table split across chunks: row 9 and row 10 are in different chunks"
    );
}

#[test]
fn is_gfm_table_line_detects_pipe_rows() {
    assert!(is_gfm_table_line("| col1 | col2 |"));
    assert!(is_gfm_table_line("| col1 | col2"));
    assert!(is_gfm_table_line("|---|---|"));
    assert!(is_gfm_table_line("| --- | :---: |"));
    assert!(is_gfm_table_line("  | padded | row |"));
}

#[test]
fn is_gfm_table_line_rejects_non_table() {
    assert!(!is_gfm_table_line(""));
    assert!(!is_gfm_table_line("just plain text"));
    assert!(!is_gfm_table_line("> a blockquote"));
    assert!(!is_gfm_table_line("# heading"));
}
