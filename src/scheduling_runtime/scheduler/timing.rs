//! Timing 助手（P2 自 scheduler.rs 拆出，纯移动）：interval 字符串解析
//! （`"5m"` / `"30s"` / `"1h"`，缺省单位为分钟）与 next-run 计算
//! （cron / interval / 一次性 `At`）。

use super::*;

// ── Interval parsing ───────────────────────────────────────────────────────

/// Parse interval string like "5m", "30m", "1h" to Duration.
pub fn parse_interval(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s == "0" {
        return None;
    }

    let (num_part, multiplier) = if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        // Default to minutes if no suffix
        (s, 60)
    };

    let num: u64 = num_part.parse().ok()?;
    Some(Duration::from_secs(num * multiplier))
}

// ── Active hours ───────────────────────────────────────────────────────────

pub fn compute_next_run(
    spec: Option<&ScheduleSpec>,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    let kind = spec?.kind();
    match &kind {
        ScheduleKind::Every { interval_ms } => {
            let interval_ms = *interval_ms;
            let base_ms = last_run
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.timestamp_millis() as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);
            let next_ms = base_ms + interval_ms;
            chrono::DateTime::from_timestamp_millis(next_ms as i64)
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        ScheduleKind::At { at } => {
            if last_run.is_some() {
                return None; // Already executed
            }
            chrono::DateTime::parse_from_rfc3339(at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        ScheduleKind::Cron { expr } => {
            let normalized = normalize_weekday_unix(expr);
            let cron_schedule: cron::Schedule = match normalized.parse() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(schedule = %expr, err = %e, "invalid cron expression");
                    return None;
                }
            };
            let tz = resolve_tz(tz_name);
            let base_utc = match last_run {
                Some(ts) => chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                None => chrono::Utc::now(),
            };
            let base_local = base_utc.with_timezone(&tz);
            cron_schedule
                .after(&base_local)
                .next()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
    }
}

