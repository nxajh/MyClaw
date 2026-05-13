//! Webhook Loader — 从 workspace/webhooks/ 目录加载 webhook 路由文件。
//!
//! 每个 `.md` 文件定义一个 webhook job，格式：
//! ```markdown
//! ---
//! path: /github/issues
//! secret: "gh-wh-secret"
//! auth: hmac
//! target: last
//! ---
//!
//! 分析并处理新的 GitHub issue。
//!
//! 仓库: {{repository.full_name}}
//! 标题: {{issue.title}}
//! ```

use std::path::Path;

use tracing::{info, warn};

use crate::str_utils::{extract_yaml_string, parse_front_matter};

/// 从 `webhooks/*.md` 加载的 webhook job 定义。
#[derive(Debug, Clone)]
pub struct WebhookJobDef {
    /// URL path，e.g. "/github/issues".
    pub path: String,
    /// HMAC secret 或 Bearer token.
    pub secret: Option<String>,
    /// 认证方式：hmac（默认）或 bearer.
    pub auth: WebhookAuth,
    /// 输出投递目标：last | none | channel name.
    pub target: String,
    /// Prompt 模板（body），可含 {{path.to.field}} 占位符.
    pub prompt_template: String,
    /// 源文件路径.
    pub source_path: std::path::PathBuf,
}

/// Webhook 认证方式。
#[derive(Debug, Clone, PartialEq)]
pub enum WebhookAuth {
    /// HMAC-SHA256，验证 X-Hub-Signature-256 header.
    Hmac,
    /// Bearer token，验证 Authorization header.
    Bearer,
}

/// 解析单个 webhook 文件。
pub fn parse_webhook_file(path: &Path) -> anyhow::Result<WebhookJobDef> {
    let content = std::fs::read_to_string(path)?;
    let (front_matter, body) = parse_front_matter(&content);

    let raw_path = extract_yaml_string(&front_matter, "path")
        .ok_or_else(|| anyhow::anyhow!("missing 'path' in front matter of {}", path.display()))?;

    let path_normalized = if raw_path.starts_with('/') {
        raw_path
    } else {
        format!("/{}", raw_path)
    };

    let secret = extract_yaml_string(&front_matter, "secret");

    let auth = match extract_yaml_string(&front_matter, "auth")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "bearer" => WebhookAuth::Bearer,
        _ => WebhookAuth::Hmac,
    };

    let target = extract_yaml_string(&front_matter, "target")
        .unwrap_or_else(|| "last".to_string());

    let prompt_template = body.trim().to_string();

    if prompt_template.is_empty() {
        anyhow::bail!("empty prompt body in {}", path.display());
    }

    Ok(WebhookJobDef {
        path: path_normalized,
        secret,
        auth,
        target,
        prompt_template,
        source_path: path.to_path_buf(),
    })
}

/// 扫描 webhooks 目录，加载所有 `.md` 文件。
pub fn load_webhook_jobs(webhooks_dir: &Path) -> Vec<WebhookJobDef> {
    let mut jobs = Vec::new();

    if !webhooks_dir.exists() {
        return jobs;
    }

    let entries = match std::fs::read_dir(webhooks_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %webhooks_dir.display(), error = %e, "failed to read webhooks directory");
            return jobs;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }

        match parse_webhook_file(&path) {
            Ok(job) => {
                info!(
                    path = %job.path,
                    auth = ?job.auth,
                    target = %job.target,
                    file = %path.file_name().unwrap_or_default().to_string_lossy(),
                    "webhook loaded"
                );
                jobs.push(job);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse webhook file");
            }
        }
    }

    jobs
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
fn navigate_json_value<'a>(val: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
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

    #[test]
    fn parse_webhook_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-issues.md");
        std::fs::write(
            &path,
            "---\npath: /github/issues\nsecret: \"gh-secret\"\nauth: hmac\ntarget: last\n---\n\n分析 issue: {{issue.title}}\n",
        )
        .unwrap();

        let job = parse_webhook_file(&path).unwrap();
        assert_eq!(job.path, "/github/issues");
        assert_eq!(job.secret, Some("gh-secret".to_string()));
        assert_eq!(job.auth, WebhookAuth::Hmac);
        assert_eq!(job.target, "last");
        assert!(job.prompt_template.contains("{{issue.title}}"));
    }

    #[test]
    fn parse_webhook_file_bearer_auth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("internal.md");
        std::fs::write(
            &path,
            "---\npath: /internal\nsecret: \"my-token\"\nauth: bearer\n---\n\n执行任务。\n",
        )
        .unwrap();

        let job = parse_webhook_file(&path).unwrap();
        assert_eq!(job.auth, WebhookAuth::Bearer);
    }

    #[test]
    fn parse_webhook_file_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("public.md");
        std::fs::write(
            &path,
            "---\npath: /public\n---\n\n公开端点。\n",
        )
        .unwrap();

        let job = parse_webhook_file(&path).unwrap();
        assert_eq!(job.secret, None);
    }

    #[test]
    fn parse_webhook_file_path_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        std::fs::write(
            &path,
            "---\npath: github/issues\n---\n\nPrompt.\n",
        )
        .unwrap();

        let job = parse_webhook_file(&path).unwrap();
        assert_eq!(job.path, "/github/issues");
    }

    #[test]
    fn parse_webhook_file_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.md");
        std::fs::write(&path, "---\ntarget: last\n---\nDo something.").unwrap();

        assert!(parse_webhook_file(&path).is_err());
    }

    #[test]
    fn parse_webhook_file_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.md");
        std::fs::write(&path, "---\npath: /test\n---\n").unwrap();

        assert!(parse_webhook_file(&path).is_err());
    }

    #[test]
    fn load_webhook_jobs_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let wh_dir = dir.path().join("webhooks");
        std::fs::create_dir_all(&wh_dir).unwrap();

        std::fs::write(
            wh_dir.join("github.md"),
            "---\npath: /github\nsecret: \"s1\"\n---\nHandle GitHub event.",
        )
        .unwrap();
        std::fs::write(
            wh_dir.join("stripe.md"),
            "---\npath: /stripe\nsecret: \"s2\"\nauth: bearer\n---\nHandle Stripe event.",
        )
        .unwrap();
        std::fs::write(wh_dir.join("notes.txt"), "not a webhook").unwrap();

        let jobs = load_webhook_jobs(&wh_dir);
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn load_webhook_jobs_missing_dir() {
        let jobs = load_webhook_jobs(Path::new("/nonexistent"));
        assert!(jobs.is_empty());
    }

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
        assert_eq!(render_template(template, &payload), "Issue: Fix bug by alice");
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
        assert_eq!(render_template(template, &payload), "Count: 42, Active: true");
    }

    #[test]
    fn render_template_unclosed_braces_ignored() {
        let template = "Hello {{name} not closed";
        let payload = serde_json::json!({"name": "world"});
        assert_eq!(render_template(template, &payload), "Hello {{name} not closed");
    }
}
