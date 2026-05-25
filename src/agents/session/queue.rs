//! Queue processing — hot-switch recovery for buffered messages.

use std::collections::HashMap;

use crate::providers::capability_chat::ChatMessage;

/// Get the queue file path for a session.
///
/// Queue files live inside the session directory alongside `history.jsonl`:
/// `sessions/{session_id}/queue.jsonl`.
fn get_session_queue_path(sessions_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    sessions_dir.join(session_id).join("queue.jsonl")
}

/// Append a message to a session's queue file.
///
/// Used during hot-switch to buffer incoming messages while the old process
/// is shutting down and the new one hasn't started yet.  Each message is
/// written as one JSON line (JSONL format).
pub fn enqueue_message(
    sessions_dir: &std::path::Path,
    session_id: &str,
    msg: &ChatMessage,
) -> std::io::Result<()> {
    let queue_file = get_session_queue_path(sessions_dir, session_id);
    if let Some(parent) = queue_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(msg).map_err(std::io::Error::other)? + "\n";
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&queue_file)?
        .write_all(line.as_bytes())
}

/// Read and drain all queued messages for a single session.
///
/// Returns the messages and removes the queue file.  Returns an empty vec
/// if no queue file exists.
pub fn process_queue(
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> std::io::Result<Vec<ChatMessage>> {
    let queue_file = get_session_queue_path(sessions_dir, session_id);
    if !queue_file.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&queue_file)?;
    let messages: Vec<ChatMessage> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Remove the queue file after successful read.
    std::fs::remove_file(&queue_file)?;

    if !messages.is_empty() {
        tracing::info!(
            session = %session_id,
            count = messages.len(),
            "processed queued messages from hot switch"
        );
    }

    Ok(messages)
}

/// Process queue files for all sessions under the given directory.
///
/// Scans each subdirectory of `sessions_dir` for a `queue.jsonl` file,
/// reads the queued messages, and removes the file.  Returns a map of
/// `session_id → queued messages` (only sessions with non-empty queues
/// are included).
pub fn process_all_queues(
    sessions_dir: &std::path::Path,
) -> std::io::Result<HashMap<String, Vec<ChatMessage>>> {
    let mut result = HashMap::new();
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(_) => return Ok(result),
    };

    for entry in entries.flatten() {
        if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().to_string();
        let messages = process_queue(sessions_dir, &session_id)?;
        if !messages.is_empty() {
            result.insert(session_id, messages);
        }
    }

    Ok(result)
}
