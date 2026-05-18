//! Cron schedule computation — pure functions, no state.
//!
//! Extracted from scheduler.rs.

use crate::agents::scheduling::cron_types::ScheduleKind;

/// Resolve an IANA timezone name to a `chrono_tz::Tz`.
/// Falls back to UTC if the name is invalid.
pub fn resolve_tz(name: &str) -> chrono_tz::Tz {
    name.parse::<chrono_tz::Tz>().unwrap_or_else(|_| {
        tracing::warn!(tz = %name, "invalid IANA timezone, falling back to UTC");
        chrono_tz::UTC
    })
}

/// Compute the next run time for a job.
/// Supports both legacy cron expressions and new ScheduleKind.
pub fn compute_next_run(schedule: &str, last_run: Option<&str>, tz_name: &str) -> Option<String> {
    compute_next_run_inner(None, schedule, last_run, tz_name)
}

/// Full compute with ScheduleKind support.
pub fn compute_next_run_full(
    kind: Option<&ScheduleKind>,
    schedule: &str,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    compute_next_run_inner(kind, schedule, last_run, tz_name)
}

pub(super) fn compute_next_run_inner(
    kind: Option<&ScheduleKind>,
    schedule: &str,
    last_run: Option<&str>,
    tz_name: &str,
) -> Option<String> {
    match kind {
        Some(ScheduleKind::Every { interval_ms }) => {
            let base_ms = last_run
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.timestamp_millis() as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);
            let next_ms = base_ms + interval_ms;
            chrono::DateTime::from_timestamp_millis(next_ms as i64)
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        Some(ScheduleKind::At { at }) => {
            if last_run.is_some() {
                return None; // Already executed
            }
            chrono::DateTime::parse_from_rfc3339(at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        Some(ScheduleKind::Cron { expr }) => {
            let cron_schedule: cron::Schedule = match expr.parse() {
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
            cron_schedule.after(&base_local).next()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
        None => {
            // Legacy cron expression from schedule string.
            let cron_schedule: cron::Schedule = match schedule.parse() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(schedule = %schedule, err = %e, "invalid cron expression");
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
            cron_schedule.after(&base_local).next()
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
        }
    }
}
