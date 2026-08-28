//! skill_manage_tool 集成测试 —— #151 Phase 8+ 从 tools 层迁出（agents 层）。
//!
//! 这些测试依赖真实 `SkillManager`/`skill_loader`（SKILL.md frontmatter 解析
//! + 写后热重载），L3 层经 api 门面无法构造具体类型，故整体迁到 L4：
//! L4→L3 引用方向合法，测试语义与工具对外行为零改动。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::json;

use crate::agents::workspace::skill_loader;
use crate::agents::workspace::skills::SkillManager;
use crate::providers::Tool;
use crate::tools::SkillManageTool;

fn setup(workspace: &Path, skill_name: &str) -> Arc<RwLock<SkillManager>> {
    let skills_dir = workspace.join("skills").join(skill_name);
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("SKILL.md"),
        format!(
            "---\nname: {}\ndescription: \"Test\"\n---\n# Test\n\nDo stuff.",
            skill_name
        ),
    )
    .unwrap();
    let defs = skill_loader::load_skills_from_dir(&workspace.join("skills"));
    let mut mgr = SkillManager::new();
    for def in &defs {
        mgr.register_definition(def);
    }
    Arc::new(RwLock::new(mgr))
}
#[tokio::test]
async fn test_create_and_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(RwLock::new(SkillManager::new()));
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let result = tool.execute(json!({
        "action": "create", "name": "mys",
        "content": "---\nname: mys\ndescription: \"My skill\"\n---\n# My Skill\n\nDo stuff."
    }), &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() }).await.unwrap();
    assert!(result.success, "{}", result.output);
    assert!(mgr.read().get("mys", None).is_some());
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
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
}
#[tokio::test]
async fn test_create_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup(dir.path(), "existing");
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let result = tool.execute(json!({
        "action": "create", "name": "existing",
        "content": "---\nname: existing\ndescription: \"Exists\"\n---\n# Exists\n\nAlready here."
    }), &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() }).await.unwrap();
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
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("does not match"));
}
#[tokio::test]
async fn test_patch_success() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup(dir.path(), "myskill");
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "myskill",
                "old_string": "Do stuff.", "new_string": "Do something better."
            }),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let content = std::fs::read_to_string(dir.path().join("skills/myskill/SKILL.md")).unwrap();
    assert!(content.contains("Do something better."));
}
#[tokio::test]
async fn test_patch_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup(dir.path(), "myskill");
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let result = tool
        .execute(
            json!({
                "action": "patch", "name": "myskill",
                "old_string": "this does not exist", "new_string": "x"
            }),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("not found"));
}
#[tokio::test]
async fn test_delete() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup(dir.path(), "myskill");
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let result = tool
        .execute(
            json!({"action": "delete", "name": "myskill"}),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(mgr.read().get("myskill", None).is_none());
}
/// Like `setup`, but the skill lives only in a separate shared-library
/// dir (`~/.agents/skills` in production), not under `local_workspace`'s
/// `skills_root` — the scenario issue #93 is about. Returns
/// `(manager, shared_skills_dir)`.
fn setup_shared_only(
    local_workspace: &Path,
    shared_workspace: &Path,
    skill_name: &str,
) -> (Arc<RwLock<SkillManager>>, PathBuf) {
    let shared_skills_dir = shared_workspace.join("skills");
    std::fs::create_dir_all(local_workspace.join("skills")).unwrap();
    let mgr = setup(shared_workspace, skill_name);
    (mgr, shared_skills_dir)
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
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("shared library"), "{}", result.output);
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
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
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
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("read-only"), "{}", result.output);
    assert!(!shared
        .path()
        .join("skills/sharedskill/references/notes.md")
        .exists());
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
        .execute(
            json!({"action": "delete", "name": "self"}),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
}
#[tokio::test]
async fn test_write_and_remove_file() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup(dir.path(), "myskill");
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let wr = tool
        .execute(
            json!({
                "action": "write_file", "name": "myskill",
                "file_path": "references/api.md", "file_content": "# API"
            }),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(wr.success, "{}", wr.output);
    assert!(dir.path().join("skills/myskill/references/api.md").exists());
    let rm = tool
        .execute(
            json!({
                "action": "remove_file", "name": "myskill", "file_path": "references/api.md"
            }),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(rm.success, "{}", rm.output);
    assert!(!dir.path().join("skills/myskill/references/api.md").exists());
}
#[tokio::test]
async fn test_path_traversal_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = setup(dir.path(), "myskill");
    let tool = SkillManageTool::new(Arc::clone(&mgr), dir.path().join("users"), dir.path().to_path_buf(), None);
    let result = tool
        .execute(
            json!({
                "action": "write_file", "name": "myskill",
                "file_path": "references/../../evil.sh", "file_content": "evil"
            }),
            &crate::api::tool::ToolContext { owner: "test".to_string(), session_id: "test".to_string(), agent_name: "main".to_string(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(!result.success);
}
