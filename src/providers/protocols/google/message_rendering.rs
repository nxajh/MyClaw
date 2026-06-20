//! Google Generate Content API message rendering.
//!
//! Converts internal `ChatMessage` / `ChatRequest` into the JSON body expected
//! by the Google Gemini `generateContent` / `streamGenerateContent` endpoints.
//!
//! Reference: https://ai.google.dev/api/generate-content

use crate::providers::ChatRequest;
use base64::Engine as _;
use serde_json::json;

/// Build the full JSON request body for the Google Generate Content API.
///
/// Handles:
/// - Extracting system messages → top-level `systemInstruction`.
/// - Converting `ChatMessage` → `contents` (role: "user" / "model").
/// - Mapping tool results to `functionResponse` parts (role: "user").
/// - Mapping assistant `tool_calls` to `functionCall` parts (role: "model").
/// - Converting `ToolSpec` → `tools[].functionDeclarations`.
/// - Mapping `ThinkingConfig` → `generationConfig.thinkingConfig`.
pub fn build_google_body(req: &ChatRequest<'_>) -> serde_json::Value {
    // ── systemInstruction ────────────────────────────────────────────────
    let system_text: String = req
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .flat_map(|m| m.parts.iter().filter_map(|p| match p {
            crate::providers::ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        }))
        .collect::<Vec<_>>()
        .join("\n");

    // ── contents ─────────────────────────────────────────────────────────
    let contents: Vec<serde_json::Value> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .filter_map(render_content)
        .collect();

    // ── Assemble body ────────────────────────────────────────────────────
    let mut body = json!({ "contents": contents });

    // systemInstruction
    if !system_text.is_empty() {
        body["systemInstruction"] = json!({
            "parts": [{ "text": system_text }]
        });
    }

    // generationConfig
    let mut gen_config = serde_json::Map::new();
    if let Some(temp) = req.temperature {
        gen_config.insert("temperature".into(), json!(temp));
    }
    if let Some(max) = req.max_tokens {
        gen_config.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(ref stop) = req.stop {
        if !stop.is_empty() {
            gen_config.insert("stopSequences".into(), json!(stop));
        }
    }
    if let Some(seed) = req.seed {
        gen_config.insert("seed".into(), json!(seed));
    }
    // Thinking / reasoning config
    if let Some(ref thinking) = req.thinking {
        if thinking.enabled {
            let budget = match thinking.effort.as_deref() {
                Some("high") => 10_000u32,
                Some("low") => 1_024,
                _ => 5_120,
            };
            gen_config.insert(
                "thinkingConfig".into(),
                json!({
                    "includeThoughts": true,
                    "thinkingBudget": budget,
                }),
            );
        }
    }
    if !gen_config.is_empty() {
        body["generationConfig"] = serde_json::Value::Object(gen_config);
    }

    // tools → functionDeclarations
    if let Some(tools) = req.tools {
        let declarations: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                let mut decl = json!({
                    "name": t.name,
                    "description": t.description,
                });
                // Only include parameters when the schema is non-null.
                if !t.input_schema.is_null() {
                    // Google's functionDeclarations supports only a subset of
                    // JSON Schema. Strip unsupported fields recursively to
                    // avoid "Unknown name X: Cannot find field" 400 errors.
                    let mut schema = t.input_schema.clone();
                    sanitize_schema_for_google(&mut schema);
                    decl["parameters"] = schema;
                }
                decl
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }

    body
}

/// Recursively remove JSON Schema fields that Google's functionDeclarations
/// endpoint does not understand.
///
/// Google supports a subset of OpenAPI 3.0 schema. Known-unsupported fields
/// include `additionalProperties`, `$schema`, `$defs`, and `definitions`.
fn sanitize_schema_for_google(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    // Remove fields Google rejects.
    obj.remove("additionalProperties");
    obj.remove("$schema");
    obj.remove("$defs");
    obj.remove("definitions");
    // Google requires "required" to be paired with "type": "object".
    // Schema nodes like anyOf branches that carry "required" without an
    // explicit object type trigger 400 "required: only allowed for OBJECT type".
    if obj.contains_key("required") {
        let is_object = obj
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s == "object")
            .unwrap_or(false);
        if !is_object {
            obj.insert("type".to_string(), serde_json::json!("object"));
        }
    }
    // Recurse into nested objects/arrays.
    for (_, v) in obj.iter_mut() {
        match v {
            serde_json::Value::Object(_) => sanitize_schema_for_google(v),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    sanitize_schema_for_google(item);
                }
            }
            _ => {}
        }
    }
}

/// Render a single `ChatMessage` into a Google `Content` object.
///
/// Returns `None` for empty/unsupported messages.
fn render_content(msg: &crate::providers::ChatMessage) -> Option<serde_json::Value> {
    match msg.role.as_str() {
        // ── Tool result → user message with functionResponse parts ──────
        "tool" => {
            let name = msg.name.as_deref().unwrap_or("unknown");
            let text: String = msg
                .parts
                .iter()
                .filter_map(|p| match p {
                    crate::providers::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let mut response = serde_json::Map::new();
            if msg.is_error.unwrap_or(false) {
                response.insert("error".into(), json!(text));
            } else {
                response.insert("result".into(), json!(text));
            }
            Some(json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": name,
                        "response": response,
                    }
                }]
            }))
        }
        // ── Assistant → model message ──────────────────────────────────
        "assistant" => {
            let mut parts = Vec::new();

            // Text + Thinking parts
            for part in &msg.parts {
                match part {
                    crate::providers::ContentPart::Text { text } => {
                        if !text.is_empty() {
                            parts.push(json!({ "text": text }));
                        }
                    }
                    crate::providers::ContentPart::Thinking { thinking, .. } => {
                        if !thinking.is_empty() {
                            parts.push(json!({ "text": thinking, "thought": true }));
                        }
                    }
                    crate::providers::ContentPart::File { path, mime_type, .. } => {
                        // Inline images as base64; other files as text markers.
                        let modality =
                            crate::providers::media::modality_from_mime(mime_type.as_deref(), path);
                        if modality == crate::providers::media::FileModality::Image {
                            if let Some(part) = inline_file_as_part(path, mime_type.as_deref()) {
                                parts.push(part);
                            }
                        }
                        // Non-image files from assistant are rare; skip silently.
                    }
                }
            }

            // Tool calls → functionCall parts
            if let Some(ref tcs) = msg.tool_calls {
                for tc in tcs {
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                            tracing::warn!(
                                tool_id = %tc.id,
                                name = %tc.name,
                                error = %e,
                                "tool arguments JSON parse failed, using empty object"
                            );
                            serde_json::Value::Object(Default::default())
                        });
                    parts.push(json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": args,
                        }
                    }));
                }
            }

            if parts.is_empty() {
                return None;
            }
            Some(json!({
                "role": "model",
                "parts": parts,
            }))
        }
        // ── User → user message ────────────────────────────────────────
        _ => {
            let mut parts = Vec::new();
            for part in &msg.parts {
                match part {
                    crate::providers::ContentPart::Text { text } => {
                        if !text.is_empty() {
                            parts.push(json!({ "text": text }));
                        }
                    }
                    crate::providers::ContentPart::File { path, mime_type, .. } => {
                        if let Some(part) = inline_file_as_part(path, mime_type.as_deref()) {
                            parts.push(part);
                        } else {
                            // Fallback: text marker
                            parts.push(json!({
                                "text": crate::providers::media::marker_for_file(path, mime_type.as_deref())
                            }));
                        }
                    }
                    crate::providers::ContentPart::Thinking { .. } => {
                        // Thinking blocks from history are not sent to Google.
                    }
                }
            }
            if parts.is_empty() {
                return None;
            }
            Some(json!({
                "role": "user",
                "parts": parts,
            }))
        }
    }
}

/// Inline a file as a Google `inlineData` part (base64-encoded).
/// Returns `None` if the file cannot be read.
fn inline_file_as_part(
    path: &str,
    mime_type: Option<&str>,
) -> Option<serde_json::Value> {
    let abs = crate::providers::media::resolve_path(path);
    let bytes = std::fs::read(&abs).ok()?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let mime = mime_type
        .map(|s| s.to_string())
        .or_else(|| {
            crate::providers::media::infer_image_mime(path).map(|s| s.to_string())
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Some(json!({
        "inlineData": {
            "mimeType": mime,
            "data": data,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_unsupported_fields() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "a": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            "additionalProperties": true,
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {},
        });
        sanitize_schema_for_google(&mut schema);
        let s = schema.to_string();
        assert!(!s.contains("additionalProperties"));
        assert!(!s.contains("$schema"));
        assert!(!s.contains("$defs"));
    }

    #[test]
    fn sanitize_recurses_into_arrays() {
        let mut schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {},
                "additionalProperties": true
            }
        });
        sanitize_schema_for_google(&mut schema);
        assert!(!schema.to_string().contains("additionalProperties"));
    }

    #[test]
    fn sanitize_preserves_supported_fields() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "test"}
            },
            "required": ["query"]
        });
        sanitize_schema_for_google(&mut schema);
        assert_eq!(schema["properties"]["query"]["description"], "test");
        assert_eq!(schema["required"][0], "query");
    }

    #[test]
    fn sanitize_anyof_branch_required_gets_type_object() {
        // Reproduces the send_message tool schema that triggered
        // "required: only allowed for OBJECT type" (400) on Google.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "files": {"type": "array", "items": {"type": "string"}}
            },
            "anyOf": [
                {"required": ["text"]},
                {"required": ["files"]}
            ]
        });
        sanitize_schema_for_google(&mut schema);
        for branch in schema["anyOf"].as_array().unwrap() {
            assert_eq!(
                branch["type"].as_str().unwrap(),
                "object",
                "anyOf branch with required must get type: object"
            );
        }
    }
}
