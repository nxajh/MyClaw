//! Cron Loader — 从 workspace/cron/ 目录加载定时任务文件。
//!
//! 每个 `.md` 文件定义一个 cron job，格式：
//! ```markdown
//! ---
//! schedule: "0 9 * * *"
//! target: telegram
//! ---
//!
//! 生成昨天的工作日报。
//! ```

use std::path::Path;

use tracing::{info, warn};

use crate::config::scheduler::CronJob;
use crate::str_utils::{extract_yaml_string, parse_front_matter};

/// 解析单个 cron job 文件。
pub fn parse_cron_file(path: &Path) -> anyhow::Result<CronJob> {
    let content = std::fs::read_to_string(path)?;
    let (front_matter, body) = parse_front_matter(&content);

    let schedule = extract_yaml_string(&front_matter, "schedule")
        .ok_or_else(|| anyhow::anyhow!("missing 'schedule' in front matter of {}", path.display()))?;

    let target = extract_yaml_string(&front_matter, "target")
        .unwrap_or_else(|| "last".to_string());

    let prompt = body.trim().to_string();

    if prompt.is_empty() {
        anyhow::bail!("empty prompt body in {}", path.display());
    }

    Ok(CronJob {
        schedule,
        prompt,
        target,
        active_hours: extract_yaml_string(&front_matter, "active_hours"),
    })
}

/// 扫描 cron 目录，加载所有 `.md` 文件为 CronJob。
pub fn load_cron_jobs(cron_dir: &Path) -> Vec<CronJob> {
    let mut jobs = Vec::new();

    if !cron_dir.exists() {
        return jobs;
    }

    let entries = match std::fs::read_dir(cron_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %cron_dir.display(), err = %e, "failed to read cron directory");
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

        match parse_cron_file(&path) {
            Ok(job) => {
                info!(
                    schedule = %job.schedule,
                    target = %job.target,
                    file = %path.file_name().unwrap_or_default().to_string_lossy(),
                    "cron job loaded"
                );
                jobs.push(job);
            }
            Err(e) => {
                warn!(path = %path.display(), err = %e, "failed to parse cron file");
            }
        }
    }

    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cron_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily-report.md");
        std::fs::write(
            &path,
            "---\nschedule: \"0 9 * * *\"\ntarget: telegram\n---\n\n生成昨天的工作日报。\n",
        )
        .unwrap();

        let job = parse_cron_file(&path).unwrap();
        assert_eq!(job.schedule, "0 9 * * *");
        assert_eq!(job.target, "telegram");
        assert_eq!(job.prompt, "生成昨天的工作日报。");
    }

    #[test]
    fn parse_cron_file_default_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check.md");
        std::fs::write(
            &path,
            "---\nschedule: \"*/30 * * * *\"\n---\n\n检查待办事项。\n",
        )
        .unwrap();

        let job = parse_cron_file(&path).unwrap();
        assert_eq!(job.target, "last");
    }

    #[test]
    fn parse_cron_file_missing_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.md");
        std::fs::write(&path, "---\ntarget: last\n---\nDo something.").unwrap();

        assert!(parse_cron_file(&path).is_err());
    }

    #[test]
    fn parse_cron_file_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.md");
        std::fs::write(&path, "---\nschedule: \"0 9 * * *\"\n---\n").unwrap();

        assert!(parse_cron_file(&path).is_err());
    }

    #[test]
    fn load_cron_jobs_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cron_dir = dir.path().join("cron");
        std::fs::create_dir_all(&cron_dir).unwrap();

        std::fs::write(
            cron_dir.join("report.md"),
            "---\nschedule: \"0 9 * * *\"\ntarget: telegram\n---\n日报",
        )
        .unwrap();
        std::fs::write(
            cron_dir.join("check.md"),
            "---\nschedule: \"*/30 * * * *\"\n---\n检查",
        )
        .unwrap();
        // 非 md 文件应跳过
        std::fs::write(cron_dir.join("notes.txt"), "not a cron job").unwrap();

        let jobs = load_cron_jobs(&cron_dir);
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn load_cron_jobs_missing_dir() {
        let jobs = load_cron_jobs(Path::new("/nonexistent"));
        assert!(jobs.is_empty());
    }
}
