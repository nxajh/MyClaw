//! Memory tool search behavior tests (P4: layered storage).

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::{Value, json};

    use crate::api::tool::ToolContext;
    use crate::identity::user_profile::UserResolver;
    use crate::providers::Tool;
    use crate::tools::{MemoryListTool, MemoryManageTool, MemorySearchTool};

    /// Registered FQID owners (valid uuidv7-shaped uuids).
    const ALICE_UID: &str = "myclaw/u/018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e7f";
    const BOB_UID: &str = "myclaw/u/018f6b2a-4c3d-7b2e-9f01-3a2b4c5d6e80";
    /// Legacy routing-key identity — NOT FQID, must be gated on user-scope writes.
    const LEGACY_KEY: &str = "client:default:web-1";

    /// `{tmp}/base` with an (empty) agent-layer memory root; returns
    /// (base_dir, memory_root). User layers live under {base}/users/.
    fn layout(dir: &std::path::Path) -> (PathBuf, PathBuf) {
        let base = dir.to_path_buf();
        let memory_root = base.join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();
        (base, memory_root)
    }

    fn session() -> ToolContext {
        ToolContext {
            owner: ALICE_UID.to_string(),
            session_id: "test-session".to_string(),
            agent_name: "main".to_string(),
            ..Default::default()
        }
    }

    fn session_for(owner: &str) -> ToolContext {
        ToolContext {
            owner: owner.to_string(),
            session_id: format!("session-{}", owner.rsplit('/').next().unwrap_or(owner)),
            agent_name: "main".to_string(),
            ..Default::default()
        }
    }

    fn user_layer_dir(base: &std::path::Path, uid: &str) -> PathBuf {
        crate::memory::user_memory_dir(&base.join("memory"), uid)
    }

    fn write_memory(
        dir: &std::path::Path,
        name: &str,
        mem_type: &str,
        description: &str,
        tags: &str,
        content: &str,
    ) {
        // dir IS the agent-layer memory root (files without scope frontmatter
        // default to the agent layer).
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
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
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "arm-host",
            "project",
            "ARM host SSH port 31822",
            "arm, ssh",
            "oci-arm-1.jinl.in uses SSH port 31822.",
        );
        let tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));

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
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "arm-host",
            "project",
            "ARM host SSH port 31822",
            "arm, ssh",
            "oci-arm-1.jinl.in uses SSH port 31822.",
        );
        let tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "31822", "memory_type": "user"})).await;
        assert_eq!(output["count"], 0);
    }

    #[tokio::test]
    async fn memory_search_broad_query_uses_or_scoring() {
        let dir = tempfile::tempdir().unwrap();
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "n8n_daily_workflow_fix",
            "project",
            "n8n RSS AI workflow on ARM host",
            "n8n, arm, workflow, ssh-port-31822",
            "域名：oci-arm-1.jinl.in，SSH 端口 31822，用户 ubuntu。",
        );
        let tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));

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
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "client-channel",
            "project",
            "WebSocket client transport",
            "websocket",
            "WebSocket-based unified client transport for Web UI.",
        );
        write_memory(
            &root,
            "ssh-host",
            "project",
            "SSH port 31822",
            "ssh",
            "SSH port 31822 is exposed on the ARM host.",
        );
        let tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "ssh port"})).await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "ssh-host");
    }

    #[tokio::test]
    async fn memory_search_ascii_boundary_allows_hyphenated_tags() {
        let dir = tempfile::tempdir().unwrap();
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "arm-host",
            "project",
            "ARM host",
            "ssh-port-31822",
            "ARM host connection details.",
        );
        let tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "port"})).await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "arm-host");
    }

    #[tokio::test]
    async fn memory_search_non_ascii_tokens_still_use_substring_match() {
        let dir = tempfile::tempdir().unwrap();
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "openlist",
            "project",
            "OpenList 百度网盘服务",
            "openlist",
            "百度网盘管理服务部署在 ARM 主机。",
        );
        let tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));

        let output = search(&tool, json!({"query": "网盘"})).await;
        assert_eq!(output["count"], 1);
        assert_eq!(output["results"][0]["name"], "openlist");
    }

    #[tokio::test]
    async fn memory_list_empty_type_lists_all_types() {
        let dir = tempfile::tempdir().unwrap();
        let (_base, root) = layout(dir.path());
        write_memory(
            &root,
            "project-memory",
            "project",
            "Project memory",
            "project",
            "Project details.",
        );
        let tool = MemoryListTool::new(root.clone(), Arc::new(UserResolver::new()));

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
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );
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

        let search_tool = MemorySearchTool::new(root.clone(), Arc::new(UserResolver::new()));
        let output = search(
            &search_tool,
            json!({"query": "updated", "memory_type": "project"}),
        )
        .await;
        assert_eq!(output["count"], 1);
    }

    // ── two-tier memory scope tests (P0) ──────────────────────────────────

    async fn manage(tool: &MemoryManageTool, args: Value, ctx: &ToolContext) -> Value {
        let result = tool.execute(args, ctx).await.unwrap();
        serde_json::from_str(&result.output).unwrap()
    }

    #[tokio::test]
    async fn memory_manage_default_and_user_scope_write_user_layer() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );
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

        // P4: user-scope files land in {base}/users/{uuid}/memory with
        // scope=user + user_id in frontmatter — NOT in the agent-layer root.
        let alice_dir = user_layer_dir(&base, ALICE_UID);
        let user_file = std::fs::read_to_string(alice_dir.join("scope-user-test.md")).unwrap();
        assert!(user_file.contains("scope: user"));
        assert!(user_file.contains("user_id: \"myclaw/u/"));
        let default_file =
            std::fs::read_to_string(alice_dir.join("scope-default-test.md")).unwrap();
        assert!(default_file.contains("scope: user"));
        assert!(!default_file.contains("scope: agent"));
        assert!(!root.join("scope-user-test.md").exists());
    }

    #[tokio::test]
    async fn memory_manage_agent_scope_writes_global_layer() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );
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

        let agent_file = std::fs::read_to_string(root.join("scope-agent-test.md")).unwrap();
        assert!(agent_file.contains("scope: agent"));
        assert!(!agent_file.contains("user_id"));
    }

    #[tokio::test]
    async fn memory_manage_agent_scope_rejects_pii() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );
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
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );
        let session = session();
        let alice_dir = user_layer_dir(&base, ALICE_UID);

        // P4: layers are independent — a user-scope add followed by an
        // agent-scope add of the SAME name succeeds (with a shadow warning).
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "scoped-replace",
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
                "name": "scoped-replace",
                "scope": "agent",
                "content": "agent-layer content"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        assert!(root.join("scoped-replace.md").exists());
        assert!(alice_dir.join("scoped-replace.md").exists());

        // A second add to the SAME layer with an existing name is rejected.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "scoped-replace",
                "scope": "agent",
                "content": "duplicate"
            }),
            &session,
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));

        // replace without scope targets the user layer (default) and keeps
        // the ownership frontmatter — the agent twin stays untouched.
        let out = manage(
            &tool,
            json!({
                "action": "replace",
                "name": "scoped-replace",
                "content": "user-layer updated"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let user_file = std::fs::read_to_string(alice_dir.join("scoped-replace.md")).unwrap();
        assert!(user_file.contains("user-layer updated"));
        assert!(user_file.contains("scope: user"));
        let agent_twin = std::fs::read_to_string(root.join("scoped-replace.md")).unwrap();
        assert!(agent_twin.contains("agent-layer content"));

        // A distinct agent-scope entry replace works and stays agent-owned.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "agent-owned",
                "scope": "agent",
                "content": "agent-layer content"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let out = manage(
            &tool,
            json!({
                "action": "replace",
                "name": "agent-owned",
                "scope": "agent",
                "content": "agent-layer updated"
            }),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let agent_file = std::fs::read_to_string(root.join("agent-owned.md")).unwrap();
        assert!(agent_file.contains("agent-layer updated"));
        assert!(agent_file.contains("scope: agent"));

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

    // ── P4 layered-scope acceptance ───────────────────────────────────────

    #[tokio::test]
    async fn fqid_gate_rejects_routing_key_user_scope_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );

        // Legacy routing key (contains ':') → rejected with a /link hint.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "legacy-identity",
                "scope": "user",
                "content": "private fact"
            }),
            &session_for(LEGACY_KEY),
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("/link"), "error must point at /link: {err}");

        // Plain non-FQID ids are gated too.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "plain-name",
                "scope": "user",
                "content": "private fact"
            }),
            &session_for("alice"),
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));

        // A registered FQID passes and lands in the user layer.
        let out = manage(
            &tool,
            json!({
                "action": "add",
                "name": "fqid-ok",
                "scope": "user",
                "content": "private fact"
            }),
            &session_for(ALICE_UID),
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        assert!(user_layer_dir(&base, ALICE_UID).join("fqid-ok.md").exists());
    }

    #[tokio::test]
    async fn user_layer_add_shadows_agent_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let resolver = Arc::new(UserResolver::new());
        let manage_tool =
            MemoryManageTool::new(root.clone(), base.clone(), Arc::clone(&resolver));
        let list_tool = MemoryListTool::new(root.clone(), Arc::clone(&resolver));
        let session = session();

        let out = manage(
            &manage_tool,
            json!({"action": "add", "name": "dup-name", "scope": "agent", "content": "agent version"}),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        // User-layer add with the same name: allowed, but warned.
        let out = manage(
            &manage_tool,
            json!({"action": "add", "name": "dup-name", "content": "user version"}),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        let warnings = out["warnings"].as_array().cloned().unwrap_or_default();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap_or("").contains("shadows")),
            "expected a shadowing warning, got: {warnings:?}"
        );

        // Merged read view: the user-layer version wins.
        let result = list_tool.execute(json!({}), &session).await.unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        let entry = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "dup-name")
            .unwrap();
        assert_eq!(entry["description"], "user version");
    }

    #[tokio::test]
    async fn replace_default_scope_hints_at_agent_layer() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let tool = MemoryManageTool::new(
            root.clone(),
            base.clone(),
            Arc::new(UserResolver::new()),
        );
        let session = session();

        let out = manage(
            &tool,
            json!({"action": "add", "name": "agent-hint", "scope": "agent", "content": "shared"}),
            &session,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        // Default (user) scope misses → error must name the agent layer and
        // require an explicit scope, not silently mutate the shared layer.
        let out = manage(
            &tool,
            json!({"action": "replace", "name": "agent-hint", "content": "hijack"}),
            &session,
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("agent layer"), "{err}");
        assert!(err.contains("scope='agent'"), "{err}");
        // The shared-layer file was not modified.
        assert!(std::fs::read_to_string(root.join("agent-hint.md"))
            .unwrap()
            .contains("shared"));
    }

    #[tokio::test]
    async fn transition_fallback_single_pool_user_entries_stay_readable() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let resolver = Arc::new(UserResolver::new());
        let list_tool = MemoryListTool::new(root.clone(), Arc::clone(&resolver));
        let manage_tool =
            MemoryManageTool::new(root.clone(), base.clone(), Arc::clone(&resolver));

        // Pre-migration single-pool entry: physically in the agent dir,
        // frontmatter says user + owner alice.
        std::fs::write(
            root.join("legacy-pool.md"),
            format!(
                "---\nname: legacy-pool\nscope: user\nuser_id: {}\ntype: project\ninject: search\ndescription: legacy pooled entry\ncreated_at: 2026-08-01\n---\n\nlegacy body",
                ALICE_UID
            ),
        )
        .unwrap();

        // Alice still reads it (merged view)…
        let result = list_tool.execute(json!({}), &session_for(ALICE_UID)).await.unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "legacy-pool"));

        // …and replace keeps editing it in place (still readable after).
        let out = manage(
            &manage_tool,
            json!({"action": "replace", "name": "legacy-pool", "content": "legacy updated"}),
            &session_for(ALICE_UID),
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        assert!(std::fs::read_to_string(root.join("legacy-pool.md"))
            .unwrap()
            .contains("legacy updated"));

        // Bob does not see alice's legacy entry.
        let result = list_tool.execute(json!({}), &session_for(BOB_UID)).await.unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert!(!output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "legacy-pool"));
    }

    #[tokio::test]
    async fn memory_isolation_user_sees_own_and_agent_only() {
        let dir = tempfile::tempdir().unwrap();
        let (base, root) = layout(dir.path());
        let resolver = Arc::new(UserResolver::new());
        let manage_tool =
            MemoryManageTool::new(root.clone(), base.clone(), Arc::clone(&resolver));
        let list_tool = MemoryListTool::new(root.clone(), Arc::clone(&resolver));
        let search_tool = MemorySearchTool::new(root.clone(), Arc::clone(&resolver));

        let alice = session_for(ALICE_UID);
        let bob = session_for(BOB_UID);

        // Agent-layer entry.
        let out = manage(
            &manage_tool,
            json!({"action": "add", "name": "agent-fact", "scope": "agent", "content": "shared methodology"}),
            &alice,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        // Alice's private entry.
        let out = manage(
            &manage_tool,
            json!({"action": "add", "name": "alice-fact", "content": "alice private"}),
            &alice,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));
        // Bob's private entry.
        let out = manage(
            &manage_tool,
            json!({"action": "add", "name": "bob-fact", "content": "bob private"}),
            &bob,
        )
        .await;
        assert!(out["success"].as_bool().unwrap_or(false));

        // Alice lists: agent + own user layer, NOT bob's.
        let result = list_tool.execute(json!({}), &alice).await.unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        let names: Vec<&str> = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"agent-fact"));
        assert!(names.contains(&"alice-fact"));
        assert!(!names.contains(&"bob-fact"));
        assert_eq!(names.len(), 2);

        // Bob lists: agent + own only.
        let result = list_tool.execute(json!({}), &bob).await.unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        let names: Vec<&str> = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"agent-fact"));
        assert!(names.contains(&"bob-fact"));
        assert!(!names.contains(&"alice-fact"));
        assert_eq!(names.len(), 2);

        // Alice cannot search bob's private entry ("bob" is unique to it;
        // partial-token matches against her own entries stay allowed).
        let result = search_tool
            .execute(json!({"query": "bob"}), &alice)
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["count"], 0);

        // Bob cannot replace alice's entry via user scope.
        let out = manage(
            &manage_tool,
            json!({"action": "replace", "name": "alice-fact", "content": "hijack"}),
            &bob,
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));
        // …nor remove it.
        let out = manage(
            &manage_tool,
            json!({"action": "remove", "name": "alice-fact", "confirm": true}),
            &bob,
        )
        .await;
        assert!(!out["success"].as_bool().unwrap_or(true));
        assert!(user_layer_dir(&base, ALICE_UID)
            .join("alice-fact.md")
            .exists());
    }
}
