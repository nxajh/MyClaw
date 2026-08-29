//! skill_manage_tool 集成测试 —— #151 Phase 8+ 从 tools 层迁出（agents 层）。
//!
//! 这些测试依赖真实 `SkillManager`/`skill_loader`（SKILL.md frontmatter 解析
//! + 写后热重载），L3 层经 api 门面无法构造具体类型，故整体迁到 L4：
//! L4→L3 引用方向合法，测试语义与工具对外行为零改动。
//!
//! RFC #101 P1.1（2026-08-29 修订为 fork 模型）：user 层直接写；agent 层
//! （skills_root）与 shared 层（agents_skills_dir）原始版只读——写操作惰性
//! fork 到本人 user 层副本（§2.6），delete 对非 user 层直接拒绝。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::json;

use crate::agents::workspace::skill_loader;
use crate::agents::workspace::skills::SkillManager;
use crate::identity::user_profile::UserResolver;
use crate::providers::Tool;
use crate::tools::SkillManageTool;

/// Standard fixture SKILL.md body — every layer's copy starts identical so
/// tests can tell them apart only by path.
fn write_skill(skills_dir: &Path, skill_name: &str) {
    let dir = skills_dir.join(skill_name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {}\ndescription: \"Test\"\n---\n# Test\n\nDo stuff.",
            skill_name
        ),
    )
    .unwrap();
}

fn ctx(owner: &str) -> crate::api::tool::ToolContext {
    crate::api::tool::ToolContext {
        owner: owner.to_string(),
        session_id: "test".to_string(),
        agent_name: "main".to_string(),
        ..Default::default()
    }
}

/// User-layer fixture (RFC #101 P1): the skill lives in
/// `users/{owner}/skills/{name}` — the only layer written in place;
/// non-user layers are forked into it (RFC #101 §2.6).
fn setup_user(workspace: &Path, owner: &str, skill_name: &str) -> Arc<RwLock<SkillManager>> {
    write_skill(
        &workspace.join("users").join(owner).join("skills"),
        skill_name,
    );
    let users_map = skill_loader::load_all_users_skills(&workspace.join("users"));
    let mut mgr = SkillManager::new();
    mgr.reload_from_definitions(users_map, Vec::new(), Vec::new());
    Arc::new(RwLock::new(mgr))
}

/// Agent-layer fixture: the skill lives in `workspace/skills/{name}`
/// (`skills_root` — the system-level, git-managed layer). A read-only
/// original: mutating actions fork it into the caller's user layer
/// (RFC #101 §2.6), `delete` is refused.
fn setup_agent(workspace: &Path, skill_name: &str) -> Arc<RwLock<SkillManager>> {
    write_skill(&workspace.join("skills"), skill_name);
    let agent_defs = skill_loader::load_skills_from_dir(&workspace.join("skills"));
    let mut mgr = SkillManager::new();
    mgr.reload_from_definitions(std::collections::HashMap::new(), agent_defs, Vec::new());
    Arc::new(RwLock::new(mgr))
}

/// Both-layer fixture: same-named skill exists in the agent layer AND the
/// owner's user layer — the registry must resolve the user copy (shadowing)
/// and skill_manage must write only that copy.
fn setup_user_shadowing_agent(
    workspace: &Path,
    owner: &str,
    skill_name: &str,
) -> Arc<RwLock<SkillManager>> {
    write_skill(&workspace.join("skills"), skill_name);
    write_skill(
        &workspace.join("users").join(owner).join("skills"),
        skill_name,
    );
    let users_map = skill_loader::load_all_users_skills(&workspace.join("users"));
    let agent_defs = skill_loader::load_skills_from_dir(&workspace.join("skills"));
    let mut mgr = SkillManager::new();
    mgr.reload_from_definitions(users_map, agent_defs, Vec::new());
    Arc::new(RwLock::new(mgr))
}

#[tokio::test]
async fn test_create_and_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(RwLock::new(SkillManager::new()));
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().to_path_buf(),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "create", "name": "mys",
                "content": "---\nname: mys\ndescription: \"My skill\"\n---\n# My Skill\n\nDo stuff."
            }),
            &crate::api::tool::ToolContext {
                owner: "test".to_string(),
                session_id: "test".to_string(),
                agent_name: "main".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    // create writes into the owner's user layer (users/{owner}/skills) —
    // assert against that owner's view, not the ownerless agent+shared view.
    assert!(mgr.read().get("mys", Some("test")).is_some());
}
#[tokio::test]
async fn test_create_reserved_name() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SkillManageTool::new(
        Arc::new(RwLock::new(SkillManager::new())),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().to_path_buf(),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "create", "name": "self",
                "content": "---\nname: self\ndescription: \"x\"\n---\n# x\n\nBody."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(!result.success);
}
#[tokio::test]
async fn test_create_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "existing");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool.execute(json!({
        "action": "create", "name": "existing",
        "content": "---\nname: existing\ndescription: \"Exists\"\n---\n# Exists\n\nAlready here."
    }), &ctx("test")).await.unwrap();
    assert!(!result.success);
    assert!(result.output.contains("already exists"));
}
#[tokio::test]
async fn test_name_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SkillManageTool::new(
        Arc::new(RwLock::new(SkillManager::new())),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().to_path_buf(),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "create", "name": "myskill",
                "content": "---\nname: other\ndescription: \"Test\"\n---\n# Body\n\nContent."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("does not match"));
}
#[tokio::test]
async fn test_patch_success() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "myskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "myskill",
                "old_string": "Do stuff.", "new_string": "Do something better."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let content =
        std::fs::read_to_string(dir.path().join("users/test/skills/myskill/SKILL.md")).unwrap();
    assert!(content.contains("Do something better."));
}
#[tokio::test]
async fn test_patch_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "myskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "myskill",
                "old_string": "this does not exist", "new_string": "x"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("not found"));
}
#[tokio::test]
async fn test_delete() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "myskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(json!({"action": "delete", "name": "myskill"}), &ctx("test"))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(!dir.path().join("users/test/skills/myskill").exists());
    // user-layer skill — check the owner-scoped view after hot reload
    assert!(mgr.read().get("myskill", Some("test")).is_none());
}
/// Like the layered fixtures, but the skill lives only in a separate
/// shared-library dir (`~/.agents/skills` in production), not under
/// `local_workspace`'s `skills_root` — the scenario issue #93 is about.
/// Returns `(manager, shared_skills_dir)`. The registry placement is the
/// flat agent map (matches how the pre-P1 loader registered shared defs);
/// `get_skill_dir` guards on the resolved directory path, not the map.
fn setup_shared_only(
    local_workspace: &Path,
    shared_workspace: &Path,
    skill_name: &str,
) -> (Arc<RwLock<SkillManager>>, PathBuf) {
    let shared_skills_dir = shared_workspace.join("skills");
    std::fs::create_dir_all(local_workspace.join("skills")).unwrap();
    write_skill(&shared_skills_dir, skill_name);
    let defs = skill_loader::load_skills_from_dir(&shared_skills_dir);
    let mut mgr = SkillManager::new();
    for def in &defs {
        mgr.register_definition(def);
    }
    (Arc::new(RwLock::new(mgr)), shared_skills_dir)
}
/// Parse the `.fork-origin` sidecar of a forked user-layer copy and
/// return its `source_layer` (RFC #101 §2.6).
fn fork_origin_source_layer(forked_skill_dir: &Path) -> String {
    let body = std::fs::read_to_string(forked_skill_dir.join(".fork-origin"))
        .expect(".fork-origin sidecar must exist in the forked copy");
    let origin: serde_json::Value =
        serde_json::from_str(&body).expect(".fork-origin must be valid JSON");
    origin["source_layer"]
        .as_str()
        .expect(".fork-origin must carry source_layer")
        .to_string()
}

#[tokio::test]
async fn test_delete_rejects_shared_library_skill() {
    let local = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let (mgr, shared_skills_dir) = setup_shared_only(local.path(), shared.path(), "sharedskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        local.path().join("users"),
        local.path().join("skills"),
        Some(shared_skills_dir),
    );
    let result = tool
        .execute(
            json!({"action": "delete", "name": "sharedskill"}),
            &ctx("test"),
        )
        .await
        .unwrap();
    // fork model (RFC #101 §2.6): delete of a non-user-layer original is
    // refused outright — forking just to delete the copy is meaningless.
    assert!(!result.success);
    assert!(
        result.output.contains("shared original"),
        "{}",
        result.output
    );
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        shared.path().join("skills/sharedskill/SKILL.md").exists(),
        "shared skill file must survive a rejected delete"
    );
    assert!(mgr.read().get("sharedskill", None).is_some());
}
#[tokio::test]
async fn test_patch_forks_shared_library_skill() {
    let local = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let (mgr, shared_skills_dir) = setup_shared_only(local.path(), shared.path(), "sharedskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        local.path().join("users"),
        local.path().join("skills"),
        Some(shared_skills_dir),
    );
    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "sharedskill",
                "old_string": "Do stuff.", "new_string": "Do something else."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    // The #93 read-only interception is superseded by the fork channel:
    // the patch lands on a fresh user-layer copy, the shared original is
    // byte-identical.
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);
    let copy = std::fs::read_to_string(
        local.path().join("users/test/skills/sharedskill/SKILL.md"),
    )
    .unwrap();
    assert!(copy.contains("Do something else."));
    assert_eq!(
        fork_origin_source_layer(&local.path().join("users/test/skills/sharedskill")),
        "shared"
    );
    assert_eq!(
        std::fs::read_to_string(shared.path().join("skills/sharedskill/SKILL.md")).unwrap(),
        "---\nname: sharedskill\ndescription: \"Test\"\n---\n# Test\n\nDo stuff.",
        "shared original must be byte-identical after the fork+patch"
    );
}
#[tokio::test]
async fn test_write_file_forks_shared_library_skill() {
    let local = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let (mgr, shared_skills_dir) = setup_shared_only(local.path(), shared.path(), "sharedskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        local.path().join("users"),
        local.path().join("skills"),
        Some(shared_skills_dir),
    );
    let result = tool
        .execute(
            json!({
                "action": "write_file", "name": "sharedskill",
                "file_path": "references/notes.md", "file_content": "hi"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);
    assert!(
        local
            .path()
            .join("users/test/skills/sharedskill/references/notes.md")
            .exists(),
        "the file must land in the forked user-layer copy"
    );
    assert_eq!(
        fork_origin_source_layer(&local.path().join("users/test/skills/sharedskill")),
        "shared"
    );
    assert!(
        !shared
            .path()
            .join("skills/sharedskill/references/notes.md")
            .exists(),
        "the shared library must stay untouched"
    );
}
#[tokio::test]
async fn test_delete_self_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SkillManageTool::new(
        Arc::new(RwLock::new(SkillManager::new())),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().to_path_buf(),
        None,
    );
    let result = tool
        .execute(json!({"action": "delete", "name": "self"}), &ctx("test"))
        .await
        .unwrap();
    assert!(!result.success);
}
#[tokio::test]
async fn test_write_and_remove_file() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "myskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let wr = tool
        .execute(
            json!({
                "action": "write_file", "name": "myskill",
                "file_path": "references/api.md", "file_content": "# API"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(wr.success, "{}", wr.output);
    assert!(
        dir.path()
            .join("users/test/skills/myskill/references/api.md")
            .exists()
    );
    let rm = tool
        .execute(
            json!({
                "action": "remove_file", "name": "myskill", "file_path": "references/api.md"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(rm.success, "{}", rm.output);
    assert!(
        !dir.path()
            .join("users/test/skills/myskill/references/api.md")
            .exists()
    );
}
#[tokio::test]
async fn test_path_traversal_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "myskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "write_file", "name": "myskill",
                "file_path": "references/../../evil.sh", "file_content": "evil"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(!result.success);
}

// ── RFC #101 §2.6: agent layer (skills_root) — read-only originals,
//    mutating writes lazily fork into the caller's user layer ─────────────────

/// Byte-expectation for a `write_skill` fixture — originals must stay
/// byte-identical across a fork, not merely "contain" their old text.
const AGENT_ORIGINAL: &str = "---\nname: agentskill\ndescription: \"Test\"\n---\n# Test\n\nDo stuff.";

#[tokio::test]
async fn test_edit_forks_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "edit", "name": "agentskill",
                "content": "---\nname: agentskill\ndescription: \"Test\"\n---\n# Test\n\nForked and rewritten."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);
    let copy =
        std::fs::read_to_string(dir.path().join("users/test/skills/agentskill/SKILL.md")).unwrap();
    assert!(copy.contains("Forked and rewritten."));
    assert_eq!(
        fork_origin_source_layer(&dir.path().join("users/test/skills/agentskill")),
        "agent"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("skills/agentskill/SKILL.md")).unwrap(),
        AGENT_ORIGINAL,
        "agent-layer original must be byte-identical after the fork+edit"
    );
}
#[tokio::test]
async fn test_patch_forks_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "agentskill",
                "old_string": "Do stuff.", "new_string": "Do something else."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);
    assert!(result.output.contains("original is untouched"), "{}", result.output);
    // the patch landed on the user-layer copy…
    let copy =
        std::fs::read_to_string(dir.path().join("users/test/skills/agentskill/SKILL.md")).unwrap();
    assert!(copy.contains("Do something else."));
    // …the original is byte-identical…
    assert_eq!(
        std::fs::read_to_string(dir.path().join("skills/agentskill/SKILL.md")).unwrap(),
        AGENT_ORIGINAL,
        "agent-layer original must be byte-identical after the fork+patch"
    );
    // …provenance recorded (§2.6 sidecar)…
    assert_eq!(
        fork_origin_source_layer(&dir.path().join("users/test/skills/agentskill")),
        "agent"
    );
    // …and the copy shadows the original for its owner (user > agent).
    let resolved = mgr
        .read()
        .skill_dir("agentskill", Some("test"))
        .expect("skill must still resolve after the fork")
        .to_path_buf();
    assert!(
        resolved.starts_with(dir.path().join("users/test/skills")),
        "post-fork resolution must prefer the user-layer copy, got {}",
        resolved.display()
    );
}
#[tokio::test]
async fn test_write_file_forks_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "write_file", "name": "agentskill",
                "file_path": "references/notes.md", "file_content": "hi"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);
    assert!(
        dir.path()
            .join("users/test/skills/agentskill/references/notes.md")
            .exists(),
        "the file must land in the forked user-layer copy"
    );
    assert!(
        !dir.path()
            .join("skills/agentskill/references/notes.md")
            .exists(),
        "the agent layer must stay untouched"
    );
}
#[tokio::test]
async fn test_remove_file_forks_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    // A real target file in the original, so the removal proves the fork
    // path (the copy inherits the file, then drops it there).
    std::fs::create_dir_all(dir.path().join("skills/agentskill/references")).unwrap();
    std::fs::write(
        dir.path().join("skills/agentskill/references/notes.md"),
        "note",
    )
    .unwrap();
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "remove_file", "name": "agentskill",
                "file_path": "references/notes.md"
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);
    // removed in the forked copy…
    assert!(
        !dir
            .path()
            .join("users/test/skills/agentskill/references/notes.md")
            .exists(),
        "the file must be gone from the user-layer copy"
    );
    // …still present (byte-identical dir) in the original.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("skills/agentskill/references/notes.md")).unwrap(),
        "note",
        "the agent-layer original must keep its file"
    );
}
#[tokio::test]
async fn test_delete_rejects_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({"action": "delete", "name": "agentskill"}),
            &ctx("test"),
        )
        .await
        .unwrap();
    // fork model (RFC #101 §2.6): forking just to delete the copy would
    // be meaningless — non-user-layer targets are refused outright.
    assert!(!result.success);
    assert!(result.output.contains("shared original"), "{}", result.output);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        dir.path().join("skills/agentskill/SKILL.md").exists(),
        "agent-layer skill must survive a rejected delete"
    );
    assert!(mgr.read().get("agentskill", None).is_some());
}
#[tokio::test]
async fn test_delete_user_fork_copy_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    // Create the fork (recovery baseline: a broken copy can be deleted
    // and re-forked — RFC #101 §2.6 motivation).
    let forked = tool
        .execute(
            json!({
                "action": "patch", "name": "agentskill",
                "old_string": "Do stuff.", "new_string": "My own take."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(forked.success, "{}", forked.output);
    // Deleting the personal fork is an ordinary user-layer delete.
    let result = tool
        .execute(
            json!({"action": "delete", "name": "agentskill"}),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(
        !dir.path().join("users/test/skills/agentskill").exists(),
        "the user-layer fork must be deleted"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("skills/agentskill/SKILL.md")).unwrap(),
        AGENT_ORIGINAL,
        "the agent-layer original must survive as the recovery baseline"
    );
}
#[tokio::test]
async fn test_create_agent_layer_duplicate_points_to_fork() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "create", "name": "agentskill",
                "content": "---\nname: agentskill\ndescription: \"Mine\"\n---\n# Mine\n\nBody."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("agent layer"), "{}", result.output);
    assert!(result.output.contains("patch"), "{}", result.output);
    assert!(result.output.contains("auto-fork"), "{}", result.output);
    // no fork was created — create is not a fork channel.
    assert!(
        !dir.path().join("users/test/skills/agentskill").exists(),
        "create must not fork the agent-layer skill"
    );
}
#[tokio::test]
async fn test_edit_targets_user_copy_when_shadowing_agent_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user_shadowing_agent(dir.path(), "test", "shadowme");
    // Registry resolution prefers the user copy (user > agent > shared).
    let resolved = mgr
        .read()
        .skill_dir("shadowme", Some("test"))
        .expect("shadowing skill must resolve")
        .to_path_buf();
    assert!(resolved.starts_with(dir.path().join("users/test/skills")));

    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "edit", "name": "shadowme",
                "content": "---\nname: shadowme\ndescription: \"Test\"\n---\n# Test\n\nUser layer wins."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let user_md =
        std::fs::read_to_string(dir.path().join("users/test/skills/shadowme/SKILL.md")).unwrap();
    assert!(user_md.contains("User layer wins."));
    let agent_md = std::fs::read_to_string(dir.path().join("skills/shadowme/SKILL.md")).unwrap();
    assert!(
        agent_md.contains("Do stuff."),
        "agent-layer file must be untouched, got: {agent_md}"
    );
}
#[tokio::test]
async fn test_write_actions_reject_path_like_skill_names() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_user(dir.path(), "test", "myskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(UserResolver::new()),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    for bad in ["../evil", "a/b", ".."] {
        let result = tool
            .execute(json!({"action": "delete", "name": bad}), &ctx("test"))
            .await
            .unwrap();
        assert!(!result.success, "name '{bad}' must be rejected");
        assert!(
            result.output.contains("Invalid skill name"),
            "'{bad}': {}",
            result.output
        );
    }
    // the user-layer skill itself is untouched by the rejected calls
    assert!(
        dir.path()
            .join("users/test/skills/myskill/SKILL.md")
            .exists()
    );
}

/// Issue #101 hotfix (fork model): daemon tool execution passes
/// `ctx.owner` = the session's routing key (e.g.
/// `client:default:web-user:default`), NOT the owner FQID. The injected
/// UserResolver must normalize it before any user-layer path or registry
/// query — a patch on an agent-layer skill forks into the owner's real
/// `users/{uuid}/skills` tree, never into a routing-key-named directory,
/// and the registry must afterwards resolve the fork for that owner
/// (instead of re-resolving the agent original → re-fork).
#[tokio::test]
async fn test_fork_normalizes_routing_key_owner_to_uuid_dir() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let routing_key = "client:default:web-user:default";
    let fqid = "myclaw/u/01234567-89ab-cdef-0123-456789abcdef";
    let resolver = UserResolver::new();
    resolver.set(routing_key, fqid);
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        Arc::new(resolver),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );

    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "agentskill",
                "old_string": "Do stuff.", "new_string": "Do it right."
            }),
            &ctx(routing_key),
        )
        .await
        .unwrap();

    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("\"forked\":true"), "{}", result.output);

    // The fork landed in the resolved owner's uuid directory…
    let uuid_copy = dir
        .path()
        .join("users")
        .join("01234567-89ab-cdef-0123-456789abcdef")
        .join("skills")
        .join("agentskill");
    assert!(uuid_copy.join("SKILL.md").is_file());
    let patched = std::fs::read_to_string(uuid_copy.join("SKILL.md")).unwrap();
    assert!(patched.contains("Do it right."));
    assert_eq!(fork_origin_source_layer(&uuid_copy), "agent");
    // …and no routing-key-named garbage directory was ever created.
    assert!(
        !dir.path().join("users").join(routing_key).exists(),
        "fork must not create a directory named after the raw routing key"
    );

    // The refreshed registry resolves the user copy for the resolved
    // owner (no second fork on the next write).
    let resolved = mgr
        .read()
        .skill_dir("agentskill", Some(fqid))
        .map(|p| p.to_path_buf())
        .expect("registry must resolve the forked copy for the owner");
    assert!(resolved.starts_with(&uuid_copy));
}
