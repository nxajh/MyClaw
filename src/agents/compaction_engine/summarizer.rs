//! Summarizer plumbing — LLM prompt construction, streamed-response shape,
//! and post-hoc summary quality audit.

use super::evidence::extract_file_paths;
use crate::providers::{ChatMessage, ChatUsage, ToolCall};

pub(super) struct SummaryResponse {
    pub(super) text: String,
    pub(super) reasoning_content: Option<String>,
    pub(super) thinking_signature: Option<String>,
    pub(super) tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    pub(super) usage: Option<ChatUsage>,
}

pub(super) fn build_summarizer_prompt(msg_count: usize, existing_summary: Option<&str>) -> String {
    // Anti-hallucination rules shared by merge and fresh summary paths.
    // Prevents fabricating active user tasks from compressed/dirty history
    // (root cause of pure-image turns being rewritten as full Villagers-style quests).
    const ANTI_FAKE_TASK_RULES: &str = "\
CRITICAL — do NOT invent user tasks:\n\
- NEVER invent, reconstruct, or complete a user request that was not explicitly present\n\
  in the messages being summarized. If the latest user input is only an image marker,\n\
  a bare word like \"继续\"/\"continue\", a system-reminder, or incomplete fragments,\n\
  do NOT invent a full task for the agent to execute.\n\
- Conversation State / Pending may only list work the user EXPLICITLY asked for in\n\
  clear natural-language text. Prefer OMIT over guess.\n\
- Image-only or media-marker turns are NOT open tasks unless the user also wrote what\n\
  to do with them.\n\
- Do NOT promote items found only in prior compaction summaries, tool chatter, or\n\
  assistant speculation into active user tasks.\n\
- If uncertain whether work is still open, put a short note under Pending as\n\
  \"[Pending] unclear — user should restate\" rather than inventing steps.\n";

    match existing_summary {
        Some(base) => format!(
            "Below is a PREVIOUS SUMMARY followed by NEW conversation messages.\n\
             \n\
             === PREVIOUS SUMMARY ===\n{base}\n\
             === END PREVIOUS SUMMARY ===\n\
             \n\
             Merge the new messages into the previous summary. Produce a single \n\
             updated summary that covers everything.\n\
             \n\
             Output the summary as plain text with the following sections. \n\
             Include the first section ONLY if there is genuinely unfinished, user-requested work \n\
             that the model MUST actively continue. If the user's last request was completed or \n\
             the topic has moved on, OMIT it entirely.\n\
             \n\
             ## Conversation State (OPTIONAL — omit if nothing is actively in progress)\n\
             Only include genuinely unfinished work the user explicitly asked for. \n\
             Do NOT include completed tasks, background context, or system status here.\n\
             \n\
             ## Key Decisions\n\
             Important choices made and why.\n\
             \n\
             ## Technical Context\n\
             Files modified, code locations, APIs used, configurations changed.\n\
             \n\
             ## Resolved\n\
             Tasks/questions that were completed or answered.\n\
             \n\
             ## Pending\n\
             Tasks/questions still open or deferred.\n\
             \n\
             ## Errors & Fixes\n\
             Problems encountered and their solutions.\n\
             \n\
             ## Evidence Index\n\
             Compact evidence needed for later verification: file paths with line numbers where available, commands run, commit hashes, CI/run IDs, artifact paths, log paths, versions, PIDs, and deployment targets.\n\
             \n\
             Rules:\n\
             - Mark resolved items clearly (prefix with [Resolved])\n\
             - Mark pending items clearly (prefix with [Pending])\n\
             - Omit raw tool output (large code blocks, logs, file contents)\n\
             - Use the same language as the conversation\n\
             - Be thorough but concise: every important detail should be preserved\n\
             - Each fact appears in exactly ONE section only — the most appropriate one. Never repeat the same information across Conversation State / Key Decisions / Resolved / Pending. If something was completed, it belongs in Resolved, not in Conversation State.\n\
             \n\
             {ANTI_FAKE_TASK_RULES}"
        ),
        None => format!(
            "Summarize the conversation history above. This summary will replace \
             the full history, so it MUST preserve all information needed to continue \
             the conversation seamlessly.\n\
             \n\
             Output the summary as plain text.\n\
             \n\
             Required sections:\n\
             1. **Conversation State (OPTIONAL)**: Only include if there is genuinely unfinished, user-requested work in progress. Omit entirely if the user's requests have been completed.\n\
             2. **Key Decisions**: Important choices made and why.\n\
             3. **Technical Context**: Files modified, code locations, APIs used, configurations changed.\n\
             4. **Errors & Fixes**: Problems encountered and their solutions.\n\
             5. **Pending Work**: What still needs to be done. Only user-explicit open work; omit if none.\n\
             6. **Evidence Index**: Compact verification evidence such as file paths with line numbers where available, commands run, commit hashes, CI/run IDs, artifact paths, log paths, versions, PIDs, and deployment targets.\n\
             \n\
             Rules:\n\
             - Omit raw tool output (large code blocks, logs, file dumps) — keep only key facts\n\
             - Use the same language as the conversation\n\
             - Be thorough: losing context means the user has to repeat themselves\n\
             - This conversation has {msg_count} messages to summarize\n\
             \n\
             {ANTI_FAKE_TASK_RULES}"
        ),
    }
}

pub(super) fn audit_summary_quality(to_compact: &[ChatMessage], summary: &str) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();

    if summary.chars().count() < 100 {
        reasons.push(format!(
            "summary too short: {} chars (minimum 100)",
            summary.chars().count()
        ));
    }

    let original_paths = extract_file_paths(to_compact);
    if !original_paths.is_empty() {
        let preserved = original_paths
            .iter()
            .filter(|p| summary.contains(*p))
            .count();
        if preserved == 0 && original_paths.len() <= 5 {
            reasons.push(format!(
                "no file paths preserved (original had {})",
                original_paths.len()
            ));
        }
    }

    (reasons.is_empty(), reasons)
}
