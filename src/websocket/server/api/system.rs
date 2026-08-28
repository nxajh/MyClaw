//! `file.read` / `models.*` / `config.*` / `commands.list` /
//! `daemon.restart` management-API route handlers, extracted verbatim
//! from `client.rs::handle_api_request` (RFC
//! docs/websocket-client-split-rfc.md, batch 2: pure move — each function
//! body is the original match-arm body, unchanged; only the wrapper
//! signature was added.)

use std::sync::Arc;

use super::ApiContext;

pub(super) fn file_read(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let rel_path = match params["path"].as_str() {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "api_error",
                "id": id,
                "error": "missing path parameter"
            }).to_string();
        }
    };
    // Reject absolute paths and traversal attempts.
    if rel_path.starts_with('/') || rel_path.starts_with('\\')
        || rel_path.contains("..") || rel_path.contains('~')
    {
        return serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "invalid path"
        }).to_string();
    }
    match ctx.workspace_dir.get() {
        Some(dir) => {
            let abs = dir.join(rel_path);
            match std::fs::read(&abs) {
                Ok(bytes) => {
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    // Infer MIME from extension.
                    let mime = match abs.extension().and_then(|e| e.to_str()).unwrap_or("") {
                        "jpg" | "jpeg" => "image/jpeg",
                        "png" => "image/png",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        "svg" => "image/svg+xml",
                        "bmp" => "image/bmp",
                        "ico" => "image/x-icon",
                        "mp3" => "audio/mpeg",
                        "ogg" => "audio/ogg",
                        "wav" => "audio/wav",
                        "mp4" => "video/mp4",
                        "webm" => "video/webm",
                        "mov" => "video/quicktime",
                        "mkv" => "video/x-matroska",
                        "avi" => "video/x-msvideo",
                        "flac" => "audio/flac",
                        "m4a" => "audio/m4a",
                        "aac" => "audio/aac",
                        "pdf" => "application/pdf",
                        _ => "application/octet-stream",
                    };
                    serde_json::json!({
                        "type": "api_response",
                        "id": id,
                        "result": { "path": rel_path, "data": b64, "mime": mime, "size": bytes.len() }
                    }).to_string()
                }
                Err(e) => serde_json::json!({
                    "type": "api_error",
                    "id": id,
                    "error": format!("failed to read file: {}", e)
                }).to_string(),
            }
        }
        None => serde_json::json!({
            "type": "api_error",
            "id": id,
            "error": "workspace directory not configured"
        }).to_string(),
    }
}

pub(super) fn config_get(id: &str, ctx: &ApiContext<'_>) -> String {
    let specs = ctx.tool_specs.read();
    let ws_dir = ctx.workspace_dir.get();
    let cfg_path = ctx.config_path.get();
    serde_json::json!({
        "type": "api_response",
        "id": id,
        "result": {
            "tool_count": specs.len(),
            "workspace_dir": ws_dir.map(|p| p.to_string_lossy().to_string()),
            "config_path": cfg_path.map(|p| p.to_string_lossy().to_string()),
        }
    }).to_string()
}

pub(super) fn config_get_raw(id: &str, ctx: &ApiContext<'_>) -> String {
    match ctx.config_path.get() {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(content) => serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": {
                    "content": content,
                    "path": path.to_string_lossy().to_string(),
                }
            }).to_string(),
            Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to read config: {}", e) }).to_string(),
        },
        None => serde_json::json!({ "type": "api_error", "id": id, "error": "config path not set" }).to_string(),
    }
}

pub(super) fn config_save(id: &str, params: &serde_json::Value, ctx: &ApiContext<'_>) -> String {
    let content = match params["content"].as_str() {
        Some(s) => s,
        None => return serde_json::json!({ "type": "api_error", "id": id, "error": "missing content parameter" }).to_string(),
    };
    match ctx.config_path.get() {
        Some(path) => match std::fs::write(path, content) {
            Ok(()) => serde_json::json!({ "type": "api_response", "id": id, "result": null }).to_string(),
            Err(e) => serde_json::json!({ "type": "api_error", "id": id, "error": format!("failed to save config: {}", e) }).to_string(),
        },
        None => serde_json::json!({ "type": "api_error", "id": id, "error": "config path not set" }).to_string(),
    }
}

pub(super) fn daemon_restart(id: &str) -> String {
    // Respond first, then send SIGUSR1 to trigger a hot-switch restart.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        unsafe { libc::kill(std::process::id() as libc::pid_t, libc::SIGUSR1); }
    });
    serde_json::json!({ "type": "api_response", "id": id, "result": { "message": "Restarting…" } }).to_string()
}

pub(super) fn commands_list(id: &str) -> String {
    let result: Vec<serde_json::Value> = crate::commands::command_catalog()
        .into_iter()
        .map(|(name, desc)| serde_json::json!({ "name": name, "description": desc }))
        .collect();
    serde_json::json!({
        "type": "api_response",
        "id": id,
        "result": result,
    }).to_string()
}

pub(super) fn models_list(
    id: &str,
    ctx: &ApiContext<'_>,
    sm: &Arc<crate::agents::SessionManager>,
    user_id: &str,
) -> String {
    match ctx.provider_registry.get() {
        Some(reg) => {
            let model_ids = reg.get_chat_routing_models();
            let active = sm.get_session_override(user_id).model
                .or_else(|| model_ids.first().cloned());
            let models: Vec<serde_json::Value> = model_ids.iter().map(|mid| {
                let supports_image = reg.get_chat_model_config(mid)
                    .map(|c| c.supports_image_input())
                    .unwrap_or(false);
                serde_json::json!({
                    "id": mid,
                    "active": active.as_deref() == Some(mid.as_str()),
                    "supports_image": supports_image,
                })
            }).collect();
            serde_json::json!({
                "type": "api_response",
                "id": id,
                "result": { "models": models, "active": active },
            }).to_string()
        }
        None => serde_json::json!({
            "type": "api_response",
            "id": id,
            "result": { "models": [], "active": null },
        }).to_string(),
    }
}

pub(super) fn models_set(
    id: &str,
    params: &serde_json::Value,
    sm: &Arc<crate::agents::SessionManager>,
    user_id: &str,
) -> String {
    let model = params["model"].as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let mut ov = sm.get_session_override(user_id);
    ov.model = model.map(|s| s.to_string());
    sm.save_session_override(user_id, ov);
    serde_json::json!({
        "type": "api_response",
        "id": id,
        "result": { "model": model },
    }).to_string()
}
