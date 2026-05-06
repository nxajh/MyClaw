//! List Directory 工具 — 列出目录内容
//!
//! 比 glob_search 更直观，不需要 pattern。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use crate::providers::{Tool, ToolResult};

pub struct ListDirTool;

impl ListDirTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ListDirTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files and directories in a given path. \
         Returns file names, types (file/dir), and sizes. \
         Defaults to current working directory if no path is provided."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (default: current directory)"
                },
                "show_hidden": {
                    "type": "boolean",
                    "description": "Whether to show hidden files (default: false)"
                }
            }
        })
    }

    fn max_output_tokens(&self) -> usize {
        5_000
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path_str = args["path"].as_str().unwrap_or(".");
        let show_hidden = args["show_hidden"].as_bool().unwrap_or(false);

        let path = Path::new(path_str);
        if !path.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("path does not exist: {}", path_str)),
            });
        }
        if !path.is_dir() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("not a directory: {}", path_str)),
            });
        }

        let mut entries: Vec<Value> = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            // 跳过隐藏文件
            if !show_hidden && name.starts_with('.') {
                continue;
            }

            let file_type = entry.file_type()?;
            let metadata = entry.metadata().ok();

            entries.push(json!({
                "name": name,
                "type": if file_type.is_dir() { "dir" } else if file_type.is_symlink() { "symlink" } else { "file" },
                "size_bytes": metadata.map(|m| m.len()).unwrap_or(0),
            }));
        }

        // 目录在前，文件在后
        entries.sort_by(|a, b| {
            let a_is_dir = a["type"] == "dir";
            let b_is_dir = b["type"] == "dir";
            b_is_dir.cmp(&a_is_dir).then_with(|| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            })
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({
                "ok": true,
                "path": path_str,
                "entries": entries,
                "total": entries.len()
            }))?,
            error: None,
        })
    }
}
