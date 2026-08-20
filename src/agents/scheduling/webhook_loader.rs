//! Webhook loader — webhook trigger view types + payload template rendering.
//!
//! Webhook channels live on unified jobs (`{jobs_root}/{uuid}/meta.json` with
//! an optional `webhook` object — design §3.4 orthogonal trigger model);
//! `Scheduler::webhook_jobs()` projects them into [`WebhookJobDef`] for the
//! HTTP server. The legacy `webhooks/*.md` file-loading path was removed with
//! the unification.

/// Webhook job definition (server-facing view projected from a JobEntry's
/// `webhook` channel). Route derives from the job name: `POST /hooks/{route}`.
#[derive(Debug, Clone)]
pub struct WebhookJobDef {
    /// Owning job id — FQID `<ns>/job/<uuid>`; used for the `_job_{uuid}`
    /// session key.
    pub id: String,
    /// Route segment (the job's name, URL-safe slug): full route is
    /// `POST /hooks/{route}`.
    pub route: String,
    /// HMAC secret or Bearer token (required — validated at projection).
    pub secret: String,
    /// Auth method: hmac (default) or bearer.
    pub auth: WebhookAuth,
    /// Output delivery target: last | none | channel name (from the job).
    pub target: String,
    /// Prompt template (the job's prompt field), with `{{a.b.c}}`
    /// placeholders rendered from the payload.
    pub prompt_template: String,
    /// Event-type whitelist (optional; non-matching events are ignored).
    pub events: Option<Vec<String>>,
    /// Condition filters (AND semantics, optional).
    pub filters: Option<Vec<crate::agents::scheduling::scheduler::WebhookFilter>>,
    /// Disable the automatic full-payload appendix.
    pub payload_off: bool,
}

/// Route-segment validity: lowercase URL-safe slug `[a-z0-9-]`, 1-64 chars.
/// Names are user-facing, so validation is strict (no `_`, no unicode).
pub fn is_route_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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

    // ── Route slug validation tests ────────────────────────────────────

    #[test]
    fn route_slug_accepts_lowercase_slugs() {
        assert!(is_route_slug("github-issues"));
        assert!(is_route_slug("r2d2"));
        assert!(is_route_slug("a"));
    }

    #[test]
    fn route_slug_rejects_bad_names() {
        assert!(!is_route_slug("")); // empty
        assert!(!is_route_slug("GitHub")); // uppercase
        assert!(!is_route_slug("under_score")); // underscore
        assert!(!is_route_slug("中文")); // unicode
        assert!(!is_route_slug("has/slash"));
        assert!(!is_route_slug(&"x".repeat(65))); // too long
    }
}
