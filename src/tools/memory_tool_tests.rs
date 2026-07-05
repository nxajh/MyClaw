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
}
