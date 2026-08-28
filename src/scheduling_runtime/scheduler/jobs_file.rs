//! Jobs 存储加载与旧格式折叠（P2 自 scheduler.rs 拆出，纯移动）：
//! 逐 job 的 `{jobs_root}/{dir}/meta.json` 目录扫描、legacy jobs.json
//! 回退读取（`schedule_kind` 判别式 / 纯字符串 schedule / 旧 `target`
//! 与 pre-#78 `delivery` 折叠），以及 schedule 规范化。

use super::*;

// ── Persistence ─────────────────────────────────────────────────────────────

/// Idempotently remove a directory tree. A missing directory is not an
/// error (#77): the caller may be pruning a dir that a previous sweep, or a
/// concurrent removal, already cleaned up.
pub(super) fn delete_job_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Fold any Legacy (plain-string) schedule specs into the canonical Kind
/// form so the store invariant holds: persisted `schedule` is always the
/// polymorphic object, never a bare string.
pub(super) fn normalize_schedule_specs(jobs: &mut [JobEntry]) {
    for job in jobs {
        if let Some(spec) = job.schedule.take() {
            job.schedule = Some(ScheduleSpec::Kind(spec.kind()));
        }
    }
}

/// Fold the legacy `schedule_kind` sibling into `schedule` on ONE job object
/// (Value level, before struct parsing). The discriminator is authoritative
/// when present; a bare string schedule is a cron expression.
fn fold_one_schedule_kind(value: serde_json::Value) -> serde_json::Value {
    let mut value = value;
    let Some(obj) = value.as_object_mut() else { return value };
    let legacy_kind = obj.remove("schedule_kind").filter(|k| !k.is_null());
    match (legacy_kind, obj.get("schedule")) {
        (Some(kind), _) => {
            obj.insert("schedule".to_string(), kind);
        }
        (None, Some(serde_json::Value::String(s))) if !s.is_empty() => {
            let expr = s.clone();
            obj.insert(
                "schedule".to_string(),
                serde_json::json!({"kind": "cron", "expr": expr}),
            );
        }
        _ => {}
    }
    value
}

/// Apply [`fold_one_schedule_kind`] to a jobs document — either a bare
/// `JobsFile` object `{"jobs": [...]}` or a bare jobs array.
pub(super) fn fold_schedule_kind(value: serde_json::Value) -> serde_json::Value {
    let mut value = value;
    if let Some(obj) = value.as_object_mut() {
        if let Some(jobs) = obj.get_mut("jobs") {
            if let Some(arr) = jobs.as_array_mut() {
                for job in arr.iter_mut() {
                    *job = fold_one_schedule_kind(job.take());
                }
            }
        }
        return value;
    }
    if let Some(arr) = value.as_array_mut() {
        for job in arr.iter_mut() {
            *job = fold_one_schedule_kind(job.take());
        }
    }
    value
}

/// Fold the legacy `target: String` field (+ pre-#78 `delivery` object,
/// which had no `mode` and required `channel`) into the canonical
/// `DeliveryConfig` shape on ONE job object (Value level, before struct
/// parsing) — #78. A `delivery` object that already carries `mode` is left
/// untouched (already migrated). `target` itself is simply ignored by
/// `serde_json::from_value::<JobEntry>` afterwards — the struct no longer
/// has that field.
fn fold_one_target_delivery(value: serde_json::Value) -> serde_json::Value {
    let mut value = value;
    let Some(obj) = value.as_object_mut() else { return value };
    if obj.get("delivery").and_then(|d| d.get("mode")).is_some() {
        return value; // already migrated
    }
    let Some(target) = obj.get("target").and_then(|v| v.as_str()).map(str::to_string) else {
        return value; // no legacy field — leave delivery as-is (serde default = Last)
    };
    let mut merged = parse_target_string(&target);
    // The pre-#78 `delivery` object's channel/account_id were dead reads
    // (see #78) — `target` alone decided routing, so it wins here. Only
    // `to`/`thread_id` actually took effect before, so those carry over.
    if let Some(existing) = obj.get("delivery").and_then(|d| d.as_object()) {
        if let Some(to) = existing.get("to").and_then(|v| v.as_str()) {
            merged.to = Some(to.to_string());
        }
        if let Some(th) = existing.get("thread_id").and_then(|v| v.as_str()) {
            merged.thread_id = Some(th.to_string());
        }
    }
    obj.insert(
        "delivery".to_string(),
        serde_json::to_value(&merged).unwrap_or_default(),
    );
    value
}

/// Apply [`fold_one_target_delivery`] to a jobs document — either a bare
/// `JobsFile` object `{"jobs": [...]}` or a bare jobs array.
pub(super) fn fold_target_delivery(value: serde_json::Value) -> serde_json::Value {
    let mut value = value;
    if let Some(obj) = value.as_object_mut() {
        if let Some(jobs) = obj.get_mut("jobs") {
            if let Some(arr) = jobs.as_array_mut() {
                for job in arr.iter_mut() {
                    *job = fold_one_target_delivery(job.take());
                }
            }
        }
        return value;
    }
    if let Some(arr) = value.as_array_mut() {
        for job in arr.iter_mut() {
            *job = fold_one_target_delivery(job.take());
        }
    }
    value
}

/// Load every `{jobs_root}/{dir}/meta.json` (P1-B2 directory-based store).
/// Malformed entries are skipped. Sorted by id for stable ordering.
pub(super) fn load_jobs_from_dirs(jobs_root: &Path) -> Vec<JobEntry> {
    let entries = match std::fs::read_dir(jobs_root) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        let meta = entry.path().join("meta.json");
        if !meta.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&meta) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // §3.4 convergence: fold the legacy `schedule_kind` discriminator
        // (and the plain-string `schedule` it accompanied) into the
        // canonical polymorphic schedule object (shared with the legacy
        // jobs.json fallback — see fold_schedule_kind).
        let value: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %meta.display(), err = %e,
                    "jobs store: skipping malformed meta.json");
                continue;
            }
        };
        let value = fold_one_schedule_kind(value);
        let value = fold_one_target_delivery(value);
        match serde_json::from_value::<JobEntry>(value) {
            Ok(mut job) => {
                // §3.4: name is required. Legacy meta.json without a name
                // backfills with the bare uuid (which is a valid slug) and
                // warns; persisted back on the next save.
                if job.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    let fallback = crate::ids::bare_dir_name(&job.id);
                    tracing::warn!(job_id = %job.id, fallback = %fallback,
                        "jobs store: meta.json without name, backfilling with bare uuid");
                    job.name = Some(fallback);
                }
                jobs.push(job)
            }
            Err(e) => tracing::warn!(
                path = %meta.display(),
                err = %e,
                "jobs store: skipping malformed meta.json"
            ),
        }
    }
    jobs.sort_by(|a, b| a.id.cmp(&b.id));
    jobs
}

