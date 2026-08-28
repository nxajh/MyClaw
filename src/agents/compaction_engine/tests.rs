use super::*;
use super::fold::HARD_TOTAL_CHARS;
use super::fold::{find_incremental_range, hard_fold_history, is_empty_compact_error};
use super::fold::open_user_protected_from;
use super::summarizer::build_summarizer_prompt;
use crate::providers::{ChatMessage, ContentPart, ToolCall};

#[test]
fn incremental_range_without_existing_summary_compacts_prefix() {
    let history = vec![
        ChatMessage::user_text("u1"),
        ChatMessage::assistant_text("a1"),
        ChatMessage::user_text("u2"),
    ];

    let (replace_start, compact_start, compact_end, existing) =
        find_incremental_range(&history, 2);

    assert_eq!(replace_start, 0);
    assert_eq!(compact_start, 0);
    assert_eq!(compact_end, 2);
    assert!(existing.is_none());
}

#[test]
fn incremental_range_excludes_existing_summary_from_new_content() {
    let history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] previous"),
        ChatMessage::user_text("new user"),
        ChatMessage::assistant_text("new assistant"),
        ChatMessage::user_text("retained"),
    ];

    let (replace_start, compact_start, compact_end, existing) =
        find_incremental_range(&history, 3);

    assert_eq!(replace_start, 0);
    assert_eq!(compact_start, 1);
    assert_eq!(compact_end, 3);
    assert_eq!(
        existing.as_deref(),
        Some("[CONTEXT COMPACTION — REFERENCE ONLY] previous")
    );
}

#[test]
fn incremental_range_detects_no_new_content_after_existing_summary() {
    let history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] previous"),
        ChatMessage::user_text("retained"),
    ];

    let (replace_start, compact_start, compact_end, existing) =
        find_incremental_range(&history, 1);

    assert_eq!(replace_start, 0);
    assert_eq!(compact_start, 1);
    assert_eq!(compact_end, 1);
    assert!(existing.is_some());
}

#[test]
fn empty_compact_error_detected() {
    let err = anyhow::anyhow!("no content to compact");
    assert!(is_empty_compact_error(&err));
    let other = anyhow::anyhow!("summarize failed");
    assert!(!is_empty_compact_error(&other));
}

/// xiaoliu-class failure: one user + many tool rounds after a prior summary
/// makes every work unit share user_start → incremental range is empty.
#[test]
fn single_user_tool_chain_incremental_range_is_empty() {
    let mut history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior work"),
        ChatMessage::user_text("中兴在互联网有多大份额？"),
    ];
    for i in 0..20 {
        let mut a = ChatMessage::assistant_text("");
        a.tool_calls = Some(vec![ToolCall {
            id: format!("c{i}"),
            name: "web_search".into(),
            arguments: format!(r#"{{"query":"q{i}"}}"#),
        }]);
        history.push(a);
        let mut t = ChatMessage::text("tool", format!("result {i}"));
        t.tool_call_id = Some(format!("c{i}"));
        history.push(t);
    }

    // Boundary pinned at the sole real user (index 1) — nothing after the
    // existing summary and before boundary is compactable.
    let boundary = 1;
    let (replace_start, compact_start, compact_end, existing) =
        find_incremental_range(&history, boundary);
    assert_eq!(replace_start, 0);
    assert_eq!(compact_start, 1);
    assert_eq!(compact_end, 1);
    assert!(existing.is_some());
    assert!(history[compact_start..compact_end].is_empty());

    // Full-fold boundary = len has content.
    let (rs, cs, ce, _) = find_incremental_range(&history, history.len());
    assert_eq!(rs, 0);
    assert_eq!(cs, 1);
    assert_eq!(ce, history.len());
    assert!(!history[cs..ce].is_empty());
}

#[test]
fn hard_fold_preserves_user_and_tool_names() {
    let mut a = ChatMessage::assistant_text("looking up");
    a.tool_calls = Some(vec![ToolCall {
        id: "c1".into(),
        name: "web_search".into(),
        arguments: "{}".into(),
    }]);
    let mut t = ChatMessage::text("tool", "ZTE share is X%");
    t.tool_call_id = Some("c1".into());
    let history = vec![
        ChatMessage::user_text("中兴份额?"),
        a,
        t,
        ChatMessage::assistant_text("share is X%"),
    ];
    let fold = hard_fold_history(&history);
    assert!(fold.contains("中兴份额"));
    assert!(fold.contains("web_search"));
    assert!(fold.contains("ZTE share") || fold.contains("share is X%"));
    assert!(fold.contains("user=1"));
    assert!(fold.chars().count() < HARD_TOTAL_CHARS);
    assert!(
        fold.contains("Do NOT invent user tasks"),
        "hard-fold banner must discourage fake tasks, got: {fold}"
    );
}

#[test]
fn summarizer_prompt_forbids_invented_tasks() {
    let p = build_summarizer_prompt(12, None);
    assert!(p.contains("do NOT invent user tasks") || p.contains("NEVER invent"));
    assert!(p.contains("继续") || p.contains("continue"));
    let merge = build_summarizer_prompt(3, Some("old summary"));
    assert!(merge.contains("NEVER invent") || merge.contains("do NOT invent"));
}

#[test]
fn hard_fold_respects_total_budget() {
    let mut history = Vec::new();
    history.push(ChatMessage::user_text("goal ".repeat(200)));
    for i in 0..200 {
        history.push(ChatMessage::assistant_text(format!(
            "assistant blob {i} {}",
            "x".repeat(300)
        )));
        let mut t = ChatMessage::text("tool", format!("tool blob {i} {}", "y".repeat(300)));
        t.tool_call_id = Some(format!("c{i}"));
        history.push(t);
    }
    let fold = hard_fold_history(&history);
    // Allow small overhead for the head/tail splice marker.
    assert!(
        fold.chars().count() <= HARD_TOTAL_CHARS + 8,
        "fold len {}",
        fold.chars().count()
    );
    assert!(fold.contains("goal"));
}

#[test]
fn hard_fold_then_apply_leaves_single_user_message() {
    // Simulates last-resort path: hard fold entire completed history → apply.
    let mut history = vec![ChatMessage::user_text("q")];
    for i in 0..10 {
        let mut a = ChatMessage::assistant_text("");
        a.tool_calls = Some(vec![ToolCall {
            id: format!("c{i}"),
            name: "web_search".into(),
            arguments: "{}".into(),
        }]);
        history.push(a);
        let mut t = ChatMessage::text("tool", "r");
        t.tool_call_id = Some(format!("c{i}"));
        history.push(t);
    }
    assert!(open_user_protected_from(&history).is_none());
    let body = hard_fold_history(&history);
    let summary = format!("[CONTEXT COMPACTION — REFERENCE ONLY] {body}");
    // After replace, only one user message remains — legal for any provider.
    let mut after = history.clone();
    let end = after.len();
    after.drain(0..end);
    after.insert(0, ChatMessage::user_text(summary));
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].role, "user");
    assert!(after[0]
        .text_content()
        .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]"));
}

#[test]
fn open_user_protected_from_trailing_user() {
    let history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior"),
        ChatMessage::assistant_text("done"),
        ChatMessage::user_text("new request"),
    ];
    assert_eq!(open_user_protected_from(&history), Some(2));
}

#[test]
fn open_user_protected_from_trailing_user_streak() {
    // attachment system-reminder user + real user at tail
    let history = vec![
        ChatMessage::assistant_text("old"),
        ChatMessage::user_text("<system-reminder>\n## Skills\n</system-reminder>"),
        ChatMessage::user_text("actual question"),
    ];
    assert_eq!(open_user_protected_from(&history), Some(1));
}

#[test]
fn open_user_protected_from_completed_turn_is_none() {
    let history = vec![
        ChatMessage::user_text("q"),
        ChatMessage::assistant_text("a"),
    ];
    assert_eq!(open_user_protected_from(&history), None);
}

#[test]
fn hard_fold_strips_system_reminder_keeps_body() {
    let reminder = "<system-reminder>\n## Memory\n- lots of injected memory text that would burn the 400 char budget if kept first\n".repeat(5);
    let text = format!(
        "{reminder}</system-reminder>\n现在的memory没有和原始消息内容建立关联 这个问题需要解决"
    );
    let history = vec![ChatMessage::user_text(text)];
    let fold = hard_fold_history(&history);
    assert!(
        fold.contains("memory没有和原始消息"),
        "fold must keep user body, got: {fold}"
    );
    assert!(
        !fold.contains("<system-reminder>"),
        "fold should not keep raw system-reminder tags"
    );
}

#[test]
fn hard_fold_preserves_image_file_marker() {
    // Pure-image user turn: text_content() is empty; marker must still appear
    // so view_image can find the path after hard-fold.
    let path = "sessions/5687fcde/files/image-3e8610fd.png";
    let msg = ChatMessage {
        role: "user".into(),
        parts: vec![ContentPart::File {
            path: path.into(),
            mime_type: Some("image/png".into()),
            name: Some("shot.png".into()),
            size_bytes: Some(829_062),
        }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        is_error: None,
        model: None,
        usage: None,
    };
    assert!(msg.text_content().is_empty());
    let fold = hard_fold_history(&[msg]);
    assert!(
        fold.contains(&format!("[image: {path}]")),
        "hard-fold must keep image marker, got: {fold}"
    );
}

#[test]
fn hard_fold_image_plus_text_keeps_both() {
    let path = "sessions/x/files/a.png";
    let msg = ChatMessage {
        role: "user".into(),
        parts: vec![
            ContentPart::File {
                path: path.into(),
                mime_type: Some("image/png".into()),
                name: None,
                size_bytes: Some(1000),
            },
            ContentPart::Text {
                text: "这是什么".into(),
            },
        ],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        is_error: None,
        model: None,
        usage: None,
    };
    let fold = hard_fold_history(&[msg]);
    assert!(fold.contains(&format!("[image: {path}]")), "got: {fold}");
    assert!(fold.contains("这是什么"), "got: {fold}");
}

#[test]
fn full_fold_range_stops_before_open_user() {
    // Repro of 14f4fba2: summary + long chain + new open user.
    // fold_end must be the open user index, not history.len().
    let mut history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior work"),
        ChatMessage::user_text("old question"),
    ];
    for i in 0..5 {
        let mut a = ChatMessage::assistant_text("");
        a.tool_calls = Some(vec![ToolCall {
            id: format!("c{i}"),
            name: "web_search".into(),
            arguments: "{}".into(),
        }]);
        history.push(a);
        let mut t = ChatMessage::text("tool", format!("result {i}"));
        t.tool_call_id = Some(format!("c{i}"));
        history.push(t);
    }
    history.push(ChatMessage::user_text(
        "现在的memory没有和原始消息内容建立关联 这个问题需要解决",
    ));

    let protected = open_user_protected_from(&history).expect("open user");
    assert_eq!(protected, history.len() - 1);

    // Simulate full-fold apply: compact [0, protected), leave open user.
    let body = hard_fold_history(&history[..protected]);
    let summary = format!("[CONTEXT COMPACTION — REFERENCE ONLY] {body}");
    let mut after = history.clone();
    after.drain(0..protected);
    after.insert(0, ChatMessage::user_text(summary));

    assert!(
        after.len() >= 2,
        "must keep summary + open user, got len={}",
        after.len()
    );
    assert!(after[0]
        .text_content()
        .starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]"));
    let last = after.last().unwrap();
    assert_eq!(last.role, "user");
    assert!(
        last.text_content()
            .contains("memory没有和原始消息内容建立关联"),
        "open user body must survive verbatim: {}",
        last.text_content()
    );
}

#[test]
fn empty_incremental_with_open_user_does_not_claim_full_len() {
    // summary + unanswered user: protect the open user, not the summary.
    let history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] prior"),
        ChatMessage::user_text("unanswered"),
    ];
    let protected = open_user_protected_from(&history).unwrap();
    assert_eq!(protected, 1);
    let (_rs, cs, ce, _) = find_incremental_range(&history, protected);
    // Only the summary sits before protected; incremental slice is empty.
    assert!(cs >= ce || history[cs..ce].is_empty());
    // Hard-fold of prefix leaves the open user after apply.
    let mut after = history.clone();
    after.drain(0..protected);
    after.insert(
        0,
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] folded"),
    );
    assert_eq!(after.len(), 2);
    assert_eq!(after[1].text_content(), "unanswered");
}

#[test]
fn open_user_skips_compaction_summary_in_trailing_streak() {
    let history = vec![
        ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY] only summary"),
    ];
    assert_eq!(open_user_protected_from(&history), None);
}
