use super::*;
use super::backend::scan_jsonl_messages;
use crate::providers::{ChatMessage, ContentPart};
use crate::storage::json_file::records::SessionMeta;

fn backend_with_session() -> (tempfile::TempDir, JsonFileBackend, String) {
    let dir = tempfile::tempdir().unwrap();
    let backend = JsonFileBackend::open(dir.path()).unwrap();
    let info = backend.create_session("owner", None).unwrap();
    (dir, backend, info.id)
}

#[test]
fn list_sessions_finds_legacy_prefixed_directory() {
    // Pre-P1-A session dirs are named `myclaw_s_<uuid>` (the FQID with
    // `/` escaped to `_`), not the bare uuid `session_dir()` now assumes.
    // Regression test for the bug where `list_sessions`/`list_all_sessions`
    // re-derived the directory from `meta.id` via `bare_dir_name` and
    // silently dropped every legacy-named session because that bare-uuid
    // directory never existed on disk.
    let (dir, backend, sid) = backend_with_session();
    let bare_name = dir
        .path()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let legacy_name = format!("myclaw_s_{}", bare_name.to_string_lossy());
    fs::rename(dir.path().join(&bare_name), dir.path().join(&legacy_name)).unwrap();

    let sessions = backend.list_sessions("owner");
    assert_eq!(sessions.len(), 1, "legacy-named session dir should still be found");
    assert_eq!(sessions[0].id, sid);

    let all = backend.list_all_sessions();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, sid);
}

#[test]
fn path_file_roundtrip_stays_path_only() {
    let (_dir, backend, sid) = backend_with_session();
    let msg = ChatMessage {
        role: "user".into(),
        parts: vec![
            ContentPart::File {
                path: "sessions/s/files/image.png".into(),
                mime_type: Some("image/png".into()),
                name: Some("image.png".into()),
                size_bytes: Some(12),
            },
            ContentPart::Text {
                text: "hello".into(),
            },
        ],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        is_error: None,
        model: None,
        usage: None,
    };
    backend.append_message(&sid, &msg).unwrap();

    let line = fs::read_to_string(backend.history_path(&sid)).unwrap();
    assert!(line.contains("\"type\":\"file\""));
    assert!(!line.contains("base64"));
    assert!(!line.contains("image_b64"));
    assert_eq!(backend.load_messages(&sid)[0].parts.len(), 2);
}

#[test]
fn delegation_checkpoint_roundtrip() {
    let (_dir, backend, _sid) = backend_with_session();
    let cp = crate::storage::DelegationCheckpoint {
        parent_session_id: "parent".to_string(),
        sub_session_id: "test/s/sub".to_string(),
        agent_name: "coder".to_string(),
        status: "checkpointed".to_string(),
        started_at: chrono::Utc::now(),
        timeout_secs: 600,
        allowed_tools: Some(vec!["shell".to_string(), "file_edit".to_string()]),
        last_checkpoint: Some(chrono::Utc::now()),
    };
    backend.save_delegation_checkpoint(&cp).unwrap();

    let loaded = backend.load_delegation_checkpoints();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].sub_session_id, "test/s/sub");
    assert_eq!(loaded[0].status, "checkpointed");
    assert_eq!(loaded[0].timeout_secs, 600);
    assert_eq!(
        loaded[0].allowed_tools.as_deref(),
        Some(&["shell".to_string(), "file_edit".to_string()][..])
    );

    // Delete works.
    backend.delete_delegation_checkpoint("test/s/sub").unwrap();
    assert!(backend.load_delegation_checkpoints().is_empty());
}

#[test]
fn delegation_checkpoint_multiple_and_corrupt_skips() {
    let (_dir, backend, _sid) = backend_with_session();

    for i in 0..3 {
        let cp = crate::storage::DelegationCheckpoint {
            parent_session_id: format!("parent-{i}"),
            sub_session_id: format!("test/s/sub-{i}"),
            agent_name: "coder".to_string(),
            status: "running".to_string(),
            started_at: chrono::Utc::now(),
            timeout_secs: 300,
            allowed_tools: None,
            last_checkpoint: None,
        };
        backend.save_delegation_checkpoint(&cp).unwrap();
    }

    // Write a corrupt checkpoint file (inside a bogus session dir).
    let corrupt_dir = backend.root.join("019fec31-0000-7000-8000-000000000000");
    fs::create_dir_all(&corrupt_dir).unwrap();
    let corrupt_path = corrupt_dir.join("delegation.json");
    fs::write(&corrupt_path, b"{not valid json").unwrap();

    let loaded = backend.load_delegation_checkpoints();
    assert_eq!(loaded.len(), 3, "corrupt file should be skipped");
}

#[test]
fn concurrent_meta_writes_never_corrupt_the_file() {
    // Regression test for the data-corruption bug where `write_json_atomic`
    // used a single hardcoded `.tmp` path: concurrent writers all created
    // and wrote through the *same* temp file, so one writer's `File::create`
    // could truncate another's in-flight write before either had renamed,
    // producing a torn/invalid `meta.json`. With a uniquely-named temp file
    // per write, every writer's rename is independently atomic, so a
    // concurrent reader must always see either a prior or a new, but always
    // *valid*, meta.json — never a partial write.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let (_dir, backend, sid) = backend_with_session();
    let backend = Arc::new(backend);
    let stop = Arc::new(AtomicBool::new(false));

    let writers: Vec<_> = (0..8)
        .map(|i| {
            let backend = Arc::clone(&backend);
            let sid = sid.clone();
            std::thread::spawn(move || {
                for n in 0..200 {
                    let mut meta = backend.read_meta(&sid).unwrap();
                    meta.owner = format!("writer-{i}-{n}");
                    backend.write_meta(&meta).unwrap();
                }
            })
        })
        .collect();

    let reader = {
        let backend = Arc::clone(&backend);
        let sid = sid.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let meta_path = backend.meta_path(&sid);
            while !stop.load(Ordering::Relaxed) {
                if let Ok(bytes) = fs::read(&meta_path) {
                    if !bytes.is_empty() {
                        serde_json::from_slice::<SessionMeta>(&bytes)
                            .expect("meta.json must never be torn/invalid mid-write");
                    }
                }
            }
        })
    };

    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    // Final state is still valid and readable.
    assert!(backend.read_meta(&sid).is_some());
}

#[test]
fn torn_write_drops_only_the_corrupted_line_not_subsequent_history() {
    // Regression test for issue #213: `read_history_with_ids` used
    // `.lines().map_while(Result::ok)`, which stops reading entirely the
    // first time a line fails UTF-8 decoding (e.g. a process killed
    // mid-write, cutting a multi-byte character in half, followed by
    // another write landing on the same unterminated line). That silently
    // discarded every subsequent line, not just the corrupted one.
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("message one"))
        .unwrap();

    let history_path = backend.history_path(&sid);

    // Simulate: message two's write was interrupted mid multi-byte
    // character (no trailing newline), and message three's write landed
    // on the same line right after — the classic torn-write + O_APPEND
    // concatenation described in the issue.
    let msg_two_json = serde_json::to_string(&ChatMessage::user_text("测试内容")).unwrap();
    let torn = {
        let bytes = msg_two_json.as_bytes();
        let cut = msg_two_json.find('测').unwrap() + 1; // 1 of 3 bytes of '测'
        bytes[..cut].to_vec()
    };
    let msg_three_json =
        serde_json::to_string(&ChatMessage::user_text("message three, glued on")).unwrap();

    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.extend_from_slice(msg_three_json.as_bytes());
    raw.push(b'\n'); // ends the merged (torn + three) line
    fs::write(&history_path, &raw).unwrap();

    // A message appended normally afterwards, on its own clean line.
    backend
        .append_message(&sid, &ChatMessage::user_text("message four"))
        .unwrap();

    let loaded = backend.load_messages(&sid);
    let texts: Vec<String> = loaded
        .iter()
        .filter_map(|m| {
            m.parts.first().and_then(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .collect();

    assert!(texts.iter().any(|t| t == "message one"));
    assert!(
        texts.iter().any(|t| t == "message four"),
        "a line after the corrupted one must still be loaded, got: {texts:?}"
    );
    assert_eq!(
        texts.len(),
        2,
        "only the torn line should be dropped, got: {texts:?}"
    );
}

#[test]
fn scan_jsonl_messages_skips_torn_line_and_keeps_reading() {
    // Regression test for issue #217: `extend_archived_live_sets` used
    // `.lines().map_while(Result::ok)` — the same anti-pattern fixed for
    // the active history in #213 — to scan archive segments for legacy
    // blob-hash refs. One torn line stopped the scan entirely, silently
    // dropping every message (and thus every live blob hash) after it.
    // `scan_jsonl_messages` is the extracted, independently-testable read
    // loop backing that scan; `collect_blob_hashes` is currently a no-op
    // stub, so this is tested at the scan level rather than through
    // `extend_archived_live_sets` itself.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("segment.jsonl");

    let msg_one = serde_json::to_string(&ChatMessage::user_text("first")).unwrap();
    let msg_three = serde_json::to_string(&ChatMessage::user_text("third")).unwrap();
    let torn = "测试".as_bytes()[..1].to_vec(); // 1 of 3 bytes of '测' — invalid UTF-8 alone

    let mut raw = Vec::new();
    raw.extend_from_slice(msg_one.as_bytes());
    raw.push(b'\n');
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    raw.extend_from_slice(msg_three.as_bytes());
    raw.push(b'\n');
    fs::write(&path, &raw).unwrap();

    let messages = scan_jsonl_messages(&path);
    let texts: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            m.parts.first().and_then(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .collect();

    assert_eq!(
        texts,
        vec!["first".to_string(), "third".to_string()],
        "the torn line must be skipped without dropping the line after it, got: {texts:?}"
    );
}

#[test]
fn remove_last_message_survives_torn_line_earlier_in_file() {
    // Regression test for issue #217: `remove_last_message` used
    // `fs::read_to_string`, which fails wholesale if *any* line in the
    // file contains invalid UTF-8 (e.g. an earlier torn write elsewhere in
    // the session's history) — silently no-opping the removal (`Ok(false)`)
    // instead of just dropping the last line, which doesn't actually
    // require decoding the rest of the file.
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("message one"))
        .unwrap();

    let history_path = backend.history_path(&sid);
    let torn = "测试".as_bytes()[..1].to_vec(); // invalid UTF-8 on its own
    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    fs::write(&history_path, &raw).unwrap();

    backend
        .append_message(&sid, &ChatMessage::user_text("message two"))
        .unwrap();

    let removed = backend.remove_last_message(&sid).unwrap();
    assert!(
        removed,
        "remove_last_message must succeed despite an earlier torn line, not silently no-op"
    );

    let final_raw = fs::read(&history_path).unwrap();
    let mut expected = serde_json::to_string(&ChatMessage::user_text("message one"))
        .unwrap()
        .into_bytes();
    expected.push(b'\n');
    expected.extend_from_slice(&torn);
    expected.push(b'\n');
    assert_eq!(
        final_raw, expected,
        "only the last line should be removed; the earlier torn line must survive byte-for-byte"
    );
}

#[test]
fn delete_message_by_id_survives_torn_line_elsewhere_in_file() {
    // Regression test for issue #219: `delete_message_by_id` used
    // `fs::read_to_string`, which fails wholesale on any invalid-UTF-8 byte
    // anywhere in the file — a torn line trailing the file made it silently
    // no-op (`Ok(false)`) even for a perfectly fine earlier message.
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("message one"))
        .unwrap();
    backend
        .append_message(&sid, &ChatMessage::user_text("message two"))
        .unwrap();

    let history_path = backend.history_path(&sid);
    let torn = "测试".as_bytes()[..1].to_vec();
    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    fs::write(&history_path, &raw).unwrap();

    let deleted = backend.delete_message_by_id(&sid, 1).unwrap();
    assert!(
        deleted,
        "delete_message_by_id must succeed despite a trailing torn line"
    );

    let final_raw = fs::read(&history_path).unwrap();
    let mut expected = serde_json::to_string(&ChatMessage::user_text("message two"))
        .unwrap()
        .into_bytes();
    expected.push(b'\n');
    expected.extend_from_slice(&torn);
    expected.push(b'\n');
    assert_eq!(
        final_raw, expected,
        "message one removed; message two and the torn line survive"
    );
}

#[test]
fn update_message_survives_torn_line_elsewhere_in_file() {
    // Regression test for issue #219: `update_message` used
    // `fs::read_to_string`, silently no-opping on a torn line anywhere in
    // the file instead of just replacing the target line by position.
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("message one"))
        .unwrap();
    backend
        .append_message(&sid, &ChatMessage::user_text("message two"))
        .unwrap();

    let history_path = backend.history_path(&sid);
    let torn = "测试".as_bytes()[..1].to_vec();
    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    fs::write(&history_path, &raw).unwrap();

    let updated = backend
        .update_message(&sid, 1, &ChatMessage::user_text("message one, edited"))
        .unwrap();
    assert!(
        updated,
        "update_message must succeed despite a trailing torn line"
    );

    let final_raw = fs::read(&history_path).unwrap();
    let mut expected = serde_json::to_string(&ChatMessage::user_text("message one, edited"))
        .unwrap()
        .into_bytes();
    expected.push(b'\n');
    expected.extend_from_slice(
        &serde_json::to_string(&ChatMessage::user_text("message two"))
            .unwrap()
            .into_bytes(),
    );
    expected.push(b'\n');
    expected.extend_from_slice(&torn);
    expected.push(b'\n');
    assert_eq!(
        final_raw, expected,
        "message one edited in place; message two and the torn line survive"
    );
}

#[test]
fn truncate_messages_survives_torn_line_in_discarded_portion() {
    // Regression test for issue #219: `truncate_messages` used
    // `fs::read_to_string`, silently no-opping (leaving the file untouched)
    // whenever any line was torn — even one entirely within the portion
    // being discarded.
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("message one"))
        .unwrap();
    backend
        .append_message(&sid, &ChatMessage::user_text("message two"))
        .unwrap();
    backend
        .append_message(&sid, &ChatMessage::user_text("message three"))
        .unwrap();

    let history_path = backend.history_path(&sid);
    let torn = "测试".as_bytes()[..1].to_vec();
    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    fs::write(&history_path, &raw).unwrap();

    backend.truncate_messages(&sid, 1).unwrap();

    let final_raw = fs::read(&history_path).unwrap();
    let mut expected = serde_json::to_string(&ChatMessage::user_text("message one"))
        .unwrap()
        .into_bytes();
    expected.push(b'\n');
    assert_eq!(
        final_raw, expected,
        "truncate must succeed despite a torn line in the discarded tail"
    );
}

#[test]
fn query_message_survives_torn_line_elsewhere_in_file() {
    // Regression test for issue #219: `query_message` used
    // `fs::read_to_string`, silently returning `None` whenever any line in
    // the file was torn — even for a query targeting a perfectly fine
    // earlier message.
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("message one"))
        .unwrap();
    backend
        .append_message(&sid, &ChatMessage::user_text("message two"))
        .unwrap();

    let history_path = backend.history_path(&sid);
    let torn = "测试".as_bytes()[..1].to_vec();
    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    fs::write(&history_path, &raw).unwrap();

    let result = backend.query_message(&sid, 1);
    assert_eq!(
        result,
        Some(("user".to_string(), "message one".to_string())),
        "query_message must succeed despite a trailing torn line"
    );
}

/// Shared setup for issue #225's regression tests: a session whose
/// `history.jsonl` is `[m1, TORN, m3]` — an INDEPENDENT torn line sitting
/// between two real messages (not glued to either, unlike #213's O_APPEND
/// scenario). `m1` gets global id 1, `m3` gets global id 2 — the torn line
/// is skipped and does not consume an id, exactly like
/// `read_history_with_ids` already treats it. Returns the raw torn bytes too
/// (needed to assert they survive byte-for-byte).
fn session_with_torn_line_between_two_messages() -> (tempfile::TempDir, JsonFileBackend, String, Vec<u8>) {
    let (dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("m1"))
        .unwrap();

    let history_path = backend.history_path(&sid);
    let torn = "测试".as_bytes()[..1].to_vec();
    let mut raw = fs::read(&history_path).unwrap();
    raw.extend_from_slice(&torn);
    raw.push(b'\n');
    fs::write(&history_path, &raw).unwrap();

    backend
        .append_message(&sid, &ChatMessage::user_text("m3"))
        .unwrap();

    (dir, backend, sid, torn)
}

#[test]
fn delete_message_by_id_targets_the_correct_message_around_an_independent_torn_line() {
    // Regression test for issue #225: `delete_message_by_id` computed
    // `idx = message_id - start_id` and indexed straight into the raw
    // `\n`-split lines — but `read_history_with_ids` assigns ids by
    // position in the sequence of SUCCESSFULLY PARSED messages (a torn
    // line is skipped, not counted). With an independent torn line between
    // two real messages (`[m1, torn, m3]`), global id 2 is `m3`, but the
    // old code's raw-position `idx` of 1 pointed at the torn line instead —
    // deleting the torn junk, leaving `m3` untouched, and falsely reporting
    // success.
    let (_dir, backend, sid, torn) = session_with_torn_line_between_two_messages();

    let deleted = backend.delete_message_by_id(&sid, 2).unwrap();
    assert!(deleted, "delete_message_by_id(id=2) must report success");

    let history_path = backend.history_path(&sid);
    let final_raw = fs::read(&history_path).unwrap();
    let mut expected = serde_json::to_string(&ChatMessage::user_text("m1"))
        .unwrap()
        .into_bytes();
    expected.push(b'\n');
    expected.extend_from_slice(&torn);
    expected.push(b'\n');
    assert_eq!(
        final_raw, expected,
        "id=2 (m3) must be the line actually removed; m1 and the torn line must survive"
    );
}

#[test]
fn update_message_targets_the_correct_message_around_an_independent_torn_line() {
    // Regression test for issue #225: same misalignment as
    // `delete_message_by_id`, but for `update_message` — the old code
    // would overwrite the TORN line's raw position instead of `m3`'s.
    let (_dir, backend, sid, torn) = session_with_torn_line_between_two_messages();

    let updated = backend
        .update_message(&sid, 2, &ChatMessage::user_text("m3, edited"))
        .unwrap();
    assert!(updated, "update_message(id=2) must report success");

    let history_path = backend.history_path(&sid);
    let final_raw = fs::read(&history_path).unwrap();
    let mut expected = serde_json::to_string(&ChatMessage::user_text("m1"))
        .unwrap()
        .into_bytes();
    expected.push(b'\n');
    expected.extend_from_slice(&torn);
    expected.push(b'\n');
    expected.extend_from_slice(
        &serde_json::to_string(&ChatMessage::user_text("m3, edited"))
            .unwrap()
            .into_bytes(),
    );
    expected.push(b'\n');
    assert_eq!(
        final_raw, expected,
        "id=2 (m3) must be the line actually edited; m1 and the torn line must survive unchanged"
    );
}

#[test]
fn query_message_targets_the_correct_message_around_an_independent_torn_line() {
    // Regression test for issue #225: `query_message` only filtered empty
    // lines (`.filter(|l| !l.is_empty())`), so a non-empty-but-unparseable
    // (torn) line still consumed a sequence slot — misaligning `line_idx`
    // against `read_history_with_ids`'s indexing the same way the write
    // paths were misaligned.
    let (_dir, backend, sid, _torn) = session_with_torn_line_between_two_messages();

    let result = backend.query_message(&sid, 2);
    assert_eq!(
        result,
        Some(("user".to_string(), "m3".to_string())),
        "id=2 must resolve to m3, not the torn line or a false not-found"
    );
}

#[test]
fn rotate_uses_computed_start_id_not_survivor_in_memory_id() {
    let (_dir, backend, sid) = backend_with_session();
    backend
        .append_message(&sid, &ChatMessage::user_text("m1"))
        .unwrap();
    backend
        .append_message(&sid, &ChatMessage::user_text("m2"))
        .unwrap();

    // Simulate a session whose meta.json was rebuilt externally: segment 0
    // is the active segment covering ids 1..=2, while the daemon's
    // in-memory survivors still carry stale ids (here 100/101).
    let mut meta = backend.read_meta(&sid).unwrap();
    meta.segments = vec![SegmentRecord {
        segment: 0,
        start_id: 1,
        count: 2,
        compactions: vec![],
    }];
    backend.write_meta(&meta).unwrap();

    let surviving = vec![
        (
            0i64,
            ChatMessage::user_text("[CONTEXT COMPACTION — REFERENCE ONLY]"),
        ),
        (100i64, ChatMessage::user_text("m2")),
    ];
    backend.rotate_history_impl(&sid, &surviving).unwrap();

    let meta = backend.read_meta(&sid).unwrap();
    let archived = meta.segments.iter().find(|s| s.segment == 0).unwrap();
    assert_eq!((archived.start_id, archived.count), (1, 2));
    let active = meta.segments.iter().find(|s| s.segment == 1).unwrap();
    // Chain must be continuous: start == archived start + archived count,
    // never the stale in-memory id (100).
    assert_eq!((active.start_id, active.count), (3, 1));
    assert_eq!(meta.segment, 1);

    // Reload assigns ids from the recorded start_id (renumbered); the
    // compaction summary is inserted at its position with id 0.
    let loaded = backend.read_history_with_ids(&sid);
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].0, 0);
    assert_eq!(loaded[1].0, 3);
}
