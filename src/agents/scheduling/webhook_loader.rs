//! Webhook loader — webhook trigger view types + payload template rendering.
//!
//! Webhook jobs live in the unified jobs store (`{jobs_root}/{uuid}/meta.json`
//! with `kind = "webhook"`); `Scheduler::webhook_jobs()` projects them into
//! [`WebhookJobDef`] for the HTTP server. The legacy `webhooks/*.md`
//! file-loading path was removed with the unification.

/// Webhook job definition (server-facing view projected from a
/// `kind = "webhook"` JobEntry).
#[derive(Debug, Clone)]
pub struct WebhookJobDef {
    /// Owning job id — FQID `<ns>/job/<uuid>`; used for the `_job_{uuid}`
    /// session key.
    pub id: String,
    /// URL path, e.g. "/github/issues".
    pub path: String,
    /// HMAC secret 或 Bearer token.
    pub secret: Option<String>,
    /// 认证方式：hmac（默认）或 bearer.
    pub auth: WebhookAuth,
    /// 输出投递目标：last | none | channel name.
    pub target: String,
    /// Prompt 模板（body），可含 {{path.to.field}} 占位符.
    pub prompt_template: String,
}

/// Webhook 认证方式。
#[derive(Debug, Clone, PartialEq)]
pub enum WebhookAuth {
    /// HMAC-SHA256，验证 X-Hub-Signature-256 header.
    Hmac,
    /// Bearer token，验证 Authorization header.
    Bearer,
}

// ── Template rendering ────────────────────────────────────────────────────

/// 渲染模板，将 `{{path.to.field}}` 替换为 JSON payload 中的值。
///
/// - `{{issue.title}}` → 从 payload 中读取 `issue.title`
/// - `{{commits[0].message}}` → 支持数组索引
/// - 找不到的字段替换为空字符串
pub fn render_template(template: &str, payload: &serde_json::Value) -> String {
    let mut result = template.to_string();
    let mut start = 0;

    while let Some(open) = result[start..].find("{{") {
        let abs_open = start + open;
        let Some(close) = result[abs_open..].find("}}") else {
            break;
        };
        let abs_close = abs_open + close;

        let key = result[abs_open + 2..abs_close].trim();
        let replacement = match navigate_json_value(payload, key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            Some(serde_json::Value::Null) => String::new(),
            Some(other) => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
            None => String::new(),
        };

        let placeholder_len = abs_close + 2 - abs_open; // includes {{ and }}
        result.replace_range(abs_open..abs_open + placeholder_len, &replacement);
        // Move past the replacement to avoid infinite loops
        start = abs_open + replacement.len();
    }

    result
}

/// Navigate a JSON value by dot-separated path with array index support.
fn navigate_json_value<'a>(
    val: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = val;
    for segment in path.split('.') {
        if let Some(bracket) = segment.find('[') {
            let field = &segment[..bracket];
            if !field.is_empty() {
                current = current.get(field)?;
            }
            let rest = &segment[bracket..];
            for idx_str in rest.split(']').filter(|s| !s.is_empty()) {
                let idx: usize = idx_str.trim_start_matches('[').parse().ok()?;
                current = current.get(idx)?;
            }
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Template rendering tests ───────────────────────────────────────

    #[test]
    fn render_template_simple() {
        let template = "Hello {{name}}!";
        let payload = serde_json::json!({"name": "world"});
        assert_eq!(render_template(template, &payload), "Hello world!");
    }

    #[test]
    fn render_template_nested() {
        let template = "Issue: {{issue.title}} by {{issue.user.login}}";
        let payload = serde_json::json!({
            "issue": {
                "title": "Fix bug",
                "user": {"login": "alice"}
            }
        });
        assert_eq!(
            render_template(template, &payload),
            "Issue: Fix bug by alice"
        );
    }

    #[test]
    fn render_template_array_index() {
        let template = "First commit: {{commits[0].message}}";
        let payload = serde_json::json!({
            "commits": [{"message": "fix"}, {"message": "feat"}]
        });
        assert_eq!(render_template(template, &payload), "First commit: fix");
    }

    #[test]
    fn render_template_missing_field() {
        let template = "Hello {{name}}!";
        let payload = serde_json::json!({});
        assert_eq!(render_template(template, &payload), "Hello !");
    }

    #[test]
    fn render_template_multiple_same_field() {
        let template = "{{x}} and {{x}}";
        let payload = serde_json::json!({"x": "foo"});
        assert_eq!(render_template(template, &payload), "foo and foo");
    }

    #[test]
    fn render_template_no_placeholders() {
        let template = "No placeholders here.";
        let payload = serde_json::json!({});
        assert_eq!(render_template(template, &payload), "No placeholders here.");
    }

    #[test]
    fn render_template_number_and_bool() {
        let template = "Count: {{count}}, Active: {{active}}";
        let payload = serde_json::json!({"count": 42, "active": true});
        assert_eq!(
            render_template(template, &payload),
            "Count: 42, Active: true"
        );
    }

    #[test]
    fn render_template_unclosed_braces_ignored() {
        let template = "Hello {{name} not closed";
        let payload = serde_json::json!({"name": "world"});
        assert_eq!(
            render_template(template, &payload),
            "Hello {{name} not closed"
        );
    }
}
