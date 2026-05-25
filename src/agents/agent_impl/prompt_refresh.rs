//! Prompt + attachment refresh methods for AgentLoop.
//!
//! Extracted from the deleted RequestBuilder (H46). These methods handle
//! hot-reload polling, attachment diffing, message building, and image state.

use super::types::estimate_tokens;

impl super::AgentLoop {
    /// Hot-reload check + all attachment diffs. Call once per turn before adding the user message.
    pub(crate) fn refresh_attachments(&mut self) {
        self.check_changes();
        self.run_diffs();
    }

    pub(super) fn check_changes(&mut self) {
        let rx = match self.change_rx.as_mut() {
            Some(rx) => rx,
            None => return,
        };

        while rx.has_changed().unwrap_or(false) {
            let changes = rx.borrow_and_update().clone();

            if changes.skills_changed {
                let new_defs = super::super::workspace::skill_loader::load_skills_from_dir(&self.resources.skills_dir);
                let new_skills: Vec<super::super::workspace::skills::Skill> =
                    new_defs.iter().map(super::super::workspace::skills::Skill::from_definition).collect();
                {
                    let mut skills = self.resources.skills.write();
                    skills.reload(new_skills);
                }
                let skills = self.resources.skills.read();
                self.attachments.diff_skills(&skills, &self.session.history);
                tracing::info!(skill_count = skills.skill_count(), "skills hot-reloaded");
            }

            if changes.agents_changed {
                self.resources.sub_agents.reload_from_dir(&self.resources.agents_dir);
                let agent_list: Vec<(String, String)> = self
                    .resources
                    .sub_agents
                    .values_cloned()
                    .into_iter()
                    .map(|a| (a.name.clone(), a.description.clone().unwrap_or_default()))
                    .collect();
                self.attachments.diff_agents(&agent_list, &self.session.history);
                tracing::info!(agent_count = agent_list.len(), "agents hot-reloaded");
            }

            if changes.memory_changed {
                let memory_dir = std::path::Path::new(&self.resources.knowledge_dir);
                let files = crate::memory::scan_memory_files(memory_dir);
                let entries: Vec<crate::memory::IndexEntry> =
                    files.iter().map(crate::memory::IndexEntry::from).collect();
                self.attachments.diff_memory(&entries, &self.session.history);
                tracing::info!(memory_count = entries.len(), "memory hot-reloaded");
            }
        }
    }

    pub(super) fn run_diffs(&mut self) {
        let history = &self.session.history;
        {
            let skills = self.resources.skills.read();
            self.attachments.diff_skills(&skills, history);
        }
        {
            let agent_list: Vec<(String, String)> = self
                .resources
                .sub_agents
                .values_cloned()
                .into_iter()
                .map(|a| (a.name.clone(), a.description.clone().unwrap_or_default()))
                .collect();
            if !agent_list.is_empty() {
                self.attachments.diff_agents(&agent_list, history);
            }
        }
        if !self.resources.mcp_instructions.is_empty() {
            self.attachments.diff_mcp(&self.resources.mcp_instructions, history);
        }
        {
            let memory_dir = std::path::Path::new(&self.resources.knowledge_dir);
            let files = crate::memory::scan_memory_files(memory_dir);
            let entries: Vec<crate::memory::IndexEntry> =
                files.iter().map(crate::memory::IndexEntry::from).collect();
            self.attachments.diff_memory(&entries, history);
        }
        self.attachments.diff_date(self.resources.timezone_offset, history);
    }

    /// Merge pending attachment text into the user message.
    pub(crate) fn merge_attachments(&self, user_text: &str) -> String {
        let skills = self.resources.skills.read();
        match self.attachments.build_text(&skills) {
            Some(reminder) => format!("{}\n\n{}", reminder, user_text),
            None => user_text.to_string(),
        }
    }

    /// Settle pending attachment deltas.
    pub(crate) fn clear_pending_attachments(&mut self) {
        self.attachments.clear_pending();
    }

    /// Build the full message list: system prompt + sanitized history.
    pub(crate) fn build_messages(&self) -> Vec<crate::providers::ChatMessage> {
        let mut messages = Vec::with_capacity(self.session.history.len() + 1);
        if !self.system_prompt.is_empty() {
            messages.push(crate::providers::ChatMessage::system_text(&self.system_prompt));
        }
        messages.extend(self.session.history.iter().cloned());
        super::super::session::sanitize_history(&mut messages);
        messages
    }

    /// Store pending images for the current turn.
    pub(crate) fn set_images(&mut self, urls: Option<Vec<String>>, b64: Option<Vec<String>>) {
        self.pending_image_urls = urls;
        self.pending_image_base64 = b64;
    }

    pub(crate) fn has_images(&self) -> bool {
        self.pending_image_urls.as_ref().is_some_and(|v| !v.is_empty())
            || self.pending_image_base64.as_ref().is_some_and(|v| !v.is_empty())
    }

    pub(crate) fn image_urls(&self) -> Option<&Vec<String>> {
        self.pending_image_urls.as_ref()
    }

    pub(crate) fn image_b64(&self) -> Option<&Vec<String>> {
        self.pending_image_base64.as_ref()
    }

    pub(crate) fn system_prompt_tokens(&self) -> u64 {
        estimate_tokens(&self.system_prompt)
    }

    pub(crate) fn diff_autonomy(&mut self, autonomy: &crate::config::agent::PermissionMode) {
        self.attachments.diff_autonomy(autonomy);
    }

    pub(crate) fn pending_keys(&self) -> Vec<&'static str> {
        self.attachments.pending_keys()
    }
}
