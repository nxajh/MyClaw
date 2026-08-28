use super::*;
use crate::providers::{ChatMessage, ContentPart};

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
