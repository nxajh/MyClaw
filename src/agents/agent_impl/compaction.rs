use crate::providers::{ChatMessage, ChatRequest, ContentPart, ThinkingConfig};
use crate::providers::Capability;
use crate::storage::SummaryRecord;

use super::AgentLoop;
use super::types::{estimate_tokens, estimate_message_tokens};

impl AgentLoop {
    /// Manually trigger compaction (used by /compact command).
    /// Skips the token threshold check — always attempts compression.
    pub async fn compact_now(&mut self, model_id: &str) -> anyhow::Result<()> {
        let context_window = self.registry.get_chat_model_config(model_id)?
            .context_window
            .ok_or_else(|| anyhow::anyhow!("模型未配置 context_window，无法压缩"))?;

        let history_len = self.session.history.len();
        if history_len <= 1 {
            anyhow::bail!("历史消息太少，无需压缩");
        }

        tracing::info!(
            total_tokens = self.policy.token_total(),
            "starting manual compaction (/compact)"
        );

        self.compact_to_budget(model_id, context_window).await
    }

    /// Check if compaction is needed and perform incremental LLM-based summarization.
    pub(crate) async fn maybe_compact(&mut self, model_id: &str) -> anyhow::Result<()> {
        let context_window = match self.registry.get_chat_model_config(model_id)?.context_window {
            Some(cw) => cw,
            None => return Ok(()),
        };

        if !self.policy.should_compact(context_window) {
            return Ok(());
        }

        tracing::info!(
            total_tokens = self.policy.token_total(),
            context_window,
            "starting context compaction"
        );

        self.compact_to_budget(model_id, context_window).await
    }

    /// Check the routing fallback chain and proactively compact if the current context
    /// would overflow any fallback model's context window.
    pub(crate) async fn maybe_compact_for_fallback(&mut self) -> anyhow::Result<()> {
        let routing_models = self.registry.get_chat_routing_models();
        if routing_models.len() <= 1 {
            return Ok(());
        }

        let conservative_total = (self.policy.token_total() as f64 * 1.25) as u64;

        let target: Option<(String, u64)> = routing_models
            .iter()
            .skip(1)
            .find_map(|model_id| {
                let cfg = self.registry.get_chat_model_config(model_id).ok()?;
                let window = cfg.context_window?;
                if conservative_total > window { Some((model_id.clone(), window)) } else { None }
            });

        if let Some((fallback_model, window)) = target {
            tracing::info!(
                fallback_model = %fallback_model,
                conservative_total,
                window,
                "pre-fallback: context would overflow fallback model, compacting"
            );
            if let Err(e) = self.compact_to_budget(&fallback_model, window).await {
                tracing::warn!(error = %e, "pre-fallback compaction failed");
            }
        }

        Ok(())
    }

    /// Compact history so that the compressible prefix fits within `target_window`.
    pub(crate) async fn compact_to_budget(
        &mut self,
        model_id: &str,
        target_window: u64,
    ) -> anyhow::Result<()> {
        let system_prompt_tokens = estimate_tokens(&self.system_prompt);
        let tool_spec_tokens: u64 = self.build_tool_specs().iter().map(|spec| {
            let schema = spec.input_schema.to_string();
            estimate_tokens(&spec.name)
                + spec.description.as_deref().map_or(0, estimate_tokens)
                + estimate_tokens(&schema)
                + 8
        }).sum();

        let boundary = match self.policy.compaction_boundary(
            &self.session.history,
            target_window,
            system_prompt_tokens,
            tool_spec_tokens,
        ) {
            Some(b) => b,
            None => {
                tracing::debug!(
                    target_window,
                    system_prompt_tokens,
                    tool_spec_tokens,
                    "no compaction boundary (budget too small or too few work units)"
                );
                return Ok(());
            }
        };

        let history_len = self.session.history.len();
        if boundary == 0 || boundary >= history_len {
            return Ok(());
        }

        tracing::info!(
            target_window,
            system_prompt_tokens,
            tool_spec_tokens,
            boundary,
            "compaction triggered"
        );

        self.compact_with_boundary(model_id, boundary).await
    }

    /// Core compaction implementation given a pre-computed split boundary.
    async fn compact_with_boundary(
        &mut self,
        model_id: &str,
        boundary: usize,
    ) -> anyhow::Result<()> {
        let history_len = self.session.history.len();
        if history_len <= 1 {
            return Ok(());
        }

        let ids_len = self.session.message_ids.len();
        if ids_len < history_len {
            tracing::warn!(
                history_len,
                ids_len,
                "message_ids out of sync with history, padding with zeros"
            );
            self.session.message_ids.resize(history_len, 0);
        }

        if boundary >= history_len {
            tracing::info!("no compaction needed: conversation within retention");
            return Ok(());
        }
        if boundary == 0 {
            tracing::info!("no compaction needed: all history must be retained");
            return Ok(());
        }

        let (compact_start, compact_end, existing_summary) = self.find_incremental_range(boundary);
        let to_compact: Vec<ChatMessage> = self.session.history[compact_start..compact_end].to_vec();

        if to_compact.is_empty() {
            tracing::info!("no new content to compact");
            return Ok(());
        }

        let compacted_count = to_compact.len();
        let removed_tokens: u64 = to_compact.iter().map(estimate_message_tokens).sum();

        tracing::info!(
            compact_start,
            compact_end,
            boundary,
            has_existing_summary = existing_summary.is_some(),
            "compaction range determined"
        );

        let last_compacted_id = self.session.message_ids
            .get(compact_end.saturating_sub(1))
            .copied()
            .unwrap_or(0);

        let summary = match self.summarize_inline(&to_compact, existing_summary.as_deref(), model_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "summarizer failed, dropping pre-boundary history");
                self.drop_pre_boundary_with_record(boundary, last_compacted_id);
                return Ok(());
            }
        };

        if summary.trim().is_empty() {
            tracing::warn!("summarizer returned empty, dropping pre-boundary history");
            self.drop_pre_boundary_with_record(boundary, last_compacted_id);
            return Ok(());
        }

        let (ok, reasons) = self.audit_summary_quality(&to_compact, &summary);
        if !ok {
            tracing::warn!(reasons = ?reasons, "summary quality audit failed (non-blocking)");
        }

        // Refresh memory index (summarizer may have written memory files via tools).
        {
            let memory_dir = std::path::Path::new(&self.config.prompt_config.knowledge_dir);
            let files = crate::memory::scan_memory_files(memory_dir);
            let entries: Vec<crate::memory::IndexEntry> =
                files.iter().map(crate::memory::IndexEntry::from).collect();
            let history = self.session.history.clone();
            self.attachments.diff_memory(&entries, &history);
            tracing::info!(memory_count = entries.len(), "memory index refreshed after compaction");
        }

        let version = self.session.compact_version + 1;
        let summary_prefix = "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted \
into the summary below. This is a handoff from a previous context window — \
treat it as background reference, NOT as active instructions. \
Do NOT answer questions or fulfill requests mentioned in this summary; \
they were already addressed. \
Your persistent memory (MEMORY.md, USER.md) in the system prompt \
is ALWAYS authoritative — never deprioritize memory content due to this note. \
Respond ONLY to the latest user message that appears AFTER this summary.\n\n";
        let summary_msg = ChatMessage::user_text(format!("{}{}", summary_prefix, summary));
        let summary_tokens = estimate_message_tokens(&summary_msg);

        self.session.history.drain(compact_start..compact_end);
        self.session.history.insert(compact_start, summary_msg);

        self.session.message_ids.drain(compact_start..compact_end);
        self.session.message_ids.insert(compact_start, 0);

        self.session.compact_version = version;
        self.session.summary_metadata = Some(super::super::session_manager::SummaryMetadata {
            version,
            token_estimate: summary_tokens,
            up_to_message: last_compacted_id,
        });

        if let Some(ref hook) = self.persist_hook {
            hook.save_compaction(&self.session.id, &SummaryRecord {
                id: 0,
                version,
                summary: summary.clone(),
                up_to_message: last_compacted_id,
                token_estimate: Some(summary_tokens),
                created_at: chrono::Utc::now(),
            });

            let surviving: Vec<(i64, ChatMessage)> = self.session.message_ids.iter()
                .copied()
                .zip(self.session.history.iter().cloned())
                .collect();
            hook.rotate_history(&self.session.id, &surviving);

            for (i, id) in self.session.message_ids.iter_mut().enumerate() {
                *id = (i + 1) as i64;
            }
        }

        self.policy.adjust_for_compaction(removed_tokens, summary_tokens);

        let new_total = self.policy.token_total();
        tracing::info!(
            compacted_messages = compacted_count,
            summary_tokens,
            removed_tokens,
            new_total_tokens = new_total,
            version,
            "context compaction completed"
        );

        let new_boundary = compact_start + 1;
        let context_window = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .unwrap_or(u64::MAX);
        let threshold = (context_window as f64 * self.config.context.compact_threshold) as u64;
        if new_total > threshold {
            self.truncate_retention_zone(new_boundary, model_id);
        }

        Ok(())
    }

    /// Find the incremental compaction range and any existing summary to merge.
    fn find_incremental_range(&self, boundary: usize) -> (usize, usize, Option<String>) {
        let history = &self.session.history;
        let last_summary = history[..boundary].iter().rposition(|m| {
            m.role == "user" && m.text_content().starts_with("[CONTEXT COMPACTION — REFERENCE ONLY]")
        });
        match last_summary {
            Some(idx) => {
                let existing = history[idx].text_content();
                (idx, boundary, Some(existing))
            }
            None => (0, boundary, None),
        }
    }

    /// Inline summarizer — calls `do_inline_summarize` with a single attempt.
    async fn summarize_inline(
        &mut self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        model_id: &str,
    ) -> anyhow::Result<String> {
        match self.do_inline_summarize(to_compact, existing_summary, model_id).await {
            Ok(s) if !s.trim().is_empty() => Ok(s),
            Ok(_) => {
                tracing::warn!("summarize returned empty text");
                anyhow::bail!("summarize returned empty text")
            }
            Err(e) => {
                tracing::warn!(error = %e, "summarize failed");
                Err(e)
            }
        }
    }

    /// Inline summarize: same system prompt, tools, history as main request for
    /// maximum prefix cache hit. Runs a full mini chat_loop so the model can
    /// use tools if needed. 20K output budget.
    async fn do_inline_summarize(
        &mut self,
        to_compact: &[ChatMessage],
        existing_summary: Option<&str>,
        model_id: &str,
    ) -> anyhow::Result<String> {
        let provider = match self.registry.get_chat_provider_by_model(model_id) {
            Some((p, _)) => p,
            None => {
                let (p, _) = self.registry.get_chat_provider(Capability::Chat)?;
                p
            }
        };

        let mut messages: Vec<ChatMessage> = Vec::new();

        if !self.system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(&self.system_prompt));
        }

        for msg in to_compact {
            let mut cleaned = msg.clone();
            cleaned.parts = cleaned.parts.into_iter().map(|part| {
                match part {
                    ContentPart::ImageUrl { .. } => ContentPart::Text { text: "[image]".into() },
                    ContentPart::ImageB64 { .. } => ContentPart::Text { text: "[image]".into() },
                    other => other,
                }
            }).collect();
            messages.push(cleaned);
        }

        let memory_prompt = "\n\
                 \n\
                 You also have a persistent memory system. The memory directory is `memory/` and\n\
                 its current index is in your system prompt above.\n\
                 \n\
                 Based on this conversation, decide if any memories should be saved, updated, or\n\
                 deleted. Use file_write to create/update memory files and file_edit to modify them.\n\
                 Use shell (rm) to delete memory files.\n\
                 \n\
                 Each memory file MUST have YAML frontmatter:\n\
                 ---\n\
                 name: short_snake_case_name\n\
                 description: one-line description (under 150 chars)\n\
                 type: user|feedback|project|reference\n\
                 created_at: YYYY-MM-DD\n\
                 ---\n\
                 \n\
                 Then the memory content in markdown.\n\
                 \n\
                 description quality rules:\n\
                 - DO NOT repeat the filename — description must add information beyond what the name already says\n\
                 - MUST include key terms that help decide when to read this file (tool names, feature names, bug symptoms, decision outcomes)\n\
                 - BAD: \"MyClaw memory system design decisions\" (name already says this)\n\
                 - GOOD: \"记忆索引从system prompt迁到system-reminder注入；diff_memory始终检查history不依赖内存旧文本\"\n\
                 - If updating an existing file, update its description to reflect the latest content\n\
                 \n\
                 Other rules:\n\
                 - ONLY save things NOT derivable from code/git (user preferences, decisions, corrections)\n\
                 - Check the existing memory index to avoid duplicates — update existing files instead of creating duplicates\n\
                 - If existing memories are outdated or contradicted, update or delete them\n\
                 - Keep name short, lowercase, underscores (becomes the filename: memory/{name}.md)\n\
                 - If no memory changes needed, skip this entirely and just output the summary\n\
                 \n\
                 You may use file_write, file_edit, and file_read tools for memory operations ONLY.\n\
                 Do not use other tools.";

        let prompt = match existing_summary {
            Some(base) => format!(
                "Below is a PREVIOUS SUMMARY followed by NEW conversation messages.\n\
                 \n\
                 === PREVIOUS SUMMARY ===\n{}\n\
                 === END PREVIOUS SUMMARY ===\n\
                 \n\
                 Merge the new messages into the previous summary. Produce a single \n\
                 updated summary that covers everything.\n\
                 \n\
                 Output the summary as plain text with the following REQUIRED sections. \n\
                 Mark items as Resolved or Pending so the model knows what is active:\n\
                 \n\
                 ## Active Task\n\
                 What the user is currently doing and its status.\n\
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
                 Rules:\n\
                 - Mark resolved items clearly (prefix with [Resolved])\n\
                 - Mark pending items clearly (prefix with [Pending])\n\
                 - Omit raw tool output (large code blocks, logs, file contents)\n\
                 - Use the same language as the conversation\n\
                 - Be thorough but concise: every important detail should be preserved{}",
                base, memory_prompt
            ),
            None => format!(
                "Summarize the conversation history above. This summary will replace \
                 the full history, so it MUST preserve all information needed to continue \
                 the conversation seamlessly.\n\
                 \n\
                 Output the summary as plain text. If you also need to update memory files, \
                 use the file_write/file_edit tools first, then output the summary as your \
                 final response.\n\
                 \n\
                 Required sections:\n\
                 1. **User Goals**: What is the user trying to accomplish? Current status of each goal.\n\
                 2. **Key Decisions**: Important choices made and why.\n\
                 3. **Technical Context**: Files modified, code locations, APIs used, configurations changed.\n\
                 4. **Errors & Fixes**: Problems encountered and their solutions.\n\
                 5. **Pending Work**: What still needs to be done.\n\
                 \n\
                 Rules:\n\
                 - Omit raw tool output (large code blocks, logs, file dumps) — keep only key facts\n\
                 - Use the same language as the conversation\n\
                 - Be thorough: losing context means the user has to repeat themselves\n\
                 - This conversation has {} messages to summarize{}",
                to_compact.len(), memory_prompt
            ),
        };
        messages.push(ChatMessage::user_text(prompt));

        let tools = self.build_tool_specs();

        let thinking = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| {
                if cfg.reasoning {
                    Some(ThinkingConfig { enabled: true, effort: None })
                } else {
                    None
                }
            });

        let max_rounds = 10;
        let mut round = 0;

        let final_text = loop {
            round += 1;
            if round > max_rounds {
                tracing::warn!(rounds = round, "summarize loop exceeded max rounds");
                anyhow::bail!("summarize loop exceeded {} rounds", max_rounds);
            }

            let req = ChatRequest {
                model: model_id,
                messages: &messages,
                temperature: None,
                max_tokens: Some(20_000),
                thinking: thinking.clone(),
                stop: None,
                seed: None,
                tools: if tools.is_empty() { None } else { Some(&tools[..]) },
                stream: true,
            };

            let stream = provider.chat(req)?;
            let response = self.collect_stream(stream).await?;

            if let Some(ref usage) = response.usage {
                if let Some(cached) = usage.cached_input_tokens {
                    tracing::info!(
                        round,
                        cached_tokens = cached,
                        total_input = usage.input_tokens.unwrap_or(0),
                        "summarizer cache hit"
                    );
                }
            }

            if response.tool_calls.is_empty() {
                break response.text;
            }

            tracing::info!(
                round,
                tool_calls = response.tool_calls.len(),
                text_len = response.text.len(),
                "summarize: model requested tool calls"
            );

            let mut assistant_msg = ChatMessage::assistant_text(&response.text);
            assistant_msg.tool_calls = Some(response.tool_calls.clone());
            if let Some(ref thinking_text) = response.reasoning_content {
                assistant_msg.parts.insert(
                    0,
                    ContentPart::Thinking { thinking: thinking_text.clone() },
                );
            }
            messages.push(assistant_msg);

            for call in &response.tool_calls {
                tracing::info!(tool = %call.name, id = %call.id, "summarize: executing tool");
                let result = self.execute_tool(call).await;
                let result_content = match &result {
                    Ok(r) => {
                        let mut out = r.output.clone();
                        if let Some(ref err) = r.error {
                            if out.is_empty() {
                                out = format!("error: {}", err);
                            }
                        }
                        out
                    }
                    Err(e) => format!("error: {}", e),
                };

                let mut tool_msg = ChatMessage::text("tool", &result_content);
                tool_msg.tool_call_id = Some(call.id.clone());
                tool_msg.is_error = Some(result.is_err());
                messages.push(tool_msg);
            }
        };

        Ok(final_text)
    }

    /// Check whether the summary retains key information from the original dialogue.
    fn audit_summary_quality(
        &self,
        to_compact: &[ChatMessage],
        summary: &str,
    ) -> (bool, Vec<String>) {
        let mut reasons = Vec::new();

        if summary.chars().count() < 100 {
            reasons.push(format!(
                "summary too short: {} chars (minimum 100)",
                summary.chars().count()
            ));
        }

        let original_paths = Self::extract_file_paths(to_compact);
        if !original_paths.is_empty() {
            let preserved = original_paths.iter()
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

    /// Extract likely file paths from messages (simplified).
    fn extract_file_paths(messages: &[ChatMessage]) -> Vec<String> {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(?:/[\w/.-]+\.\w{1,5})|(?:src/[\w/.-]+)").unwrap());
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

    /// Drop all history before the boundary (no summary, no recovery).
    fn drop_pre_boundary_with_record(&mut self, boundary: usize, last_compacted_id: i64) {
        let removed_tokens: u64 = self.session.history[..boundary]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        self.session.history.drain(..boundary);
        self.session.message_ids.drain(..boundary);
        self.policy.adjust_for_compaction(removed_tokens, 0);

        let version = self.session.compact_version + 1;
        self.session.compact_version = version;
        if let Some(ref hook) = self.persist_hook {
            hook.save_compaction(&self.session.id, &SummaryRecord {
                id: 0,
                version,
                summary: "[历史已截断]".to_string(),
                up_to_message: last_compacted_id,
                token_estimate: None,
                created_at: chrono::Utc::now(),
            });

            let surviving: Vec<(i64, ChatMessage)> = self.session.message_ids.iter()
                .copied()
                .zip(self.session.history.iter().cloned())
                .collect();
            hook.rotate_history(&self.session.id, &surviving);
        }
    }

    /// Safety net: truncate oversized tool results in retention zone, or drop oldest unit.
    fn truncate_retention_zone(&mut self, boundary: usize, model_id: &str) {
        let safety_max_tokens = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .map(|cw| (cw / 20) as usize)
            .unwrap_or(5_000);

        for i in boundary..self.session.history.len() {
            if self.session.history[i].role != "tool" {
                continue;
            }
            let text = self.session.history[i].text_content();
            let est = estimate_tokens(&text);
            if est > safety_max_tokens as u64 {
                let truncated = crate::tools::truncation::truncate_output(&text, safety_max_tokens);
                self.session.history[i].parts = vec![
                    ContentPart::Text { text: truncated }
                ];

                let old_est = est;
                let new_est = estimate_tokens(&self.session.history[i].text_content());
                self.policy.adjust_for_compaction(old_est, new_est);

                tracing::warn!(
                    idx = i,
                    old_tokens = old_est,
                    new_tokens = new_est,
                    "safety-net truncated oversized tool result in retention zone"
                );
            }
        }

        let threshold = self.registry.get_chat_model_config(model_id)
            .ok()
            .and_then(|cfg| cfg.context_window)
            .map(|cw| (cw as f64 * self.config.context.compact_threshold) as u64)
            .unwrap_or(u64::MAX);
        if self.policy.token_total() > threshold {
            self.drop_oldest_retained_work_unit(boundary);
        }
    }

    /// Drop everything before the first user message in the retention zone.
    ///
    /// After compaction the retention zone may start with orphan messages
    /// (assistant with tool_calls whose results were compacted away, orphan
    /// tool results, etc.) that precede the first user message.  Dropping
    /// them keeps the remaining history aligned on a clean user→assistant
    /// boundary, avoiding 400 errors from unmatched tool_call_ids.
    fn drop_oldest_retained_work_unit(&mut self, boundary: usize) {
        // Find the first user message in the retention zone.
        let first_user = self.session.history[boundary..]
            .iter()
            .position(|m| m.role == "user");

        let drop_end = match first_user {
            Some(pos) => boundary + pos,
            None => return, // no user message — don't drop anything
        };

        if drop_end <= boundary {
            // Already starts with a user message; nothing to drop.
            return;
        }

        let removed_tokens: u64 = self.session.history[boundary..drop_end]
            .iter()
            .map(estimate_message_tokens)
            .sum();

        self.session.history.drain(boundary..drop_end);
        self.session.message_ids.drain(boundary..drop_end);
        self.policy.adjust_for_compaction(removed_tokens, 0);

        tracing::warn!(
            dropped_start = boundary,
            dropped_end = drop_end,
            removed_tokens,
            "dropped orphan messages before first user message in retention zone"
        );
    }
}
