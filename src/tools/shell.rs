//! Shell execution tool — foreground (with timeout + partial output) and background mode.
//!
//! Multi-segment commands (`a && b`, `a; b`, `a || b`) use **checkpoint
//! execution**: the command is split into segments, a checkpoint script is
//! generated with markers after each segment, and output is redirected to a
//! file. If the daemon dies mid-execution, recovery reads the output file to
//! determine which segments completed, then executes only the remaining ones.

use crate::providers::{Tool, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, timeout};

const MAX_OUTPUT_INLINE: usize = 30_000;

/// Cap on the checkpoint output file. A leftover journal that is never
/// cleared causes every subsequent multi-segment call to append to the same
/// file with the same stale marker_id — the file grows unboundedly and can
/// exceed this cap. Past the cap the journal+output pair is discarded.
const MAX_JOURNAL_OUTPUT: usize = 64 * 1024 * 1024; // 64 MiB

struct TruncatedOutput {
    text: String,
    full_output_path: Option<String>,
    truncated: bool,
    total_bytes: usize,
    total_lines: usize,
}

async fn truncate_large_output(output: &str) -> TruncatedOutput {
    let total_bytes = output.len();
    let total_lines = output.lines().count();
    if output.len() <= MAX_OUTPUT_INLINE {
        return TruncatedOutput {
            text: output.to_string(),
            full_output_path: None,
            truncated: false,
            total_bytes,
            total_lines,
        };
    }

    let cut = safe_char_boundary(output, MAX_OUTPUT_INLINE);
    let head_lines = output[..cut].lines().count();
    let remaining_lines = total_lines.saturating_sub(head_lines);
    let file_path = format!("/tmp/myclaw-shell-{}.txt", uuid::Uuid::new_v4().simple());
    match tokio::fs::write(&file_path, output).await {
        Ok(()) => TruncatedOutput {
            text: format!(
                "{}\n\n... [{} of {} lines truncated. full_output_path={}] ...\n... [Read with file_read offset/limit] ...",
                &output[..cut], remaining_lines, total_lines, file_path
            ),
            full_output_path: Some(file_path),
            truncated: true,
            total_bytes,
            total_lines,
        },
        Err(_) => TruncatedOutput {
            text: format!(
                "{}\n\n... [{} of {} lines truncated; failed to persist full output] ...",
                &output[..cut], remaining_lines, total_lines
            ),
            full_output_path: None,
            truncated: true,
            total_bytes,
            total_lines,
        },
    }
}

fn safe_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Read a file as lossy UTF-8. Checkpoint output files can accumulate bytes
/// from commands that truncate mid-multibyte-character (e.g. `cut -c`), which
/// makes the whole file invalid UTF-8 — `read_to_string` would fail outright
/// and the tool would report every segment as "Not Executed".
fn read_lossy(path: &std::path::Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(_) => std::fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .ok(),
    }
}

// ── Segment splitting ──────────────────────────────────────────────────────

/// Separator connecting two consecutive shell segments.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sep {
    AndThen,   // &&
    OrElse,    // ||
    Sequence,  // ; or \n
    None,      // first segment (no preceding separator)
}

/// A single shell command segment extracted from a compound command.
struct Segment {
    command: String,
    /// Separator that connects the **previous** segment to this one.
    prev_sep: Sep,
}

fn sep_to_str(s: Sep) -> &'static str {
    match s {
        Sep::AndThen => "&&",
        Sep::OrElse => "||",
        Sep::Sequence => ";",
        Sep::None => "",
    }
}

/// Split a compound shell command into segments at `&&`, `||`, `;`, and `\n`
/// boundaries. Respects single quotes, double quotes, backslash escaping, and
/// `()` nesting. Pipe `|` and background `&` (single char) are **not**
/// separators — they stay within the current segment.
fn split_shell_command(command: &str) -> Vec<Segment> {
    let mut pieces: Vec<String> = Vec::new();
    let mut seps: Vec<Sep> = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth = 0i32;

    while let Some(ch) = chars.next() {
        if in_single {
            current.push(ch);
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            current.push(ch);
            if ch == '"' {
                in_double = false;
            }
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                current.push(ch);
            }
            '"' => {
                in_double = true;
                current.push(ch);
            }
            '\\' => {
                current.push(ch);
                if let Some(&next) = chars.peek() {
                    current.push(next);
                    chars.next();
                }
            }
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth -= 1;
                current.push(ch);
            }
            '&' if paren_depth == 0 => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        pieces.push(trimmed);
                        seps.push(Sep::AndThen);
                    }
                    current.clear();
                } else {
                    current.push(ch); // single & = background
                }
            }
            '|' if paren_depth == 0 => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        pieces.push(trimmed);
                        seps.push(Sep::OrElse);
                    }
                    current.clear();
                } else {
                    current.push(ch); // single | = pipe
                }
            }
            ';' if paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    pieces.push(trimmed);
                    seps.push(Sep::Sequence);
                }
                current.clear();
            }
            '\n' if paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    pieces.push(trimmed);
                    seps.push(Sep::Sequence);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        pieces.push(trimmed);
    }

    pieces
        .into_iter()
        .enumerate()
        .map(|(i, cmd)| Segment {
            command: cmd,
            prev_sep: if i == 0 {
                Sep::None
            } else {
                seps.get(i - 1).copied().unwrap_or(Sep::Sequence)
            },
        })
        .collect()
}

// ── Checkpoint script generation ────────────────────────────────────────────

/// Generate a shell script that executes segments `start_idx..` sequentially,
/// printing a unique marker line after each segment to capture its exit code.
///
/// The marker format is: `__MYCLAW_CHK_{marker_id}_{abs_index}_{exit_code}__`
///
/// `&&` / `||` semantics are honoured: if a segment is skipped because the
/// previous segment's exit code didn't meet the condition, the marker is still
/// printed with the propagated exit code.
fn generate_checkpoint_script(
    segments: &[Segment],
    start_idx: usize,
    prev_exit_code: Option<i32>,
    marker_id: &str,
) -> String {
    let mut s = String::new();

    // On recovery, seed the exit code of the last known segment so that
    // `&&` / `||` checks for the first executed segment work correctly.
    if start_idx > 0 {
        let code = prev_exit_code.unwrap_or(0);
        s.push_str(&format!("_E{}={}\n", start_idx - 1, code));
    }

    for (i, seg) in segments.iter().enumerate().skip(start_idx) {

        if i > 0 {
            match seg.prev_sep {
                Sep::AndThen => {
                    s.push_str(&format!(
                        "if [ $_E{} -ne 0 ]; then _E{}=$_E{}; else\n",
                        i - 1,
                        i,
                        i - 1
                    ));
                }
                Sep::OrElse => {
                    s.push_str(&format!(
                        "if [ $_E{} -eq 0 ]; then _E{}=$_E{}; else\n",
                        i - 1,
                        i,
                        i - 1
                    ));
                }
                Sep::Sequence | Sep::None => {}
            }
        }

        s.push_str(&seg.command);
        s.push('\n');
        s.push_str(&format!("_E{}=$?\n", i));

        if i > 0 {
            match seg.prev_sep {
                Sep::AndThen | Sep::OrElse => s.push_str("fi\n"),
                _ => {}
            }
        }

        s.push_str(&format!(
            "printf '\\n__MYCLAW_CHK_{}_{}_%d__\\n' $_E{}\n",
            marker_id, i, i
        ));
    }

    if !segments.is_empty() {
        s.push_str(&format!("exit $_E{}\n", segments.len() - 1));
    }
    s
}

// ── Checkpoint output parsing ───────────────────────────────────────────────

struct ParsedSegment {
    stdout: String,
    exit_code: Option<i32>,
    completed: bool,
}

/// Parse checkpoint output for segments `start_idx..start_idx+count`.
/// Returns one `ParsedSegment` per requested segment. The first segment
/// without a marker is considered interrupted; subsequent segments are
/// marked as not-started.
fn parse_segment_range(
    content: &str,
    marker_id: &str,
    start_idx: usize,
    count: usize,
) -> Vec<ParsedSegment> {
    let mut results = Vec::with_capacity(count);
    let mut search_from = 0;

    for offset in 0..count {
        let abs_idx = start_idx + offset;
        let prefix = format!("__MYCLAW_CHK_{}_{}_", marker_id, abs_idx);

        if let Some(rel_pos) = content[search_from..].find(&prefix) {
            let abs_pos = search_from + rel_pos;
            let stdout = content[search_from..abs_pos]
                .trim_end_matches('\n')
                .to_string();

            let line_end = content[abs_pos..]
                .find('\n')
                .map(|n| abs_pos + n)
                .unwrap_or(content.len());
            let marker_line = content[abs_pos..line_end].trim();
            let exit_code = marker_line
                .strip_prefix(&prefix)
                .and_then(|r| r.strip_suffix("__"))
                .and_then(|s| s.trim().parse::<i32>().ok());

            results.push(ParsedSegment {
                stdout,
                exit_code,
                completed: true,
            });
            search_from = line_end + 1;
        } else {
            // Interrupted — collect partial output, mark remaining as unstarted.
            let partial = content[search_from..].to_string();
            results.push(ParsedSegment {
                stdout: partial,
                exit_code: None,
                completed: false,
            });
            for _ in (offset + 1)..count {
                results.push(ParsedSegment {
                    stdout: String::new(),
                    exit_code: None,
                    completed: false,
                });
            }
            break;
        }
    }
    results
}

// ── Shell journal ───────────────────────────────────────────────────────────

/// Shell checkpoint journal — tracks segment-level progress so recovery
/// can resume from the first un-executed segment after a daemon restart.
#[derive(Serialize, Deserialize)]
pub struct ShellJournal {
    marker_id: String,
    segments: Vec<String>,
    seps: Vec<String>,
}

impl ShellJournal {
    fn journal_path(dir: &Path, session_id: &str) -> PathBuf {
        dir.join(crate::ids::bare_dir_name(session_id)).join(".shell_journal")
    }

    fn output_path(dir: &Path, session_id: &str) -> PathBuf {
        dir.join(crate::ids::bare_dir_name(session_id)).join(".shell_output")
    }

    /// Read journal + output file. Returns `None` if journal is absent or
    /// invalid.
    fn load(
        sessions_dir: Option<&Path>,
        session_id: &str,
    ) -> Option<(Self, String)> {
        let dir = sessions_dir?;
        let data = std::fs::read_to_string(Self::journal_path(dir, session_id)).ok()?;
        let journal: Self = serde_json::from_str(&data).ok()?;
        let output =
            read_lossy(&Self::output_path(dir, session_id)).unwrap_or_default();
        Some((journal, output))
    }

    fn write(&self, sessions_dir: &Path, session_id: &str) {
        let path = Self::journal_path(sessions_dir, session_id);
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn clear(sessions_dir: Option<&Path>, session_id: &str) {
        if let Some(dir) = sessions_dir {
            let _ = std::fs::remove_file(Self::journal_path(dir, session_id));
            let _ = std::fs::remove_file(Self::output_path(dir, session_id));
        }
    }

    pub fn exists(sessions_dir: Option<&Path>, session_id: &str) -> bool {
        sessions_dir
            .map(|dir| Self::journal_path(dir, session_id).exists())
            .unwrap_or(false)
    }
}

/// Whether a journal describes exactly the same command (segments + separators)
/// as the one about to run. A journal left behind by an interrupted *different*
/// command must be discarded, otherwise the stale marker_id keeps being reused
/// against an ever-growing output file.
fn journal_matches_command(journal: &ShellJournal, segments: &[Segment]) -> bool {
    journal.segments.len() == segments.len()
        && journal
            .segments
            .iter()
            .zip(segments.iter())
            .all(|(jc, s)| jc == &s.command)
        && journal.seps.len() == segments.len()
        && journal
            .seps
            .iter()
            .zip(segments.iter())
            .all(|(js, s)| js == sep_to_str(s.prev_sep))
}

pub struct BgProcEntry {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    started_at: std::time::Instant,
    command: String,
    finished: bool,
    exit_code: Option<i32>,
}

pub type BgProcRegistry = Arc<RwLock<HashMap<String, BgProcEntry>>>;

pub struct ShellTool {
    bg_procs: BgProcRegistry,
    sessions_dir: Option<PathBuf>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ShellTool {
    pub fn new(sessions_dir: Option<PathBuf>) -> Self {
        Self {
            bg_procs: Arc::new(RwLock::new(HashMap::new())),
            sessions_dir,
        }
    }

    pub fn bg_registry(&self) -> BgProcRegistry {
        self.bg_procs.clone()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr, and exit code. \
         On timeout, partial output collected so far is returned. \
         Set `background: true` for fire-and-forget execution — returns a process_id \
         you can poll later with `shell_poll`. \
         Large output (>30K chars) is truncated: the first 30K is returned inline \
         and the full output is saved to a temp file with full_output_path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 300). On timeout, partial output collected so far is returned." },
                "workdir": { "type": "string", "description": "Working directory (default: current)." },
                "background": { "type": "boolean", "description": "If true, run the command in the background and return a process_id immediately. Use shell_poll to check status and collect output." }
            },
            "required": ["command"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        3_000
    }

    fn preferred_timeout_secs(&self) -> Option<u64> {
        Some(300)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'command' is required"))?;
        let background = args["background"].as_bool().unwrap_or(false);
        let workdir = args["workdir"].as_str();

        if background {
            return self.run_background(command, workdir).await;
        }

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120).min(300);

        // Multi-segment commands use checkpoint execution so that recovery
        // can resume from the first un-executed segment after a daemon restart.
        if self.sessions_dir.is_some() {
            let segments = split_shell_command(command);
            if segments.len() > 1 {
                return self
                    .run_with_checkpoints(&segments, workdir, timeout_secs, &session.id)
                    .await;
            }
        }

        self.run_foreground(command, workdir, timeout_secs).await
    }
}

impl ShellTool {
    async fn spawn_child(command: &str, workdir: Option<&str>) -> anyhow::Result<Child> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn command: {}", e))
    }

    async fn run_foreground(
        &self,
        command: &str,
        workdir: Option<&str>,
        timeout_secs: u64,
    ) -> anyhow::Result<ToolResult> {
        let mut child = match Self::spawn_child(command, workdir).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        spawn_reader(child.stdout.take(), stdout_buf.clone());
        spawn_reader(child.stderr.take(), stderr_buf.clone());

        let wait_result = timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        let (state, exit_code, success, error) = match wait_result {
            Ok(Ok(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                (
                    "exited",
                    Some(exit_code),
                    status.success(),
                    if status.success() {
                        None
                    } else {
                        Some(format!("exit code {}", exit_code))
                    },
                )
            }
            Ok(Err(e)) => ("wait_error", None, false, Some(format!("wait failed: {}", e))),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (
                    "timeout",
                    None,
                    false,
                    Some(format!("command timed out after {}s", timeout_secs)),
                )
            }
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        let stdout = stdout_buf.lock().await.clone();
        let stderr = stderr_buf.lock().await.clone();
        let output_text = format_shell_output(state, exit_code, timeout_secs, &stdout, &stderr);
        let output_text = crate::str_utils::neutralize_spoofing(&output_text);
        let truncated = truncate_large_output(&output_text).await;
        let output = add_truncation_metadata(truncated);

        Ok(ToolResult {
            success,
            output,
            error,
        })
    }

    async fn run_background(
        &self,
        command: &str,
        workdir: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        let mut child = match Self::spawn_child(command, workdir).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let proc_id = format!("bg_{}", uuid::Uuid::new_v4().simple());
        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        spawn_reader(child.stdout.take(), stdout_buf.clone());
        spawn_reader(child.stderr.take(), stderr_buf.clone());

        self.bg_procs.write().await.insert(
            proc_id.clone(),
            BgProcEntry {
                stdout: stdout_buf,
                stderr: stderr_buf,
                started_at: std::time::Instant::now(),
                command: command.to_string(),
                finished: false,
                exit_code: None,
            },
        );

        let bg_procs = self.bg_procs.clone();
        let proc_id_clone = proc_id.clone();
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);
            if let Some(entry) = bg_procs.write().await.get_mut(&proc_id_clone) {
                entry.finished = true;
                entry.exit_code = Some(exit_code);
            }
        });

        Ok(ToolResult {
            success: true,
            output: format!(
                "state=running\nprocess_id={}\ncommand={}\nuse shell_poll to check status and collect output",
                proc_id, command
            ),
            error: None,
        })
    }

    async fn run_with_checkpoints(
        &self,
        segments: &[Segment],
        workdir: Option<&str>,
        timeout_secs: u64,
        session_id: &str,
    ) -> anyhow::Result<ToolResult> {
        let sessions_dir = self.sessions_dir.as_deref().unwrap();

        // Stale-journal guard. The journal is only cleared on a clean,
        // fully-completed run; an interrupted or daemon-crashed run leaves it
        // behind. A leftover journal from a *different* command poisons every
        // subsequent multi-segment call: the stale marker_id is reused to
        // append into an ever-growing output file, and the recovery parser
        // then matches against historical markers. Verify the journal matches
        // the current command and the output file is within the size cap;
        // otherwise discard both and start fresh.
        if let Some((journal, output)) = ShellJournal::load(Some(sessions_dir), session_id) {
            let matches = journal_matches_command(&journal, segments);
            if !matches || output.len() > MAX_JOURNAL_OUTPUT {
                tracing::warn!(
                    matched = matches,
                    output_bytes = output.len(),
                    "discarding stale shell checkpoint journal"
                );
                ShellJournal::clear(Some(sessions_dir), session_id);
            }
        }

        let (start_idx, prev_exit_code, marker_id) =
            match ShellJournal::load(Some(sessions_dir), session_id) {
                Some((journal, output)) => {
                    let parsed = parse_segment_range(&output, &journal.marker_id, 0, journal.segments.len());
                    let mut first_incomplete = 0;
                    let mut last_code = None;
                    for (i, p) in parsed.iter().enumerate() {
                        if p.completed {
                            first_incomplete = i + 1;
                            last_code = p.exit_code;
                        } else {
                            break;
                        }
                    }
                    if first_incomplete >= segments.len() {
                        // Already fully completed (should be rare/impossible due to execution model, but handle it).
                        ShellJournal::clear(Some(sessions_dir), session_id);
                        return Ok(ToolResult {
                            success: last_code.unwrap_or(0) == 0,
                            output: output.clone(),
                            error: None,
                        });
                    }
                    (first_incomplete, last_code, journal.marker_id)
                }
                None => {
                    let marker_id = uuid::Uuid::new_v4().simple().to_string();
                    let journal = ShellJournal {
                        marker_id: marker_id.clone(),
                        segments: segments.iter().map(|s| s.command.clone()).collect(),
                        seps: segments.iter().map(|s| sep_to_str(s.prev_sep).to_string()).collect(),
                    };
                    journal.write(sessions_dir, session_id);
                    (0, None, marker_id)
                }
            };

        let script = generate_checkpoint_script(segments, start_idx, prev_exit_code, &marker_id);
        
        // Ensure parent dir exists (session dir)
        std::fs::create_dir_all(sessions_dir.join(crate::ids::bare_dir_name(session_id)))?;
        let script_path = sessions_dir
            .join(crate::ids::bare_dir_name(session_id))
            .join(".shell_checkpoint.sh");
        let output_path = ShellJournal::output_path(sessions_dir, session_id);
        
        std::fs::write(&script_path, &script)?;

        let mut cmd = Command::new("sh");
        cmd.arg(&script_path);
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        
        // Append mode so we keep output from previously completed segments
        let out_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)?;
        let err_file = out_file.try_clone()?; // Both to same file

        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file));

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&script_path);
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to spawn checkpoint script: {}", e)),
                });
            }
        };

        let wait_result = timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        
        let (state, child_exit_code, error) = match wait_result {
            Ok(Ok(status)) => {
                let code = status.code().unwrap_or(-1);
                ("exited", Some(code), None)
            }
            Ok(Err(e)) => ("wait_error", None, Some(format!("wait failed: {}", e))),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                ("timeout", None, Some(format!("command timed out after {}s", timeout_secs)))
            }
        };

        // Clean up the script
        let _ = std::fs::remove_file(&script_path);

        // Read the combined output
        let raw_output = read_lossy(&output_path).unwrap_or_default();
        let parsed = parse_segment_range(&raw_output, &marker_id, 0, segments.len());
        
        let mut final_code = child_exit_code.unwrap_or(-1);
        let mut success = child_exit_code.unwrap_or(-1) == 0;
        
        // Reconstruct the formatted output for the user
        let mut formatted = format!(
            "state={}\nexit_code={}\ntimeout_secs={}\ntotal_bytes={}\n",
            state,
            child_exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
            timeout_secs,
            raw_output.len(),
        );

        if let Some(e) = &error {
            formatted.push_str(&format!("error={}\n", e));
        }
        
        formatted.push_str("\n--- Segments ---\n");
        let mut all_completed = true;

        for (i, (seg, p)) in segments.iter().zip(parsed.iter()).enumerate() {
            let sep_str = sep_to_str(seg.prev_sep);
            if !sep_str.is_empty() {
                formatted.push_str(&format!(" [ {} ]\n", sep_str));
            }
            formatted.push_str(&format!("Segment {}: `{}`\n", i, seg.command));
            
            if p.completed {
                let code = p.exit_code.unwrap_or(-1);
                formatted.push_str(&format!("Status: Completed (Exit Code: {})\n", code));
                if !p.stdout.is_empty() {
                    formatted.push_str("Output:\n");
                    formatted.push_str(&p.stdout);
                    if !p.stdout.ends_with('\n') {
                        formatted.push('\n');
                    }
                }
                final_code = code; // last completed code
            } else if p.stdout.is_empty() {
                formatted.push_str("Status: Not Executed\n");
                all_completed = false;
            } else {
                formatted.push_str("Status: Interrupted / Timed Out\n");
                if !p.stdout.is_empty() {
                    formatted.push_str("Partial Output:\n");
                    formatted.push_str(&p.stdout);
                    if !p.stdout.ends_with('\n') {
                        formatted.push('\n');
                    }
                }
                all_completed = false;
            }
        }
        
        if all_completed && state == "exited" {
            // Clean up journal on full clean success
            ShellJournal::clear(Some(sessions_dir), session_id);
            success = final_code == 0;
        }

        let truncated = truncate_large_output(
            &crate::str_utils::neutralize_spoofing(&formatted)
        ).await;
        
        Ok(ToolResult {
            success,
            output: add_truncation_metadata(truncated),
            error: if !success && error.is_none() {
                Some(format!("exit code {}", final_code))
            } else {
                error
            },
        })
    }
}

fn spawn_reader<R>(reader: Option<R>, buf: Arc<Mutex<String>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    if let Some(mut reader) = reader {
        tokio::spawn(async move {
            let mut tmp = vec![0u8; 8192];
            loop {
                match reader.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => buf.lock().await.push_str(&String::from_utf8_lossy(&tmp[..n])),
                    Err(_) => break,
                }
            }
        });
    }
}

fn format_shell_output(
    state: &str,
    exit_code: Option<i32>,
    timeout_secs: u64,
    stdout: &str,
    stderr: &str,
) -> String {
    let mut output = format!(
        "state={}\nexit_code={}\ntimeout_secs={}\nstdout_bytes={}\nstderr_bytes={}\n",
        state,
        exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
        timeout_secs,
        stdout.len(),
        stderr.len()
    );
    if !stdout.is_empty() {
        output.push_str("\nstdout:\n");
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        output.push_str("\nstderr:\n");
        output.push_str(stderr);
    }
    output
}

fn add_truncation_metadata(truncated: TruncatedOutput) -> String {
    let mut output = format!(
        "truncated={}\ntotal_bytes={}\ntotal_lines={}\nfull_output_path={}\n",
        truncated.truncated,
        truncated.total_bytes,
        truncated.total_lines,
        truncated.full_output_path.as_deref().unwrap_or("null")
    );
    output.push_str(&truncated.text);
    output
}

pub struct ShellPollTool {
    bg_procs: BgProcRegistry,
}

impl ShellPollTool {
    pub fn new(bg_procs: BgProcRegistry) -> Self {
        Self { bg_procs }
    }
}

#[async_trait]
impl Tool for ShellPollTool {
    fn name(&self) -> &str {
        "shell_poll"
    }

    fn description(&self) -> &str {
        "Poll a background shell process started with `background: true`. Returns accumulated stdout/stderr and machine-readable state/exit_code/elapsed_secs. Set `wait_secs` to wait for completion before returning. Set `remove: true` to clean up the process entry after reading; running processes are never removed. Large output is truncated with full_output_path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string", "description": "The process_id returned by `shell` when background=true." },
                "remove": { "type": "boolean", "description": "If true, remove the process entry after reading output (default: false). Ignored while the process is still running." },
                "wait_secs": { "type": "integer", "description": "Optional seconds to wait for the process to finish before returning (default 0, max 300)." }
            },
            "required": ["process_id"]
        })
    }

    fn max_output_tokens(&self) -> usize {
        10_000
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _session: &crate::agents::session::Session,
    ) -> anyhow::Result<ToolResult> {
        let proc_id = args["process_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'process_id' is required"))?;
        let remove = args["remove"].as_bool().unwrap_or(false);
        let wait_secs = args["wait_secs"].as_u64().unwrap_or(0).min(300);

        let start_wait = std::time::Instant::now();
        loop {
            let finished = {
                let procs = self.bg_procs.read().await;
                let entry = match procs.get(proc_id) {
                    Some(e) => e,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("process_id '{}' not found", proc_id)),
                        });
                    }
                };
                entry.finished
            };
            if finished || wait_secs == 0 || start_wait.elapsed() >= Duration::from_secs(wait_secs) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let procs = self.bg_procs.read().await;
        let entry = match procs.get(proc_id) {
            Some(e) => e,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("process_id '{}' not found", proc_id)),
                });
            }
        };
        let stdout = entry.stdout.lock().await.clone();
        let stderr = entry.stderr.lock().await.clone();
        let elapsed = entry.started_at.elapsed();
        let finished = entry.finished;
        let exit_code = entry.exit_code;
        let command = entry.command.clone();
        drop(procs);

        let removed = remove && finished;
        if removed {
            self.bg_procs.write().await.remove(proc_id);
        }

        let state = if finished { "exited" } else { "running" };
        let mut output = format!(
            "state={}\nprocess_id={}\ncommand={}\nexit_code={}\nelapsed_secs={}\nstdout_bytes={}\nstderr_bytes={}\nremoved={}\n",
            state,
            proc_id,
            command,
            exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string()),
            elapsed.as_secs(),
            stdout.len(),
            stderr.len(),
            removed
        );
        if remove && !finished {
            output.push_str("note=remove_ignored_process_running\n");
        }
        if !stdout.is_empty() {
            output.push_str("\nstdout:\n");
            output.push_str(&stdout);
        }
        if !stderr.is_empty() {
            output.push_str("\nstderr:\n");
            output.push_str(&stderr);
        }

        let truncated = truncate_large_output(&output).await;
        Ok(ToolResult {
            success: true,
            output: add_truncation_metadata(truncated),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_single_command() {
        let segs = split_shell_command("echo hello");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].command, "echo hello");
        assert_eq!(segs[0].prev_sep, Sep::None);
    }

    #[test]
    fn split_and_then() {
        let segs = split_shell_command("a && b");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].command, "a");
        assert_eq!(segs[0].prev_sep, Sep::None);
        assert_eq!(segs[1].command, "b");
        assert_eq!(segs[1].prev_sep, Sep::AndThen);
    }

    #[test]
    fn split_or_else() {
        let segs = split_shell_command("a || b");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].prev_sep, Sep::OrElse);
    }

    #[test]
    fn split_sequence() {
        let segs = split_shell_command("a; b");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].prev_sep, Sep::Sequence);
    }

    #[test]
    fn split_newline() {
        let segs = split_shell_command("echo a\necho b");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].prev_sep, Sep::Sequence);
    }

    #[test]
    fn split_three_segments() {
        let segs = split_shell_command("a && b && c");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].prev_sep, Sep::None);
        assert_eq!(segs[1].prev_sep, Sep::AndThen);
        assert_eq!(segs[2].prev_sep, Sep::AndThen);
    }

    #[test]
    fn split_pipe_not_separator() {
        let segs = split_shell_command("a | b && c");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].command, "a | b");
        assert_eq!(segs[1].command, "c");
    }

    #[test]
    fn split_quotes_respected() {
        let segs = split_shell_command("echo \"a && b\" && c");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].command, "echo \"a && b\"");
        assert_eq!(segs[1].command, "c");
    }

    #[test]
    fn split_single_quotes_respected() {
        let segs = split_shell_command("echo 'a; b' ; c");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].command, "echo 'a; b'");
        assert_eq!(segs[1].command, "c");
    }

    #[test]
    fn split_parens_respected() {
        let segs = split_shell_command("(a && b) ; c");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].command, "(a && b)");
        assert_eq!(segs[1].command, "c");
    }

    #[test]
    fn script_simple_sequence() {
        let segs = split_shell_command("echo a && echo b");
        let script = generate_checkpoint_script(&segs, 0, None, "test123");
        assert!(script.contains("echo a"));
        assert!(script.contains("echo b"));
        assert!(script.contains("__MYCLAW_CHK_test123_0_"));
        assert!(script.contains("__MYCLAW_CHK_test123_1_"));
        assert!(script.contains("exit $_E1"));
    }

    #[test]
    fn script_recovery_start() {
        let segs = split_shell_command("a && b && c");
        let script = generate_checkpoint_script(&segs, 2, Some(0), "mid");
        assert!(script.contains("_E1=0"));
        assert!(script.contains("c"));
        assert!(!script.contains("\na\n"));
    }

    #[test]
    fn parse_all_completed() {
        let content = "output_a\n\n__MYCLAW_CHK_abc_0_0__\noutput_b\n\n__MYCLAW_CHK_abc_1_0__\n";
        let parsed = parse_segment_range(content, "abc", 0, 2);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].completed);
        assert_eq!(parsed[0].exit_code, Some(0));
        assert_eq!(parsed[0].stdout, "output_a");
        assert!(parsed[1].completed);
        assert_eq!(parsed[1].exit_code, Some(0));
        assert_eq!(parsed[1].stdout, "output_b");
    }

    #[test]
    fn parse_interrupted() {
        let content = "output_a\n\n__MYCLAW_CHK_abc_0_0__\npartial_b";
        let parsed = parse_segment_range(content, "abc", 0, 2);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].completed);
        assert!(!parsed[1].completed);
        assert_eq!(parsed[1].stdout, "partial_b");
    }

    #[test]
    fn parse_nonzero_exit() {
        let content = "err\n\n__MYCLAW_CHK_x_0_127__\n";
        let parsed = parse_segment_range(content, "x", 0, 1);
        assert_eq!(parsed[0].exit_code, Some(127));
    }

    #[test]
    fn journal_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "test_session";
        std::fs::create_dir_all(tmp.path().join(crate::ids::bare_dir_name(sid))).unwrap();

        let journal = ShellJournal {
            marker_id: "abc123".to_string(),
            segments: vec!["a".to_string(), "b".to_string()],
            seps: vec!["".to_string(), "&&".to_string()],
        };
        journal.write(tmp.path(), sid);
        assert!(ShellJournal::exists(Some(tmp.path()), sid));

        std::fs::write(ShellJournal::output_path(tmp.path(), sid), "test output").unwrap();
        let (loaded, output) = ShellJournal::load(Some(tmp.path()), sid).unwrap();
        assert_eq!(loaded.marker_id, "abc123");
        assert_eq!(loaded.segments, vec!["a", "b"]);
        assert_eq!(output, "test output");

        ShellJournal::clear(Some(tmp.path()), sid);
        assert!(!ShellJournal::exists(Some(tmp.path()), sid));
    }

    #[test]
    fn journal_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ShellJournal::load(Some(tmp.path()), "nope").is_none());
        assert!(!ShellJournal::exists(Some(tmp.path()), "nope"));
    }

    #[test]
    fn script_and_parse_integration() {
        let segs = split_shell_command("echo hello && echo world");
        let marker = "inttest";
        let script = generate_checkpoint_script(&segs, 0, None, marker);
        assert!(!script.is_empty());

        let simulated_output = "hello\n\n__MYCLAW_CHK_inttest_0_0__\nworld\n\n__MYCLAW_CHK_inttest_1_0__\n";
        let parsed = parse_segment_range(simulated_output, marker, 0, 2);
        assert!(parsed[0].completed);
        assert_eq!(parsed[0].stdout, "hello");
        assert_eq!(parsed[0].exit_code, Some(0));
        assert!(parsed[1].completed);
        assert_eq!(parsed[1].stdout, "world");
        assert_eq!(parsed[1].exit_code, Some(0));
    }

    #[test]
    fn journal_matches_same_command() {
        let segs = split_shell_command("echo a && echo b");
        let journal = ShellJournal {
            marker_id: "m".to_string(),
            segments: segs.iter().map(|s| s.command.clone()).collect(),
            seps: segs.iter().map(|s| sep_to_str(s.prev_sep).to_string()).collect(),
        };
        assert!(journal_matches_command(&journal, &segs));
    }

    #[test]
    fn journal_mismatch_different_command() {
        let segs = split_shell_command("echo a && echo b");
        let stale = ShellJournal {
            marker_id: "old".to_string(),
            segments: vec!["cd /somewhere".to_string(), "grep x".to_string()],
            seps: vec!["".to_string(), "&&".to_string()],
        };
        assert!(!journal_matches_command(&stale, &segs));
    }

    #[test]
    fn journal_mismatch_different_separator() {
        let segs = split_shell_command("echo a && echo b");
        let stale = ShellJournal {
            marker_id: "old".to_string(),
            segments: vec!["echo a".to_string(), "echo b".to_string()],
            seps: vec!["".to_string(), ";".to_string()],
        };
        assert!(!journal_matches_command(&stale, &segs));
    }

    #[test]
    fn read_lossy_tolerates_invalid_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.bin");
        // Invalid UTF-8: truncated multibyte char (e.g. from `cut -c` on CJK).
        let mut bytes = "ab".as_bytes().to_vec();
        bytes.push(0xe4); // start of a 3-byte char, never completed
        std::fs::write(&path, &bytes).unwrap();
        let s = read_lossy(&path).expect("lossy read must succeed");
        assert!(s.starts_with("ab"));
    }
}
