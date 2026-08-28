//! `sessions.*` management-API route handlers, extracted verbatim from
//! `client.rs::handle_api_request` (RFC docs/websocket-client-split-rfc.md,
//! batch 2: pure move — each function body is the original match-arm
//! body, unchanged; only the wrapper signature was added.)
//!
//! [`reconstruct_history`] (the sessions.history implementation helper)
//! moved alongside, unchanged.

use std::sync::Arc;

use super::memory::memory_user_id;
use super::ApiContext;

pub(super) fn list(
    id: &str,
    ctx: &ApiContext<'_>,
    sm: &Arc<crate::agents::SessionManager>,
    user_id: &str,
) -> String {
    // Aggregate across every routing_key linked to this identity via
    // `/link` (UserResolver), not just this web-client connection's
    // own routing_key — otherwise sessions created from a
    // previously-used channel become invisible the moment that
    // channel is linked to the account.
    let resolved = memory_user_id(ctx);
    let linked_routing_keys = sm.resolver().routing_keys_for(&resolved);
    let sessions = sm.list_sessions_for_user(&resolved);
    tracing::info!(
        raw_routing_key = %ctx.user_id,
        resolved_uid = %resolved,
        linked_routing_keys = ?linked_routing_keys,
        session_count = sessions.len(),
        "sessions.list diagnostic"
    );
    let active = sm.active_session_id(user_id);
    let result: Vec<serde_json::Value> = sessions.iter().map(|s| {
        serde_json::json!({
            "id": s.id,
            "name": s.display_name,
            "created_at": s.created_at.to_rfc3339(),
            "owner": s.owner,
            "is_active": active.as_ref() == Some(&s.id),
        })
    }).collect();
    serde_json::json!({
        "type": "api_response",
        "id": id,
        "result": result,
    }).to_string()
}

pub(super) fn create(
    id: &str,
    params: &serde_json::Value,
    sm: &Arc<crate::agents::SessionManager>,
    user_id: &str,
) -> String {
    let name = params["name"].as_str();
    // Evict cached SessionContext so the next message materializes
    // a fresh one for the newly-active session (mirrors /new).
    sm.drop_context(user_id);
    match sm.new_session(user_id, name) {
        Ok(info) => {
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": {
                    "id": info.id,
                    "name": info.display_name,
                    "created_at": info.created_at.to_rfc3339(),
                }
            }).to_string()
        }
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to create session: {}", e)
        }).to_string(),
    }
}

pub(super) fn switch(
    id: &str,
    params: &serde_json::Value,
    sm: &Arc<crate::agents::SessionManager>,
    user_id: &str,
) -> String {
    let session_id = match params["id"].as_str() {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing id parameter"
            }).to_string();
        }
    };
    // Evict cached SessionContext so the next message loads the
    // switched-to session's history (mirrors /switch).
    sm.drop_context(user_id);
    match sm.switch_session(user_id, session_id) {
        Ok(info) => {
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": {
                    "id": info.id,
                    "name": info.display_name,
                }
            }).to_string()
        }
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to switch session: {}", e)
        }).to_string(),
    }
}

pub(super) fn delete(
    id: &str,
    params: &serde_json::Value,
    sm: &Arc<crate::agents::SessionManager>,
    user_id: &str,
) -> String {
    let session_id = match params["id"].as_str() {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing id parameter"
            }).to_string();
        }
    };
    let existing_owner = sm.backend().get_session(session_id).map(|s| s.owner);
    match sm.delete_session(user_id, session_id) {
        Ok(()) => {
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": null
            }).to_string()
        }
        Err(e) => {
            tracing::warn!(
                attempted_user = %user_id,
                session = %session_id,
                actual_owner = existing_owner.as_deref().unwrap_or("<missing>"),
                error_kind = ?e.kind(),
                err = %e,
                "failed to delete WebSocket session"
            );
            serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": format!("failed to delete session: {}", e)
            }).to_string()
        }
    }
}

pub(super) fn delete_message(
    id: &str,
    params: &serde_json::Value,
    sm: &Arc<crate::agents::SessionManager>,
) -> String {
    let session_id = match params["id"].as_str() {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing id parameter"
            }).to_string();
        }
    };
    let message_id = match params["message_id"].as_i64() {
        Some(n) => n,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing message_id parameter"
            }).to_string();
        }
    };
    match sm.backend().delete_message_by_id(session_id, message_id) {
        Ok(true) => serde_json::json!({
            "type": "api_response",
            "id": id,
            "result": null
        }).to_string(),
        Ok(false) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "message not found"
        }).to_string(),
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to delete message: {}", e)
        }).to_string(),
    }
}

pub(super) fn rename(
    id: &str,
    params: &serde_json::Value,
    sm: &Arc<crate::agents::SessionManager>,
) -> String {
    let session_id = match params["id"].as_str() {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing id parameter"
            }).to_string();
        }
    };
    let name = match params["name"].as_str() {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing or empty name parameter"
            }).to_string();
        }
    };
    match sm.rename_session(session_id, name) {
        Ok(()) => serde_json::json!({
            "type": "api_response",
            "id": id,
            "result": null
        }).to_string(),
        Err(e) => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": format!("failed to rename session: {}", e)
        }).to_string(),
    }
}

pub(super) fn history(id: &str, sm: &Arc<crate::agents::SessionManager>, user_id: &str) -> String {
    let session = sm.get_or_create(user_id);
    let msgs = reconstruct_history(&session.history);
    serde_json::json!({
        "type": "api_response",
        "id": id,
        "result": msgs,
    }).to_string()
}

/// Reconstruct a session's stored history into WebSocket chat-message shape.
pub(super) fn reconstruct_history(
    history: &[crate::providers::capability_chat::ChatMessage],
) -> Vec<serde_json::Value> {
    use crate::providers::capability_chat::ContentPart;
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut counter = 0u64;
    for m in history {
        let mut text = String::new();
        let mut has_image = false;
        for p in &m.parts {
            match p {
                ContentPart::Text { text: t } => text.push_str(t),
                ContentPart::File {
                    path, mime_type, ..
                } if crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                    == crate::providers::media::FileModality::Image =>
                {
                    has_image = true
                }
                _ => {}
            }
        }
        match m.role.as_str() {
            "user" => {
                let has_files = m
                    .parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::File { .. }));
                let content = if !text.is_empty() {
                    text
                } else if has_image {
                    "🖼️ (image)".to_string()
                } else if has_files {
                    "📎 (file)".to_string()
                } else {
                    continue;
                };
                // Collect file references for the frontend to display.
                let mut images: Vec<serde_json::Value> = Vec::new();
                let mut files: Vec<serde_json::Value> = Vec::new();
                for p in &m.parts {
                    if let ContentPart::File {
                        path,
                        mime_type,
                        name,
                        ..
                    } = p
                    {
                        let entry = serde_json::json!({
                            "path": path,
                            "mime": mime_type,
                            "name": name,
                        });
                        let is_image =
                            crate::providers::media::modality_from_mime(mime_type.as_deref(), path)
                                == crate::providers::media::FileModality::Image;
                        if is_image {
                            images.push(entry);
                        } else {
                            files.push(entry);
                        }
                    }
                }
                counter += 1;
                let mut msg = serde_json::json!({
                    "role": "user",
                    "content": content,
                    "id": format!("h-{}", counter),
                });
                if !images.is_empty() {
                    msg["images"] = serde_json::Value::Array(images);
                }
                if !files.is_empty() {
                    msg["files"] = serde_json::Value::Array(files);
                }
                out.push(msg);
            }
            "assistant" => {
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                for p in &m.parts {
                    match p {
                        ContentPart::Text { text: t } if !t.is_empty() => {
                            blocks.push(serde_json::json!({ "type": "content", "text": t }));
                        }
                        ContentPart::Thinking { thinking: t, .. } if !t.is_empty() => {
                            blocks.push(serde_json::json!({ "type": "thinking", "text": t }));
                        }
                        _ => {}
                    }
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let args = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        blocks.push(serde_json::json!({
                            "type": "tool_call",
                            "id": tc.id,
                            "name": tc.name,
                            "args": args,
                        }));
                    }
                }

                // Try to find the closest assistant message in out to merge blocks with,
                // stopping if we encounter a user message. This robustly merges fragmented turns
                // even if intermediate virtual turns are present.
                let mut merged = false;
                for msg in out.iter_mut().rev() {
                    if msg["role"] == "user" {
                        break;
                    }
                    if msg["role"] == "assistant" {
                        if let Some(arr) = msg.get_mut("blocks").and_then(|v| v.as_array_mut()) {
                            arr.extend(blocks.clone());
                            merged = true;
                        }
                        break;
                    }
                }
                if merged {
                    continue;
                }

                if blocks.is_empty() {
                    continue;
                }
                counter += 1;
                out.push(serde_json::json!({
                    "role": "assistant",
                    "blocks": blocks,
                    "id": format!("h-{}", counter),
                    "done": true,
                }));
            }
            "tool" => {
                if let Some(tcid) = &m.tool_call_id {
                    let tcid_val = serde_json::Value::String(tcid.clone());
                    'outer: for msg in out.iter_mut().rev() {
                        if msg["role"] != "assistant" {
                            continue;
                        }
                        if let Some(arr) = msg.get_mut("blocks").and_then(|v| v.as_array_mut()) {
                            for block in arr.iter_mut() {
                                if block["type"] == "tool_call" && block["id"] == tcid_val {
                                    block["output"] = serde_json::json!(text);
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}
