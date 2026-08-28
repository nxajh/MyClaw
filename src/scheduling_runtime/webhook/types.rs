//! Webhook 类型与数据形态（P2 自 webhook.rs 拆出，纯移动）：
//! `impl WebhookDef` / `WebhookContext` / `WebhookJobDef` / `WebhookAuth`、
//! 模板渲染（`render_template` / `navigate_json_value`）、安全栈
//! （`WebhookGuard` / `InflightGuard` + 限流/并发/幂等/体积常量）、
//! `extract_event_type` / `filter_matches`，以及 `is_route_slug` re-export
//! （#151 Phase 8+ 已下沉 scheduling_types::job_types，保持既有路径）。

use super::*;
impl WebhookDef {
    pub fn auth_kind(&self) -> WebhookAuth {
        match self.auth.as_str() {
            "bearer" => WebhookAuth::Bearer,
            _ => WebhookAuth::Hmac,
        }
    }
}

// ── Webhook app state ──────────────────────────────────────────────────────

/// Axum app state for the webhook server. Holds the orchestrator callback
/// hook ([`OrchestratorHook`] — dependency-inverted #151 Phase 3d; previously
/// a direct `Arc<OrchestratorCtx>`), the webhook-specific timezone for cron
/// parsing, and the live scheduler handle.
pub struct WebhookContext {
    /// Orchestrator callbacks (run scheduled turn / channel lookup),
    /// implemented by agents, wired in by the daemon.
    pub hook: Arc<dyn OrchestratorHook>,
    /// Timezone string used for cron evaluation in the webhook server.
    pub timezone: String,
    /// Live scheduler handle — routes are projected per request from the
    /// unified jobs store, so webhook jobs created/updated/removed after
    /// startup are picked up without a daemon restart.
    pub scheduler: SharedScheduler,
}

// ── Webhook channel view + template rendering (migrated from the removed
// webhook_loader.rs per §4 checklist) ───────────────────────────────────────

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
    /// Output delivery configuration (from the job).
    pub delivery: DeliveryConfig,
    /// Prompt template (the job's prompt field), with `{{a.b.c}}`
    /// placeholders rendered from the payload.
    pub prompt_template: String,
    /// Event-type whitelist (optional; non-matching events are ignored).
    pub events: Option<Vec<String>>,
    /// Condition filters (AND semantics, optional).
    pub filters: Option<Vec<WebhookFilter>>,
    /// Disable the automatic full-payload appendix.
    pub payload_off: bool,
}

/// Webhook auth method.
#[derive(Debug, Clone, PartialEq)]
pub enum WebhookAuth {
    /// HMAC-SHA256 via the X-Hub-Signature-256 header.
    Hmac,
    /// Bearer token via the Authorization header.
    Bearer,
}

/// Render a prompt template, replacing `{{path.to.field}}` placeholders with
/// values from the JSON payload.
///
/// - `{{issue.title}}` → reads `issue.title` from the payload
/// - `{{commits[0].message}}` → array indexing supported
/// - missing fields render as an empty string
// #151 Phase 8+：is_route_slug 已下沉 scheduling_types::job_types（L3 工具要用），此处 re-export 保持既有路径。
pub use crate::scheduling_types::job_types::is_route_slug;

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

// ── Webhook safety stack (§3.4.1) ──────────────────────────────────────────

/// Per-route + per-IP request guard: rate limit, in-flight cap, delivery-id
/// idempotency cache. Cheap checks first; body-size caps live in
/// `collect_body_capped`.
#[derive(Default)]
pub struct WebhookGuard {
    /// (route, ip) → request timestamps within the sliding 60s window.
    rate: std::sync::Mutex<std::collections::HashMap<(String, String), std::collections::VecDeque<std::time::Instant>>>,
    /// route → in-flight request count (cap 8 per route).
    inflight: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// delivery id → first-seen instant (dedupe window 1h).
    deliveries: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// RAII in-flight slot release.
pub struct InflightGuard {
    route: String,
    guard: Arc<WebhookGuard>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.guard.inflight.lock() {
            if let Some(c) = m.get_mut(&self.route) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&self.route);
                }
            }
        }
    }
}

const WEBHOOK_RATE_MAX: usize = 120;
const WEBHOOK_RATE_WINDOW_SECS: u64 = 60;
const WEBHOOK_CONCURRENCY_MAX: usize = 8;
const WEBHOOK_DELIVERY_TTL_SECS: u64 = 3600;
/// Hard body cap for custom webhook routes.
pub const WEBHOOK_BODY_LIMIT: usize = 256 * 1024;
/// Body read timeout.
pub const WEBHOOK_BODY_TIMEOUT_SECS: u64 = 15;
/// Allowed clock skew for V2 timestamped signatures.
pub(super) const WEBHOOK_V2_MAX_SKEW_SECS: i64 = 300;

impl WebhookGuard {
    /// Sliding-window rate check: ≤120 requests / 60s per (route, ip).
    pub fn check_rate(&self, route: &str, ip: &str) -> bool {
        let now = std::time::Instant::now();
        let Ok(mut m) = self.rate.lock() else { return true };
        let key = (route.to_string(), ip.to_string());
        let win = m.entry(key).or_default();
        let cutoff = now - std::time::Duration::from_secs(WEBHOOK_RATE_WINDOW_SECS);
        while win.front().is_some_and(|t| *t < cutoff) {
            win.pop_front();
        }
        if win.len() >= WEBHOOK_RATE_MAX {
            return false;
        }
        win.push_back(now);
        true
    }

    /// Acquire an in-flight slot; None when the route already has 8 running.
    pub fn acquire(&self, route: &str, guard: Arc<WebhookGuard>) -> Option<InflightGuard> {
        let Ok(mut m) = self.inflight.lock() else { return None };
        let c = m.entry(route.to_string()).or_insert(0);
        if *c >= WEBHOOK_CONCURRENCY_MAX {
            return None;
        }
        *c += 1;
        Some(InflightGuard { route: route.to_string(), guard })
    }

    /// Delivery-id idempotency: true when this id is seen for the first time
    /// within the 1h TTL (and is now recorded); false = duplicate.
    pub fn check_delivery(&self, delivery_id: &str) -> bool {
        let now = std::time::Instant::now();
        let Ok(mut m) = self.deliveries.lock() else { return true };
        m.retain(|_, t| now.duration_since(*t).as_secs() < WEBHOOK_DELIVERY_TTL_SECS);
        if m.contains_key(delivery_id) {
            return false;
        }
        m.insert(delivery_id.to_string(), now);
        true
    }
}

// ── Auth helpers ───────────────────────────────────────────────────────────

/// Extract the event type: header fallback chain first (X-GitHub-Event →
/// X-GitLab-Event, captured before body consumption), then payload fields
/// (`event_type` → `type`) — hermes 5-level chain, §3.4.1.
pub(super) fn extract_event_type(event_header: Option<&str>, payload: &serde_json::Value) -> Option<String> {
    if let Some(h) = event_header.filter(|h| !h.is_empty()) {
        return Some(h.to_string());
    }
    payload
        .get("event_type")
        .or_else(|| payload.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Evaluate a single webhook filter condition against the payload.
/// AND semantics across the filter list; `not` negates the match.
pub(super) fn filter_matches(
    f: &WebhookFilter,
    payload: &serde_json::Value,
) -> bool {
    let mut cur = payload;
    for seg in f.field.split('.') {
        cur = match cur.get(seg) {
            Some(v) => v,
            None => return f.not, // missing field: only a `not` filter passes
        };
    }
    // Strings compare directly; numbers/bools against their string form.
    let actual = match cur {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return f.not,
    };
    let matched = if let Some(eq) = f.equals.as_deref() {
        actual == eq
    } else if let Some(re) = f.matches.as_deref() {
        regex::Regex::new(re).map(|r| r.is_match(&actual)).unwrap_or(false)
    } else {
        true
    };
    matched != f.not
}
