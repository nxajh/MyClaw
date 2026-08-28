use super::super::types::User;
use super::*;

use super::api::{is_edit_not_modified, plain_send_message_body};
use super::super::types::{Chat, Message};
use crate::config::channel::TelegramAccountConfig;
use crate::DedupState;


/// issue #113: this is Telegram's benign "content unchanged" rejection
/// from a throttled preview re-editing identical text — must be
/// distinguished from a genuine edit failure.
#[test]
fn detects_not_modified_edit_rejection() {
    assert!(is_edit_not_modified(
        reqwest::StatusCode::BAD_REQUEST,
        r#"{"ok":false,"error_code":400,"description":"Bad Request: message is not modified"}"#
    ));
}

#[test]
fn does_not_flag_other_400s_as_not_modified() {
    assert!(!is_edit_not_modified(
        reqwest::StatusCode::BAD_REQUEST,
        r#"{"ok":false,"error_code":400,"description":"Bad Request: message to edit not found"}"#
    ));
}

#[test]
fn does_not_flag_non_400_status() {
    assert!(!is_edit_not_modified(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "message is not modified"
    ));
}

/// #144: the plain fallback body must carry NO parse_mode — with
/// Markdown attached, an unclosed entity (odd underscore count) turns
/// the "guaranteed delivery" tier into a 400 and the message is lost.
#[test]
fn plain_fallback_body_has_no_parse_mode() {
    let body = plain_send_message_body("chat-42", "sessions_yield _odd_", None, None);
    assert_eq!(body["chat_id"], "chat-42");
    assert_eq!(body["text"], "sessions_yield _odd_");
    assert!(
        body.get("parse_mode").is_none(),
        "plain fallback must not request any parse mode"
    );
}

#[test]
fn plain_fallback_body_carries_thread_and_markup() {
    let markup = serde_json::json!({"inline_keyboard": []});
    let body = plain_send_message_body(
        "chat-42",
        "hi",
        Some("7"),
        Some(&markup),
    );
    assert_eq!(body["message_thread_id"], "7");
    assert_eq!(body["reply_markup"], markup);
    assert!(body.get("parse_mode").is_none());
}

pub(crate) fn make_config() -> TelegramAccountConfig {
    TelegramAccountConfig {
        bot_token: "test_token_123".into(),
        allowed_users: vec!["alice".into(), "123456".into()],
        allowed_groups: None,
        mention_only: false,
        api_base: Some("https://api.telegram.org".into()),
        proxy_url: None,
        enabled: true,
        approval_timeout_secs: 120,
        ack_reactions: true,
        workspace_dir: None,
        debounce_ms: 0,        // disabled in tests
        stall_timeout_secs: 0, // disabled in tests
        streaming_mode: crate::config::channel::StreamingMode::Partial,
        tts: false,
    }
}

#[test]
fn test_normalize_identity() {
    assert_eq!(TelegramChannel::normalize_identity("@Alice"), "Alice");
    assert_eq!(TelegramChannel::normalize_identity("  Bob  "), "Bob");
    assert_eq!(TelegramChannel::normalize_identity("charlie"), "charlie");
}

#[test]
fn test_normalize_allowed_users() {
    let users = vec!["@Alice".into(), "  Bob  ".into(), "charlie".into()];
    let normalized = TelegramChannel::normalize_allowed_users(users);
    assert_eq!(normalized, vec!["Alice", "Bob", "charlie"]);
}

#[test]
fn phase4_security_policy_default_rejects_groups() {
    use crate::channels::{Channel, GroupAuthMode};
    let ch = TelegramChannel::new(make_config());
    // make_config has allowed_groups = None, mention_only = false
    let policy = ch.security_policy();
    assert!(
        matches!(policy.group_mode, GroupAuthMode::Reject),
        "Phase 4 default: missing allowed_groups must reject groups"
    );
}

#[test]
fn phase4_check_authorization_dm_allows_listed_user() {
    use crate::channels::{AuthDecision, MessageScope};
    let ch = TelegramChannel::new(make_config());
    // make_config lists "alice" — should allow DM from username alice
    let decision = ch.try_authorize(Some("alice"), Some(999), MessageScope::Direct);
    assert_eq!(decision, AuthDecision::Allow);

    // user_id 123456 also listed — should allow DM by id even without username
    let decision = ch.try_authorize(None, Some(123456), MessageScope::Direct);
    assert_eq!(decision, AuthDecision::Allow);

    // Unlisted user → Reject
    let decision = ch.try_authorize(Some("bob"), Some(777), MessageScope::Direct);
    assert!(matches!(decision, AuthDecision::Reject { .. }));
}

#[test]
fn phase4_group_mention_only_with_allowlist() {
    use crate::channels::{AuthDecision, MessageScope};
    let mut cfg = make_config();
    cfg.allowed_groups = Some(vec!["*".into()]);
    cfg.mention_only = true;
    let ch = TelegramChannel::new(cfg);

    // Allowed user, group, no mention → Ignore (silent drop, not warn)
    let decision = ch.try_authorize(
        Some("alice"),
        None,
        MessageScope::Group {
            id: "-100123",
            has_mention: false,
        },
    );
    assert_eq!(decision, AuthDecision::Ignore);

    // Allowed user, group, with mention → Allow
    let decision = ch.try_authorize(
        Some("alice"),
        None,
        MessageScope::Group {
            id: "-100123",
            has_mention: true,
        },
    );
    assert_eq!(decision, AuthDecision::Allow);
}

/// 挂起轮折叠 (2026-08-11): a flushed preview message is reported as a
/// fold candidate (id + current body) so the suspension machinery can
#[test]
fn test_parse_reply_target() {
    assert_eq!(
        TelegramChannel::parse_reply_target("12345"),
        ("12345".to_string(), None)
    );
    assert_eq!(
        TelegramChannel::parse_reply_target("12345:67890"),
        ("12345".to_string(), Some("67890".to_string()))
    );
}

#[test]
fn test_message_thread_id_in_reply_target() {
    // Simulates: chat.id = -100123456, message_thread_id = Some(42)
    let reply_target = if let Some(tid) = Some(42_i64) {
        format!("{}:{}", -100123456_i64, tid)
    } else {
        (-100123456_i64).to_string()
    };
    assert_eq!(reply_target, "-100123456:42");
    let (chat_id, thread_id) = TelegramChannel::parse_reply_target(&reply_target);
    assert_eq!(chat_id, "-100123456");
    assert_eq!(thread_id, Some("42".to_string()));
}

#[test]
fn test_forward_attribution_user() {
    let msg = Message {
        message_id: 1,
        message_thread_id: None,
        from: None,
        chat: Chat {
            id: 1,
            kind: "private".into(),
            username: None,
            title: None,
        },
        text: Some("hello".into()),
        caption: None,
        photo: None,
        voice: None,
        audio: None,
        video: None,
        video_note: None,
        document: None,
        forward_from: Some(User {
            id: 42,
            username: Some("bob".into()),
            first_name: None,
        }),
        forward_from_chat: None,
        forward_sender_name: None,
        forward_date: Some(1_700_000_000),
        reply_to_message: None,
    };
    assert_eq!(
        TelegramChannel::format_forward_attribution(&msg),
        Some("[Forwarded from @bob] ".to_string())
    );
}

#[test]
fn test_forward_attribution_channel() {
    let msg = Message {
        message_id: 1,
        message_thread_id: None,
        from: None,
        chat: Chat {
            id: 1,
            kind: "private".into(),
            username: None,
            title: None,
        },
        text: Some("news".into()),
        caption: None,
        photo: None,
        voice: None,
        audio: None,
        video: None,
        video_note: None,
        document: None,
        forward_from: None,
        forward_from_chat: Some(Chat {
            id: -1_001_234_567_890_i64,
            kind: "channel".into(),
            username: Some("dailynews".into()),
            title: Some("Daily News".into()),
        }),
        forward_sender_name: None,
        forward_date: Some(1_700_000_000),
        reply_to_message: None,
    };
    assert_eq!(
        TelegramChannel::format_forward_attribution(&msg),
        Some("[Forwarded from channel: Daily News] ".to_string())
    );
}

#[test]
fn test_forward_attribution_hidden_sender() {
    let msg = Message {
        message_id: 1,
        message_thread_id: None,
        from: None,
        chat: Chat {
            id: 1,
            kind: "private".into(),
            username: None,
            title: None,
        },
        text: Some("secret".into()),
        caption: None,
        photo: None,
        voice: None,
        audio: None,
        video: None,
        video_note: None,
        document: None,
        forward_from: None,
        forward_from_chat: None,
        forward_sender_name: Some("Hidden User".into()),
        forward_date: Some(1_700_000_000),
        reply_to_message: None,
    };
    assert_eq!(
        TelegramChannel::format_forward_attribution(&msg),
        Some("[Forwarded from Hidden User] ".to_string())
    );
}

#[test]
fn test_forward_attribution_none() {
    let msg = Message {
        message_id: 1,
        message_thread_id: None,
        from: Some(User {
            id: 1,
            username: Some("alice".into()),
            first_name: None,
        }),
        chat: Chat {
            id: 1,
            kind: "private".into(),
            username: None,
            title: None,
        },
        text: Some("hello".into()),
        caption: None,
        photo: None,
        voice: None,
        audio: None,
        video: None,
        video_note: None,
        document: None,
        forward_from: None,
        forward_from_chat: None,
        forward_sender_name: None,
        forward_date: None,
        reply_to_message: None,
    };
    assert_eq!(TelegramChannel::format_forward_attribution(&msg), None);
}

#[test]
fn test_bot_mention_spans() {
    let ch = TelegramChannel::new(make_config());
    // Set bot username directly in the Arc<Mutex<>>.
    *ch.bot_username.lock() = Some("mybot".to_string());

    // Direct mention: "@mybot" at indices [7, 12) in "Hello @mybot how are you?"
    let text = "Hello @mybot how are you?";
    let spans = ch.find_bot_mention_spans(text);
    assert_eq!(spans, vec![(7, 12)]); // [7, 12) = "mybot"

    // Not a mention (alphanumeric before @).
    let text2 = "email@mybot.com";
    let spans2 = ch.find_bot_mention_spans(text2);
    assert!(spans2.is_empty());

    // Strip mentions.
    let text3 = "Hey @mybot what's up?";
    let stripped = ch.strip_bot_mentions(text3);
    assert!(!stripped.contains("@mybot"));
    assert!(stripped.contains("Hey"));
}

#[test]
fn test_dedup() {
    let dedup = DedupState::new();
    assert!(!dedup.check_and_record("msg1")); // new → false (not seen before)
    assert!(dedup.check_and_record("msg1")); // duplicate → true (already seen)
    assert!(!dedup.check_and_record("msg2")); // new → false (not seen before)
}

#[test]
fn test_message_chunking() {
    use crate::channels::message::LenUnit;
    let chunks =
        crate::channels::message::split_message_chunk("short", 10, LenUnit::Codepoints);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], "short");

    let long = "a".repeat(5000);
    let chunks = crate::channels::message::split_message_chunk(&long, 100, LenUnit::Codepoints);
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| c.len() <= 100));
}

#[test]
fn test_utf16_chunking_emoji() {
    use crate::channels::message::{LenUnit, split_message_chunk};
    // Build 2100 codepoints of emoji (each is 2 UTF-16 units = 4200 units total)
    let emoji_text = "😀".repeat(2100);
    assert_eq!(emoji_text.chars().count(), 2100);
    assert_eq!(emoji_text.encode_utf16().count(), 4200);

    let chunks = split_message_chunk(&emoji_text, 4096, LenUnit::Utf16Units);
    assert!(chunks.len() > 1, "must split when UTF-16 exceeds limit");
    for c in &chunks {
        assert!(
            c.encode_utf16().count() <= 4096,
            "each chunk must fit Telegram UTF-16 limit"
        );
    }
}

#[test]
fn test_normalize_markdown_tables() {
    // No blank line before table → should insert one.
    let input = "Here are the results:\n| A | B |\n| --- | --- |\n| 1 | 2 |";
    let out = TelegramChannel::normalize_markdown_tables(input);
    assert!(
        out.contains("Here are the results:\n\n| A | B |"),
        "expected blank line before table, got:\n{out}"
    );

    // Already has a blank line → unchanged.
    let input2 = "Intro:\n\n| A | B |\n| --- | --- |\n| 1 | 2 |";
    let out2 = TelegramChannel::normalize_markdown_tables(input2);
    assert_eq!(out2, input2);

    // Table at start of message → no blank needed.
    let input3 = "| A | B |\n| --- | --- |\n| 1 | 2 |";
    let out3 = TelegramChannel::normalize_markdown_tables(input3);
    assert_eq!(out3, input3);

    // Multiple tables in one message.
    let input4 = "Text1\n| A |\n| - |\n| x |\n\nText2\n| B |\n| - |\n| y |";
    let out4 = TelegramChannel::normalize_markdown_tables(input4);
    assert!(out4.contains("Text1\n\n| A |"), "first table missing blank");
    assert!(
        out4.contains("Text2\n\n| B |"),
        "second table missing blank"
    );
}

/// issue #114: Telegram's rich-message renderer parses `#108` (no
/// space) at line start as an ATX heading, even though CommonMark
/// requires whitespace after the `#` run for that. This is the
/// exact repro from the issue.
#[test]
fn test_escape_digit_heading_lookalikes_issue_reference() {
    let input = "#108 已立案";
    let out = TelegramChannel::escape_digit_heading_lookalikes(input);
    assert_eq!(out, "\\#108 已立案");
}

#[test]
fn test_escape_digit_heading_lookalikes_leaves_real_headings_alone() {
    assert_eq!(
        TelegramChannel::escape_digit_heading_lookalikes("## 备注"),
        "## 备注"
    );
    assert_eq!(
        TelegramChannel::escape_digit_heading_lookalikes("# 标准标题"),
        "# 标准标题"
    );
}

#[test]
fn test_escape_digit_heading_lookalikes_multiple_hashes() {
    // Rare but per the issue: a run up to 6 '#'s immediately followed
    // by a digit is escaped too, not just a single '#'.
    assert_eq!(
        TelegramChannel::escape_digit_heading_lookalikes("#####108"),
        "\\#####108"
    );
}

#[test]
fn test_escape_digit_heading_lookalikes_only_at_line_start() {
    // A '#' mid-line (not at column 0) is not heading syntax at all —
    // must be left untouched.
    let input = "see #108 for details";
    assert_eq!(
        TelegramChannel::escape_digit_heading_lookalikes(input),
        input
    );
}

#[test]
fn test_escape_digit_heading_lookalikes_multiline() {
    let input = "line1\n#108 fixed\nline3\n#2026 date-ish";
    let out = TelegramChannel::escape_digit_heading_lookalikes(input);
    assert_eq!(out, "line1\n\\#108 fixed\nline3\n\\#2026 date-ish");
}
