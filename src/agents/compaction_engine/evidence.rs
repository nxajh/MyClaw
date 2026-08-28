//! Evidence index — file paths / shell commands / commit hashes / CI run IDs
//! mined from compacted history and appended to summaries for verification.

use crate::providers::ChatMessage;

pub(super) fn append_evidence_index(
    mut summary: String,
    messages: &[ChatMessage],
    compact_start: usize,
    compact_end: usize,
    boundary: usize,
    model_id: &str,
) -> String {
    let paths = extract_file_paths(messages);
    let commands = extract_shell_commands(messages);
    let commits = extract_commit_hashes(messages);
    let runs = extract_ci_runs(messages);

    let has_section = summary.contains("## Evidence Index") || summary.contains("Evidence Index");
    if !has_section {
        summary.push_str("\n\n## Evidence Index\n");
    } else {
        summary.push_str("\n\nAdditional evidence captured by compaction:\n");
    }
    summary.push_str(&format!(
        "- Compaction range: compact_start={}, compact_end={}, boundary={}, messages_compacted={}, model={}\n",
        compact_start,
        compact_end,
        boundary,
        messages.len(),
        model_id
    ));
    if !paths.is_empty() {
        summary.push_str(&format!(
            "- File paths: {}\n",
            paths
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !commands.is_empty() {
        summary.push_str(&format!(
            "- Commands: {}\n",
            commands
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    if !commits.is_empty() {
        summary.push_str(&format!(
            "- Commit hashes: {}\n",
            commits
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !runs.is_empty() {
        summary.push_str(&format!(
            "- CI/run IDs: {}\n",
            runs.iter().take(20).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    summary
}

fn extract_shell_commands(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r#"(?s)"command"\s*:\s*"([^"]{1,240})""#).unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(1) {
                let value = m.as_str().replace("\\n", " ");
                if seen.insert(value.clone()) {
                    commands.push(value);
                }
            }
        }
    }
    commands
}

fn extract_commit_hashes(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut hashes = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(0) {
                let value = m.as_str().to_string();
                if seen.insert(value.clone()) {
                    hashes.push(value);
                }
            }
        }
    }
    hashes
}

fn extract_ci_runs(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(?:run id|run_id|workflow run|CI run)[:= ]+([0-9]{5,})\b")
            .unwrap()
    });
    let mut seen = std::collections::HashSet::new();
    let mut runs = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(1) {
                let value = m.as_str().to_string();
                if seen.insert(value.clone()) {
                    runs.push(value);
                }
            }
        }
    }
    runs
}

pub(super) fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"(?:/[\w/.-]+\.\w{1,5})|(?:src/[\w/.-]+)").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for msg in messages {
        for cap in re.captures_iter(&msg.text_content()) {
            if let Some(m) = cap.get(0) {
                let p = m.as_str().to_string();
                if seen.insert(p.clone()) {
                    paths.push(p);
                }
            }
        }
    }
    paths
}
