//! Token estimation + tracking utilities.
//!
//! Extracted from `agent_impl::types` so that the types Session and
//! ContextEngine depend on (TokenTracker, estimate_*, is_write_tool)
//! survive the H45 deletion of `agent_impl`.

use crate::providers::media::{
    marker_for_file, modality_from_mime, resolve_path, MediaInlineDecision, MediaPolicy,
};
use crate::providers::{ChatMessage, ContentPart};

/// Estimate token count from text length.
///
/// CJK characters (Chinese, Japanese, Korean) typically map to ~1 token each
/// in BPE tokenizers, but occupy 3 UTF-8 bytes — so a naive `bytes / 4`
/// underestimates them by ~1.3–2.7×. We split the count: CJK chars count as
/// 1 token each, remaining bytes use the standard `bytes / 4` heuristic.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other_bytes = 0u64;
    for ch in text.chars() {
        let code = ch as u32;
        if (0x4E00..=0x9FFF).contains(&code)
            || (0x3400..=0x4DBF).contains(&code)
            || (0x3040..=0x30FF).contains(&code)
            || (0xAC00..=0xD7AF).contains(&code)
            || (0xFF00..=0xFFEF).contains(&code)
        {
            cjk += 1;
        } else {
            other_bytes += ch.len_utf8() as u64;
        }
    }
    cjk + other_bytes.div_ceil(4)
}

/// Resolve on-disk size for a media path, preferring the part's recorded size.
fn resolve_file_size(path: &str, size_bytes: Option<u64>) -> Option<u64> {
    if let Some(n) = size_bytes {
        return Some(n);
    }
    std::fs::metadata(resolve_path(path))
        .map(|m| m.len())
        .ok()
}

/// Estimate tokens for a `ContentPart::File` as it will appear after media lowering.
///
/// - [`MediaInlineDecision::Inline`]: base64 payload cost (`size/3` + framing).
/// - [`MediaInlineDecision::Marker`] or no policy: short path marker only.
///
/// When `policy` is `None`, files are counted as markers. Compaction must pass the
/// current model's [`MediaPolicy`] so vision models still get the heavy estimate.
pub fn estimate_file_part_tokens(
    path: &str,
    mime_type: Option<&str>,
    size_bytes: Option<u64>,
    policy: Option<MediaPolicy>,
) -> u64 {
    let size = resolve_file_size(path, size_bytes);
    let decision = match policy {
        Some(p) => {
            let modality = modality_from_mime(mime_type, path);
            p.decision_for(modality, size)
        }
        // Unknown model/policy: assume marker (text-only primary path). Counting
        // full base64 here was what made a single QQ screenshot blow past 200k
        // and force hard-fold even though glm-5.2 never receives the bytes.
        None => MediaInlineDecision::Marker(
            crate::providers::media::MediaMarkerReason::ModelUnsupported,
        ),
    };

    match decision {
        MediaInlineDecision::Inline => {
            // Base64 expands by 4/3; at ~4 bytes/token that's `file_bytes / 3`
            // for the payload alone, plus path/framing overhead.
            let path_tokens = estimate_tokens(path);
            let media_tokens = size.unwrap_or(0) / 3;
            path_tokens + 12 + media_tokens
        }
        MediaInlineDecision::Marker(_) => {
            let marker = marker_for_file(path, mime_type);
            estimate_tokens(&marker) + 4
        }
    }
}

/// Estimate token count for a `ChatMessage` with optional media policy.
///
/// File parts follow [`estimate_file_part_tokens`]. Pass the active model's
/// policy for compaction / pre-send sizing.
pub fn estimate_message_tokens_with_media(
    msg: &ChatMessage,
    policy: Option<MediaPolicy>,
) -> u64 {
    let mut tokens = 4u64; // metadata overhead
    for part in &msg.parts {
        tokens += match part {
            ContentPart::Text { text } => estimate_tokens(text),
            ContentPart::File {
                path,
                mime_type,
                size_bytes,
                ..
            } => estimate_file_part_tokens(
                path,
                mime_type.as_deref(),
                *size_bytes,
                policy,
            ),
            ContentPart::Thinking { thinking, .. } => estimate_tokens(thinking),
        };
    }
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            tokens += estimate_tokens(&tc.id)
                + estimate_tokens(&tc.name)
                + estimate_tokens(&tc.arguments)
                + 8;
        }
    }
    if let Some(ref tcid) = msg.tool_call_id {
        tokens += estimate_tokens(tcid) + 4;
    }
    tokens
}

/// Estimate token count for a `ChatMessage`.
///
/// File parts are counted as **markers** (no policy). Prefer
/// [`estimate_message_tokens_with_media`] when the target model is known.
pub fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    estimate_message_tokens_with_media(msg, None)
}

/// Estimate the total prompt tokens for a system prompt + full history under a
/// media policy (what the model request will carry after lowering).
pub fn estimate_history_tokens_with_media(
    system_prompt: &str,
    history: &[ChatMessage],
    policy: Option<MediaPolicy>,
) -> u64 {
    let mut total = 0u64;
    if !system_prompt.is_empty() {
        total += estimate_tokens(system_prompt) + 4;
    }
    for msg in history {
        total += estimate_message_tokens_with_media(msg, policy);
    }
    total
}

/// Estimate the total prompt tokens for a system prompt + full history.
///
/// File parts use marker cost (see [`estimate_message_tokens`]). Compaction
/// should call [`estimate_history_tokens_with_media`] with the model policy.
pub fn estimate_history_tokens(system_prompt: &str, history: &[ChatMessage]) -> u64 {
    estimate_history_tokens_with_media(system_prompt, history, None)
}

/// Token usage tracker — combines precise API-reported usage with
/// estimated pending tokens. Lives on `Session.token_tracker`.
#[derive(Debug, Clone, Default)]
pub struct TokenTracker {
    last_input_tokens: u64,
    last_cached_tokens: u64,
    last_output_tokens: u64,
    pending_estimated_tokens: u64,
}

impl TokenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with precise usage from API response. Resets pending estimates.
    pub fn update_from_usage(&mut self, input_tokens: u64, output_tokens: u64, cached_tokens: u64) {
        self.last_input_tokens = input_tokens;
        self.last_output_tokens = output_tokens;
        self.last_cached_tokens = cached_tokens;
        self.pending_estimated_tokens = 0;
    }

    pub fn record_pending(&mut self, tokens: u64) {
        self.pending_estimated_tokens += tokens;
    }

    /// Seed the tracker by estimating tokens for the system prompt + every
    /// message in the history. Used at turn start when no prior API usage
    /// has been recorded yet (fresh session or post-restart).
    ///
    /// Uses marker-cost for File parts (display-only; compaction decisions use
    /// policy-aware estimates in `maybe_compact`).
    pub fn seed_from_history(&mut self, system_prompt: &str, history: &[ChatMessage]) {
        self.record_pending(estimate_history_tokens(system_prompt, history));
    }

    pub fn total_tokens(&self) -> u64 {
        self.last_input_tokens
            .saturating_add(self.last_cached_tokens)
            .saturating_add(self.last_output_tokens)
            .saturating_add(self.pending_estimated_tokens)
    }

    pub fn is_fresh(&self) -> bool {
        self.last_input_tokens == 0
            && self.last_cached_tokens == 0
            && self.pending_estimated_tokens == 0
    }

    pub fn last_input(&self) -> u64 {
        self.last_input_tokens
    }
    pub fn last_cached(&self) -> u64 {
        self.last_cached_tokens
    }
    pub fn last_output(&self) -> u64 {
        self.last_output_tokens
    }

    pub fn adjust_for_compaction(&mut self, removed_tokens: u64, added_tokens: u64) {
        let net_reduction = removed_tokens.saturating_sub(added_tokens);
        let from_pending = net_reduction.min(self.pending_estimated_tokens);
        self.pending_estimated_tokens -= from_pending;
        self.last_input_tokens = self
            .last_input_tokens
            .saturating_sub(net_reduction - from_pending);
    }
}

/// True for tools that can mutate system state and are therefore blocked
/// in `PermissionMode::ReadOnly`.
pub fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "shell"
            | "file_write"
            | "file_edit"
            | "file_delete"
            | "agent_delegate"
            | "agent_kill"
            | "http"
    )
}

#[cfg(test)]
mod tracker_tests {
    use super::*;
    use crate::providers::media::MediaPolicy;

    #[test]
    fn estimate_history_tokens_counts_system_and_messages() {
        let history = vec![
            ChatMessage::user_text("hello there"),
            ChatMessage::assistant_text("hi"),
        ];
        let est = estimate_history_tokens("you are a bot", &history);
        // Strictly greater than the system prompt alone + nonzero per message.
        assert!(est > estimate_tokens("you are a bot"));
    }

    #[test]
    fn estimate_tokens_cjk_not_underestimated() {
        // 100 CJK chars: old formula = 300 bytes / 4 = 75 tokens (too low).
        // New formula = 100 tokens (1 per CJK char).
        let cjk_text = "字".repeat(100);
        assert_eq!(estimate_tokens(&cjk_text), 100);

        // Mixed: 50 CJK + 100 ASCII bytes
        let mixed = format!("{}{}", "字".repeat(50), "a".repeat(100));
        // 50 CJK tokens + ceil(100/4) = 50 + 25 = 75
        assert_eq!(estimate_tokens(&mixed), 75);

        // Pure ASCII unchanged: "hello" = 5 bytes / 4 = 2 tokens
        assert_eq!(estimate_tokens("hello"), 2);
    }

    #[test]
    fn file_without_policy_is_marker_not_base64() {
        let path = "sessions/x/files/image-abc.png";
        // ~800KB would be ~266k tokens under base64; marker must stay tiny.
        let big = estimate_file_part_tokens(path, Some("image/png"), Some(829_062), None);
        assert!(
            big < 200,
            "marker estimate should be tiny, got {big}"
        );

        let marker = marker_for_file(path, Some("image/png"));
        assert!(marker.contains("图片") || marker.contains("image"), "{marker}");
        let expected = estimate_tokens(&marker) + 4;
        assert_eq!(big, expected);
    }

    #[test]
    fn file_with_vision_policy_is_base64_sized() {
        let path = "sessions/x/files/shot.png";
        let policy = MediaPolicy::from_model_support(true, false, false);
        let size = 300_000u64;
        let est =
            estimate_file_part_tokens(path, Some("image/png"), Some(size), Some(policy));
        // size/3 = 100_000 plus framing
        assert!(est > 90_000, "inline estimate too small: {est}");
        assert!(est < 120_000, "inline estimate too large: {est}");
    }

    #[test]
    fn file_with_text_only_policy_is_marker() {
        let path = "sessions/x/files/shot.png";
        let policy = MediaPolicy::from_model_support(false, false, false);
        let est =
            estimate_file_part_tokens(path, Some("image/png"), Some(829_062), Some(policy));
        assert!(est < 200, "text-only policy must not count base64, got {est}");
    }

    #[test]
    fn message_with_file_respects_policy() {
        let path = "sessions/x/files/shot.png".to_string();
        let msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                ContentPart::File {
                    path: path.clone(),
                    mime_type: Some("image/png".into()),
                    name: Some("shot.png".into()),
                    size_bytes: Some(829_062),
                },
                ContentPart::Text {
                    text: "看这张图".into(),
                },
            ],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
        };

        let as_marker = estimate_message_tokens_with_media(&msg, None);
        let text_only = MediaPolicy::from_model_support(false, false, false);
        let as_text_model =
            estimate_message_tokens_with_media(&msg, Some(text_only));
        let vision = MediaPolicy::from_model_support(true, false, false);
        let as_vision = estimate_message_tokens_with_media(&msg, Some(vision));

        assert!(as_marker < 500, "default: {as_marker}");
        assert!(as_text_model < 500, "text-only: {as_text_model}");
        assert!(as_vision > 200_000, "vision: {as_vision}");
    }
}
