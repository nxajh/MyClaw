//! Active hours checks and target parsing helpers.
//!
//! Extracted from scheduler.rs.

use std::time::Duration;
use chrono::Timelike;

use super::schedule::resolve_tz;

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

/// Parse target string "channel:account" into channel part.
/// Returns None for "last", "none", or empty strings.
pub(super) fn parse_target_channel(target: &str) -> Option<String> {
    match target {
        "last" | "none" | "" => None,
        _ => target.split_once(':').map(|(ch, _)| ch.to_string()).or_else(|| Some(target.to_string())),
    }
}

/// Parse target string "channel:account" into account part.
/// Returns None for "last", "none", or empty strings.
pub(super) fn parse_target_account(target: &str) -> Option<String> {
    match target {
        "last" | "none" | "" => None,
        _ => target.split_once(':').map(|(_, acc)| acc.to_string()),
    }
}

/// Check if current time is within active hours.
/// Format: "HH:MM-HH:MM" e.g. "08:00-24:00".
/// `tz_name` is the IANA timezone (e.g. "Asia/Shanghai").
pub fn is_active_hours(active_hours: &Option<String>, tz_name: &str) -> bool {
    let Some(hours) = active_hours else {
        return true; // No restriction = always active
    };

    let (start_mins, end_mins) = match parse_hours(hours) {
        Some(h) => h,
        None => return true, // Invalid format = always active
    };

    let tz = resolve_tz(tz_name);
    let now_local = chrono::Utc::now().with_timezone(&tz);
    let now_mins = now_local.hour() * 60 + now_local.minute();

    now_mins >= start_mins && now_mins < end_mins
}

/// Parse "HH:MM-HH:MM" → (start_minutes, end_minutes).
fn parse_hours(s: &str) -> Option<(u32, u32)> {
    let (start, end) = s.split_once('-')?;
    Some((parse_hhmm(start.trim())?, parse_hhmm(end.trim())?))
}

pub(crate) fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let hours: u32 = h.trim().parse().ok()?;
    let mins: u32 = m.trim().parse().ok()?;
    Some(hours * 60 + mins)
}
