//! skill_manage_tool 集成测试 —— #151 Phase 8+ 从 tools 层迁出（agents 层）。
//!
//! 这些测试依赖真实 `SkillManager`/`skill_loader`（SKILL.md frontmatter 解析
//! + 写后热重载），L3 层经 api 门面无法构造具体类型，故整体迁到 L4：
//! L4→L3 引用方向合法，测试语义与工具对外行为零改动。
//!
//! RFC #101 P1.1 后 skill_manage 写操作只落 user 层：agent 层（skills_root）
//! 与 shared 层（agents_skills_dir）一律只读拦截，见各 fixture 注释。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::json;

use crate::agents::workspace::skill_loader;
use crate::agents::workspace::skills::SkillManager;
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
/// `users/{owner}/skills/{name}` — the only layer `skill_manage` write
/// actions may touch since P1.1.
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
/// (`skills_root` — the system-level, git-managed layer). P1.1 makes it
/// read-only for every skill_manage write action.
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
#[tokio::test]
async fn test_delete_rejects_shared_library_skill() {
    let local = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let (mgr, shared_skills_dir) = setup_shared_only(local.path(), shared.path(), "sharedskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(
        result.output.contains("shared library"),
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
async fn test_patch_rejects_shared_library_skill() {
    let local = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let (mgr, shared_skills_dir) = setup_shared_only(local.path(), shared.path(), "sharedskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(result.output.contains("read-only"), "{}", result.output);
    let content =
        std::fs::read_to_string(shared.path().join("skills/sharedskill/SKILL.md")).unwrap();
    assert!(
        content.contains("Do stuff."),
        "shared skill content must be unchanged, got: {content}"
    );
}
#[tokio::test]
async fn test_write_file_rejects_shared_library_skill() {
    let local = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let (mgr, shared_skills_dir) = setup_shared_only(local.path(), shared.path(), "sharedskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        !shared
            .path()
            .join("skills/sharedskill/references/notes.md")
            .exists()
    );
}
#[tokio::test]
async fn test_delete_self_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SkillManageTool::new(
        Arc::new(RwLock::new(SkillManager::new())),
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

// ── RFC #101 P1.1: agent layer (skills_root) is read-only for writes ─────────

#[tokio::test]
async fn test_edit_rejects_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
        dir.path().join("users"),
        dir.path().join("skills"),
        None,
    );
    let result = tool
        .execute(
            json!({
                "action": "edit", "name": "agentskill",
                "content": "---\nname: agentskill\ndescription: \"Test\"\n---\n# Test\n\nHijacked."
            }),
            &ctx("test"),
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("agent layer"), "{}", result.output);
    assert!(result.output.contains("read-only"), "{}", result.output);
    let content = std::fs::read_to_string(dir.path().join("skills/agentskill/SKILL.md")).unwrap();
    assert!(
        content.contains("Do stuff."),
        "agent-layer file must be unchanged, got: {content}"
    );
}
#[tokio::test]
async fn test_patch_rejects_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(result.output.contains("agent layer"), "{}", result.output);
    assert!(result.output.contains("read-only"), "{}", result.output);
    let content = std::fs::read_to_string(dir.path().join("skills/agentskill/SKILL.md")).unwrap();
    assert!(
        content.contains("Do stuff."),
        "agent-layer file must be unchanged, got: {content}"
    );
}
#[tokio::test]
async fn test_delete_rejects_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(result.output.contains("agent layer"), "{}", result.output);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        dir.path().join("skills/agentskill/SKILL.md").exists(),
        "agent-layer skill must survive a rejected delete"
    );
    assert!(mgr.read().get("agentskill", None).is_some());
}
#[tokio::test]
async fn test_write_file_rejects_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(result.output.contains("agent layer"), "{}", result.output);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        !dir.path()
            .join("skills/agentskill/references/notes.md")
            .exists()
    );
}
#[tokio::test]
async fn test_remove_file_rejects_agent_layer_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup_agent(dir.path(), "agentskill");
    // A real target file, so the rejection proves path interception (not a
    // missing-file shortcut).
    std::fs::create_dir_all(dir.path().join("skills/agentskill/references")).unwrap();
    std::fs::write(
        dir.path().join("skills/agentskill/references/notes.md"),
        "note",
    )
    .unwrap();
    let tool = SkillManageTool::new(
        Arc::clone(&mgr),
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
    assert!(!result.success);
    assert!(result.output.contains("agent layer"), "{}", result.output);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(
        dir.path()
            .join("skills/agentskill/references/notes.md")
            .exists(),
        "agent-layer file must survive a rejected remove_file"
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
