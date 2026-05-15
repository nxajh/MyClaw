use crate::providers::ChatMessage;
use crate::providers::ContentPart;
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
        let system_prompt_tokens = self.request_builder.system_prompt_tokens();
        let tool_specs = self.build_tool_specs();
        let tool_spec_tokens: u64 = tool_specs.iter().map(|spec| {
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

        self.compact_with_boundary(model_id, boundary, &tool_specs).await
    }

    /// Core compaction implementation given a pre-computed split boundary.
    ///
    /// Calls CompactionExecutor (read-only history view) to generate the summary,
    /// then applies the result to session state.
    async fn compact_with_boundary(
        &mut self,
        model_id: &str,
        boundary: usize,
        tool_specs: &[crate::providers::capability_chat::ToolSpec],
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

        let last_compacted_id = self.session.message_ids
            .get(boundary.saturating_sub(1))
            .copied()
            .unwrap_or(0);

        let system_prompt = self.request_builder.system_prompt().to_string();

        let result = self.compactor.execute(
            &self.session.history,
            &system_prompt,
            tool_specs,
            boundary,
            model_id,
        ).await;

        let result = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "summarizer failed, dropping pre-boundary history");
                self.drop_pre_boundary_with_record(boundary, last_compacted_id);
                return Ok(());
            }
        };

        if result.summary.trim().is_empty() {
            tracing::warn!("summarizer returned empty, dropping pre-boundary history");
            self.drop_pre_boundary_with_record(boundary, last_compacted_id);
            return Ok(());
        }

        // Refresh memory index (summarizer may have written memory files via tools).
        {
            let memory_dir = std::path::Path::new(&self.request_builder.resources.knowledge_dir);
            let files = crate::memory::scan_memory_files(memory_dir);
            let entries: Vec<crate::memory::IndexEntry> =
                files.iter().map(crate::memory::IndexEntry::from).collect();
            let history = self.session.history.clone();
            self.request_builder.attachments.diff_memory(&entries, &history);
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
        let summary_msg = ChatMessage::user_text(format!("{}{}", summary_prefix, result.summary));

        self.session.apply_compaction(
            result.compact_start,
            result.compact_end,
            summary_msg,
            version,
            last_compacted_id,
            result.summary_tokens,
        );

        let compact_start = result.compact_start;

        if let Some(ref hook) = self.persist_hook {
            hook.save_compaction(&self.session.id, &SummaryRecord {
                id: 0,
                version,
                summary: result.summary.clone(),
                up_to_message: last_compacted_id,
                token_estimate: Some(result.summary_tokens),
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

        self.policy.adjust_for_compaction(result.removed_tokens, result.summary_tokens);

        let new_total = self.policy.token_total();
        tracing::info!(
            compacted_messages = result.compacted_count,
            summary_tokens = result.summary_tokens,
            removed_tokens = result.removed_tokens,
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

    /// Drop all history before the boundary (no summary, no recovery).
    fn drop_pre_boundary_with_record(&mut self, boundary: usize, last_compacted_id: i64) {
        let removed_tokens: u64 = self.session.history[..boundary]
            .iter()
            .map(estimate_message_tokens)
            .sum();

        let version = self.session.compact_version + 1;
        self.session.drop_pre_boundary(boundary, version);
        self.policy.adjust_for_compaction(removed_tokens, 0);

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
    fn drop_oldest_retained_work_unit(&mut self, boundary: usize) {
        let first_user = self.session.history[boundary..]
            .iter()
            .position(|m| m.role == "user");

        let drop_end = match first_user {
            Some(pos) => boundary + pos,
            None => return,
        };

        if drop_end <= boundary {
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
