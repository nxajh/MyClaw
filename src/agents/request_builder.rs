use std::sync::Arc;

use tokio::sync::watch;

use crate::config::agent::AutonomyLevel;
use crate::providers::ChatMessage;
use super::attachment::AttachmentManager;
use super::resource_provider::ResourceProvider;
use super::session_manager::Session;
use super::workspace::watcher::ChangeSet;
use super::agent_impl::types::estimate_tokens;

/// Owns message construction, attachment management, image state, and hot-reload polling.
///
/// One RequestBuilder per AgentLoop (not Arc-shared), so all methods take &mut self.
pub(crate) struct RequestBuilder {
    pub(crate) system_prompt: String,
    pub(crate) attachments: AttachmentManager,
    pub(crate) resources: Arc<ResourceProvider>,
    /// Pending images for the current turn (set by set_images, read in attach_images).
    pub(crate) pending_image_urls: Option<Vec<String>>,
    pub(crate) pending_image_base64: Option<Vec<String>>,
    /// File-change watcher receiver. None for sub-agents (no hot-reload).
    pub(crate) change_rx: Option<watch::Receiver<ChangeSet>>,
}

impl RequestBuilder {
    pub(crate) fn new(system_prompt: String, resources: Arc<ResourceProvider>) -> Self {
        Self {
            system_prompt,
            attachments: AttachmentManager::new(),
            resources,
            pending_image_urls: None,
            pending_image_base64: None,
            change_rx: None,
        }
    }

    /// Hot-reload check + all attachment diffs. Call once per turn before adding the user message.
    ///
    /// Mutates skills/sub_agents RwLocks on change, then updates attachment deltas.
    pub(crate) fn refresh(&mut self, session: &Session) {
        self.check_changes(session);
        self.run_diffs(session);
    }

    fn check_changes(&mut self, session: &Session) {
        let rx = match self.change_rx.as_mut() {
            Some(rx) => rx,
            None => return,
        };

        while rx.has_changed().unwrap_or(false) {
            let changes = rx.borrow_and_update().clone();

            if changes.skills_changed {
                let new_defs = super::workspace::skill_loader::load_skills_from_dir(&self.resources.skills_dir);
                let new_skills: Vec<super::workspace::skills::Skill> =
                    new_defs.iter().map(super::workspace::skills::Skill::from_definition).collect();
                {
                    let mut skills = self.resources.skills.write();
                    skills.reload(new_skills);
                }
                let skills = self.resources.skills.read();
                self.attachments.diff_skills(&skills, &session.history);
                tracing::info!(skill_count = skills.skill_count(), "skills hot-reloaded");
            }

            if changes.agents_changed {
                let new_agents = super::workspace::agent_loader::load_agents_from_dir(&self.resources.agents_dir);
                let agent_list: Vec<(String, String)> = new_agents
                    .iter()
                    .map(|a| (a.name.clone(), a.description.clone().unwrap_or_default()))
                    .collect();
                {
                    let mut configs = self.resources.sub_agents.write();
                    *configs = new_agents;
                }
                self.attachments.diff_agents(&agent_list, &session.history);
                tracing::info!(agent_count = agent_list.len(), "agents hot-reloaded");
            }

            if changes.memory_changed {
                let memory_dir = std::path::Path::new(&self.resources.knowledge_dir);
                let files = crate::memory::scan_memory_files(memory_dir);
                let entries: Vec<crate::memory::IndexEntry> =
                    files.iter().map(crate::memory::IndexEntry::from).collect();
                self.attachments.diff_memory(&entries, &session.history);
                tracing::info!(memory_count = entries.len(), "memory hot-reloaded");
            }
        }
    }

    fn run_diffs(&mut self, session: &Session) {
        let history = &session.history;
        {
            let skills = self.resources.skills.read();
            self.attachments.diff_skills(&skills, history);
        }
        {
            let configs = self.resources.sub_agents.read();
            let agent_list: Vec<(String, String)> = configs
                .iter()
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

    /// Merge pending attachment text into the user message. Returns the combined string.
    /// Call clear_pending() after the combined text has been added to session.
    pub(crate) fn merge_attachments(&self, user_text: &str) -> String {
        let skills = self.resources.skills.read();
        match self.attachments.build_text(&skills) {
            Some(reminder) => format!("{}\n\n{}", reminder, user_text),
            None => user_text.to_string(),
        }
    }

    /// Settle pending attachment deltas (call after merge_attachments text is persisted).
    pub(crate) fn clear_pending(&mut self) {
        self.attachments.clear_pending();
    }

    /// Build the full message list: system prompt + history (with sanitize_history applied).
    pub(crate) fn build(&self, session: &Session) -> Vec<ChatMessage> {
        let mut messages = Vec::with_capacity(session.history.len() + 1);
        if !self.system_prompt.is_empty() {
            messages.push(ChatMessage::system_text(&self.system_prompt));
        }
        messages.extend(session.history.iter().cloned());
        super::session_manager::sanitize_history(&mut messages);
        messages
    }

    /// Store pending images for the current turn.
    pub(crate) fn set_images(
        &mut self,
        urls: Option<Vec<String>>,
        b64: Option<Vec<String>>,
    ) {
        self.pending_image_urls = urls;
        self.pending_image_base64 = b64;
    }

    pub(crate) fn has_images(&self) -> bool {
        self.pending_image_urls.as_ref().is_some_and(|v| !v.is_empty())
            || self.pending_image_base64.as_ref().is_some_and(|v| !v.is_empty())
    }

    /// Read-only reference to pending image URLs.
    pub(crate) fn image_urls(&self) -> Option<&Vec<String>> {
        self.pending_image_urls.as_ref()
    }

    /// Read-only reference to pending base64 images.
    pub(crate) fn image_b64(&self) -> Option<&Vec<String>> {
        self.pending_image_base64.as_ref()
    }

    /// Read-only access to the system prompt.
    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Proxy for diff_autonomy (used by apply_session_override in AgentLoop).
    pub(crate) fn diff_autonomy(&mut self, autonomy: &AutonomyLevel) {
        self.attachments.diff_autonomy(autonomy);
    }

    /// Debug helper: list pending attachment kinds.
    pub(crate) fn pending_keys(&self) -> Vec<&'static str> {
        self.attachments.pending_keys()
    }

    /// Estimate tokens for the system prompt (for compaction budget).
    pub(crate) fn system_prompt_tokens(&self) -> u64 {
        estimate_tokens(&self.system_prompt)
    }

    /// Set the change receiver (called by AgentLoop::with_change_rx).
    pub(crate) fn set_change_rx(&mut self, rx: watch::Receiver<ChangeSet>) {
        self.change_rx = Some(rx);
    }
}
