//! Memory tool search behavior tests.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use crate::agents::session::Session;
    use crate::agents::user_profile::UserResolver;
    use crate::providers::Tool;
    use crate::tools::{MemoryListTool, MemoryManageTool, MemorySearchTool};

    fn session() -> Session {
        let mut session = Session::new("test-session".to_string());
        session.owner = "test-user".to_string();
        session
    }

    fn write_memory(
        dir: &std::path::Path,
        name: &str,
        mem_type: &str,
        description: &str,
        tags: &str,
        content: &str,
    ) {
        let memory_dir = dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: {description}\ntags: [{tags}]\ntype: {mem_type}\ncreated_at: 2026-07-05\n---\n\n{content}"
            ),
        )
        .unwrap();
    }

    async fn search(tool: &MemorySearchTool, args: Value) -> Value {
        let result = tool.execute(args, &session()).await.unwrap();
        assert!(result.success);
        serde_json::from_str(&result.output).unwrap()
    }

    #[tokio::test]
    async fn memory_search_empty_type_searches_all_types() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "arm-host",
            "project",
            "ARM host SSH port 31822",
            "arm, ssh",
            "oci-arm-1.jinl.in uses SSH port 31822.",
        );
        let tool = MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        for memory_type in [None, Some(""), Some("   ")] {
            let mut args = json!({"query": "31822"});
            if let Some(memory_type) = memory_type {
                args["memory_type"] = json!(memory_type);
            }
            let output = search(&tool, args).await;
            assert_eq!(output["count"], 1);
            assert_eq!(output["results"][0]["name"], "arm-host");
        }
    }

    #[tokio::test]
    async fn memory_search_type_filter_still_filters_when_explicit() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "arm-host",
            "project",
            "ARM host SSH port 31822",
            "arm, ssh",
            "oci-arm-1.jinl.in uses SSH port 31822.",
        );
        let tool = MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "31822", "memory_type": "user"})).await;
        assert_eq!(output["count"], 0);
    }

    #[tokio::test]
    async fn memory_search_broad_query_uses_or_scoring() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "n8n_daily_workflow_fix",
            "project",
            "n8n RSS AI workflow on ARM host",
            "n8n, arm, workflow, ssh-port-31822",
            "域名：oci-arm-1.jinl.in，SSH 端口 31822，用户 ubuntu。",
        );
        let tool = MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        let output = search(
            &tool,
            json!({"query": "ARM ssh port Ampere 10.0.2.100 oci-arm", "memory_type": "project"}),
        )
        .await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "n8n_daily_workflow_fix");
    }

    #[tokio::test]
    async fn memory_search_ascii_tokens_do_not_match_inside_words() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "client-channel",
            "project",
            "WebSocket client transport",
            "websocket",
            "WebSocket-based unified client transport for Web UI.",
        );
        write_memory(
            dir.path(),
            "ssh-host",
            "project",
            "SSH port 31822",
            "ssh",
            "SSH port 31822 is exposed on the ARM host.",
        );
        let tool = MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "ssh port"})).await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "ssh-host");
    }

    #[tokio::test]
    async fn memory_search_ascii_boundary_allows_hyphenated_tags() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "arm-host",
            "project",
            "ARM host",
            "ssh-port-31822",
            "ARM host connection details.",
        );
        let tool = MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "port"})).await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "arm-host");
    }

    #[tokio::test]
    async fn memory_search_non_ascii_tokens_still_use_substring_match() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "openlist",
            "project",
            "OpenList 百度网盘服务",
            "openlist",
            "百度网盘管理服务部署在 ARM 主机。",
        );
        let tool = MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "网盘"})).await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "openlist");
    }

    #[tokio::test]
    async fn memory_list_empty_type_lists_all_types() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(
            dir.path(),
            "project-memory",
            "project",
            "Project memory",
            "project",
            "Project details.",
        );
        let tool = MemoryListTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));

        let result = tool
            .execute(json!({"memory_type": ""}), &session())
            .await
            .unwrap();
        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["count"], 1);
        assert_eq!(output["entries"][0]["name"], "project-memory");
    }

    #[tokio::test]
    async fn memory_manage_empty_type_defaults_or_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryManageTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));
        let session = session();

        let result = tool
            .execute(
                json!({
                    "action": "add",
                    "name": "empty-type-add",
                    "memory_type": "   ",
                    "description": "empty type add",
                    "content": "body"
                }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["type"], "project");

        let result = tool
            .execute(
                json!({
                    "action": "replace",
                    "name": "empty-type-add",
                    "memory_type": "",
                    "content": "updated body"
                }),
                &session,
            )
            .await
            .unwrap();
        assert!(result.success);

        let search_tool =
            MemorySearchTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));
        let output = search(
            &search_tool,
            json!({"query": "updated", "memory_type": "project"}),
        )
        .await;
        assert_eq!(output["count"], 1);
    }

    // ── two-tier memory scope tests (P0) ──────────────────────────────────

    async fn manage(tool: &MemoryManageTool, args: Value, session: &Session) -> Value {
        let result = tool.execute(args, session).await.unwrap();
        serde_json::from_str(&result.output).unwrap()
    }

    #[tokio::test]
    async fn memory_manage_default_and_user_scope_write_user_layer() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryManageTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));
        let session = session();

        // Explicit scope=user.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "scope-user-test",
                "scope": "user",
                "content": "private fact for test-user"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        // Default (no scope) also lands in the user layer.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "scope-default-test",
                "content": "private fact for test-user"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        let user_mem = dir.path().join("users").join("test-user").join("memory");
        assert!(user_mem.join("scope-user-test.md").exists());
        assert!(user_mem.join("scope-default-test.md").exists());
        // Nothing leaked into the global agent layer.
        let global_mem = dir.path().join("memory");
        assert!(!global_mem.join("scope-user-test.md").exists());
        assert!(!global_mem.join("scope-default-test.md").exists());
    }

    #[tokio::test]
    async fn memory_manage_agent_scope_writes_global_layer() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryManageTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));
        let session = session();

        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "scope-agent-test",
                "scope": "agent",
                "content": "shared methodology: verify deliverables against the spec"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        let global_mem = dir.path().join("memory");
        assert!(global_mem.join("scope-agent-test.md").exists());
        let user_mem = dir.path().join("users").join("test-user").join("memory");
        assert!(!user_mem.join("scope-agent-test.md").exists());
    }

    #[tokio::test]
    async fn memory_manage_agent_scope_rejects_pii() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryManageTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));
        let session = session();

        let pii_cases: &[(&str, &str)] = &[
            ("routing key", "telegram:myclaw:6270938644 is the user's channel"),
            ("numeric id", "the user id is 6270938644"),
            ("email", "reach me at user@example.com"),
            ("phone", "call 13812345678"),
        ];
        for (label, content) in pii_cases {
            let out = manage(
                &tool,
                json!({
                    "action": "add",
                    "name": "pii-agent",
                    "scope": "agent",
                    "content": content
                }),
                &session,
            )
            .await;
            assert!(
                !out["success"].as_bool().unwrap_or(true),
                "{label} should be blocked by the PII guard"
            );
            let err = out["error"].as_str().unwrap_or_default();
            assert!(
                err.contains("de-identified"),
                "{label}: expected de-identification error, got: {err}"
            );
        }

        // The same user-identifying content is fine in the user scope.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "pii-user",
                "scope": "user",
                "content": "telegram:myclaw:6270938644 is the user's channel"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
    }

    #[tokio::test]
    async fn memory_manage_replace_is_scope_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryManageTool::new(dir.path().to_path_buf(), Arc::new(UserResolver::new()));
        let session = session();

        // Same name in both layers: user scope has one, agent scope has another.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "same-name",
                "scope": "user",
                "content": "user-layer content"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "same-name",
                "scope": "agent",
                "content": "agent-layer content"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        // replace without scope targets the user layer (default).
        let out = manage(
            &tool,
            json!({
                "action": "replace",
                "name": "same-name",
                "content": "user-layer updated"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let user_file =
            std::fs::read_to_string(dir.path().join("users/test-user/memory/same-name.md"))
                .unwrap();
        assert!(user_file.contains("user-layer updated"));

        // replace with scope=agent targets the agent layer and leaves the user file intact.
        let out = manage(
            &tool,
            json!({
                "action": "replace",
                "name": "same-name",
                "scope": "agent",
                "content": "agent-layer updated"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let agent_file = std::fs::read_to_string(dir.path().join("memory/same-name.md")).unwrap();
        assert!(agent_file.contains("agent-layer updated"));
        let user_file =
            std::fs::read_to_string(dir.path().join("users/test-user/memory/same-name.md"))
                .unwrap();
        assert!(user_file.contains("user-layer updated"));

        // Missing entry reports the scope in the error.
        let out = manage(
            &tool,
            json!({
                "action": "replace",
                "name": "no-such-memory",
                "scope": "agent",
                "content": "x"
            }),
            &session,
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));
        assert!(out["error"].as_str().unwrap().contains("agent scope"));
    }
}
