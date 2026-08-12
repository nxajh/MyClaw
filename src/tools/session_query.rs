//! Session query tool: list sessions and read messages by global ID.
//!
//! Used by the agent to look up provenance information (session IDs and
//! message IDs) when writing memory annotations.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::agents::session::Session;
use crate::agents::user_resolver::UserResolver;
use crate::providers::{Tool, ToolResult};
use crate::storage::SessionBackend;

pub struct SessionQueryTool {
    backend: Arc<dyn SessionBackend>,
    #[allow(dead_code)]
    resolver: Arc<UserResolver>,
}

impl SessionQueryTool {
    pub fn new(backend: Arc<dyn SessionBackend>, resolver: Arc<UserResolver>) -> Self {
        Self { backend, resolver }
    }
}

#[async_trait]
impl Tool for SessionQueryTool {
    fn name(&self) -> &str {
        "session_query"
    }

    fn description(&self) -> &str {
        "Query session history for provenance annotation. Two actions: \
         (1) action=\"list\" — list your sessions with IDs and message counts. \
         (2) action=\"messages\" — read recent messages from a session, \
         showing their global message IDs, roles, and content previews."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "messages"],
                    "description": "\"list\" to see your sessions, \"messages\" to read messages from one."
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID (required for action=\"messages\")."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 20 for messages, 50 for list).",
                    "default": 20
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N messages (for action=\"messages\", 0-based from newest). Default 0.",
                    "default": 0
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &Session,
    ) -> anyhow::Result<ToolResult> {
        let action = match args["action"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: json!({"success": false, "error": "'action' is required (\"list\" or \"messages\")."}).to_string(),
                    error: None,
                });
            }
        };

        match action {
            "list" => {
                let limit = args["limit"].as_u64().unwrap_or(50) as usize;
                let sessions = self.backend.list_sessions_for_owner(&session.owner);
                let entries: Vec<serde_json::Value> = sessions
                    .iter()
                    .take(limit)
                    .map(|s| {
                        json!({
                            "id": &s.id,
                            "name": s.display_name.as_deref().unwrap_or(""),
                            "messages": s.message_count,
                            "last_activity": s.last_activity.to_rfc3339(),
                        })
                    })
                    .collect();
                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "success": true,
                        "sessions": entries,
                        "current_session": session.id,
                    })
                    .to_string(),
                    error: None,
                })
            }
            "messages" => {
                let target_id = match args["session_id"].as_str() {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: json!({"success": false, "error": "'session_id' is required for action=\"messages\"."}).to_string(),
                            error: None,
                        });
                    }
                };

                // Security: verify the target session belongs to the same user.
                let sessions = self.backend.list_sessions_for_owner(&session.owner);
                if !sessions.iter().any(|s| s.id == target_id) {
                    return Ok(ToolResult {
                        success: false,
                        output: json!({
                            "success": false,
                            "error": "Session not found or not owned by you."
                        })
                        .to_string(),
                        error: None,
                    });
                }

                let limit = args["limit"].as_u64().unwrap_or(20) as usize;
                let offset = args["offset"].as_u64().unwrap_or(0) as usize;

                // Read messages from the backend.
                let rows = self.backend.load_incremental(target_id, 0);
                let total = rows.len();
                let start = total.saturating_sub(offset + limit);
                let end = total.saturating_sub(offset);
                let slice = if start < end {
                    &rows[start..end]
                } else {
                    &rows[..0]
                };

                let messages: Vec<serde_json::Value> = slice
                    .iter()
                    .map(|(id, msg)| {
                        let text = msg.text_content();
                        let preview: String = if text.chars().count() > 200 {
                            text.chars().take(200).collect()
                        } else {
                            text
                        };
                        json!({
                            "id": id,
                            "role": &msg.role,
                            "preview": preview,
                        })
                    })
                    .collect();

                Ok(ToolResult {
                    success: true,
                    output: json!({
                        "success": true,
                        "session_id": target_id,
                        "total_messages": total,
                        "messages": messages,
                    })
                    .to_string(),
                    error: None,
                })
            }
            other => Ok(ToolResult {
                success: false,
                output: json!({"success": false, "error": format!("Unknown action '{}'. Use \"list\" or \"messages\".", other)}).to_string(),
                error: None,
            }),
        }
    }
}
