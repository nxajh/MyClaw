use super::*;
use crate::agents::{SessionManager, SkillManager, UserResolver};
use crate::providers::capability_tool::ToolSpec;
use crate::providers::ProviderRegistry;

const USER_KEY: &str = "client:default:web-user:default";
const USER_UID: &str = "myclaw/u/019fe342-test";
const OTHER_UID: &str = "myclaw/u/019fe342-other";

fn mem_body(name: &str, body: &str) -> String {
    // No `scope` field → agent layer (matches legacy agent files).
    format!(
        "---\nname: \"{name}\"\ndescription: \"test entry\"\ntype: \"project\"\ninject: \"search\"\ncreated_at: \"2026-08-14\"\ntags: []\n---\n\n{body}"
    )
}

fn user_mem_body(name: &str, body: &str, uid: &str) -> String {
    format!(
        "---\nname: \"{name}\"\nscope: \"user\"\nuser_id: \"{uid}\"\ndescription: \"test entry\"\ntype: \"project\"\ninject: \"search\"\ncreated_at: \"2026-08-14\"\ntags: []\n---\n\n{body}"
    )
}

fn write_mem(path: &std::path::Path, name: &str, body: &str) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join(format!("{name}.md")), mem_body(name, body)).unwrap();
}

fn write_user_mem(path: &std::path::Path, name: &str, body: &str, uid: &str) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(
        path.join(format!("{name}.md")),
        user_mem_body(name, body, uid),
    )
    .unwrap();
}

fn test_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    // P1-B2: single flat memory dir; ownership via frontmatter.
    // (A name is unique in the flat dir — the old two-layer
    // same-name-in-both-scopes case no longer exists.)
    let mem = ws.join("memory");
    write_mem(&mem, "agent_only", "agent body");
    write_mem(&mem, "agent_second", "second agent body");
    write_user_mem(&mem, "user_only", "user body", USER_UID);
    write_user_mem(&mem, "user_second", "second user body", USER_UID);
    // Another user's private entry — must be invisible to USER_UID.
    write_user_mem(&mem, "other_user_only", "other body", OTHER_UID);
    tmp
}

/// Build an ApiContext with a resolver pinning USER_KEY → USER_UID.
fn api(method: &str, params: serde_json::Value, ws: &std::path::Path) -> serde_json::Value {
    let sm: Arc<OnceLock<Arc<SessionManager>>> = Arc::new(OnceLock::new());
    let wd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
    let ur: Arc<OnceLock<Arc<UserResolver>>> = Arc::new(OnceLock::new());
    let ts: Arc<RwLock<Vec<ToolSpec>>> = Arc::new(RwLock::new(Vec::new()));
    let cp: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
    let sk: Arc<OnceLock<Arc<RwLock<SkillManager>>>> = Arc::new(OnceLock::new());
    let pr: Arc<OnceLock<Arc<dyn ProviderRegistry>>> = Arc::new(OnceLock::new());
    let resolver = Arc::new(UserResolver::new());
    resolver.set(USER_KEY.to_string(), USER_UID.to_string());
    let _ = ur.set(Arc::clone(&resolver));
    // Mirrors daemon.rs: SessionManager and ApiContext share one
    // resolver instance — sessions.list depends on that (G44).
    let _ = sm.set(Arc::new(SessionManager::in_memory().with_resolver(resolver)));
    let _ = wd.set(ws.to_path_buf());
    let kd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
    let _ = kd.set(ws.join("memory"));
    let ctx = ApiContext {
        user_id: USER_KEY,
        session_manager: &sm,
        tool_specs: &ts,
        workspace_dir: &wd,
        memory_root: &kd,
        config_path: &cp,
        skill_manager: &sk,
        provider_registry: &pr,
        user_resolver: &ur,
    };
    let resp = handle_api_request("t1", method, &params, &ctx);
    serde_json::from_str(&resp).unwrap()
}

fn rows(resp: &serde_json::Value) -> Vec<serde_json::Value> {
    resp["result"].as_array().cloned().unwrap_or_default()
}

#[test]
fn memory_list_merges_scopes_with_dedup() {
    let tmp = test_workspace();
    let resp = api("memory.list", serde_json::json!({}), tmp.path());
    assert_eq!(resp["type"], "api_response");
    let r = rows(&resp);
    // agent(2) + own user(2) = 4; other user's entry invisible
    assert_eq!(r.len(), 4);
    let agent = r.iter().find(|x| x["mem_name"] == "agent_only").unwrap();
    assert_eq!(agent["scope"], "agent");
    assert_eq!(agent["content"], "agent body");
    let user_only = r.iter().find(|x| x["mem_name"] == "user_only").unwrap();
    assert_eq!(user_only["scope"], "user");
    // Missing-scope fixture files count as agent layer.
    assert_eq!(
        r.iter()
            .filter(|x| x["scope"] == "agent")
            .count(),
        2
    );
}

#[test]
fn memory_list_scope_filter() {
    let tmp = test_workspace();
    let resp = api(
        "memory.list",
        serde_json::json!({ "scope": "user" }),
        tmp.path(),
    );
    let r = rows(&resp);
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|x| x["scope"] == "user"));
    let resp_agent = api(
        "memory.list",
        serde_json::json!({ "scope": "agent" }),
        tmp.path(),
    );
    let r_agent = rows(&resp_agent);
    assert_eq!(r_agent.len(), 2);
    assert!(r_agent.iter().all(|x| x["scope"] == "agent"));
}

#[test]
fn memory_read_scope_routing() {
    let tmp = test_workspace();
    // No scope: agent entry resolves to agent layer.
    let resp = api(
        "memory.read",
        serde_json::json!({ "name": "agent_only.md" }),
        tmp.path(),
    );
    assert_eq!(resp["result"]["content"], mem_body("agent_only", "agent body"));
    assert_eq!(resp["result"]["scope"], "agent");
    // Explicit user scope on a user-owned file.
    let resp = api(
        "memory.read",
        serde_json::json!({ "name": "user_only.md", "scope": "user" }),
        tmp.path(),
    );
    assert_eq!(resp["result"]["content"], user_mem_body("user_only", "user body", USER_UID));
    assert_eq!(resp["result"]["scope"], "user");
    // User-only entry found via fallback (agent miss → user hit).
    let resp = api(
        "memory.read",
        serde_json::json!({ "name": "user_only.md" }),
        tmp.path(),
    );
    assert_eq!(resp["result"]["content"], user_mem_body("user_only", "user body", USER_UID));
    assert_eq!(resp["result"]["scope"], "user");
    // Asking for user scope on an agent file misses.
    let resp = api(
        "memory.read",
        serde_json::json!({ "name": "agent_only.md", "scope": "user" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_error");
    // Missing entry.
    let resp = api(
        "memory.read",
        serde_json::json!({ "name": "nope" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_error");
}

#[test]
fn memory_write_user_scope() {
    let tmp = test_workspace();
    let resp = api(
        "memory.write",
        serde_json::json!({ "name": "fresh_user.md", "content": "new user body", "scope": "user" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_response");
    let user_path = tmp.path().join("memory").join("fresh_user.md");
    let written = std::fs::read_to_string(&user_path).unwrap();
    assert!(written.contains("scope: user"));
    assert!(written.contains(&format!("user_id: {}", USER_UID)));
    assert!(written.contains("new user body"));
    // Visible to this user via scope=user listing…
    let resp = api(
        "memory.list",
        serde_json::json!({ "scope": "user" }),
        tmp.path(),
    );
    let r = rows(&resp);
    assert!(r.iter().any(|x| x["mem_name"] == "fresh_user"));
    // Default scope (agent) writes the agent marker instead.
    let resp = api(
        "memory.write",
        serde_json::json!({ "name": "default_scope.md", "content": "body" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_response");
    let agent_written =
        std::fs::read_to_string(tmp.path().join("memory").join("default_scope.md")).unwrap();
    assert!(agent_written.contains("scope: agent"));
    assert!(!agent_written.contains("user_id"));
}

#[test]
fn memory_delete_scope_routing() {
    let tmp = test_workspace();
    // Explicit user scope removes the user-owned file.
    let resp = api(
        "memory.delete",
        serde_json::json!({ "name": "user_second.md", "scope": "user" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_response");
    assert!(!tmp.path().join("memory").join("user_second.md").exists());
    // Default fallback removes the agent-layer copy.
    let resp = api(
        "memory.delete",
        serde_json::json!({ "name": "agent_only.md" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_response");
    assert!(!tmp.path().join("memory").join("agent_only.md").exists());        // Another user's entry is not deletable via any scope param.
    let resp = api(
        "memory.delete",
        serde_json::json!({ "name": "other_user_only.md", "scope": "user" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_error");
    assert!(tmp.path().join("memory").join("other_user_only.md").exists());
}

#[test]
fn memory_user_isolation_flat_dir() {
    let tmp = test_workspace();
    // This user sees: agent layer (2) + own user layer (2) — NOT the
    // other user's private entry.
    let resp = api("memory.list", serde_json::json!({}), tmp.path());
    let r = rows(&resp);
    assert_eq!(r.len(), 4);
    assert!(r.iter().any(|x| x["mem_name"] == "agent_only"));
    assert!(r.iter().any(|x| x["mem_name"] == "user_only"));
    assert!(!r.iter().any(|x| x["mem_name"] == "other_user_only"));
    // Read cannot reach the other user's entry either.
    let resp = api(
        "memory.read",
        serde_json::json!({ "name": "other_user_only.md", "scope": "user" }),
        tmp.path(),
    );
    assert_eq!(resp["type"], "api_error");
}

/// Regression test for the bug where linking a channel's routing_key to
/// an existing user (via `/link`) did not surface that user's
/// pre-existing sessions in the web client's session list: `sessions.list`
/// used to call `list_sessions(raw_routing_key)` instead of the
/// resolver-aware `list_sessions_for_user(resolved_uid)` (G44).
#[test]
fn sessions_list_surfaces_linked_channel_sessions() {
    let tmp = test_workspace();
    let sm: Arc<OnceLock<Arc<SessionManager>>> = Arc::new(OnceLock::new());
    let wd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
    let ur: Arc<OnceLock<Arc<UserResolver>>> = Arc::new(OnceLock::new());
    let ts: Arc<RwLock<Vec<ToolSpec>>> = Arc::new(RwLock::new(Vec::new()));
    let cp: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
    let sk: Arc<OnceLock<Arc<RwLock<SkillManager>>>> = Arc::new(OnceLock::new());
    let pr: Arc<OnceLock<Arc<dyn ProviderRegistry>>> = Arc::new(OnceLock::new());
    let kd: Arc<OnceLock<std::path::PathBuf>> = Arc::new(OnceLock::new());
    let _ = kd.set(tmp.path().join("memory"));
    let _ = wd.set(tmp.path().to_path_buf());

    // Both a pre-existing Telegram channel and the web client's own
    // routing_key are folded into the same uid — exactly what
    // `/link` + `/link_confirm` does at runtime.
    const TELEGRAM_KEY: &str = "telegram:default:12345";
    let resolver = Arc::new(UserResolver::new());
    resolver.set(USER_KEY.to_string(), USER_UID.to_string());
    resolver.set(TELEGRAM_KEY.to_string(), USER_UID.to_string());
    let _ = ur.set(Arc::clone(&resolver));

    let session_manager = Arc::new(SessionManager::in_memory().with_resolver(resolver));
    // A session that already existed on the Telegram channel, created
    // before the user ever touched the web client.
    let telegram_session = session_manager
        .new_session(TELEGRAM_KEY, Some("old telegram chat"))
        .unwrap();
    let _ = sm.set(session_manager);

    let ctx = ApiContext {
        user_id: USER_KEY,
        session_manager: &sm,
        tool_specs: &ts,
        workspace_dir: &wd,
        memory_root: &kd,
        config_path: &cp,
        skill_manager: &sk,
        provider_registry: &pr,
        user_resolver: &ur,
    };

    let resp: serde_json::Value = serde_json::from_str(&handle_api_request(
        "t1",
        "sessions.list",
        &serde_json::json!({}),
        &ctx,
    ))
    .unwrap();
    let r = rows(&resp);
    assert!(
        r.iter().any(|s| s["id"] == telegram_session.id),
        "linked channel's pre-existing session should be visible: {r:?}"
    );
}

/// Full-rk receivers (orchestrator replies carry the session key) must
/// keep hitting the bus through the exact candidate.
#[tokio::test]
async fn send_message_full_rk_receiver_hits_bus_directly() {
    let channel = ClientChannel::new(ClientConfig::default());
    let full_key = USER_KEY.to_string();
    channel
        .session_buses
        .write()
        .insert(full_key.clone(), Arc::new(SyncMutex::new(SessionOutputBus::new())));

    let msg = ChannelOutboundMessage::text(USER_KEY, "direct hit");
    channel.send_message(&msg).await.unwrap();

    let bus = channel.session_buses.read().get(&full_key).unwrap().clone();
    let queued = bus.lock().drain_messages();
    assert_eq!(queued.len(), 1);
    assert!(queued[0].contains("direct hit"));
}

/// Legacy key form: the resolver still maps `client:default:ws-3` to the
/// same uid as the live identity key. A receiver.id of `ws-3` (a bare
/// tail that only exists in the resolver) must resolve through
/// routing_keys_for onto the identity bus.
#[tokio::test]
async fn send_message_legacy_key_resolves_via_resolver_to_identity_bus() {
    let channel = ClientChannel::new(ClientConfig::default());
    let resolver = Arc::new(UserResolver::new());
    resolver.set("client:default:ws-3".to_string(), USER_UID.to_string());
    resolver.set(USER_KEY.to_string(), USER_UID.to_string());
    channel.set_user_resolver(resolver);

    // Only the identity-key bus exists — the legacy `ws-3` bus does not.
    channel
        .session_buses
        .write()
        .insert(USER_KEY.to_string(), Arc::new(SyncMutex::new(SessionOutputBus::new())));

    let msg = ChannelOutboundMessage::text("ws-3", "legacy addressed");
    channel.send_message(&msg).await.unwrap();

    let bus = channel.session_buses.read().get(USER_KEY).unwrap().clone();
    let queued = bus.lock().drain_messages();
    assert_eq!(
        queued.len(),
        1,
        "legacy key must resolve onto the identity bus via the resolver"
    );
    assert!(queued[0].contains("legacy addressed"));
}

/// Total miss → Err (not Ok-with-empty). notify_peer relies on this to
/// tell the user the peer's channel is unreachable instead of lying
/// about a delivered /link code.
#[tokio::test]
async fn send_message_unknown_recipient_returns_err() {
    let channel = ClientChannel::new(ClientConfig::default());
    channel
        .session_buses
        .write()
        .insert(USER_KEY.to_string(), Arc::new(SyncMutex::new(SessionOutputBus::new())));

    let msg = ChannelOutboundMessage::text("web-user:nobody", "should not deliver");
    let err = channel
        .send_message(&msg)
        .await
        .expect_err("total candidate miss must be an Err");
    assert!(
        err.to_string().contains("no live client bus"),
        "unexpected error text: {err}"
    );

    // Nothing was pushed anywhere.
    let bus = channel.session_buses.read().get(USER_KEY).unwrap().clone();
    assert!(bus.lock().drain_messages().is_empty());
}

/// Cross-channel keys folded into the same uid (e.g. `telegram:*` from a
/// /link) must never become client bus candidates: the telegram bus does
/// not exist in this channel, and prefixing it would be wrong.
#[test]
fn bus_key_candidates_skips_cross_channel_keys() {
    let resolver = Arc::new(UserResolver::new());
    resolver.set(USER_KEY.to_string(), USER_UID.to_string());
    resolver.set("telegram:default:12345".to_string(), USER_UID.to_string());
    let holder: Arc<OnceLock<Arc<UserResolver>>> = Arc::new(OnceLock::new());
    let _ = holder.set(resolver);

    // Bare tail: candidates = [tail, client:default:tail] plus any
    // client:default:* key of the same uid — never the telegram key.
    let bare = bus_key_candidates(&holder, "web-user:default");
    assert_eq!(
        bare,
        vec![
            "web-user:default".to_string(),
            USER_KEY.to_string(),
        ],
        "cross-channel keys must not appear: {bare:?}"
    );

    // Legacy key with a client:default sibling in the resolver.
    let resolver2 = Arc::new(UserResolver::new());
    resolver2.set("client:default:ws-3".to_string(), USER_UID.to_string());
    resolver2.set(USER_KEY.to_string(), USER_UID.to_string());
    resolver2.set("telegram:default:12345".to_string(), USER_UID.to_string());
    let holder2: Arc<OnceLock<Arc<UserResolver>>> = Arc::new(OnceLock::new());
    let _ = holder2.set(resolver2);
    let legacy = bus_key_candidates(&holder2, "ws-3");
    assert_eq!(
        legacy,
        vec![
            "ws-3".to_string(),
            "client:default:ws-3".to_string(),
            USER_KEY.to_string(),
        ],
        "expected exact → normalized → resolver identity key: {legacy:?}"
    );
    assert!(!legacy.iter().any(|k| k.starts_with("telegram:")));
}

/// create_stream resolves through the same candidate list: a bare tail
/// must find the identity bus, and a miss must yield None (caller falls
/// back to non-streaming send).
#[test]
fn create_stream_resolves_bare_tail_and_misses_to_none() {
    let channel = ClientChannel::new(ClientConfig::default());
    channel
        .session_buses
        .write()
        .insert(USER_KEY.to_string(), Arc::new(SyncMutex::new(SessionOutputBus::new())));

    let stream = channel.create_stream("web-user:default");
    assert!(stream.is_some(), "bare tail should resolve the identity bus");

    assert!(
        channel.create_stream("web-user:ghost").is_none(),
        "miss must return None"
    );
}
