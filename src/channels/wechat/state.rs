use std::collections::HashMap;
// ── Shared state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub(crate) struct SharedState {
    pub(crate) bot_token: Option<String>,
    pub(crate) bot_wxid: Option<String>,
    pub(crate) bot_nickname: Option<String>,
    pub(crate) get_updates_buf: String,
    pub(crate) typing_tickets: HashMap<String, (String, std::time::Instant)>,
    pub(crate) aes_key: Option<String>,
    pub(crate) context_tokens: HashMap<String, String>,
    /// Per-recipient run_id (UUID) for the current turn. Tool progress
    /// messages and the final text reply share the same run_id so the
    /// WeChat client can group them into a single "AI reply" card.
    pub(crate) run_ids: HashMap<String, String>,
    pub(crate) api_base: Option<String>,
}

pub(crate) fn context_token_path() -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .map(|h| format!("{h}/.myclaw/state"))
        .unwrap_or_else(|_| "/tmp/myclaw-state".to_string());
    std::path::PathBuf::from(format!("{dir}/wechat_context_tokens.json"))
}

pub(crate) fn persist_context_tokens(state: &SharedState) {
    let path = context_token_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string(&state.context_tokens).unwrap_or_default();
    let _ = std::fs::write(path, json);
}

pub(crate) fn load_context_tokens() -> HashMap<String, String> {
    match std::fs::read_to_string(context_token_path()) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub(crate) fn get_updates_buf_path(account_id: &str) -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .map(|h| format!("{h}/.myclaw/state"))
        .unwrap_or_else(|_| "/tmp/myclaw-state".to_string());
    std::path::PathBuf::from(format!("{dir}/wechat_get_updates_buf_{account_id}.json"))
}

/// Persist the get_updates cursor atomically (tmp + rename): the file is
/// rewritten on every successful poll, and a torn write would deserialize
/// into an empty cursor (history re-pull). Fail-open like context tokens.
pub(crate) fn persist_get_updates_buf(account_id: &str, buf: &str) {
    let path = get_updates_buf_path(account_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string(buf).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path));
}

pub(crate) fn load_get_updates_buf(account_id: &str) -> String {
    match std::fs::read_to_string(get_updates_buf_path(account_id)) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => String::new(),
    }
}
